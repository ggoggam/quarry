//! PostgreSQL driver for Quarry.
//!
//! Built on tokio-postgres, driven by a current-thread tokio runtime the
//! driver owns, so it fits [`quarry_core::Session`]'s one-thread-per-
//! connection model: every call blocks the session's thread on the runtime,
//! which also drives the connection's I/O.
//!
//! A statement runs in two steps. It is first prepared, which describes its
//! result columns (types and type modifiers) without running it. It then
//! runs through the simple query protocol, whose rows arrive in the text
//! format every type has, and are parsed into typed Arrow arrays where an
//! Arrow type fits (see [`convert`]). Rows stream: the first batch is sent
//! after 100 rows or 50 ms, later ones after 4096 rows or 250 ms.
//!
//! The driver adapts to the server's release; see [`gates`].

mod catalog;
mod config;
mod convert;
mod error;
pub mod gates;
mod statement;

use std::pin::{Pin, pin};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use arrow::array::{ArrayRef, Int64Array, RecordBatch, RecordBatchOptions};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use futures_util::StreamExt as _;
use quarry_core::catalog::Catalog;
use quarry_core::result::{empty_schema, native_type};
use quarry_core::{BatchSink, Canceller, Dialect, Driver, Flavor};
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout, timeout_at};
use tokio_postgres::{CancelToken, Client, SimpleQueryMessage, SimpleQueryRow, SimpleQueryStream};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::convert::ColumnKind;

/// The first batch goes out after this many rows or this long, whichever
/// comes first, so the grid fills quickly; later batches are larger.
const FIRST_BATCH_ROWS: usize = 100;
const FIRST_BATCH_WAIT: Duration = Duration::from_millis(50);
const BATCH_ROWS: usize = 4096;
const BATCH_WAIT: Duration = Duration::from_millis(250);
/// Wide rows (large json, bytea) flush on size before the row count.
const BATCH_BYTES: usize = 8 << 20;

/// After the sink stops, how long to keep reading (and discarding) the rest
/// of a result before asking the server to cancel it.
const DRAIN_BEFORE_CANCEL: Duration = Duration::from_millis(200);
const CANCEL_TIMEOUT: Duration = Duration::from_secs(10);

/// True when no explicit transaction block is open: a statement in
/// autocommit starts its own transaction, so `now()`, the transaction's
/// start, equals the statement's start. Inside `BEGIN` they differ.
const AUTOCOMMIT_PROBE: &str = "SELECT now() = statement_timestamp()";

/// One connection to a PostgreSQL server.
pub struct PostgresDriver {
    runtime: Runtime,
    /// `None` only while dropping.
    client: Option<Client>,
    connection: Option<JoinHandle<()>>,
    cancel: CancelToken,
    tls: MakeRustlsConnect,
    flavor: Flavor,
    target: String,
}

impl PostgresDriver {
    /// Connect with a `postgres://` / `postgresql://` URL or a libpq
    /// `key=value` string. `sslmode` may be `disable`, `prefer` (the
    /// default), `require`, `verify-ca` or `verify-full`; `sslrootcert`
    /// names a PEM file of trusted roots (default: the system's and
    /// Mozilla's). Gives up after `connect_timeout` (default 10 s).
    ///
    /// Blocks; do not call it from inside a tokio runtime.
    pub fn connect(url: &str) -> anyhow::Result<Self> {
        let options = config::parse(url)?;
        let target = config::display_target(&options.config);
        let tls = config::tls_connector(&options)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("starting the PostgreSQL client runtime")?;

        let wait = options.connect_timeout();
        // `timeout` must be built inside the runtime: its timer needs the
        // runtime's reactor.
        let (client, connection) = runtime
            .block_on(async { timeout(wait, options.config.connect(tls.clone())).await })
            .map_err(|_| {
                anyhow!(
                    "could not connect to {target}: no answer within {}s",
                    wait.as_secs()
                )
            })?
            .map_err(|err| {
                anyhow!(
                    "could not connect to {target}: {}",
                    error::describe(&err, None)
                )
            })?;
        let connection = runtime.spawn(async move {
            if let Err(err) = connection.await {
                log::warn!("PostgreSQL connection ended: {err}");
            }
        });

        let setup = async {
            let version =
                first_value(&client.simple_query("SHOW server_version").await?).unwrap_or_default();
            let flavor = Flavor::new(Dialect::Postgres, short_version(&version));
            if !gates::prints_floats_exactly(&flavor) {
                client.batch_execute("SET extra_float_digits = 3").await?;
            }
            Ok::<_, tokio_postgres::Error>(flavor)
        };
        let flavor = runtime
            .block_on(setup)
            .map_err(|err| anyhow!("{target}: {}", error::describe(&err, None)))?;
        log::info!("connected to {target} ({flavor})");

        Ok(Self {
            runtime,
            cancel: client.cancel_token(),
            client: Some(client),
            connection: Some(connection),
            tls,
            flavor,
            target,
        })
    }

    fn client(&self) -> anyhow::Result<&Client> {
        self.client.as_ref().context("the connection has closed")
    }
}

impl Driver for PostgresDriver {
    fn flavor(&self) -> Flavor {
        self.flavor.clone()
    }

    fn target(&self) -> String {
        self.target.clone()
    }

    fn execute(&mut self, sql: &str, sink: &mut dyn BatchSink) -> anyhow::Result<()> {
        let client = self.client()?;
        let canceller = (&self.cancel, &self.tls);
        self.runtime.block_on(execute(client, canceller, sql, sink))
    }

    fn catalog(&mut self) -> anyhow::Result<Catalog> {
        let client = self.client()?;
        self.runtime
            .block_on(catalog::read(client, &self.flavor))
            .map_err(|err| anyhow!(error::describe(&err, None)))
    }

    fn canceller(&self) -> Canceller {
        let token = self.cancel.clone();
        let tls = self.tls.clone();
        Arc::new(move || {
            let (token, tls) = (token.clone(), tls.clone());
            // The caller may be the UI thread: send the cancel request from
            // a thread of its own rather than wait for the server.
            let spawned = thread::Builder::new()
                .name("quarry-postgres-cancel".into())
                .spawn(move || {
                    let runtime = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(runtime) => runtime,
                        Err(err) => return log::warn!("could not cancel: {err}"),
                    };
                    runtime.block_on(send_cancel(&token, &tls));
                });
            if let Err(err) = spawned {
                log::warn!("could not cancel: {err}");
            }
        })
    }
}

impl Drop for PostgresDriver {
    fn drop(&mut self) {
        // Dropping the client asks the connection task to send Terminate;
        // give it a moment so the server sees a clean goodbye.
        drop(self.client.take());
        if let Some(connection) = self.connection.take()
            && tokio::runtime::Handle::try_current().is_err()
        {
            let _ = self
                .runtime
                .block_on(async { timeout(Duration::from_secs(1), connection).await });
        }
    }
}

async fn send_cancel(token: &CancelToken, tls: &MakeRustlsConnect) {
    match timeout(CANCEL_TIMEOUT, token.cancel_query(tls.clone())).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => log::warn!("cancel request failed: {err}"),
        Err(_) => log::warn!("cancel request timed out"),
    }
}

/// `"16.4"` from `"16.4 (Debian 16.4-1.pgdg120+2)"`; `"17beta1"` stays.
fn short_version(server_version: &str) -> &str {
    server_version
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_end_matches(|c: char| !c.is_ascii_alphanumeric())
}

fn first_value(messages: &[SimpleQueryMessage]) -> Option<String> {
    messages.iter().find_map(|message| match message {
        SimpleQueryMessage::Row(row) => row.get(0).map(str::to_owned),
        _ => None,
    })
}

/// A result column as the prepared statement described it.
struct ColumnSpec {
    name: String,
    kind: ColumnKind,
    native: String,
}

async fn execute(
    client: &Client,
    canceller: (&CancelToken, &MakeRustlsConnect),
    sql: &str,
    sink: &mut dyn BatchSink,
) -> anyhow::Result<()> {
    let fail = |err: tokio_postgres::Error| anyhow!(error::describe(&err, Some(sql)));

    // Describe the result first. For reads, also learn whether a
    // transaction block is open, in the same round trip: stopping a read
    // early is done with a cancel, which must not abort the user's
    // transaction.
    let read_only = statement::is_read_only(sql);
    let (prepared, autocommit) = if read_only {
        let (prepared, probe) =
            futures_util::future::join(client.prepare(sql), client.simple_query(AUTOCOMMIT_PROBE))
                .await;
        let autocommit = probe.is_ok_and(|rows| first_value(&rows).as_deref() == Some("t"));
        (prepared, autocommit)
    } else {
        (client.prepare(sql).await, false)
    };
    let (columns, prepare_error) = match prepared {
        Ok(statement) => {
            let columns = statement
                .columns()
                .iter()
                .map(|column| ColumnSpec {
                    name: column.name().to_owned(),
                    kind: ColumnKind::of(column.type_(), column.type_modifier()),
                    native: convert::native_name(column.type_(), column.type_modifier()),
                })
                .collect::<Vec<_>>();
            (Some(columns), None)
        }
        Err(err) => {
            // Several statements in one string cannot be prepared but run
            // fine; run it anyway and report its own error if it has one.
            log::debug!("could not prepare, running untyped: {err}");
            (None, Some(err))
        }
    };

    let stream = client.simple_query_raw(sql).await.map_err(fail)?;
    let mut stream = pin!(stream);
    let mut output = Output {
        sink,
        rows: None,
        statements_done: 0,
    };
    let stopped = loop {
        let next = match output.deadline() {
            Some(deadline) => match timeout_at(deadline, stream.next()).await {
                Ok(next) => next,
                Err(_) => {
                    if output.flush()?.is_break() {
                        break true;
                    }
                    continue;
                }
            },
            None => stream.next().await,
        };
        let message = match next {
            None => break false,
            Some(Ok(message)) => message,
            Some(Err(err)) => {
                // When preparing failed inside a transaction block, the
                // block is now aborted and the run only says so; the
                // prepare's error is the one that explains why.
                let aborted =
                    err.code() == Some(&tokio_postgres::error::SqlState::IN_FAILED_SQL_TRANSACTION);
                return Err(match prepare_error {
                    Some(prepare_error) if aborted && output.statements_done == 0 => {
                        fail(prepare_error)
                    }
                    _ => fail(err),
                });
            }
        };
        if output.statements_done > 0 {
            // Only the first statement's result is shown.
            continue;
        }
        match message {
            SimpleQueryMessage::RowDescription(described) => {
                let spec = columns
                    .as_ref()
                    .filter(|columns| columns.len() == described.len());
                output.start_rows(match spec {
                    Some(columns) => Rows::typed(columns),
                    None => Rows::untyped(described.iter().map(|c| c.name())),
                });
            }
            SimpleQueryMessage::Row(row) => {
                if output.push(row)?.is_break() {
                    break true;
                }
            }
            SimpleQueryMessage::CommandComplete(count) => {
                let flow = output.complete(count, sql)?;
                if flow.is_break() {
                    break true;
                }
            }
            _ => {}
        }
    };
    if stopped {
        let cancel = (read_only && autocommit).then_some(canceller);
        finish_early(stream, cancel).await;
    }
    Ok(())
}

/// Read the rest of a result the sink no longer wants, so the connection
/// is ready for the next statement. A read in autocommit that is still
/// going after a moment is cancelled; anything else is drained, since a
/// cancel would undo writes or abort the open transaction.
async fn finish_early(
    mut stream: Pin<&mut SimpleQueryStream>,
    cancel: Option<(&CancelToken, &MakeRustlsConnect)>,
) {
    let deadline = Instant::now() + DRAIN_BEFORE_CANCEL;
    loop {
        match timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(_))) => continue,
            // Done, or ended by an error nobody is waiting for.
            Ok(None | Some(Err(_))) => return,
            Err(_) => break,
        }
    }
    if let Some((token, tls)) = cancel {
        log::debug!("cancelling the rest of a result");
        send_cancel(token, tls).await;
    }
    while let Some(Ok(_)) = stream.next().await {}
}

/// Where a statement's results go: rows into batches for the sink.
struct Output<'a> {
    sink: &'a mut dyn BatchSink,
    rows: Option<Rows>,
    statements_done: usize,
}

impl Output<'_> {
    fn deadline(&self) -> Option<Instant> {
        self.rows.as_ref().and_then(|rows| rows.deadline)
    }

    fn start_rows(&mut self, rows: Rows) {
        self.sink.schema(rows.schema.clone());
        self.rows = Some(rows);
    }

    fn push(&mut self, row: SimpleQueryRow) -> anyhow::Result<std::ops::ControlFlow<()>> {
        let Some(rows) = self.rows.as_mut() else {
            return Ok(std::ops::ControlFlow::Continue(()));
        };
        if rows.push(row) {
            self.flush()
        } else {
            Ok(std::ops::ControlFlow::Continue(()))
        }
    }

    fn flush(&mut self) -> anyhow::Result<std::ops::ControlFlow<()>> {
        match self.rows.as_mut().map(Rows::take).transpose()?.flatten() {
            Some(batch) => Ok(self.sink.batch(batch)),
            None => Ok(std::ops::ControlFlow::Continue(())),
        }
    }

    /// The first statement finished: send what rows remain, or for a
    /// statement without rows, its row count or an empty result.
    fn complete(&mut self, count: u64, sql: &str) -> anyhow::Result<std::ops::ControlFlow<()>> {
        self.statements_done += 1;
        if self.rows.is_some() {
            return self.flush();
        }
        if statement::reports_rows_affected(sql) {
            let schema = Arc::new(Schema::new(vec![Field::new(
                "rows affected",
                DataType::Int64,
                false,
            )]));
            let count = i64::try_from(count).unwrap_or(i64::MAX);
            let column: ArrayRef = Arc::new(Int64Array::from(vec![count]));
            self.sink.schema(schema.clone());
            Ok(self.sink.batch(RecordBatch::try_new(schema, vec![column])?))
        } else {
            self.sink.schema(empty_schema());
            Ok(std::ops::ControlFlow::Continue(()))
        }
    }
}

/// Rows of the result being read, waiting to become the next batch.
struct Rows {
    schema: SchemaRef,
    kinds: Vec<ColumnKind>,
    pending: Vec<SimpleQueryRow>,
    bytes: usize,
    batches: usize,
    deadline: Option<Instant>,
}

impl Rows {
    fn typed(columns: &[ColumnSpec]) -> Self {
        let fields: Vec<Field> = columns
            .iter()
            .map(|column| {
                native_type(
                    Field::new(&column.name, column.kind.data_type(), true),
                    &column.native,
                )
            })
            .collect();
        Self::new(
            Arc::new(Schema::new(fields)),
            columns.iter().map(|c| c.kind).collect(),
        )
    }

    /// Text columns, when the statement could not be described up front.
    fn untyped<'a>(names: impl Iterator<Item = &'a str>) -> Self {
        let fields: Vec<Field> = names
            .map(|name| Field::new(name, DataType::Utf8, true))
            .collect();
        let kinds = vec![ColumnKind::Text; fields.len()];
        Self::new(Arc::new(Schema::new(fields)), kinds)
    }

    fn new(schema: SchemaRef, kinds: Vec<ColumnKind>) -> Self {
        Self {
            schema,
            kinds,
            pending: Vec::new(),
            bytes: 0,
            batches: 0,
            deadline: None,
        }
    }

    /// Queue a row; true when the batch is full.
    fn push(&mut self, row: SimpleQueryRow) -> bool {
        let first = self.batches == 0;
        if self.pending.is_empty() {
            let wait = if first { FIRST_BATCH_WAIT } else { BATCH_WAIT };
            self.deadline = Some(Instant::now() + wait);
        }
        self.bytes += (0..row.len())
            .filter_map(|i| row.try_get(i).ok().flatten())
            .map(str::len)
            .sum::<usize>();
        self.pending.push(row);
        let limit = if first { FIRST_BATCH_ROWS } else { BATCH_ROWS };
        self.pending.len() >= limit || self.bytes >= BATCH_BYTES
    }

    fn take(&mut self) -> anyhow::Result<Option<RecordBatch>> {
        self.deadline = None;
        if self.pending.is_empty() {
            return Ok(None);
        }
        let rows = std::mem::take(&mut self.pending);
        self.bytes = 0;
        self.batches += 1;
        build_batch(&self.schema, &self.kinds, &rows).map(Some)
    }
}

fn build_batch(
    schema: &SchemaRef,
    kinds: &[ColumnKind],
    rows: &[SimpleQueryRow],
) -> anyhow::Result<RecordBatch> {
    let mut columns = Vec::with_capacity(kinds.len());
    let mut fields = Vec::with_capacity(kinds.len());
    let mut retyped = false;
    for (i, (field, kind)) in schema.fields().iter().zip(kinds).enumerate() {
        let values = rows
            .iter()
            .map(|row| row.try_get(i))
            .collect::<Result<Vec<_>, _>>()?;
        let column = convert::column(*kind, &values);
        if column.data_type() == field.data_type() {
            fields.push(field.clone());
        } else {
            // This batch needed text, or another numeric scale.
            retyped = true;
            fields.push(Arc::new(
                field
                    .as_ref()
                    .clone()
                    .with_data_type(column.data_type().clone()),
            ));
        }
        columns.push(column);
    }
    let schema = if retyped {
        Arc::new(Schema::new_with_metadata(fields, schema.metadata().clone()))
    } else {
        schema.clone()
    };
    // `SELECT FROM t` returns rows without columns.
    let options = RecordBatchOptions::new().with_row_count(Some(rows.len()));
    Ok(RecordBatch::try_new_with_options(
        schema, columns, &options,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_server_versions() {
        assert_eq!(short_version("16.4 (Debian 16.4-1.pgdg120+2)"), "16.4");
        assert_eq!(short_version("17beta1"), "17beta1");
        assert_eq!(short_version("13.16"), "13.16");
        assert_eq!(short_version("9.6.24, compiled by gcc"), "9.6.24");
        assert_eq!(short_version(""), "");
        let flavor = Flavor::new(Dialect::Postgres, short_version("17beta1"));
        assert!(flavor.at_least(17, 0));
        assert!(!flavor.at_least(17, 1));
    }
}
