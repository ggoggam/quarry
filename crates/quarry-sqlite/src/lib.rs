//! SQLite driver for Quarry.
//!
//! SQLite is dynamically typed: a column's declared type is a hint, and any
//! row may hold an integer, float, text or blob. Rows are stepped one at a
//! time and gathered into Arrow batches whose column types come from the
//! values in each batch (see [`batch`]); the result's schema takes the
//! declared types where the columns have them.
//!
//! The connection keeps SQLite's defaults: in particular `foreign_keys` stays
//! off, as it is for every SQLite client unless the user turns it on with
//! `PRAGMA foreign_keys = ON`, because enabling it changes what statements
//! do. rusqlite's default 5 second busy timeout applies to locked databases.

mod batch;
mod catalog;
pub mod features;

use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use arrow::array::Int64Array;
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use quarry_core::catalog::Catalog;
use quarry_core::{BatchSink, Canceller, Dialect, Driver, Flavor};
use rusqlite::{Connection, InterruptHandle, OpenFlags};

use crate::batch::{BatchBuilder, Cell};
pub use crate::features::Features;

/// Rows in the first batch, kept small so the grid fills at once.
const FIRST_BATCH_ROWS: usize = 100;
/// Rows in each later batch.
const BATCH_ROWS: usize = 4096;
/// A slow statement still shows what it has after this long.
const BATCH_MAX_WAIT: Duration = Duration::from_millis(250);

/// The bytes every SQLite 3 database file starts with.
const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";

/// Where a SQLite connection's data lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    InMemory,
    /// A database file, created when it does not exist yet unless opened
    /// read-only.
    File {
        path: PathBuf,
        read_only: bool,
    },
}

pub struct SqliteDriver {
    conn: Connection,
    target: Target,
    interrupt: Arc<InterruptHandle>,
    flavor: Flavor,
    features: Features,
}

impl SqliteDriver {
    pub fn open(target: Target) -> anyhow::Result<Self> {
        let conn = match &target {
            Target::InMemory => Connection::open_in_memory().map_err(message)?,
            Target::File { path, read_only } => {
                let access = if *read_only {
                    OpenFlags::SQLITE_OPEN_READ_ONLY
                } else {
                    OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
                };
                let flags = access | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
                let conn = Connection::open_with_flags(path, flags)
                    .map_err(message)
                    .with_context(|| format!("opening {}", path.display()))?;
                // SQLite reads the file lazily, so a file that is not a
                // database would only fail at the first query. Fail here.
                conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))
                    .map_err(message)
                    .with_context(|| format!("opening {}", path.display()))?;
                conn
            }
        };
        let interrupt = Arc::new(conn.get_interrupt_handle());
        let flavor = Flavor::new(Dialect::Sqlite, rusqlite::version());
        let features = Features::of(&flavor);
        Ok(Self {
            conn,
            target,
            interrupt,
            flavor,
            features,
        })
    }

    pub fn open_path(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        Self::open(Target::File {
            path: path.as_ref().to_owned(),
            read_only: false,
        })
    }

    pub fn in_memory() -> anyhow::Result<Self> {
        Self::open(Target::InMemory)
    }

    /// What the connected engine release supports.
    pub fn features(&self) -> Features {
        self.features
    }
}

/// Whether the file starts with the SQLite header ("SQLite format 3\0"), so
/// the app can pick the driver on open.
pub fn is_sqlite_file(path: &Path) -> bool {
    let mut header = [0u8; 16];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut header))
        .is_ok_and(|()| &header == SQLITE_HEADER)
}

impl Driver for SqliteDriver {
    fn flavor(&self) -> Flavor {
        self.flavor.clone()
    }

    fn target(&self) -> String {
        match &self.target {
            Target::InMemory => ":memory:".into(),
            Target::File { path, .. } => path.display().to_string(),
        }
    }

    fn execute(&mut self, sql: &str, sink: &mut dyn BatchSink) -> anyhow::Result<()> {
        let kind = statement_kind(sql);
        if kind == StatementKind::Empty {
            // Only comments: SQLite prepares nothing, and nothing runs.
            sink.schema(quarry_core::result::empty_schema());
            return Ok(());
        }
        let mut stmt = self.conn.prepare(sql).map_err(message)?;
        if stmt.column_count() == 0 {
            stmt.raw_execute().map_err(message)?;
            if kind == StatementKind::Dml {
                report_changes(sink, self.conn.changes());
            } else {
                sink.schema(quarry_core::result::empty_schema());
            }
            return Ok(());
        }

        let names: Vec<String> = stmt.column_names().map(String::from).collect();
        let decls: Vec<Option<String>> = stmt
            .columns()
            .map(|c| c.decl_type().map(String::from))
            .collect();
        let width = names.len();
        let mut builder = BatchBuilder::new(width);
        let mut schema = None;
        let mut limit = FIRST_BATCH_ROWS;
        let mut since = Instant::now();
        let mut rows = stmt.raw_query();
        loop {
            let row = rows.next().map_err(message)?;
            if let Some(row) = row {
                for col in 0..width {
                    builder.push(col, Cell::from(row.get_ref(col).map_err(message)?));
                }
                builder.end_row();
                if builder.len() < limit && since.elapsed() < BATCH_MAX_WAIT {
                    continue;
                }
            } else if builder.is_empty() {
                break;
            }
            let done = row.is_none();
            let arrays = builder.take_arrays();
            let schema = schema.get_or_insert_with(|| {
                let schema = batch::result_schema(&names, &decls, Some(&arrays));
                sink.schema(schema.clone());
                schema
            });
            let batch = batch::make_batch(schema, arrays)?;
            // Dropping `rows` resets the statement, which ends it early.
            if sink.batch(batch).is_break() || done {
                return Ok(());
            }
            limit = BATCH_ROWS;
            since = Instant::now();
        }
        if schema.is_none() {
            sink.schema(batch::result_schema(&names, &decls, None));
        }
        Ok(())
    }

    fn catalog(&mut self) -> anyhow::Result<Catalog> {
        catalog::read_catalog(&self.conn, &self.features).map_err(|err| {
            match err.downcast::<rusqlite::Error>() {
                Ok(err) => message(err),
                Err(err) => err,
            }
        })
    }

    fn canceller(&self) -> Canceller {
        let interrupt = self.interrupt.clone();
        Arc::new(move || interrupt.interrupt())
    }
}

/// A one-row `rows affected` result for INSERT, UPDATE and DELETE.
fn report_changes(sink: &mut dyn BatchSink, changes: u64) {
    let schema = Arc::new(Schema::new(vec![Field::new(
        "rows affected",
        DataType::Int64,
        false,
    )]));
    sink.schema(schema.clone());
    let count = i64::try_from(changes).unwrap_or(i64::MAX);
    let batch = RecordBatch::try_new(schema, vec![Arc::new(Int64Array::from(vec![count]))])
        .expect("one Int64 column");
    let _ = sink.batch(batch);
}

/// SQLite's own message. rusqlite appends the whole statement to syntax
/// errors ("... in SELECT ... at offset 7"), which the editor already shows.
fn message(err: rusqlite::Error) -> anyhow::Error {
    match err {
        rusqlite::Error::SqlInputError { msg, .. } => anyhow::anyhow!(msg),
        other => anyhow::anyhow!(other.to_string()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatementKind {
    /// Nothing but whitespace and comments.
    Empty,
    /// INSERT, REPLACE, UPDATE or DELETE, possibly after a WITH clause.
    Dml,
    Other,
}

/// What kind of statement `sql` is, from its first keyword, or for a WITH
/// statement the first keyword after its common table expressions.
fn statement_kind(sql: &str) -> StatementKind {
    let mut words = TopLevelWords::new(sql);
    let Some(first) = words.next() else {
        return if words.blank {
            StatementKind::Empty
        } else {
            StatementKind::Other
        };
    };
    let verb = if first.eq_ignore_ascii_case("WITH") {
        words.find(|w| {
            ["SELECT", "VALUES", "INSERT", "REPLACE", "UPDATE", "DELETE"]
                .iter()
                .any(|k| w.eq_ignore_ascii_case(k))
        })
    } else {
        Some(first)
    };
    match verb {
        Some(v)
            if ["INSERT", "REPLACE", "UPDATE", "DELETE"]
                .iter()
                .any(|k| v.eq_ignore_ascii_case(k)) =>
        {
            StatementKind::Dml
        }
        _ => StatementKind::Other,
    }
}

/// The bare words of a statement outside parentheses, skipping comments,
/// string literals and quoted identifiers. Enough to find its verb.
struct TopLevelWords<'a> {
    sql: &'a str,
    pos: usize,
    depth: usize,
    /// Nothing but whitespace, comments and `;` so far.
    blank: bool,
}

impl<'a> TopLevelWords<'a> {
    fn new(sql: &'a str) -> Self {
        Self {
            sql,
            pos: 0,
            depth: 0,
            blank: true,
        }
    }

    /// Move past the end of a quoted run that started just before `pos`.
    fn skip_quoted(&mut self, close: u8) {
        let bytes = self.sql.as_bytes();
        while self.pos < bytes.len() {
            let b = bytes[self.pos];
            self.pos += 1;
            if b == close {
                // A doubled quote is an escaped one, except for `]`.
                if close != b']' && bytes.get(self.pos) == Some(&close) {
                    self.pos += 1;
                } else {
                    return;
                }
            }
        }
    }
}

impl<'a> Iterator for TopLevelWords<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        let bytes = self.sql.as_bytes();
        while self.pos < bytes.len() {
            let b = bytes[self.pos];
            let next = bytes.get(self.pos + 1).copied();
            match b {
                b'-' if next == Some(b'-') => {
                    self.pos = self.sql[self.pos..]
                        .find('\n')
                        .map_or(bytes.len(), |n| self.pos + n + 1);
                }
                b'/' if next == Some(b'*') => {
                    self.pos = self.sql[self.pos + 2..]
                        .find("*/")
                        .map_or(bytes.len(), |n| self.pos + 2 + n + 2);
                }
                b if b.is_ascii_whitespace() || b == b';' => self.pos += 1,
                b'\'' | b'"' | b'`' | b'[' => {
                    self.blank = false;
                    self.pos += 1;
                    self.skip_quoted(if b == b'[' { b']' } else { b });
                }
                b'(' => {
                    self.blank = false;
                    self.depth += 1;
                    self.pos += 1;
                }
                b')' => {
                    self.blank = false;
                    self.depth = self.depth.saturating_sub(1);
                    self.pos += 1;
                }
                b if b.is_ascii_alphabetic() || b == b'_' => {
                    self.blank = false;
                    let start = self.pos;
                    while self.pos < bytes.len()
                        && (bytes[self.pos].is_ascii_alphanumeric() || bytes[self.pos] == b'_')
                    {
                        self.pos += 1;
                    }
                    if self.depth == 0 {
                        return Some(&self.sql[start..self.pos]);
                    }
                }
                _ => {
                    self.blank = false;
                    self.pos += 1;
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use quarry_core::catalog::{ColumnDef, RelationKind};
    use quarry_core::{QueryEvent, QueryOptions, ResultSet, Session};

    use super::*;

    fn collect(
        session: &Session,
        sql: &str,
        options: QueryOptions,
    ) -> (Option<ResultSet>, QueryEvent) {
        let query = session.query(sql, options);
        let mut result = None;
        loop {
            match query
                .events
                .recv_timeout(Duration::from_secs(30))
                .expect("event")
            {
                QueryEvent::Schema(schema) => result = Some(ResultSet::new(schema)),
                QueryEvent::Batch(batch) => result.as_mut().expect("schema first").push(batch),
                done => return (result, done),
            }
        }
    }

    fn run(session: &Session, sql: &str) -> ResultSet {
        let (result, done) = collect(session, sql, QueryOptions::default());
        assert!(matches!(done, QueryEvent::Finished(_)), "{sql}: {done:?}");
        result.expect("a schema")
    }

    fn failure(session: &Session, sql: &str) -> String {
        let (_, done) = collect(session, sql, QueryOptions::default());
        let QueryEvent::Failed { message, .. } = done else {
            panic!("{sql}: {done:?}")
        };
        message
    }

    const COUNT_TO_100K: &str = "WITH RECURSIVE c(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM c WHERE i < 99999) \
         SELECT i, 'row ' || i AS label, CASE WHEN i % 2 = 0 THEN NULL ELSE i * 1.5 END AS half FROM c";

    #[test]
    fn reports_its_version() {
        let driver = SqliteDriver::in_memory().unwrap();
        let flavor = driver.flavor();
        assert_eq!(flavor.dialect, Dialect::Sqlite);
        assert_eq!(flavor.version_text, rusqlite::version());
        assert!(flavor.at_least(3, 54), "{flavor:?}");
        assert!(driver.features().table_list);
        assert_eq!(driver.target(), ":memory:");
    }

    #[test]
    fn streams_many_rows_across_batches() {
        let session = Session::spawn(SqliteDriver::in_memory().unwrap());
        let (result, done) = collect(&session, COUNT_TO_100K, QueryOptions::default());
        let QueryEvent::Finished(summary) = done else {
            panic!("{done:?}")
        };
        let result = result.unwrap();
        assert_eq!(summary.rows, 100_000);
        assert!(!summary.truncated);
        assert_eq!(result.row_count(), 100_000);
        assert!(result.batches()[0].num_rows() <= FIRST_BATCH_ROWS);
        assert!(result.batches().len() > 20, "{}", result.batches().len());
        assert_eq!(result.column_name(1), "label");
        assert_eq!(result.cell_text(54_321, 0).as_deref(), Some("54321"));
        assert_eq!(result.cell_text(54_321, 1).as_deref(), Some("row 54321"));
        assert!(result.is_null(54_320, 2));
        assert_eq!(result.cell_text(54_321, 2).as_deref(), Some("81481.5"));
    }

    #[test]
    fn stops_at_the_row_limit() {
        let session = Session::spawn(SqliteDriver::in_memory().unwrap());
        let (result, done) = collect(&session, COUNT_TO_100K, QueryOptions { max_rows: 3000 });
        let QueryEvent::Finished(summary) = done else {
            panic!("{done:?}")
        };
        assert!(summary.truncated);
        assert_eq!(summary.rows, 3000);
        assert_eq!(result.unwrap().row_count(), 3000);
    }

    #[test]
    fn types_each_batch_by_its_values() {
        let session = Session::spawn(SqliteDriver::in_memory().unwrap());
        // Integers for the first batch's 100 rows, then text from row 150.
        let result = run(
            &session,
            "WITH RECURSIVE c(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM c WHERE i < 299) \
             SELECT CASE WHEN i < 150 THEN i ELSE 'v' || i END AS v, \
                    CASE WHEN i < 50 THEN i ELSE i + 0.5 END AS n \
               FROM c",
        );
        assert_eq!(result.row_count(), 300);
        assert_eq!(result.column_type(0), &DataType::Int64);
        let first = &result.batches()[0];
        assert_eq!(first.column(0).data_type(), &DataType::Int64);
        assert_eq!(first.column(1).data_type(), &DataType::Float64);
        let second = &result.batches()[1];
        assert_eq!(second.column(0).data_type(), &DataType::Utf8);
        assert_eq!(result.cell_text(7, 0).as_deref(), Some("7"));
        assert_eq!(result.cell_text(120, 0).as_deref(), Some("120"));
        assert_eq!(result.cell_text(200, 0).as_deref(), Some("v200"));
        assert_eq!(result.cell_text(7, 1).as_deref(), Some("7.0"));
        assert_eq!(result.cell_text(70, 1).as_deref(), Some("70.5"));
    }

    #[test]
    fn names_declared_types() {
        let session = Session::spawn(SqliteDriver::in_memory().unwrap());
        run(
            &session,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name VARCHAR(20), price DECIMAL(10,2), \
             data BLOB, at DATETIME, loose)",
        );
        run(
            &session,
            "INSERT INTO t VALUES (1, 'a', 9.5, x'cafe', '2026-11-10', 5), \
                                  (2, 'b', 3, NULL, NULL, 'five')",
        );
        let result = run(&session, "SELECT *, id + 1 AS next FROM t ORDER BY id");
        let labels: Vec<_> = (0..result.column_count())
            .map(|c| result.column_type_label(c))
            .collect();
        assert_eq!(
            labels,
            [
                "INTEGER",
                "VARCHAR(20)",
                "DECIMAL(10,2)",
                "BLOB",
                "DATETIME",
                "varchar",
                "bigint"
            ]
        );
        assert_eq!(result.column_type(0), &DataType::Int64);
        assert_eq!(result.column_type(1), &DataType::Utf8);
        assert_eq!(result.column_type(3), &DataType::Binary);
        // NUMERIC affinity: typed from the first batch's values.
        assert_eq!(result.column_type(2), &DataType::Float64);
        assert_eq!(result.column_type(4), &DataType::Utf8);
        assert_eq!(result.cell_text(1, 2).as_deref(), Some("3.0"));
        assert_eq!(result.cell_text(0, 3).as_deref(), Some("cafe"));
        assert!(result.is_null(1, 3));
        assert_eq!(result.cell_text(0, 5).as_deref(), Some("5"));
        assert_eq!(result.cell_text(1, 5).as_deref(), Some("five"));
    }

    #[test]
    fn shows_blobs_and_nulls() {
        let session = Session::spawn(SqliteDriver::in_memory().unwrap());
        let result = run(
            &session,
            "SELECT x'00ff10' AS b, NULL AS n UNION ALL SELECT 'text', NULL",
        );
        assert_eq!(result.row_count(), 2);
        // Mixed with text, the blob is written as a blob literal.
        assert_eq!(result.cell_text(0, 0).as_deref(), Some("x'00ff10'"));
        assert_eq!(result.cell_text(1, 0).as_deref(), Some("text"));
        assert_eq!(result.cell_text(0, 1), None);
        assert!(result.is_null(1, 1));
        assert_eq!(result.column_type(1), &DataType::Utf8);

        let result = run(&session, "SELECT x'cafe' AS b");
        assert_eq!(result.column_type(0), &DataType::Binary);
        assert_eq!(result.cell_text(0, 0).as_deref(), Some("cafe"));

        let result = run(&session, "SELECT 1 AS a WHERE 0");
        assert_eq!(result.row_count(), 0);
        assert_eq!(result.column_count(), 1);
    }

    #[test]
    fn runs_ddl_and_dml() {
        let session = Session::spawn(SqliteDriver::in_memory().unwrap());
        let created = run(
            &session,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
        );
        assert_eq!(created.column_count(), 0);

        let inserted = run(&session, "INSERT INTO t (name) VALUES ('a'), ('b'), ('c')");
        assert_eq!(inserted.column_name(0), "rows affected");
        assert_eq!(inserted.cell_text(0, 0).as_deref(), Some("3"));

        let updated = run(
            &session,
            "WITH doomed AS (SELECT id FROM t WHERE name > 'a') \
             UPDATE t SET name = upper(name) WHERE id IN doomed",
        );
        assert_eq!(updated.cell_text(0, 0).as_deref(), Some("2"));
        let deleted = run(&session, "DELETE FROM t WHERE 0");
        assert_eq!(deleted.cell_text(0, 0).as_deref(), Some("0"));

        let returned = run(
            &session,
            "INSERT INTO t (name) VALUES ('d'), ('e') RETURNING id, name",
        );
        assert_eq!(returned.row_count(), 2);
        assert_eq!(returned.column_name(0), "id");
        assert_eq!(returned.cell_text(1, 0).as_deref(), Some("5"));
        assert_eq!(returned.cell_text(1, 1).as_deref(), Some("e"));

        let comment = run(&session, "-- nothing to run\n/* at all */");
        assert_eq!(comment.column_count(), 0);
        let pragma = run(&session, "PRAGMA user_version = 7");
        assert_eq!(pragma.column_count(), 0);
        let read = run(&session, "PRAGMA user_version");
        assert_eq!(read.cell_text(0, 0).as_deref(), Some("7"));
        let trailing = run(&session, "SELECT count(*) FROM t; -- done");
        assert_eq!(trailing.cell_text(0, 0).as_deref(), Some("5"));
    }

    #[test]
    fn reports_errors_plainly_and_keeps_working() {
        let session = Session::spawn(SqliteDriver::in_memory().unwrap());
        run(&session, "CREATE TABLE t (a INTEGER NOT NULL)");

        let message = failure(&session, "SELECT nope FROM t");
        assert_eq!(message, "no such column: nope");
        let message = failure(&session, "SELEC 1");
        assert_eq!(message, r#"near "SELEC": syntax error"#);
        let message = failure(&session, "INSERT INTO t VALUES (NULL)");
        assert_eq!(message, "NOT NULL constraint failed: t.a");
        let message = failure(&session, "SELECT 1; SELECT 2");
        assert!(message.contains("Multiple statements"), "{message}");

        let result = run(&session, "SELECT count(*) FROM t");
        assert_eq!(result.cell_text(0, 0).as_deref(), Some("0"));
    }

    #[test]
    fn cancels_a_long_query() {
        let session = Session::spawn(SqliteDriver::in_memory().unwrap());
        let query = session.query(
            "WITH RECURSIVE c(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM c) \
             SELECT count(*) FROM c",
            QueryOptions::default(),
        );
        std::thread::sleep(Duration::from_millis(300));
        session.cancel();
        let done = loop {
            match query
                .events
                .recv_timeout(Duration::from_secs(10))
                .expect("cancel took too long")
            {
                QueryEvent::Schema(_) | QueryEvent::Batch(_) => continue,
                done => break done,
            }
        };
        let QueryEvent::Finished(summary) = done else {
            panic!("{done:?}")
        };
        assert!(summary.cancelled);

        let result = run(&session, "SELECT 42");
        assert_eq!(result.cell_text(0, 0).as_deref(), Some("42"));
    }

    fn column(name: &str, data_type: &str, nullable: bool) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            data_type: data_type.into(),
            nullable,
        }
    }

    #[test]
    fn reads_the_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("shop.sqlite");
        let extra = dir.path().join("extra.sqlite");
        let empty = dir.path().join("empty.sqlite");
        let session = Session::spawn(SqliteDriver::open_path(&main).unwrap());
        for sql in [
            "CREATE TABLE orders (id INTEGER PRIMARY KEY, total REAL NOT NULL, note)",
            "CREATE INDEX orders_total ON orders (total)",
            "INSERT INTO orders (total) VALUES (1), (200), (300)",
            "CREATE VIEW big AS SELECT id, total FROM orders WHERE total > 100",
            "CREATE TABLE items (sku TEXT PRIMARY KEY, qty INT, price REAL, \
               cost REAL GENERATED ALWAYS AS (qty * price), \
               tag TEXT AS (upper(sku)) STORED) STRICT",
            "CREATE TABLE pairs (a TEXT, b TEXT, PRIMARY KEY (a, b)) WITHOUT ROWID",
            "CREATE VIRTUAL TABLE docs USING fts5(title, body)",
            "ANALYZE",
        ] {
            run(&session, sql);
        }
        run(&session, &format!("ATTACH '{}' AS extra", extra.display()));
        run(&session, "CREATE TABLE extra.t (x)");
        run(&session, &format!("ATTACH '{}' AS empty", empty.display()));

        let catalog = session.catalog().recv().unwrap().unwrap();
        let names: Vec<_> = catalog.databases.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["main", "extra", "empty"]);
        for db in &catalog.databases {
            assert_eq!(db.schemas.len(), 1);
            assert_eq!(db.schemas[0].name, "");
        }

        let main = &catalog.databases[0].schemas[0].relations;
        let relations: Vec<_> = main.iter().map(|r| (r.name.as_str(), r.kind)).collect();
        assert_eq!(
            relations,
            [
                ("big", RelationKind::View),
                ("docs", RelationKind::Table),
                ("items", RelationKind::Table),
                ("orders", RelationKind::Table),
                ("pairs", RelationKind::Table),
            ]
        );
        let get = |name: &str| main.iter().find(|r| r.name == name).unwrap();
        let orders = get("orders");
        assert_eq!(
            orders.columns,
            [
                column("id", "INTEGER", false),
                column("total", "REAL", false),
                column("note", "", true),
            ]
        );
        assert_eq!(orders.estimated_rows, Some(3));
        assert_eq!(get("big").estimated_rows, None);
        assert_eq!(get("big").columns.len(), 2);
        assert_eq!(
            get("items").columns,
            [
                column("sku", "TEXT", false),
                column("qty", "INT", true),
                column("price", "REAL", true),
                column("cost", "REAL GENERATED", true),
                column("tag", "TEXT GENERATED STORED", true),
            ]
        );
        assert_eq!(
            get("pairs").columns,
            [column("a", "TEXT", false), column("b", "TEXT", false)]
        );
        let docs: Vec<_> = get("docs")
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(docs, ["title", "body"]);

        let extra = &catalog.databases[1].schemas[0].relations;
        assert_eq!(extra.len(), 1);
        assert_eq!(extra[0].columns, [column("x", "", true)]);
        assert!(catalog.databases[2].schemas[0].relations.is_empty());

        // `temp` shows up once it holds something.
        run(&session, "CREATE TEMP TABLE scratch (a)");
        let catalog = session.catalog().recv().unwrap().unwrap();
        let names: Vec<_> = catalog.databases.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["main", "temp", "extra", "empty"]);
        assert_eq!(catalog.databases[1].schemas[0].relations[0].name, "scratch");
    }

    #[test]
    fn reads_the_catalog_the_old_way_too() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT NOT NULL, \
               b INT GENERATED ALWAYS AS (length(a))); \
             CREATE VIEW v AS SELECT a FROM t; \
             CREATE TABLE \"odd \"\"name\" (x)",
        )
        .unwrap();
        let at = |version| Features::of(&Flavor::new(Dialect::Sqlite, version));
        let new = catalog::read_catalog(&conn, &at("3.54.1")).unwrap();
        let old = catalog::read_catalog(&conn, &at("3.25.0")).unwrap();

        let relations = |catalog: &Catalog| {
            catalog.databases[0].schemas[0]
                .relations
                .iter()
                .map(|r| (r.name.clone(), r.kind, r.columns.clone()))
                .collect::<Vec<_>>()
        };
        let (new, old) = (relations(&new), relations(&old));
        let names: Vec<_> = old.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(names, ["odd \"name", "t", "v"]);
        assert_eq!(new[0], old[0]);
        assert_eq!(new[2], old[2]);
        // Only table_xinfo (3.26) lists the generated column.
        assert_eq!(
            new[1].2,
            [
                column("id", "INTEGER", false),
                column("a", "TEXT", false),
                column("b", "INT GENERATED", true),
            ]
        );
        assert_eq!(
            old[1].2,
            [column("id", "INTEGER", false), column("a", "TEXT", false)]
        );

        // Between the two: table_xinfo but not table_list.
        let mid = catalog::read_catalog(&conn, &at("3.30.0")).unwrap();
        assert_eq!(relations(&mid), new);
    }

    #[test]
    fn read_only_files_refuse_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ro.sqlite");
        {
            let session = Session::spawn(SqliteDriver::open_path(&path).unwrap());
            run(&session, "CREATE TABLE t (a)");
            run(&session, "INSERT INTO t VALUES (1)");
        }
        let driver = SqliteDriver::open(Target::File {
            path: path.clone(),
            read_only: true,
        })
        .unwrap();
        assert_eq!(driver.target(), path.display().to_string());
        let session = Session::spawn(driver);
        let result = run(&session, "SELECT a FROM t");
        assert_eq!(result.cell_text(0, 0).as_deref(), Some("1"));
        let message = failure(&session, "INSERT INTO t VALUES (2)");
        assert_eq!(message, "attempt to write a readonly database");

        let missing = SqliteDriver::open(Target::File {
            path: dir.path().join("missing.sqlite"),
            read_only: true,
        });
        let message = format!("{:#}", missing.err().expect("no such file"));
        assert!(message.contains("unable to open"), "{message}");
    }

    #[test]
    fn spots_sqlite_files() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("real.db");
        SqliteDriver::open_path(&db)
            .unwrap()
            .conn
            .execute_batch("CREATE TABLE t (a)")
            .unwrap();
        assert!(is_sqlite_file(&db));

        // DuckDB files carry their magic at offset 8.
        let duck = dir.path().join("fake.duckdb");
        let mut bytes = vec![0u8; 64];
        bytes[8..12].copy_from_slice(b"DUCK");
        std::fs::write(&duck, bytes).unwrap();
        assert!(!is_sqlite_file(&duck));

        let text = dir.path().join("notes.txt");
        std::fs::write(&text, "SQLite format 3 is a nice format\n").unwrap();
        assert!(!is_sqlite_file(&text));
        assert!(!is_sqlite_file(&dir.path().join("missing.db")));

        // And the driver refuses a file that is not a database.
        let err = SqliteDriver::open_path(&duck)
            .err()
            .expect("not a database");
        assert!(format!("{err:#}").contains("not a database"), "{err:#}");
    }

    #[test]
    fn finds_the_statement_verb() {
        use StatementKind::*;
        assert_eq!(statement_kind("  -- hi\n/* there */ "), Empty);
        assert_eq!(statement_kind(" ; -- x"), Empty);
        assert_eq!(statement_kind("1"), Other);
        assert_eq!(statement_kind("insert into t values (1)"), Dml);
        assert_eq!(statement_kind("REPLACE INTO t VALUES (1)"), Dml);
        assert_eq!(statement_kind("/* x */ DELETE FROM t"), Dml);
        assert_eq!(
            statement_kind("WITH RECURSIVE \"select\"(x) AS (SELECT 1) UPDATE t SET a = 1"),
            Dml
        );
        assert_eq!(
            statement_kind("WITH a AS MATERIALIZED (DELETE), b AS (SELECT 1) SELECT * FROM a"),
            Other
        );
        assert_eq!(statement_kind("CREATE TABLE t (a)"), Other);
        assert_eq!(statement_kind("SELECT 'insert'"), Other);
        assert_eq!(statement_kind("[insert]"), Other);
    }
}
