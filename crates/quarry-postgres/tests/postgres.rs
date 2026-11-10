//! Integration tests against real servers.
//!
//! Set `QUARRY_TEST_POSTGRES_URLS` to a comma-separated list of connection
//! URLs (ideally releases far apart, e.g. 13 and 17); every case runs
//! against each. Unset, the cases print a note and pass.

use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use arrow::array::RecordBatch;
use arrow::datatypes::{DataType, SchemaRef};
use quarry_core::catalog::{Relation, RelationKind};
use quarry_core::result::field_type_label;
use quarry_core::{BatchSink, Dialect, Driver, QueryEvent, QueryOptions, ResultSet, Session};
use quarry_postgres::PostgresDriver;

const URLS: &str = "QUARRY_TEST_POSTGRES_URLS";

fn each_server(test: impl Fn(&str)) {
    let urls: Vec<String> = std::env::var(URLS)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_owned)
        .collect();
    if urls.is_empty() {
        eprintln!("skipping: set {URLS} to run against PostgreSQL servers");
        return;
    }
    for url in urls {
        eprintln!("against {}", redacted(&url));
        test(&url);
    }
}

fn redacted(url: &str) -> String {
    PostgresDriver::connect(url)
        .map(|driver| driver.target())
        .unwrap_or_else(|_| "<unreachable server>".into())
}

fn session(url: &str) -> Session {
    Session::spawn(PostgresDriver::connect(url).expect("connect"))
}

fn collect(session: &Session, sql: &str, options: QueryOptions) -> (Option<ResultSet>, QueryEvent) {
    let query = session.query(sql, options);
    let mut result = None;
    loop {
        match query
            .events
            .recv_timeout(Duration::from_secs(60))
            .expect("event")
        {
            QueryEvent::Schema(schema) => result = Some(ResultSet::new(schema)),
            QueryEvent::Batch(batch) => result.as_mut().expect("schema first").push(batch),
            done => return (result, done),
        }
    }
}

/// Run a statement that must succeed, returning its result.
fn run(session: &Session, sql: &str) -> ResultSet {
    let (result, done) = collect(session, sql, QueryOptions::default());
    assert!(
        matches!(done, QueryEvent::Finished(ref s) if !s.cancelled),
        "{sql}: {done:?}"
    );
    result.unwrap_or_else(|| panic!("{sql}: no schema"))
}

fn fail(session: &Session, sql: &str) -> String {
    let (_, done) = collect(session, sql, QueryOptions::default());
    match done {
        QueryEvent::Failed { message, .. } => message,
        other => panic!("{sql} should fail: {other:?}"),
    }
}

fn fresh_schema(session: &Session, name: &str) {
    run(session, &format!("DROP SCHEMA IF EXISTS {name} CASCADE"));
    run(session, &format!("CREATE SCHEMA {name}"));
}

fn major(session: &Session) -> u32 {
    session.flavor().version.expect("version").major
}

#[test]
fn connects_and_reports_the_server_version() {
    each_server(|url| {
        let driver = PostgresDriver::connect(url).unwrap();
        let flavor = driver.flavor();
        let target = driver.target();
        assert_eq!(flavor.dialect, Dialect::Postgres);
        assert!(!flavor.version_text.contains(' '), "{flavor:?}");
        assert!(!target.contains(":quarry@"), "{target}");

        let session = Session::spawn(driver);
        let num = run(&session, "SHOW server_version_num");
        let num: u32 = num.cell_value(0, 0).unwrap().parse().unwrap();
        let version = session.flavor().version.unwrap();
        // 170002 for 17.2, 90624 for 9.6.24.
        assert_eq!(version.major, num / 10_000);
    });
}

#[test]
fn fails_to_connect_with_a_clear_message() {
    each_server(|url| {
        let wrong = url.replacen(":quarry@", ":wrong-password@", 1);
        if wrong == url {
            return;
        }
        let err = PostgresDriver::connect(&wrong).err().expect("bad password");
        let text = format!("{err:#}");
        assert!(text.contains("password authentication failed"), "{text}");
        assert!(text.contains("28P01"), "{text}");
        assert!(!text.contains("wrong-password"), "{text}");
    });
}

#[test]
fn streams_many_rows_across_batches() {
    each_server(|url| {
        let session = session(url);
        let result = run(
            &session,
            "SELECT n, 'row ' || n AS label, CASE WHEN n % 2 = 0 THEN NULL ELSE n * 1.5 END AS half \
               FROM generate_series(1, 20000) n",
        );
        assert_eq!(result.row_count(), 20000);
        assert_eq!(result.column_name(1), "label");
        assert_eq!(result.cell_text(4320, 1).as_deref(), Some("row 4321"));
        assert!(result.is_null(4321, 2));
        assert_eq!(result.cell_text(4320, 2).as_deref(), Some("6481.5"));
        let sizes: Vec<_> = result.batches().iter().map(|b| b.num_rows()).collect();
        assert_eq!(sizes[0], 100, "{sizes:?}");
        assert!(sizes[1..].iter().all(|&n| n <= 4096), "{sizes:?}");
        assert!(sizes.len() >= 5, "{sizes:?}");
    });
}

#[derive(Default)]
struct Timed {
    started: Option<Instant>,
    batches: Vec<(usize, Duration)>,
}

impl BatchSink for Timed {
    fn schema(&mut self, _: SchemaRef) {}
    fn batch(&mut self, batch: RecordBatch) -> ControlFlow<()> {
        let at = self.started.unwrap().elapsed();
        self.batches.push((batch.num_rows(), at));
        ControlFlow::Continue(())
    }
}

#[test]
fn sends_the_first_rows_without_waiting_for_a_full_batch() {
    each_server(|url| {
        let mut driver = PostgresDriver::connect(url).unwrap();
        let mut sink = Timed {
            started: Some(Instant::now()),
            ..Timed::default()
        };
        // One 2 KB row every 20 ms for 1.2 s. The server buffers rows and
        // sends them in ~8 KB socket writes, so narrow rows reach the client
        // only at the end of a slow query; wide rows stream as they come.
        driver
            .execute(
                "SELECT n, repeat('x', 2000), pg_sleep(0.02) FROM generate_series(1, 60) n",
                &mut sink,
            )
            .unwrap();
        let (_, first_at) = sink.batches[0];
        let (_, last_at) = *sink.batches.last().unwrap();
        assert!(first_at < Duration::from_millis(500), "{:?}", sink.batches);
        assert!(
            last_at > first_at + Duration::from_millis(500),
            "rows arrived all at once: {:?}",
            sink.batches
        );
        assert!(sink.batches.len() > 1, "{:?}", sink.batches);
        assert_eq!(sink.batches.iter().map(|b| b.0).sum::<usize>(), 60);
    });
}

#[test]
fn stops_at_the_row_limit_and_stays_usable() {
    each_server(|url| {
        let session = session(url);
        // Far more rows than anyone waits for: the rest must be cancelled,
        // not drained.
        let started = Instant::now();
        let (result, done) = collect(
            &session,
            "SELECT generate_series(1, 200000000) AS n",
            QueryOptions { max_rows: 1000 },
        );
        let QueryEvent::Finished(summary) = done else {
            panic!("{done:?}")
        };
        assert!(summary.truncated && !summary.cancelled, "{summary:?}");
        assert_eq!(result.unwrap().row_count(), 1000);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(
            run(&session, "SELECT 42").cell_text(0, 0).as_deref(),
            Some("42")
        );

        let (result, done) = collect(
            &session,
            "SELECT * FROM generate_series(1, 4096)",
            QueryOptions { max_rows: 4096 },
        );
        assert!(
            matches!(done, QueryEvent::Finished(ref s) if !s.truncated),
            "{done:?}"
        );
        assert_eq!(result.unwrap().row_count(), 4096);
    });
}

#[test]
fn a_row_limit_neither_aborts_a_transaction_nor_skips_writes() {
    each_server(|url| {
        let session = session(url);
        fresh_schema(&session, "quarry_t_limit");
        run(&session, "CREATE TABLE quarry_t_limit.t (id int)");

        // Inside a transaction, the rest of a read is drained: a cancel
        // would abort the transaction.
        run(&session, "BEGIN");
        run(&session, "INSERT INTO quarry_t_limit.t VALUES (0)");
        let (result, done) = collect(
            &session,
            "SELECT generate_series(1, 300000) AS n",
            QueryOptions { max_rows: 100 },
        );
        assert!(
            matches!(done, QueryEvent::Finished(ref s) if s.truncated),
            "{done:?}"
        );
        assert_eq!(result.unwrap().row_count(), 100);
        let count = run(&session, "SELECT count(*) FROM quarry_t_limit.t");
        assert_eq!(count.cell_text(0, 0).as_deref(), Some("1"));
        run(&session, "COMMIT");

        // A write's rows past the limit are still written.
        let (result, done) = collect(
            &session,
            "INSERT INTO quarry_t_limit.t SELECT generate_series(1, 50000) RETURNING id",
            QueryOptions { max_rows: 10 },
        );
        assert!(
            matches!(done, QueryEvent::Finished(ref s) if s.truncated),
            "{done:?}"
        );
        assert_eq!(result.unwrap().row_count(), 10);
        let count = run(&session, "SELECT count(*) FROM quarry_t_limit.t");
        assert_eq!(count.cell_text(0, 0).as_deref(), Some("50001"));
        run(&session, "DROP SCHEMA quarry_t_limit CASCADE");
    });
}

#[test]
fn types_columns_and_names_them_like_postgres() {
    each_server(|url| {
        let session = session(url);
        run(&session, "SET TIME ZONE 'Asia/Seoul'");
        let result = run(
            &session,
            "SELECT true AS b, 7::int2 AS i2, 42 AS i4, 9000000000 AS i8, 1.5::float4 AS f4, \
                    0.1::float8 AS f8, 123.45::numeric(10,2) AS dec, 2.50::numeric / 2 AS num, \
                    'hi'::text AS txt, 'x'::varchar(20) AS vc, 'ab'::char(3) AS ch, \
                    DATE '2024-01-31' AS d, TIME '12:34:56.5' AS t, \
                    TIMESTAMP '2024-01-31 12:34:56' AS ts, \
                    TIMESTAMPTZ '2024-01-31 21:34:56+09' AS tstz, \
                    INTERVAL '1 year 2 months 3 days 04:05:06' AS iv, '\\xdead'::bytea AS by, \
                    '{\"a\": [1, 2]}'::jsonb AS j, \
                    'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid AS u, ARRAY[1, NULL, 3] AS arr, \
                    '[1,5)'::int4range AS r, 'pg_class'::regclass::oid AS o",
        );
        // Typed NULLs, in a separate statement: a UNION would reset the
        // numeric(10,2) modifier to plain numeric.
        let nulls = run(
            &session,
            "SELECT NULL::boolean, NULL::smallint, NULL::integer, NULL::bigint, NULL::real, \
                    NULL::double precision, NULL::numeric(10,2), NULL::numeric, NULL::text, \
                    NULL::varchar(20), NULL::char(3), NULL::date, NULL::time, NULL::timestamp, \
                    NULL::timestamptz, NULL::interval, NULL::bytea, NULL::jsonb, NULL::uuid, \
                    NULL::int4[], NULL::int4range, NULL::oid",
        );
        let expect = [
            ("b", "bool", DataType::Boolean, "true"),
            ("i2", "int2", DataType::Int16, "7"),
            ("i4", "int4", DataType::Int32, "42"),
            ("i8", "int8", DataType::Int64, "9000000000"),
            ("f4", "float4", DataType::Float32, "1.5"),
            ("f8", "float8", DataType::Float64, "0.1"),
            (
                "dec",
                "numeric(10,2)",
                DataType::Decimal128(10, 2),
                "123.45",
            ),
            (
                "num",
                "numeric",
                // Announced before any row, so the scale is a placeholder;
                // the batch below carries the scale its values need.
                DataType::Decimal128(38, 0),
                "1.25000000000000000000",
            ),
            ("txt", "text", DataType::Utf8, "hi"),
            ("vc", "varchar(20)", DataType::Utf8, "x"),
            ("ch", "char(3)", DataType::Utf8, "ab "),
            ("d", "date", DataType::Date32, "2024-01-31"),
            (
                "t",
                "time",
                DataType::Time64(arrow::datatypes::TimeUnit::Microsecond),
                "12:34:56.500",
            ),
            (
                "ts",
                "timestamp",
                DataType::Timestamp(arrow::datatypes::TimeUnit::Microsecond, None),
                "2024-01-31T12:34:56",
            ),
            (
                "tstz",
                "timestamptz",
                DataType::Timestamp(
                    arrow::datatypes::TimeUnit::Microsecond,
                    Some("+00:00".into()),
                ),
                "2024-01-31T12:34:56Z",
            ),
            (
                "iv",
                "interval",
                DataType::Interval(arrow::datatypes::IntervalUnit::MonthDayNano),
                "14 mons 3 days 4 hours 5 mins 6.000000000 secs",
            ),
            ("by", "bytea", DataType::Binary, "dead"),
            ("j", "jsonb", DataType::Utf8, "{\"a\": [1, 2]}"),
            (
                "u",
                "uuid",
                DataType::Utf8,
                "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
            ),
            ("arr", "int4[]", DataType::Utf8, "{1,NULL,3}"),
            ("r", "int4range", DataType::Utf8, "[1,5)"),
            ("o", "oid", DataType::UInt32, "1259"),
        ];
        let schema = result.schema().clone();
        for (col, (name, native, data_type, text)) in expect.iter().enumerate() {
            let field = schema.field(col);
            assert_eq!(field.name(), name);
            assert_eq!(field_type_label(field), *native, "{name}");
            assert_eq!(field.data_type(), data_type, "{name}");
            if !text.is_empty() {
                assert_eq!(result.cell_text(0, col).as_deref(), Some(*text), "{name}");
            }
            assert!(nulls.is_null(0, col), "{name}");
        }
        // Unconstrained numeric takes its batch's scale.
        let batch = &result.batches()[0];
        assert_eq!(
            batch.schema().field(7).data_type(),
            &DataType::Decimal128(38, 20)
        );
        assert_eq!(
            result.cell_text(0, 7).as_deref(),
            Some("1.25000000000000000000")
        );

        // Values Arrow cannot hold fall back to text for their batch.
        let special = run(
            &session,
            "SELECT 'infinity'::date AS d, 'NaN'::numeric AS n, '24:00:00'::time AS t",
        );
        assert_eq!(special.cell_text(0, 0).as_deref(), Some("infinity"));
        assert_eq!(special.cell_text(0, 1).as_deref(), Some("NaN"));
        assert_eq!(special.cell_text(0, 2).as_deref(), Some("24:00:00"));
        assert_eq!(special.column_type_label(0), "date");

        // Floats come back exactly on every release.
        let floats = run(&session, "SELECT 0.1::float8 + 0.2::float8, 1/3::float4");
        assert_eq!(
            floats.cell_text(0, 0).as_deref(),
            Some("0.30000000000000004")
        );
    });
}

#[test]
fn names_enums_domains_and_runs_unpreparable_scripts() {
    each_server(|url| {
        let session = session(url);
        fresh_schema(&session, "quarry_t_types");
        run(
            &session,
            "CREATE TYPE quarry_t_types.mood AS ENUM ('sad', 'ok')",
        );
        run(
            &session,
            "CREATE DOMAIN quarry_t_types.price AS numeric(8,2) CHECK (VALUE >= 0)",
        );
        let result = run(
            &session,
            "SELECT 'ok'::quarry_t_types.mood AS m, 9.5::quarry_t_types.price AS p",
        );
        assert_eq!(result.column_type_label(0), "mood");
        assert_eq!(result.cell_text(0, 0).as_deref(), Some("ok"));
        // A domain column is reported as its base type, as PostgreSQL does.
        assert_eq!(result.column_type_label(1), "numeric(8,2)");
        assert_eq!(result.cell_text(0, 1).as_deref(), Some("9.50"));

        // Two statements cannot be prepared; the first one's rows still
        // show, as text.
        let result = run(&session, "SELECT 1 AS a; SELECT 2 AS b");
        assert_eq!(result.column_name(0), "a");
        assert_eq!(result.column_type(0), &DataType::Utf8);
        assert_eq!(result.cell_text(0, 0).as_deref(), Some("1"));
        run(&session, "DROP SCHEMA quarry_t_types CASCADE");
    });
}

#[test]
fn reports_rows_affected_and_runs_ddl() {
    each_server(|url| {
        let session = session(url);
        fresh_schema(&session, "quarry_t_dml");
        let ddl = run(
            &session,
            "CREATE TABLE quarry_t_dml.t (id int PRIMARY KEY, name text)",
        );
        assert_eq!(ddl.column_count(), 0);
        let insert = run(
            &session,
            "INSERT INTO quarry_t_dml.t VALUES (1, 'a'), (2, 'b'), (3, 'c')",
        );
        assert_eq!(insert.column_name(0), "rows affected");
        assert_eq!(insert.cell_text(0, 0).as_deref(), Some("3"));
        let update = run(
            &session,
            "UPDATE quarry_t_dml.t SET name = 'z' WHERE id > 1",
        );
        assert_eq!(update.cell_text(0, 0).as_deref(), Some("2"));
        let cte = run(
            &session,
            "WITH gone AS (SELECT 3 AS id) DELETE FROM quarry_t_dml.t USING gone WHERE t.id = gone.id",
        );
        assert_eq!(cte.cell_text(0, 0).as_deref(), Some("1"));
        let returning = run(
            &session,
            "DELETE FROM quarry_t_dml.t WHERE id = 2 RETURNING name",
        );
        assert_eq!(returning.column_name(0), "name");
        assert_eq!(returning.cell_text(0, 0).as_deref(), Some("z"));
        if major(&session) >= 15 {
            let merge = run(
                &session,
                "MERGE INTO quarry_t_dml.t USING (VALUES (1), (5)) v(id) ON t.id = v.id \
                 WHEN MATCHED THEN UPDATE SET name = 'm' \
                 WHEN NOT MATCHED THEN INSERT VALUES (v.id, 'n')",
            );
            assert_eq!(merge.cell_text(0, 0).as_deref(), Some("2"));
        }
        for sql in [
            "SET search_path = quarry_t_dml, public",
            "BEGIN",
            "COMMIT",
            "-- nothing",
        ] {
            assert_eq!(run(&session, sql).column_count(), 0, "{sql}");
        }
        run(&session, "DROP SCHEMA quarry_t_dml CASCADE");
    });
}

#[test]
fn reports_postgres_errors_and_keeps_working() {
    each_server(|url| {
        let session = session(url);
        let message = fail(&session, "SELECT 1,\n  nope FROM pg_class");
        assert!(
            message.starts_with("column \"nope\" does not exist"),
            "{message}"
        );
        assert!(
            message.contains("SQLSTATE 42703 at line 2, column 3"),
            "{message}"
        );
        assert!(!message.contains("db error"), "{message}");

        fresh_schema(&session, "quarry_t_err");
        run(&session, "CREATE TABLE quarry_t_err.t (id int PRIMARY KEY)");
        run(&session, "INSERT INTO quarry_t_err.t VALUES (1)");
        let message = fail(&session, "INSERT INTO quarry_t_err.t VALUES (1)");
        assert!(message.contains("duplicate key value"), "{message}");
        assert!(
            message.contains("\nDETAIL: Key (id)=(1) already exists."),
            "{message}"
        );
        assert!(message.contains("SQLSTATE 23505"), "{message}");

        let message = fail(&session, "SELEC 1");
        assert!(
            message.contains("syntax error at or near \"SELEC\""),
            "{message}"
        );

        // In a transaction, the error that aborted it is the one reported.
        run(&session, "BEGIN");
        let message = fail(&session, "SELECT nope");
        assert!(
            message.contains("column \"nope\" does not exist"),
            "{message}"
        );
        let message = fail(&session, "SELECT 1");
        assert!(message.contains("25P02"), "{message}");
        run(&session, "ROLLBACK");

        assert_eq!(
            run(&session, "SELECT 7").cell_text(0, 0).as_deref(),
            Some("7")
        );
        run(&session, "DROP SCHEMA quarry_t_err CASCADE");
    });
}

#[test]
fn cancels_a_running_query() {
    each_server(|url| {
        let session = session(url);
        let query = session.query("SELECT pg_sleep(30)", QueryOptions::default());
        std::thread::sleep(Duration::from_millis(300));
        let started = Instant::now();
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
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(
            run(&session, "SELECT 42").cell_text(0, 0).as_deref(),
            Some("42")
        );
    });
}

fn relation<'a>(relations: &'a [Relation], name: &str) -> &'a Relation {
    relations
        .iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| panic!("no {name} in {relations:?}"))
}

#[test]
fn reads_the_catalog() {
    each_server(|url| {
        let session = session(url);
        let version = major(&session);
        for name in ["quarry_t_cat", "quarry_t_cat_empty"] {
            fresh_schema(&session, name);
        }
        for sql in [
            "CREATE TABLE quarry_t_cat.orders (id bigint NOT NULL, total numeric(10,2), \
                 note varchar(20), meta jsonb, tags text[], at timestamptz)",
            "ALTER TABLE quarry_t_cat.orders DROP COLUMN note",
            "CREATE VIEW quarry_t_cat.big AS SELECT id, total FROM quarry_t_cat.orders WHERE total > 100",
            "CREATE MATERIALIZED VIEW quarry_t_cat.totals AS SELECT sum(total) AS s FROM quarry_t_cat.orders",
            "CREATE TABLE quarry_t_cat.events (id int, at date NOT NULL) PARTITION BY RANGE (at)",
            "CREATE TABLE quarry_t_cat.events_2023 PARTITION OF quarry_t_cat.events \
                 FOR VALUES FROM ('2023-01-01') TO ('2024-01-01')",
            "CREATE TABLE quarry_t_cat.events_2024 PARTITION OF quarry_t_cat.events \
                 FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')",
            "INSERT INTO quarry_t_cat.events SELECT n, DATE '2023-06-01' + n % 400 \
                 FROM generate_series(1, 2000) n",
            "ANALYZE quarry_t_cat.events",
            "CREATE TABLE quarry_t_cat.fresh (id int)",
            "CREATE TABLE quarry_t_cat.analyzed_empty (id int)",
            "ANALYZE quarry_t_cat.analyzed_empty",
        ] {
            run(&session, sql);
        }

        let catalog = session.catalog().recv().unwrap().unwrap();
        assert_eq!(catalog.databases.len(), 1);
        let db = &catalog.databases[0];
        let names: Vec<_> = db.schemas.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"public"), "{names:?}");
        assert!(names.contains(&"quarry_t_cat_empty"), "{names:?}");
        for hidden in ["pg_catalog", "information_schema", "pg_toast"] {
            assert!(!names.contains(&hidden), "{names:?}");
        }
        assert!(!names.iter().any(|n| n.starts_with("pg_temp")), "{names:?}");

        let schema = db
            .schemas
            .iter()
            .find(|s| s.name == "quarry_t_cat")
            .unwrap();
        let rels = &schema.relations;
        let listed: Vec<_> = rels.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            listed,
            [
                "analyzed_empty",
                "big",
                "events",
                "fresh",
                "orders",
                "totals"
            ],
            "partitions are hidden"
        );

        let orders = relation(rels, "orders");
        assert_eq!(orders.kind, RelationKind::Table);
        let columns: Vec<_> = orders
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.data_type.as_str(), c.nullable))
            .collect();
        assert_eq!(
            columns,
            [
                ("id", "bigint", false),
                ("total", "numeric(10,2)", true),
                ("meta", "jsonb", true),
                ("tags", "text[]", true),
                ("at", "timestamp with time zone", true),
            ]
        );
        assert_eq!(relation(rels, "big").kind, RelationKind::View);
        assert_eq!(relation(rels, "big").estimated_rows, None);
        assert_eq!(relation(rels, "big").columns.len(), 2);
        assert_eq!(
            relation(rels, "totals").kind,
            RelationKind::MaterializedView
        );

        let events = relation(rels, "events");
        assert_eq!(events.kind, RelationKind::Table);
        assert_eq!(events.columns.len(), 2);
        assert_eq!(events.estimated_rows, Some(2000), "the partitions' sum");

        // The version gate: only 14+ can tell "analyzed, empty" from
        // "never analyzed".
        assert_eq!(relation(rels, "fresh").estimated_rows, None);
        let analyzed_empty = relation(rels, "analyzed_empty").estimated_rows;
        if version >= 14 {
            assert_eq!(analyzed_empty, Some(0));
        } else {
            assert_eq!(analyzed_empty, None);
        }

        let empty = db
            .schemas
            .iter()
            .find(|s| s.name == "quarry_t_cat_empty")
            .unwrap();
        assert!(empty.relations.is_empty());
        for name in ["quarry_t_cat", "quarry_t_cat_empty"] {
            run(&session, &format!("DROP SCHEMA {name} CASCADE"));
        }
    });
}

#[test]
fn version_gated_types() {
    each_server(|url| {
        let session = session(url);
        if major(&session) >= 15 {
            // Negative scales arrived in 15.
            let result = run(&session, "SELECT 12345::numeric(5,-2) AS n");
            assert_eq!(result.column_type_label(0), "numeric(5,-2)");
            assert_eq!(result.column_type(0), &DataType::Decimal128(5, -2));
            assert_eq!(result.cell_value(0, 0).as_deref(), Some("12300"));
        } else {
            let message = fail(&session, "SELECT 12345::numeric(5,-2)");
            assert!(message.contains("SQLSTATE"), "{message}");
        }
        if major(&session) >= 14 {
            // So did numeric infinity in 14; it stays text.
            let result = run(&session, "SELECT 'Infinity'::numeric AS n");
            assert_eq!(result.cell_text(0, 0).as_deref(), Some("Infinity"));
        }
    });
}

#[test]
fn honours_sslmode() {
    each_server(|url| {
        let with = |mode: &str| {
            let sep = if url.contains('?') { '&' } else { '?' };
            format!("{url}{sep}sslmode={mode}")
        };
        let plain = session(&with("disable"));
        let ssl = |session: &Session| {
            run(
                session,
                "SELECT ssl::text FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
            )
            .cell_value(0, 0)
        };
        assert_eq!(ssl(&plain).as_deref(), Some("false"));
        match PostgresDriver::connect(&with("require")) {
            Ok(driver) => {
                let session = Session::spawn(driver);
                assert_eq!(ssl(&session).as_deref(), Some("true"));
                // A self-signed certificate does not verify.
                let err = PostgresDriver::connect(&with("verify-full")).err().unwrap();
                let text = format!("{err:#}");
                assert!(text.to_lowercase().contains("certificate"), "{text}");
            }
            Err(err) => {
                let text = format!("{err:#}");
                assert!(text.contains("TLS"), "{text}");
            }
        }
    });
}
