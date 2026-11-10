//! The schema browser's view of a PostgreSQL database, read from pg_catalog
//! (information_schema is far slower and hides objects the user does not
//! own).

use std::collections::{BTreeMap, HashMap};

use quarry_core::Flavor;
use quarry_core::catalog::{Catalog, ColumnDef, Database, Relation, RelationKind, Schema};
use tokio_postgres::Client;

use crate::gates;

/// Schemas the browser leaves out: the system's, TOAST storage, and every
/// session's temporary schemas.
const USER_SCHEMAS: &str = "n.nspname NOT IN ('pg_catalog', 'information_schema') \
     AND n.nspname NOT LIKE 'pg\\_toast%' AND n.nspname NOT LIKE 'pg\\_temp\\_%'";

fn schemas_sql() -> String {
    format!("SELECT n.nspname::text FROM pg_catalog.pg_namespace n WHERE {USER_SCHEMAS} ORDER BY 1")
}

fn relations_sql(flavor: &Flavor) -> String {
    format!(
        "SELECT c.oid::int8, n.nspname::text, c.relname::text, c.relkind::text, \
                c.reltuples::float8, c.relpages::int8 \
           FROM pg_catalog.pg_class c \
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
          WHERE c.relkind IN ({kinds}) AND {not_partition} AND {USER_SCHEMAS} \
          ORDER BY 2, 3",
        kinds = gates::listed_relkinds(flavor),
        not_partition = gates::hide_partitions(flavor),
    )
}

fn columns_sql(flavor: &Flavor) -> String {
    format!(
        "SELECT c.oid::int8, a.attname::text, \
                pg_catalog.format_type(a.atttypid, a.atttypmod), a.attnotnull \
           FROM pg_catalog.pg_attribute a \
           JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
          WHERE a.attnum > 0 AND NOT a.attisdropped \
            AND c.relkind IN ({kinds}) AND {not_partition} AND {USER_SCHEMAS} \
          ORDER BY c.oid, a.attnum",
        kinds = gates::listed_relkinds(flavor),
        not_partition = gates::hide_partitions(flavor),
    )
}

/// Every leaf partition under each top-level partitioned table, through any
/// depth of sub-partitioning. A recursive walk of pg_inherits works on every
/// release with partitioning (pg_partition_tree() needs 12).
const PARTITION_LEAVES_SQL: &str = "WITH RECURSIVE tree(root, relid) AS ( \
         SELECT c.oid, c.oid FROM pg_catalog.pg_class c \
          WHERE c.relkind = 'p' AND NOT c.relispartition \
         UNION ALL \
         SELECT t.root, i.inhrelid FROM tree t \
           JOIN pg_catalog.pg_inherits i ON i.inhparent = t.relid) \
     SELECT t.root::int8, c.reltuples::float8, c.relpages::int8 \
       FROM tree t JOIN pg_catalog.pg_class c ON c.oid = t.relid \
      WHERE c.relkind <> 'p'";

pub async fn read(client: &Client, flavor: &Flavor) -> Result<Catalog, tokio_postgres::Error> {
    let (schemas_sql, relations_sql, columns_sql) =
        (schemas_sql(), relations_sql(flavor), columns_sql(flavor));
    // Sent together, so the round trips overlap.
    let database = client.query_one("SELECT current_database()::text", &[]);
    let schemas = client.query(&schemas_sql, &[]);
    let relations = client.query(&relations_sql, &[]);
    let columns = client.query(&columns_sql, &[]);
    let leaves = async {
        if gates::has_partitioned_tables(flavor) {
            client.query(PARTITION_LEAVES_SQL, &[]).await
        } else {
            Ok(Vec::new())
        }
    };
    let (database, schemas, relations, columns, leaves) =
        futures_util::future::try_join5(database, schemas, relations, columns, leaves).await?;

    // A partitioned table's estimate is its leaves' sum, counting only the
    // leaves the server has an estimate for.
    let mut partitioned: HashMap<i64, Option<u64>> = HashMap::new();
    for row in &leaves {
        let estimate = gates::estimated_rows(flavor, row.get(1), row.get(2));
        let total = partitioned.entry(row.get(0)).or_default();
        if let Some(rows) = estimate {
            *total = Some(total.unwrap_or(0) + rows);
        }
    }

    let mut by_schema: BTreeMap<String, Vec<Relation>> = schemas
        .iter()
        .map(|row| (row.get::<_, String>(0), Vec::new()))
        .collect();
    let mut index: HashMap<i64, (String, usize)> = HashMap::new();
    for row in &relations {
        let oid: i64 = row.get(0);
        let schema: String = row.get(1);
        let relkind: String = row.get(3);
        let (kind, estimated_rows) = match relkind.as_str() {
            "v" => (RelationKind::View, None),
            "m" => (
                RelationKind::MaterializedView,
                gates::estimated_rows(flavor, row.get(4), row.get(5)),
            ),
            // A partitioned table with no partitions can hold no rows.
            "p" => (
                RelationKind::Table,
                partitioned.get(&oid).copied().unwrap_or(Some(0)),
            ),
            // Plain and foreign tables.
            _ => (
                RelationKind::Table,
                gates::estimated_rows(flavor, row.get(4), row.get(5)),
            ),
        };
        let relations = by_schema.entry(schema.clone()).or_default();
        index.insert(oid, (schema, relations.len()));
        relations.push(Relation {
            name: row.get(2),
            kind,
            columns: Vec::new(),
            estimated_rows,
        });
    }
    for row in &columns {
        let Some((schema, at)) = index.get(&row.get::<_, i64>(0)) else {
            continue;
        };
        if let Some(relation) = by_schema.get_mut(schema).and_then(|r| r.get_mut(*at)) {
            relation.columns.push(ColumnDef {
                name: row.get(1),
                data_type: row.get(2),
                nullable: !row.get::<_, bool>(3),
            });
        }
    }

    Ok(Catalog {
        databases: vec![Database {
            name: database.get(0),
            schemas: by_schema
                .into_iter()
                .map(|(name, relations)| Schema { name, relations })
                .collect(),
        }],
    })
}

#[cfg(test)]
mod tests {
    use quarry_core::Dialect;

    use super::*;

    #[test]
    fn leaves_out_what_old_releases_lack() {
        let old = Flavor::new(Dialect::Postgres, "9.6.24");
        let sql = relations_sql(&old);
        assert!(!sql.contains("relispartition"), "{sql}");
        assert!(!sql.contains("'p'"), "{sql}");
        let new = Flavor::new(Dialect::Postgres, "17.2");
        let sql = columns_sql(&new);
        assert!(sql.contains("NOT c.relispartition"), "{sql}");
        assert!(sql.contains("'m', 'p'"), "{sql}");
    }
}
