//! The results pane: result tabs and an Output view per editor tab.
//!
//! Each statement that returns rows gets its own result tab, so a script of
//! three SELECTs leaves three grids. The pane shows the tabs of the editor
//! tab that is active; [`tabs`] has the rules for which tabs a new run
//! replaces (its own unpinned ones) and which stay (pinned ones). The Output
//! view, first in every strip, logs what each statement did: rows retrieved
//! or affected, how long it took, and the engine's message when it failed.
//!
//! Results are Arrow batches in memory, up to the row limit each. Together
//! they are kept within [`MEMORY_BUDGET`]: past it, the results shown least
//! recently are let go of (unpinned before pinned, never the one on screen
//! or still arriving), and their tab offers to run the statement again.
//!
//! For the rest of the app, [`Results::active_result`] is the result on
//! screen: its [`ResultSet`], its table and the statement that produced it.

mod grid;
mod run;
mod tabs;
mod title;
mod view;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui_kit::component::table::TableState;
use gpui_kit::{
    App, AppContext as _, Context, Entity, EventEmitter, ScrollHandle, SharedString, Task, Window,
};
use quarry_core::arrow::datatypes::SchemaRef;
use quarry_core::{Dialect, QueryEvent, QueryOptions, QuerySummary, ResultSet, Session};

use crate::editor::TabId;
use crate::format::{format_count, format_elapsed, now_ms};
use crate::history::{self, Outcome};
pub use grid::ResultsDelegate;
pub use run::{OutputLine, Tone};
use run::{Run, Step};
use tabs::ResultTabs;
pub use tabs::{ResultTabId, View};
pub use title::preview;
pub use view::{actions_menu_items, bind_actions};

/// How much memory results may take together before the least recently
/// shown are let go of.
pub const MEMORY_BUDGET: usize = 1024 * 1024 * 1024;

/// A result tab's grid.
pub type ResultTable = Entity<TableState<ResultsDelegate>>;

/// Where a result tab's statement stands.
#[derive(Debug, Clone)]
pub enum ResultState {
    /// Rows are still arriving.
    Running {
        started: Instant,
    },
    Done(QuerySummary),
    /// It failed after some rows arrived.
    Failed {
        message: SharedString,
        elapsed: Duration,
    },
    /// Its rows were let go of to stay within [`MEMORY_BUDGET`].
    Released {
        rows: usize,
        elapsed: Duration,
    },
}

pub struct ResultData {
    statement: String,
    /// The connection it ran on, as the sidebar describes it.
    connection: SharedString,
    table: ResultTable,
    state: ResultState,
}

/// The result on screen, for tools that work on it (copy, export, filter).
pub struct ActiveResult<'a> {
    pub id: ResultTabId,
    /// The statement that produced it, as it was sent.
    #[allow(
        dead_code,
        reason = "for the grid tools: export as INSERT, find in results"
    )]
    pub statement: &'a str,
    pub table: &'a ResultTable,
    pub state: &'a ResultState,
}

impl<'a> ActiveResult<'a> {
    /// Its rows, still arriving while [`ResultState::Running`]; `None` once
    /// released.
    pub fn result_set<'b>(&self, cx: &'b App) -> Option<&'b ResultSet> {
        self.table.read(cx).delegate().result()
    }
}

/// The connection a run goes to.
#[derive(Debug, Clone)]
pub struct RunTarget {
    /// Its password-free key, for the history.
    pub key: String,
    /// As the sidebar describes it.
    pub label: SharedString,
    pub dialect: Dialect,
}

pub struct RunRequest {
    /// The editor tab running it, whose results it replaces.
    pub owner: TabId,
    /// That tab's title, for the status bar while another tab shows.
    pub owner_title: String,
    pub statements: Vec<String>,
    /// Run one statement again into this tab, rather than into a new one.
    pub into: Option<ResultTabId>,
    pub target: RunTarget,
}

pub enum ResultsEvent {
    /// A statement ended: what the history keeps of it.
    Executed(history::Entry),
    /// A run ended. It may have created or dropped something.
    RunEnded { changed_schema: bool },
    /// The user asked to run a result tab's statement again.
    Rerun(ResultTabId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Next,
    Stop,
}

/// The run in progress.
struct Active {
    id: u64,
    owner: TabId,
    owner_title: String,
    run: Run,
    into: Option<ResultTabId>,
    connection: SharedString,
    started: Instant,
    statement_started: Instant,
    /// The tab the running statement's rows go to.
    streaming: Option<ResultTabId>,
    /// The user picked a tab while it ran, so new results don't take over.
    picked: bool,
    changed_schema: bool,
}

/// How an editor tab's last run went, for the status bar over its Output.
struct LastRun {
    statements: usize,
    done: usize,
    failed: bool,
    cancelled: bool,
    elapsed: Duration,
}

/// A line for the status bar.
pub struct StatusLine {
    pub text: String,
    pub tone: Tone,
}

pub struct Results {
    tabs: ResultTabs<TabId, ResultData>,
    /// The editor tab whose results show.
    owner: TabId,
    active: Option<Active>,
    next_run: u64,
    last_runs: HashMap<TabId, LastRun>,
    task: Option<Task<()>>,
    ticker: Option<Task<()>>,
    /// The tab the context menu is for: set by a right click on it.
    menu_target: Option<View>,
    output_scroll: ScrollHandle,
}

impl EventEmitter<ResultsEvent> for Results {}

impl Results {
    pub fn new(owner: TabId) -> Self {
        Self {
            tabs: ResultTabs::new(),
            owner,
            active: None,
            next_run: 0,
            last_runs: HashMap::new(),
            task: None,
            ticker: None,
            menu_target: None,
            output_scroll: ScrollHandle::new(),
        }
    }

    // ── The API the rest of the app uses ─────────────────────────────────────

    /// The result tab on screen, if one is rather than the Output.
    pub fn active_result(&self) -> Option<ActiveResult<'_>> {
        let tab = self.tabs.viewed(self.owner)?;
        Some(ActiveResult {
            id: tab.id(),
            statement: &tab.data.statement,
            table: &tab.data.table,
            state: &tab.data.state,
        })
    }

    pub fn is_running(&self) -> bool {
        self.active.is_some()
    }

    /// The editor tab whose run is in progress.
    pub fn running_owner(&self) -> Option<TabId> {
        self.active.as_ref().map(|a| a.owner)
    }

    /// Show `owner`'s results: the editor switched tabs.
    pub fn set_owner(&mut self, owner: TabId, cx: &mut Context<Self>) {
        if self.owner == owner {
            return;
        }
        self.owner = owner;
        if let View::Result(id) = self.tabs.view(owner) {
            self.tabs.touch(id);
        }
        cx.notify();
    }

    /// The editor tab `owner` closed: its results go. Cancel its run first.
    pub fn remove_owner(&mut self, owner: TabId, cx: &mut Context<Self>) {
        if self.running_owner() == Some(owner) {
            self.abort(cx);
        }
        self.tabs.remove_owner(owner);
        self.last_runs.remove(&owner);
        cx.notify();
    }

    /// The statement of a result tab, to run it again.
    pub fn statement_of(&self, id: ResultTabId) -> Option<&str> {
        Some(&self.tabs.get(id)?.data.statement)
    }

    // ── Running ──────────────────────────────────────────────────────────────

    /// Run `request` on `session`. A run still in progress is let go of
    /// (cancel it on the session first).
    pub fn start(
        &mut self,
        session: Session,
        request: RunRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if request.statements.is_empty() {
            return;
        }
        self.abort(cx);
        let RunRequest {
            owner,
            owner_title,
            statements,
            into,
            target,
        } = request;
        let into = into.filter(|&id| self.tabs.get(id).is_some());
        let owner = into
            .and_then(|id| self.tabs.get(id))
            .map_or(owner, |tab| tab.owner);
        if into.is_none() {
            // Their rows are dropped with them.
            self.tabs.begin_run(owner);
        }
        let changed_schema = statements.iter().any(|s| target.dialect.changes_schema(s));
        let count = statements.len();
        self.next_run += 1;
        let id = self.next_run;
        let now = Instant::now();
        self.active = Some(Active {
            id,
            owner,
            owner_title,
            run: Run::new(statements, target.dialect, target.key),
            into,
            connection: target.label,
            started: now,
            statement_started: now,
            streaming: None,
            picked: false,
            changed_schema,
        });
        self.last_runs.remove(&owner);

        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            for ix in 0..count {
                let Ok(Some(statement)) = this.update(cx, |this, _| this.begin_statement(id, ix))
                else {
                    return;
                };
                let query = session.query(statement, QueryOptions::default());
                loop {
                    let Ok(first) = query.events.recv_async().await else {
                        // The connection closed under the statement.
                        let _ = this.update(cx, |this, cx| {
                            if this.active.as_ref().is_some_and(|a| a.id == id) {
                                this.abort(cx);
                            }
                        });
                        return;
                    };
                    // Apply everything that has arrived in one update, so a
                    // fast stream of batches costs one redraw, not one each.
                    let mut events = vec![first];
                    events.extend(query.events.try_iter());
                    match this.update_in(cx, |this, window, cx| this.apply(id, events, window, cx))
                    {
                        Ok(Flow::Continue) => {}
                        Ok(Flow::Next) => break,
                        Ok(Flow::Stop) | Err(_) => return,
                    }
                }
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

    /// Statement `ix` of run `id` is about to be sent: its text.
    fn begin_statement(&mut self, id: u64, ix: usize) -> Option<String> {
        let active = self.active.as_mut().filter(|a| a.id == id)?;
        active.run.start(ix, now_ms());
        active.statement_started = Instant::now();
        Some(active.run.statement(ix).to_owned())
    }

    fn apply(
        &mut self,
        id: u64,
        events: Vec<QueryEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Flow {
        let Some(mut active) = self.active.take_if(|a| a.id == id) else {
            return Flow::Stop;
        };
        let mut flow = Flow::Continue;
        let mut new_rows = None;
        for event in events {
            match active.run.apply(event) {
                Step::Nothing => {}
                Step::Open(schema) => self.open_result(&mut active, schema, window, cx),
                Step::Rows(batch) => {
                    if let Some(tab) = active.streaming.and_then(|id| self.tabs.get(id)) {
                        tab.data
                            .table
                            .update(cx, |table, _| table.delegate_mut().push(batch));
                        new_rows = Some(tab.data.table.clone());
                    }
                }
                Step::Finished(finished) => {
                    flow = if finished.more {
                        Flow::Next
                    } else {
                        Flow::Stop
                    };
                    self.finished(&mut active, *finished, cx);
                    break;
                }
            }
        }
        if let Some(table) = new_rows {
            table.update(cx, |_, cx| cx.notify());
            self.keep_memory(active.streaming, cx);
        }
        if flow == Flow::Stop {
            self.end(active, cx);
        } else {
            self.active = Some(active);
        }
        cx.notify();
        flow
    }

    /// The running statement returns rows: give them a tab and show it.
    fn open_result(
        &mut self,
        active: &mut Active,
        schema: SchemaRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ix = active.run.current().unwrap_or_default();
        let statement = active.run.statement(ix).to_owned();
        let state = ResultState::Running {
            started: active.statement_started,
        };
        // A rerun refills its tab, once.
        let id = match active.into.take().filter(|&id| self.tabs.get(id).is_some()) {
            Some(id) => {
                let tab = self.tabs.get_mut(id).expect("checked above");
                tab.data.state = state;
                tab.data.connection = active.connection.clone();
                tab.data.table.update(cx, |table, cx| {
                    table.delegate_mut().set_schema(schema);
                    table.refresh(cx);
                });
                id
            }
            None => {
                let table = cx.new(|cx| {
                    let mut table = TableState::new(ResultsDelegate::default(), window, cx)
                        .cell_selectable(true)
                        .row_header(false)
                        .sortable(false)
                        .col_movable(false);
                    table.delegate_mut().set_schema(schema);
                    // The table laid its columns out in `new`, before it had any.
                    table.refresh(cx);
                    table
                });
                let data = ResultData {
                    statement: statement.clone(),
                    connection: active.connection.clone(),
                    table,
                    state,
                };
                let title = title::title(&statement, active.run.dialect());
                let (id, _closed) = self.tabs.open(active.owner, title, data);
                id
            }
        };
        active.streaming = Some(id);
        if !active.picked {
            self.tabs.select(active.owner, View::Result(id));
        }
    }

    fn finished(&mut self, active: &mut Active, finished: run::Finished, cx: &mut Context<Self>) {
        let streaming = active.streaming.take();
        if let Some(tab) = streaming.and_then(|id| self.tabs.get_mut(id)) {
            tab.data.state = match (&finished.summary, &finished.error) {
                (_, Some(message)) => ResultState::Failed {
                    message: message.clone().into(),
                    elapsed: finished.elapsed,
                },
                (Some(summary), None) => ResultState::Done(summary.clone()),
                (None, None) => ResultState::Done(QuerySummary {
                    rows: 0,
                    elapsed: finished.elapsed,
                    truncated: false,
                    cancelled: false,
                }),
            };
            tab.data.table.update(cx, |_, cx| cx.notify());
        }
        let owner = active.owner;
        self.tabs.log(owner, finished.line);
        if let Some(id) = streaming.filter(|&id| finished.opened && self.tabs.get(id).is_some()) {
            self.tabs.link_last_line(owner, id);
        }
        if owner == self.owner {
            self.output_scroll.scroll_to_bottom();
        }
        // A failure stops the run: show why, unless the user is looking at
        // something else they picked.
        if finished.outcome == Outcome::Failed && !active.picked {
            self.tabs.select(owner, View::Output);
        }
        cx.emit(ResultsEvent::Executed(finished.entry));
    }

    /// The run is over: note how it went for the status bar.
    fn end(&mut self, active: Active, cx: &mut Context<Self>) {
        let (done, failed, cancelled) = active.run.progress();
        self.last_runs.insert(
            active.owner,
            LastRun {
                statements: active.run.len(),
                done,
                failed,
                cancelled,
                elapsed: active.started.elapsed(),
            },
        );
        cx.emit(ResultsEvent::RunEnded {
            changed_schema: active.changed_schema,
        });
    }

    /// Let the run in progress go: the statement running counts as
    /// cancelled. The caller cancels it on the session.
    pub fn abort(&mut self, cx: &mut Context<Self>) {
        let Some(mut active) = self.active.take() else {
            return;
        };
        let rows = active
            .streaming
            .and_then(|id| self.tabs.get(id))
            .map_or(0, |tab| tab.data.table.read(cx).delegate().row_count());
        if let Some(finished) = active.run.abort(rows, active.statement_started.elapsed()) {
            self.finished(&mut active, *finished, cx);
        }
        self.end(active, cx);
        cx.notify();
    }

    /// Let go of the rows of results not on screen until all of them fit in
    /// [`MEMORY_BUDGET`]. `streaming` is kept too.
    fn keep_memory(&mut self, streaming: Option<ResultTabId>, cx: &mut Context<Self>) {
        let size = |tab: &tabs::ResultTab<TabId, ResultData>, cx: &App| {
            tab.data.table.read(cx).delegate().memory_bytes()
        };
        let mut total: usize = self.tabs.iter().map(|tab| size(tab, cx)).sum();
        if total <= MEMORY_BUDGET {
            return;
        }
        let keep: Vec<ResultTabId> = self
            .tabs
            .viewed(self.owner)
            .map(|t| t.id())
            .into_iter()
            .chain(streaming)
            .collect();
        for id in self.tabs.release_order(&keep) {
            let Some(tab) = self.tabs.get_mut(id) else {
                continue;
            };
            let (bytes, rows) = {
                let delegate = tab.data.table.read(cx).delegate();
                (delegate.memory_bytes(), delegate.row_count())
            };
            if bytes == 0 {
                continue;
            }
            let elapsed = match &tab.data.state {
                ResultState::Done(summary) => summary.elapsed,
                ResultState::Failed { elapsed, .. } => *elapsed,
                ResultState::Running { started } => started.elapsed(),
                ResultState::Released { elapsed, .. } => *elapsed,
            };
            tab.data.state = ResultState::Released { rows, elapsed };
            tab.data.table.update(cx, |table, cx| {
                table.delegate_mut().clear();
                table.refresh(cx);
            });
            log::info!("released a result of {rows} rows ({bytes} bytes) to stay within memory");
            total = total.saturating_sub(bytes);
            if total <= MEMORY_BUDGET {
                break;
            }
        }
    }

    // ── Tabs ─────────────────────────────────────────────────────────────────

    pub fn view(&self) -> View {
        self.tabs.view(self.owner)
    }

    pub fn select(&mut self, view: View, cx: &mut Context<Self>) {
        if self.tabs.select(self.owner, view) {
            if let Some(active) = self.active.as_mut().filter(|a| a.owner == self.owner) {
                active.picked = true;
            }
            if view == View::Output {
                self.output_scroll.scroll_to_bottom();
            }
            cx.notify();
        }
    }

    /// Show the view `delta` places along the strip, wrapping around.
    pub fn cycle(&mut self, delta: isize, cx: &mut Context<Self>) {
        let strip: Vec<View> = std::iter::once(View::Output)
            .chain(self.tabs.shown(self.owner).map(|t| View::Result(t.id())))
            .collect();
        let at = strip.iter().position(|&v| v == self.view()).unwrap_or(0) as isize;
        let next = strip[(at + delta).rem_euclid(strip.len() as isize) as usize];
        self.select(next, cx);
    }

    /// The result tab on screen, for actions on "the current tab".
    fn active_id(&self) -> Option<ResultTabId> {
        self.active_result().map(|result| result.id)
    }

    pub fn close(&mut self, id: ResultTabId, cx: &mut Context<Self>) {
        self.tabs.close(id);
        cx.notify();
    }

    pub fn close_others(&mut self, id: ResultTabId, cx: &mut Context<Self>) {
        self.tabs.close_others(id);
        cx.notify();
    }

    pub fn close_unpinned(&mut self, cx: &mut Context<Self>) {
        self.tabs.close_unpinned(self.owner);
        cx.notify();
    }

    pub fn set_pinned(&mut self, id: ResultTabId, pinned: bool, cx: &mut Context<Self>) {
        self.tabs.set_pinned(id, pinned);
        cx.notify();
    }

    pub fn rename(&mut self, id: ResultTabId, name: &str, cx: &mut Context<Self>) {
        self.tabs.rename(id, name);
        cx.notify();
    }

    pub fn clear_output(&mut self, cx: &mut Context<Self>) {
        self.tabs.clear_output(self.owner);
        cx.notify();
    }

    pub fn rerun(&mut self, id: ResultTabId, cx: &mut Context<Self>) {
        if self.tabs.get(id).is_some() {
            cx.emit(ResultsEvent::Rerun(id));
        }
    }

    // ── Status bar ───────────────────────────────────────────────────────────

    /// What the status bar says about the results on screen.
    pub fn status(&self, cx: &App) -> StatusLine {
        let running_here = self.active.as_ref().filter(|a| a.owner == self.owner);
        let step = running_here
            .filter(|a| a.run.len() > 1)
            .and_then(|a| a.run.current().map(|ix| (ix + 1, a.run.len())))
            .map(|(at, of)| format!("statement {at} of {of} · "))
            .unwrap_or_default();
        let mut line = match self.active_result() {
            Some(result) => {
                let rows = result.result_set(cx).map_or(0, ResultSet::row_count);
                match result.state {
                    ResultState::Running { started } => StatusLine {
                        text: format!(
                            "Running… {step}{} rows · {}",
                            format_count(rows),
                            format_elapsed(started.elapsed())
                        ),
                        tone: Tone::Normal,
                    },
                    ResultState::Done(summary) => {
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
                        StatusLine {
                            text,
                            tone: Tone::Muted,
                        }
                    }
                    ResultState::Failed { elapsed, .. } => StatusLine {
                        text: format!(
                            "Failed after {} · {} rows",
                            format_elapsed(*elapsed),
                            format_count(rows)
                        ),
                        tone: Tone::Error,
                    },
                    ResultState::Released { rows, elapsed } => StatusLine {
                        text: format!(
                            "{} rows in {} · let go of to save memory",
                            format_count(*rows),
                            format_elapsed(*elapsed)
                        ),
                        tone: Tone::Muted,
                    },
                }
            }
            None => match (running_here, self.last_runs.get(&self.owner)) {
                (Some(active), _) => StatusLine {
                    text: format!(
                        "Running… {step}{}",
                        format_elapsed(active.statement_started.elapsed())
                    ),
                    tone: Tone::Normal,
                },
                (None, Some(last)) => StatusLine {
                    text: format!(
                        "{} in {}",
                        run::summary_text(last.statements, last.done, last.failed, last.cancelled),
                        format_elapsed(last.elapsed)
                    ),
                    tone: if last.failed {
                        Tone::Error
                    } else {
                        Tone::Muted
                    },
                },
                (None, None) => StatusLine {
                    text: "Ready".into(),
                    tone: Tone::Muted,
                },
            },
        };
        if let Some(active) = self.active.as_ref().filter(|a| a.owner != self.owner) {
            line.text = format!("{} · running in {}…", line.text, active.owner_title);
        }
        line
    }
}
