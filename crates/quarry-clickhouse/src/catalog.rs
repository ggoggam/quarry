//! The schema browser's view of a ClickHouse server, from its system tables.
//!
//! ClickHouse has no schema level, so each database holds one [`Schema`]
//! with an empty name.

use std::collections::BTreeMap;

use quarry_core::Flavor;
use quarry_core::catalog::{Catalog, ColumnDef, Database, Relation, RelationKind, Schema};

use crate::gates;
use crate::stream::unescape_tsv;

/// Databases ClickHouse keeps for itself.
const HIDDEN: &str = "('system', 'INFORMATION_SCHEMA', 'information_schema')";

pub(crate) fn read(
    flavor: &Flavor,
    mut query: impl FnMut(&str) -> anyhow::Result<String>,
) -> anyhow::Result<Catalog> {
    let mut databases: BTreeMap<String, BTreeMap<String, Relation>> = BTreeMap::new();
    for row in rows(&query(&format!(
        "SELECT name FROM system.databases WHERE name NOT IN {HIDDEN} ORDER BY name"
    ))?) {
        if let [name] = row.as_slice() {
            databases.entry(name.clone()).or_default();
        }
    }

    let rows_column = if gates::tables_have_total_rows(flavor) {
        "total_rows"
    } else {
        "NULL"
    };
    let temporary = if gates::tables_have_is_temporary(flavor) {
        " AND NOT is_temporary"
    } else {
        ""
    };
    for row in rows(&query(&format!(
        "SELECT database, name, engine, {rows_column} FROM system.tables \
          WHERE database NOT IN {HIDDEN}{temporary} ORDER BY database, name"
    ))?) {
        let [database, name, engine, total_rows] = row.as_slice() else {
            continue;
        };
        // A materialized view's own storage, shown through the view.
        if name.starts_with(".inner") {
            continue;
        }
        databases.entry(database.clone()).or_default().insert(
            name.clone(),
            Relation {
                name: name.clone(),
                kind: relation_kind(engine),
                columns: Vec::new(),
                estimated_rows: total_rows.parse().ok(),
            },
        );
    }

    for row in rows(&query(&format!(
        "SELECT database, table, name, type FROM system.columns \
          WHERE database NOT IN {HIDDEN} ORDER BY database, table, position"
    ))?) {
        let [database, table, name, data_type] = row.as_slice() else {
            continue;
        };
        if let Some(relation) = databases
            .get_mut(database)
            .and_then(|relations| relations.get_mut(table))
        {
            relation.columns.push(ColumnDef {
                name: name.clone(),
                nullable: is_nullable(data_type),
                data_type: data_type.clone(),
            });
        }
    }

    Ok(Catalog {
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
    })
}

/// How the schema browser shows a table with this engine. Dictionaries,
/// `Distributed`, `Merge` and other proxies read like tables.
pub(crate) fn relation_kind(engine: &str) -> RelationKind {
    match engine {
        "View" | "LiveView" | "WindowView" => RelationKind::View,
        "MaterializedView" => RelationKind::MaterializedView,
        _ => RelationKind::Table,
    }
}

/// Whether a column of this ClickHouse type can hold NULL.
pub(crate) fn is_nullable(data_type: &str) -> bool {
    let inner = data_type
        .strip_prefix("LowCardinality(")
        .unwrap_or(data_type);
    inner.starts_with("Nullable(")
}

/// TabSeparated output as rows of unescaped fields. `\N` (NULL) becomes "".
fn rows(text: &str) -> Vec<Vec<String>> {
    text.lines()
        .map(|line| {
            line.split('\t')
                .map(|field| {
                    if field == "\\N" {
                        String::new()
                    } else {
                        unescape_tsv(field)
                    }
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use quarry_core::Dialect;

    use super::*;

    #[test]
    fn maps_engines_to_kinds() {
        assert_eq!(relation_kind("MergeTree"), RelationKind::Table);
        assert_eq!(relation_kind("Dictionary"), RelationKind::Table);
        assert_eq!(relation_kind("Distributed"), RelationKind::Table);
        assert_eq!(relation_kind("View"), RelationKind::View);
        assert_eq!(
            relation_kind("MaterializedView"),
            RelationKind::MaterializedView
        );
    }

    #[test]
    fn spots_nullable_types() {
        assert!(is_nullable("Nullable(String)"));
        assert!(is_nullable("LowCardinality(Nullable(String))"));
        assert!(!is_nullable("LowCardinality(String)"));
        assert!(!is_nullable("Array(Nullable(Int8))"));
        assert!(!is_nullable("UInt64"));
    }

    #[test]
    fn builds_a_catalog_from_system_tables() {
        let flavor = Flavor::new(Dialect::ClickHouse, "24.8.4.13");
        let catalog = read(&flavor, |sql| {
            Ok(if sql.contains("system.databases") {
                "default\nempty\n".into()
            } else if sql.contains("system.tables") {
                assert!(
                    sql.contains("total_rows") && sql.contains("is_temporary"),
                    "{sql}"
                );
                "default\tevents\tMergeTree\t42\n\
                 default\tdaily\tMaterializedView\t\\N\n\
                 default\t.inner_id.0f0e\tAggregatingMergeTree\t3\n"
                    .into()
            } else {
                "default\tevents\tid\tUInt64\n\
                 default\tevents\tnote\tNullable(String)\n\
                 default\tdaily\tday\tDate\n"
                    .into()
            })
        })
        .unwrap();
        let names: Vec<_> = catalog.databases.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["default", "empty"]);
        let schema = &catalog.databases[0].schemas[0];
        assert_eq!(schema.name, "");
        assert_eq!(schema.relations.len(), 2);
        let events = schema
            .relations
            .iter()
            .find(|r| r.name == "events")
            .unwrap();
        assert_eq!(events.estimated_rows, Some(42));
        assert_eq!(events.columns[1].data_type, "Nullable(String)");
        assert!(events.columns[1].nullable);
        let daily = schema.relations.iter().find(|r| r.name == "daily").unwrap();
        assert_eq!(daily.kind, RelationKind::MaterializedView);
        assert_eq!(daily.estimated_rows, None);
        assert_eq!(catalog.databases[1].schemas[0].relations.len(), 0);
    }
}
