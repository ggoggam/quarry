//! Turning a [`Catalog`] into tree items for the schema browser.
//!
//! Each item's id encodes what it is, so a click can be resolved back to the
//! object without a side table: `db/<db>`, `schema/<db>/<schema>`,
//! `table/<db>/<schema>/<name>`, `view/...`, `matview/...`,
//! `col/<db>/<schema>/<rel>/<col>`. Engines without schemas use an empty
//! `<schema>`.
//! Parts are separated by `\u{1f}` (unit separator), which cannot appear in
//! an identifier a user typed by accident.

use std::collections::HashSet;

use gpui_kit::SharedString;
use gpui_kit::component::tree::TreeItem;
use quarry_core::catalog::{Catalog, Database, Relation, RelationKind, Schema};

const SEP: char = '\u{1f}';

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Database(String),
    Schema(String, String),
    Relation {
        database: String,
        schema: String,
        name: String,
        kind: RelationKind,
    },
    Column {
        database: String,
        schema: String,
        relation: String,
        name: String,
    },
}

impl Node {
    pub fn id(&self) -> SharedString {
        let parts: Vec<&str> = match self {
            Node::Database(db) => vec!["db", db],
            Node::Schema(db, schema) => vec!["schema", db, schema],
            Node::Relation {
                database,
                schema,
                name,
                kind,
            } => vec![kind_tag(*kind), database, schema, name],
            Node::Column {
                database,
                schema,
                relation,
                name,
            } => vec!["col", database, schema, relation, name],
        };
        parts.join(&SEP.to_string()).into()
    }

    pub fn parse(id: &str) -> Option<Node> {
        let parts: Vec<&str> = id.split(SEP).collect();
        let owned = |i: usize| parts.get(i).map(|s| (*s).to_owned());
        Some(match parts.first()? {
            &"db" => Node::Database(owned(1)?),
            &"schema" => Node::Schema(owned(1)?, owned(2)?),
            tag @ (&"table" | &"view" | &"matview") => Node::Relation {
                database: owned(1)?,
                schema: owned(2)?,
                name: owned(3)?,
                kind: match *tag {
                    "table" => RelationKind::Table,
                    "view" => RelationKind::View,
                    _ => RelationKind::MaterializedView,
                },
            },
            &"col" => Node::Column {
                database: owned(1)?,
                schema: owned(2)?,
                relation: owned(3)?,
                name: owned(4)?,
            },
            _ => return None,
        })
    }
}

fn kind_tag(kind: RelationKind) -> &'static str {
    match kind {
        RelationKind::Table => "table",
        RelationKind::View => "view",
        RelationKind::MaterializedView => "matview",
    }
}

/// Labels carry type information for columns so the tree reads like DDL.
/// Items whose id is in `expanded` start expanded, so a refresh keeps the
/// tree as the user left it. A schema with an empty name (engines without
/// schemas) is left out, and its relations sit right under the database.
pub fn build_items(catalog: &Catalog, expanded: &HashSet<SharedString>) -> Vec<TreeItem> {
    let item = |node: Node, label: String| {
        let id = node.id();
        let open = expanded.contains(&id);
        TreeItem::new(id, label).expanded(open)
    };
    let relation_item = |db: &Database, schema: &Schema, relation: &Relation| {
        let node = Node::Relation {
            database: db.name.clone(),
            schema: schema.name.clone(),
            name: relation.name.clone(),
            kind: relation.kind,
        };
        item(node, relation.name.clone()).children(relation.columns.iter().map(|column| {
            let node = Node::Column {
                database: db.name.clone(),
                schema: schema.name.clone(),
                relation: relation.name.clone(),
                name: column.name.clone(),
            };
            let nullable = if column.nullable { "" } else { " not null" };
            item(
                node,
                format!(
                    "{}  {}{nullable}",
                    column.name,
                    column.data_type.to_lowercase()
                ),
            )
        }))
    };
    catalog
        .databases
        .iter()
        .map(|db| {
            let mut children = Vec::new();
            for schema in &db.schemas {
                let relations = schema
                    .relations
                    .iter()
                    .map(|relation| relation_item(db, schema, relation));
                if schema.name.is_empty() {
                    children.extend(relations);
                } else {
                    children.push(
                        item(
                            Node::Schema(db.name.clone(), schema.name.clone()),
                            schema.name.clone(),
                        )
                        .children(relations),
                    );
                }
            }
            item(Node::Database(db.name.clone()), db.name.clone()).children(children)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_databases_without_schemas() {
        use quarry_core::catalog::ColumnDef;
        let catalog = Catalog {
            databases: vec![Database {
                name: "shop".into(),
                schemas: vec![Schema {
                    name: String::new(),
                    relations: vec![Relation {
                        name: "orders".into(),
                        kind: RelationKind::Table,
                        columns: vec![ColumnDef {
                            name: "id".into(),
                            data_type: "INT".into(),
                            nullable: false,
                        }],
                        estimated_rows: None,
                    }],
                }],
            }],
        };
        let items = build_items(&catalog, &HashSet::new());
        let orders = &items[0].children[0];
        assert_eq!(orders.label.as_ref(), "orders");
        assert_eq!(
            Node::parse(&orders.id),
            Some(Node::Relation {
                database: "shop".into(),
                schema: String::new(),
                name: "orders".into(),
                kind: RelationKind::Table,
            })
        );
    }

    #[test]
    fn ids_round_trip() {
        let nodes = [
            Node::Database("my db".into()),
            Node::Schema("d".into(), "s/x".into()),
            Node::Relation {
                database: "d".into(),
                schema: "s".into(),
                name: "orders".into(),
                kind: RelationKind::View,
            },
            Node::Column {
                database: "d".into(),
                schema: "s".into(),
                relation: "t".into(),
                name: "a.b".into(),
            },
        ];
        for node in nodes {
            assert_eq!(Node::parse(&node.id()), Some(node));
        }
        let matview = Node::Relation {
            database: "d".into(),
            schema: String::new(),
            name: "m".into(),
            kind: RelationKind::MaterializedView,
        };
        assert_eq!(Node::parse(&matview.id()), Some(matview));
        assert_eq!(Node::parse("nonsense"), None);
    }
}
