//! MySQL and MariaDB driver for Quarry.
//!
//! One [`mysql::Conn`] per driver, used from the session's thread. Queries
//! run over the text protocol with [`Queryable::query_iter`], and rows are
//! parsed by their column metadata into typed Arrow batches as they arrive
//! (see [`convert`]): the first batch goes out after 100 rows or 50 ms, the
//! rest every 4096 rows (or every second on a slow stream).
//!
//! The server's flavor and version are read once at connect, and the
//! places MySQL and MariaDB releases differ are gated by the helpers in
//! [`flavor`].
//!
//! # Stopping early
//!
//! The protocol has no way to stop a result set short: the client must
//! read it to the end before the connection can be used again. When the
//! sink stops (row limit, cancel), the driver keeps reading and discarding
//! rows for up to [`DRAIN_BUDGET`]; a result still streaming after that is
//! stopped with `KILL QUERY` from a side connection, which ends it with an
//! error the driver swallows. The connection, and with it the session's
//! state (current database, variables, temporary tables, transaction), is
//! kept.
//!
//! # Several result sets
//!
//! A `CALL` can return several result sets. The first with columns is
//! shown; the rest are read and dropped.

pub mod catalog;
pub mod convert;
pub mod flavor;
pub mod options;

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use arrow::array::{ArrayRef, Int64Array};
use arrow::datatypes::{Field, Schema, SchemaRef};
use arrow::record_batch::RecordBatch;
use mysql::prelude::Queryable;
use mysql::{Conn, DriverError, Opts, OptsBuilder, Row, Value};
use quarry_core::catalog::Catalog;
use quarry_core::result::{empty_schema, native_type};
use quarry_core::{BatchSink, Canceller, Driver, Flavor};

use crate::catalog::{ColumnKey, read_catalog};
use crate::convert::{ColumnMeta, RawColumn, build_array};
use crate::flavor::{detect_flavor, kill_query_sql, names_columns_from_catalog};
use crate::options::{ConnectOptions, SslMode};

/// Rows in the first batch, so the grid fills quickly.
const FIRST_BATCH_ROWS: usize = 100;
/// The first batch also goes out once this much time has passed.
const FIRST_BATCH_WAIT: Duration = Duration::from_millis(50);
/// Rows in each later batch.
const BATCH_ROWS: usize = 4096;
/// A slow stream still sends what it has this often.
const BATCH_WAIT: Duration = Duration::from_secs(1);
/// How long to read off the rest of a result the sink stopped before
/// killing the statement instead.
pub const DRAIN_BUDGET: Duration = Duration::from_millis(250);
/// Connect timeout for the side connection that sends `KILL QUERY`.
const KILL_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub struct MySqlDriver {
    conn: Conn,
    options: ConnectOptions,
    flavor: Flavor,
    killer: Killer,
    /// Engine type names for columns the wire mislabels, from the last
    /// catalog read.
    type_names: HashMap<ColumnKey, String>,
    /// The connection dropped; reconnect before the next statement.
    lost: bool,
    /// The database last chosen with `USE`, restored on a reconnect.
    database: Option<String>,
}

impl MySqlDriver {
    /// `mysql://` or `mariadb://` URLs (user, password, host, port, database, query options).
    ///
    /// Besides the `mysql` crate's own query options, the URL takes
    /// `ssl-mode` (`disabled`, `preferred` (default), `required`,
    /// `verify_ca`, `verify_identity`), `ssl-ca` (a PEM file) and
    /// `connect_timeout` (seconds, default 10).
    pub fn connect(url: &str) -> anyhow::Result<Self> {
        ensure_crypto_provider();
        let mut options = ConnectOptions::parse(url)?;
        let mut conn = open(&mut options)?;
        let version: Option<String> = conn.query_first("SELECT VERSION()").map_err(message)?;
        let flavor = detect_flavor(version.as_deref().unwrap_or_default());
        let killer = Killer {
            opts: kill_opts(&options.opts),
            connection_id: Arc::new(AtomicU32::new(conn.connection_id())),
        };
        log::debug!("connected to {} ({flavor})", options.target);
        Ok(Self {
            conn,
            options,
            flavor,
            killer,
            type_names: HashMap::new(),
            lost: false,
            database: None,
        })
    }

    fn reconnect_if_lost(&mut self) -> anyhow::Result<()> {
        if self.lost {
            log::info!("reconnecting to {}", self.options.target);
            let mut conn = open(&mut self.options)?;
            if let Some(database) = &self.database {
                // Best effort: the database may have been dropped since.
                if let Err(err) = conn.select_db(database) {
                    log::warn!("could not return to database {database}: {err}");
                }
            }
            self.conn = conn;
            self.killer
                .connection_id
                .store(self.conn.connection_id(), Ordering::Release);
            self.lost = false;
        }
        Ok(())
    }

    fn note_failure(&mut self, err: &mysql::Error) {
        if is_connection_lost(err) {
            self.lost = true;
        }
    }

    fn run(&mut self, sql: &str, sink: &mut dyn BatchSink) -> mysql::Result<()> {
        let type_names = names_columns_from_catalog(&self.flavor).then_some(&self.type_names);
        let mut killed = false;
        let mut affected: u64 = 0;
        let mut streamed = false;
        {
            let mut result = self.conn.query_iter(sql)?;
            loop {
                let columns: Vec<ColumnMeta> = result
                    .columns()
                    .as_ref()
                    .iter()
                    .map(|column| column_meta(column, type_names))
                    .collect();
                let Some(mut set) = result.iter() else {
                    break;
                };
                if columns.is_empty() {
                    affected = affected.saturating_add(set.affected_rows());
                    for row in set.by_ref() {
                        row?;
                    }
                    continue;
                }
                streamed = true;
                if stream(&mut set, &columns, sink)?.is_break() {
                    killed = drain(&mut set, &self.killer);
                }
                break;
            }
            // Dropping `result` reads off any further result sets.
        }
        if killed {
            // The kill may have landed after the result ended, with the
            // connection idle. It does not carry over to the next statement
            // on the servers tested, but a no-op takes it if it ever does.
            let _ = self.conn.query_drop("DO 0");
        }
        let keyword = first_keyword(sql);
        if keyword == "USE" {
            self.database = self.conn.query_first("SELECT DATABASE()")?;
        }
        if !streamed {
            if reports_row_count(&keyword) {
                sink.schema(rows_affected_schema());
                let batch = RecordBatch::try_new(
                    rows_affected_schema(),
                    vec![Arc::new(Int64Array::from(vec![
                        i64::try_from(affected).unwrap_or(i64::MAX),
                    ])) as ArrayRef],
                )
                .expect("rows affected batch matches its schema");
                let _ = sink.batch(batch);
            } else {
                sink.schema(empty_schema());
            }
        }
        Ok(())
    }
}

impl Driver for MySqlDriver {
    fn flavor(&self) -> Flavor {
        self.flavor.clone()
    }

    fn target(&self) -> String {
        self.options.target.clone()
    }

    fn execute(&mut self, sql: &str, sink: &mut dyn BatchSink) -> anyhow::Result<()> {
        self.reconnect_if_lost()?;
        self.run(sql, sink).map_err(|err| {
            self.note_failure(&err);
            let err = message(err);
            if self.lost {
                err.context("the connection was lost; the next statement reconnects")
            } else {
                err
            }
        })
    }

    fn catalog(&mut self) -> anyhow::Result<Catalog> {
        self.reconnect_if_lost()?;
        match read_catalog(&mut self.conn, &self.flavor) {
            Ok(read) => {
                self.type_names = read.type_names;
                Ok(read.catalog)
            }
            Err(err) => {
                self.note_failure(&err);
                Err(message(err))
            }
        }
    }

    fn canceller(&self) -> Canceller {
        let killer = self.killer.clone();
        // Called from the UI thread: never block it on a network round trip.
        Arc::new(move || {
            let killer = killer.clone();
            let spawned = thread::Builder::new()
                .name("quarry-mysql-kill".into())
                .spawn(move || killer.kill());
            if let Err(err) = spawned {
                log::warn!("could not start the cancel thread: {err}");
            }
        })
    }
}

/// Stops the driver connection's running statement from a side connection.
#[derive(Clone)]
struct Killer {
    opts: Opts,
    connection_id: Arc<AtomicU32>,
}

impl Killer {
    /// `KILL QUERY` the driver's connection. Blocks until the server has
    /// taken the kill; failures are logged, as there is nobody to tell.
    fn kill(&self) {
        let id = self.connection_id.load(Ordering::Acquire);
        let outcome =
            Conn::new(self.opts.clone()).and_then(|mut conn| conn.query_drop(kill_query_sql(id)));
        match outcome {
            Ok(()) => log::debug!("killed the query on connection {id}"),
            // 1094: unknown thread id, the statement already ended.
            Err(mysql::Error::MySqlError(err)) if err.code == 1094 => {}
            Err(err) => log::warn!("could not cancel the query on connection {id}: {err}"),
        }
    }
}

/// Options for the kill connection: the driver's, without a default
/// database (it may have been dropped since) and with a short timeout.
fn kill_opts(opts: &Opts) -> Opts {
    OptsBuilder::from_opts(opts.clone())
        .db_name(None::<String>)
        .tcp_connect_timeout(Some(KILL_CONNECT_TIMEOUT))
        .into()
}

/// Opens a connection, settling `ssl-mode=preferred` on the first try: if
/// the server has no TLS (or none rustls accepts), the options switch to
/// plain TCP for this connection and any reconnects.
fn open(options: &mut ConnectOptions) -> anyhow::Result<Conn> {
    match Conn::new(options.opts.clone()) {
        Ok(conn) => Ok(conn),
        Err(err)
            if options.ssl_mode == SslMode::Preferred
                && matches!(
                    err,
                    mysql::Error::DriverError(DriverError::TlsNotSupported)
                        | mysql::Error::TlsError(_)
                ) =>
        {
            log::debug!(
                "{}: no usable TLS ({err}), connecting without",
                options.target
            );
            options.opts = options.without_tls();
            options.ssl_mode = SslMode::Disabled;
            Conn::new(options.opts.clone()).map_err(message)
        }
        Err(err) => Err(message(err)),
    }
}

/// rustls needs one process-wide crypto provider. With only `ring`
/// compiled in it picks that itself, but if another crate in the app also
/// enables `aws-lc-rs`, rustls refuses to choose; install `ring` then.
fn ensure_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

fn column_meta(
    column: &mysql::Column,
    type_names: Option<&HashMap<ColumnKey, String>>,
) -> ColumnMeta {
    let mut meta = ColumnMeta::from_column(column);
    if let Some(type_names) = type_names {
        let key = (
            column.schema_str().into_owned(),
            column.org_table_str().into_owned(),
            column.org_name_str().into_owned(),
        );
        meta.type_override = type_names.get(&key).cloned();
    }
    meta
}

/// Stream the current result set to `sink`, returning `Break` if the sink
/// stopped before the rows ran out.
fn stream<I>(
    rows: &mut I,
    columns: &[ColumnMeta],
    sink: &mut dyn BatchSink,
) -> mysql::Result<ControlFlow<()>>
where
    I: Iterator<Item = mysql::Result<Row>>,
{
    let schema: SchemaRef = Arc::new(Schema::new(
        columns.iter().map(ColumnMeta::field).collect::<Vec<_>>(),
    ));
    sink.schema(schema.clone());

    let mut batch = Collector::new(columns.len());
    let started = Instant::now();
    let mut last_flush = started;
    let mut first = true;
    for row in rows.by_ref() {
        batch.push(row?);
        let due = if first {
            batch.rows >= FIRST_BATCH_ROWS || started.elapsed() >= FIRST_BATCH_WAIT
        } else {
            batch.rows >= BATCH_ROWS || last_flush.elapsed() >= BATCH_WAIT
        };
        if due {
            first = false;
            last_flush = Instant::now();
            if sink.batch(batch.take(&schema, columns)).is_break() {
                return Ok(ControlFlow::Break(()));
            }
        }
    }
    if batch.rows > 0 {
        // The rows ran out, so a `Break` here leaves nothing to stop.
        let _ = sink.batch(batch.take(&schema, columns));
    }
    Ok(ControlFlow::Continue(()))
}

/// Read off the rest of a result set the sink no longer wants. Returns
/// whether it had to be killed.
fn drain<I>(rows: &mut I, killer: &Killer) -> bool
where
    I: Iterator<Item = mysql::Result<Row>>,
{
    let (done, finished) = mpsc::channel::<()>();
    let watchdog = {
        let killer = killer.clone();
        thread::Builder::new()
            .name("quarry-mysql-drain".into())
            .spawn(move || match finished.recv_timeout(DRAIN_BUDGET) {
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    killer.kill();
                    true
                }
                _ => false,
            })
    };
    let watchdog = match watchdog {
        Ok(watchdog) => Some(watchdog),
        Err(err) => {
            log::warn!("could not start the drain watchdog: {err}");
            killer.kill();
            None
        }
    };
    for row in rows.by_ref() {
        // A killed statement ends with "Query execution was interrupted".
        if row.is_err() {
            break;
        }
    }
    drop(done);
    watchdog.is_none_or(|watchdog| watchdog.join().unwrap_or(true))
}

/// Rows gathered for the next batch, column by column, as the server sent
/// them.
struct Collector {
    columns: Vec<RawColumn>,
    rows: usize,
}

impl Collector {
    fn new(width: usize) -> Self {
        Self {
            columns: vec![Vec::new(); width],
            rows: 0,
        }
    }

    fn push(&mut self, row: Row) {
        for (column, value) in self.columns.iter_mut().zip(row.unwrap_raw()) {
            column.push(match value {
                None | Some(Value::NULL) => None,
                Some(Value::Bytes(bytes)) => Some(bytes),
                // The text protocol only sends bytes; keep anything else
                // readable all the same.
                Some(other) => Some(other.as_sql(true).into_bytes()),
            });
        }
        self.rows += 1;
    }

    fn take(&mut self, schema: &SchemaRef, columns: &[ColumnMeta]) -> RecordBatch {
        let arrays: Vec<ArrayRef> = self
            .columns
            .iter_mut()
            .zip(columns)
            .map(|(values, meta)| {
                let array = build_array(meta.kind(), values);
                values.clear();
                array
            })
            .collect();
        self.rows = 0;
        // A column that fell back to text (a zero date) needs a schema of
        // its own for this batch.
        let fell_back = arrays
            .iter()
            .zip(schema.fields())
            .any(|(array, field)| array.data_type() != field.data_type());
        let schema = if fell_back {
            Arc::new(Schema::new(
                arrays
                    .iter()
                    .zip(schema.fields())
                    .map(|(array, field)| {
                        Field::new(field.name(), array.data_type().clone(), true)
                            .with_metadata(field.metadata().clone())
                    })
                    .collect::<Vec<_>>(),
            ))
        } else {
            schema.clone()
        };
        RecordBatch::try_new(schema, arrays).expect("arrays match their schema")
    }
}

fn rows_affected_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![native_type(
        Field::new("rows affected", arrow::datatypes::DataType::Int64, false),
        "bigint",
    )]))
}

/// Whether a statement starting with `keyword` that returned no rows
/// should report how many it changed: `INSERT`, `UPDATE`, `DELETE`,
/// `REPLACE` and `LOAD DATA`, and `WITH ...` (which, without a result set,
/// was an `UPDATE` or `DELETE`).
fn reports_row_count(keyword: &str) -> bool {
    matches!(
        keyword,
        "INSERT" | "UPDATE" | "DELETE" | "REPLACE" | "LOAD" | "WITH"
    )
}

/// The statement's first word, upper case, after comments and parentheses.
fn first_keyword(sql: &str) -> String {
    let mut rest = sql;
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '(');
        if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, tail)| tail);
        } else if rest.starts_with('#')
            || (rest.starts_with("--") && rest[2..].starts_with(char::is_whitespace))
            || rest == "--"
        {
            rest = rest.split_once('\n').map_or("", |(_, tail)| tail);
        } else {
            break;
        }
    }
    rest.chars()
        .take_while(char::is_ascii_alphabetic)
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Whether an error means the connection itself is gone.
fn is_connection_lost(err: &mysql::Error) -> bool {
    match err {
        mysql::Error::IoError(_) | mysql::Error::CodecError(_) | mysql::Error::TlsError(_) => true,
        mysql::Error::DriverError(err) => matches!(
            err,
            DriverError::PacketOutOfSync
                | DriverError::UnexpectedPacket
                | DriverError::ConnectTimeout
                | DriverError::CouldNotConnect(_)
        ),
        // 1927: connection killed; 4031: disconnected for inactivity
        // (MySQL 8.0.24+).
        mysql::Error::MySqlError(err) => matches!(err.code, 1927 | 4031),
        _ => false,
    }
}

/// The server's message as the mysql client prints it,
/// `ERROR 1146 (42S02): Table 'shop.nope' doesn't exist`, and plain words
/// for client-side failures instead of the crate's debug-style wrappers.
fn message(err: mysql::Error) -> anyhow::Error {
    match err {
        mysql::Error::MySqlError(err) => {
            anyhow::anyhow!("ERROR {} ({}): {}", err.code, err.state, err.message)
        }
        mysql::Error::IoError(err) => anyhow::anyhow!("connection error: {err}"),
        mysql::Error::CodecError(err) => anyhow::anyhow!("protocol error: {err}"),
        mysql::Error::DriverError(DriverError::TlsNotSupported) => anyhow::anyhow!(
            "the server does not support TLS; connect with ssl-mode=preferred or disabled"
        ),
        mysql::Error::DriverError(err) => anyhow::anyhow!("{err}"),
        mysql::Error::UrlError(err) => anyhow::anyhow!("invalid URL: {err}"),
        mysql::Error::TlsError(err) => anyhow::anyhow!("TLS error: {err}"),
        other => anyhow::anyhow!("{other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_first_keyword() {
        assert_eq!(first_keyword("  select 1"), "SELECT");
        assert_eq!(first_keyword("/* hi */ INSERT INTO t VALUES (1)"), "INSERT");
        assert_eq!(first_keyword("-- note\nupdate t set a = 1"), "UPDATE");
        assert_eq!(first_keyword("# note\n\tdelete from t"), "DELETE");
        assert_eq!(first_keyword("(SELECT 1) UNION (SELECT 2)"), "SELECT");
        assert_eq!(first_keyword("--1\nselect"), "");
        assert_eq!(first_keyword(""), "");
    }

    #[test]
    fn reports_row_counts_for_dml_only() {
        let reports = |sql| reports_row_count(&first_keyword(sql));
        assert!(reports("insert into t values (1)"));
        assert!(reports("REPLACE INTO t VALUES (1)"));
        assert!(reports("LOAD DATA INFILE 'x' INTO TABLE t"));
        assert!(reports("WITH x AS (SELECT 1) UPDATE t SET a = 1"));
        assert!(!reports("CREATE TABLE t (a INT)"));
        assert!(!reports("SET @a = 1"));
        assert!(!reports("CALL p()"));
    }

    #[test]
    fn formats_server_errors_like_the_mysql_client() {
        let err = mysql::Error::MySqlError(mysql::MySqlError {
            state: "42S02".into(),
            message: "Table 'shop.nope' doesn't exist".into(),
            code: 1146,
        });
        assert_eq!(
            message(err).to_string(),
            "ERROR 1146 (42S02): Table 'shop.nope' doesn't exist"
        );
        let io = mysql::Error::IoError(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "refused",
        ));
        assert!(is_connection_lost(&io));
        assert_eq!(message(io).to_string(), "connection error: refused");
    }
}
