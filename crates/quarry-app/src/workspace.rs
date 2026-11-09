//! The main window: schema browser, SQL editor, result grid and status bar.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::tree::{TreeEvent, TreeState, tree};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, Theme,
    ThemeMode, TitleBar, WindowExt as _, h_flex, h_resizable, resizable_panel, v_flex, v_resizable,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, KeyBinding, ParentElement as _, PathPromptOptions, Render, SharedString,
    Styled as _, Task, Window, div, prelude::FluentBuilder as _, px,
};
use quarry_core::catalog::{Catalog, RelationKind, qualified_name};
use quarry_core::{QueryEvent, QueryOptions, QuerySummary, Session, sql};
use quarry_duckdb::{DuckDbDriver, Target};

use crate::results::ResultsDelegate;
use crate::schema_tree::{self, Node};

gpui_kit::actions!(
    quarry,
    [
        RunStatement,
        RunScript,
        CancelQuery,
        OpenDatabase,
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
        KeyBinding::new(primary!("r"), RefreshCatalog, Some(CONTEXT)),
    ]);
}

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
    engine_version: SharedString,
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
}

impl Workspace {
    pub fn new(path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (driver, error) = match open_driver(path.clone()) {
            Ok(driver) => (driver, None),
            Err(err) => (
                DuckDbDriver::in_memory().expect("in-memory DuckDB always opens"),
                Some(format!("{err:#}")),
            ),
        };
        let engine_version = driver.version().unwrap_or_default().into();
        let session = Session::spawn(driver);

        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .default_value(WELCOME_SQL)
        });
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
            engine_version,
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
        };
        if let Some(message) = error {
            window.push_notification(format!("Could not open database: {message}"), cx);
        }
        this.refresh_catalog(&RefreshCatalog, window, cx);
        this.editor
            .update(cx, |editor, cx| editor.focus(window, cx));
        this
    }

    fn switch_session(
        &mut self,
        driver: DuckDbDriver,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.session.cancel();
        self.query_task = None;
        self.ticker = None;
        self.engine_version = driver.version().unwrap_or_default().into();
        self.session = Session::spawn(driver);
        self.status = Status::Idle;
        self.catalog = Catalog::default();
        self.expanded.clear();
        self.results.update(cx, |table, cx| {
            table.delegate_mut().clear();
            table.refresh(cx);
        });
        self.refresh_catalog(&RefreshCatalog, window, cx);
        cx.notify();
    }

    fn open_database(&mut self, _: &OpenDatabase, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open".into()),
        });
        self.open_task = Some(cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let opened = cx
                .background_executor()
                .spawn(async move { open_driver(Some(path)) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| match opened {
                Ok(driver) => this.switch_session(driver, window, cx),
                Err(err) => {
                    window.push_notification(format!("Could not open database: {err:#}"), cx)
                }
            });
        }));
    }

    fn new_in_memory(&mut self, _: &NewInMemory, window: &mut Window, cx: &mut Context<Self>) {
        match DuckDbDriver::in_memory() {
            Ok(driver) => self.switch_session(driver, window, cx),
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
        let statements = if selected.is_empty() {
            sql::statement_at(&text, editor.cursor())
                .map(|span| vec![span.text(&text).to_owned()])
                .unwrap_or_default()
        } else {
            // Run exactly what is selected, split if it holds several.
            let selection = &text[selected];
            sql::split_statements(selection)
                .iter()
                .map(|span| span.text(selection).to_owned())
                .collect()
        };
        self.run(statements, window, cx);
    }

    fn run_script(&mut self, _: &RunScript, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).value().to_string();
        let statements = sql::split_statements(&text)
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
        let modifies_schema = statements.iter().any(|s| changes_schema(s));

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
            qualified_name(&database, &schema, &name)
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
                        Button::new("open")
                            .icon(Icon::new(gpui_kit::assets::IconName::FolderOpen))
                            .xsmall()
                            .ghost()
                            .tooltip("Open a DuckDB file (⌘O)")
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
            .child(div().text_color(cx.theme().muted_foreground).child(format!(
                "{} {}",
                self.session.engine(),
                self.engine_version
            )))
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

fn open_driver(path: Option<PathBuf>) -> anyhow::Result<DuckDbDriver> {
    match path {
        Some(path) => DuckDbDriver::open(Target::File {
            path,
            read_only: false,
        }),
        None => DuckDbDriver::in_memory(),
    }
}

/// Whether a statement may add, drop or rename objects, so the schema
/// browser should reload after it.
fn changes_schema(statement: &str) -> bool {
    let first = statement
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    matches!(
        first.as_str(),
        "CREATE" | "DROP" | "ALTER" | "ATTACH" | "DETACH" | "USE" | "IMPORT" | "COMMENT"
    )
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

    #[test]
    fn spots_schema_changes() {
        assert!(changes_schema("create table t (a int)"));
        assert!(changes_schema("  DROP VIEW v"));
        assert!(!changes_schema("select 1"));
        assert!(!changes_schema("insert into t values (1)"));
    }
}
