//! Turning what the user typed (a file path or a URL) into a driver.
//!
//! - `postgres://` and `postgresql://` URLs open PostgreSQL.
//! - `mysql://` and `mariadb://` URLs open MySQL or MariaDB (the server says which).
//! - `clickhouse://`, `clickhouses://`, `http://` and `https://` URLs open ClickHouse.
//! - `sqlite:<path>`, `duckdb:<path>` and `:memory:` pick an embedded engine.
//! - A bare path opens the file with the engine whose header it has: SQLite
//!   files start with `SQLite format 3`, DuckDB files carry `DUCK` at byte 8.
//!   A new file's extension decides (`.sqlite`, `.sqlite3`, `.db` mean SQLite).

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
    let mut header = [0u8; 16];
    let read = File::open(path)
        .and_then(|mut file| file.read(&mut header))
        .unwrap_or(0);
    if read >= 16 && &header == b"SQLite format 3\0" {
        return Dialect::Sqlite;
    }
    if read >= 12 && &header[8..12] == b"DUCK" {
        return Dialect::DuckDb;
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
    fn opens_embedded_engines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sqlite");
        let driver = open(&Target::parse(path.to_str().unwrap()).unwrap()).unwrap();
        assert_eq!(driver.flavor().dialect, Dialect::Sqlite);
        let driver = open(&Target::InMemory(Dialect::DuckDb)).unwrap();
        assert_eq!(driver.flavor().dialect, Dialect::DuckDb);
    }
}
