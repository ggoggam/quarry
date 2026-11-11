//! The catalog flattened for completion.
//!
//! A catalog can hold thousands of tables. Completing walks it on every
//! keystroke, so it is built once, whenever the catalog is read, into flat
//! lists with every name already lowered for matching, relations sorted by
//! name and found by name through a map. Quoting waits until an item is
//! actually shown, since it depends on the dialect the editor tab writes,
//! which may not be the connection's.

use std::collections::HashMap;
use std::ops::Range;

use crate::catalog::{Catalog, RelationKind};
use crate::dialect::Dialect;

/// A [`Catalog`] ready for completion. The default one is empty: before a
/// catalog arrives, or with no connection, only words complete.
#[derive(Debug, Default)]
pub struct CatalogIndex {
    databases: Vec<DatabaseEntry>,
    schemas: Vec<SchemaEntry>,
    /// Sorted by lowered name.
    relations: Vec<RelationEntry>,
    columns: Vec<ColumnEntry>,
    by_name: HashMap<String, Vec<u32>>,
}

#[derive(Debug)]
pub(super) struct DatabaseEntry {
    pub name: String,
    pub lower: String,
    /// Its objects need its name in front to be found from the connection's
    /// default database.
    pub qualify: bool,
}

#[derive(Debug)]
pub(super) struct SchemaEntry {
    /// Empty for engines without schemas.
    pub name: String,
    pub lower: String,
    pub database: u32,
    /// Its relations need its name in front: it is not on the search path.
    pub qualify: bool,
}

#[derive(Debug)]
pub(super) struct RelationEntry {
    pub name: String,
    pub lower: String,
    pub kind: RelationKind,
    pub schema: u32,
    pub columns: Range<u32>,
}

#[derive(Debug)]
pub(super) struct ColumnEntry {
    pub name: String,
    pub lower: String,
    pub data_type: String,
}

impl CatalogIndex {
    /// Index `catalog`, as read from a `dialect` engine. The dialect decides
    /// which namespaces are searched without naming them (PostgreSQL's
    /// `public`, DuckDB's `main`, ClickHouse's `default` database), so names
    /// elsewhere are inserted qualified.
    pub fn new(catalog: &Catalog, dialect: Dialect) -> Self {
        let mut this = CatalogIndex::default();
        let several_databases = catalog.databases.len() > 1 && dialect != Dialect::Postgres;
        let mut relations = Vec::new();
        for database in &catalog.databases {
            let db = this.databases.len() as u32;
            this.databases.push(DatabaseEntry {
                lower: database.name.to_lowercase(),
                name: database.name.clone(),
                qualify: several_databases && !is_default_database(dialect, &database.name),
            });
            for schema in &database.schemas {
                let schema_ix = this.schemas.len() as u32;
                this.schemas.push(SchemaEntry {
                    lower: schema.name.to_lowercase(),
                    name: schema.name.clone(),
                    database: db,
                    qualify: !schema.name.is_empty()
                        && dialect.has_schemas()
                        && !is_default_schema(dialect, &schema.name),
                });
                for relation in &schema.relations {
                    let start = this.columns.len() as u32;
                    this.columns
                        .extend(relation.columns.iter().map(|column| ColumnEntry {
                            lower: column.name.to_lowercase(),
                            name: column.name.clone(),
                            data_type: column.data_type.clone(),
                        }));
                    relations.push(RelationEntry {
                        lower: relation.name.to_lowercase(),
                        name: relation.name.clone(),
                        kind: relation.kind,
                        schema: schema_ix,
                        columns: start..this.columns.len() as u32,
                    });
                }
            }
        }
        relations.sort_by(|a, b| a.lower.cmp(&b.lower).then_with(|| a.name.cmp(&b.name)));
        for (ix, relation) in relations.iter().enumerate() {
            this.by_name
                .entry(relation.lower.clone())
                .or_default()
                .push(ix as u32);
        }
        this.relations = relations;
        this
    }

    pub fn is_empty(&self) -> bool {
        self.databases.is_empty()
    }

    pub(super) fn databases(&self) -> &[DatabaseEntry] {
        &self.databases
    }

    pub(super) fn schemas(&self) -> &[SchemaEntry] {
        &self.schemas
    }

    pub(super) fn relations(&self) -> &[RelationEntry] {
        &self.relations
    }

    pub(super) fn relation(&self, ix: u32) -> &RelationEntry {
        &self.relations[ix as usize]
    }

    pub(super) fn schema(&self, ix: u32) -> &SchemaEntry {
        &self.schemas[ix as usize]
    }

    pub(super) fn database(&self, ix: u32) -> &DatabaseEntry {
        &self.databases[ix as usize]
    }

    pub(super) fn columns(&self, relation: u32) -> &[ColumnEntry] {
        let range = &self.relation(relation).columns;
        &self.columns[range.start as usize..range.end as usize]
    }

    /// Whether more than one database shows, so names say which.
    pub(super) fn several_databases(&self) -> bool {
        self.databases.len() > 1
    }

    /// The relations a written name can mean, best guess first: `t`,
    /// `schema.t` or `db.t`, `db.schema.t`. Names compare ignoring case.
    pub(super) fn find_relations(&self, parts: &[String]) -> Vec<u32> {
        let Some(name) = parts.last() else {
            return Vec::new();
        };
        let Some(found) = self.by_name.get(&name.to_lowercase()) else {
            return Vec::new();
        };
        let lower: Vec<String> = parts.iter().map(|p| p.to_lowercase()).collect();
        let mut found: Vec<u32> = found
            .iter()
            .copied()
            .filter(|&ix| {
                let schema = self.schema(self.relation(ix).schema);
                let database = self.database(schema.database);
                match lower.as_slice() {
                    [_] => true,
                    [qualifier, _] => {
                        schema.lower == *qualifier
                            || (database.lower == *qualifier && !schema.qualify)
                    }
                    [db, sch, _] => database.lower == *db && schema.lower == *sch,
                    _ => false,
                }
            })
            .collect();
        // What the engine would find without a qualifier first, then a
        // name spelled exactly as written.
        found.sort_by_key(|&ix| {
            let relation = self.relation(ix);
            let schema = self.schema(relation.schema);
            (
                self.database(schema.database).qualify || schema.qualify,
                relation.name != *name,
            )
        });
        found
    }

    /// The schemas a written qualifier can mean: `schema`, or `db.schema`.
    /// For engines without schemas, `db` means its one unnamed schema.
    pub(super) fn find_schemas(&self, parts: &[String]) -> Vec<u32> {
        let lower: Vec<String> = parts.iter().map(|p| p.to_lowercase()).collect();
        (0..self.schemas.len() as u32)
            .filter(|&ix| {
                let schema = self.schema(ix);
                let database = self.database(schema.database);
                match lower.as_slice() {
                    [name] => {
                        (!schema.name.is_empty() && schema.lower == *name)
                            || (schema.name.is_empty() && database.lower == *name)
                    }
                    [db, name] => database.lower == *db && schema.lower == *name,
                    _ => false,
                }
            })
            .collect()
    }

    /// The database a written qualifier names, if any.
    pub(super) fn find_database(&self, name: &str) -> Option<u32> {
        let lower = name.to_lowercase();
        self.databases
            .iter()
            .position(|db| db.lower == lower)
            .map(|ix| ix as u32)
    }

    /// The relations directly in schema `schema`, in name order.
    pub(super) fn relations_in(&self, schema: u32) -> impl Iterator<Item = u32> + '_ {
        (0..self.relations.len() as u32).filter(move |&ix| self.relation(ix).schema == schema)
    }

    /// The text that names relation `ix` from the connection's defaults:
    /// just the name, or with the schema and database it needs, quoted as
    /// `dialect` quotes.
    pub(super) fn qualified(&self, ix: u32, dialect: Dialect) -> String {
        let relation = self.relation(ix);
        let schema = self.schema(relation.schema);
        let database = self.database(schema.database);
        let mut parts = Vec::with_capacity(3);
        if database.qualify {
            parts.push(dialect.quote_ident(&database.name));
        }
        if !schema.name.is_empty() && (schema.qualify || database.qualify) {
            parts.push(dialect.quote_ident(&schema.name));
        }
        parts.push(dialect.quote_ident(&relation.name));
        parts.join(".")
    }

    /// Schema `ix` as a qualifier, with its database when that is needed.
    pub(super) fn qualified_schema(&self, ix: u32, dialect: Dialect) -> String {
        let schema = self.schema(ix);
        let database = self.database(schema.database);
        if database.qualify {
            format!(
                "{}.{}",
                dialect.quote_ident(&database.name),
                dialect.quote_ident(&schema.name)
            )
        } else {
            dialect.quote_ident(&schema.name)
        }
    }

    /// Where relation `ix` lives, for the menu: `public`, `shop`,
    /// `memory.main` when several databases show.
    pub(super) fn namespace(&self, ix: u32) -> String {
        let schema = self.schema(self.relation(ix).schema);
        let database = self.database(schema.database);
        match (schema.name.is_empty(), self.several_databases()) {
            (true, _) => database.name.clone(),
            (false, false) => schema.name.clone(),
            (false, true) => format!("{}.{}", database.name, schema.name),
        }
    }
}

/// The schema a dialect searches when a name has none.
fn is_default_schema(dialect: Dialect, schema: &str) -> bool {
    match dialect {
        Dialect::Postgres => schema == "public",
        Dialect::DuckDb => schema == "main",
        _ => true,
    }
}

/// The database a dialect searches when a name has none, where that is
/// known without asking the connection.
fn is_default_database(dialect: Dialect, database: &str) -> bool {
    match dialect {
        Dialect::Sqlite => database == "main" || database == "temp",
        Dialect::ClickHouse => database == "default",
        // Temporary objects, such as the views over opened data files, are
        // found before the current database's.
        Dialect::DuckDb => database == "memory" || database == "temp",
        Dialect::Postgres => true,
        Dialect::MySql | Dialect::MariaDb => false,
    }
}
