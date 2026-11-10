//! Find in results: matches of the find bar's text in a result grid.
//!
//! The search walks the grid's view off the UI thread, a slice of rows at a
//! time, and hands over the matches of each slice as it goes, so the count
//! climbs and the first match is selected long before a million rows are
//! done. Changing the text, an option or the view stops the search under
//! way and starts again.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui_kit::{Context, Task};
use quarry_core::result::CellText;
use quarry_core::view::find::{self, FindQuery, Match, Needle};

use super::grid::{GridState, ROW_NUMBER_COLUMNS, focused_cell};

/// Matches kept for highlighting and stepping through; past it they are
/// only counted.
const MAX_MATCHES: usize = 1_000_000;

/// Rows searched between two hand-overs.
const SLICE_ROWS: usize = 20_000;

#[derive(Default)]
pub struct Search {
    pub query: FindQuery,
    /// In grid order: (grid row, data column).
    pub matches: Vec<Match>,
    /// All matches, past the ones kept.
    pub total: usize,
    /// The match selected, an index into `matches`.
    pub current: Option<usize>,
    /// How far the search has got while it runs.
    pub progress: Option<f32>,
    generation: u64,
    /// Select the first match after the selected cell once one is found.
    jump: Option<Option<Match>>,
    job: Option<Job>,
}

struct Job {
    stop: Arc<AtomicBool>,
    _task: Task<()>,
}

impl Drop for Job {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Search {
    /// Drop the matches, keeping the query, for new rows.
    pub fn reset(&mut self) {
        self.matches.clear();
        self.total = 0;
        self.current = None;
        self.progress = None;
        self.job = None;
        self.generation += 1;
    }

    /// Whether the cell is a match: `Some(true)` for the one selected.
    pub fn at(&self, row: usize, col: usize) -> Option<bool> {
        if self.matches.is_empty() {
            return None;
        }
        let at = (row as u32, col as u32);
        let ix = self.matches.binary_search(&at).ok()?;
        Some(self.current == Some(ix))
    }

    pub fn is_active(&self) -> bool {
        !self.query.text.is_empty()
    }
}

/// Search for `query`, then select its first match after the selected cell.
pub fn set_query(table: &mut GridState, query: FindQuery, cx: &mut Context<GridState>) {
    if table.delegate().search.query == query && table.delegate().search.job.is_some() {
        return;
    }
    let from = focused_cell(table).map(|(row, col)| (row as u32, col as u32));
    let search = &mut table.delegate_mut().search;
    search.query = query;
    search.jump = Some(from);
    run(table, cx);
}

/// The view changed: search it again, selecting nothing.
pub fn restart(table: &mut GridState, cx: &mut Context<GridState>) {
    if table.delegate().search.is_active() {
        table.delegate_mut().search.jump = None;
        run(table, cx);
    } else {
        table.delegate_mut().search.reset();
    }
}

/// Stop searching and forget the matches (the find bar closed).
pub fn clear(table: &mut GridState, cx: &mut Context<GridState>) {
    let search = &mut table.delegate_mut().search;
    search.reset();
    search.query.text.clear();
    table.delegate().changed(cx);
}

/// Select the next match (`forward`) or the previous one, from the selected
/// cell, wrapping around.
pub fn step(table: &mut GridState, forward: bool, cx: &mut Context<GridState>) {
    let from = focused_cell(table).map(|(row, col)| (row as u32, col as u32));
    let search = &table.delegate().search;
    // From the match selected, if the selection is still on it.
    let from = match search.current.and_then(|ix| search.matches.get(ix)) {
        Some(&current) if Some(current) == from => Some(current),
        _ => from,
    };
    if let Some(ix) = find::step(&search.matches, from, forward) {
        select(table, ix, cx);
    }
}

fn select(table: &mut GridState, ix: usize, cx: &mut Context<GridState>) {
    let Some(&(row, col)) = table.delegate().search.matches.get(ix) else {
        return;
    };
    table.delegate_mut().search.current = Some(ix);
    table.set_selected_cell(row as usize, col as usize + ROW_NUMBER_COLUMNS, cx);
    table.delegate().changed(cx);
}

enum Message {
    Slice {
        matches: Vec<Match>,
        found: usize,
        progress: f32,
    },
    Done,
}

fn run(table: &mut GridState, cx: &mut Context<GridState>) {
    let delegate = table.delegate_mut();
    let jump = delegate.search.jump.take();
    delegate.search.reset();
    delegate.search.jump = jump;
    let generation = delegate.search.generation;
    let (Some(result), Some(needle)) =
        (delegate.result.clone(), Needle::new(&delegate.search.query))
    else {
        delegate.changed(cx);
        return;
    };
    let view = delegate.arrange.view.clone();
    let rows = delegate.view_len();
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = flume::unbounded();
    let work = {
        let stop = stop.clone();
        cx.background_executor().spawn(async move {
            let mut cells = CellText::new(&result);
            let mut kept = 0;
            let mut start = 0;
            while start < rows && !stop.load(Ordering::Relaxed) {
                let end = (start + SLICE_ROWS).min(rows);
                let mut matches = Vec::new();
                let found = find::find_cells(
                    &mut cells,
                    view.as_deref(),
                    start..end,
                    &needle,
                    &mut matches,
                    MAX_MATCHES.saturating_sub(kept),
                );
                kept += matches.len();
                let progress = end as f32 / rows as f32;
                if tx
                    .send(Message::Slice {
                        matches,
                        found,
                        progress,
                    })
                    .is_err()
                {
                    return;
                }
                start = end;
            }
            let _ = tx.send(Message::Done);
        })
    };
    let task = cx.spawn(async move |table, cx| {
        let _work = work;
        while let Ok(message) = rx.recv_async().await {
            let done = matches!(message, Message::Done);
            let current = table.update(cx, |table, cx| {
                if table.delegate().search.generation != generation {
                    return false;
                }
                receive(table, message, cx);
                true
            });
            if done || !matches!(current, Ok(true)) {
                break;
            }
        }
    });
    let search = &mut table.delegate_mut().search;
    search.progress = Some(0.0);
    search.job = Some(Job { stop, _task: task });
    table.delegate().changed(cx);
}

fn receive(table: &mut GridState, message: Message, cx: &mut Context<GridState>) {
    let search = &mut table.delegate_mut().search;
    let target = match message {
        Message::Slice {
            matches,
            found,
            progress,
        } => {
            search.matches.extend(matches);
            search.total += found;
            search.progress = Some(progress);
            // The first match after where the selection was.
            match search.jump {
                Some(from) => {
                    let at = from.map_or(0, |from| search.matches.partition_point(|m| *m < from));
                    (at < search.matches.len()).then_some(at)
                }
                None => None,
            }
        }
        Message::Done => {
            search.progress = None;
            search.job = None;
            // Nothing after it: wrap to the first.
            search
                .jump
                .is_some()
                .then_some(0)
                .filter(|_| !search.matches.is_empty())
        }
    };
    if let Some(ix) = target {
        table.delegate_mut().search.jump = None;
        select(table, ix, cx);
    } else {
        table.delegate().changed(cx);
    }
}
