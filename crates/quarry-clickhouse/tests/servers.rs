//! Tests against real ClickHouse servers.
//!
//! Set `QUARRY_TEST_CLICKHOUSE_URLS` to comma-separated connection URLs, e.g.
//! `clickhouse://default:quarry@localhost:58123,clickhouse://default:quarry@localhost:58125`;
//! every case runs against each. Without it the tests pass with a note.

use std::time::{Duration, Instant};

use quarry_clickhouse::ClickHouseDriver;
use quarry_clickhouse::gates;
use quarry_core::arrow::datatypes::DataType;
use quarry_core::catalog::RelationKind;
use quarry_core::{Driver, QueryEvent, QueryOptions, ResultSet, Session};

fn urls() -> Vec<String> {
    let urls: Vec<String> = std::env::var("QUARRY_TEST_CLICKHOUSE_URLS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_owned)
        .collect();
    if urls.is_empty() {
        eprintln!("QUARRY_TEST_CLICKHOUSE_URLS is not set; skipping ClickHouse server tests");
    }
    urls
}

fn session(url: &str) -> Session {
    Session::spawn(ClickHouseDriver::connect(url).expect("connect"))
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

/// Run `sql` and expect it to succeed.
fn run(session: &Session, sql: &str) -> ResultSet {
    let (result, done) = collect(session, sql, QueryOptions::default());
    match done {
        QueryEvent::Finished(_) => result.expect("a schema"),
        other => panic!("{} on {}: {other:?}", sql, session.flavor()),
    }
}

fn failure(session: &Session, sql: &str) -> String {
    match collect(session, sql, QueryOptions::default()).1 {
        QueryEvent::Failed { message, .. } => message,
        other => panic!("{sql} should fail on {}: {other:?}", session.flavor()),
    }
}

/// Wait for queries containing `marker` to leave `system.processes`.
fn wait_until_gone(session: &Session, marker: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let running = run(
            session,
            &format!(
                "SELECT count() FROM system.processes \
                 WHERE query LIKE '%{marker}%' AND query NOT LIKE '%system.processes%'"
            ),
        );
        if cell(&running, 0, 0) == "0" {
            return;
        }
        assert!(Instant::now() < deadline, "`{marker}` kept running");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn cell(result: &ResultSet, row: usize, col: usize) -> String {
    result.cell_value(row, col).unwrap_or_else(|| "NULL".into())
}

#[test]
fn connects_and_reports_the_version() {
    for url in urls() {
        let driver = ClickHouseDriver::connect(&url).unwrap();
        let flavor = driver.flavor();
        assert_eq!(flavor.dialect, quarry_core::Dialect::ClickHouse);
        let version = flavor.version.expect("a parsed version");
        assert!(version.major >= 20, "{flavor:?}");
        assert!(flavor.version_text.split('.').count() >= 3, "{flavor:?}");
        let target = driver.target();
        assert!(!target.contains("quarry"), "password in {target}");
        assert!(target.starts_with("default@localhost:"), "{target}");
    }
}

#[test]
fn reports_a_bad_password_clearly() {
    for url in urls() {
        let bad = url.replacen(":quarry@", ":wrong@", 1);
        let err = ClickHouseDriver::connect(&bad).err().expect("refused");
        let message = format!("{err:#}");
        // 23.8 says 516 (AUTHENTICATION_FAILED), later releases 194.
        assert!(message.contains("Authentication failed"), "{message}");
        assert!(!message.contains("(version"), "{message}");
        assert!(!message.contains("wrong"), "{message}");
    }
}

#[test]
fn streams_many_rows_across_batches() {
    for url in urls() {
        let session = session(&url);
        let started = Instant::now();
        let result = run(
            &session,
            "SELECT number, toString(number) AS s, if(number % 3 = 0, NULL, number * 2) AS n \
             FROM numbers(1000000)",
        );
        assert_eq!(result.row_count(), 1_000_000);
        assert!(result.batches().len() > 1, "{}", result.batches().len());
        assert_eq!(cell(&result, 765_432, 1), "765432");
        assert_eq!(cell(&result, 765_432, 2), "NULL");
        assert_eq!(cell(&result, 765_433, 2), "1530866");
        assert_eq!(result.column_type(1), &DataType::Utf8);
        assert_eq!(result.column_type_label(0), "UInt64");
        assert_eq!(result.column_type_label(2), "Nullable(UInt64)");
        eprintln!("{}: 1M rows in {:?}", session.flavor(), started.elapsed());
    }
}

#[test]
fn the_first_batch_arrives_before_the_query_ends() {
    for url in urls() {
        let session = session(&url);
        // Ten blocks, each taking half a second.
        let query = session.query(
            "SELECT number, sleepEachRow(0.05) FROM numbers(100) SETTINGS max_block_size = 10",
            QueryOptions::default(),
        );
        let started = Instant::now();
        let first = loop {
            match query.events.recv_timeout(Duration::from_secs(30)).unwrap() {
                QueryEvent::Batch(_) => break started.elapsed(),
                QueryEvent::Schema(_) => {}
                other => panic!("{other:?}"),
            }
        };
        assert!(
            first < Duration::from_secs(3),
            "first batch after {first:?}"
        );
        session.cancel();
    }
}

#[test]
fn stops_at_the_row_limit_and_keeps_working() {
    for url in urls() {
        let session = session(&url);
        let started = Instant::now();
        let (result, done) = collect(
            &session,
            "SELECT number FROM numbers(10000000000)",
            QueryOptions { max_rows: 1000 },
        );
        let QueryEvent::Finished(summary) = done else {
            panic!("{done:?}")
        };
        assert!(summary.truncated);
        assert_eq!(result.unwrap().row_count(), 1000);
        // The same session (and HTTP session) runs the next statement.
        let result = run(&session, "SELECT 42 AS answer");
        assert_eq!(cell(&result, 0, 0), "42");
        assert!(started.elapsed() < Duration::from_secs(20));
        // And the abandoned query does not go on running on the server.
        wait_until_gone(&session, "numbers(10000000000)");
    }
}

#[test]
fn shows_typed_columns_with_their_clickhouse_types() {
    for url in urls() {
        let session = session(&url);
        let flavor = session.flavor().clone();
        run(&session, "DROP TABLE IF EXISTS quarry_types");
        run(
            &session,
            "CREATE TABLE quarry_types (\
                id UInt32, \
                name Nullable(String), \
                tag LowCardinality(String), \
                at DateTime64(3, 'Asia/Seoul'), \
                ts DateTime('UTC'), \
                day Date, \
                price Decimal(18, 4), \
                uid UUID, \
                ip IPv4, \
                list Array(Nullable(Int32)), \
                attrs Map(String, UInt64), \
                pair Tuple(a UInt8, b String), \
                mood Enum8('happy' = 1, 'sad' = 2), \
                big Int128, \
                code FixedString(2)\
             ) ENGINE = MergeTree ORDER BY id",
        );
        let inserted = run(
            &session,
            "INSERT INTO quarry_types VALUES \
             (1, 'alpha', 'red', '2024-05-06 07:08:09.123', '2024-05-06 07:08:09', '2024-05-06', \
              12.3456, '61f0c404-5cb3-11e7-907b-a6006ad3dba0', '10.0.0.1', [1, NULL, 3], \
              {'k': 7}, (1, 'x'), 'sad', -170141183460469231731687303715884105728, 'KR'), \
             (2, NULL, 'blue', '2024-01-01 00:00:00', '1970-01-01 00:00:00', '1970-01-01', \
              0, '00000000-0000-0000-0000-000000000000', '255.255.255.255', [], \
              {}, (2, ''), 'happy', 0, 'U')",
        );
        assert_eq!(cell(&inserted, 0, 0), "2");
        assert_eq!(inserted.column_name(0), "rows affected");

        let result = run(&session, "SELECT * FROM quarry_types ORDER BY id");
        assert_eq!(result.row_count(), 2);
        let label = |name: &str| {
            let col = (0..result.column_count())
                .find(|&c| result.column_name(c) == name)
                .unwrap();
            result.column_type_label(col)
        };
        let value = |row: usize, name: &str| {
            let col = (0..result.column_count())
                .find(|&c| result.column_name(c) == name)
                .unwrap();
            cell(&result, row, col)
        };
        assert_eq!(label("name"), "Nullable(String)");
        assert_eq!(label("tag"), "LowCardinality(String)");
        assert_eq!(label("at"), "DateTime64(3, 'Asia/Seoul')");
        assert_eq!(label("price"), "Decimal(18, 4)");
        assert_eq!(label("mood"), "Enum8('happy' = 1, 'sad' = 2)");
        assert_eq!(label("list"), "Array(Nullable(Int32))");

        assert_eq!(value(0, "name"), "alpha");
        assert_eq!(value(1, "name"), "NULL");
        assert_eq!(value(0, "tag"), "red");
        assert_eq!(value(0, "at"), "2024-05-06T07:08:09.123+09:00");
        assert_eq!(value(0, "ts"), "2024-05-06T07:08:09Z");
        assert_eq!(value(0, "day"), "2024-05-06");
        assert_eq!(value(0, "price"), "12.3456");
        assert_eq!(value(0, "uid"), "61f0c404-5cb3-11e7-907b-a6006ad3dba0");
        assert_eq!(value(0, "ip"), "10.0.0.1");
        assert_eq!(value(0, "list"), "[1, , 3]");
        assert_eq!(value(0, "attrs"), "{k: 7}");
        assert_eq!(value(0, "pair"), "{a: 1, b: x}");
        assert_eq!(value(0, "mood"), "sad");
        assert_eq!(value(1, "mood"), "happy");
        assert_eq!(value(0, "big"), "-170141183460469231731687303715884105728");
        assert_eq!(value(0, "code"), "KR");
        assert_eq!(value(1, "code"), "U");

        // LowCardinality arrives as a dictionary only where that is safe.
        let tag = (0..result.column_count())
            .find(|&c| result.column_name(c) == "tag")
            .unwrap();
        let is_dictionary = matches!(result.column_type(tag), DataType::Dictionary(..));
        assert_eq!(
            is_dictionary,
            gates::lowcardinality_as_dictionary(&flavor),
            "{flavor}: {:?}",
            result.column_type(tag)
        );
        run(&session, "DROP TABLE quarry_types");
    }
}

#[test]
fn lowcardinality_values_survive_a_growing_dictionary() {
    // 23.8 writes dictionary indexes sized by the first block, corrupting
    // values once the dictionary grows; the gate keeps it off there.
    for url in urls() {
        let session = session(&url);
        let result = run(
            &session,
            "SELECT toLowCardinality(toString(if(number < 70000, number % 3, number % 70000))) AS lc \
             FROM numbers(200000)",
        );
        assert_eq!(result.row_count(), 200_000);
        for row in [0, 69_999, 70_000, 150_000, 199_999] {
            let expected = if row < 70_000 { row % 3 } else { row % 70_000 };
            assert_eq!(cell(&result, row, 0), expected.to_string(), "row {row}");
        }
    }
}

#[test]
fn falls_back_to_text_for_types_arrow_cannot_carry() {
    for url in urls() {
        let session = session(&url);
        let result = run(
            &session,
            "SELECT NULL AS nothing, toIntervalDay(3) AS days, [toDate('2024-01-02')] AS dates, \
                    toIPv6('::1') AS ip6, map('a', toUUID('61f0c404-5cb3-11e7-907b-a6006ad3dba0')) AS m",
        );
        assert_eq!(cell(&result, 0, 0), "NULL");
        assert_eq!(cell(&result, 0, 1), "3");
        assert!(
            cell(&result, 0, 2).contains("2024-01-02"),
            "{}",
            cell(&result, 0, 2)
        );
        assert_eq!(cell(&result, 0, 3), "::1");
        assert!(
            cell(&result, 0, 4).contains("61f0c404"),
            "{}",
            cell(&result, 0, 4)
        );
        assert_eq!(result.column_type_label(3), "IPv6");

        let flavor = session.flavor().clone();
        if flavor.at_least(24, 8) {
            // JSON, Dynamic and Variant: newer types, text on every release.
            let result = run(
                &session,
                "SELECT CAST('{\"a\": 1}', 'JSON') AS j, CAST(1, 'Dynamic') AS d, \
                        CAST('x', 'Variant(String, UInt8)') AS v \
                 SETTINGS allow_experimental_json_type = 1, allow_experimental_dynamic_type = 1, \
                          allow_experimental_variant_type = 1",
            );
            assert_eq!(cell(&result, 0, 0), "{\"a\":1}");
            assert_eq!(cell(&result, 0, 1), "1");
            assert_eq!(cell(&result, 0, 2), "x");
            assert_eq!(result.column_type_label(0), "JSON");
        }
    }
}

#[test]
fn keeps_the_order_of_a_rewritten_query() {
    for url in urls() {
        let session = session(&url);
        let result = run(
            &session,
            "SELECT toDate('2024-01-01') + number AS day, toUUID('00000000-0000-0000-0000-000000000000') AS u \
             FROM numbers(1000) ORDER BY day DESC",
        );
        assert_eq!(cell(&result, 0, 0), "2026-09-26");
        assert_eq!(cell(&result, 999, 0), "2024-01-01");
    }
}

#[test]
fn runs_ddl_and_insert() {
    for url in urls() {
        let session = session(&url);
        run(&session, "DROP TABLE IF EXISTS quarry_ddl");
        let created = run(
            &session,
            "CREATE TABLE quarry_ddl (id UInt64, note String) ENGINE = Memory",
        );
        assert_eq!(created.column_count(), 0);
        let inserted = run(
            &session,
            "INSERT INTO quarry_ddl SELECT number, toString(number) FROM numbers(12345)",
        );
        assert_eq!(cell(&inserted, 0, 0), "12345");
        assert_eq!(inserted.column_type_label(0), "UInt64");
        let count = run(&session, "SELECT count() FROM quarry_ddl;");
        assert_eq!(cell(&count, 0, 0), "12345");
        run(&session, "DROP TABLE quarry_ddl");
    }
}

#[test]
fn keeps_session_state_between_statements() {
    for url in urls() {
        let session = session(&url);
        run(&session, "SET max_threads = 3");
        let result = run(&session, "SELECT getSetting('max_threads')");
        assert_eq!(cell(&result, 0, 0), "3");
        run(&session, "CREATE TEMPORARY TABLE quarry_tmp (x UInt8)");
        run(&session, "INSERT INTO quarry_tmp VALUES (7)");
        assert_eq!(cell(&run(&session, "SELECT x FROM quarry_tmp"), 0, 0), "7");
        run(&session, "USE system");
        assert_eq!(
            cell(&run(&session, "SELECT currentDatabase()"), 0, 0),
            "system"
        );
        run(&session, "USE default");
    }
}

#[test]
fn shows_other_formats_as_text() {
    for url in urls() {
        let session = session(&url);
        let result = run(
            &session,
            "SELECT number, 'a,b' AS s FROM numbers(3) FORMAT CSV",
        );
        assert_eq!(result.column_count(), 1);
        assert_eq!(result.column_name(0), "CSV");
        assert_eq!(result.row_count(), 3);
        assert_eq!(cell(&result, 2, 0), "2,\"a,b\"");

        let result = run(&session, "SELECT 1 AS x FORMAT JSONEachRow");
        assert_eq!(cell(&result, 0, 0), "{\"x\":1}");

        // The Arrow file format is read as Arrow, not text.
        let result = run(&session, "SELECT number FROM numbers(5) FORMAT Arrow");
        assert_eq!(result.row_count(), 5);
        assert_eq!(cell(&result, 4, 0), "4");
    }
}

#[test]
fn reports_clean_errors_and_recovers() {
    for url in urls() {
        let session = session(&url);
        let message = failure(&session, "SELECT * FROM quarry_no_such_table");
        assert!(message.starts_with("Code: 60. DB::Exception:"), "{message}");
        assert!(message.ends_with("(UNKNOWN_TABLE)"), "{message}");
        assert!(!message.contains("version"), "{message}");
        assert!(!message.contains("Stack trace"), "{message}");

        let message = failure(&session, "SELEC 1");
        assert!(message.contains("SYNTAX_ERROR"), "{message}");

        // An exception thrown after part of the result was sent.
        let (_, done) = collect(
            &session,
            "SELECT number, throwIf(number = 150000, 'quarry boom') FROM numbers(1000000) \
             SETTINGS max_block_size = 10000",
            QueryOptions::default(),
        );
        let QueryEvent::Failed { message, .. } = done else {
            panic!("{done:?}")
        };
        assert!(message.starts_with("Code: 395."), "{message}");
        assert!(message.contains("quarry boom"), "{message}");
        assert!(!message.contains("__exception__"), "{message}");

        assert_eq!(cell(&run(&session, "SELECT 1 + 1"), 0, 0), "2");
    }
}

#[test]
fn cancels_a_running_query() {
    for url in urls() {
        let session = session(&url);
        let query = session.query(
            "SELECT sleepEachRow(0.5) FROM numbers(60) SETTINGS max_block_size = 1",
            QueryOptions::default(),
        );
        std::thread::sleep(Duration::from_millis(1200));
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
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );

        // The server stopped the query, and the session works again.
        let result = run(&session, "SELECT 'still here'");
        assert_eq!(cell(&result, 0, 0), "still here");
        wait_until_gone(&session, "sleepEachRow(0.5)");
    }
}

#[test]
fn reads_the_catalog() {
    for url in urls() {
        let session = session(&url);
        for sql in [
            "DROP DATABASE IF EXISTS quarry_cat",
            "DROP DATABASE IF EXISTS quarry_cat_empty",
            "CREATE DATABASE quarry_cat",
            "CREATE DATABASE quarry_cat_empty",
            "CREATE TABLE quarry_cat.events (id UInt64, kind LowCardinality(String), \
             note Nullable(String), at DateTime64(3, 'UTC')) ENGINE = MergeTree ORDER BY id",
            "INSERT INTO quarry_cat.events SELECT number, 'k', NULL, now() FROM numbers(42)",
            "CREATE VIEW quarry_cat.recent AS SELECT id, at FROM quarry_cat.events WHERE id > 10",
            "CREATE MATERIALIZED VIEW quarry_cat.per_kind ENGINE = SummingMergeTree ORDER BY kind \
             AS SELECT kind, count() AS n FROM quarry_cat.events GROUP BY kind",
        ] {
            run(&session, sql);
        }

        let catalog = session.catalog().recv().unwrap().unwrap();
        let names: Vec<_> = catalog.databases.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"default"), "{names:?}");
        assert!(!names.contains(&"system"), "{names:?}");
        assert!(
            !names
                .iter()
                .any(|n| n.eq_ignore_ascii_case("information_schema"))
        );
        let empty = catalog
            .databases
            .iter()
            .find(|d| d.name == "quarry_cat_empty")
            .expect("empty database listed");
        assert_eq!(empty.schemas.len(), 1);
        assert!(empty.schemas[0].relations.is_empty());

        let db = catalog
            .databases
            .iter()
            .find(|d| d.name == "quarry_cat")
            .unwrap();
        assert_eq!(db.schemas.len(), 1);
        assert_eq!(db.schemas[0].name, "");
        let relations = &db.schemas[0].relations;
        let names: Vec<_> = relations.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            ["events", "per_kind", "recent"],
            "inner tables hidden"
        );
        let events = &relations[0];
        assert_eq!(events.kind, RelationKind::Table);
        assert_eq!(events.estimated_rows, Some(42));
        let columns: Vec<_> = events
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.data_type.as_str(), c.nullable))
            .collect();
        assert_eq!(
            columns,
            [
                ("id", "UInt64", false),
                ("kind", "LowCardinality(String)", false),
                ("note", "Nullable(String)", true),
                ("at", "DateTime64(3, 'UTC')", false),
            ]
        );
        assert_eq!(relations[1].kind, RelationKind::MaterializedView);
        assert_eq!(relations[1].columns.len(), 2);
        assert_eq!(relations[2].kind, RelationKind::View);
        assert_eq!(relations[2].columns.len(), 2);

        run(&session, "DROP DATABASE quarry_cat");
        run(&session, "DROP DATABASE quarry_cat_empty");
    }
}

#[test]
fn sends_only_settings_the_server_knows() {
    let mut seen = Vec::new();
    for url in urls() {
        let session = session(&url);
        let flavor = session.flavor().clone();
        let settings = gates::output_settings(&flavor);
        for (name, _) in settings.iter().filter(|(n, _)| *n != "default_format") {
            let result = run(
                &session,
                &format!("SELECT count() FROM system.settings WHERE name = '{name}'"),
            );
            assert_eq!(cell(&result, 0, 0), "1", "{flavor} lacks {name}");
        }
        // Settings the gates hold back really are missing, or keep the
        // value Quarry wants by default.
        if !flavor.supports(gates::SAFE_ARROW_DICTIONARIES) {
            let result = run(
                &session,
                "SELECT count() FROM system.settings \
                 WHERE name = 'output_format_arrow_use_64_bit_indexes_for_dictionary'",
            );
            assert_eq!(cell(&result, 0, 0), "0", "{flavor}");
        }
        if !flavor.supports(gates::STRINGS_AS_ARROW_STRINGS) {
            let result = run(
                &session,
                "SELECT value FROM system.settings WHERE name = 'output_format_arrow_string_as_string'",
            );
            assert_eq!(cell(&result, 0, 0), "0", "{flavor}");
        }
        seen.push((flavor, settings.len()));
    }
    // Releases far apart get different settings.
    if seen.len() >= 2 {
        let (min, max) = (
            seen.iter().min_by_key(|(f, _)| f.version).unwrap(),
            seen.iter().max_by_key(|(f, _)| f.version).unwrap(),
        );
        if max.0.at_least(24, 3) && !min.0.at_least(24, 1) {
            assert!(max.1 > min.1, "{seen:?}");
        }
    }
}
