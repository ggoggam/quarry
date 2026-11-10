//! The SQL editor: a strip of tabs, each with its own buffer, caret and undo
//! history.
//!
//! A tab is a console, a scratch or a file ([`TabKind`]):
//!
//! - **Consoles** belong to a connection, like DataGrip's query consoles: the
//!   strip shows the consoles of the connection the window is on, and
//!   switching connections swaps them for the new connection's (opening one
//!   if it has none). Their text is saved as you type, under Quarry's
//!   settings folder, and comes back after a restart.
//! - **Scratches** are throwaway buffers saved the same way, and show
//!   whatever the connection.
//! - **Files** are `.sql` files on disk. They save when asked, show a dot
//!   while they have unsaved changes, and ask before closing with them.
//!   Quitting keeps those changes in the session and brings them back.
//!
//! [`Editors`] owns the tabs. The rest of the app reaches the editor through
//! [`Editors::active_editor`], [`Editors::active_flavor`] and the statement
//! helpers, and follows tabs through [`EditorsEvent`].

mod session;
mod tabs;
mod view;

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui_kit::base::input::Rope;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::input::{EditorState, InputEvent};
use gpui_kit::{
    App, AppContext as _, Context, Entity, EventEmitter, PromptButton, PromptLevel, SharedString,
    Subscription, Task, Window,
};
use quarry_core::{Flavor, sql};

use crate::{connect, highlight};
pub use session::Store;
use session::{SavedFlavor, SavedTab, Session};
use tabs::Tabs;
pub use tabs::{TabId, TabKind, connection_key};
pub use view::{EDITOR_CONTEXT, bind_actions, flavor_picker, init, menu_items};

/// How long typing pauses before a console or scratch is written.
const AUTOSAVE_DELAY: Duration = Duration::from_millis(750);
/// How long the tabs stay put before the session is written.
const SESSION_DELAY: Duration = Duration::from_millis(300);
/// How many closed tabs ⇧⌘T can bring back.
const CLOSED_LIMIT: usize = 20;

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

/// One tab's editor and what Quarry knows about its text.
pub struct Buffer {
    editor: Entity<EditorState>,
    /// The text as last read from or written to disk.
    saved: Rope,
    /// The text differs from `saved`.
    dirty: bool,
    /// Its text is still being read; the editor is read-only meanwhile.
    loading: bool,
    /// The unsaved text the last session kept, until the file is read.
    pending_draft: Option<String>,
    /// The flavor picked for this tab; `None` follows the connection.
    flavor: Option<Flavor>,
    /// The flavor the highlighter and version notes were last set up for.
    applied: Flavor,
    autosave: Option<Task<()>>,
    _changes: Subscription,
}

/// A closed tab ⇧⌘T can reopen. A file reopens from disk; a console or
/// scratch from the text it had.
struct Closed {
    kind: TabKind,
    text: Option<String>,
    flavor: Option<Flavor>,
}

/// What happened to the tabs, for parts of the window that follow them.
#[allow(dead_code, reason = "nothing follows tabs by id yet")]
pub enum EditorsEvent {
    Opened(TabId),
    /// Another tab became the active one (also when a connection switch
    /// brings up its console).
    Activated(TabId),
    Closed(TabId),
}

pub struct Editors {
    tabs: Tabs<Buffer>,
    store: Store,
    connection_flavor: Flavor,
    /// The connection, as the sidebar describes it.
    connection_label: SharedString,
    /// No session was found, so this is Quarry's first launch.
    first_run: bool,
    closed: Vec<Closed>,
    session_task: Option<Task<()>>,
    /// A save-changes prompt is open; the platform allows one at a time.
    prompting: bool,
    /// Where the last file dialog was, to open the next one there.
    last_dir: Option<PathBuf>,
    _quit: Subscription,
}

impl EventEmitter<EditorsEvent> for Editors {}

impl Editors {
    /// The tabs of the last session, on `connection`. Reads `editors.json`
    /// now and the tabs' text in the background.
    pub fn new(
        store: Store,
        connection: &connect::Target,
        flavor: Flavor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let session = store.load_session();
        let mut this = Self {
            tabs: Tabs::new(connection_key(connection)),
            store,
            connection_flavor: flavor,
            connection_label: connection.describe().into(),
            first_run: session.is_none(),
            closed: Vec::new(),
            session_task: None,
            prompting: false,
            last_dir: None,
            _quit: cx.on_app_quit(|this, cx| {
                this.flush(cx);
                async {}
            }),
        };
        // Closing the window drops the tabs without quitting Quarry.
        let editors = cx.entity().downgrade();
        window.on_window_should_close(cx, move |_, cx| {
            let _ = editors.update(cx, |this, cx| this.flush(cx));
            true
        });
        if let Some(session) = session {
            this.restore(session, window, cx);
        }
        this.ensure_console(window, cx);
        this
    }

    fn restore(&mut self, session: Session, window: &mut Window, cx: &mut Context<Self>) {
        let mut ids = Vec::with_capacity(session.tabs.len());
        let mut loads = Vec::new();
        for saved in session.tabs {
            let duplicate = match &saved.kind {
                TabKind::File { path } => self.tabs.find_file(path).is_some(),
                kind => self.tabs.name_taken(kind),
            };
            let path = self.store.path_of(&saved.kind);
            if duplicate || path.is_none() {
                ids.push(None);
                continue;
            }
            let flavor = saved.flavor.as_ref().and_then(SavedFlavor::flavor);
            let id = self.add_tab(saved.kind, "", flavor, window, cx);
            if let Some(tab) = self.tabs.get_mut(id) {
                tab.buffer.loading = true;
                tab.buffer.pending_draft = saved.draft;
            }
            ids.push(Some(id));
            loads.extend(path.map(|path| (id, path)));
        }
        if let Some(id) = session.active.and_then(|ix| ids.get(ix).copied().flatten()) {
            self.tabs.activate(id);
        }
        let read = cx.background_executor().spawn(async move {
            loads
                .into_iter()
                .map(|(id, path)| (id, session::read_text(&path)))
                .collect::<Vec<_>>()
        });
        cx.spawn_in(window, async move |this, cx| {
            let read = read.await;
            let _ = this.update_in(cx, |this, window, cx| {
                for (id, text) in read {
                    this.loaded(id, text, window, cx);
                }
                this.ensure_console(window, cx);
                this.save_session(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// A restored tab's text arrived.
    fn loaded(
        &mut self,
        id: TabId,
        text: anyhow::Result<Option<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get_mut(id) else {
            return;
        };
        let draft = tab.buffer.pending_draft.take();
        let text = match (text, &tab.kind) {
            (Ok(Some(text)), _) => text,
            // An empty console or scratch has no file.
            (Ok(None), kind) if kind.is_autosaved() => String::new(),
            // A file that went away keeps its unsaved text, to save again.
            (Ok(None), _) if draft.is_some() => String::new(),
            (result, kind) => {
                let why = match result {
                    Err(err) => format!("{err:#}"),
                    Ok(_) => "it is no longer there".to_owned(),
                };
                let message = format!("Could not reopen {}: {why}", kind.file_name());
                self.tabs.remove(id);
                window.push_notification(message, cx);
                return;
            }
        };
        let buffer = &mut tab.buffer;
        buffer.loading = false;
        buffer.editor.update(cx, |editor, cx| {
            editor.set_value(text.as_str(), window, cx);
        });
        buffer.saved = buffer.editor.read(cx).text().clone();
        if let Some(draft) = draft {
            buffer.editor.update(cx, |editor, cx| {
                editor.set_value(draft.as_str(), window, cx);
            });
        }
        self.changed(id, cx);
    }

    // ── The API the rest of the app uses ─────────────────────────────────────

    /// The editor of the active tab. There always is one: closing the last
    /// tab opens a new console.
    pub fn active_editor(&self) -> &Entity<EditorState> {
        &self.active_buffer().editor
    }

    pub fn active_id(&self) -> TabId {
        self.tabs
            .active_id()
            .expect("there is always an active tab")
    }

    pub fn active_kind(&self) -> &TabKind {
        &self
            .tabs
            .active()
            .expect("there is always an active tab")
            .kind
    }

    fn active_buffer(&self) -> &Buffer {
        &self
            .tabs
            .active()
            .expect("there is always an active tab")
            .buffer
    }

    /// The flavor the active tab is written in: its picked one, or the
    /// connection's.
    pub fn active_flavor(&self) -> Flavor {
        self.effective_flavor(self.active_buffer())
    }

    /// The flavor picked for the active tab, if it does not follow the
    /// connection.
    pub fn picked_flavor(&self) -> Option<&Flavor> {
        self.active_buffer().flavor.as_ref()
    }

    pub fn connection_flavor(&self) -> &Flavor {
        &self.connection_flavor
    }

    /// The statement under the caret, or what is selected split into
    /// statements, in the active tab.
    pub fn statements_at_caret(&self, cx: &App) -> Vec<String> {
        let editor = self.active_editor().read(cx);
        let text = editor.value().to_string();
        let selected = editor.selected_range();
        let dialect = self.active_flavor().dialect;
        if selected.is_empty() {
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
        }
    }

    /// Every statement in the active tab.
    pub fn all_statements(&self, cx: &App) -> Vec<String> {
        let text = self.active_editor().read(cx).value().to_string();
        sql::split_statements_for(&text, self.active_flavor().dialect)
            .iter()
            .map(|span| span.text(&text).to_owned())
            .collect()
    }

    pub fn focus_active(&self, window: &mut Window, cx: &mut App) {
        self.active_editor()
            .update(cx, |editor, cx| editor.focus(window, cx));
    }

    /// Show `connection`'s consoles, opening one if it has none.
    pub fn set_connection(
        &mut self,
        connection: &connect::Target,
        flavor: Flavor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.connection_flavor = flavor;
        self.connection_label = connection.describe().into();
        self.tabs.set_connection(connection_key(connection));
        self.ensure_console(window, cx);
        self.activated(window, cx);
    }

    /// Pick the flavor the active tab is written in; `None` follows the
    /// connection.
    pub fn set_picked_flavor(&mut self, flavor: Option<Flavor>, cx: &mut Context<Self>) {
        let id = self.active_id();
        if let Some(tab) = self.tabs.active_mut() {
            tab.buffer.flavor = flavor;
        }
        self.sync_flavor(id, cx);
        self.save_session(cx);
        cx.notify();
    }

    // ── Tabs ─────────────────────────────────────────────────────────────────

    fn effective_flavor(&self, buffer: &Buffer) -> Flavor {
        buffer
            .flavor
            .clone()
            .unwrap_or_else(|| self.connection_flavor.clone())
    }

    /// Add a tab at the end of the strip, not activated.
    fn add_tab(
        &mut self,
        kind: TabKind,
        text: &str,
        flavor: Option<Flavor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TabId {
        let id = self.tabs.alloc_id();
        let applied = flavor
            .clone()
            .unwrap_or_else(|| self.connection_flavor.clone());
        let editor = cx.new(|cx| {
            let mut editor = EditorState::new(window, cx)
                .language("sql")
                .default_value(text.to_owned());
            editor.set_highlighter_factory(highlight::factory(applied.clone()), cx);
            editor
        });
        let changes = cx.subscribe(&editor, move |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.changed(id, cx);
            }
        });
        editor.update(cx, |editor, cx| {
            highlight::update_version_notes(editor, &applied, cx)
        });
        let saved = editor.read(cx).text().clone();
        self.tabs.insert(
            id,
            kind,
            Buffer {
                editor,
                saved,
                dirty: false,
                loading: false,
                pending_draft: None,
                flavor,
                applied,
                autosave: None,
                _changes: changes,
            },
        );
        cx.emit(EditorsEvent::Opened(id));
        id
    }

    /// Make `id` the active tab and put the caret in it.
    pub fn activate(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.activate(id) {
            self.activated(window, cx);
        }
    }

    fn activated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.active_id();
        self.sync_flavor(id, cx);
        self.focus_active(window, cx);
        cx.emit(EditorsEvent::Activated(id));
        self.save_session(cx);
        cx.notify();
    }

    /// Re-highlight and re-check a tab whose flavor changed: picked anew, or
    /// following a connection that changed while the tab was hidden.
    fn sync_flavor(&mut self, id: TabId, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(id) else {
            return;
        };
        let flavor = self.effective_flavor(&tab.buffer);
        if flavor == tab.buffer.applied {
            return;
        }
        let editor = tab.buffer.editor.clone();
        editor.update(cx, |editor, cx| {
            editor.set_highlighter_factory(highlight::factory(flavor.clone()), cx);
            highlight::update_version_notes(editor, &flavor, cx);
        });
        if let Some(tab) = self.tabs.get_mut(id) {
            tab.buffer.applied = flavor;
        }
    }

    /// The text of tab `id` changed.
    fn changed(&mut self, id: TabId, cx: &mut Context<Self>) {
        let connection_flavor = self.connection_flavor.clone();
        let Some(tab) = self.tabs.get_mut(id) else {
            return;
        };
        let buffer = &mut tab.buffer;
        let flavor = buffer.flavor.clone().unwrap_or(connection_flavor);
        let dirty = *buffer.editor.read(cx).text() != buffer.saved;
        buffer.editor.update(cx, |editor, cx| {
            highlight::update_version_notes(editor, &flavor, cx)
        });
        buffer.applied = flavor;
        if dirty != buffer.dirty {
            buffer.dirty = dirty;
            cx.notify();
        }
        if dirty && tab.kind.is_autosaved() {
            self.autosave(id, AUTOSAVE_DELAY, cx);
        }
    }

    /// Open a console for the connection if it has none, so the strip is
    /// never without it.
    fn ensure_console(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let connection = self.tabs.connection().to_owned();
        if self.tabs.has_console(&connection) {
            return;
        }
        let welcome = self.first_run && connection == ":memory:";
        let name = self.tabs.console_name(&connection);
        let id = self.add_tab(
            TabKind::Console { connection, name },
            if welcome { WELCOME_SQL } else { "" },
            None,
            window,
            cx,
        );
        if welcome {
            // Kept like anything typed, so it is there next time too.
            self.first_run = false;
            self.autosave(id, Duration::ZERO, cx);
        }
        if !self.tabs.active().is_some_and(|t| {
            matches!(&t.kind, TabKind::Console { connection: c, .. } if c == self.tabs.connection())
        }) {
            self.tabs.activate(id);
        }
        self.save_session(cx);
    }

    pub fn new_console(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let connection = self.tabs.connection().to_owned();
        let name = self.tabs.console_name(&connection);
        let id = self.add_tab(TabKind::Console { connection, name }, "", None, window, cx);
        self.activate(id, window, cx);
    }

    pub fn new_scratch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.tabs.scratch_name();
        let id = self.add_tab(TabKind::Scratch { name }, "", None, window, cx);
        self.activate(id, window, cx);
    }

    pub fn next_tab(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.cycle(delta).is_some() {
            self.activated(window, cx);
        }
    }

    /// Activate the strip's `ix`th tab, or its last for `None`.
    pub fn select_tab(&mut self, ix: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.activate_index(ix).is_some() {
            self.activated(window, cx);
        }
    }

    /// Close tab `id`, asking first when it is a file with unsaved changes.
    pub fn close(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(id) else {
            return;
        };
        if tab.kind.is_autosaved() || !tab.buffer.dirty {
            self.discard(id, window, cx);
            return;
        }
        if std::mem::replace(&mut self.prompting, true) {
            return;
        }
        let name = tab.kind.file_name();
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Do you want to save the changes you made to “{name}”?"),
            Some("Your changes will be lost if you don’t save them."),
            &[
                PromptButton::ok("Save"),
                PromptButton::new("Don’t Save"),
                PromptButton::cancel("Cancel"),
            ],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let answer = answer.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.prompting = false;
                match answer {
                    Ok(0) => this.save(id, true, window, cx),
                    Ok(1) => this.discard(id, window, cx),
                    _ => {}
                }
            });
        })
        .detach();
    }

    pub fn close_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.active_id();
        self.close(id, window, cx);
    }

    /// Close tab `id` without saving. A console's or scratch's file goes
    /// with it, but ⇧⌘T can bring the tab back while Quarry runs.
    fn discard(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let was_active = self.tabs.active_id() == Some(id);
        let Some(tab) = self.tabs.remove(id) else {
            return;
        };
        let text = tab
            .kind
            .is_autosaved()
            .then(|| tab.buffer.editor.read(cx).value().to_string());
        if tab.kind.is_autosaved()
            && let Some(path) = self.store.path_of(&tab.kind)
        {
            cx.background_executor()
                .spawn(async move {
                    if let Err(err) = session::remove(&path) {
                        log::warn!("{err:#}");
                    }
                })
                .detach();
        }
        self.closed.push(Closed {
            kind: tab.kind,
            text,
            flavor: tab.buffer.flavor,
        });
        if self.closed.len() > CLOSED_LIMIT {
            self.closed.remove(0);
        }
        cx.emit(EditorsEvent::Closed(id));
        if self.tabs.shown().next().is_none() {
            self.new_console(window, cx);
        } else if was_active {
            self.activated(window, cx);
        } else {
            self.save_session(cx);
            cx.notify();
        }
    }

    /// Bring back the last closed tab that belongs in the strip.
    pub fn reopen_closed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let connection = self.tabs.connection().to_owned();
        let Some(ix) = self
            .closed
            .iter()
            .rposition(|c| c.kind.shows_on(&connection))
        else {
            return;
        };
        let Closed {
            mut kind,
            text,
            flavor,
        } = self.closed.remove(ix);
        match (&mut kind, text) {
            (TabKind::File { path }, _) => {
                self.open_paths(vec![path.clone()], false, window, cx);
            }
            (_, text) => {
                if self.tabs.name_taken(&kind) {
                    match &mut kind {
                        TabKind::Console { connection, name } => {
                            *name = self.tabs.console_name(connection)
                        }
                        TabKind::Scratch { name } => *name = self.tabs.scratch_name(),
                        TabKind::File { .. } => {}
                    }
                }
                let text = text.unwrap_or_default();
                let id = self.add_tab(kind, "", flavor, window, cx);
                if let Some(tab) = self.tabs.get(id) {
                    tab.buffer.editor.update(cx, |editor, cx| {
                        editor.set_value(text.as_str(), window, cx);
                    });
                }
                // Its file was removed on close; write it again.
                self.changed(id, cx);
                self.activate(id, window, cx);
            }
        }
    }

    // ── Files ────────────────────────────────────────────────────────────────

    /// Ask for `.sql` files and open them.
    pub fn prompt_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Open".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.open_paths(paths, false, window, cx)
            });
        })
        .detach();
    }

    /// Open files in tabs, or bring up the tabs that have them open. With
    /// `create_missing`, a path with no file opens empty and is created on
    /// save (as from the command line); otherwise it is an error.
    pub fn open_paths(
        &mut self,
        paths: Vec<PathBuf>,
        create_missing: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        let read = cx.background_executor().spawn(async move {
            paths
                .into_iter()
                .map(|path| {
                    let path = std::fs::canonicalize(&path)
                        .or_else(|_| std::path::absolute(&path))
                        .unwrap_or(path);
                    let text = session::read_text(&path);
                    (path, text)
                })
                .collect::<Vec<_>>()
        });
        cx.spawn_in(window, async move |this, cx| {
            let read = read.await;
            let _ = this.update_in(cx, |this, window, cx| {
                let mut last = None;
                for (path, text) in read {
                    this.last_dir = path.parent().map(Path::to_owned);
                    if let Some(id) = this.tabs.find_file(&path) {
                        last = Some(id);
                        continue;
                    }
                    let text = match text {
                        Ok(Some(text)) => text,
                        Ok(None) if create_missing => String::new(),
                        Ok(None) => {
                            let message =
                                format!("Could not open {}: no such file", path.display());
                            window.push_notification(message, cx);
                            continue;
                        }
                        Err(err) => {
                            let message = format!("Could not open {}: {err:#}", path.display());
                            window.push_notification(message, cx);
                            continue;
                        }
                    };
                    last = Some(this.add_tab(TabKind::File { path }, &text, None, window, cx));
                }
                match last {
                    Some(id) => this.activate(id, window, cx),
                    None => cx.notify(),
                }
            });
        })
        .detach();
    }

    /// Ask where to put a new `.sql` file, create it and open it.
    pub fn prompt_new_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = cx.prompt_for_new_path(&self.dialog_dir(), Some("untitled.sql"));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = path.await else {
                return;
            };
            let created = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    // The dialog already asked before replacing a file.
                    async move { session::write_file(&path, &Rope::new()) }
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| match created {
                Ok(()) => this.open_paths(vec![path], false, window, cx),
                Err(err) => {
                    let message = format!("Could not create {}: {err:#}", path.display());
                    window.push_notification(message, cx);
                }
            });
        })
        .detach();
    }

    /// ⌘S: write a file, asking where for none, or write a console or scratch
    /// now rather than when typing pauses.
    pub fn save_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.active_id();
        self.save(id, false, window, cx);
    }

    fn save(&mut self, id: TabId, then_close: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(id) else {
            return;
        };
        match &tab.kind {
            TabKind::File { path } => {
                let path = path.clone();
                self.write_file(id, path, then_close, window, cx);
            }
            _ => {
                if tab.buffer.dirty {
                    self.autosave(id, Duration::ZERO, cx);
                }
                if then_close {
                    self.discard(id, window, cx);
                }
            }
        }
    }

    /// ⇧⌘S: write the active tab to a file of the user's choosing, which the
    /// tab then edits. A console or scratch saved this way becomes that file.
    pub fn save_active_as(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.active_id();
        let kind = self.active_kind();
        let name = kind.file_name();
        let dir = match kind {
            TabKind::File { path } => path.parent().map(Path::to_owned),
            _ => None,
        }
        .unwrap_or_else(|| self.dialog_dir());
        let path = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = path.await else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.write_file(id, path, false, window, cx)
            });
        })
        .detach();
    }

    /// Write tab `id` to `path` off the UI thread. When the tab was a
    /// console, a scratch or another file, it becomes the file at `path`.
    fn write_file(
        &mut self,
        id: TabId,
        path: PathBuf,
        then_close: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(id) else {
            return;
        };
        let text = tab.buffer.editor.read(cx).text().clone();
        let written = text.clone();
        let write = cx.background_executor().spawn(async move {
            session::write_file(&path, &text)?;
            anyhow::Ok(std::fs::canonicalize(&path).unwrap_or(path))
        });
        cx.spawn_in(window, async move |this, cx| {
            let written_to = write.await;
            let _ = this.update_in(cx, |this, window, cx| match written_to {
                Ok(path) => {
                    this.wrote_file(id, path, written, cx);
                    if then_close {
                        this.discard(id, window, cx);
                    }
                }
                Err(err) => {
                    let message = format!("Could not save: {err:#}");
                    window.push_notification(message, cx);
                }
            });
        })
        .detach();
    }

    fn wrote_file(&mut self, id: TabId, path: PathBuf, written: Rope, cx: &mut Context<Self>) {
        self.last_dir = path.parent().map(Path::to_owned);
        // Another tab showing the file that was just replaced is out of date.
        if let Some(other) = self.tabs.find_file(&path).filter(|&other| other != id) {
            self.tabs.remove(other);
            cx.emit(EditorsEvent::Closed(other));
        }
        let Some(tab) = self.tabs.get_mut(id) else {
            return;
        };
        let kind = TabKind::File { path };
        if tab.kind != kind {
            let old = std::mem::replace(&mut tab.kind, kind);
            tab.buffer.autosave = None;
            if old.is_autosaved()
                && let Some(old_path) = self.store.path_of(&old)
            {
                cx.background_executor()
                    .spawn(async move { session::remove(&old_path) })
                    .detach();
            }
        }
        let tab = self.tabs.get_mut(id).expect("checked above");
        tab.buffer.saved = written;
        tab.buffer.dirty = *tab.buffer.editor.read(cx).text() != tab.buffer.saved;
        self.save_session(cx);
        cx.notify();
    }

    /// Write a console's or scratch's text after `delay`, replacing a write
    /// already waiting.
    fn autosave(&mut self, id: TabId, delay: Duration, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(id) else {
            return;
        };
        let Some(path) = self.store.path_of(&tab.kind) else {
            return;
        };
        tab.buffer.autosave = Some(cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            let Ok(Some(text)) = this.update(cx, |this, cx| {
                let tab = this.tabs.get(id)?;
                Some(tab.buffer.editor.read(cx).text().clone())
            }) else {
                return;
            };
            let written = text.clone();
            let saved = cx
                .background_executor()
                .spawn(async move { session::write_autosaved(&path, &text) })
                .await;
            let _ = this.update(cx, |this, cx| {
                let Some(tab) = this.tabs.get_mut(id) else {
                    return;
                };
                match saved {
                    Ok(()) => {
                        tab.buffer.saved = written;
                        tab.buffer.dirty = *tab.buffer.editor.read(cx).text() != tab.buffer.saved;
                    }
                    Err(err) => log::warn!("{err:#}"),
                }
            });
        }));
    }

    fn dialog_dir(&self) -> PathBuf {
        let active = match self.active_kind() {
            TabKind::File { path } => path.parent().map(Path::to_owned),
            _ => None,
        };
        active
            .or_else(|| self.last_dir.clone())
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    // ── Session ──────────────────────────────────────────────────────────────

    fn snapshot(&self, cx: &App) -> Session {
        let mut tabs = Vec::new();
        let mut active = None;
        for tab in self.tabs.iter() {
            let buffer = &tab.buffer;
            let text = buffer.editor.read(cx).text();
            // An empty console of a connection the window is not on would
            // only be opened again when it comes back.
            if matches!(&tab.kind, TabKind::Console { connection, .. }
                    if connection != self.tabs.connection())
                && !buffer.loading
                && text.len() == 0
            {
                continue;
            }
            if self.tabs.active_id() == Some(tab.id()) {
                active = Some(tabs.len());
            }
            let draft = if buffer.loading {
                buffer.pending_draft.clone()
            } else {
                (buffer.dirty && !tab.kind.is_autosaved()).then(|| text.to_string())
            };
            tabs.push(SavedTab {
                kind: tab.kind.clone(),
                flavor: buffer.flavor.as_ref().map(SavedFlavor::of),
                draft,
            });
        }
        Session {
            version: 1,
            tabs,
            active,
        }
    }

    /// Write the session shortly, once the tabs settle.
    fn save_session(&mut self, cx: &mut Context<Self>) {
        self.session_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SESSION_DELAY).await;
            let Ok((store, session)) =
                this.update(cx, |this, cx| (this.store.clone(), this.snapshot(cx)))
            else {
                return;
            };
            let written = cx
                .background_executor()
                .spawn(async move { store.write_session(&session) })
                .await;
            if let Err(err) = written {
                log::warn!("{err:#}");
            }
        }));
    }

    /// Write everything now: Quarry is quitting or the window closing, so
    /// there is no later to do it in.
    fn flush(&mut self, cx: &mut Context<Self>) {
        for tab in self.tabs.iter_mut() {
            tab.buffer.autosave = None;
            if !(tab.kind.is_autosaved() && tab.buffer.dirty) {
                continue;
            }
            if let Some(path) = self.store.path_of(&tab.kind)
                && let Err(err) = session::write_autosaved(&path, tab.buffer.editor.read(cx).text())
            {
                log::warn!("{err:#}");
            }
        }
        self.session_task = None;
        if let Err(err) = self.store.write_session(&self.snapshot(cx)) {
            log::warn!("{err:#}");
        }
    }
}
