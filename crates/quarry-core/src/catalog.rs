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
    pub name: String,
    pub relations: Vec<Relation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RelationKind {
    Table,
    View,
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

/// Quote an identifier for SQL, doubling embedded quotes.
pub fn quote_ident(name: &str) -> String {
    let plain = !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if plain {
        name.to_owned()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// `database.schema.relation`, each part quoted as needed.
pub fn qualified_name(database: &str, schema: &str, relation: &str) -> String {
    [database, schema, relation]
        .iter()
        .map(|part| quote_ident(part))
        .collect::<Vec<_>>()
        .join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_only_when_needed() {
        assert_eq!(quote_ident("orders"), "orders");
        assert_eq!(quote_ident("Orders"), "\"Orders\"");
        assert_eq!(quote_ident("my table"), "\"my table\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_ident("1st"), "\"1st\"");
        assert_eq!(qualified_name("memory", "main", "t"), "memory.main.t");
    }
}
