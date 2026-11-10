//! Completion end to end: SQL with a caret in, items out.

use std::time::Instant;

use super::*;
use crate::catalog::{Catalog, ColumnDef, Database, Relation, RelationKind, Schema};
use crate::dialect::Dialect;

/// Marks the caret in test SQL.
const CARET: char = '‸';

fn column(name: &str, data_type: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_owned(),
        data_type: data_type.to_owned(),
        nullable: true,
    }
}

fn relation(name: &str, kind: RelationKind, columns: &[(&str, &str)]) -> Relation {
    Relation {
        name: name.to_owned(),
        kind,
        columns: columns.iter().map(|(n, t)| column(n, t)).collect(),
        estimated_rows: None,
    }
}

fn table(name: &str, columns: &[(&str, &str)]) -> Relation {
    relation(name, RelationKind::Table, columns)
}

fn schema(name: &str, relations: Vec<Relation>) -> Schema {
    Schema {
        name: name.to_owned(),
        relations,
    }
}

fn database(name: &str, schemas: Vec<Schema>) -> Database {
    Database {
        name: name.to_owned(),
        schemas,
    }
}

/// An in-memory DuckDB with a shop in `main` and reports in `analytics`.
fn shop() -> Catalog {
    Catalog {
        databases: vec![database(
            "memory",
            vec![
                schema(
                    "main",
                    vec![
                        table(
                            "orders",
                            &[
                                ("id", "INTEGER"),
                                ("customer_id", "INTEGER"),
                                ("amount", "DECIMAL(10,2)"),
                                ("status", "VARCHAR"),
                                ("created", "DATE"),
                            ],
                        ),
                        table(
                            "customers",
                            &[("id", "INTEGER"), ("name", "VARCHAR"), ("Email", "VARCHAR")],
                        ),
                        table(
                            "order_items",
                            &[
                                ("order_id", "INTEGER"),
                                ("product", "VARCHAR"),
                                ("qty", "INTEGER"),
                            ],
                        ),
                        table("Order Archive", &[("id", "INTEGER")]),
                        table("user", &[("id", "INTEGER"), ("group", "VARCHAR")]),
                        relation(
                            "active_customers",
                            RelationKind::View,
                            &[("id", "INTEGER"), ("name", "VARCHAR")],
                        ),
                    ],
                ),
                schema(
                    "analytics",
                    vec![table(
                        "daily_sales",
                        &[("day", "DATE"), ("total", "DECIMAL(18,2)")],
                    )],
                ),
            ],
        )],
    }
}

fn duckdb() -> Flavor {
    Flavor::new(Dialect::DuckDb, "1.4.1")
}

/// Complete `sql`, whose caret is marked with [`CARET`].
fn run(sql: &str, flavor: &Flavor, catalog: &CatalogIndex, trigger: Trigger) -> Completions {
    let caret = sql.find(CARET).expect("the test SQL marks its caret");
    let text = sql.replace(CARET, "");
    complete(&text, caret, flavor, catalog, trigger)
}

/// Asked for in the shop catalog, on DuckDB.
fn ask(sql: &str) -> Vec<Item> {
    let catalog = CatalogIndex::new(&shop(), Dialect::DuckDb);
    run(sql, &duckdb(), &catalog, Trigger::Invoked).items
}

fn labels(items: &[Item]) -> Vec<&str> {
    items.iter().map(|i| i.label.as_str()).collect()
}

fn inserts(items: &[Item]) -> Vec<&str> {
    items.iter().map(|i| i.insert.as_str()).collect()
}

fn of_kind(items: &[Item], kind: ItemKind) -> Vec<&str> {
    items
        .iter()
        .filter(|i| i.kind == kind)
        .map(|i| i.label.as_str())
        .collect()
}

fn position_of(items: &[Item], label: &str, kind: ItemKind) -> usize {
    items
        .iter()
        .position(|i| i.label.eq_ignore_ascii_case(label) && i.kind == kind)
        .unwrap_or_else(|| panic!("no {kind:?} {label} in {:?}", labels(items)))
}

// ── Relations ───────────────────────────────────────────────────────────────

#[test]
fn relations_come_first_after_from() {
    let items = ask("SELECT * FROM ord‸");
    assert_eq!(labels(&items)[..2], ["orders", "order_items"]);
    assert_eq!(items[0].kind, ItemKind::Table);
    assert_eq!(items[0].detail, "table · main");
    // The keyword `ORDER` matches too, but after every relation.
    let keyword = position_of(&items, "order", ItemKind::Keyword);
    assert!(keyword > position_of(&items, "Order Archive", ItemKind::Table));
}

#[test]
fn every_relation_keyword_offers_relations() {
    for sql in [
        "INSERT INTO cus‸",
        "UPDATE cus‸",
        "DELETE FROM cus‸",
        "TRUNCATE TABLE cus‸",
        "SELECT * FROM orders o JOIN cus‸",
        "SELECT * FROM orders o LEFT OUTER JOIN cus‸ ON true",
        "SELECT * FROM orders, cus‸",
        "SELECT * FROM orders o, cus‸ c WHERE o.id = c.id",
        "select * from (select * from cus‸) x",
        "SELECT * FROM ONLY cus‸",
        "DROP TABLE IF EXISTS cus‸",
    ] {
        let items = ask(sql);
        assert_eq!(
            items.first().map(|i| i.label.as_str()),
            Some("customers"),
            "{sql}"
        );
    }
}

#[test]
fn views_say_what_they_are() {
    let items = ask("SELECT * FROM active‸");
    assert_eq!(items[0].kind, ItemKind::View);
    assert_eq!(items[0].detail, "view · main");
}

#[test]
fn relations_outside_the_search_path_are_inserted_qualified() {
    let items = ask("SELECT * FROM dai‸");
    assert_eq!(items[0].label, "daily_sales");
    assert_eq!(items[0].insert, "analytics.daily_sales");
    assert_eq!(items[0].detail, "table · analytics");
    // `main` is searched, so its tables go in bare.
    assert_eq!(ask("SELECT * FROM orde‸")[0].insert, "orders");
}

#[test]
fn schemas_qualify_relations() {
    let items = ask("SELECT * FROM analytics.‸");
    assert_eq!(labels(&items), ["daily_sales"]);
    // Already qualified: the name goes in alone.
    assert_eq!(items[0].insert, "daily_sales");
    assert_eq!(labels(&ask("SELECT * FROM analytics.da‸")), ["daily_sales"]);
    assert!(ask("SELECT * FROM analytics.ord‸").is_empty());
    // Schemas are offered where a relation goes, to qualify one.
    let items = ask("SELECT * FROM analy‸");
    assert_eq!(items[0].kind, ItemKind::Schema);
}

#[test]
fn a_database_qualifier_lists_its_schemas_and_default_tables() {
    let items = ask("SELECT * FROM memory.‸");
    let schemas = of_kind(&items, ItemKind::Schema);
    assert!(
        schemas.contains(&"main") && schemas.contains(&"analytics"),
        "{schemas:?}"
    );
    assert!(of_kind(&items, ItemKind::Table).contains(&"orders"));
    assert!(!of_kind(&items, ItemKind::Table).contains(&"daily_sales"));
    assert_eq!(
        labels(&ask("SELECT * FROM memory.analytics.‸")),
        ["daily_sales"]
    );
}

#[test]
fn ctes_are_relations_too() {
    let items = ask("WITH recent AS (SELECT * FROM orders), big AS (SELECT 1) SELECT * FROM re‸");
    assert_eq!(items[0].label, "recent");
    assert_eq!(items[0].kind, ItemKind::Cte);
    let items = ask("WITH recent (a, b) AS MATERIALIZED (SELECT 1, 2) SELECT * FROM ‸");
    assert_eq!(items[0].label, "recent");
}

// ── Columns ─────────────────────────────────────────────────────────────────

#[test]
fn alias_dot_lists_that_tables_columns() {
    let expected = ["id", "customer_id", "amount", "status", "created"];
    for sql in [
        "SELECT o.‸ FROM orders o",
        "SELECT o.‸ FROM orders AS o",
        "SELECT orders.‸ FROM orders",
        "SELECT o.‸ FROM main.orders o JOIN customers c ON c.id = o.customer_id",
        "SELECT * FROM orders o WHERE o.‸",
        "SELECT * FROM orders o JOIN customers c ON c.id = o.‸",
        "SELECT x.‸ FROM memory.main.orders AS x",
    ] {
        assert_eq!(labels(&ask(sql)), expected, "{sql}");
    }
    let items = ask("SELECT c.na‸ FROM customers c");
    assert_eq!(labels(&items), ["name"]);
    assert_eq!(items[0].kind, ItemKind::Column);
    assert_eq!(items[0].detail, "VARCHAR · customers c");
    assert_eq!(items[0].insert, "name");
}

#[test]
fn a_qualified_table_lists_its_columns_without_a_from() {
    assert_eq!(labels(&ask("SELECT customers.‸")), ["id", "name", "Email"]);
    assert_eq!(
        labels(&ask("SELECT analytics.daily_sales.‸")),
        ["day", "total"]
    );
}

#[test]
fn expressions_offer_the_statements_columns_first() {
    let items = ask("SELECT ‸ FROM orders o JOIN customers c ON o.customer_id = c.id");
    let columns: Vec<_> = items
        .iter()
        .take_while(|i| i.kind == ItemKind::Column)
        .map(|i| i.insert.as_str())
        .collect();
    // In statement order; a name both tables have needs its alias.
    assert_eq!(
        columns,
        [
            "o.id",
            "customer_id",
            "amount",
            "status",
            "created",
            "c.id",
            "name",
            "\"Email\""
        ]
    );
    assert_eq!(items[0].detail, "INTEGER · orders o");
    // Then the aliases, then functions, then keywords.
    let alias = position_of(&items, "o", ItemKind::Alias);
    assert_eq!(items[alias].detail, "alias · orders");
    assert!(alias > position_of(&items, "Email", ItemKind::Column));
    let function = items
        .iter()
        .position(|i| i.kind == ItemKind::Function)
        .unwrap();
    let keyword = items.iter().position(|i| i.kind == ItemKind::Keyword);
    assert!(function > alias);
    assert!(keyword.is_none_or(|k| k > function));
}

#[test]
fn every_expression_clause_offers_columns() {
    for (sql, first) in [
        ("SELECT * FROM orders WHERE am‸", "amount"),
        ("SELECT * FROM orders WHERE amount > 1 AND sta‸", "status"),
        (
            "SELECT status, count(*) FROM orders GROUP BY sta‸",
            "status",
        ),
        ("SELECT * FROM orders ORDER BY cre‸ DESC", "created"),
        (
            "SELECT * FROM orders o JOIN customers c ON c.id = cust‸",
            "customer_id",
        ),
        (
            "SELECT status FROM orders GROUP BY 1 HAVING sum(amo‸",
            "amount",
        ),
        ("UPDATE orders SET sta‸", "status"),
        ("UPDATE orders SET status = 'x' WHERE amo‸", "amount"),
        ("DELETE FROM orders WHERE cre‸", "created"),
        ("SELECT * FROM orders WHERE (amount > 1 OR (sta‸", "status"),
        (
            "SELECT * FROM orders WHERE id IN (SELECT order_id FROM order_items WHERE pro‸)",
            "product",
        ),
        ("SELECT sum(amo‸) FROM orders", "amount"),
    ] {
        let items = ask(sql);
        assert_eq!(
            items.first().map(|i| i.label.as_str()),
            Some(first),
            "{sql}"
        );
        assert_eq!(items[0].kind, ItemKind::Column, "{sql}");
    }
}

#[test]
fn subqueries_see_their_own_tables_then_the_outer_ones() {
    let items = ask("SELECT * FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE ‸)");
    let columns = of_kind(&items, ItemKind::Column);
    assert_eq!(columns[0], "id");
    assert!(columns.contains(&"customer_id") && columns.contains(&"name"));
    assert!(items[0].detail.ends_with("orders o"));
    // The outer query does not see into the subquery.
    let items = ask("SELECT ‸ FROM customers WHERE id IN (SELECT customer_id FROM orders)");
    assert!(!of_kind(&items, ItemKind::Column).contains(&"amount"));
}

#[test]
fn self_joins_qualify_every_column() {
    let items = ask("SELECT ‸ FROM orders a JOIN orders b ON a.id = b.id");
    assert_eq!(items[0].insert, "a.id");
    assert!(inserts(&items).contains(&"b.status"));
}

#[test]
fn insert_column_lists_offer_the_targets_columns() {
    let items = ask("INSERT INTO orders (‸");
    assert_eq!(
        labels(&items),
        ["id", "customer_id", "amount", "status", "created"]
    );
    assert_eq!(
        labels(&ask("INSERT INTO orders (id, am‸) VALUES (1, 2)")),
        ["amount"]
    );
}

// ── Words ───────────────────────────────────────────────────────────────────

#[test]
fn keywords_follow_the_case_being_typed() {
    assert_eq!(ask("sel‸")[0].insert, "select");
    assert_eq!(ask("SEL‸")[0].insert, "SELECT");
    assert_eq!(ask("Sel‸")[0].insert, "SELECT");
    // Nothing typed yet: the case of the keywords before.
    let after = |sql| {
        ask(sql)
            .into_iter()
            .find(|i| i.kind == ItemKind::Keyword)
            .unwrap()
            .insert
    };
    assert_eq!(after("select * from orders ‸"), "where");
    assert_eq!(after("SELECT * FROM orders ‸"), "WHERE");
    // And upper case when there are none.
    assert_eq!(after("‸"), "SELECT");
    // Functions keep their lower case unless typed otherwise.
    let items = ask("SELECT coun‸");
    assert_eq!(items[0].insert, "count");
    assert_eq!(items[0].kind, ItemKind::Function);
    assert_eq!(ask("SELECT COUN‸")[0].insert, "COUNT");
}

#[test]
fn engine_spellings_are_kept() {
    let flavor = Flavor::new(Dialect::ClickHouse, "24.8");
    let catalog = CatalogIndex::default();
    let items = run("SELECT todate‸", &flavor, &catalog, Trigger::Invoked).items;
    assert!(inserts(&items).contains(&"toDate"), "{:?}", inserts(&items));
    let items = run("SELECT x::uint6‸", &flavor, &catalog, Trigger::Invoked).items;
    assert_eq!(items[0].insert, "UInt64");
}

#[test]
fn statement_starts_lead_at_the_start() {
    let items = ask("‸");
    assert!(items.iter().all(|i| i.kind == ItemKind::Keyword));
    assert_eq!(items[0].insert, "SELECT");
    let items = ask("SELECT 1; S‸");
    assert_eq!(items[0].insert, "SELECT");
}

#[test]
fn keywords_follow_a_relation() {
    let items = ask("SELECT * FROM orders o ‸");
    assert!(items.iter().all(|i| i.kind == ItemKind::Keyword));
    assert_eq!(items[0].insert, "WHERE");
    assert_eq!(ask("SELECT * FROM orders o LEFT J‸")[0].insert, "JOIN");
    assert_eq!(ask("SELECT * FROM orders WH‸")[0].insert, "WHERE");
}

#[test]
fn types_follow_casts() {
    let items = ask("SELECT amount::VARC‸ FROM orders");
    assert_eq!(items[0].insert, "VARCHAR");
    assert_eq!(items[0].kind, ItemKind::Type);
    assert_eq!(
        ask("SELECT CAST(amount AS dou‸) FROM orders")[0].insert,
        "double"
    );
    let items = ask("CREATE TABLE t (id INT‸");
    assert_eq!(items[0].kind, ItemKind::Type);
}

#[test]
fn new_names_get_nothing() {
    for sql in [
        "SELECT amount AS ‸ FROM orders",
        "SELECT * FROM orders AS ‸",
        "CREATE TABLE ‸",
        "CREATE TABLE IF NOT EXISTS ‸",
        "CREATE TABLE t (‸",
        "CREATE TABLE t (id INTEGER, ‸",
    ] {
        assert!(ask(sql).is_empty(), "{sql}: {:?}", labels(&ask(sql)));
    }
}

#[test]
fn words_respect_the_release() {
    let catalog = CatalogIndex::default();
    let pg14 = Flavor::new(Dialect::Postgres, "14.12");
    let pg15 = Flavor::new(Dialect::Postgres, "15.4");
    let sql = "SELECT regexp_cou‸";
    assert!(run(sql, &pg14, &catalog, Trigger::Invoked).items.is_empty());
    let items = run(sql, &pg15, &catalog, Trigger::Invoked).items;
    assert_eq!(items[0].insert, "regexp_count");
}

#[test]
fn words_complete_without_a_catalog() {
    let catalog = CatalogIndex::default();
    assert!(catalog.is_empty());
    let flavor = duckdb();
    let items = run(
        "SELECT * FROM orders WHERE coal‸",
        &flavor,
        &catalog,
        Trigger::Invoked,
    )
    .items;
    assert_eq!(items[0].insert, "coalesce");
    let items = run("sel‸", &flavor, &catalog, Trigger::Invoked).items;
    assert_eq!(items[0].insert, "select");
    assert!(
        run(
            "SELECT o.‸ FROM orders o",
            &flavor,
            &catalog,
            Trigger::Invoked
        )
        .items
        .is_empty()
    );
}

// ── Quoting ─────────────────────────────────────────────────────────────────

#[test]
fn names_are_quoted_as_the_dialect_needs() {
    assert_eq!(ask("SELECT * FROM archi‸")[0].insert, "\"Order Archive\"");
    assert_eq!(ask("SELECT * FROM us‸")[0].insert, "\"user\"");
    assert_eq!(ask("SELECT u.gr‸ FROM \"user\" u")[0].insert, "\"group\"");
    assert_eq!(ask("SELECT c.em‸ FROM customers c")[0].insert, "\"Email\"");

    // A MySQL tab quotes with backticks, and keeps case unquoted.
    let catalog = CatalogIndex::new(&shop(), Dialect::DuckDb);
    let mysql = Flavor::new(Dialect::MySql, "8.0.39");
    let items = run("SELECT * FROM archi‸", &mysql, &catalog, Trigger::Invoked).items;
    assert_eq!(items[0].insert, "`Order Archive`");
    let items = run(
        "SELECT c.em‸ FROM customers c",
        &mysql,
        &catalog,
        Trigger::Invoked,
    )
    .items;
    assert_eq!(items[0].insert, "Email");
}

#[test]
fn quoted_names_complete_inside_their_quotes() {
    let catalog = CatalogIndex::new(&shop(), Dialect::DuckDb);
    let completions = run(
        "SELECT * FROM \"Ord‸",
        &duckdb(),
        &catalog,
        Trigger::Invoked,
    );
    assert_eq!(completions.items[0].insert, "\"Order Archive\"");
    assert_eq!(completions.replace, 14..18);
    // Only names: no keyword `ORDER` inside quotes.
    assert!(completions.items.iter().all(|i| i.kind == ItemKind::Table));
    // Even names that need no quotes keep the ones typed.
    let completions = run(
        "SELECT * FROM \"cust‸\"",
        &duckdb(),
        &catalog,
        Trigger::Invoked,
    );
    assert_eq!(completions.items[0].insert, "\"customers\"");
    assert_eq!(completions.replace, 14..20);
    let items = run(
        "SELECT o.\"amo‸\" FROM orders o",
        &duckdb(),
        &catalog,
        Trigger::Invoked,
    )
    .items;
    assert_eq!(items[0].insert, "\"amount\"");
}

#[test]
fn quoted_qualifiers_resolve() {
    assert_eq!(
        labels(&ask("SELECT \"user\".‸ FROM \"user\"")),
        ["id", "group"]
    );
    assert_eq!(labels(&ask("SELECT a.‸ FROM \"Order Archive\" a")), ["id"]);
    assert_eq!(
        labels(&ask("SELECT * FROM \"analytics\".‸")),
        ["daily_sales"]
    );
}

// ── Where nothing goes ──────────────────────────────────────────────────────

#[test]
fn nothing_in_strings_or_comments() {
    for sql in [
        "SELECT 'ord‸'",
        "SELECT 'ord‸",
        "SELECT * FROM orders WHERE status = 'pa‸",
        "-- FROM ord‸",
        "SELECT 1 -- FROM ord‸",
        "/* FROM ord‸ */",
        "SELECT /* unfinished ‸",
        "SELECT $$ ord‸ $$",
        "SELECT E'ord‸'",
        "SELECT 12‸",
        "SELECT $1‸",
    ] {
        assert!(ask(sql).is_empty(), "{sql}");
    }
    // Right after a finished string or comment, completion is back.
    assert!(!ask("SELECT 'a' ‸").is_empty());
    assert!(!ask("SELECT 1 /* c */ ‸").is_empty());
    assert!(!ask("SELECT 'it''s' ‸").is_empty());
}

#[test]
fn dialect_comments_and_strings() {
    let catalog = CatalogIndex::new(&shop(), Dialect::DuckDb);
    let mysql = Flavor::new(Dialect::MySql, "8.0.39");
    // A MySQL `#` comment and a `"..."` string.
    assert!(
        run("SELECT 1 # from ord‸", &mysql, &catalog, Trigger::Invoked)
            .items
            .is_empty()
    );
    assert!(
        run("SELECT \"ord‸", &mysql, &catalog, Trigger::Invoked)
            .items
            .is_empty()
    );
    // A backtick name is a name.
    let items = run("SELECT * FROM `cust‸`", &mysql, &catalog, Trigger::Invoked).items;
    assert_eq!(items[0].insert, "`customers`");
}

#[test]
fn only_the_statement_at_the_caret_counts() {
    // `c` is an alias in the other statement only.
    assert!(ask("SELECT * FROM customers c; SELECT c.‸ FROM orders o").is_empty());
    let items = ask("SELECT * FROM orders;\nSELECT ‸ FROM customers;\nSELECT * FROM order_items");
    let columns = of_kind(&items, ItemKind::Column);
    assert_eq!(columns, ["id", "name", "Email"]);
}

#[test]
fn mysql_delimiters_keep_routine_bodies_whole() {
    let catalog = CatalogIndex::new(&shop(), Dialect::DuckDb);
    let mysql = Flavor::new(Dialect::MySql, "8.0.39");
    let sql = "DELIMITER //\nCREATE PROCEDURE p() BEGIN SELECT o.‸ FROM orders o; SELECT 1; END//";
    let items = run(sql, &mysql, &catalog, Trigger::Invoked).items;
    assert_eq!(labels(&items)[0], "id");
}

#[test]
fn half_written_sql_never_panics() {
    let catalog = CatalogIndex::new(&shop(), Dialect::DuckDb);
    let scripts = [
        "SELECT o.amount, c. FROM orders o JOIN customers c ON (o.customer_id = c.id",
        "SELECT ((( FROM WHERE ))) . .. o.. \"a\"\". `x` [y] 'z",
        "WITH a AS (SELECT * FROM orders), b AS ( SELECT * FROM a JOIN",
        "INSERT INTO orders (id, VALUES ( UPDATE SET , , FROM",
        "SELECT 한글.컬럼 FROM 테이블 한 WHERE 한.",
        "DELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT 1; END$$\nDELIMITER ;\nSELECT",
        "select ::: as as ( , ) ) ) from from . ;;; $tag$ ",
    ];
    for flavor in Dialect::ALL.map(Flavor::unversioned) {
        for sql in scripts {
            for caret in 0..=sql.len() {
                for trigger in [Trigger::Invoked, Trigger::Typing] {
                    let completions = complete(sql, caret, &flavor, &catalog, trigger);
                    assert!(completions.replace.end <= sql.len());
                    assert!(sql.is_char_boundary(completions.replace.start));
                    assert!(sql.is_char_boundary(completions.replace.end));
                }
            }
        }
    }
}

// ── Typing ──────────────────────────────────────────────────────────────────

fn typing(sql: &str) -> Vec<Item> {
    let catalog = CatalogIndex::new(&shop(), Dialect::DuckDb);
    run(sql, &duckdb(), &catalog, Trigger::Typing).items
}

#[test]
fn typing_needs_something_to_complete() {
    assert!(typing("SELECT * FROM ‸").is_empty());
    assert!(!ask("SELECT * FROM ‸").is_empty());
    // A `.` after a qualifier is enough.
    assert_eq!(typing("SELECT o.‸ FROM orders o").len(), 5);
    assert!(!typing("SELECT * FROM o‸").is_empty());
}

#[test]
fn a_finished_word_closes_the_menu() {
    // Enter after a whole name must stay a new line.
    assert!(typing("SELECT * FROM orders‸").is_empty());
    assert!(typing("SELECT * FROM orders WHERE status‸").is_empty());
    assert!(typing("select‸").is_empty());
    // Even when only the case would change.
    assert!(typing("Select‸").is_empty());
    // But a word that begins a longer name still offers it.
    assert_eq!(typing("SELECT * FROM order‸")[0].insert, "orders");
}

#[test]
fn an_alias_being_typed_is_left_alone() {
    assert!(typing("SELECT * FROM orders o‸").is_empty());
    assert_eq!(typing("SELECT * FROM orders wh‸")[0].insert, "where");
}

#[test]
fn replaces_the_whole_word_at_the_caret() {
    let catalog = CatalogIndex::new(&shop(), Dialect::DuckDb);
    let completions = run(
        "SELECT * FROM ord‸ers_x",
        &duckdb(),
        &catalog,
        Trigger::Invoked,
    );
    assert_eq!(completions.replace, 14..22);
    let completions = run(
        "SELECT o.‸ FROM orders o",
        &duckdb(),
        &catalog,
        Trigger::Invoked,
    );
    assert_eq!(completions.replace, 9..9);
}

// ── Other engines ───────────────────────────────────────────────────────────

#[test]
fn postgres_searches_public() {
    let catalog = Catalog {
        databases: vec![database(
            "app",
            vec![
                schema("public", vec![table("orders", &[("id", "integer")])]),
                schema("audit", vec![table("Events", &[("at", "timestamptz")])]),
            ],
        )],
    };
    let catalog = CatalogIndex::new(&catalog, Dialect::Postgres);
    let flavor = Flavor::new(Dialect::Postgres, "16.4");
    let items = run("SELECT * FROM ev‸", &flavor, &catalog, Trigger::Invoked).items;
    assert_eq!(items[0].insert, "audit.\"Events\"");
    let items = run("SELECT * FROM ord‸", &flavor, &catalog, Trigger::Invoked).items;
    assert_eq!(items[0].insert, "orders");
    let items = run(
        "SELECT e.‸ FROM audit.\"Events\" e",
        &flavor,
        &catalog,
        Trigger::Invoked,
    )
    .items;
    assert_eq!(labels(&items), ["at"]);
    assert_eq!(items[0].detail, "timestamptz · Events e");
}

#[test]
fn mysql_databases_are_namespaces() {
    let catalog = Catalog {
        databases: vec![
            database(
                "crm",
                vec![schema("", vec![table("customers", &[("id", "int")])])],
            ),
            database(
                "shop",
                vec![schema("", vec![table("orders", &[("id", "int")])])],
            ),
        ],
    };
    let catalog = CatalogIndex::new(&catalog, Dialect::MySql);
    let flavor = Flavor::new(Dialect::MySql, "8.4.2");
    let items = run("SELECT * FROM ord‸", &flavor, &catalog, Trigger::Invoked).items;
    // Which database the connection uses is not known, so say it.
    assert_eq!(items[0].insert, "shop.orders");
    assert_eq!(items[0].detail, "table · shop");
    let items = run("SELECT * FROM sh‸", &flavor, &catalog, Trigger::Invoked).items;
    assert_eq!(items[0].kind, ItemKind::Database);
    let items = run("SELECT * FROM shop.‸", &flavor, &catalog, Trigger::Invoked).items;
    assert_eq!(inserts(&items), ["orders"]);
    let items = run(
        "SELECT o.‸ FROM shop.orders o",
        &flavor,
        &catalog,
        Trigger::Invoked,
    )
    .items;
    assert_eq!(labels(&items), ["id"]);
}

#[test]
fn one_mysql_database_needs_no_qualifier() {
    let catalog = Catalog {
        databases: vec![database(
            "shop",
            vec![schema("", vec![table("orders", &[])])],
        )],
    };
    let catalog = CatalogIndex::new(&catalog, Dialect::MySql);
    let flavor = Flavor::new(Dialect::MySql, "8.4.2");
    let items = run("SELECT * FROM ord‸", &flavor, &catalog, Trigger::Invoked).items;
    assert_eq!(items[0].insert, "orders");
}

// ── Ranking and size ────────────────────────────────────────────────────────

#[test]
fn closer_matches_rank_higher() {
    let items = ask("SELECT * FROM items‸");
    assert_eq!(items[0].label, "order_items");
    let items = ask("SELECT * FROM oi‸");
    assert_eq!(items[0].label, "order_items");
    // A prefix match beats a loose one, whatever the kinds.
    let items = ask("SELECT * FROM orders WHERE st‸");
    assert_eq!(items[0].label, "status");
}

#[test]
fn large_catalogs_stay_fast() {
    let columns: Vec<(String, String)> = (0..20)
        .map(|c| (format!("column_{c}"), "INTEGER".to_owned()))
        .collect();
    let columns: Vec<(&str, &str)> = columns
        .iter()
        .map(|(n, t)| (n.as_str(), t.as_str()))
        .collect();
    let relations = (0..5_000)
        .map(|t| table(&format!("table_{t:04}"), &columns))
        .collect();
    let catalog = Catalog {
        databases: vec![database("memory", vec![schema("main", relations)])],
    };
    let built = Instant::now();
    let catalog = CatalogIndex::new(&catalog, Dialect::DuckDb);
    let built = built.elapsed();

    let flavor = duckdb();
    let started = Instant::now();
    for sql in [
        "SELECT * FROM t‸",
        "SELECT * FROM ‸",
        "SELECT * FROM table_42‸",
        "SELECT t.‸ FROM table_0042 t",
        "SELECT col‸ FROM table_0001 JOIN table_0002 ON true",
    ] {
        let items = run(sql, &flavor, &catalog, Trigger::Invoked).items;
        assert!(!items.is_empty() && items.len() <= LIMIT, "{sql}");
    }
    let per_keystroke = started.elapsed() / 5;
    // Generous for unoptimized test builds on slow runners; a keystroke is
    // well under a millisecond in a release build.
    assert!(
        per_keystroke.as_millis() < 100,
        "{per_keystroke:?} per keystroke"
    );
    assert!(built.as_secs() < 5, "{built:?} to index");
}
