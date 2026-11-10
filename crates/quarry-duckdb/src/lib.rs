//! DuckDB driver for Quarry.
//!
//! Results come straight out of DuckDB as Arrow batches through
//! [`duckdb::Statement::stream_arrow`], which fetches chunks lazily: the first
//! rows reach the grid while the rest are still being produced, and stopping
//! at the row limit does not materialize the remainder.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use duckdb::{AccessMode, Config, Connection, InterruptHandle};
use quarry_core::catalog::{Catalog, ColumnDef, Database, Relation, RelationKind, Schema};
use quarry_core::{BatchSink, Canceller, Dialect, Driver, Flavor};

/// Where a DuckDB connection's data lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    InMemory,
    File { path: PathBuf, read_only: bool },
}

pub struct DuckDbDriver {
    conn: Connection,
    target: Target,
    interrupt: Arc<InterruptHandle>,
    flavor: Flavor,
}

impl DuckDbDriver {
    pub fn open(target: Target) -> anyhow::Result<Self> {
        let conn = match &target {
            Target::InMemory => Connection::open_in_memory()?,
            Target::File { path, read_only } => {
                let mode = if *read_only {
                    AccessMode::ReadOnly
                } else {
                    AccessMode::ReadWrite
                };
                let config = Config::default().access_mode(mode)?;
                Connection::open_with_flags(path, config)
                    .with_context(|| format!("opening {}", path.display()))?
            }
        };
        let interrupt = conn.interrupt_handle();
        let version: String =
            conn.query_row("SELECT library_version FROM pragma_version()", [], |row| {
                row.get(0)
            })?;
        let flavor = Flavor::new(Dialect::DuckDb, version.trim_start_matches('v'));
        Ok(Self {
            conn,
            target,
            interrupt,
            flavor,
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
}

impl Driver for DuckDbDriver {
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
        let mut stmt = self.conn.prepare(sql).map_err(message)?;
        let stream = stmt.stream_arrow([]).map_err(message)?;
        sink.schema(stream.get_schema());
        for batch in stream {
            if sink.batch(batch.map_err(message)?).is_break() {
                break;
            }
        }
        Ok(())
    }

    fn catalog(&mut self) -> anyhow::Result<Catalog> {
        read_catalog(&self.conn)
    }

    fn canceller(&self) -> Canceller {
        let interrupt = self.interrupt.clone();
        Arc::new(move || interrupt.interrupt())
    }
}

/// DuckDB's own message, without the FFI error duckdb-rs chains under it
/// ("Error code 1: Unknown error code"), which says nothing to a user.
fn message(err: duckdb::Error) -> anyhow::Error {
    anyhow::anyhow!(err.to_string())
}

type SchemaKey = (String, String);
type RelationKey = (String, String, String);

fn read_catalog(conn: &Connection) -> anyhow::Result<Catalog> {
    // Databases and schemas first, so empty ones still show up.
    let mut schemas: BTreeMap<String, BTreeMap<String, Vec<Relation>>> = BTreeMap::new();
    let mut stmt = conn.prepare(
        "SELECT database_name FROM duckdb_databases() WHERE NOT internal ORDER BY database_name",
    )?;
    for name in stmt.query_map([], |row| row.get::<_, String>(0))? {
        schemas.entry(name?).or_default();
    }
    let mut stmt = conn.prepare(
        // DuckDB marks every database's `main` schema internal, but it is
        // where user tables go by default.
        "SELECT s.database_name, s.schema_name FROM duckdb_schemas() s \
           JOIN duckdb_databases() d USING (database_name) \
          WHERE NOT d.internal AND (NOT s.internal OR s.schema_name = 'main') \
          ORDER BY s.database_name, s.schema_name",
    )?;
    for row in stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (database, schema) = row?;
        schemas
            .entry(database)
            .or_default()
            .entry(schema)
            .or_default();
    }

    let mut relations: BTreeMap<RelationKey, Relation> = BTreeMap::new();
    let mut stmt = conn.prepare(
        "SELECT database_name, schema_name, table_name, false AS is_view, estimated_size \
           FROM duckdb_tables() WHERE NOT internal \
         UNION ALL \
         SELECT database_name, schema_name, view_name, true, NULL \
           FROM duckdb_views() WHERE NOT internal",
    )?;
    for row in stmt.query_map([], |row| {
        Ok((
            (
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ),
            row.get::<_, bool>(3)?,
            row.get::<_, Option<i64>>(4)?,
        ))
    })? {
        let (key, is_view, estimated) = row?;
        relations.insert(
            key.clone(),
            Relation {
                name: key.2,
                kind: if is_view {
                    RelationKind::View
                } else {
                    RelationKind::Table
                },
                columns: Vec::new(),
                estimated_rows: estimated.and_then(|n| u64::try_from(n).ok()),
            },
        );
    }

    let mut stmt = conn.prepare(
        "SELECT database_name, schema_name, table_name, column_name, data_type, is_nullable \
           FROM duckdb_columns() WHERE NOT internal \
          ORDER BY database_name, schema_name, table_name, column_index",
    )?;
    for row in stmt.query_map([], |row| {
        Ok((
            (
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ),
            ColumnDef {
                name: row.get(3)?,
                data_type: row.get(4)?,
                nullable: row.get(5)?,
            },
        ))
    })? {
        let (key, column) = row?;
        if let Some(relation) = relations.get_mut(&key) {
            relation.columns.push(column);
        }
    }

    for ((database, schema, _), relation) in relations {
        let key: SchemaKey = (database, schema);
        schemas
            .entry(key.0)
            .or_default()
            .entry(key.1)
            .or_default()
            .push(relation);
    }

    Ok(Catalog {
        databases: schemas
            .into_iter()
            .map(|(name, schemas)| Database {
                name,
                schemas: schemas
                    .into_iter()
                    .map(|(name, relations)| Schema { name, relations })
                    .collect(),
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

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

    #[test]
    fn reports_its_version() {
        let flavor = DuckDbDriver::in_memory().unwrap().flavor();
        assert_eq!(flavor.dialect, Dialect::DuckDb);
        assert!(flavor.at_least(1, 0), "{flavor:?}");
        assert!(!flavor.version_text.starts_with('v'), "{flavor:?}");
    }

    #[test]
    fn streams_a_query_into_a_result_set() {
        let session = Session::spawn(DuckDbDriver::in_memory().unwrap());
        let (result, done) = collect(
            &session,
            "SELECT i, 'row ' || i AS label, CASE WHEN i % 2 = 0 THEN NULL ELSE i * 1.5 END AS half \
             FROM range(5000) t(i)",
            QueryOptions::default(),
        );
        let result = result.unwrap();
        let QueryEvent::Finished(summary) = done else {
            panic!("{done:?}")
        };
        assert_eq!(summary.rows, 5000);
        assert!(!summary.truncated);
        assert_eq!(result.row_count(), 5000);
        assert_eq!(result.column_name(1), "label");
        assert_eq!(result.cell_text(4321, 1).as_deref(), Some("row 4321"));
        assert!(result.is_null(4320, 2));
        assert_eq!(result.cell_text(4321, 2).as_deref(), Some("6481.5"));
        assert!(
            result.batches().len() > 1,
            "DuckDB streams more than one chunk"
        );
    }

    #[test]
    fn stops_at_the_row_limit() {
        let session = Session::spawn(DuckDbDriver::in_memory().unwrap());
        let (result, done) = collect(
            &session,
            "SELECT * FROM range(100000)",
            QueryOptions { max_rows: 3000 },
        );
        let QueryEvent::Finished(summary) = done else {
            panic!("{done:?}")
        };
        assert!(summary.truncated);
        assert_eq!(summary.rows, 3000);
        assert_eq!(result.unwrap().row_count(), 3000);
    }

    #[test]
    fn a_result_of_exactly_the_limit_is_not_truncated() {
        let session = Session::spawn(DuckDbDriver::in_memory().unwrap());
        let (result, done) = collect(
            &session,
            "SELECT * FROM range(4096)",
            QueryOptions { max_rows: 4096 },
        );
        let QueryEvent::Finished(summary) = done else {
            panic!("{done:?}")
        };
        assert!(!summary.truncated);
        assert_eq!(result.unwrap().row_count(), 4096);
    }

    #[test]
    fn runs_ddl_and_dml_and_reports_errors() {
        let session = Session::spawn(DuckDbDriver::in_memory().unwrap());
        let (_, done) = collect(
            &session,
            "CREATE TABLE t (id INTEGER, name VARCHAR)",
            QueryOptions::default(),
        );
        assert!(matches!(done, QueryEvent::Finished(_)), "{done:?}");
        let (result, _) = collect(
            &session,
            "INSERT INTO t VALUES (1, 'a'), (2, 'b')",
            QueryOptions::default(),
        );
        assert_eq!(result.unwrap().cell_text(0, 0).as_deref(), Some("2"));

        let (_, done) = collect(&session, "SELECT nope FROM t", QueryOptions::default());
        let QueryEvent::Failed { message, .. } = done else {
            panic!("{done:?}")
        };
        assert!(message.contains("nope"), "{message}");
        assert!(!message.contains("Error code"), "{message}");

        // The session keeps working after an error.
        let (result, _) = collect(&session, "SELECT count(*) FROM t", QueryOptions::default());
        assert_eq!(result.unwrap().cell_text(0, 0).as_deref(), Some("2"));
    }

    #[test]
    fn cancels_a_long_query() {
        let session = Session::spawn(DuckDbDriver::in_memory().unwrap());
        let query = session.query(
            "SELECT count(*) FROM range(10000000000) a, range(1000) b",
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

        let (result, _) = collect(&session, "SELECT 42", QueryOptions::default());
        assert_eq!(result.unwrap().cell_text(0, 0).as_deref(), Some("42"));
    }

    #[test]
    fn reads_the_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shop.duckdb");
        let session = Session::spawn(DuckDbDriver::open_path(&path).unwrap());
        for sql in [
            "CREATE SCHEMA sales",
            "CREATE TABLE sales.orders (id BIGINT NOT NULL, total DECIMAL(10,2))",
            "CREATE VIEW sales.big AS SELECT * FROM sales.orders WHERE total > 100",
            "CREATE SCHEMA empty",
        ] {
            let (_, done) = collect(&session, sql, QueryOptions::default());
            assert!(matches!(done, QueryEvent::Finished(_)), "{sql}: {done:?}");
        }

        let catalog = session.catalog().recv().unwrap().unwrap();
        let db = catalog
            .databases
            .iter()
            .find(|db| db.name == "shop")
            .expect("shop database");
        let names: Vec<_> = db.schemas.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["empty", "main", "sales"]);
        let sales = &db.schemas[2];
        assert_eq!(sales.relations.len(), 2);
        let orders = sales.relations.iter().find(|r| r.name == "orders").unwrap();
        assert_eq!(orders.kind, RelationKind::Table);
        assert_eq!(
            orders.columns[0],
            ColumnDef {
                name: "id".into(),
                data_type: "BIGINT".into(),
                nullable: false
            }
        );
        assert_eq!(orders.columns[1].data_type, "DECIMAL(10,2)");
        let big = sales.relations.iter().find(|r| r.name == "big").unwrap();
        assert_eq!(big.kind, RelationKind::View);
        assert_eq!(big.columns.len(), 2);
    }
}
