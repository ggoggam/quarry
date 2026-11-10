//! The main window: schema browser, SQL editor, result grid and status bar.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::{DialogAction, DialogButtonProps, DialogClose, DialogFooter};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::tree::{TreeEvent, TreeState, tree};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, Theme,
    ThemeMode, TitleBar, WindowExt as _, h_flex, h_resizable, resizable_panel, v_flex, v_resizable,
};
use gpui_kit::{
    App, AppContext as _, ClickEvent, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, PathPromptOptions,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, Task, Window, div,
    prelude::FluentBuilder as _, px,
};
use quarry_core::catalog::{Catalog, RelationKind};
use quarry_core::{Dialect, Driver, QueryEvent, QueryOptions, QuerySummary, Session};
use quarry_duckdb::DuckDbDriver;

use crate::editor::{self, EDITOR_CONTEXT, Editors};
use crate::results::ResultsDelegate;
use crate::saved::{self, SavedConnection, Secret, Secrets};
use crate::schema_tree::{self, Node};
use crate::{connect, update};

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
    editor::init(CONTEXT, cx);
}

/// The File menu: the editor's items, then the database's.
pub fn file_menu_items() -> Vec<gpui_kit::MenuItem> {
    use gpui_kit::MenuItem;
    let mut items = editor::menu_items();
    items.extend([
        MenuItem::separator(),
        MenuItem::action("Open Database…", OpenDatabase),
        MenuItem::action("Connect…", Connect),
        MenuItem::action("New In-Memory Database", NewInMemory),
    ]);
    items
}

const CONNECT_HELP: &str = "PostgreSQL: postgres://user@host:5432/db   \
MySQL / MariaDB: mysql://user@host:3306/db   \
ClickHouse: clickhouse://user@host:8123/db (clickhouses:// for TLS)   \
SQLite / DuckDB: a file path, sqlite:path or duckdb:path";

/// Where saved passwords go, as the platform calls it.
const KEYCHAIN: &str = if cfg!(target_os = "macos") {
    "Keychain"
} else {
    "keyring"
};

/// Where the connection dialog's Test Connection stands.
enum TestState {
    Idle,
    Running,
    Passed(SharedString),
    Failed(SharedString),
}

/// What the connection dialog is for.
#[derive(Clone)]
enum ConnectionDialog {
    /// Connect to something new, which saves it.
    New,
    Edit(SavedConnection),
    /// Save the connection the window has open.
    SaveCurrent,
}

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
    /// The editor tabs; queries run from the active one.
    editors: Entity<Editors>,
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
    saved: saved::Store,
    secrets: Arc<dyn Secrets>,
    /// What the session is connected to, password included, so it can be
    /// saved after the fact.
    current: connect::Target,
    /// The saved connection the session is connected to.
    current_saved: Option<String>,
}

impl Workspace {
    /// `args` are the command-line arguments: `.sql` files to open, and a
    /// file path, connection URL or saved connection's name to connect to.
    /// Quarry starts on an in-memory DuckDB and connects to it from there.
    pub fn new(args: Vec<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let driver = DuckDbDriver::in_memory().expect("in-memory DuckDB always opens");
        let session = Session::spawn(driver);
        let current = connect::Target::InMemory(Dialect::DuckDb);
        let editors = cx.new(|cx| {
            let store = editor::Store::new(crate::config_dir());
            Editors::new(store, &current, session.flavor().clone(), window, cx)
        });
        // The tab strip draws itself; the editor below it is drawn here.
        cx.observe(&editors, |_, _, cx| cx.notify()).detach();
        let updater = update::Updater::global(cx);
        cx.observe(&updater, |_, _, cx| cx.notify()).detach();
        cx.subscribe_in(&updater, window, |_, _, event, window, cx| {
            update::show(event, window, cx)
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
            editors,
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
            saved: saved::Store::load_default(),
            secrets: saved::Keychain::shared(),
            current,
            current_saved: None,
        };
        let launch = connect::Launch::parse(args);
        for ignored in &launch.ignored {
            window.push_notification(
                format!("Quarry opens one database at a time; not opening {ignored}"),
                cx,
            );
        }
        this.editors.update(cx, |editors, cx| {
            editors.open_paths(launch.scripts, true, window, cx)
        });
        match launch.target {
            // A saved connection's name, unless a file has that name too.
            Some(arg) if !Path::new(&arg).exists() && this.saved_named(&arg).is_some() => {
                let connection = this.saved_named(&arg).cloned().expect("checked above");
                this.connect_saved(connection, window, cx);
            }
            Some(arg) => match connect::Target::parse(&arg) {
                Ok(target) => this.connect_to(target, window, cx),
                Err(err) => window.push_notification(format!("{err:#}"), cx),
            },
            None => this.refresh_catalog(&RefreshCatalog, window, cx),
        }
        let active = this.editors.read(cx).active_editor().clone();
        active.update(cx, |editor, cx| editor.focus(window, cx));
        this
    }

    fn saved_named(&self, name: &str) -> Option<&SavedConnection> {
        self.saved.all().iter().find(|c| c.name == name)
    }

    fn switch_session(
        &mut self,
        driver: Box<dyn Driver>,
        target: connect::Target,
        saved: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.current = target;
        self.current_saved = saved;
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
        // Its consoles come up, and tabs following the connection read as the
        // new engine.
        let flavor = self.session.flavor().clone();
        self.editors.update(cx, |editors, cx| {
            editors.set_connection(&self.current, flavor, window, cx)
        });
        self.refresh_catalog(&RefreshCatalog, window, cx);
        cx.notify();
    }

    /// Open `target` and switch to it.
    fn connect_to(&mut self, target: connect::Target, window: &mut Window, cx: &mut Context<Self>) {
        let saved = self.saved.find(&target).map(|c| c.id.clone());
        self.open_with(target.describe(), saved, move |_| Ok(target), window, cx);
    }

    /// Open a saved connection, with its password from the keychain.
    fn connect_saved(
        &mut self,
        connection: SavedConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (label, id) = (connection.name.clone(), connection.id.clone());
        self.open_with(
            label,
            Some(id),
            move |secrets| saved::resolve(&connection, secrets),
            window,
            cx,
        );
    }

    /// Work out the target and open it off the UI thread (servers block on
    /// the network, the keychain may ask the user), then switch to it.
    fn open_with(
        &mut self,
        label: String,
        saved: Option<String>,
        resolve: impl FnOnce(&dyn Secrets) -> anyhow::Result<connect::Target> + Send + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.connecting = Some(label.into());
        cx.notify();
        let secrets = self.secrets.clone();
        self.open_task = Some(cx.spawn_in(window, async move |this, cx| {
            let opened = cx
                .background_executor()
                .spawn(async move {
                    let target = resolve(&*secrets)?;
                    let driver = connect::open(&target)?;
                    anyhow::Ok((target, driver))
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.connecting = None;
                match opened {
                    Ok((target, driver)) => this.switch_session(driver, target, saved, window, cx),
                    Err(err) => {
                        window.push_notification(format!("Could not connect: {err:#}"), cx);
                        cx.notify();
                    }
                }
            });
        }));
    }

    /// Update the keychain off the UI thread, then save the connection and,
    /// for `connect`, open it.
    fn save_connection(
        &mut self,
        prepared: saved::Prepared,
        connect: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let secrets = self.secrets.clone();
        cx.spawn_in(window, async move |this, cx| {
            let applying = prepared.clone();
            let applied = cx
                .background_executor()
                .spawn(async move { applying.apply(&*secrets) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let mut connection = prepared.connection;
                let password = match applied {
                    Ok(password) => password,
                    Err(err) => {
                        window.push_notification(
                            format!("Could not update the password in the {KEYCHAIN}: {err:#}"),
                            cx,
                        );
                        connection.password =
                            this.saved.get(&connection.id).is_some_and(|c| c.password);
                        match prepared.secret {
                            Secret::Set(password) => Some(password),
                            _ => None,
                        }
                    }
                };
                if let Err(err) = this.saved.commit(connection.clone()) {
                    window.push_notification(format!("Could not save the connection: {err:#}"), cx);
                }
                if connect {
                    match (connection.target(), password) {
                        (Ok(target), Some(password)) => {
                            let target = target.with_password(&password);
                            let SavedConnection { id, name, .. } = connection;
                            this.open_with(name, Some(id), move |_| Ok(target), window, cx);
                        }
                        // No new password: the keychain's, if it holds one.
                        (Ok(_), None) => this.connect_saved(connection, window, cx),
                        (Err(err), _) => window.push_notification(format!("{err:#}"), cx),
                    }
                } else if this
                    .saved
                    .find(&this.current)
                    .is_some_and(|c| c.id == connection.id)
                {
                    this.current_saved = Some(connection.id);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn delete_saved(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let removed = match self.saved.remove(id) {
            Ok(removed) => removed,
            Err(err) => {
                window.push_notification(format!("Could not delete the connection: {err:#}"), cx);
                return;
            }
        };
        if self.current_saved.as_deref() == Some(id) {
            self.current_saved = None;
        }
        if let Some(connection) = removed.filter(|c| c.password) {
            let secrets = self.secrets.clone();
            cx.spawn_in(window, async move |_, cx| {
                let deleted = cx
                    .background_executor()
                    .spawn(async move { secrets.delete(&connection.id) })
                    .await;
                if let Err(err) = deleted {
                    log::warn!("deleting the password of {}: {err:#}", connection.name);
                }
            })
            .detach();
        }
        cx.notify();
    }

    fn confirm_delete(
        &mut self,
        connection: SavedConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (workspace, id) = (workspace.clone(), connection.id.clone());
            alert
                .confirm()
                .title(format!("Delete “{}”?", connection.name))
                .description(if connection.password {
                    format!(
                        "Quarry forgets this connection and the password it saved in the \
                         {KEYCHAIN}. Nothing in the database changes."
                    )
                } else {
                    "Quarry forgets this connection. Nothing in the database changes.".to_owned()
                })
                .ok_text("Delete")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let _ = workspace.update(cx, |this, cx| this.delete_saved(&id, window, cx));
                    true
                })
        });
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
            if connect::is_sql_script(&path) {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.editors.update(cx, |editors, cx| {
                        editors.open_paths(vec![path], false, window, cx)
                    })
                });
                return;
            }
            let target = connect::Target::File {
                dialect: connect::file_dialect(&path),
                path,
            };
            let _ = this.update_in(cx, |this, window, cx| this.connect_to(target, window, cx));
        })
        .detach();
    }

    /// Ask for a connection URL (and, separately, a password), save it to the
    /// connection list and connect.
    fn connect(&mut self, _: &Connect, window: &mut Window, cx: &mut Context<Self>) {
        self.open_connection_dialog(ConnectionDialog::New, window, cx);
    }

    fn open_connection_dialog(
        &mut self,
        kind: ConnectionDialog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (title, ok, name, input, password, saved_password) = match &kind {
            ConnectionDialog::New => (
                "Connect",
                "Connect",
                String::new(),
                String::new(),
                None,
                false,
            ),
            ConnectionDialog::Edit(c) => (
                "Edit Connection",
                "Save",
                c.name.clone(),
                c.target.clone(),
                None,
                c.password,
            ),
            ConnectionDialog::SaveCurrent => {
                let (target, password) = self.current.clone().take_password();
                (
                    "Save Connection",
                    "Save",
                    String::new(),
                    target.to_input(),
                    password,
                    false,
                )
            }
        };
        let name_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Name (optional)")
                .default_value(name)
        });
        let url = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("postgres://user@host:5432/database")
                .default_value(input)
        });
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(if saved_password {
                    format!("Saved in the {KEYCHAIN}; type a new one to replace it")
                } else {
                    "Password (optional)".to_owned()
                })
                .masked(true)
                .default_value(password.unwrap_or_default())
        });
        let remember = Rc::new(Cell::new(true));
        let test_state = Rc::new(RefCell::new(TestState::Idle));
        let secrets = self.secrets.clone();
        let workspace = cx.entity().downgrade();
        let initial_focus = Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if initial_focus.replace(false) {
                let first = match kind {
                    ConnectionDialog::New => &url,
                    _ => &name_input,
                };
                first.focus_handle(cx).focus(window, cx);
            }
            let muted = cx.theme().muted_foreground;
            let remember_toggle = remember.clone();
            let (name_ok, url_ok, password_ok) =
                (name_input.clone(), url.clone(), password.clone());
            let (remember_ok, kind_ok, workspace) =
                (remember.clone(), kind.clone(), workspace.clone());
            let run_test = {
                let (url, password, kind, secrets, state) = (
                    url.clone(),
                    password.clone(),
                    kind.clone(),
                    secrets.clone(),
                    test_state.clone(),
                );
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let input = url.read(cx).value().to_string();
                    let password = password.read(cx).value().to_string();
                    let saved = match &kind {
                        ConnectionDialog::Edit(c) => Some(c.clone()),
                        _ => None,
                    };
                    let secrets = secrets.clone();
                    let state = state.clone();
                    *state.borrow_mut() = TestState::Running;
                    window.refresh();
                    window
                        .spawn(cx, async move |cx| {
                            let result = cx
                                .background_executor()
                                .spawn(async move {
                                    saved::test(&input, &password, saved.as_ref(), &*secrets)
                                })
                                .await;
                            *state.borrow_mut() = match result {
                                Ok(message) => TestState::Passed(message.into()),
                                Err(err) => TestState::Failed(format!("{err:#}").into()),
                            };
                            let _ = cx.update(|window, _| window.refresh());
                        })
                        .detach();
                }
            };
            let testing = matches!(*test_state.borrow(), TestState::Running);
            let test_result = match &*test_state.borrow() {
                TestState::Idle => None,
                TestState::Running => Some(("Testing…".into(), muted)),
                TestState::Passed(message) => Some((message.clone(), cx.theme().success)),
                TestState::Failed(message) => Some((message.clone(), cx.theme().danger)),
            };
            dialog
                .title(title)
                .w(px(520.))
                .child(
                    v_flex()
                        .gap_2()
                        .child(Input::new(&name_input))
                        .child(Input::new(&url))
                        .child(Input::new(&password))
                        .child(
                            Checkbox::new("remember")
                                .label(format!("Remember the password in the {KEYCHAIN}"))
                                .text_sm()
                                .checked(remember.get())
                                .on_click(
                                    move |checked: &bool, window: &mut Window, _: &mut App| {
                                        remember_toggle.set(*checked);
                                        window.refresh();
                                    },
                                ),
                        )
                        .child(div().text_xs().text_color(muted).child(CONNECT_HELP))
                        .when_some(test_result, |this, (message, color)| {
                            this.child(div().text_sm().text_color(color).child(message))
                        }),
                )
                .button_props(DialogButtonProps::default().show_cancel(true).ok_text(ok))
                // A plain dialog draws no buttons of its own.
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("connection-test")
                                .label("Test Connection")
                                .ghost()
                                .loading(testing)
                                .disabled(testing)
                                .on_click(run_test),
                        )
                        .child(div().flex_1())
                        .child(DialogClose::new().trigger(|button| button.label("Cancel")))
                        .child(
                            DialogAction::new()
                                .child(Button::new("connection-ok").primary().label(ok)),
                        ),
                )
                .on_ok(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let name = name_ok.read(cx).value().to_string();
                    let input = url_ok.read(cx).value().to_string();
                    let password = password_ok.read(cx).value().to_string();
                    let remember = remember_ok.get();
                    workspace
                        .update(cx, |this, cx| {
                            this.submit_connection(
                                &kind_ok, &name, &input, &password, remember, window, cx,
                            )
                        })
                        .unwrap_or(true)
                })
        });
    }

    /// Save what the connection dialog holds and, for a new connection,
    /// connect to it. `false` keeps the dialog open, for input that does not
    /// parse.
    #[allow(clippy::too_many_arguments)]
    fn submit_connection(
        &mut self,
        kind: &ConnectionDialog,
        name: &str,
        input: &str,
        password: &str,
        remember: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let (editing, connect) = match kind {
            ConnectionDialog::New => (None, true),
            ConnectionDialog::Edit(c) => (Some(c.id.as_str()), false),
            ConnectionDialog::SaveCurrent => (None, false),
        };
        match self.saved.prepare(editing, name, input, password, remember) {
            Ok(prepared) => {
                self.save_connection(prepared, connect, window, cx);
                true
            }
            Err(err) => {
                window.push_notification(format!("{err:#}"), cx);
                false
            }
        }
    }

    fn new_in_memory(&mut self, _: &NewInMemory, window: &mut Window, cx: &mut Context<Self>) {
        match DuckDbDriver::in_memory() {
            Ok(driver) => self.switch_session(
                Box::new(driver),
                connect::Target::InMemory(Dialect::DuckDb),
                None,
                window,
                cx,
            ),
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
        let statements = self.editors.read(cx).statements_at_caret(cx);
        self.run(statements, window, cx);
    }

    fn run_script(&mut self, _: &RunScript, window: &mut Window, cx: &mut Context<Self>) {
        let statements = self.editors.read(cx).all_statements(cx);
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
            .child(editor::flavor_picker(&self.editors, cx))
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = cx.entity().downgrade();
        let target = SharedString::from(self.session.target().to_owned());
        v_flex()
            .size_full()
            .child(self.render_connections(cx))
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
                        Button::new("save-connection")
                            .icon(Icon::new(gpui_kit::assets::IconName::BookmarkPlus))
                            .xsmall()
                            .ghost()
                            .disabled(
                                self.current_saved.is_some()
                                    || matches!(self.current, connect::Target::InMemory(_)),
                            )
                            .tooltip("Save this connection")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_connection_dialog(
                                    ConnectionDialog::SaveCurrent,
                                    window,
                                    cx,
                                )
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

    /// The saved connections: click one to connect, right-click to edit or
    /// delete it.
    fn render_connections(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let workspace = cx.entity().downgrade();
        let rows = self.saved.all().iter().enumerate().map(|(ix, connection)| {
            let target = connection.target().ok();
            let icon = if target.as_ref().is_some_and(connect::Target::is_server) {
                gpui_kit::assets::IconName::Server
            } else {
                gpui_kit::assets::IconName::HardDrive
            };
            let detail = target.map_or_else(|| connection.target.clone(), |t| t.describe());
            let active = self.current_saved.as_deref() == Some(connection.id.as_str());
            let on = |run: fn(&mut Self, SavedConnection, &mut Window, &mut Context<Self>)| {
                let (workspace, connection) = (workspace.clone(), connection.clone());
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let _ =
                        workspace.update(cx, |this, cx| run(this, connection.clone(), window, cx));
                }
            };
            let connect = on(Self::connect_saved);
            let (menu_connect, edit, delete) = (
                on(Self::connect_saved),
                on(|this, c, window, cx| {
                    this.open_connection_dialog(ConnectionDialog::Edit(c), window, cx)
                }),
                on(Self::confirm_delete),
            );
            let (menu_connect, edit, delete) =
                (Rc::new(menu_connect), Rc::new(edit), Rc::new(delete));
            div()
                .id(SharedString::from(format!("saved-{}", connection.id)))
                .context_menu(move |menu, _, _| {
                    let (menu_connect, edit, delete) =
                        (menu_connect.clone(), edit.clone(), delete.clone());
                    menu.item(
                        PopupMenuItem::new("Connect")
                            .on_click(move |e, window, cx| menu_connect(e, window, cx)),
                    )
                    .item(
                        PopupMenuItem::new("Edit…")
                            .on_click(move |e, window, cx| edit(e, window, cx)),
                    )
                    .separator()
                    .item(
                        PopupMenuItem::new("Delete…")
                            .on_click(move |e, window, cx| delete(e, window, cx)),
                    )
                })
                .child(
                    ListItem::new(ix)
                        .selected(active)
                        .px_2()
                        .child(
                            h_flex()
                                .gap_2()
                                .min_w_0()
                                .child(Icon::new(icon).small().text_color(muted))
                                .child(
                                    v_flex()
                                        .min_w_0()
                                        .child(
                                            div()
                                                .text_sm()
                                                .truncate()
                                                .child(connection.name.clone()),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(muted)
                                                .truncate()
                                                .child(detail),
                                        ),
                                ),
                        )
                        .on_click(connect),
                )
        });
        v_flex()
            .flex_shrink_0()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .font_semibold()
                            .text_color(muted)
                            .child("CONNECTIONS"),
                    )
                    .child(
                        Button::new("new-connection")
                            .icon(Icon::new(gpui_kit::assets::IconName::Plus))
                            .xsmall()
                            .ghost()
                            .tooltip("New connection (⇧⌘O)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.connect(&Connect, window, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .id("saved-connections")
                    .max_h(px(240.))
                    .overflow_y_scroll()
                    .pb_1()
                    .children(rows),
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
            .children(update::status_item(cx))
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
        let root = v_flex().id("workspace");
        editor::bind_actions(root, &self.editors)
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
                                                .child(self.editors.clone())
                                                .child(self.render_toolbar(cx))
                                                .child(
                                                    div().flex_1().min_h_0().child(
                                                        self.editors.read(cx).editor_element(),
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
