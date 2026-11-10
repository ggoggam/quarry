//! The editor tabs as plain data: what each tab is, their order, which one
//! is active, and how they are named and titled. No GPUI here, so the rules
//! are unit-tested; [`super::Editors`] keeps a [`Tabs`] of its buffers.
//!
//! A console belongs to one connection, and only the consoles of the
//! connection the window is on are shown. Scratches and files belong to no
//! connection and are always shown; they run on whatever the window is
//! connected to.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Identifies a tab for as long as Quarry runs. Not persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TabId(pub(super) u64);

/// What a tab edits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TabKind {
    /// A query console of `connection` (a [`connection_key`]), auto-saved in
    /// Quarry's settings folder as `<name>.sql`.
    Console { connection: String, name: String },
    /// A scratch file, auto-saved in Quarry's settings folder as
    /// `<name>.sql`.
    Scratch { name: String },
    /// A SQL file on disk, written when the user saves it.
    File { path: PathBuf },
}

impl TabKind {
    /// Consoles and scratches save themselves; files wait for Save.
    pub fn is_autosaved(&self) -> bool {
        !matches!(self, TabKind::File { .. })
    }

    /// Whether the tab shows while the window is on `connection`.
    pub fn shows_on(&self, connection: &str) -> bool {
        match self {
            TabKind::Console { connection: c, .. } => c == connection,
            _ => true,
        }
    }

    /// The name the tab's text is saved under: `console_1`'s file is
    /// `console_1.sql`, a file's is its own.
    pub fn file_name(&self) -> String {
        match self {
            TabKind::Console { name, .. } | TabKind::Scratch { name } => format!("{name}.sql"),
            TabKind::File { path } => file_name(path),
        }
    }

    /// The tab's title before it is told apart from tabs with the same one:
    /// `console`, `scratch_1.sql`, `report.sql`.
    fn base_title(&self) -> String {
        match self {
            TabKind::Console { name, .. } => name.clone(),
            _ => self.file_name(),
        }
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// The key consoles are filed under for a connection: what the connection
/// dialog reads back as it, without a password. A saved connection and the
/// same URL typed in again share their consoles.
pub fn connection_key(target: &crate::connect::Target) -> String {
    target.clone().take_password().0.to_input()
}

pub struct Tab<B> {
    id: TabId,
    pub kind: TabKind,
    pub buffer: B,
}

impl<B> Tab<B> {
    pub fn id(&self) -> TabId {
        self.id
    }
}

/// The open tabs, in strip order, and which is active.
pub struct Tabs<B> {
    tabs: Vec<Tab<B>>,
    active: Option<TabId>,
    connection: String,
    /// The tab each connection had active when the window left it.
    last_active: HashMap<String, TabId>,
    next_id: u64,
}

impl<B> Tabs<B> {
    pub fn new(connection: impl Into<String>) -> Self {
        Self {
            tabs: Vec::new(),
            active: None,
            connection: connection.into(),
            last_active: HashMap::new(),
            next_id: 0,
        }
    }

    /// The [`connection_key`] whose consoles show.
    pub fn connection(&self) -> &str {
        &self.connection
    }

    /// An id for a tab about to be [`Tabs::insert`]ed, so its buffer can
    /// know it.
    pub fn alloc_id(&mut self) -> TabId {
        self.next_id += 1;
        TabId(self.next_id)
    }

    /// Add a tab at the end of the strip. It is not activated.
    pub fn insert(&mut self, id: TabId, kind: TabKind, buffer: B) {
        debug_assert!(self.get(id).is_none());
        self.tabs.push(Tab { id, kind, buffer });
        if self.active.is_none() && self.is_shown(id) {
            self.active = Some(id);
        }
    }

    pub fn get(&self, id: TabId) -> Option<&Tab<B>> {
        self.tabs.iter().find(|t| t.id == id)
    }

    pub fn get_mut(&mut self, id: TabId) -> Option<&mut Tab<B>> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    /// Every tab, shown or not, in strip order.
    pub fn iter(&self) -> impl Iterator<Item = &Tab<B>> {
        self.tabs.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Tab<B>> {
        self.tabs.iter_mut()
    }

    pub fn is_shown(&self, id: TabId) -> bool {
        self.get(id)
            .is_some_and(|t| t.kind.shows_on(&self.connection))
    }

    /// The tabs in the strip, in order.
    pub fn shown(&self) -> impl Iterator<Item = &Tab<B>> {
        self.tabs
            .iter()
            .filter(|t| t.kind.shows_on(&self.connection))
    }

    pub fn shown_ids(&self) -> Vec<TabId> {
        self.shown().map(|t| t.id).collect()
    }

    pub fn active_id(&self) -> Option<TabId> {
        self.active
    }

    pub fn active(&self) -> Option<&Tab<B>> {
        self.active.and_then(|id| self.get(id))
    }

    pub fn active_mut(&mut self) -> Option<&mut Tab<B>> {
        let id = self.active?;
        self.get_mut(id)
    }

    /// Where the active tab is in the strip.
    pub fn active_index(&self) -> Option<usize> {
        let active = self.active?;
        self.shown().position(|t| t.id == active)
    }

    /// Make `id` active, if it is in the strip.
    pub fn activate(&mut self, id: TabId) -> bool {
        if !self.is_shown(id) {
            return false;
        }
        self.active = Some(id);
        true
    }

    /// Activate the strip's `ix`th tab, or its last for `None` (⌘9).
    pub fn activate_index(&mut self, ix: Option<usize>) -> Option<TabId> {
        let shown = self.shown_ids();
        let id = match ix {
            Some(ix) => *shown.get(ix)?,
            None => *shown.last()?,
        };
        self.active = Some(id);
        Some(id)
    }

    /// Activate the tab `delta` places along the strip, wrapping around.
    pub fn cycle(&mut self, delta: isize) -> Option<TabId> {
        let shown = self.shown_ids();
        if shown.is_empty() {
            return None;
        }
        let at = self.active_index().unwrap_or(0) as isize;
        let ix = (at + delta).rem_euclid(shown.len() as isize) as usize;
        self.active = Some(shown[ix]);
        self.active
    }

    /// Take a tab out. A closed active tab hands over to the next tab in the
    /// strip, or the one before it when it was the last.
    pub fn remove(&mut self, id: TabId) -> Option<Tab<B>> {
        let shown = self.shown_ids();
        let ix = self.tabs.iter().position(|t| t.id == id)?;
        if self.active == Some(id) {
            let at = shown.iter().position(|&s| s == id);
            self.active = at.and_then(|at| {
                shown
                    .get(at + 1)
                    .or_else(|| at.checked_sub(1).and_then(|before| shown.get(before)))
                    .copied()
            });
        }
        self.last_active.retain(|_, tab| *tab != id);
        Some(self.tabs.remove(ix))
    }

    /// Show `connection`'s consoles instead of the current connection's.
    /// Its tab active when the window last left it comes back, else its
    /// first console, else whatever was active if it still shows.
    pub fn set_connection(&mut self, connection: impl Into<String>) {
        let connection = connection.into();
        if connection == self.connection {
            return;
        }
        if let Some(active) = self.active {
            self.last_active.insert(self.connection.clone(), active);
        }
        let previous = self.active;
        self.connection = connection;
        let remembered = self.last_active.get(&self.connection).copied();
        let first_console = self
            .shown()
            .find(|t| matches!(t.kind, TabKind::Console { .. }))
            .map(|t| t.id);
        self.active = [remembered, first_console, previous]
            .into_iter()
            .flatten()
            .find(|&id| self.is_shown(id))
            .or_else(|| self.shown().next().map(|t| t.id));
    }

    pub fn has_console(&self, connection: &str) -> bool {
        self.tabs
            .iter()
            .any(|t| matches!(&t.kind, TabKind::Console { connection: c, .. } if c == connection))
    }

    /// The tab editing the file at `path`.
    pub fn find_file(&self, path: &Path) -> Option<TabId> {
        self.tabs
            .iter()
            .find(|t| matches!(&t.kind, TabKind::File { path: p } if p == path))
            .map(|t| t.id)
    }

    /// A name for a new console of `connection`: `console`, then
    /// `console_1`, `console_2`… (as DataGrip names them), skipping names its
    /// open consoles have.
    pub fn console_name(&self, connection: &str) -> String {
        free_name("console", |name| {
            self.tabs.iter().any(|t| {
                matches!(&t.kind, TabKind::Console { connection: c, name: n }
                    if c == connection && n == name)
            })
        })
    }

    /// A name for a new scratch: `scratch`, `scratch_1`…
    pub fn scratch_name(&self) -> String {
        free_name("scratch", |name| {
            self.tabs
                .iter()
                .any(|t| matches!(&t.kind, TabKind::Scratch { name: n } if n == name))
        })
    }

    /// Whether a console or scratch named like `kind` is open, so one coming
    /// back from the closed list needs a new name.
    pub fn name_taken(&self, kind: &TabKind) -> bool {
        match kind {
            TabKind::File { .. } => false,
            _ => self.tabs.iter().any(|t| t.kind == *kind),
        }
    }

    /// Each shown tab's title. Files that share a name are told apart by
    /// their folder: `query.sql — reports`, `query.sql — billing`.
    pub fn titles(&self) -> Vec<(TabId, String)> {
        let shown: Vec<&Tab<B>> = self.shown().collect();
        shown
            .iter()
            .map(|tab| {
                let title = tab.kind.base_title();
                let TabKind::File { path } = &tab.kind else {
                    return (tab.id, title);
                };
                let clash = shown.iter().any(|other| {
                    other.id != tab.id
                        && matches!(other.kind, TabKind::File { .. })
                        && other.kind.base_title() == title
                });
                let folder = path
                    .parent()
                    .and_then(Path::file_name)
                    .map(|f| f.to_string_lossy().into_owned());
                match folder {
                    Some(folder) if clash => (tab.id, format!("{title} — {folder}")),
                    _ => (tab.id, title),
                }
            })
            .collect()
    }
}

/// `base`, or `base_1`, `base_2`… whichever is first not `taken`.
fn free_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_owned();
    }
    (1..)
        .map(|n| format!("{base}_{n}"))
        .find(|name| !taken(name))
        .expect("some name is free")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn console(connection: &str, name: &str) -> TabKind {
        TabKind::Console {
            connection: connection.into(),
            name: name.into(),
        }
    }

    fn file(path: &str) -> TabKind {
        TabKind::File { path: path.into() }
    }

    fn open(tabs: &mut Tabs<()>, kind: TabKind) -> TabId {
        let id = tabs.alloc_id();
        tabs.insert(id, kind, ());
        id
    }

    fn titles(tabs: &Tabs<()>) -> Vec<String> {
        tabs.titles().into_iter().map(|(_, t)| t).collect()
    }

    #[test]
    fn names_consoles_and_scratches_like_datagrip() {
        let mut tabs = Tabs::new(":memory:");
        assert_eq!(tabs.console_name(":memory:"), "console");
        open(&mut tabs, console(":memory:", "console"));
        assert_eq!(tabs.console_name(":memory:"), "console_1");
        // Another connection's consoles start over.
        assert_eq!(tabs.console_name("postgres://a@h/db"), "console");
        let first = open(
            &mut tabs,
            TabKind::Scratch {
                name: "scratch".into(),
            },
        );
        open(
            &mut tabs,
            TabKind::Scratch {
                name: "scratch_1".into(),
            },
        );
        tabs.remove(first);
        // A closed name is reused.
        assert_eq!(tabs.scratch_name(), "scratch");
        assert_eq!(
            titles(&tabs),
            ["console", "scratch_1.sql"],
            "consoles have no extension, scratches do"
        );
    }

    #[test]
    fn shows_only_the_connections_consoles() {
        let mut tabs = Tabs::new("a");
        let a = open(&mut tabs, console("a", "console"));
        let scratch = open(
            &mut tabs,
            TabKind::Scratch {
                name: "scratch".into(),
            },
        );
        let b = open(&mut tabs, console("b", "console"));
        assert_eq!(tabs.shown_ids(), [a, scratch]);
        assert!(!tabs.activate(b), "b's console is not in the strip");

        tabs.activate(scratch);
        tabs.set_connection("b");
        assert_eq!(tabs.shown_ids(), [scratch, b]);
        assert_eq!(
            tabs.active_id(),
            Some(b),
            "the connection's console comes up"
        );

        // Coming back to a restores the tab it had active.
        tabs.set_connection("a");
        assert_eq!(tabs.active_id(), Some(scratch));
        tabs.set_connection("b");
        assert_eq!(tabs.active_id(), Some(b));

        // A connection without consoles keeps what still shows.
        tabs.set_connection("c");
        assert_eq!(tabs.shown_ids(), [scratch]);
        assert_eq!(tabs.active_id(), Some(scratch));
        assert!(tabs.has_console("a") && !tabs.has_console("c"));
    }

    #[test]
    fn closing_hands_over_to_the_next_tab_then_the_one_before() {
        let mut tabs = Tabs::new("a");
        let one = open(&mut tabs, file("/x/1.sql"));
        let two = open(&mut tabs, file("/x/2.sql"));
        let three = open(&mut tabs, file("/x/3.sql"));
        assert_eq!(
            tabs.active_id(),
            Some(one),
            "the first tab opened is active"
        );
        tabs.activate(two);
        tabs.remove(two);
        assert_eq!(tabs.active_id(), Some(three));
        tabs.remove(three);
        assert_eq!(tabs.active_id(), Some(one));
        // Closing a tab that is not active leaves the active one alone.
        let four = open(&mut tabs, file("/x/4.sql"));
        tabs.remove(four);
        assert_eq!(tabs.active_id(), Some(one));
        tabs.remove(one);
        assert_eq!(tabs.active_id(), None);
        assert!(tabs.remove(one).is_none());
    }

    #[test]
    fn closing_skips_tabs_of_other_connections() {
        let mut tabs = Tabs::new("a");
        let a = open(&mut tabs, console("a", "console"));
        open(&mut tabs, console("b", "console"));
        let scratch = open(
            &mut tabs,
            TabKind::Scratch {
                name: "scratch".into(),
            },
        );
        tabs.activate(a);
        tabs.remove(a);
        assert_eq!(tabs.active_id(), Some(scratch));
    }

    #[test]
    fn cycles_and_jumps_through_the_strip() {
        let mut tabs = Tabs::new("a");
        let one = open(&mut tabs, file("/1.sql"));
        open(&mut tabs, console("b", "console"));
        let two = open(&mut tabs, file("/2.sql"));
        let three = open(&mut tabs, file("/3.sql"));
        assert_eq!(tabs.cycle(1), Some(two), "b's console is skipped");
        assert_eq!(tabs.cycle(1), Some(three));
        assert_eq!(tabs.cycle(1), Some(one), "wraps around");
        assert_eq!(tabs.cycle(-1), Some(three));
        assert_eq!(tabs.activate_index(Some(1)), Some(two));
        assert_eq!(tabs.active_index(), Some(1));
        assert_eq!(tabs.activate_index(Some(7)), None);
        assert_eq!(tabs.active_id(), Some(two));
        assert_eq!(tabs.activate_index(None), Some(three), "⌘9 is the last tab");
    }

    #[test]
    fn titles_tell_same_named_files_apart() {
        let mut tabs = Tabs::new("a");
        open(&mut tabs, file("/work/reports/query.sql"));
        open(&mut tabs, file("/work/billing/query.sql"));
        open(&mut tabs, file("/work/billing/other.sql"));
        open(&mut tabs, console("a", "console"));
        assert_eq!(
            titles(&tabs),
            [
                "query.sql — reports",
                "query.sql — billing",
                "other.sql",
                "console"
            ]
        );
        assert!(
            tabs.find_file(Path::new("/work/billing/query.sql"))
                .is_some()
        );
        assert!(tabs.find_file(Path::new("/work/query.sql")).is_none());
    }

    #[test]
    fn only_consoles_and_scratches_save_themselves() {
        assert!(console("a", "console").is_autosaved());
        assert!(
            TabKind::Scratch {
                name: "scratch".into()
            }
            .is_autosaved()
        );
        assert!(!file("/q.sql").is_autosaved());
        assert_eq!(console("a", "console_2").file_name(), "console_2.sql");
    }

    #[test]
    fn keys_consoles_by_the_connection_without_its_password() {
        let target = crate::connect::Target::parse("postgres://alice:secret@db/app").unwrap();
        assert_eq!(connection_key(&target), "postgres://alice@db/app");
        let memory = crate::connect::Target::parse(":memory:").unwrap();
        assert_eq!(connection_key(&memory), ":memory:");
    }
}
