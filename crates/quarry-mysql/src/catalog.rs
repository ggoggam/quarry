//! The schema browser's view of a server, from `information_schema`.

use std::collections::{BTreeMap, HashMap};

use mysql::Conn;
use mysql::prelude::Queryable;
use quarry_core::Flavor;
use quarry_core::catalog::{Catalog, ColumnDef, Database, Relation, Schema};

use crate::flavor::{
    caches_table_stats, is_mariadb_string_type, json_is_checked_longtext, relation_kind,
    row_estimate,
};

/// The server's own databases, hidden from the browser.
const SYSTEM_DATABASES: &str = "'mysql', 'information_schema', 'performance_schema', 'sys'";

/// `(database, table, column)`.
pub type ColumnKey = (String, String, String);

/// What a catalog read found: the tree, and the engine's names for columns
/// the wire protocol mislabels (see
/// [`crate::flavor::names_columns_from_catalog`]).
pub struct CatalogRead {
    pub catalog: Catalog,
    pub type_names: HashMap<ColumnKey, String>,
}

pub fn read_catalog(conn: &mut Conn, flavor: &Flavor) -> mysql::Result<CatalogRead> {
    if !caches_table_stats(flavor) {
        return read(conn, flavor);
    }
    let previous: Option<u64> =
        conn.query_first("SELECT @@SESSION.information_schema_stats_expiry")?;
    conn.query_drop("SET SESSION information_schema_stats_expiry = 0")?;
    let read = read(conn, flavor);
    let restore = match previous {
        Some(seconds) => format!("SET SESSION information_schema_stats_expiry = {seconds}"),
        None => "SET SESSION information_schema_stats_expiry = DEFAULT".to_owned(),
    };
    conn.query_drop(restore)?;
    read
}

fn read(conn: &mut Conn, flavor: &Flavor) -> mysql::Result<CatalogRead> {
    // Databases first, so empty ones still show up.
    let mut databases: BTreeMap<String, BTreeMap<String, Relation>> = BTreeMap::new();
    let names: Vec<String> = conn.query(format!(
        "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA \
          WHERE SCHEMA_NAME NOT IN ({SYSTEM_DATABASES})"
    ))?;
    for name in names {
        databases.entry(name).or_default();
    }

    type TableRow = (String, String, String, Option<String>, Option<u64>);
    let tables: Vec<TableRow> = conn.query(format!(
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE, ENGINE, TABLE_ROWS \
           FROM information_schema.TABLES \
          WHERE TABLE_SCHEMA NOT IN ({SYSTEM_DATABASES})"
    ))?;
    for (database, name, table_type, engine, rows) in tables {
        let Some(kind) = relation_kind(&table_type) else {
            continue;
        };
        databases.entry(database).or_default().insert(
            name.clone(),
            Relation {
                name,
                kind,
                columns: Vec::new(),
                estimated_rows: row_estimate(kind, engine.as_deref(), rows),
            },
        );
    }

    let json_columns = if json_is_checked_longtext(flavor) {
        mariadb_json_columns(conn)?
    } else {
        Default::default()
    };
    let mut type_names = HashMap::new();

    type ColumnRow = (String, String, String, String, String, String);
    let columns: Vec<ColumnRow> = conn.query(format!(
        "SELECT TABLE_SCHEMA, TABLE_NAME, COLUMN_NAME, DATA_TYPE, COLUMN_TYPE, IS_NULLABLE \
           FROM information_schema.COLUMNS \
          WHERE TABLE_SCHEMA NOT IN ({SYSTEM_DATABASES}) \
          ORDER BY TABLE_SCHEMA, TABLE_NAME, ORDINAL_POSITION"
    ))?;
    for (database, table, column, data_type, column_type, nullable) in columns {
        let key = (database, table, column);
        let data_type = data_type.to_ascii_lowercase();
        let column_type = if json_columns.contains(&key) && data_type == "longtext" {
            "json".to_owned()
        } else {
            column_type
        };
        if is_mariadb_string_type(&column_type) {
            type_names.insert(key.clone(), column_type.clone());
        }
        let (database, table, column) = key;
        if let Some(relation) = databases
            .get_mut(&database)
            .and_then(|relations| relations.get_mut(&table))
        {
            relation.columns.push(ColumnDef {
                name: column,
                data_type: column_type,
                nullable: nullable == "YES",
            });
        }
    }

    let catalog = Catalog {
        databases: databases
            .into_iter()
            .map(|(name, relations)| Database {
                name,
                schemas: vec![Schema {
                    name: String::new(),
                    relations: relations.into_values().collect(),
                }],
            })
            .collect(),
    };
    Ok(CatalogRead {
        catalog,
        type_names,
    })
}

/// Columns MariaDB declared `JSON`: `LONGTEXT` with the
/// `CHECK (json_valid(col))` it adds for the alias.
fn mariadb_json_columns(conn: &mut Conn) -> mysql::Result<std::collections::HashSet<ColumnKey>> {
    let checks: Vec<(String, String, String)> = conn.query(format!(
        "SELECT CONSTRAINT_SCHEMA, TABLE_NAME, CHECK_CLAUSE \
           FROM information_schema.CHECK_CONSTRAINTS \
          WHERE CONSTRAINT_SCHEMA NOT IN ({SYSTEM_DATABASES}) \
            AND CHECK_CLAUSE LIKE 'json\\_valid(%'"
    ))?;
    Ok(checks
        .into_iter()
        .filter_map(|(database, table, clause)| {
            json_check_column(&clause).map(|column| (database, table, column))
        })
        .collect())
}

/// The column a `json_valid(`col`)` check clause validates, or `None` for
/// any other clause (such as `json_valid(a) and a <> '{}'`, which is a
/// user's constraint, not the alias's).
pub fn json_check_column(clause: &str) -> Option<String> {
    let inner = clause
        .trim()
        .strip_prefix("json_valid(")?
        .strip_suffix(')')?;
    let name = inner.strip_prefix('`')?.strip_suffix('`')?;
    // Inside backticks a backtick is doubled; a lone one ends the name.
    let mut out = String::with_capacity(name.len());
    let mut chars = name.chars();
    while let Some(c) = chars.next() {
        if c == '`' && chars.next() != Some('`') {
            return None;
        }
        out.push(c);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_column_of_a_json_check() {
        assert_eq!(
            json_check_column("json_valid(`doc`)").as_deref(),
            Some("doc")
        );
        assert_eq!(
            json_check_column("json_valid(`odd``name`)").as_deref(),
            Some("odd`name")
        );
        assert_eq!(json_check_column("json_valid(`a`) and `a` <> '{}'"), None);
        assert_eq!(json_check_column("`a` > 0"), None);
    }
}
