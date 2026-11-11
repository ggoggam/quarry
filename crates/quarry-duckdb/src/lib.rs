//! DuckDB driver for Quarry.
//!
//! Results come straight out of DuckDB as Arrow batches through
//! [`duckdb::Statement::stream_arrow`], which fetches chunks lazily: the first
//! rows reach the grid while the rest are still being produced, and stopping
//! at the row limit does not materialize the remainder.
//!
//! Data files are added as relations by [`Driver::add_file`]; see [`files`].

mod extensions;
pub mod files;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use duckdb::{AccessMode, Config, Connection, InterruptHandle};
use quarry_core::catalog::{Catalog, ColumnDef, Database, Relation, RelationKind, Schema};
use quarry_core::{AddedFile, BatchSink, Canceller, Dialect, Driver, Flavor};

pub use extensions::{EXTENSION_REPOSITORY, extension_purpose, missing_extension};
pub use files::DataFormat;

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
    /// The files added so far and the relations they became, so adding one
    /// again reuses its name.
    files: HashMap<PathBuf, AddedFile>,
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
        // Extensions already installed still load by themselves, but nothing
        // is downloaded without asking: a statement that needs a new one
        // fails naming it (see [`missing_extension`]) and the app asks.
        conn.execute_batch("SET autoinstall_known_extensions = false")?;
        files::register_arrow_scan(&conn)?;
        Ok(Self {
            conn,
            target,
            interrupt,
            flavor,
            files: HashMap::new(),
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

    /// An in-memory database gets a view (or, for Arrow, a table) in its
    /// `main` schema. A database file gets a temporary one, so opening a CSV
    /// next to it does not change the file.
    fn add_file(&mut self, path: &Path) -> anyhow::Result<AddedFile> {
        let path =
            std::fs::canonicalize(path).with_context(|| format!("opening {}", path.display()))?;
        let format = DataFormat::detect(&path).with_context(|| {
            format!(
                "{} is not a file Quarry can read (CSV, TSV, JSON, Parquet, Arrow, Excel or Avro)",
                path.display()
            )
        })?;
        let temporary = matches!(self.target, Target::File { .. });
        let (database, schema) = if temporary {
            ("temp".to_owned(), "main".to_owned())
        } else {
            let database: String = self
                .conn
                .query_row("SELECT current_database()", [], |row| row.get(0))
                .map_err(message)?;
            (database, "main".to_owned())
        };
        // The same file again keeps its name (and, for Arrow, is read again),
        // as long as the relation is still there.
        let name = match self.files.get(&path) {
            Some(added)
                if added.database == database
                    && files::free_name(&self.conn, &database, &schema, &added.name)?
                        != added.name =>
            {
                added.name.clone()
            }
            _ => files::free_name(&self.conn, &database, &schema, &files::relation_stem(&path))?,
        };
        let relation = files::qualified(&database, &schema, &name);
        files::create(&self.conn, &path, format, &relation, temporary)
            .with_context(|| format!("reading {} as {}", path.display(), format.name()))?;
        let added = AddedFile {
            database,
            schema,
            name,
        };
        self.files.insert(path, added.clone());
        Ok(added)
    }
}

/// DuckDB's own message, without the FFI error duckdb-rs chains under it
/// ("Error code 1: Unknown error code"), which says nothing to a user.
pub(crate) fn message(err: duckdb::Error) -> anyhow::Error {
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

    fn run(session: &Session, sql: &str) -> Option<ResultSet> {
        let (result, done) = collect(session, sql, QueryOptions::default());
        assert!(matches!(done, QueryEvent::Finished(_)), "{sql}: {done:?}");
        result
    }

    /// Rows of `a` that are not in `b` and the other way round.
    fn differences(session: &Session, a: &str, b: &str) -> String {
        let result = run(
            session,
            &format!(
                "SELECT (SELECT count(*) FROM (FROM {a} EXCEPT ALL FROM {b})) || ' ' || \
                 (SELECT count(*) FROM (FROM {b} EXCEPT ALL FROM {a}))"
            ),
        )
        .unwrap();
        result.cell_value(0, 0).unwrap()
    }

    /// Awkward values: quotes, separators, line breaks, empty strings,
    /// NULLs, NaN, dates, timestamps, decimals, JSON.
    const AWKWARD: &str = "CREATE TABLE src AS SELECT i AS id, \
        CASE i % 5 WHEN 0 THEN NULL WHEN 1 THEN '' WHEN 2 THEN 'say \"hi\", | it''s' \
            WHEN 3 THEN e'two\\nlines\\ttab' ELSE 'back\\slash ' || i END AS text, \
        CASE WHEN i % 7 = 0 THEN NULL ELSE i / 3.0 END AS score, \
        (i * 1.25)::DECIMAL(12, 2) AS price, \
        DATE '2024-01-01' + i::INTEGER AS day, \
        TIMESTAMP '2024-01-01 12:34:56.789' + INTERVAL (i) MINUTE AS at, \
        i % 3 = 0 AS ok, \
        json_object('i', i, 'tags', [i % 2, i % 3]) AS doc \
        FROM range(200) t(i)";

    #[test]
    fn exports_read_back_as_the_same_rows() {
        use quarry_core::export::{self, Format, Options};
        let dir = tempfile::tempdir().unwrap();
        let session = Session::spawn(DuckDbDriver::in_memory().unwrap());
        run(&session, AWKWARD);
        let result = run(&session, "SELECT * FROM src").unwrap();
        let columns: Vec<usize> = (0..result.column_count()).collect();
        let rows = 0..result.row_count();
        let write = |format: Format, table: Option<&str>| {
            let path = dir.path().join(format!("out.{}", format.extension()));
            let mut file = std::fs::File::create(&path).unwrap();
            let options = Options {
                table: table.map(str::to_owned),
                ..Options::default()
            };
            export::write(
                &result,
                rows.clone(),
                &columns,
                format,
                options,
                &mut file,
                &mut |_| true,
            )
            .unwrap();
            path.display().to_string()
        };
        let types = "{'id': 'BIGINT', 'text': 'VARCHAR', 'score': 'DOUBLE', \
                     'price': 'DECIMAL(12,2)', 'day': 'DATE', 'at': 'TIMESTAMP', \
                     'ok': 'BOOLEAN', 'doc': 'JSON'}";

        let csv = write(Format::Csv, None);
        run(
            &session,
            &format!(
                "CREATE TABLE from_csv AS FROM read_csv('{csv}', header = true, \
                 allow_quoted_nulls = false, columns = {types})"
            ),
        );
        assert_eq!(differences(&session, "src", "from_csv"), "0 0");

        let tsv = write(Format::Tsv, None);
        run(
            &session,
            &format!(
                "CREATE TABLE from_tsv AS FROM read_csv('{tsv}', header = true, delim = '\\t', \
                 allow_quoted_nulls = false, columns = {types})"
            ),
        );
        assert_eq!(differences(&session, "src", "from_tsv"), "0 0");

        // DuckDB does not tell a JSON column from text in its Arrow
        // output, so its JSON values are exported as strings.
        let json = write(Format::Json, None);
        run(
            &session,
            &format!(
                "CREATE TABLE from_json AS FROM read_json('{json}', format = 'array', \
                 columns = {})",
                types.replace("'doc': 'JSON'", "'doc': 'VARCHAR'")
            ),
        );
        assert_eq!(
            differences(
                &session,
                "(SELECT * REPLACE (doc::VARCHAR AS doc) FROM src)",
                "from_json"
            ),
            "0 0"
        );

        let sql = write(Format::SqlInsert, Some("from_sql"));
        run(&session, "CREATE TABLE from_sql AS FROM src LIMIT 0");
        let script = std::fs::read_to_string(&sql).unwrap();
        for statement in quarry_core::sql::split_statements_for(&script, Dialect::DuckDb) {
            run(&session, statement.text(&script));
        }
        assert_eq!(differences(&session, "src", "from_sql"), "0 0");
    }

    #[test]
    fn sorts_as_duckdb_orders() {
        use quarry_core::view::sort::{SortKey, sort_indices};
        let session = Session::spawn(DuckDbDriver::in_memory().unwrap());
        run(&session, AWKWARD);
        let result = run(&session, "SELECT * FROM src").unwrap();
        // DuckDB orders by bytes too; the id makes the order total.
        let keys = [
            SortKey::ascending(1),
            SortKey::descending(2),
            SortKey::ascending(0),
        ];
        let ours: Vec<String> = sort_indices(&result, &keys)
            .unwrap()
            .into_iter()
            .map(|row| result.cell_value(row as usize, 0).unwrap())
            .collect();
        let theirs = run(
            &session,
            "SELECT id FROM src ORDER BY text ASC NULLS LAST, score DESC NULLS LAST, id",
        )
        .unwrap();
        let theirs: Vec<String> = (0..theirs.row_count())
            .map(|row| theirs.cell_value(row, 0).unwrap())
            .collect();
        assert_eq!(ours, theirs);
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

    fn text(driver: &DuckDbDriver, sql: &str) -> String {
        driver
            .conn
            .query_row(sql, [], |row| row.get::<_, String>(0))
            .unwrap()
    }

    /// `users` as an Arrow IPC file, lz4 compressed the way pyarrow writes
    /// Feather by default, in two batches.
    fn write_feather(path: &Path) {
        use arrow_ipc::CompressionType;
        use arrow_ipc::writer::{FileWriter, IpcWriteOptions};
        use duckdb::arrow::array::ArrayRef;
        use duckdb::arrow::array::{Int64Array, StringArray};
        use duckdb::arrow::datatypes::{DataType, Field, Schema};
        use duckdb::arrow::record_batch::RecordBatch;
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("team", DataType::Utf8, true),
        ]));
        let options = IpcWriteOptions::default()
            .try_with_compression(Some(CompressionType::LZ4_FRAME))
            .unwrap();
        let file = std::fs::File::create(path).unwrap();
        let mut writer = FileWriter::try_new_with_options(file, &schema, options).unwrap();
        for (ids, teams) in [
            (vec![1, 2], vec![Some("red"), None]),
            (vec![3], vec![Some("blue")]),
        ] {
            let batch = RecordBatch::try_new(
                schema.clone(),
                vec![
                    Arc::new(Int64Array::from(ids)) as ArrayRef,
                    Arc::new(StringArray::from(teams)),
                ],
            )
            .unwrap();
            writer.write(&batch).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn joins_files_of_every_format() {
        let dir = tempfile::tempdir().unwrap();
        let at = |name: &str| dir.path().join(name);
        std::fs::write(at("Users.csv"), "id,name\n1,ann\n2,bo\n3,cy\n").unwrap();
        std::fs::write(
            at("orders.ndjson"),
            "{\"user_id\":1,\"amount\":10}\n{\"user_id\":3,\"amount\":5}\n{\"user_id\":1,\"amount\":7}\n",
        )
        .unwrap();
        write_feather(&at("teams.feather"));

        let mut driver = DuckDbDriver::in_memory().unwrap();
        driver
            .conn
            .execute_batch(&format!(
                "COPY (SELECT 1 AS user_id, 'card' AS method UNION ALL SELECT 3, 'cash') \
                 TO '{}' (FORMAT parquet)",
                at("payments.parquet").display()
            ))
            .unwrap();
        let added = |driver: &mut DuckDbDriver, name: &str| driver.add_file(&at(name)).unwrap();
        let users = added(&mut driver, "Users.csv");
        assert_eq!(
            users,
            AddedFile {
                database: "memory".into(),
                schema: "main".into(),
                name: "users".into(),
            }
        );
        assert_eq!(added(&mut driver, "orders.ndjson").name, "orders");
        assert_eq!(added(&mut driver, "payments.parquet").name, "payments");
        assert_eq!(added(&mut driver, "teams.feather").name, "teams");

        assert_eq!(
            text(
                &driver,
                "SELECT string_agg(concat_ws(':', u.name, t.team, p.method, total), ' ' ORDER BY u.id) \
                   FROM users u \
                   JOIN teams t USING (id) \
                   LEFT JOIN payments p ON p.user_id = u.id \
                   LEFT JOIN (SELECT user_id, sum(amount) AS total FROM orders GROUP BY ALL) o \
                     ON o.user_id = u.id"
            ),
            "ann:red:card:17 bo cy:blue:cash:5"
        );

        // Files show up in the catalog like tables, columns and all.
        let catalog = driver.catalog().unwrap();
        let main = &catalog.databases[0].schemas[0];
        let names: Vec<_> = main.relations.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["orders", "payments", "teams", "users"]);
        let teams = main.relations.iter().find(|r| r.name == "teams").unwrap();
        assert_eq!(teams.kind, RelationKind::Table);
        assert_eq!(teams.columns[1].name, "team");
        let users = main.relations.iter().find(|r| r.name == "users").unwrap();
        assert_eq!(users.kind, RelationKind::View);
        assert_eq!(users.columns.len(), 2);

        // Views read the file each time; adding the same file again keeps
        // its name, while another file with the same stem gets a new one.
        std::fs::write(at("Users.csv"), "id,name\n1,ann\n").unwrap();
        assert_eq!(text(&driver, "SELECT count(*)::VARCHAR FROM users"), "1");
        assert_eq!(added(&mut driver, "Users.csv").name, "users");
        std::fs::create_dir(at("2025")).unwrap();
        std::fs::write(at("2025/users.csv"), "id\n9\n").unwrap();
        assert_eq!(added(&mut driver, "2025/users.csv").name, "users_2");
        // A name the user took is left alone.
        driver
            .conn
            .execute_batch("CREATE TABLE notes (x INT)")
            .unwrap();
        std::fs::write(at("notes.csv"), "x\n1\n").unwrap();
        assert_eq!(added(&mut driver, "notes.csv").name, "notes_2");
        // A relation the user dropped comes back under its old name.
        driver.conn.execute_batch("DROP VIEW orders").unwrap();
        assert_eq!(added(&mut driver, "orders.ndjson").name, "orders");
    }

    #[test]
    fn database_files_get_temporary_relations() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("shop.duckdb");
        let csv = dir.path().join("prices.csv");
        std::fs::write(&csv, "sku,price\na,1.5\n").unwrap();
        {
            let mut driver = DuckDbDriver::open_path(&db).unwrap();
            let added = driver.add_file(&csv).unwrap();
            assert_eq!(
                (added.database.as_str(), added.schema.as_str()),
                ("temp", "main")
            );
            assert_eq!(text(&driver, "SELECT sku FROM prices"), "a");
            let catalog = driver.catalog().unwrap();
            let temp = catalog.databases.iter().find(|d| d.name == "temp").unwrap();
            assert_eq!(temp.schemas[0].relations[0].name, "prices");
        }
        // Nothing was written into the database file.
        let driver = DuckDbDriver::open_path(&db).unwrap();
        assert_eq!(
            text(
                &driver,
                "SELECT count(*)::VARCHAR FROM duckdb_views() WHERE NOT internal"
            ),
            "0"
        );
    }

    #[test]
    fn explains_files_it_cannot_read() {
        let dir = tempfile::tempdir().unwrap();
        let mut driver = DuckDbDriver::in_memory().unwrap();
        let v1 = dir.path().join("old.feather");
        std::fs::write(&v1, b"FEA1\0\0\0\0").unwrap();
        let err = format!("{:#}", driver.add_file(&v1).unwrap_err());
        assert!(err.contains("Feather version 1"), "{err}");
        let other = dir.path().join("notes.txt");
        std::fs::write(&other, "hello").unwrap();
        let err = format!("{:#}", driver.add_file(&other).unwrap_err());
        assert!(err.contains("not a file Quarry can read"), "{err}");
        let broken = dir.path().join("broken.arrow");
        std::fs::write(&broken, b"ARROW1\0\0garbage").unwrap();
        assert!(driver.add_file(&broken).is_err());
        // Nothing half-made is left behind.
        assert!(
            driver.catalog().unwrap().databases[0].schemas[0]
                .relations
                .is_empty()
        );
    }

    #[test]
    fn asks_before_installing_an_extension() {
        let dir = tempfile::tempdir().unwrap();
        let mut driver = DuckDbDriver::in_memory().unwrap();
        // No extensions installed, so nothing loads by itself either.
        let extensions = dir.path().join("extensions");
        driver
            .conn
            .execute_batch(&format!(
                "SET extension_directory = '{}'",
                extensions.display()
            ))
            .unwrap();
        let xlsx = dir.path().join("tiers.xlsx");
        std::fs::write(&xlsx, b"PK\x03\x04").unwrap();
        let err = format!("{:#}", driver.add_file(&xlsx).unwrap_err());
        assert_eq!(missing_extension(&err).as_deref(), Some("excel"), "{err}");
        let mut sink = Collect;
        let err = driver
            .execute("SELECT * FROM 's3://bucket/x.parquet'", &mut sink)
            .unwrap_err();
        assert_eq!(
            missing_extension(&format!("{err:#}")).as_deref(),
            Some("httpfs")
        );
        // Nothing was downloaded.
        assert!(!extensions.exists() || std::fs::read_dir(&extensions).unwrap().next().is_none());
    }

    struct Collect;

    impl BatchSink for Collect {
        fn schema(&mut self, _: duckdb::arrow::datatypes::SchemaRef) {}
        fn batch(
            &mut self,
            _: duckdb::arrow::record_batch::RecordBatch,
        ) -> std::ops::ControlFlow<()> {
            std::ops::ControlFlow::Continue(())
        }
    }
}
