//! The query history: every statement Quarry runs, with when it ran, on
//! which connection, how long it took, how many rows and how it ended. It is
//! kept in `history.json` in Quarry's settings folder ([`store`] has the
//! format and the caps) and browsed with Query History (⌥⌘E), which puts a
//! statement back into the editor or runs it again.
//!
//! The history is one per app, not per window, so it outlives a closed
//! window: it is read in the background at launch, written a moment after
//! each statement, and written once more when Quarry quits.

mod store;
mod view;

use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::{App, AppContext as _, Context, Entity, Global, Subscription, Task};

pub use store::{Entry, Outcome};
pub use view::{Handlers, open as open_dialog};

/// How long history stays unwritten after a statement, so a script of many
/// statements writes the file once rather than once each.
const SAVE_DELAY: Duration = Duration::from_secs(1);

pub struct History {
    log: store::Log,
    path: Option<PathBuf>,
    /// The file has been read, so writing it keeps what it had.
    loaded: bool,
    /// Bumped on every change; `saved` is the revision last written.
    revision: u64,
    saved: u64,
    save_task: Option<Task<()>>,
    _quit: Subscription,
}

struct GlobalHistory(Entity<History>);

impl Global for GlobalHistory {}

pub fn init(cx: &mut App) {
    let path = crate::config_dir().map(|dir| dir.join("history.json"));
    let history = cx.new(|cx| {
        if let Some(path) = path.clone() {
            let read = cx
                .background_executor()
                .spawn(async move { store::load(&path) });
            cx.spawn(async move |this, cx| {
                let entries = read.await;
                let _ = this.update(cx, |this: &mut History, cx| this.loaded(entries, cx));
            })
            .detach();
        }
        History {
            log: store::Log::default(),
            loaded: path.is_none(),
            path,
            revision: 0,
            saved: 0,
            save_task: None,
            _quit: cx.on_app_quit(|this, _| {
                this.flush();
                async {}
            }),
        }
    });
    cx.set_global(GlobalHistory(history));
}

impl History {
    pub fn global(cx: &App) -> Entity<History> {
        cx.global::<GlobalHistory>().0.clone()
    }

    /// Newest first.
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.log.newest_first()
    }

    pub fn len(&self) -> usize {
        self.log.len()
    }

    pub fn record(&mut self, entry: Entry, cx: &mut Context<Self>) {
        self.log.push(entry);
        self.changed(cx);
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.log.clear();
        self.changed(cx);
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        self.revision += 1;
        if self.loaded {
            self.schedule_save(cx);
        }
        cx.notify();
    }

    fn loaded(&mut self, entries: anyhow::Result<Vec<Entry>>, cx: &mut Context<Self>) {
        match entries {
            Ok(entries) => {
                // Over the caps (a later Quarry's, or edited by hand): write
                // it back trimmed.
                if self.log.prepend(entries) {
                    self.revision += 1;
                }
                self.loaded = true;
                if self.revision != self.saved {
                    self.schedule_save(cx);
                }
            }
            // Not writable over either, or what it had would be lost.
            Err(err) => log::warn!("{err:#}; the query history is not saved this time"),
        }
        cx.notify();
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.path.clone() else {
            return;
        };
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            let Ok((revision, entries)) =
                this.update(cx, |this, _| (this.revision, this.log.to_vec()))
            else {
                return;
            };
            let saved = cx
                .background_executor()
                .spawn(async move { store::save(&path, entries) })
                .await;
            match saved {
                Ok(()) => {
                    let _ = this.update(cx, |this, _| this.saved = this.saved.max(revision));
                }
                Err(err) => log::warn!("{err:#}"),
            }
        }));
    }

    /// Write now what is not written yet: Quarry is quitting.
    fn flush(&mut self) {
        self.save_task = None;
        let Some(path) = &self.path else {
            return;
        };
        if self.revision == self.saved {
            return;
        }
        if !self.loaded {
            // Quitting before the file was read: read it first so writing
            // keeps what it had.
            match store::load(path) {
                Ok(entries) => {
                    self.log.prepend(entries);
                }
                Err(err) => {
                    log::warn!("{err:#}; the query history is not saved this time");
                    return;
                }
            }
        }
        match store::save(path, self.log.to_vec()) {
            Ok(()) => self.saved = self.revision,
            Err(err) => log::warn!("{err:#}"),
        }
    }
}
