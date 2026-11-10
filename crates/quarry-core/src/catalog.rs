//! The database objects a connection exposes, for the schema browser.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    pub databases: Vec<Database>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Database {
    pub name: String,
    pub schemas: Vec<Schema>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    /// Empty for engines without schemas (MySQL, MariaDB, ClickHouse,
    /// SQLite), whose relations sit directly in the database.
    pub name: String,
    pub relations: Vec<Relation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RelationKind {
    Table,
    View,
    MaterializedView,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    pub name: String,
    pub kind: RelationKind,
    pub columns: Vec<ColumnDef>,
    /// Row count when the engine knows it without scanning.
    pub estimated_rows: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
}
