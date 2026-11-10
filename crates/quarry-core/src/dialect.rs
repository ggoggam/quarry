//! SQL dialects and engine versions.
//!
//! Every engine speaks its own SQL: how identifiers are quoted, which
//! comments and string forms exist, how objects are named. A [`Flavor`] is a
//! dialect plus the version of the engine a connection reached, so features
//! can be switched on for the releases that have them.

use std::fmt;

/// The SQL dialect an engine speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Dialect {
    DuckDb,
    Postgres,
    Sqlite,
    MySql,
    MariaDb,
    ClickHouse,
}

impl Dialect {
    pub const ALL: [Dialect; 6] = [
        Dialect::DuckDb,
        Dialect::Postgres,
        Dialect::Sqlite,
        Dialect::MySql,
        Dialect::MariaDb,
        Dialect::ClickHouse,
    ];

    /// The engine's name for display, e.g. `"PostgreSQL"`.
    pub fn name(self) -> &'static str {
        match self {
            Dialect::DuckDb => "DuckDB",
            Dialect::Postgres => "PostgreSQL",
            Dialect::Sqlite => "SQLite",
            Dialect::MySql => "MySQL",
            Dialect::MariaDb => "MariaDB",
            Dialect::ClickHouse => "ClickHouse",
        }
    }

    /// A stable lowercase id, e.g. `"postgres"`.
    pub fn id(self) -> &'static str {
        match self {
            Dialect::DuckDb => "duckdb",
            Dialect::Postgres => "postgres",
            Dialect::Sqlite => "sqlite",
            Dialect::MySql => "mysql",
            Dialect::MariaDb => "mariadb",
            Dialect::ClickHouse => "clickhouse",
        }
    }

    /// The lexical rules [`crate::sql`] needs to find statement boundaries.
    pub fn lexical(self) -> Lexical {
        match self {
            Dialect::DuckDb | Dialect::Postgres => Lexical {
                hash_comments: HashComments::None,
                double_dash_needs_space: false,
                nested_block_comments: true,
                backtick_identifiers: false,
                bracket_identifiers: false,
                backslash_escapes: false,
                escape_strings: true,
                dollar_quotes: true,
                delimiter_command: false,
            },
            Dialect::Sqlite => Lexical {
                hash_comments: HashComments::None,
                double_dash_needs_space: false,
                nested_block_comments: false,
                backtick_identifiers: true,
                bracket_identifiers: true,
                backslash_escapes: false,
                escape_strings: false,
                dollar_quotes: false,
                delimiter_command: false,
            },
            Dialect::MySql | Dialect::MariaDb => Lexical {
                hash_comments: HashComments::Always,
                double_dash_needs_space: true,
                nested_block_comments: false,
                backtick_identifiers: true,
                bracket_identifiers: false,
                backslash_escapes: true,
                escape_strings: false,
                dollar_quotes: false,
                delimiter_command: true,
            },
            Dialect::ClickHouse => Lexical {
                hash_comments: HashComments::BeforeSpaceOrBang,
                double_dash_needs_space: false,
                nested_block_comments: true,
                backtick_identifiers: true,
                bracket_identifiers: false,
                backslash_escapes: true,
                escape_strings: false,
                dollar_quotes: false,
                delimiter_command: false,
            },
        }
    }

    /// Whether objects live in schemas inside a database. MySQL, MariaDB
    /// and ClickHouse call their one namespace level a database; SQLite's
    /// attached databases hold tables directly.
    pub fn has_schemas(self) -> bool {
        matches!(self, Dialect::DuckDb | Dialect::Postgres)
    }

    /// Quote an identifier as this dialect expects, only when needed.
    pub fn quote_ident(self, name: &str) -> String {
        let quote = match self {
            Dialect::MySql | Dialect::MariaDb | Dialect::ClickHouse => '`',
            _ => '"',
        };
        // PostgreSQL and DuckDB fold unquoted names to lower case, so only
        // lower case names are safe bare. MySQL and ClickHouse keep case;
        // SQLite ignores it. Keywords are always quoted where it matters, by
        // quoting anything that is not a plain lower case word for the
        // folding engines and anything that is not a plain word elsewhere.
        let folds = matches!(self, Dialect::DuckDb | Dialect::Postgres);
        let plain = name.chars().next().is_some_and(|c| {
            c == '_' || c.is_ascii_lowercase() || (!folds && c.is_ascii_uppercase())
        }) && name.chars().all(|c| {
            c == '_'
                || c.is_ascii_digit()
                || c.is_ascii_lowercase()
                || (!folds && c.is_ascii_uppercase())
        });
        if plain && !is_reserved(name) {
            name.to_owned()
        } else {
            let doubled = format!("{quote}{quote}");
            format!("{quote}{}{quote}", name.replace(quote, &doubled))
        }
    }

    /// The name a query uses for a relation shown in the schema browser.
    /// Empty parts (a database without schemas) are left out. PostgreSQL can
    /// only see the database it is connected to, so its name is dropped too.
    pub fn relation_name(self, database: &str, schema: &str, relation: &str) -> String {
        let database = if self == Dialect::Postgres {
            ""
        } else {
            database
        };
        [database, schema, relation]
            .iter()
            .filter(|part| !part.is_empty())
            .map(|part| self.quote_ident(part))
            .collect::<Vec<_>>()
            .join(".")
    }

    /// Whether a statement may add, drop or rename objects, so the schema
    /// browser should reload after it.
    pub fn changes_schema(self, statement: &str) -> bool {
        let first = statement
            .split(|c: char| !c.is_ascii_alphabetic())
            .find(|word| !word.is_empty())
            .unwrap_or_default()
            .to_ascii_uppercase();
        let common = matches!(
            first.as_str(),
            "CREATE" | "DROP" | "ALTER" | "RENAME" | "TRUNCATE" | "COMMENT" | "IMPORT"
        );
        common
            || match self {
                Dialect::DuckDb => matches!(first.as_str(), "ATTACH" | "DETACH" | "USE"),
                Dialect::Sqlite => matches!(first.as_str(), "ATTACH" | "DETACH"),
                Dialect::Postgres => false,
                Dialect::MySql | Dialect::MariaDb => matches!(first.as_str(), "USE"),
                Dialect::ClickHouse => {
                    matches!(first.as_str(), "ATTACH" | "DETACH" | "USE" | "EXCHANGE")
                }
            }
    }
}

impl fmt::Display for Dialect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Words that must be quoted as identifiers in every dialect Quarry speaks.
/// Not exhaustive: the engines reserve more, but these are the ones a table
/// or column is plausibly named after.
fn is_reserved(word: &str) -> bool {
    const RESERVED: &[&str] = &[
        "all",
        "and",
        "as",
        "asc",
        "between",
        "by",
        "case",
        "check",
        "column",
        "constraint",
        "create",
        "cross",
        "default",
        "delete",
        "desc",
        "distinct",
        "drop",
        "else",
        "end",
        "exists",
        "false",
        "for",
        "foreign",
        "from",
        "full",
        "group",
        "having",
        "in",
        "index",
        "inner",
        "insert",
        "into",
        "is",
        "join",
        "key",
        "left",
        "like",
        "limit",
        "not",
        "null",
        "on",
        "or",
        "order",
        "outer",
        "primary",
        "references",
        "right",
        "select",
        "set",
        "table",
        "then",
        "to",
        "true",
        "union",
        "unique",
        "update",
        "user",
        "using",
        "values",
        "when",
        "where",
        "with",
    ];
    RESERVED.contains(&word.to_ascii_lowercase().as_str())
}

/// Whether `#` starts a line comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashComments {
    None,
    /// MySQL and MariaDB: `#` always starts a comment.
    Always,
    /// ClickHouse: `# ` and `#!` start a comment.
    BeforeSpaceOrBang,
}

/// What a statement scanner must know to tell a real `;` from one inside a
/// string, identifier or comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lexical {
    pub hash_comments: HashComments,
    /// MySQL only treats `--` as a comment when whitespace follows it.
    pub double_dash_needs_space: bool,
    pub nested_block_comments: bool,
    pub backtick_identifiers: bool,
    /// SQLite's `[name]` identifiers.
    pub bracket_identifiers: bool,
    /// `\` escapes a quote inside ordinary string literals.
    pub backslash_escapes: bool,
    /// PostgreSQL's `E'...'` strings, where `\` escapes.
    pub escape_strings: bool,
    /// PostgreSQL's `$tag$ ... $tag$` strings.
    pub dollar_quotes: bool,
    /// The mysql client's `DELIMITER //` command, for routine bodies.
    pub delimiter_command: bool,
}

/// An engine release, compared component by component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// The first `major[.minor[.patch]]` in `text`, so whole version banners
    /// parse: `"PostgreSQL 16.4 (Debian ...)"`, `"v1.4.1"`,
    /// `"11.4.2-MariaDB-ubu2404"`, `"24.8.4.13"`, `"17beta1"`.
    pub fn parse(text: &str) -> Option<Self> {
        let start = text.find(|c: char| c.is_ascii_digit())?;
        let mut parts = text[start..]
            .split('.')
            .map(|part| {
                let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
                (digits.len() == part.len(), digits.parse::<u32>().ok())
            })
            .peekable();
        let mut numbers = [0u32; 3];
        for slot in &mut numbers {
            let Some((whole, Some(n))) = parts.next() else {
                break;
            };
            *slot = n;
            if !whole {
                break;
            }
        }
        Some(Self::new(numbers[0], numbers[1], numbers[2]))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A dialect at the version of the engine a connection reached.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Flavor {
    pub dialect: Dialect,
    /// Parsed for comparisons. `None` when the engine did not say.
    pub version: Option<Version>,
    /// The version as the engine reports it, shortened for display, e.g.
    /// `"16.4"` or `"8.0.39"`.
    pub version_text: String,
}

impl Flavor {
    pub fn new(dialect: Dialect, version_text: impl Into<String>) -> Self {
        let version_text = version_text.into();
        Self {
            dialect,
            version: Version::parse(&version_text),
            version_text,
        }
    }

    /// A flavor whose version is unknown; every version check passes.
    pub fn unversioned(dialect: Dialect) -> Self {
        Self {
            dialect,
            version: None,
            version_text: String::new(),
        }
    }

    /// Whether the engine is at least `major.minor`. An unknown version is
    /// assumed to be recent.
    pub fn at_least(&self, major: u32, minor: u32) -> bool {
        self.version
            .is_none_or(|v| (v.major, v.minor) >= (major, minor))
    }

    /// Whether a feature introduced in `since` exists here.
    pub fn supports(&self, since: Version) -> bool {
        self.version.is_none_or(|v| v >= since)
    }

    /// `"PostgreSQL 16.4"`, or just the name when the version is unknown.
    pub fn label(&self) -> String {
        if self.version_text.is_empty() {
            self.dialect.name().to_owned()
        } else {
            format!("{} {}", self.dialect.name(), self.version_text)
        }
    }
}

impl fmt::Display for Flavor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_version_banners() {
        let v = |s| Version::parse(s).unwrap();
        assert_eq!(v("16.4"), Version::new(16, 4, 0));
        assert_eq!(
            v("PostgreSQL 16.4 (Debian 16.4-1.pgdg120+2) on x86_64"),
            Version::new(16, 4, 0)
        );
        assert_eq!(v("v1.4.1"), Version::new(1, 4, 1));
        assert_eq!(v("11.4.2-MariaDB-ubu2404"), Version::new(11, 4, 2));
        assert_eq!(v("8.0.39"), Version::new(8, 0, 39));
        assert_eq!(v("24.8.4.13"), Version::new(24, 8, 4));
        assert_eq!(v("17beta1"), Version::new(17, 0, 0));
        assert_eq!(v("3.46.1"), Version::new(3, 46, 1));
        assert_eq!(Version::parse("unknown"), None);
    }

    #[test]
    fn compares_versions() {
        let pg14 = Flavor::new(Dialect::Postgres, "14.12");
        assert!(pg14.at_least(14, 0));
        assert!(!pg14.at_least(15, 0));
        assert!(pg14.supports(Version::new(13, 0, 0)));
        assert!(!pg14.supports(Version::new(14, 13, 0)));
        assert!(Flavor::unversioned(Dialect::Postgres).at_least(99, 0));
        assert_eq!(pg14.label(), "PostgreSQL 14.12");
    }

    #[test]
    fn quotes_identifiers_per_dialect() {
        use Dialect::*;
        assert_eq!(Postgres.quote_ident("orders"), "orders");
        assert_eq!(Postgres.quote_ident("Orders"), "\"Orders\"");
        assert_eq!(DuckDb.quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(Postgres.quote_ident("1st"), "\"1st\"");
        assert_eq!(Postgres.quote_ident("order"), "\"order\"");
        assert_eq!(MySql.quote_ident("Orders"), "Orders");
        assert_eq!(MySql.quote_ident("my table"), "`my table`");
        assert_eq!(ClickHouse.quote_ident("a`b"), "`a``b`");
        assert_eq!(Sqlite.quote_ident("Orders"), "Orders");
        assert_eq!(Sqlite.quote_ident("group"), "\"group\"");
    }

    #[test]
    fn names_relations_per_dialect() {
        use Dialect::*;
        assert_eq!(DuckDb.relation_name("memory", "main", "t"), "memory.main.t");
        assert_eq!(Postgres.relation_name("app", "public", "t"), "public.t");
        assert_eq!(MySql.relation_name("shop", "", "Orders"), "shop.Orders");
        assert_eq!(Sqlite.relation_name("main", "", "t"), "main.t");
    }

    #[test]
    fn spots_schema_changes() {
        use Dialect::*;
        assert!(DuckDb.changes_schema("create table t (a int)"));
        assert!(Postgres.changes_schema("  DROP VIEW v"));
        assert!(MySql.changes_schema("rename table a to b"));
        assert!(Sqlite.changes_schema("attach 'x.db' as x"));
        assert!(!Postgres.changes_schema("attach 'x.db' as x"));
        assert!(!DuckDb.changes_schema("select 1"));
        assert!(!MySql.changes_schema("insert into t values (1)"));
        assert!(ClickHouse.changes_schema("EXCHANGE TABLES a AND b"));
    }
}
