//! Reading what a SQLite connection can see.
//!
//! SQLite has no schemas: every attached database (`main`, `temp` once it
//! holds objects, and each `ATTACH`ed file) becomes one [`Database`] with a
//! single [`Schema`] whose name is empty. Each step that has a newer, richer
//! form asks [`Features`] which one the engine supports.

use std::collections::HashMap;

use quarry_core::Dialect;
use quarry_core::catalog::{Catalog, ColumnDef, Database, Relation, RelationKind, Schema};
use rusqlite::{Connection, OptionalExtension as _};

use crate::features::Features;

pub(crate) fn read_catalog(conn: &Connection, features: &Features) -> anyhow::Result<Catalog> {
    let mut databases = Vec::new();
    for name in database_names(conn)? {
        if name == "temp" && !has_objects(conn, &name)? {
            continue;
        }
        let estimates = row_estimates(conn, &name)?;
        let mut relations = Vec::new();
        for entry in list_relations(conn, &name, features)? {
            let columns = match list_columns(conn, &name, &entry, features) {
                Ok(columns) => columns,
                // A virtual table whose module is not loaded here cannot
                // describe its columns; show it without them.
                Err(err) => {
                    log::debug!("columns of {name}.{}: {err}", entry.name);
                    Vec::new()
                }
            };
            relations.push(Relation {
                estimated_rows: match entry.kind {
                    RelationKind::Table => estimates.get(&entry.name).copied(),
                    _ => None,
                },
                name: entry.name,
                kind: entry.kind,
                columns,
            });
        }
        databases.push(Database {
            name,
            schemas: vec![Schema {
                name: String::new(),
                relations,
            }],
        });
    }
    Ok(Catalog { databases })
}

fn quote(name: &str) -> String {
    Dialect::Sqlite.quote_ident(name)
}

/// The attached databases in `PRAGMA database_list` order: `main`, `temp`,
/// then attached ones as attached.
fn database_names(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("PRAGMA database_list")?;
    let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
    names.collect()
}

fn has_objects(conn: &Connection, database: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        &format!(
            "SELECT EXISTS (SELECT 1 FROM {}.sqlite_master)",
            quote(database)
        ),
        [],
        |row| row.get(0),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelationEntry {
    pub name: String,
    pub kind: RelationKind,
    /// STRICT and WITHOUT ROWID tables make their primary key NOT NULL.
    pub pk_not_null: bool,
}

/// Tables (virtual ones included), and views of one database, by name,
/// without SQLite's own `sqlite_*` tables.
pub(crate) fn list_relations(
    conn: &Connection,
    database: &str,
    features: &Features,
) -> rusqlite::Result<Vec<RelationEntry>> {
    let master = format!("{}.sqlite_master", quote(database));
    let (sql, has_strict) = if features.table_list {
        // Knows virtual tables from ordinary ones, and leaves out the
        // shadow tables virtual tables keep their data in.
        (
            format!(
                "SELECT t.name, t.type, t.strict, m.sql \
                   FROM pragma_table_list t \
                   LEFT JOIN {master} m ON m.name = t.name AND m.type = t.type \
                  WHERE t.schema = ?1 AND t.type IN ('table', 'view', 'virtual') \
                    AND t.name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
                  ORDER BY t.name"
            ),
            true,
        )
    } else {
        // `?1` is unused here but keeps one call shape for both forms.
        (
            format!(
                "SELECT name, type, 0, sql FROM {master} \
                  WHERE ?1 IS NOT NULL AND type IN ('table', 'view') \
                    AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
                  ORDER BY name"
            ),
            false,
        )
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([database], |row| {
        let name: String = row.get(0)?;
        let kind: String = row.get(1)?;
        let strict: bool = has_strict && row.get::<_, i64>(2)? != 0;
        let sql: Option<String> = row.get(3)?;
        Ok(RelationEntry {
            name,
            kind: if kind == "view" {
                RelationKind::View
            } else {
                RelationKind::Table
            },
            pk_not_null: strict || sql.as_deref().is_some_and(is_without_rowid),
        })
    })?;
    rows.collect()
}

/// Whether a `CREATE TABLE` statement ends in the `WITHOUT ROWID` option.
fn is_without_rowid(sql: &str) -> bool {
    let options = sql.rsplit_once(')').map_or("", |(_, tail)| tail);
    let words: Vec<String> = options
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_uppercase)
        .collect();
    words
        .windows(2)
        .any(|w| w[0] == "WITHOUT" && w[1] == "ROWID")
}

/// One relation's columns in order, with declared types.
///
/// `table_xinfo` (3.26) also lists generated columns, shown as their type
/// followed by `GENERATED` (`GENERATED STORED` for stored ones); virtual
/// tables' hidden columns are left out. Older engines use `table_info`,
/// which knows neither.
pub(crate) fn list_columns(
    conn: &Connection,
    database: &str,
    relation: &RelationEntry,
    features: &Features,
) -> rusqlite::Result<Vec<ColumnDef>> {
    struct Raw {
        name: String,
        decl: String,
        not_null: bool,
        pk: i64,
        hidden: i64,
    }
    let raw: Vec<Raw> = if features.table_xinfo {
        let mut stmt = conn.prepare(
            "SELECT name, type, \"notnull\", pk, hidden FROM pragma_table_xinfo(?1, ?2) \
              ORDER BY cid",
        )?;
        stmt.query_map([relation.name.as_str(), database], |row| {
            Ok(Raw {
                name: row.get(0)?,
                decl: row.get(1)?,
                not_null: row.get(2)?,
                pk: row.get(3)?,
                hidden: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?
    } else {
        // The statement form takes no parameters, but runs on every release.
        let mut stmt = conn.prepare(&format!(
            "PRAGMA {}.table_info({})",
            quote(database),
            quote(&relation.name)
        ))?;
        stmt.query_map([], |row| {
            Ok(Raw {
                name: row.get(1)?,
                decl: row.get(2)?,
                not_null: row.get(3)?,
                pk: row.get(5)?,
                hidden: 0,
            })
        })?
        .collect::<rusqlite::Result<_>>()?
    };

    let pk_columns = raw.iter().filter(|c| c.pk > 0).count();
    Ok(raw
        .into_iter()
        .filter(|c| c.hidden != 1)
        .map(|c| {
            // An INTEGER PRIMARY KEY is the rowid, which is never NULL. Other
            // primary keys allow NULL in ordinary tables, a legacy quirk,
            // but not in STRICT or WITHOUT ROWID ones.
            let rowid_alias = pk_columns == 1
                && c.pk == 1
                && relation.kind == RelationKind::Table
                && c.decl.trim().eq_ignore_ascii_case("INTEGER");
            let pk_not_null = c.pk > 0 && relation.pk_not_null;
            let data_type = match c.hidden {
                2 => format!("{} GENERATED", c.decl).trim().to_owned(),
                3 => format!("{} GENERATED STORED", c.decl).trim().to_owned(),
                _ => c.decl,
            };
            ColumnDef {
                name: c.name,
                data_type,
                nullable: !(c.not_null || rowid_alias || pk_not_null),
            }
        })
        .collect())
}

/// Row counts `ANALYZE` left in `sqlite_stat1`, by table. Empty when the
/// database was never analyzed: counting rows would mean scanning them.
pub(crate) fn row_estimates(
    conn: &Connection,
    database: &str,
) -> rusqlite::Result<HashMap<String, u64>> {
    let db = quote(database);
    let analyzed = conn
        .query_row(
            &format!(
                "SELECT 1 FROM {db}.sqlite_master WHERE type = 'table' AND name = 'sqlite_stat1'"
            ),
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    let mut estimates = HashMap::new();
    if !analyzed {
        return Ok(estimates);
    }
    let mut stmt = conn.prepare(&format!("SELECT tbl, stat FROM {db}.sqlite_stat1"))?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })?;
    for row in rows {
        let (table, stat) = row?;
        // The first number is the row count of the table, or of the index
        // the row describes, which is the same for all but partial indexes.
        let Some(count) = stat
            .as_deref()
            .and_then(|s| s.split_whitespace().next())
            .and_then(|n| n.parse::<u64>().ok())
        else {
            continue;
        };
        let entry = estimates.entry(table).or_insert(0);
        *entry = (*entry).max(count);
    }
    Ok(estimates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spots_without_rowid_tables() {
        assert!(is_without_rowid(
            "CREATE TABLE t (a TEXT PRIMARY KEY) WITHOUT ROWID"
        ));
        assert!(is_without_rowid(
            "CREATE TABLE t (a TEXT PRIMARY KEY) STRICT, without  rowid"
        ));
        assert!(!is_without_rowid("CREATE TABLE t (a TEXT PRIMARY KEY)"));
        assert!(!is_without_rowid("CREATE TABLE t (without_rowid INT)"));
    }
}
