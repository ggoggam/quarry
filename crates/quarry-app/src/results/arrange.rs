//! Sorting and filtering a result grid: the view of rows the grid shows.
//!
//! The view is a list of result row indices ([`quarry_core::view`]),
//! computed off the UI thread from a snapshot of the result (its batches are
//! shared, not copied). A new sort or filter supersedes a computation still
//! running: the old one is told to stop and its answer is dropped. The sorted
//! order and the filter's mask are kept, so changing only the filter does not
//! sort again, and the other way round.
//!
//! While a result is still streaming, a view covers the rows that had
//! arrived when it was asked for; rows arriving after it wait, and the view
//! is worked out again over all of them when the query ends.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use gpui_kit::{Context, SharedString, Task};
use quarry_core::view::filter::{self, Filter};
use quarry_core::view::sort::{self, SortKey};
use quarry_core::view::{self as rows};

use super::grid::{GridState, refresh_range};
use super::search;

/// A grid's sort, filter and the view they make.
#[derive(Default)]
pub struct Arrange {
    pub sort: Vec<SortKey>,
    /// The filter applied, and its text as typed.
    pub filter: Option<Arc<Filter>>,
    pub filter_text: String,
    /// Result rows in the order shown; `None` is all of them as they came.
    pub view: Option<Arc<[u32]>>,
    /// How many result rows the view was worked out over.
    pub basis: usize,
    /// The computation in progress.
    pub job: Option<Job>,
    /// Why the last sort or filter could not be applied.
    pub error: Option<SharedString>,
    /// How long the last one took.
    pub elapsed: Option<Duration>,
    generation: u64,
    sorted: Option<(Vec<SortKey>, usize, Arc<[u32]>)>,
    filtered: Option<(Arc<Filter>, usize, Arc<[bool]>)>,
}

/// A sort or filter being worked out.
pub struct Job {
    pub what: &'static str,
    /// From 0 to 1, where it can tell.
    pub progress: Option<f32>,
    stop: Arc<AtomicBool>,
    _task: Task<()>,
}

impl Drop for Job {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Arrange {
    /// Forget the view, for a new result. `keep` holds on to the sort and
    /// filter (a rerun with the same columns), to apply once it has rows.
    pub fn reset(&mut self, keep: bool) {
        let (sort, filter, text) = (
            std::mem::take(&mut self.sort),
            self.filter.take(),
            std::mem::take(&mut self.filter_text),
        );
        *self = Arrange::default();
        if keep {
            self.sort = sort;
            self.filter = filter;
            self.filter_text = text;
        }
    }

    /// Whether rows are sorted or filtered.
    pub fn is_active(&self) -> bool {
        !self.sort.is_empty() || self.filter.is_some()
    }
}

enum Message {
    Progress(&'static str, Option<f32>),
    Done(Result<Outcome, String>),
}

struct Outcome {
    rows: usize,
    sorted: Option<(Vec<SortKey>, Arc<[u32]>)>,
    filtered: Option<(Arc<Filter>, Arc<[bool]>)>,
    view: Option<Arc<[u32]>>,
}

/// Sort by `keys` (none: the result's order).
pub fn set_sort(table: &mut GridState, keys: Vec<SortKey>, cx: &mut Context<GridState>) {
    let arrange = &mut table.delegate_mut().arrange;
    if arrange.sort == keys {
        return;
    }
    arrange.sort = keys;
    rearrange(table, cx);
}

/// A click on a column's sort control.
pub fn toggle_sort(table: &mut GridState, col: usize, add: bool, cx: &mut Context<GridState>) {
    let keys = sort::toggle(&table.delegate().arrange.sort, col, add);
    set_sort(table, keys, cx);
}

/// Filter by `text`, in the filter language; blank clears the filter. A
/// filter that does not parse is not applied, and says why.
pub fn set_filter(
    table: &mut GridState,
    text: &str,
    cx: &mut Context<GridState>,
) -> Result<(), filter::FilterError> {
    let Some(schema) = table.delegate().result().map(|r| r.schema().clone()) else {
        return Ok(());
    };
    let parsed = Filter::parse(text, &schema);
    let delegate = table.delegate_mut();
    match parsed {
        Ok(filter) => {
            delegate.arrange.filter_text = text.trim().to_owned();
            delegate.arrange.filter = filter.map(Arc::new);
            delegate.arrange.error = None;
            rearrange(table, cx);
            Ok(())
        }
        Err(err) => {
            delegate.arrange.error = Some(format!("{err} (at {})", err.at + 1).into());
            delegate.changed(cx);
            Err(err)
        }
    }
}

/// The query ended: a view over part of its rows is worked out again over
/// all of them, and so is a search.
pub fn finished_streaming(table: &mut GridState, cx: &mut Context<GridState>) {
    let delegate = table.delegate();
    if delegate.arrange.is_active() && delegate.arrange.basis < delegate.row_count() {
        // Searches again too, once the view is in place.
        rearrange(table, cx);
    } else if delegate.search.is_active() {
        // A search started while rows arrived covers them all now.
        search::restart(table, cx);
    }
}

/// Work the view out again, off the UI thread.
pub fn rearrange(table: &mut GridState, cx: &mut Context<GridState>) {
    let delegate = table.delegate_mut();
    let arrange = &mut delegate.arrange;
    arrange.generation += 1;
    arrange.job = None;
    arrange.error = None;
    let Some(result) = delegate.result.clone() else {
        return;
    };
    if !arrange.is_active() {
        arrange.view = None;
        arrange.basis = result.row_count();
        arrange.elapsed = None;
        applied(table, cx);
        return;
    }
    let generation = arrange.generation;
    let rows_now = result.row_count();
    let keys = arrange.sort.clone();
    let filter = arrange.filter.clone();
    // What is still good from the last time.
    let sorted = arrange
        .sorted
        .clone()
        .filter(|(k, n, _)| *k == keys && *n == rows_now)
        .map(|(_, _, order)| order);
    let filtered = arrange
        .filtered
        .clone()
        .zip(filter.clone())
        .filter(|((f, n, _), current)| **f == **current && *n == rows_now)
        .map(|((_, _, mask), _)| mask);
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = flume::unbounded();
    let work = {
        let stop = stop.clone();
        cx.background_executor().spawn(async move {
            let outcome = compute(&result, keys, filter, sorted, filtered, &stop, &tx);
            let _ = tx.send(Message::Done(outcome));
        })
    };
    let what = if arrange.sort.is_empty() {
        "Filtering"
    } else {
        "Sorting"
    };
    let task = cx.spawn(async move |table, cx| {
        let _work = work;
        let started = Instant::now();
        while let Ok(message) = rx.recv_async().await {
            let done = matches!(message, Message::Done(_));
            let applied = table.update(cx, |table, cx| {
                if table.delegate().arrange.generation != generation {
                    return false;
                }
                match message {
                    Message::Progress(what, progress) => {
                        if let Some(job) = &mut table.delegate_mut().arrange.job {
                            job.what = what;
                            job.progress = progress;
                        }
                        table.delegate().changed(cx);
                    }
                    Message::Done(outcome) => finish(table, outcome, started.elapsed(), cx),
                }
                true
            });
            if done || !matches!(applied, Ok(true)) {
                break;
            }
        }
    });
    let arrange = &mut table.delegate_mut().arrange;
    arrange.job = Some(Job {
        what,
        progress: None,
        stop,
        _task: task,
    });
    table.delegate().changed(cx);
}

fn compute(
    result: &quarry_core::ResultSet,
    keys: Vec<SortKey>,
    filter: Option<Arc<Filter>>,
    sorted: Option<Arc<[u32]>>,
    filtered: Option<Arc<[bool]>>,
    stop: &AtomicBool,
    tx: &flume::Sender<Message>,
) -> Result<Outcome, String> {
    let rows = result.row_count();
    let order = match (keys.is_empty(), sorted) {
        (true, _) => None,
        (false, Some(order)) => Some(order),
        (false, None) => {
            let _ = tx.send(Message::Progress("Sorting", None));
            Some(Arc::from(sort::sort_indices(result, &keys)?))
        }
    };
    if stop.load(Ordering::Relaxed) {
        return Err("stopped".into());
    }
    let mask = match (&filter, filtered) {
        (None, _) => None,
        (Some(_), Some(mask)) => Some(mask),
        (Some(filter), None) => {
            let mut last = 0.0;
            let mut progress = |done: f32| {
                if done - last >= 0.02 {
                    last = done;
                    let _ = tx.send(Message::Progress("Filtering", Some(done)));
                }
                !stop.load(Ordering::Relaxed)
            };
            let keep = filter
                .evaluate(result, &mut progress)
                .map_err(|e| e.to_string())?;
            Some(Arc::from(keep))
        }
    };
    let view = rows::combine(rows, order.as_deref(), mask.as_deref()).map(Arc::from);
    Ok(Outcome {
        rows,
        sorted: order.map(|o| (keys, o)),
        filtered: filter.zip(mask),
        view,
    })
}

fn finish(
    table: &mut GridState,
    outcome: Result<Outcome, String>,
    elapsed: Duration,
    cx: &mut Context<GridState>,
) {
    let arrange = &mut table.delegate_mut().arrange;
    arrange.job = None;
    match outcome {
        Ok(outcome) => {
            arrange.view = outcome.view;
            arrange.basis = outcome.rows;
            arrange.elapsed = Some(elapsed);
            if let Some((keys, order)) = outcome.sorted {
                arrange.sorted = Some((keys, outcome.rows, order));
            }
            if let Some((filter, mask)) = outcome.filtered {
                arrange.filtered = Some((filter, outcome.rows, mask));
            }
            applied(table, cx);
        }
        Err(message) if message == "stopped" => {}
        Err(message) => {
            arrange.error = Some(message.into());
            table.delegate().changed(cx);
        }
    }
}

/// A new view is in place: keep the selection inside it and search it
/// again.
fn applied(table: &mut GridState, cx: &mut Context<GridState>) {
    let rows = table.delegate().view_len();
    let selected_row = super::grid::focused_cell(table).map(|(row, _)| row);
    let delegate = table.delegate_mut();
    delegate.range = Default::default();
    if selected_row.is_none_or(|row| row >= rows) {
        table.clear_selection(cx);
    }
    refresh_range(table);
    search::restart(table, cx);
    table.delegate().changed(cx);
}
