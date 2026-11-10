//! The main window: schema browser, SQL editor, result grid and status bar.

use std::cell::Cell;
use std::collections::HashSet;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::tree::{TreeEvent, TreeState, tree};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, Theme,
    ThemeMode, TitleBar, WindowExt as _, h_flex, h_resizable, resizable_panel, v_flex, v_resizable,
};
use gpui_kit::{
    Anchor, App, AppContext as _, ClickEvent, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, PathPromptOptions,
    Render, SharedString, Styled as _, Task, Window, div, prelude::FluentBuilder as _, px,
};
use quarry_core::catalog::{Catalog, RelationKind};
use quarry_core::{Dialect, Driver, Flavor, QueryEvent, QueryOptions, QuerySummary, Session, sql};
use quarry_duckdb::DuckDbDriver;

use crate::results::ResultsDelegate;
use crate::schema_tree::{self, Node};
use crate::{connect, highlight};

gpui_kit::actions!(
    quarry,
    [
        RunStatement,
        RunScript,
        CancelQuery,
        OpenDatabase,
        Connect,
        NewInMemory,
        RefreshCatalog,
        ToggleTheme,
    ]
);

const CONTEXT: &str = "Workspace";
const EDITOR_CONTEXT: &str = "SqlEditor";

/// `cmd-<key>` on macOS, `ctrl-<key>` elsewhere, as a `&'static str`.
#[macro_export]
macro_rules! primary {
    ($key:literal) => {
        if cfg!(target_os = "macos") {
            concat!("cmd-", $key)
        } else {
            concat!("ctrl-", $key)
        }
    };
}

pub fn init(cx: &mut App) {
    // The editor binds cmd-enter itself (to insert a newline), and the
    // deepest context wins, so the run bindings must match inside it too.
    let in_editor = format!("{EDITOR_CONTEXT} > Input");
    for context in [CONTEXT, in_editor.as_str()] {
        cx.bind_keys([
            KeyBinding::new(primary!("enter"), RunStatement, Some(context)),
            KeyBinding::new(primary!("shift-enter"), RunScript, Some(context)),
            KeyBinding::new(primary!("."), CancelQuery, Some(context)),
        ]);
    }
    cx.bind_keys([
        KeyBinding::new(primary!("o"), OpenDatabase, Some(CONTEXT)),
        KeyBinding::new(primary!("shift-o"), Connect, Some(CONTEXT)),
        KeyBinding::new(primary!("r"), RefreshCatalog, Some(CONTEXT)),
    ]);
}

const CONNECT_HELP: &str = "PostgreSQL: postgres://user@host:5432/db   \
MySQL / MariaDB: mysql://user@host:3306/db   \
ClickHouse: clickhouse://user@host:8123/db (clickhouses:// for TLS)   \
SQLite / DuckDB: a file path, sqlite:path or duckdb:path";

const WELCOME_SQL: &str = "\
-- Welcome to Quarry. ⌘↵ runs the statement under the caret, ⇧⌘↵ runs everything.
-- DuckDB reads CSV, Parquet and JSON files directly: SELECT * FROM 'data.parquet';

CREATE OR REPLACE TABLE numbers AS
SELECT i AS id,
       'item ' || i AS label,
       random() * 1000 AS score,
       DATE '2026-01-01' + (i % 365)::INTEGER AS day
FROM range(1000000) t(i);

SELECT day, count(*) AS items, round(avg(score), 2) AS avg_score
FROM numbers
GROUP BY day
ORDER BY day;

SELECT * FROM numbers;
";

enum Status {
    Idle,
    Running {
        started: Instant,
        statement: usize,
        statements: usize,
    },
    Done {
        summary: QuerySummary,
        statements: usize,
    },
    Failed {
        message: SharedString,
        elapsed: Duration,
        statement: usize,
        statements: usize,
    },
}

enum Flow {
    Continue,
    NextStatement,
    Stop,
}

pub struct Workspace {
    focus_handle: FocusHandle,
    session: Session,
    editor: Entity<EditorState>,
    tree: Entity<TreeState>,
    catalog: Catalog,
    /// Tree items the user has open, kept across catalog refreshes.
    expanded: HashSet<SharedString>,
    results: Entity<TableState<ResultsDelegate>>,
    status: Status,
    query_task: Option<Task<()>>,
    ticker: Option<Task<()>>,
    catalog_task: Option<Task<()>>,
    open_task: Option<Task<()>>,
    /// What a connection in progress is opening, for the status bar.
    connecting: Option<SharedString>,
    /// The flavor the editor highlights, splits and checks queries as, when
    /// the user picked one. `None` follows the connection.
    editor_flavor: Option<Flavor>,
}

impl Workspace {
    /// `target` is the command-line argument: a file path or connection URL.
    /// Quarry starts on an in-memory DuckDB and connects to it from there.
    pub fn new(target: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let driver = DuckDbDriver::in_memory().expect("in-memory DuckDB always opens");
        let session = Session::spawn(driver);
        let flavor = session.flavor().clone();

        let editor = cx.new(|cx| {
            let mut editor = EditorState::new(window, cx)
                .language("sql")
                .default_value(WELCOME_SQL);
            editor.set_highlighter_factory(highlight::factory(flavor), cx);
            editor
        });
        cx.subscribe(&editor, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.update_version_notes(cx);
            }
        })
        .detach();
        let tree = cx.new(|cx| TreeState::new(cx));
        cx.subscribe(&tree, |this, _, event: &TreeEvent, _| match event {
            TreeEvent::Expanded(id) => {
                this.expanded.insert(id.clone());
            }
            TreeEvent::Collapsed(id) => {
                this.expanded.remove(id);
            }
        })
        .detach();
        let results = cx.new(|cx| {
            TableState::new(ResultsDelegate::default(), window, cx)
                .cell_selectable(true)
                .row_header(false)
                .sortable(false)
                .col_movable(false)
        });

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            session,
            editor,
            tree,
            catalog: Catalog::default(),
            expanded: HashSet::new(),
            results,
            status: Status::Idle,
            query_task: None,
            ticker: None,
            catalog_task: None,
            open_task: None,
            connecting: None,
            editor_flavor: None,
        };
        match target.map(|t| connect::Target::parse(&t)) {
            Some(Ok(target)) => this.connect_to(target, window, cx),
            Some(Err(err)) => window.push_notification(format!("{err:#}"), cx),
            None => this.refresh_catalog(&RefreshCatalog, window, cx),
        }
        this.editor
            .update(cx, |editor, cx| editor.focus(window, cx));
        this
    }

    fn switch_session(
        &mut self,
        driver: Box<dyn Driver>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.session.cancel();
        self.query_task = None;
        self.ticker = None;
        self.session = Session::spawn(driver);
        self.status = Status::Idle;
        self.catalog = Catalog::default();
        self.expanded.clear();
        self.results.update(cx, |table, cx| {
            table.delegate_mut().clear();
            table.refresh(cx);
        });
        // An editor following the connection now reads as the new engine.
        self.apply_editor_flavor(cx);
        self.refresh_catalog(&RefreshCatalog, window, cx);
        cx.notify();
    }

    /// The flavor queries are written in: the one picked in the editor's
    /// toolbar, or the connection's.
    fn editor_flavor(&self) -> Flavor {
        self.editor_flavor
            .clone()
            .unwrap_or_else(|| self.session.flavor().clone())
    }

    fn set_editor_flavor(&mut self, flavor: Option<Flavor>, cx: &mut Context<Self>) {
        self.editor_flavor = flavor;
        self.apply_editor_flavor(cx);
        cx.notify();
    }

    /// Re-highlight and re-check the editor as [`Self::editor_flavor`].
    fn apply_editor_flavor(&mut self, cx: &mut Context<Self>) {
        let flavor = self.editor_flavor();
        self.editor.update(cx, |editor, cx| {
            editor.set_highlighter_factory(highlight::factory(flavor), cx);
        });
        self.update_version_notes(cx);
    }

    fn update_version_notes(&mut self, cx: &mut Context<Self>) {
        let flavor = self.editor_flavor();
        self.editor.update(cx, |editor, cx| {
            highlight::update_version_notes(editor, &flavor, cx)
        });
    }

    /// Open `target` off the UI thread (servers block on the network), then
    /// switch to it.
    fn connect_to(&mut self, target: connect::Target, window: &mut Window, cx: &mut Context<Self>) {
        self.connecting = Some(
            match &target {
                connect::Target::InMemory(dialect) => format!("in-memory {dialect}"),
                connect::Target::File { path, .. } => path.display().to_string(),
                connect::Target::Server { url, .. } => connect::redact(url),
            }
            .into(),
        );
        cx.notify();
        self.open_task = Some(cx.spawn_in(window, async move |this, cx| {
            let opened = cx
                .background_executor()
                .spawn(async move { connect::open(&target) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.connecting = None;
                match opened {
                    Ok(driver) => this.switch_session(driver, window, cx),
                    Err(err) => {
                        window.push_notification(format!("Could not connect: {err:#}"), cx);
                        cx.notify();
                    }
                }
            });
        }));
    }

    fn open_database(&mut self, _: &OpenDatabase, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let target = connect::Target::File {
                dialect: connect::file_dialect(&path),
                path,
            };
            let _ = this.update_in(cx, |this, window, cx| this.connect_to(target, window, cx));
        })
        .detach();
    }

    /// Ask for a connection URL (and, separately, a password) and connect.
    fn connect(&mut self, _: &Connect, window: &mut Window, cx: &mut Context<Self>) {
        let url = cx.new(|cx| {
            InputState::new(window, cx).placeholder("postgres://user@host:5432/database")
        });
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Password (optional)")
                .masked(true)
        });
        let workspace = cx.entity().downgrade();
        let initial_focus = Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if initial_focus.replace(false) {
                url.focus_handle(cx).focus(window, cx);
            }
            let muted = cx.theme().muted_foreground;
            let (url_ok, password_ok, workspace) =
                (url.clone(), password.clone(), workspace.clone());
            dialog
                .title("Connect")
                .w(px(520.))
                .child(
                    v_flex()
                        .gap_2()
                        .child(Input::new(&url))
                        .child(Input::new(&password))
                        .child(div().text_xs().text_color(muted).child(CONNECT_HELP)),
                )
                .button_props(
                    DialogButtonProps::default()
                        .show_cancel(true)
                        .ok_text("Connect"),
                )
                .on_ok(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let text = url_ok.read(cx).value().to_string();
                    let password = password_ok.read(cx).value().to_string();
                    match connect::Target::parse(&text) {
                        Ok(target) => {
                            let target = target.with_password(&password);
                            let _ = workspace
                                .update(cx, |this, cx| this.connect_to(target, window, cx));
                            true
                        }
                        Err(err) => {
                            window.push_notification(format!("{err:#}"), cx);
                            false
                        }
                    }
                })
        });
    }

    fn new_in_memory(&mut self, _: &NewInMemory, window: &mut Window, cx: &mut Context<Self>) {
        match DuckDbDriver::in_memory() {
            Ok(driver) => self.switch_session(Box::new(driver), window, cx),
            Err(err) => window.push_notification(format!("{err:#}"), cx),
        }
    }

    fn refresh_catalog(&mut self, _: &RefreshCatalog, window: &mut Window, cx: &mut Context<Self>) {
        let reply = self.session.catalog();
        self.catalog_task = Some(cx.spawn_in(window, async move |this, cx| {
            let Ok(catalog) = reply.recv_async().await else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| match catalog {
                Ok(catalog) => {
                    // Open the databases the first time a connection's
                    // catalog arrives.
                    if this.catalog.databases.is_empty() {
                        this.expanded.extend(
                            catalog
                                .databases
                                .iter()
                                .map(|db| Node::Database(db.name.clone()).id()),
                        );
                    }
                    let items = schema_tree::build_items(&catalog, &this.expanded);
                    this.catalog = catalog;
                    this.tree.update(cx, |tree, cx| tree.set_items(items, cx));
                    cx.notify();
                }
                Err(err) => {
                    window.push_notification(format!("Could not read the schema: {err:#}"), cx)
                }
            });
        }));
    }

    fn run_statement(&mut self, _: &RunStatement, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.editor.read(cx);
        let text = editor.value().to_string();
        let selected = editor.selected_range();
        let dialect = self.editor_flavor().dialect;
        let statements = if selected.is_empty() {
            sql::statement_at_for(&text, editor.cursor(), dialect)
                .map(|span| vec![span.text(&text).to_owned()])
                .unwrap_or_default()
        } else {
            // Run exactly what is selected, split if it holds several.
            let selection = &text[selected];
            sql::split_statements_for(selection, dialect)
                .iter()
                .map(|span| span.text(selection).to_owned())
                .collect()
        };
        self.run(statements, window, cx);
    }

    fn run_script(&mut self, _: &RunScript, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).value().to_string();
        let statements = sql::split_statements_for(&text, self.editor_flavor().dialect)
            .iter()
            .map(|span| span.text(&text).to_owned())
            .collect();
        self.run(statements, window, cx);
    }

    fn cancel_query(&mut self, _: &CancelQuery, _: &mut Window, _: &mut Context<Self>) {
        self.session.cancel();
    }

    fn toggle_theme(&mut self, _: &ToggleTheme, window: &mut Window, cx: &mut Context<Self>) {
        let mode = if cx.theme().mode.is_dark() {
            ThemeMode::Light
        } else {
            ThemeMode::Dark
        };
        Theme::change(mode, Some(window), cx);
        crate::fonts::apply(cx);
    }

    fn is_running(&self) -> bool {
        matches!(self.status, Status::Running { .. })
    }

    fn run(&mut self, statements: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        if statements.is_empty() {
            return;
        }
        if self.is_running() {
            self.session.cancel();
        }
        let total = statements.len();
        self.status = Status::Running {
            started: Instant::now(),
            statement: 0,
            statements: total,
        };
        self.results.update(cx, |table, cx| {
            table.delegate_mut().clear();
            table.refresh(cx);
        });
        let dialect = self.session.flavor().dialect;
        let modifies_schema = statements.iter().any(|s| dialect.changes_schema(s));

        let session = self.session.clone();
        self.query_task = Some(cx.spawn_in(window, async move |this, cx| {
            'statements: for (ix, statement) in statements.into_iter().enumerate() {
                let query = session.query(statement, QueryOptions::default());
                loop {
                    let Ok(first) = query.events.recv_async().await else {
                        break 'statements;
                    };
                    // Apply everything that has arrived in one update, so a
                    // fast stream of batches costs one redraw, not one each.
                    let mut events = vec![first];
                    events.extend(query.events.try_iter());
                    let flow = this.update(cx, |this, cx| this.apply_events(events, ix, total, cx));
                    match flow {
                        Ok(Flow::Continue) => {}
                        Ok(Flow::NextStatement) => break,
                        Ok(Flow::Stop) => break 'statements,
                        Err(_) => return,
                    }
                }
            }
            // Even a run that failed or was cancelled part-way may have
            // created or dropped something.
            if modifies_schema {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.refresh_catalog(&RefreshCatalog, window, cx)
                });
            }
        }));

        // Keep the elapsed time in the status bar moving while it runs.
        self.ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                let running = this
                    .update(cx, |this, cx| {
                        cx.notify();
                        this.is_running()
                    })
                    .unwrap_or(false);
                if !running {
                    break;
                }
            }
        }));
        cx.notify();
    }

    fn apply_events(
        &mut self,
        events: Vec<QueryEvent>,
        statement: usize,
        statements: usize,
        cx: &mut Context<Self>,
    ) -> Flow {
        let mut flow = Flow::Continue;
        let mut new_schema = false;
        let mut new_rows = false;
        self.results.update(cx, |table, _| {
            for event in events {
                match event {
                    QueryEvent::Schema(schema) => {
                        table.delegate_mut().set_schema(schema);
                        new_schema = true;
                    }
                    QueryEvent::Batch(batch) => {
                        table.delegate_mut().push(batch);
                        new_rows = true;
                    }
                    QueryEvent::Finished(summary) => {
                        let last = statement + 1 == statements;
                        flow = if summary.cancelled || last {
                            self.status = Status::Done {
                                summary,
                                statements,
                            };
                            Flow::Stop
                        } else {
                            if let Status::Running { statement: s, .. } = &mut self.status {
                                *s = statement + 1;
                            }
                            Flow::NextStatement
                        };
                    }
                    QueryEvent::Failed { message, elapsed } => {
                        self.status = Status::Failed {
                            message: message.into(),
                            elapsed,
                            statement,
                            statements,
                        };
                        flow = Flow::Stop;
                    }
                }
            }
        });
        if new_schema {
            self.results.update(cx, |table, cx| table.refresh(cx));
        } else if new_rows {
            self.results.update(cx, |_, cx| cx.notify());
        }
        cx.notify();
        flow
    }

    /// Double-clicking a table or view previews it.
    fn open_node(&mut self, node: Node, window: &mut Window, cx: &mut Context<Self>) {
        let Node::Relation {
            database,
            schema,
            name,
            ..
        } = node
        else {
            return;
        };
        let sql = format!(
            "SELECT * FROM {} LIMIT 1000;",
            self.session
                .flavor()
                .dialect
                .relation_name(&database, &schema, &name)
        );
        self.run(vec![sql], window, cx);
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.is_running();
        h_flex()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("run")
                    .icon(Icon::new(gpui_kit::assets::IconName::Play))
                    .label("Run")
                    .small()
                    .primary()
                    .tooltip("Run statement at caret (⌘↵)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.run_statement(&RunStatement, window, cx)
                    })),
            )
            .child(
                Button::new("run-all")
                    .icon(Icon::new(gpui_kit::assets::IconName::ListEnd))
                    .label("Run all")
                    .small()
                    .ghost()
                    .tooltip("Run every statement (⇧⌘↵)")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.run_script(&RunScript, window, cx)),
                    ),
            )
            .child(
                Button::new("cancel")
                    .icon(Icon::new(gpui_kit::assets::IconName::Square))
                    .label("Cancel")
                    .small()
                    .ghost()
                    .disabled(!running)
                    .tooltip("Cancel the running query (⌘.)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.cancel_query(&CancelQuery, window, cx)
                    })),
            )
            .child(div().flex_1())
            .child(self.render_flavor_picker(cx))
    }

    /// The editor's SQL dialect and release, like DataGrip's per-console
    /// dialect: it follows the connection unless the user picks one, e.g.
    /// to write for PostgreSQL 13 while connected to PostgreSQL 17.
    fn render_flavor_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let flavor = self.editor_flavor();
        let connection = self.session.flavor().clone();
        let picked = self.editor_flavor.clone();
        let differs = picked
            .as_ref()
            .is_some_and(|p| p.dialect != connection.dialect);
        let tooltip: SharedString = match &picked {
            None => format!("Writing {} (from the connection)", connection.label()),
            Some(p) if differs => format!(
                "Writing {}; queries still run on {}",
                p.label(),
                connection.label()
            ),
            Some(p) => format!("Writing {}; connected to {}", p.label(), connection.label()),
        }
        .into();
        let workspace = cx.entity().downgrade();
        Button::new("flavor")
            .label(flavor.label())
            .icon(Icon::new(gpui_kit::assets::IconName::Database))
            .small()
            .ghost()
            .when(differs, |b| b.text_color(cx.theme().warning))
            .tooltip(tooltip)
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, window, cx| {
                let owner = workspace.clone();
                let pick = move |choice: Option<Flavor>| {
                    let workspace = owner.clone();
                    move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                        let _ = workspace
                            .update(cx, |this, cx| this.set_editor_flavor(choice.clone(), cx));
                    }
                };
                let mut menu = menu.item(
                    PopupMenuItem::new(format!("Auto: {}", connection.label()))
                        .checked(picked.is_none())
                        .on_click(pick(None)),
                );
                menu = menu.separator();
                for dialect in Dialect::ALL {
                    let picked = picked.clone();
                    let pick = pick.clone();
                    menu = menu.submenu(dialect.name(), window, cx, move |mut sub, _, _| {
                        let latest = Flavor::unversioned(dialect);
                        sub = sub.item(
                            PopupMenuItem::new("Latest")
                                .checked(picked.as_ref() == Some(&latest))
                                .on_click(pick(Some(latest))),
                        );
                        for release in dialect.releases().iter().rev() {
                            let flavor = Flavor::release(dialect, release);
                            sub = sub.item(
                                PopupMenuItem::new(release.label)
                                    .checked(picked.as_ref() == Some(&flavor))
                                    .on_click(pick(Some(flavor))),
                            );
                        }
                        sub
                    });
                }
                menu
            })
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = cx.entity().downgrade();
        let target = SharedString::from(self.session.target().to_owned());
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .font_semibold()
                            .text_color(cx.theme().muted_foreground)
                            .child("DATABASE"),
                    )
                    .child(
                        Button::new("connect")
                            .icon(Icon::new(gpui_kit::assets::IconName::Plug))
                            .xsmall()
                            .ghost()
                            .tooltip("Connect to a server (⇧⌘O)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.connect(&Connect, window, cx)
                            })),
                    )
                    .child(
                        Button::new("open")
                            .icon(Icon::new(gpui_kit::assets::IconName::FolderOpen))
                            .xsmall()
                            .ghost()
                            .tooltip("Open a DuckDB or SQLite file (⌘O)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_database(&OpenDatabase, window, cx)
                            })),
                    )
                    .child(
                        Button::new("memory")
                            .icon(Icon::new(gpui_kit::assets::IconName::DatabasePlus))
                            .xsmall()
                            .ghost()
                            .tooltip("New in-memory database")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.new_in_memory(&NewInMemory, window, cx)
                            })),
                    )
                    .child(
                        Button::new("refresh")
                            .icon(Icon::new(gpui_kit::assets::IconName::RefreshCw))
                            .xsmall()
                            .ghost()
                            .tooltip("Refresh schema (⌘R)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.refresh_catalog(&RefreshCatalog, window, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .truncate()
                    .child(target),
            )
            .child(
                div().flex_1().min_h_0().child(
                    tree(&self.tree, move |ix, entry, _selected, _window, cx| {
                        let item = entry.item();
                        let node = Node::parse(&item.id);
                        let icon = match &node {
                            Some(Node::Database(_)) => gpui_kit::assets::IconName::Database,
                            Some(Node::Schema(..)) => gpui_kit::assets::IconName::Folder,
                            Some(Node::Relation {
                                kind: RelationKind::View,
                                ..
                            }) => gpui_kit::assets::IconName::Eye,
                            Some(Node::Relation {
                                kind: RelationKind::MaterializedView,
                                ..
                            }) => gpui_kit::assets::IconName::Layers,
                            Some(Node::Relation { .. }) => gpui_kit::assets::IconName::Table,
                            _ => gpui_kit::assets::IconName::Columns3,
                        };
                        let chevron = if entry.is_folder() {
                            if entry.is_expanded() {
                                Some(IconName::ChevronDown)
                            } else {
                                Some(IconName::ChevronRight)
                            }
                        } else {
                            None
                        };
                        let workspace = workspace.clone();
                        ListItem::new(ix)
                            .pl(px(8.) + px(14.) * entry.depth() as f32)
                            .child(
                                h_flex()
                                    .gap_1()
                                    .text_sm()
                                    .child(div().w(px(14.)).flex_shrink_0().when_some(
                                        chevron,
                                        |d, chevron| {
                                            d.child(
                                                Icon::new(chevron)
                                                    .xsmall()
                                                    .text_color(cx.theme().muted_foreground),
                                            )
                                        },
                                    ))
                                    .child(
                                        Icon::new(icon)
                                            .small()
                                            .text_color(cx.theme().muted_foreground),
                                    )
                                    .child(div().truncate().child(item.label.clone())),
                            )
                            .on_click(move |event, window, cx| {
                                if event.click_count() < 2 {
                                    return;
                                }
                                if let Some(node) = node.clone() {
                                    let _ = workspace
                                        .update(cx, |this, cx| this.open_node(node, window, cx));
                                }
                            })
                    })
                    .size_full(),
                ),
            )
    }

    fn render_results(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let error = match &self.status {
            Status::Failed {
                message,
                statement,
                statements,
                ..
            } => Some((message.clone(), *statement, *statements)),
            _ => None,
        };
        v_flex().size_full().map(|this| match error {
            Some((message, statement, statements)) => this.child(
                v_flex()
                    .size_full()
                    .p_3()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .text_color(cx.theme().danger)
                            .child(Icon::new(gpui_kit::assets::IconName::TriangleAlert).small())
                            .child(div().font_semibold().child(if statements > 1 {
                                format!("Statement {} of {statements} failed", statement + 1)
                            } else {
                                "Query failed".to_owned()
                            })),
                    )
                    .child(
                        div()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_sm()
                            .child(message),
                    ),
            ),
            None => this.child(
                DataTable::new(&self.results)
                    .stripe(true)
                    .bordered(false)
                    .small(),
            ),
        })
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.results.read(cx).delegate().row_count();
        let (text, tone): (String, _) = match &self.status {
            _ if self.connecting.is_some() => (
                format!(
                    "Connecting to {}…",
                    self.connecting.clone().unwrap_or_default()
                ),
                cx.theme().foreground,
            ),
            Status::Idle => ("Ready".into(), cx.theme().muted_foreground),
            Status::Running {
                started,
                statement,
                statements,
            } => {
                let step = if *statements > 1 {
                    format!("statement {} of {statements} · ", statement + 1)
                } else {
                    String::new()
                };
                (
                    format!(
                        "Running… {step}{} rows · {}",
                        format_count(rows),
                        format_elapsed(started.elapsed())
                    ),
                    cx.theme().foreground,
                )
            }
            Status::Done {
                summary,
                statements,
            } => {
                let mut text = format!(
                    "{} rows in {}",
                    format_count(summary.rows),
                    format_elapsed(summary.elapsed)
                );
                if summary.truncated {
                    text.push_str(" · stopped at the row limit");
                }
                if summary.cancelled {
                    text.push_str(" · cancelled");
                }
                if *statements > 1 {
                    text = format!("{statements} statements · last: {text}");
                }
                (text, cx.theme().muted_foreground)
            }
            Status::Failed { elapsed, .. } => (
                format!("Failed after {}", format_elapsed(*elapsed)),
                cx.theme().danger,
            ),
        };
        h_flex()
            .px_3()
            .py_1()
            .gap_4()
            .text_xs()
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().title_bar)
            .child(div().flex_1().text_color(tone).child(text))
            .child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(self.session.flavor().label()),
            )
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dark = cx.theme().mode.is_dark();
        v_flex()
            .id("workspace")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::run_statement))
            .on_action(cx.listener(Self::run_script))
            .on_action(cx.listener(Self::cancel_query))
            .on_action(cx.listener(Self::open_database))
            .on_action(cx.listener(Self::connect))
            .on_action(cx.listener(Self::new_in_memory))
            .on_action(cx.listener(Self::refresh_catalog))
            .on_action(cx.listener(Self::toggle_theme))
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                TitleBar::new().child(
                    h_flex()
                        .w_full()
                        .pr_2()
                        .justify_between()
                        .child(div().text_sm().font_semibold().child("Quarry"))
                        .child(
                            Button::new("theme")
                                .icon(Icon::new(if dark {
                                    gpui_kit::assets::IconName::Sun
                                } else {
                                    gpui_kit::assets::IconName::Moon
                                }))
                                .xsmall()
                                .ghost()
                                .tooltip("Toggle light and dark")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.toggle_theme(&ToggleTheme, window, cx)
                                })),
                        ),
                ),
            )
            .child(
                div().flex_1().min_h_0().child(
                    h_resizable("main")
                        .child(
                            resizable_panel()
                                .size(px(280.))
                                .size_range(px(180.)..px(560.))
                                .child(self.render_sidebar(cx)),
                        )
                        .child(
                            resizable_panel().child(
                                v_resizable("work")
                                    .child(
                                        resizable_panel().size(px(320.)).child(
                                            v_flex()
                                                .size_full()
                                                .child(self.render_toolbar(cx))
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_h_0()
                                                        .key_context(EDITOR_CONTEXT)
                                                        .child(
                                                            Editor::new(&self.editor)
                                                                .bordered(false)
                                                                .h_full(),
                                                        ),
                                                ),
                                        ),
                                    )
                                    .child(resizable_panel().child(self.render_results(cx))),
                            ),
                        ),
                ),
            )
            .child(self.render_status_bar(cx))
    }
}

fn format_count(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn format_elapsed(elapsed: Duration) -> String {
    let ms = elapsed.as_secs_f64() * 1000.0;
    if ms < 1000.0 {
        format!("{ms:.0} ms")
    } else if ms < 60_000.0 {
        format!("{:.2} s", ms / 1000.0)
    } else {
        let secs = elapsed.as_secs();
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_counts_and_durations() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1000), "1,000");
        assert_eq!(format_count(1234567), "1,234,567");
        assert_eq!(format_elapsed(Duration::from_millis(42)), "42 ms");
        assert_eq!(format_elapsed(Duration::from_millis(1500)), "1.50 s");
        assert_eq!(format_elapsed(Duration::from_secs(75)), "1m 15s");
    }
}
