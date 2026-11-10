//! A connection owned by its own thread.
//!
//! Engine connections are generally not `Sync` and their calls block, so each
//! [`Session`] moves its [`Driver`] onto a dedicated worker thread and talks
//! to it over channels. The UI never blocks on the engine: it submits work,
//! receives [`QueryEvent`]s as batches arrive, and can cancel from any thread.

use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use arrow::datatypes::SchemaRef;
use arrow::record_batch::RecordBatch;

use crate::catalog::Catalog;
use crate::dialect::Flavor;

/// Receives a statement's output as the engine produces it.
pub trait BatchSink {
    /// Called once, before any batch, with the result's shape.
    fn schema(&mut self, schema: SchemaRef);
    /// Called per batch. Return `Break` to stop fetching.
    fn batch(&mut self, batch: RecordBatch) -> ControlFlow<()>;
}

/// Interrupts whatever the driver is running. Must be callable from any
/// thread while the driver's own thread is blocked in the engine.
pub type Canceller = Arc<dyn Fn() + Send + Sync>;

/// A database engine behind one connection.
pub trait Driver: Send + 'static {
    /// The dialect and the version of the engine this connection reached,
    /// read once when the connection opens.
    fn flavor(&self) -> Flavor;

    /// Human-readable target, e.g. a file path or `:memory:`.
    fn target(&self) -> String;

    /// Run one statement, streaming its rows to `sink`. Statements that
    /// return no rows should still report a schema (an empty one is fine),
    /// or a one-row result such as the number of rows changed.
    fn execute(&mut self, sql: &str, sink: &mut dyn BatchSink) -> anyhow::Result<()>;

    /// Read the objects the connection can see.
    fn catalog(&mut self) -> anyhow::Result<Catalog>;

    fn canceller(&self) -> Canceller;
}

/// A boxed driver is a driver, so the app can pick an engine at runtime.
impl<D: Driver + ?Sized> Driver for Box<D> {
    fn flavor(&self) -> Flavor {
        (**self).flavor()
    }

    fn target(&self) -> String {
        (**self).target()
    }

    fn execute(&mut self, sql: &str, sink: &mut dyn BatchSink) -> anyhow::Result<()> {
        (**self).execute(sql, sink)
    }

    fn catalog(&mut self) -> anyhow::Result<Catalog> {
        (**self).catalog()
    }

    fn canceller(&self) -> Canceller {
        (**self).canceller()
    }
}

pub type QueryId = u64;

#[derive(Debug, Clone)]
pub enum QueryEvent {
    Schema(SchemaRef),
    Batch(RecordBatch),
    Finished(QuerySummary),
    Failed { message: String, elapsed: Duration },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuerySummary {
    pub rows: usize,
    pub elapsed: Duration,
    /// Rows stopped at the row limit, so more may exist.
    pub truncated: bool,
    pub cancelled: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct QueryOptions {
    /// Stop fetching after this many rows. The engine still finishes the
    /// statement's side effects; only the fetch stops.
    pub max_rows: usize,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            max_rows: 1_000_000,
        }
    }
}

enum Request {
    Query {
        id: QueryId,
        sql: String,
        options: QueryOptions,
        events: flume::Sender<QueryEvent>,
    },
    Catalog {
        reply: flume::Sender<anyhow::Result<Catalog>>,
    },
}

/// A handle to a connection running on its own thread. Cheap to clone.
#[derive(Clone)]
pub struct Session {
    inner: Arc<SessionInner>,
}

struct SessionInner {
    flavor: Flavor,
    target: String,
    requests: flume::Sender<Request>,
    canceller: Canceller,
    /// The query currently allowed to run; a cancel bumps past it.
    cancelled_through: AtomicU64,
    next_id: AtomicU64,
    running: Arc<AtomicBool>,
}

/// A query in flight. Events arrive on [`RunningQuery::events`].
pub struct RunningQuery {
    pub id: QueryId,
    pub events: flume::Receiver<QueryEvent>,
}

impl Session {
    pub fn spawn(driver: impl Driver) -> Self {
        let flavor = driver.flavor();
        let target = driver.target();
        let canceller = driver.canceller();
        let (requests, inbox) = flume::unbounded();
        let running = Arc::new(AtomicBool::new(false));
        let inner = Arc::new(SessionInner {
            flavor,
            target,
            requests,
            canceller,
            cancelled_through: AtomicU64::new(0),
            next_id: AtomicU64::new(1),
            running: running.clone(),
        });

        let worker = Arc::downgrade(&inner);
        thread::Builder::new()
            .name(format!("quarry-session-{}", inner.target))
            .spawn(move || run_worker(driver, inbox, worker, running))
            .expect("spawn session thread");

        Self { inner }
    }

    pub fn flavor(&self) -> &Flavor {
        &self.inner.flavor
    }

    pub fn target(&self) -> &str {
        &self.inner.target
    }

    pub fn is_running(&self) -> bool {
        self.inner.running.load(Ordering::Acquire)
    }

    /// Queue `sql` (one statement) to run after any earlier requests.
    pub fn query(&self, sql: impl Into<String>, options: QueryOptions) -> RunningQuery {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (events, receiver) = flume::unbounded();
        let request = Request::Query {
            id,
            sql: sql.into(),
            options,
            events,
        };
        if let Err(flume::SendError(Request::Query { events, .. })) =
            self.inner.requests.send(request)
        {
            let _ = events.send(QueryEvent::Failed {
                message: "the connection has closed".into(),
                elapsed: Duration::ZERO,
            });
        }
        RunningQuery {
            id,
            events: receiver,
        }
    }

    /// Cancel every query submitted so far, running or queued.
    pub fn cancel(&self) {
        let last = self.inner.next_id.load(Ordering::Relaxed) - 1;
        self.inner
            .cancelled_through
            .fetch_max(last, Ordering::AcqRel);
        if self.is_running() {
            (self.inner.canceller)();
        }
    }

    pub fn catalog(&self) -> flume::Receiver<anyhow::Result<Catalog>> {
        let (reply, receiver) = flume::bounded(1);
        if let Err(flume::SendError(Request::Catalog { reply })) =
            self.inner.requests.send(Request::Catalog { reply })
        {
            let _ = reply.send(Err(anyhow::anyhow!("the connection has closed")));
        }
        receiver
    }
}

fn run_worker(
    mut driver: impl Driver,
    inbox: flume::Receiver<Request>,
    session: std::sync::Weak<SessionInner>,
    running: Arc<AtomicBool>,
) {
    while let Ok(request) = inbox.recv() {
        match request {
            Request::Query {
                id,
                sql,
                options,
                events,
            } => {
                let is_cancelled = || {
                    session
                        .upgrade()
                        .is_none_or(|s| s.cancelled_through.load(Ordering::Acquire) >= id)
                };
                let started = Instant::now();
                if is_cancelled() {
                    let _ = events.send(QueryEvent::Finished(QuerySummary {
                        rows: 0,
                        elapsed: Duration::ZERO,
                        truncated: false,
                        cancelled: true,
                    }));
                    continue;
                }

                running.store(true, Ordering::Release);
                let mut sink = ChannelSink {
                    events: &events,
                    rows: 0,
                    max_rows: options.max_rows,
                    truncated: false,
                    cancelled: &is_cancelled,
                };
                let outcome = driver.execute(&sql, &mut sink);
                running.store(false, Ordering::Release);

                let (rows, truncated) = (sink.rows, sink.truncated);
                let cancelled = is_cancelled();
                let event = match outcome {
                    Ok(()) => QueryEvent::Finished(QuerySummary {
                        rows,
                        elapsed: started.elapsed(),
                        truncated,
                        cancelled,
                    }),
                    Err(_) if cancelled => QueryEvent::Finished(QuerySummary {
                        rows,
                        elapsed: started.elapsed(),
                        truncated,
                        cancelled: true,
                    }),
                    Err(err) => QueryEvent::Failed {
                        message: format!("{err:#}"),
                        elapsed: started.elapsed(),
                    },
                };
                let _ = events.send(event);
            }
            Request::Catalog { reply } => {
                let _ = reply.send(driver.catalog());
            }
        }
    }
    log::debug!("session worker for {} stopped", driver.target());
}

struct ChannelSink<'a, F: Fn() -> bool> {
    events: &'a flume::Sender<QueryEvent>,
    rows: usize,
    max_rows: usize,
    truncated: bool,
    cancelled: &'a F,
}

impl<F: Fn() -> bool> BatchSink for ChannelSink<'_, F> {
    fn schema(&mut self, schema: SchemaRef) {
        let _ = self.events.send(QueryEvent::Schema(schema));
    }

    fn batch(&mut self, batch: RecordBatch) -> ControlFlow<()> {
        if (self.cancelled)() {
            return ControlFlow::Break(());
        }
        if batch.num_rows() == 0 {
            return ControlFlow::Continue(());
        }
        // Rows beyond the limit exist, so the result is incomplete. Checked
        // on the next non-empty batch rather than when the limit is reached,
        // so a result of exactly `max_rows` rows is not reported as cut.
        if self.rows >= self.max_rows {
            self.truncated = true;
            return ControlFlow::Break(());
        }
        let room = self.max_rows - self.rows;
        let batch = if batch.num_rows() > room {
            self.truncated = true;
            batch.slice(0, room)
        } else {
            batch
        };
        self.rows += batch.num_rows();
        if self.events.send(QueryEvent::Batch(batch)).is_err() {
            // Nobody is listening any more.
            return ControlFlow::Break(());
        }
        if self.truncated {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }
}
