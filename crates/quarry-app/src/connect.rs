//! Turning what the user typed (a file path or a URL) into a driver.
//!
//! - `postgres://` and `postgresql://` URLs open PostgreSQL.
//! - `mysql://` and `mariadb://` URLs open MySQL or MariaDB (the server says which).
//! - `clickhouse://`, `clickhouses://`, `http://` and `https://` URLs open ClickHouse.
//! - `sqlite:<path>`, `duckdb:<path>` and `:memory:` pick an embedded engine.
//! - A bare path opens the file with the engine whose header it has: SQLite
//!   files start with `SQLite format 3`, DuckDB files carry `DUCK` at byte 8.
//!   A new file's extension decides (`.sqlite`, `.sqlite3`, `.db` mean SQLite).
//! - Data files (CSV, JSON, Parquet, Arrow, Excel, Avro) are not connections:
//!   they are added to a DuckDB connection, see [`PathKind`].

use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use quarry_core::{Dialect, Driver};

/// What to connect to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    InMemory(Dialect),
    File { dialect: Dialect, path: PathBuf },
    Server { dialect: Dialect, url: String },
}

impl Target {
    /// Read a command-line argument or the connect dialog's text.
    pub fn parse(input: &str) -> anyhow::Result<Target> {
        let input = input.trim();
        anyhow::ensure!(!input.is_empty(), "enter a file path or a connection URL");
        if input == ":memory:" {
            return Ok(Target::InMemory(Dialect::DuckDb));
        }
        if let Some((scheme, rest)) = input.split_once(':') {
            let server = |dialect| Target::Server {
                dialect,
                url: input.to_owned(),
            };
            match scheme.to_ascii_lowercase().as_str() {
                "postgres" | "postgresql" => return Ok(server(Dialect::Postgres)),
                "mysql" | "mariadb" => return Ok(server(Dialect::MySql)),
                "clickhouse" | "clickhouses" | "http" | "https" => {
                    return Ok(server(Dialect::ClickHouse));
                }
                "sqlite" | "duckdb" => {
                    let dialect = if scheme.eq_ignore_ascii_case("sqlite") {
                        Dialect::Sqlite
                    } else {
                        Dialect::DuckDb
                    };
                    let path = rest.trim_start_matches("//");
                    return Ok(if path.is_empty() || path == ":memory:" {
                        Target::InMemory(dialect)
                    } else {
                        Target::File {
                            dialect,
                            path: PathBuf::from(expand_home(path)),
                        }
                    });
                }
                _ => {}
            }
        }
        let path = PathBuf::from(expand_home(input));
        Ok(Target::File {
            dialect: file_dialect(&path),
            path,
        })
    }

    /// The text [`Target::parse`] reads back as this target. The engine is
    /// spelled out for files, so a saved file opens with the engine it was
    /// saved with even before the file exists.
    pub fn to_input(&self) -> String {
        match self {
            Target::InMemory(Dialect::Sqlite) => "sqlite::memory:".to_owned(),
            Target::InMemory(_) => ":memory:".to_owned(),
            Target::File {
                dialect: Dialect::Sqlite,
                path,
            } => format!("sqlite:{}", path.display()),
            Target::File { path, .. } => format!("duckdb:{}", path.display()),
            Target::Server { url, .. } => url.clone(),
        }
    }

    pub fn is_server(&self) -> bool {
        matches!(self, Target::Server { .. })
    }

    /// The URL without the password in it, and the password, decoded.
    pub fn take_password(self) -> (Target, Option<String>) {
        match self {
            Target::Server { dialect, url } => {
                let password = url_password(&url);
                let url = redact(&url);
                (Target::Server { dialect, url }, password)
            }
            other => (other, None),
        }
    }

    /// A short name for a connection that has not been given one: the file's
    /// name, or the server URL's `user@host/db`.
    pub fn default_name(&self) -> String {
        match self {
            Target::InMemory(dialect) => format!("In-memory {dialect}"),
            Target::File { path, .. } => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            Target::Server { url, .. } => {
                let url = redact(url);
                let rest = url.split_once("://").map_or(url.as_str(), |(_, rest)| rest);
                rest.split(['?', '#'])
                    .next()
                    .unwrap_or(rest)
                    .trim_end_matches('/')
                    .to_owned()
            }
        }
    }

    /// How the target reads in the sidebar: the redacted URL, or the path
    /// with the home folder as `~`.
    pub fn describe(&self) -> String {
        match self {
            Target::InMemory(dialect) => format!("in-memory {dialect}"),
            Target::File { path, .. } => {
                let home = std::env::var_os("HOME").map(PathBuf::from);
                match home
                    .as_deref()
                    .and_then(|home| path.strip_prefix(home).ok())
                {
                    Some(rest) => format!("~/{}", rest.display()),
                    None => path.display().to_string(),
                }
            }
            Target::Server { url, .. } => redact(url),
        }
    }

    /// The URL or path with a password added, for a password typed in its
    /// own field instead of the URL.
    pub fn with_password(self, password: &str) -> Target {
        match self {
            Target::Server { dialect, url } if !password.is_empty() => Target::Server {
                dialect,
                url: insert_password(&url, password),
            },
            other => other,
        }
    }
}

/// What `quarry` was started with: `.sql` files to open in editor tabs, data
/// files to query, and the database to connect to (the first argument that
/// is neither).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Launch {
    pub target: Option<String>,
    pub scripts: Vec<PathBuf>,
    pub files: Vec<PathBuf>,
    /// Further databases, which Quarry does not open: it has one connection
    /// at a time.
    pub ignored: Vec<String>,
}

impl Launch {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Launch {
        let mut launch = Launch::default();
        for arg in args {
            match bare_path(&arg).map(|path| (path_kind(&path), path)) {
                Some((PathKind::Script, path)) => launch.scripts.push(path),
                Some((PathKind::Data, path)) => launch.files.push(path),
                _ if launch.target.is_none() => launch.target = Some(arg),
                _ => launch.ignored.push(arg),
            }
        }
        launch
    }
}

/// The path an argument names as a bare path; `sqlite:x.sql` and URLs name
/// databases.
fn bare_path(arg: &str) -> Option<PathBuf> {
    let path = PathBuf::from(expand_home(arg.trim()));
    match Target::parse(arg) {
        Ok(Target::File { path: parsed, .. }) if parsed == path => Some(path),
        _ => None,
    }
}

/// What a file opened, dropped on the window or named on the command line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// SQL to edit.
    Script,
    /// A data file to query, added to a DuckDB connection.
    Data,
    /// A database to connect to.
    Database,
}

pub fn path_kind(path: &Path) -> PathKind {
    if is_sql_script(path) {
        PathKind::Script
    } else if header_dialect(path).is_none() && quarry_duckdb::DataFormat::detect(path).is_some() {
        PathKind::Data
    } else {
        PathKind::Database
    }
}

/// Whether a file is SQL text to edit rather than a database to open.
pub fn is_sql_script(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("sql"))
}

/// Open `target`. Blocks on the network for servers, so call it off the UI
/// thread.
pub fn open(target: &Target) -> anyhow::Result<Box<dyn Driver>> {
    Ok(match target {
        Target::InMemory(Dialect::Sqlite) => Box::new(quarry_sqlite::SqliteDriver::in_memory()?),
        Target::InMemory(_) => Box::new(quarry_duckdb::DuckDbDriver::in_memory()?),
        Target::File {
            dialect: Dialect::Sqlite,
            path,
        } => Box::new(quarry_sqlite::SqliteDriver::open_path(path)?),
        Target::File { path, .. } => Box::new(quarry_duckdb::DuckDbDriver::open(
            quarry_duckdb::Target::File {
                path: path.clone(),
                read_only: false,
            },
        )?),
        Target::Server { dialect, url } => open_server(*dialect, url)
            .with_context(|| format!("connecting to {}", dialect.name()))?,
    })
}

fn open_server(dialect: Dialect, url: &str) -> anyhow::Result<Box<dyn Driver>> {
    Ok(match dialect {
        Dialect::Postgres => Box::new(quarry_postgres::PostgresDriver::connect(url)?),
        Dialect::MySql | Dialect::MariaDb => Box::new(quarry_mysql::MySqlDriver::connect(url)?),
        Dialect::ClickHouse => Box::new(quarry_clickhouse::ClickHouseDriver::connect(url)?),
        _ => anyhow::bail!(
            "{dialect} connections are not available yet ({})",
            redact(url)
        ),
    })
}

/// The engine a file belongs to, from its header or, for a file that does
/// not exist yet, its extension.
pub fn file_dialect(path: &Path) -> Dialect {
    if let Some(dialect) = header_dialect(path) {
        return dialect;
    }
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("sqlite" | "sqlite3" | "db" | "db3" | "s3db" | "sl3") => Dialect::Sqlite,
        _ => Dialect::DuckDb,
    }
}

/// The engine whose header the file starts with, if any.
fn header_dialect(path: &Path) -> Option<Dialect> {
    let mut header = [0u8; 16];
    let read = File::open(path)
        .and_then(|mut file| file.read(&mut header))
        .unwrap_or(0);
    if read >= 16 && &header == b"SQLite format 3\0" {
        return Some(Dialect::Sqlite);
    }
    if read >= 12 && &header[8..12] == b"DUCK" {
        return Some(Dialect::DuckDb);
    }
    None
}

fn expand_home(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest).display().to_string(),
        _ => path.to_owned(),
    }
}

/// `scheme://user@host` → `scheme://user:password@host`, percent-encoding
/// the password. A URL that already has a password keeps it.
fn insert_password(url: &str, password: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let encoded: String = password
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            b => format!("%{b:02X}"),
        })
        .collect();
    match authority.rsplit_once('@') {
        Some((user, _)) if user.contains(':') => url.to_owned(),
        Some((user, host)) => format!("{scheme}://{user}:{encoded}@{host}{tail}"),
        None => url.to_owned(),
    }
}

/// The password in `scheme://user:password@host`, percent-decoded.
fn url_password(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (user, _) = rest[..authority_end].rsplit_once('@')?;
    let (_, password) = user.split_once(':')?;
    Some(percent_decode(password))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            )
        {
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The URL without its password, for messages.
pub fn redact(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    match authority.rsplit_once('@') {
        Some((user, host)) => {
            let user = user.split(':').next().unwrap_or_default();
            format!("{scheme}://{user}@{host}{tail}")
        }
        None => url.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_targets() {
        let server = |dialect, url: &str| Target::Server {
            dialect,
            url: url.into(),
        };
        assert_eq!(
            Target::parse("postgres://a@h/db").unwrap(),
            server(Dialect::Postgres, "postgres://a@h/db")
        );
        assert_eq!(
            Target::parse(" mariadb://root@h:3306 ").unwrap(),
            server(Dialect::MySql, "mariadb://root@h:3306")
        );
        assert_eq!(
            Target::parse("https://ch.example.com:8443").unwrap(),
            server(Dialect::ClickHouse, "https://ch.example.com:8443")
        );
        assert_eq!(
            Target::parse(":memory:").unwrap(),
            Target::InMemory(Dialect::DuckDb)
        );
        assert_eq!(
            Target::parse("sqlite::memory:").unwrap(),
            Target::InMemory(Dialect::Sqlite)
        );
        assert_eq!(
            Target::parse("sqlite:data/app.bin").unwrap(),
            Target::File {
                dialect: Dialect::Sqlite,
                path: "data/app.bin".into()
            }
        );
        assert!(Target::parse("  ").is_err());
    }

    #[test]
    fn sql_files_on_the_command_line_open_in_the_editor() {
        let launch = Launch::parse(
            [
                "report.sql",
                "shop.duckdb",
                "~/q/Setup.SQL",
                "sqlite:odd.sql",
                "orders.csv",
                "postgres://a@h/db",
                "~/d/events.parquet",
            ]
            .map(String::from),
        );
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            launch,
            Launch {
                target: Some("shop.duckdb".into()),
                scripts: vec![
                    "report.sql".into(),
                    PathBuf::from(format!("{home}/q/Setup.SQL"))
                ],
                files: vec![
                    "orders.csv".into(),
                    PathBuf::from(format!("{home}/d/events.parquet"))
                ],
                ignored: vec!["sqlite:odd.sql".into(), "postgres://a@h/db".into()],
            }
        );
        assert_eq!(Launch::parse([]), Launch::default());
        assert!(!is_sql_script(Path::new("data.sqlite")));
        assert!(!is_sql_script(Path::new("sql")));
    }

    #[test]
    fn tells_data_files_from_databases_and_scripts() {
        let dir = tempfile::tempdir().unwrap();
        let at = |name: &str, bytes: &[u8]| {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            path_kind(&path)
        };
        assert_eq!(at("a.csv", b"id\n1\n"), PathKind::Data);
        assert_eq!(at("a.parquet", b"PAR1"), PathKind::Data);
        assert_eq!(at("a.feather", b"ARROW1"), PathKind::Data);
        assert_eq!(at("a.sql", b"SELECT 1"), PathKind::Script);
        assert_eq!(
            at("a.duckdb", b"\0\0\0\0\0\0\0\0DUCK\x40\0\0\0"),
            PathKind::Database
        );
        // A database's header wins over a data file's name.
        assert_eq!(at("odd.csv", b"SQLite format 3\0"), PathKind::Database);
        assert_eq!(path_kind(&dir.path().join("new.db")), PathKind::Database);
    }

    #[test]
    fn picks_the_engine_from_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let sqlite = dir.path().join("x.bin");
        std::fs::write(&sqlite, b"SQLite format 3\0rest of the header").unwrap();
        assert_eq!(file_dialect(&sqlite), Dialect::Sqlite);
        let duck = dir.path().join("y.db");
        std::fs::write(&duck, b"\0\0\0\0\0\0\0\0DUCK\x40\0\0\0").unwrap();
        assert_eq!(file_dialect(&duck), Dialect::DuckDb);
        assert_eq!(
            file_dialect(&dir.path().join("new.sqlite3")),
            Dialect::Sqlite
        );
        assert_eq!(
            file_dialect(&dir.path().join("new.duckdb")),
            Dialect::DuckDb
        );
    }

    #[test]
    fn passwords_go_into_the_url_and_out_of_messages() {
        assert_eq!(
            insert_password("postgres://alice@db:5432/app?sslmode=require", "p@ss/w:rd"),
            "postgres://alice:p%40ss%2Fw%3Ard@db:5432/app?sslmode=require"
        );
        assert_eq!(
            insert_password("mysql://root:old@h/db", "new"),
            "mysql://root:old@h/db"
        );
        assert_eq!(
            redact("postgres://alice:secret@db/app"),
            "postgres://alice@db/app"
        );
        assert_eq!(redact("clickhouse://h:8123"), "clickhouse://h:8123");
    }

    #[test]
    fn targets_round_trip_through_their_input() {
        for target in [
            Target::InMemory(Dialect::DuckDb),
            Target::InMemory(Dialect::Sqlite),
            Target::File {
                dialect: Dialect::Sqlite,
                path: "/data/app.bin".into(),
            },
            Target::File {
                dialect: Dialect::DuckDb,
                path: "/data/shop.db".into(),
            },
            Target::Server {
                dialect: Dialect::Postgres,
                url: "postgres://a@h:5432/db".into(),
            },
        ] {
            assert_eq!(Target::parse(&target.to_input()).unwrap(), target);
        }
    }

    #[test]
    fn passwords_come_out_of_urls_decoded() {
        let (target, password) = Target::parse("postgres://alice:p%40ss%2Fw%3Ard@db:5432/app")
            .unwrap()
            .take_password();
        assert_eq!(password.as_deref(), Some("p@ss/w:rd"));
        assert_eq!(target.to_input(), "postgres://alice@db:5432/app");
        let (_, password) = Target::parse("mysql://root@h/db").unwrap().take_password();
        assert_eq!(password, None);
        // Taking it out and putting it back gives the same URL.
        let url = "mysql://root:%C3%A9t%C3%A9%25@h/db";
        let (target, password) = Target::parse(url).unwrap().take_password();
        assert_eq!(target.with_password(&password.unwrap()).to_input(), url);
    }

    #[test]
    fn names_connections_that_have_none() {
        let name = |input: &str| Target::parse(input).unwrap().default_name();
        assert_eq!(
            name("postgres://alice:secret@db:5432/app?sslmode=require"),
            "alice@db:5432/app"
        );
        assert_eq!(name("clickhouse://h:8123/"), "h:8123");
        assert_eq!(name("sqlite:/data/app.sqlite"), "app.sqlite");
    }

    #[test]
    fn opens_embedded_engines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sqlite");
        let driver = open(&Target::parse(path.to_str().unwrap()).unwrap()).unwrap();
        assert_eq!(driver.flavor().dialect, Dialect::Sqlite);
        let driver = open(&Target::InMemory(Dialect::DuckDb)).unwrap();
        assert_eq!(driver.flavor().dialect, Dialect::DuckDb);
    }
}
