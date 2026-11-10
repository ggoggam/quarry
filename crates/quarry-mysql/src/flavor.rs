//! Telling MySQL from MariaDB, and what each release can do.
//!
//! Every place the driver behaves differently by engine or release goes
//! through one of the small helpers here, so the differences are named,
//! documented and unit tested in one place.

use quarry_core::catalog::RelationKind;
use quarry_core::{Dialect, Flavor, Version};

/// The flavor a `SELECT VERSION()` banner describes.
///
/// MariaDB says so in its version (`11.4.2-MariaDB-ubu2404`), and servers
/// that still speak the old handshake prefix it with `5.5.5-` so that
/// clients checking for MySQL 5.5+ accept them. Anything else is treated as
/// MySQL, including MySQL-compatible servers such as TiDB
/// (`8.0.11-TiDB-v7.5.0`), Percona Server (`8.0.39-30`) and Aurora.
pub fn detect_flavor(version: &str) -> Flavor {
    let version = version.trim();
    let mariadb = version.to_ascii_lowercase().contains("mariadb");
    let version = if mariadb {
        version.strip_prefix("5.5.5-").unwrap_or(version)
    } else {
        version
    };
    let short = version.split(['-', ' ']).next().unwrap_or_default();
    let dialect = if mariadb {
        Dialect::MariaDb
    } else {
        Dialect::MySql
    };
    Flavor::new(dialect, short)
}

/// MySQL 8.0.3 caches `information_schema.TABLES` statistics (`TABLE_ROWS`,
/// `DATA_LENGTH`, ...) for `information_schema_stats_expiry` seconds, a day
/// by default, so a table's row count can be a day stale (or 0 for a table
/// created after its first lookup). The catalog sets the session's expiry
/// to 0 while it reads, so counts are live as on MySQL 5.7 and MariaDB,
/// which have no such cache.
pub fn caches_table_stats(flavor: &Flavor) -> bool {
    flavor.dialect == Dialect::MySql && flavor.supports(Version::new(8, 0, 3))
}

/// MariaDB has no JSON type: `JSON` is an alias for `LONGTEXT` with a
/// `CHECK (json_valid(col))` constraint, so `information_schema.COLUMNS`
/// says `longtext`. MariaDB 10.2.1 added both the alias and
/// `information_schema.CHECK_CONSTRAINTS`, where the catalog finds those
/// constraints to call the column `json` again. MySQL has a native type.
pub fn json_is_checked_longtext(flavor: &Flavor) -> bool {
    flavor.dialect == Dialect::MariaDb && flavor.supports(Version::new(10, 2, 1))
}

/// Whether result columns can be mislabelled by the wire protocol, so the
/// driver should name them from the catalog where it knows the column.
///
/// MariaDB sends its own types as plain strings unless the client asks for
/// extended metadata, which the `mysql` crate does not: `json` arrives as
/// `longtext`, `uuid` (10.7+) as `char`, `inet4` / `inet6` (10.10+ / 10.5+)
/// as `char`. MySQL's wire types are already right.
pub fn names_columns_from_catalog(flavor: &Flavor) -> bool {
    flavor.dialect == Dialect::MariaDb
}

/// MariaDB's own column types that travel as plain strings; see
/// [`names_columns_from_catalog`].
pub fn is_mariadb_string_type(data_type: &str) -> bool {
    matches!(data_type, "json" | "uuid" | "inet4" | "inet6")
}

/// How an `information_schema.TABLES.TABLE_TYPE` shows in the schema
/// browser, or `None` to leave the object out.
///
/// - `BASE TABLE`; `SYSTEM VERSIONED` (MariaDB 10.3+ tables `WITH SYSTEM
///   VERSIONING`, queried like any table); and `TEMPORARY` (MariaDB 11.2+
///   lists the session's own temporary tables, which this connection can
///   query) are tables.
/// - `VIEW` and `SYSTEM VIEW` are views.
/// - `SEQUENCE` (MariaDB 10.3+) is left out: it is selectable but is not
///   data, and the browser has no kind for it.
pub fn relation_kind(table_type: &str) -> Option<RelationKind> {
    match table_type {
        "BASE TABLE" | "SYSTEM VERSIONED" | "TEMPORARY" => Some(RelationKind::Table),
        "VIEW" | "SYSTEM VIEW" => Some(RelationKind::View),
        _ => None,
    }
}

/// `TABLE_ROWS` as a row estimate, where it means one.
///
/// Views have none. InnoDB's figure is a sampled estimate (often off by
/// tens of percent) but still the best cheap number; MyISAM, Aria and
/// MEMORY keep exact counts. Engines that do not store rows locally
/// (FEDERATED, CONNECT, SPIDER, BLACKHOLE) report 0 or a guess that says
/// nothing about the remote data, so they get no estimate.
pub fn row_estimate(
    kind: RelationKind,
    engine: Option<&str>,
    table_rows: Option<u64>,
) -> Option<u64> {
    if kind != RelationKind::Table {
        return None;
    }
    let remote = engine.is_some_and(|engine| {
        matches!(
            engine.to_ascii_uppercase().as_str(),
            "FEDERATED" | "FEDERATEDX" | "CONNECT" | "SPIDER" | "BLACKHOLE"
        )
    });
    if remote { None } else { table_rows }
}

/// The statement that stops `connection_id`'s running statement and
/// leaves its connection open.
///
/// MySQL and MariaDB both take `KILL QUERY <connection id>`. MariaDB
/// 10.0.5 also has `KILL QUERY ID <query id>`, which names one statement
/// rather than a connection; it would avoid hitting the connection's next
/// statement if the kill arrives late, but the query id is only known from
/// `information_schema.PROCESSLIST`, a second round trip. The driver
/// instead checks that a late kill does not carry over (see the crate docs).
pub fn kill_query_sql(connection_id: u32) -> String {
    format!("KILL QUERY {connection_id}")
}

/// Whether `INSERT`, `REPLACE` and `DELETE` can return rows with
/// `RETURNING`: `DELETE` since MariaDB 10.0.5, `INSERT` and `REPLACE`
/// since 10.5. MySQL has no `RETURNING`. Such statements produce a result
/// set, which the driver streams like a query instead of a row count.
pub fn supports_returning(flavor: &Flavor, statement: &str) -> bool {
    if flavor.dialect != Dialect::MariaDb {
        return false;
    }
    match statement.to_ascii_uppercase().as_str() {
        "DELETE" => flavor.supports(Version::new(10, 0, 5)),
        "INSERT" | "REPLACE" => flavor.supports(Version::new(10, 5, 0)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_mysql_and_mariadb() {
        let mysql = detect_flavor("8.0.39");
        assert_eq!(mysql.dialect, Dialect::MySql);
        assert_eq!(mysql.version_text, "8.0.39");
        assert_eq!(mysql.version, Some(Version::new(8, 0, 39)));

        let mariadb = detect_flavor("11.4.2-MariaDB-ubu2404");
        assert_eq!(mariadb.dialect, Dialect::MariaDb);
        assert_eq!(mariadb.version_text, "11.4.2");

        let old_handshake = detect_flavor("5.5.5-10.6.18-MariaDB-1:10.6.18+maria~ubu2004-log");
        assert_eq!(old_handshake.dialect, Dialect::MariaDb);
        assert_eq!(old_handshake.version_text, "10.6.18");

        let percona = detect_flavor("8.0.39-30");
        assert_eq!(percona.dialect, Dialect::MySql);
        assert_eq!(percona.version_text, "8.0.39");

        let tidb = detect_flavor("8.0.11-TiDB-v7.5.0");
        assert_eq!(tidb.dialect, Dialect::MySql);
        assert_eq!(tidb.version_text, "8.0.11");

        let log = detect_flavor("8.4.3-log");
        assert_eq!(log.version_text, "8.4.3");
        // A MySQL 5.5.5 is still MySQL.
        assert_eq!(detect_flavor("5.5.5").dialect, Dialect::MySql);
    }

    #[test]
    fn gates_the_stats_cache_to_mysql_8() {
        assert!(caches_table_stats(&detect_flavor("8.0.39")));
        assert!(caches_table_stats(&detect_flavor("8.4.3")));
        assert!(caches_table_stats(&detect_flavor("9.1.0")));
        assert!(!caches_table_stats(&detect_flavor("8.0.2")));
        assert!(!caches_table_stats(&detect_flavor("5.7.44")));
        assert!(!caches_table_stats(&detect_flavor("11.4.2-MariaDB")));
    }

    #[test]
    fn gates_json_labelling_to_mariadb() {
        assert!(json_is_checked_longtext(&detect_flavor("10.6.18-MariaDB")));
        assert!(json_is_checked_longtext(&detect_flavor("11.4.2-MariaDB")));
        assert!(!json_is_checked_longtext(&detect_flavor("10.1.48-MariaDB")));
        assert!(!json_is_checked_longtext(&detect_flavor("8.0.39")));
        assert!(names_columns_from_catalog(&detect_flavor(
            "10.6.18-MariaDB"
        )));
        assert!(!names_columns_from_catalog(&detect_flavor("8.4.3")));
        assert!(is_mariadb_string_type("uuid"));
        assert!(!is_mariadb_string_type("longtext"));
    }

    #[test]
    fn maps_table_types() {
        assert_eq!(relation_kind("BASE TABLE"), Some(RelationKind::Table));
        assert_eq!(relation_kind("SYSTEM VERSIONED"), Some(RelationKind::Table));
        assert_eq!(relation_kind("TEMPORARY"), Some(RelationKind::Table));
        assert_eq!(relation_kind("VIEW"), Some(RelationKind::View));
        assert_eq!(relation_kind("SYSTEM VIEW"), Some(RelationKind::View));
        assert_eq!(relation_kind("SEQUENCE"), None);
    }

    #[test]
    fn estimates_rows_only_where_meaningful() {
        use RelationKind::*;
        assert_eq!(row_estimate(Table, Some("InnoDB"), Some(42)), Some(42));
        assert_eq!(row_estimate(Table, Some("Aria"), Some(7)), Some(7));
        assert_eq!(row_estimate(Table, None, None), None);
        assert_eq!(row_estimate(View, None, Some(3)), None);
        assert_eq!(row_estimate(Table, Some("FEDERATED"), Some(2)), None);
        assert_eq!(row_estimate(Table, Some("Connect"), Some(0)), None);
    }

    #[test]
    fn knows_where_returning_exists() {
        let maria106 = detect_flavor("10.6.18-MariaDB");
        let maria104 = detect_flavor("10.4.34-MariaDB");
        let mysql = detect_flavor("8.4.3");
        assert!(supports_returning(&maria106, "insert"));
        assert!(supports_returning(&maria104, "DELETE"));
        assert!(!supports_returning(&maria104, "INSERT"));
        assert!(!supports_returning(&mysql, "DELETE"));
        assert!(!supports_returning(&maria106, "UPDATE"));
    }

    #[test]
    fn kills_by_connection_id() {
        assert_eq!(kill_query_sql(42), "KILL QUERY 42");
    }
}
