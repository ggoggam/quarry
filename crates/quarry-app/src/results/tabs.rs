//! Result tabs as plain data: which tabs each editor tab has, which one it
//! shows, which are pinned, what they are called, and its Output log. No
//! GPUI here, so the rules are unit-tested; [`super::Results`] keeps a
//! [`ResultTabs`] of its grids.
//!
//! The rules, as in DataGrip:
//!
//! - Results belong to the editor tab whose run produced them (its *owner*),
//!   and the pane shows the owner that is active in the editor. Runs from
//!   different editor tabs never touch each other's results.
//! - A run starts by closing its owner's unpinned tabs; each statement that
//!   returns rows then opens a tab at the end of the strip. Pinned tabs stay
//!   until the user closes them, one at a time (Close Others and Close
//!   Unpinned leave them be), or closes the editor tab they belong to.
//! - An owner keeps at most [`MAX_TABS`] tabs: a run that opens more closes
//!   its owner's oldest unpinned tab for each one past that.
//! - Every owner has an Output view, first in its strip, logging what each
//!   statement did; it keeps the last [`MAX_OUTPUT_LINES`] lines.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

use super::run::OutputLine;

/// Most result tabs one editor tab keeps.
pub const MAX_TABS: usize = 32;
/// Most lines one Output view keeps.
pub const MAX_OUTPUT_LINES: usize = 500;

/// Identifies a result tab for as long as Quarry runs. Not persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ResultTabId(pub(super) u64);

impl ResultTabId {
    /// For element ids.
    pub fn raw(self) -> u64 {
        self.0
    }
}

/// What an owner's pane shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    #[default]
    Output,
    Result(ResultTabId),
}

pub struct ResultTab<O, D> {
    id: ResultTabId,
    pub owner: O,
    /// The title its statement gave it.
    pub title: String,
    /// What the user renamed it to.
    pub name: Option<String>,
    pub pinned: bool,
    /// When it was last shown, on [`ResultTabs`]'s clock.
    last_shown: u64,
    pub data: D,
}

impl<O, D> ResultTab<O, D> {
    pub fn id(&self) -> ResultTabId {
        self.id
    }
}

struct Owner {
    view: View,
    output: VecDeque<OutputLine>,
}

impl Default for Owner {
    fn default() -> Self {
        Self {
            view: View::Output,
            output: VecDeque::new(),
        }
    }
}

pub struct ResultTabs<O, D> {
    /// Every owner's tabs, each owner's in strip order.
    tabs: Vec<ResultTab<O, D>>,
    owners: HashMap<O, Owner>,
    next_id: u64,
    clock: u64,
}

impl<O: Copy + Eq + Hash, D> Default for ResultTabs<O, D> {
    fn default() -> Self {
        Self::new()
    }
}

impl<O: Copy + Eq + Hash, D> ResultTabs<O, D> {
    pub fn new() -> Self {
        Self {
            tabs: Vec::new(),
            owners: HashMap::new(),
            next_id: 0,
            clock: 0,
        }
    }

    pub fn get(&self, id: ResultTabId) -> Option<&ResultTab<O, D>> {
        self.tabs.iter().find(|t| t.id == id)
    }

    pub fn get_mut(&mut self, id: ResultTabId) -> Option<&mut ResultTab<O, D>> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    /// Every tab of every owner.
    pub fn iter(&self) -> impl Iterator<Item = &ResultTab<O, D>> {
        self.tabs.iter()
    }

    /// `owner`'s tabs, in strip order.
    pub fn shown(&self, owner: O) -> impl Iterator<Item = &ResultTab<O, D>> {
        self.tabs.iter().filter(move |t| t.owner == owner)
    }

    fn shown_ids(&self, owner: O) -> Vec<ResultTabId> {
        self.shown(owner).map(|t| t.id).collect()
    }

    pub fn view(&self, owner: O) -> View {
        self.owners.get(&owner).map(|o| o.view).unwrap_or_default()
    }

    /// The tab `owner` shows, if it shows one rather than its Output.
    pub fn viewed(&self, owner: O) -> Option<&ResultTab<O, D>> {
        match self.view(owner) {
            View::Result(id) => self.get(id),
            View::Output => None,
        }
    }

    /// Show `view` for `owner`. `false` for a tab that is not `owner`'s.
    pub fn select(&mut self, owner: O, view: View) -> bool {
        if let View::Result(id) = view {
            let clock = self.tick();
            let Some(tab) = self.get_mut(id).filter(|t| t.owner == owner) else {
                return false;
            };
            tab.last_shown = clock;
        }
        self.owners.entry(owner).or_default().view = view;
        true
    }

    /// Note that `id` is on screen now, for [`ResultTabs::release_order`].
    pub fn touch(&mut self, id: ResultTabId) {
        let clock = self.tick();
        if let Some(tab) = self.get_mut(id) {
            tab.last_shown = clock;
        }
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// A run of `owner`'s starts: its unpinned tabs go, and are returned so
    /// their results can be let go of.
    pub fn begin_run(&mut self, owner: O) -> Vec<ResultTab<O, D>> {
        let unpinned: Vec<ResultTabId> = self
            .shown(owner)
            .filter(|t| !t.pinned)
            .map(|t| t.id)
            .collect();
        unpinned
            .into_iter()
            .filter_map(|id| self.remove(id))
            .collect()
    }

    /// Add a tab at the end of `owner`'s strip. It is not shown; the caller
    /// [`select`](Self::select)s it. Returns its id and any tab closed to
    /// keep within [`MAX_TABS`].
    pub fn open(
        &mut self,
        owner: O,
        title: impl Into<String>,
        data: D,
    ) -> (ResultTabId, Vec<ResultTab<O, D>>) {
        self.next_id += 1;
        let id = ResultTabId(self.next_id);
        let last_shown = self.tick();
        self.tabs.push(ResultTab {
            id,
            owner,
            title: title.into(),
            name: None,
            pinned: false,
            last_shown,
            data,
        });
        self.owners.entry(owner).or_default();
        let mut closed = Vec::new();
        while self.shown(owner).count() > MAX_TABS {
            let Some(oldest) = self
                .shown(owner)
                .find(|t| !t.pinned && t.id != id)
                .map(|t| t.id)
            else {
                break;
            };
            closed.extend(self.remove(oldest));
        }
        (id, closed)
    }

    /// Close one tab, pinned or not. The owner then shows the tab after it,
    /// or the one before it when it was the last, or its Output.
    pub fn close(&mut self, id: ResultTabId) -> Option<ResultTab<O, D>> {
        self.remove(id)
    }

    /// Close `id`'s owner's other unpinned tabs.
    pub fn close_others(&mut self, id: ResultTabId) -> Vec<ResultTab<O, D>> {
        let Some(owner) = self.get(id).map(|t| t.owner) else {
            return Vec::new();
        };
        let others: Vec<ResultTabId> = self
            .shown(owner)
            .filter(|t| !t.pinned && t.id != id)
            .map(|t| t.id)
            .collect();
        others
            .into_iter()
            .filter_map(|id| self.remove(id))
            .collect()
    }

    /// Close `owner`'s unpinned tabs.
    pub fn close_unpinned(&mut self, owner: O) -> Vec<ResultTab<O, D>> {
        self.begin_run(owner)
    }

    /// The editor tab `owner` closed: everything of it goes, pinned too.
    pub fn remove_owner(&mut self, owner: O) -> Vec<ResultTab<O, D>> {
        self.owners.remove(&owner);
        let (gone, kept) = std::mem::take(&mut self.tabs)
            .into_iter()
            .partition(|t| t.owner == owner);
        self.tabs = kept;
        gone
    }

    fn remove(&mut self, id: ResultTabId) -> Option<ResultTab<O, D>> {
        let ix = self.tabs.iter().position(|t| t.id == id)?;
        let owner = self.tabs[ix].owner;
        if self.view(owner) == View::Result(id) {
            let strip = self.shown_ids(owner);
            let at = strip.iter().position(|&t| t == id).unwrap_or(0);
            let next = strip
                .get(at + 1)
                .or_else(|| at.checked_sub(1).and_then(|before| strip.get(before)))
                .copied();
            self.owners.entry(owner).or_default().view = next.map_or(View::Output, View::Result);
        }
        Some(self.tabs.remove(ix))
    }

    pub fn set_pinned(&mut self, id: ResultTabId, pinned: bool) {
        if let Some(tab) = self.get_mut(id) {
            tab.pinned = pinned;
        }
    }

    /// Name a tab; a blank name gives it back its own title.
    pub fn rename(&mut self, id: ResultTabId, name: &str) {
        if let Some(tab) = self.get_mut(id) {
            let name = name.trim();
            tab.name = (!name.is_empty() && name != tab.title).then(|| name.to_owned());
        }
    }

    /// The title a tab shows before it is told apart from others.
    pub fn title(&self, id: ResultTabId) -> Option<&str> {
        let tab = self.get(id)?;
        Some(tab.name.as_deref().unwrap_or(&tab.title))
    }

    /// `owner`'s tabs and their titles. Tabs titled alike by their
    /// statements are numbered: `orders`, `orders (2)`; a name the user
    /// gave stays as given.
    pub fn titles(&self, owner: O) -> Vec<(ResultTabId, String)> {
        let mut seen: HashMap<&str, usize> = HashMap::new();
        self.shown(owner)
            .map(|tab| match &tab.name {
                Some(name) => (tab.id, name.clone()),
                None => {
                    let count = seen.entry(tab.title.as_str()).or_default();
                    *count += 1;
                    match *count {
                        1 => (tab.id, tab.title.clone()),
                        n => (tab.id, format!("{} ({n})", tab.title)),
                    }
                }
            })
            .collect()
    }

    /// Tabs whose results may be let go of to save memory, the first to go
    /// first: unpinned before pinned, then the longest since shown. `keep`
    /// (on screen, still running) never goes.
    pub fn release_order(&self, keep: &[ResultTabId]) -> Vec<ResultTabId> {
        let mut tabs: Vec<&ResultTab<O, D>> =
            self.tabs.iter().filter(|t| !keep.contains(&t.id)).collect();
        tabs.sort_by_key(|t| (t.pinned, t.last_shown));
        tabs.into_iter().map(|t| t.id).collect()
    }

    // ── Output ───────────────────────────────────────────────────────────────

    pub fn log(&mut self, owner: O, line: OutputLine) {
        let output = &mut self.owners.entry(owner).or_default().output;
        output.push_back(line);
        while output.len() > MAX_OUTPUT_LINES {
            output.pop_front();
        }
    }

    /// `owner`'s Output, oldest line first.
    pub fn output(&self, owner: O) -> impl Iterator<Item = &OutputLine> {
        self.owners
            .get(&owner)
            .into_iter()
            .flat_map(|o| o.output.iter())
    }

    pub fn output_len(&self, owner: O) -> usize {
        self.owners.get(&owner).map_or(0, |o| o.output.len())
    }

    pub fn clear_output(&mut self, owner: O) {
        if let Some(o) = self.owners.get_mut(&owner) {
            o.output.clear();
        }
    }

    /// Point the Output line `owner` logged last at the tab it opened.
    pub fn link_last_line(&mut self, owner: O, id: ResultTabId) {
        if let Some(line) = self
            .owners
            .get_mut(&owner)
            .and_then(|o| o.output.back_mut())
        {
            line.result = Some(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::run::Tone;
    use super::*;

    type Tabs = ResultTabs<u32, &'static str>;

    const A: u32 = 1;
    const B: u32 = 2;

    fn titles(tabs: &Tabs, owner: u32) -> Vec<String> {
        tabs.titles(owner).into_iter().map(|(_, t)| t).collect()
    }

    fn open(tabs: &mut Tabs, owner: u32, title: &str) -> ResultTabId {
        let (id, closed) = tabs.open(owner, title, "grid");
        assert!(closed.is_empty());
        tabs.select(owner, View::Result(id));
        id
    }

    #[test]
    fn a_run_replaces_its_unpinned_tabs_and_keeps_pinned_ones() {
        let mut tabs = Tabs::new();
        let first = open(&mut tabs, A, "orders");
        let second = open(&mut tabs, A, "customers");
        tabs.set_pinned(first, true);
        let other = open(&mut tabs, B, "orders");

        let gone = tabs.begin_run(A);
        assert_eq!(gone.iter().map(|t| t.id()).collect::<Vec<_>>(), [second]);
        assert_eq!(tabs.view(A), View::Result(first), "the pinned tab is shown");
        let third = open(&mut tabs, A, "orders");
        assert_eq!(titles(&tabs, A), ["orders", "orders (2)"]);
        assert_eq!(tabs.view(A), View::Result(third));

        // Another editor tab's results are left alone.
        assert_eq!(tabs.shown(B).map(|t| t.id()).collect::<Vec<_>>(), [other]);
        assert!(!tabs.select(B, View::Result(first)), "not B's tab");

        // With nothing pinned, the Output shows until a result arrives.
        tabs.begin_run(B);
        assert_eq!(tabs.view(B), View::Output);
        assert_eq!(tabs.shown(B).count(), 0);
    }

    #[test]
    fn closing_hands_over_to_the_next_tab_then_the_one_before_then_output() {
        let mut tabs = Tabs::new();
        let one = open(&mut tabs, A, "one");
        let two = open(&mut tabs, A, "two");
        let three = open(&mut tabs, A, "three");
        tabs.select(A, View::Result(two));
        tabs.close(two);
        assert_eq!(tabs.view(A), View::Result(three));
        tabs.close(three);
        assert_eq!(tabs.view(A), View::Result(one));
        tabs.close(one);
        assert_eq!(tabs.view(A), View::Output);
        assert!(tabs.close(one).is_none());
    }

    #[test]
    fn close_others_and_close_unpinned_keep_pinned_tabs() {
        let mut tabs = Tabs::new();
        let pinned = open(&mut tabs, A, "p");
        tabs.set_pinned(pinned, true);
        let keep = open(&mut tabs, A, "k");
        open(&mut tabs, A, "x");
        assert_eq!(tabs.close_others(keep).len(), 1);
        assert_eq!(titles(&tabs, A), ["p", "k"]);
        assert_eq!(tabs.close_unpinned(A).len(), 1);
        assert_eq!(titles(&tabs, A), ["p"]);
        // A pinned tab closes when asked to by itself.
        assert!(tabs.close(pinned).is_some());
    }

    #[test]
    fn an_editor_tab_closing_takes_its_results_and_output() {
        let mut tabs = Tabs::new();
        let pinned = open(&mut tabs, A, "p");
        tabs.set_pinned(pinned, true);
        open(&mut tabs, B, "b");
        tabs.log(A, line("SELECT 1"));
        assert_eq!(tabs.remove_owner(A).len(), 1);
        assert!(tabs.get(pinned).is_none());
        assert_eq!(tabs.output_len(A), 0);
        assert_eq!(tabs.shown(B).count(), 1);
    }

    #[test]
    fn keeps_at_most_max_tabs_per_editor_tab() {
        let mut tabs = Tabs::new();
        let pinned = open(&mut tabs, A, "pinned");
        tabs.set_pinned(pinned, true);
        let first = open(&mut tabs, A, "t");
        for _ in 2..MAX_TABS {
            open(&mut tabs, A, "t");
        }
        assert_eq!(tabs.shown(A).count(), MAX_TABS);
        let (_, closed) = tabs.open(A, "t", "grid");
        assert_eq!(
            closed.iter().map(|t| t.id()).collect::<Vec<_>>(),
            [first],
            "the oldest unpinned one goes"
        );
        assert_eq!(tabs.shown(A).count(), MAX_TABS);
        assert!(tabs.get(pinned).is_some());
    }

    #[test]
    fn renames_and_gives_back_the_title() {
        let mut tabs = Tabs::new();
        let id = open(&mut tabs, A, "orders");
        let other = open(&mut tabs, A, "orders");
        tabs.rename(id, "  big orders ");
        assert_eq!(titles(&tabs, A), ["big orders", "orders"]);
        assert_eq!(tabs.title(id), Some("big orders"));
        tabs.rename(other, "big orders");
        assert_eq!(titles(&tabs, A), ["big orders", "big orders"], "as given");
        tabs.rename(id, "");
        assert_eq!(tabs.get(id).unwrap().name, None);
        tabs.rename(id, "orders");
        assert_eq!(tabs.get(id).unwrap().name, None, "its own title is no name");
    }

    #[test]
    fn releases_unpinned_and_least_recently_shown_first() {
        let mut tabs = Tabs::new();
        let old = open(&mut tabs, A, "old");
        let pinned = open(&mut tabs, A, "pinned");
        tabs.set_pinned(pinned, true);
        let recent = open(&mut tabs, B, "recent");
        let shown = open(&mut tabs, A, "shown");
        tabs.touch(old);
        tabs.touch(recent);
        assert_eq!(tabs.release_order(&[shown]), [old, recent, pinned]);
    }

    fn line(statement: &str) -> OutputLine {
        OutputLine {
            at: 0,
            statement: statement.into(),
            message: "completed in 1 ms".into(),
            tone: Tone::Normal,
            detail: None,
            result: None,
        }
    }

    #[test]
    fn output_keeps_the_last_lines() {
        let mut tabs = Tabs::new();
        for i in 0..MAX_OUTPUT_LINES + 3 {
            tabs.log(A, line(&format!("SELECT {i}")));
        }
        assert_eq!(tabs.output_len(A), MAX_OUTPUT_LINES);
        assert_eq!(tabs.output(A).next().unwrap().statement, "SELECT 3");
        let id = open(&mut tabs, A, "t");
        tabs.link_last_line(A, id);
        assert_eq!(tabs.output(A).last().unwrap().result, Some(id));
        tabs.clear_output(A);
        assert_eq!(tabs.output(A).count(), 0);
        assert_eq!(tabs.output(B).count(), 0);
    }
}
