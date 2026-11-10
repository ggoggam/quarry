//! What the connected SQLite release can do.
//!
//! The bundled library has one fixed version, but every version-dependent
//! query in this crate asks a [`Features`] first, so the driver keeps
//! working (with older, plainer SQL) against an older engine, and the app
//! can switch editor features on only where the engine has them.

use quarry_core::{Flavor, Version};

/// `PRAGMA table_list`, with each table's type and STRICT flag.
pub const TABLE_LIST: Version = Version::new(3, 37, 0);
/// `CREATE TABLE ... STRICT`.
pub const STRICT_TABLES: Version = Version::new(3, 37, 0);
/// `PRAGMA table_xinfo`, which also lists hidden and generated columns.
pub const TABLE_XINFO: Version = Version::new(3, 26, 0);
/// `GENERATED ALWAYS AS (...)` columns.
pub const GENERATED_COLUMNS: Version = Version::new(3, 31, 0);
/// `INSERT/UPDATE/DELETE ... RETURNING`.
pub const RETURNING: Version = Version::new(3, 35, 0);
/// `ALTER TABLE ... DROP COLUMN`.
pub const DROP_COLUMN: Version = Version::new(3, 35, 0);
/// `unixepoch()`.
pub const UNIXEPOCH: Version = Version::new(3, 38, 0);
/// The `->` and `->>` JSON operators, and JSON built in by default.
pub const JSON_OPERATORS: Version = Version::new(3, 38, 0);
/// `RIGHT JOIN` and `FULL JOIN`.
pub const RIGHT_FULL_JOIN: Version = Version::new(3, 39, 0);
/// Window functions (`OVER (...)`).
pub const WINDOW_FUNCTIONS: Version = Version::new(3, 25, 0);
/// `INSERT ... ON CONFLICT DO UPDATE`.
pub const UPSERT: Version = Version::new(3, 24, 0);
/// `sqlite_schema` as the name of the schema table (`sqlite_master` before).
pub const SQLITE_SCHEMA: Version = Version::new(3, 33, 0);
/// `jsonb()` and the binary JSON functions.
pub const JSONB: Version = Version::new(3, 45, 0);
/// `concat()`, `concat_ws()` and `string_agg()`.
pub const CONCAT: Version = Version::new(3, 44, 0);

/// Feature switches for one engine release, read from its [`Flavor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Features {
    pub table_list: bool,
    pub strict_tables: bool,
    pub table_xinfo: bool,
    pub generated_columns: bool,
    pub returning: bool,
    pub drop_column: bool,
    pub unixepoch: bool,
    pub json_operators: bool,
    pub right_full_join: bool,
    pub window_functions: bool,
    pub upsert: bool,
    pub sqlite_schema: bool,
    pub jsonb: bool,
    pub concat: bool,
}

impl Features {
    /// The features of the release `flavor` names. An unknown version is
    /// taken to be recent, as [`Flavor::supports`] does.
    pub fn of(flavor: &Flavor) -> Self {
        let has = |since| flavor.supports(since);
        Self {
            table_list: has(TABLE_LIST),
            strict_tables: has(STRICT_TABLES),
            table_xinfo: has(TABLE_XINFO),
            generated_columns: has(GENERATED_COLUMNS),
            returning: has(RETURNING),
            drop_column: has(DROP_COLUMN),
            unixepoch: has(UNIXEPOCH),
            json_operators: has(JSON_OPERATORS),
            right_full_join: has(RIGHT_FULL_JOIN),
            window_functions: has(WINDOW_FUNCTIONS),
            upsert: has(UPSERT),
            sqlite_schema: has(SQLITE_SCHEMA),
            jsonb: has(JSONB),
            concat: has(CONCAT),
        }
    }

    /// The name of the table holding a database's schema.
    pub fn schema_table(&self) -> &'static str {
        if self.sqlite_schema {
            "sqlite_schema"
        } else {
            "sqlite_master"
        }
    }
}

#[cfg(test)]
mod tests {
    use quarry_core::Dialect;

    use super::*;

    fn at(version: &str) -> Features {
        Features::of(&Flavor::new(Dialect::Sqlite, version))
    }

    #[test]
    fn switches_features_by_release() {
        let old = at("3.22.0");
        assert!(!old.upsert && !old.window_functions && !old.table_xinfo);
        assert!(!old.returning && !old.table_list && !old.strict_tables);
        assert_eq!(old.schema_table(), "sqlite_master");

        let mid = at("3.35.5");
        assert!(mid.table_xinfo && mid.generated_columns && mid.returning);
        assert!(mid.sqlite_schema && !mid.table_list && !mid.unixepoch);
        assert_eq!(mid.schema_table(), "sqlite_schema");

        let new = at("3.54.1");
        assert!(new.table_list && new.strict_tables && new.unixepoch && new.jsonb);

        let unknown = Features::of(&Flavor::unversioned(Dialect::Sqlite));
        assert_eq!(unknown, new);
    }
}
