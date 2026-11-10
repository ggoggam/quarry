//! Integration tests against real MySQL and MariaDB servers.
//!
//! Set `QUARRY_TEST_MYSQL_URLS` to a comma-separated list of server URLs
//! (MySQL and MariaDB may be mixed); every case runs against each. Without
//! it the tests print a note and pass. Each case works in its own
//! `quarry_t_*` database, dropped and recreated first.
//!
//! ```sh
//! QUARRY_TEST_MYSQL_URLS=mysql://root:quarry@127.0.0.1:53384,mariadb://root:quarry@127.0.0.1:53311 \
//!     cargo test -p quarry-mysql
//! ```

use std::time::{Duration, Instant};

use quarry_core::arrow::datatypes::DataType;
use quarry_core::catalog::{Catalog, ColumnDef, RelationKind};
use quarry_core::result::field_type_label;
use quarry_core::{Dialect, Driver, QueryEvent, QueryOptions, ResultSet, Session, Version};
use quarry_mysql::MySqlDriver;
use quarry_mysql::flavor::supports_returning;

fn urls() -> Vec<String> {
    let urls: Vec<String> = std::env::var("QUARRY_TEST_MYSQL_URLS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_owned)
        .collect();
    if urls.is_empty() {
        eprintln!("QUARRY_TEST_MYSQL_URLS is not set; skipping MySQL/MariaDB server tests");
    }
    urls
}

/// A session on `url` using a fresh database `quarry_t_<name>`.
fn fresh(url: &str, name: &str) -> Session {
    let session = Session::spawn(MySqlDriver::connect(url).expect("connect"));
    let db = format!("quarry_t_{name}");
    ok(&session, &format!("DROP DATABASE IF EXISTS {db}"));
    ok(&session, &format!("CREATE DATABASE {db}"));
    ok(&session, &format!("USE {db}"));
    session
}

fn each(name: &str, test: impl Fn(&str, &Session)) {
    for url in urls() {
        let session = fresh(&url, name);
        eprintln!("{name}: {}", session.flavor());
        test(&url, &session);
    }
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

/// Run `sql`, which must succeed, and return its result.
fn ok(session: &Session, sql: &str) -> ResultSet {
    let (result, done) = collect(session, sql, QueryOptions::default());
    assert!(
        matches!(done, QueryEvent::Finished(_)),
        "{} {sql}: {done:?}",
        session.flavor()
    );
    result.expect("a schema")
}

fn cell(session: &Session, sql: &str) -> Option<String> {
    ok(session, sql).cell_text(0, 0)
}

/// `SELECT` of the numbers 0 .. 10^digits.
fn numbers(digits: u32) -> String {
    let ten = "(SELECT 0 n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 \
               UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 \
               UNION ALL SELECT 8 UNION ALL SELECT 9)";
    let expr = (0..digits)
        .map(|d| format!("d{d}.n * {}", 10u64.pow(d)))
        .collect::<Vec<_>>()
        .join(" + ");
    let from = (0..digits)
        .map(|d| format!("{ten} d{d}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("SELECT {expr} AS i, CONCAT('row ', {expr}) AS label FROM {from}")
}

fn catalog(session: &Session) -> Catalog {
    session.catalog().recv().unwrap().unwrap()
}

#[test]
fn detects_the_flavor() {
    for url in urls() {
        let driver = MySqlDriver::connect(&url).unwrap();
        let flavor = driver.flavor();
        assert!(!driver.target().contains("quarry"), "{}", driver.target());
        assert!(
            driver.target().starts_with("root@127.0.0.1:"),
            "{}",
            driver.target()
        );

        let session = Session::spawn(driver);
        let version = cell(&session, "SELECT VERSION()").unwrap();
        let mariadb = version.contains("MariaDB");
        assert_eq!(
            flavor.dialect,
            if mariadb {
                Dialect::MariaDb
            } else {
                Dialect::MySql
            },
            "{version}"
        );
        assert!(
            version.starts_with(&flavor.version_text),
            "{version} vs {flavor:?}"
        );
        assert!(!flavor.version_text.contains('-'), "{flavor:?}");
        let major = flavor.version.unwrap().major;
        assert!(if mariadb { major >= 10 } else { major >= 8 }, "{flavor:?}");
    }
}

#[test]
fn streams_many_rows_in_batches() {
    each("stream", |_, session| {
        let (result, done) = collect(session, &numbers(5), QueryOptions::default());
        let QueryEvent::Finished(summary) = done else {
            panic!("{done:?}")
        };
        let result = result.unwrap();
        assert_eq!(summary.rows, 100_000);
        assert!(!summary.truncated);
        assert_eq!(result.row_count(), 100_000);
        assert!(
            result.batches()[0].num_rows() <= 100,
            "first batch is small"
        );
        assert!(
            result.batches().len() > 20,
            "{} batches",
            result.batches().len()
        );
        assert!(result.batches().iter().all(|b| b.num_rows() <= 4096));
        let mut seen: Vec<i64> = (0..result.row_count())
            .map(|row| result.cell_text(row, 0).unwrap().parse().unwrap())
            .collect();
        seen.sort_unstable();
        assert_eq!(seen.first(), Some(&0));
        assert_eq!(seen.last(), Some(&99_999));
        assert_eq!(result.column_name(1), "label");
    });
}

#[test]
fn stops_at_the_row_limit_and_keeps_the_connection() {
    each("limit", |_, session| {
        ok(session, "SET @marker = 7");
        let (result, done) = collect(session, &numbers(5), QueryOptions { max_rows: 3000 });
        let QueryEvent::Finished(summary) = done else {
            panic!("{done:?}")
        };
        assert!(summary.truncated);
        assert_eq!(result.unwrap().row_count(), 3000);

        // Ten million rows: draining would take seconds, so the driver
        // kills the rest; the session's state survives.
        let started = Instant::now();
        let (result, done) = collect(session, &numbers(7), QueryOptions { max_rows: 10 });
        let QueryEvent::Finished(summary) = done else {
            panic!("{done:?}")
        };
        assert!(summary.truncated);
        assert_eq!(result.unwrap().row_count(), 10);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(cell(session, "SELECT @marker").as_deref(), Some("7"));
        assert_eq!(cell(session, "SELECT 42").as_deref(), Some("42"));
    });
}

#[test]
fn a_result_of_exactly_the_limit_is_not_truncated() {
    each("exact", |_, session| {
        let (result, done) = collect(session, &numbers(3), QueryOptions { max_rows: 1000 });
        let QueryEvent::Finished(summary) = done else {
            panic!("{done:?}")
        };
        assert!(!summary.truncated);
        assert_eq!(result.unwrap().row_count(), 1000);
    });
}

#[test]
fn types_columns_and_names_native_types() {
    each("types", |_, session| {
        // Zero dates need a permissive sql_mode; the driver leaves it alone.
        ok(session, "SET SESSION sql_mode = ''");
        ok(
            session,
            "CREATE TABLE t (
                ti TINYINT, tu TINYINT UNSIGNED, si SMALLINT, mi MEDIUMINT, i INT,
                iu INT UNSIGNED, bi BIGINT, bu BIGINT UNSIGNED, flag BOOLEAN,
                f FLOAT, d DOUBLE, dec10 DECIMAL(10,2), dec65 DECIMAL(65,4),
                dt DATE, dtm DATETIME(6), ts TIMESTAMP NULL, tm TIME, y YEAR, b BIT(10),
                c CHAR(3), vc VARCHAR(20), tx TEXT, bin BINARY(2), vb VARBINARY(10),
                bl BLOB, j JSON, e ENUM('small','large'), st SET('a','b'), g GEOMETRY
            )",
        );
        ok(
            session,
            "INSERT INTO t VALUES (
                -128, 255, -32768, -8388608, -2147483648, 4294967295,
                -9223372036854775808, 18446744073709551615, TRUE,
                1.5, -2.25, -12345678.90, 123456789012345678901234567890123456789012345.6789,
                '2024-02-29', '2024-05-06 07:08:09.123456', '2001-02-03 04:05:06', '-838:59:59',
                2024, b'1000000001', 'abc', 'héllo', 'long text', 0x00FF, 0xCAFE,
                0xDEADBEEF, '{\"k\": [1, 2]}', 'large', 'a,b', ST_GeomFromText('POINT(1 2)')
            )",
        );
        ok(session, "INSERT INTO t () VALUES ()");
        let result = ok(session, "SELECT * FROM t");
        assert_eq!(result.row_count(), 2);

        let column = |name: &str| {
            (0..result.column_count())
                .find(|&c| result.column_name(c) == name)
                .unwrap_or_else(|| panic!("no column {name}"))
        };
        let check = |name: &str, native: &str, arrow: DataType, text: &str| {
            let c = column(name);
            assert_eq!(result.column_type_label(c), native, "{name}");
            assert_eq!(result.column_type(c), &arrow, "{name}");
            assert_eq!(result.batches()[0].column(c).data_type(), &arrow, "{name}");
            assert_eq!(result.cell_text(0, c).as_deref(), Some(text), "{name}");
            assert!(result.is_null(1, c), "{name} is NULL in the empty row");
        };
        let maria = session.flavor().dialect == Dialect::MariaDb;
        check("ti", "tinyint", DataType::Int8, "-128");
        check("tu", "tinyint unsigned", DataType::UInt8, "255");
        check("si", "smallint", DataType::Int16, "-32768");
        check("mi", "mediumint", DataType::Int32, "-8388608");
        check("i", "int", DataType::Int32, "-2147483648");
        check("iu", "int unsigned", DataType::UInt32, "4294967295");
        check("bi", "bigint", DataType::Int64, "-9223372036854775808");
        check(
            "bu",
            "bigint unsigned",
            DataType::UInt64,
            "18446744073709551615",
        );
        check("flag", "tinyint(1)", DataType::Int8, "1");
        check("f", "float", DataType::Float32, "1.5");
        check("d", "double", DataType::Float64, "-2.25");
        check(
            "dec10",
            "decimal(10,2)",
            DataType::Decimal128(10, 2),
            "-12345678.90",
        );
        check(
            "dec65",
            "decimal(65,4)",
            DataType::Utf8,
            "123456789012345678901234567890123456789012345.6789",
        );
        check("dt", "date", DataType::Date32, "2024-02-29");
        check(
            "dtm",
            "datetime(6)",
            DataType::Timestamp(quarry_core::arrow::datatypes::TimeUnit::Microsecond, None),
            "2024-05-06T07:08:09.123456",
        );
        check(
            "ts",
            "timestamp",
            DataType::Timestamp(quarry_core::arrow::datatypes::TimeUnit::Microsecond, None),
            "2001-02-03T04:05:06",
        );
        check("tm", "time", DataType::Utf8, "-838:59:59");
        check("y", "year", DataType::UInt16, "2024");
        check("b", "bit(10)", DataType::UInt64, "513");
        check("c", "char", DataType::Utf8, "abc");
        check("vc", "varchar", DataType::Utf8, "héllo");
        check("tx", "text", DataType::Utf8, "long text");
        check("bin", "binary", DataType::Binary, "00ff");
        check("vb", "varbinary", DataType::Binary, "cafe");
        check("bl", "blob", DataType::Binary, "deadbeef");
        check("e", "enum", DataType::Utf8, "large");
        check("st", "set", DataType::Utf8, "a,b");
        check("g", "geometry", DataType::Utf8, "POINT(1 2)");
        // MariaDB's JSON is LONGTEXT on the wire; the catalog names it
        // (see `json_is_labelled_on_every_flavor`).
        let j = column("j");
        assert_eq!(
            result.column_type_label(j),
            if maria { "longtext" } else { "json" }
        );
        assert!(result.cell_text(0, j).unwrap().contains("\"k\""));

        // Zero dates come through as text in their batch.
        ok(session, "CREATE TABLE z (dt DATE, dtm DATETIME(6))");
        ok(
            session,
            "INSERT INTO z VALUES ('0000-00-00', '0000-00-00 00:00:00'), ('2024-01-02', NULL)",
        );
        let zero = ok(session, "SELECT * FROM z");
        assert_eq!(zero.cell_text(0, 0).as_deref(), Some("0000-00-00"));
        assert_eq!(
            zero.cell_text(0, 1).as_deref(),
            Some("0000-00-00 00:00:00.000000")
        );
        assert_eq!(zero.cell_text(1, 0).as_deref(), Some("2024-01-02"));
        assert!(zero.is_null(1, 1));
        assert_eq!(zero.column_type(0), &DataType::Date32);
        let batch = &zero.batches()[0];
        assert_eq!(batch.column(0).data_type(), &DataType::Utf8);
        assert_eq!(
            field_type_label(batch.schema().field(0)),
            "date",
            "the fallback keeps the native type"
        );

        // Expressions are typed by the server too.
        let result = ok(
            session,
            "SELECT 1 AS one, 1.5 AS num, NOW(6) AS now, NULL AS nothing, 'x' AS s",
        );
        // MySQL types a literal 1 as BIGINT, MariaDB as INT.
        assert_eq!(
            result.column_type(0),
            if maria {
                &DataType::Int32
            } else {
                &DataType::Int64
            }
        );
        assert_eq!(result.column_type_label(1), "decimal(2,1)");
        assert_eq!(result.column_type_label(2), "datetime(6)");
        assert_eq!(result.column_type_label(3), "null");
        assert!(result.is_null(0, 3));
        assert_eq!(result.column_type_label(4), "varchar");
    });
}

#[test]
fn runs_ddl_and_dml() {
    each("dml", |_, session| {
        let created = ok(
            session,
            "CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(10))",
        );
        assert_eq!(created.column_count(), 0);

        let inserted = ok(session, "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')");
        assert_eq!(inserted.column_name(0), "rows affected");
        assert_eq!(inserted.cell_text(0, 0).as_deref(), Some("3"));
        let updated = ok(session, "UPDATE t SET name = 'z' WHERE id >= 2");
        assert_eq!(updated.cell_text(0, 0).as_deref(), Some("2"));
        let replaced = ok(session, "REPLACE INTO t VALUES (1, 'q')");
        assert_eq!(replaced.cell_text(0, 0).as_deref(), Some("2"));
        let deleted = ok(session, "/* tidy */ DELETE FROM t WHERE id = 3");
        assert_eq!(deleted.cell_text(0, 0).as_deref(), Some("1"));

        assert_eq!(ok(session, "SET @x = 1").column_count(), 0);
        assert_eq!(ok(session, "DROP TABLE t").column_count(), 0);

        // A procedure returning two result sets shows the first.
        ok(
            session,
            "CREATE PROCEDURE two() BEGIN SELECT 1 AS a; SELECT 2 AS b, 3 AS c; END",
        );
        let first = ok(session, "CALL two()");
        assert_eq!(first.column_name(0), "a");
        assert_eq!(first.cell_text(0, 0).as_deref(), Some("1"));
        assert_eq!(
            cell(session, "SELECT 'still usable'").as_deref(),
            Some("still usable")
        );
    });
}

#[test]
fn reports_server_errors_and_keeps_working() {
    each("errors", |_, session| {
        ok(session, "CREATE TABLE t (id INT)");
        let (_, done) = collect(session, "SELECT nope FROM t", QueryOptions::default());
        let QueryEvent::Failed { message, .. } = done else {
            panic!("{done:?}")
        };
        assert!(
            message.starts_with("ERROR 1054 (42S22): Unknown column 'nope'"),
            "{message}"
        );

        let (_, done) = collect(session, "SELEC 1", QueryOptions::default());
        let QueryEvent::Failed { message, .. } = done else {
            panic!("{done:?}")
        };
        assert!(message.starts_with("ERROR 1064 (42000): "), "{message}");
        assert!(!message.contains("MySqlError"), "{message}");

        assert_eq!(
            cell(session, "SELECT COUNT(*) FROM t").as_deref(),
            Some("0")
        );
    });
}

#[test]
fn cancels_a_long_query() {
    each("cancel", |_, session| {
        let started = Instant::now();
        let query = session.query("SELECT SLEEP(30)", QueryOptions::default());
        std::thread::sleep(Duration::from_millis(500));
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
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(cell(session, "SELECT 42").as_deref(), Some("42"));
    });
}

#[test]
fn a_late_kill_does_not_hit_the_next_statement() {
    // What `drain` relies on: a KILL QUERY that reaches an idle connection
    // does not interrupt that connection's next statement.
    each("late_kill", |url, session| {
        let id = cell(session, "SELECT CONNECTION_ID()").unwrap();
        let other = Session::spawn(MySqlDriver::connect(url).unwrap());
        ok(&other, &format!("KILL QUERY {id}"));
        assert_eq!(cell(session, "SELECT 'fine'").as_deref(), Some("fine"));
    });
}

#[test]
fn reads_the_catalog() {
    each("catalog", |_, session| {
        ok(session, "DROP DATABASE IF EXISTS quarry_t_catalog_empty");
        ok(session, "CREATE DATABASE quarry_t_catalog_empty");
        ok(
            session,
            "CREATE TABLE orders (id BIGINT UNSIGNED NOT NULL PRIMARY KEY, \
             total DECIMAL(10,2), placed DATETIME(3) NULL, note TEXT)",
        );
        ok(
            session,
            "CREATE VIEW big AS SELECT id, total FROM orders WHERE total > 100",
        );
        ok(
            session,
            "INSERT INTO orders (id, total) VALUES (1, 5), (2, 500)",
        );

        let catalog = catalog(session);
        let names: Vec<_> = catalog
            .databases
            .iter()
            .map(|db| db.name.as_str())
            .collect();
        for hidden in ["mysql", "information_schema", "performance_schema", "sys"] {
            assert!(!names.contains(&hidden), "{names:?}");
        }
        let empty = catalog
            .databases
            .iter()
            .find(|db| db.name == "quarry_t_catalog_empty")
            .expect("empty database listed");
        assert_eq!(empty.schemas.len(), 1);
        assert!(empty.schemas[0].relations.is_empty());

        let db = catalog
            .databases
            .iter()
            .find(|db| db.name == "quarry_t_catalog")
            .unwrap();
        assert_eq!(db.schemas.len(), 1);
        assert_eq!(db.schemas[0].name, "");
        let relations = &db.schemas[0].relations;
        let names: Vec<_> = relations.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["big", "orders"]);
        let orders = &relations[1];
        assert_eq!(orders.kind, RelationKind::Table);
        let maria = session.flavor().dialect == Dialect::MariaDb;
        assert_eq!(
            orders.columns[0],
            ColumnDef {
                name: "id".into(),
                // MySQL 8.0.19+ dropped integer display widths; MariaDB
                // keeps them.
                data_type: if maria {
                    "bigint(20) unsigned".into()
                } else {
                    "bigint unsigned".into()
                },
                nullable: false,
            }
        );
        let types: Vec<_> = orders
            .columns
            .iter()
            .map(|c| c.data_type.as_str())
            .collect();
        assert_eq!(&types[1..], ["decimal(10,2)", "datetime(3)", "text"]);
        assert!(orders.columns[1].nullable);
        assert!(orders.estimated_rows.is_some());

        let big = &relations[0];
        assert_eq!(big.kind, RelationKind::View);
        assert_eq!(big.estimated_rows, None);
        assert_eq!(big.columns.len(), 2);

        ok(session, "DROP DATABASE quarry_t_catalog_empty");
    });
}

#[test]
fn json_is_labelled_on_every_flavor() {
    each("json", |_, session| {
        ok(
            session,
            "CREATE TABLE docs (id INT, doc JSON, body LONGTEXT)",
        );
        ok(session, "INSERT INTO docs VALUES (1, '{\"a\": 1}', 'x')");
        let catalog = catalog(session);
        let docs = &catalog
            .databases
            .iter()
            .find(|db| db.name == "quarry_t_json")
            .unwrap()
            .schemas[0]
            .relations[0];
        assert_eq!(docs.columns[1].data_type, "json");
        assert_eq!(docs.columns[2].data_type, "longtext");

        // After a catalog read, MariaDB's wire `longtext` is named too.
        let result = ok(session, "SELECT doc, body FROM docs");
        assert_eq!(result.column_type_label(0), "json");
        assert_eq!(result.column_type_label(1), "longtext");
    });
}

#[test]
fn mariadb_versioned_tables_sequences_and_temporaries() {
    each("versioned", |_, session| {
        let flavor = session.flavor().clone();
        if flavor.dialect != Dialect::MariaDb {
            return;
        }
        ok(session, "CREATE TABLE hist (id INT) WITH SYSTEM VERSIONING");
        ok(session, "CREATE SEQUENCE seq");
        ok(session, "CREATE TEMPORARY TABLE scratch (id INT)");
        let catalog = catalog(session);
        let relations = &catalog
            .databases
            .iter()
            .find(|db| db.name == "quarry_t_versioned")
            .unwrap()
            .schemas[0]
            .relations;
        let hist = relations.iter().find(|r| r.name == "hist").unwrap();
        assert_eq!(hist.kind, RelationKind::Table);
        assert!(
            relations.iter().all(|r| r.name != "seq"),
            "sequences are left out"
        );
        // MariaDB 11.2 lists the session's temporary tables.
        let lists_temporaries = flavor.supports(Version::new(11, 2, 0));
        assert_eq!(
            relations.iter().any(|r| r.name == "scratch"),
            lists_temporaries,
            "{flavor}"
        );
    });
}

#[test]
fn returning_streams_rows_where_supported() {
    each("returning", |_, session| {
        ok(session, "CREATE TABLE t (id INT)");
        ok(session, "INSERT INTO t VALUES (1), (2)");
        let flavor = session.flavor().clone();
        let (result, done) = collect(
            session,
            "DELETE FROM t WHERE id = 2 RETURNING id",
            QueryOptions::default(),
        );
        if supports_returning(&flavor, "DELETE") {
            assert!(matches!(done, QueryEvent::Finished(_)), "{done:?}");
            let result = result.unwrap();
            assert_eq!(result.column_name(0), "id");
            assert_eq!(result.cell_text(0, 0).as_deref(), Some("2"));
        } else {
            assert!(
                matches!(done, QueryEvent::Failed { .. }),
                "{flavor}: {done:?}"
            );
        }
    });
}

#[test]
fn mysql_8_row_counts_are_live_despite_the_stats_cache() {
    each("stats", |_, session| {
        let flavor = session.flavor().clone();
        ok(session, "CREATE TABLE t (id INT PRIMARY KEY)");
        let cached_rows = "SELECT TABLE_ROWS FROM information_schema.TABLES \
                           WHERE TABLE_SCHEMA = 'quarry_t_stats' AND TABLE_NAME = 't'";
        // Look the empty table up once, so MySQL 8 caches its statistics.
        let before = cell(session, cached_rows);
        assert!(
            before.as_deref() == Some("0") || before.is_none(),
            "{before:?}"
        );
        ok(
            session,
            &format!("INSERT INTO t SELECT i FROM ({}) n", numbers(3)),
        );
        // Wait for InnoDB's background statistics to see the rows.
        let deadline = Instant::now() + Duration::from_secs(20);
        let live = loop {
            let catalog = catalog(session);
            let rows = catalog
                .databases
                .iter()
                .find(|db| db.name == "quarry_t_stats")
                .unwrap()
                .schemas[0]
                .relations[0]
                .estimated_rows;
            if rows.is_some_and(|n| n > 0) || Instant::now() > deadline {
                break rows;
            }
            std::thread::sleep(Duration::from_millis(500));
        };
        assert!(live.is_some_and(|n| n > 0), "{flavor}: {live:?}");
        // The catalog put the session's cache setting back.
        if quarry_mysql::flavor::caches_table_stats(&flavor) {
            assert_eq!(
                cell(session, "SELECT @@SESSION.information_schema_stats_expiry").as_deref(),
                Some("86400")
            );
            // Without the catalog's override, MySQL 8 still serves the
            // count it cached for the empty table.
            assert_eq!(cell(session, cached_rows).as_deref(), Some("0"), "{flavor}");
        }
    });
}

#[test]
fn honours_ssl_modes() {
    for url in urls() {
        let with = |mode: &str| {
            let separator = if url.contains('?') { '&' } else { '?' };
            MySqlDriver::connect(&format!("{url}{separator}ssl-mode={mode}"))
        };
        let cipher = |driver: MySqlDriver| {
            let session = Session::spawn(driver);
            ok(&session, "SHOW SESSION STATUS LIKE 'Ssl_cipher'")
                .cell_text(0, 1)
                .unwrap_or_default()
        };
        let preferred = MySqlDriver::connect(&url).unwrap();
        let target = preferred.target();
        let preferred = cipher(preferred);
        assert_eq!(cipher(with("disabled").unwrap()), "");
        match with("required") {
            Ok(driver) => {
                let required = cipher(driver);
                assert!(!required.is_empty(), "{target}");
                assert_eq!(preferred, required, "preferred uses TLS when offered");
            }
            Err(err) => {
                assert!(err.to_string().contains("TLS"), "{err:#}");
                assert_eq!(preferred, "", "preferred falls back to plain TCP");
            }
        }
        // The server's self-signed certificate is not trusted.
        assert!(with("verify_identity").is_err(), "{target}");
        eprintln!("{target}: TLS cipher {preferred:?}");
    }
}

#[test]
fn reconnects_after_the_connection_drops() {
    each("reconnect", |url, session| {
        let id = cell(session, "SELECT CONNECTION_ID()").unwrap();
        let other = Session::spawn(MySqlDriver::connect(url).unwrap());
        ok(&other, &format!("KILL CONNECTION {id}"));
        // The statement that finds the connection gone fails ...
        let (_, done) = collect(session, "SELECT 1", QueryOptions::default());
        let QueryEvent::Failed { message, .. } = done else {
            panic!("{done:?}")
        };
        assert!(message.starts_with("the connection was lost"), "{message}");
        // ... and the next one runs on a new connection, in the database
        // chosen with USE, which cancels too.
        let new_id = cell(session, "SELECT CONNECTION_ID()").unwrap();
        assert_ne!(new_id, id);
        assert_eq!(
            cell(session, "SELECT DATABASE()").as_deref(),
            Some("quarry_t_reconnect")
        );
        let query = session.query("SELECT SLEEP(30)", QueryOptions::default());
        std::thread::sleep(Duration::from_millis(300));
        session.cancel();
        let started = Instant::now();
        while !matches!(
            query
                .events
                .recv_timeout(Duration::from_secs(10))
                .expect("cancelled"),
            QueryEvent::Finished(_) | QueryEvent::Failed { .. }
        ) {}
        assert!(started.elapsed() < Duration::from_secs(10));
    });
}
