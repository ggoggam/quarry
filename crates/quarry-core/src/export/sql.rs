//! SQL literals per dialect, for `INSERT` statements that read back the
//! values they were written from.
//!
//! | Value     | PostgreSQL        | DuckDB            | SQLite      | MySQL, MariaDB | ClickHouse      |
//! |-----------|-------------------|-------------------|-------------|----------------|-----------------|
//! | text      | `'it''s'`         | `'it''s'`         | `'it''s'`   | `'it''s'`, `\\` for `\` | `'it''s'`, `\\` for `\` |
//! | boolean   | `TRUE`            | `TRUE`            | `1`         | `TRUE`         | `true`          |
//! | blob      | `'\x00ff'::bytea` | `'\x00\xFF'::BLOB`| `X'00FF'`   | `X'00FF'`      | `unhex('00FF')` |
//! | NaN, ±∞   | `'NaN'`, `'Infinity'` | `'NaN'`, `'Infinity'` | NULL, `9e999` | NULL | `nan`, `inf` |
//!
//! Numbers and decimals are written as they are; dates, times and
//! timestamps as quoted text (`'2024-01-01 12:00:00'`), which every engine
//! reads as the column's type on insert. Lists, structs and maps are written
//! as their quoted text.

use std::fmt::Write as _;

use crate::Dialect;

/// A column name for an `INSERT`, always quoted: the names come from the
/// result exactly as the engine spelled them, and a keyword or a capital
/// left bare would break the script or name another column.
pub fn identifier(dialect: Dialect, name: &str) -> String {
    let quote = match dialect {
        Dialect::MySql | Dialect::MariaDb | Dialect::ClickHouse => '`',
        _ => '"',
    };
    let doubled = format!("{quote}{quote}");
    format!("{quote}{}{quote}", name.replace(quote, &doubled))
}

/// Whether the dialect reads `\` in a string literal as an escape.
fn backslash_escapes(dialect: Dialect) -> bool {
    matches!(
        dialect,
        Dialect::MySql | Dialect::MariaDb | Dialect::ClickHouse
    )
}

/// `text` as a string literal.
pub fn string(dialect: Dialect, text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('\'');
    for ch in text.chars() {
        match ch {
            '\'' => out.push_str("''"),
            '\\' if backslash_escapes(dialect) => out.push_str("\\\\"),
            '\0' if backslash_escapes(dialect) => out.push_str("\\0"),
            ch => out.push(ch),
        }
    }
    out.push('\'');
    out
}

pub fn boolean(dialect: Dialect, value: bool) -> &'static str {
    match (dialect, value) {
        (Dialect::Sqlite, true) => "1",
        (Dialect::Sqlite, false) => "0",
        (Dialect::ClickHouse, true) => "true",
        (Dialect::ClickHouse, false) => "false",
        (_, true) => "TRUE",
        (_, false) => "FALSE",
    }
}

/// NaN or an infinity, where the dialect can say it; NULL where it cannot.
pub fn non_finite(dialect: Dialect, value: f64) -> String {
    let (nan, inf, neg) = match dialect {
        Dialect::Postgres | Dialect::DuckDb => ("'NaN'", "'Infinity'", "'-Infinity'"),
        Dialect::ClickHouse => ("nan", "inf", "-inf"),
        // SQLite has no NaN, and reads an overflowing literal as infinity.
        Dialect::Sqlite => ("NULL", "9e999", "-9e999"),
        Dialect::MySql | Dialect::MariaDb => ("NULL", "NULL", "NULL"),
    };
    if value.is_nan() {
        nan
    } else if value > 0.0 {
        inf
    } else {
        neg
    }
    .to_owned()
}

/// Bytes as a blob literal.
pub fn blob(dialect: Dialect, bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 4 + 16);
    match dialect {
        Dialect::Postgres => {
            out.push_str("'\\x");
            for b in bytes {
                let _ = write!(out, "{b:02x}");
            }
            out.push_str("'::bytea");
        }
        Dialect::DuckDb => {
            out.push('\'');
            for b in bytes {
                let _ = write!(out, "\\x{b:02X}");
            }
            out.push_str("'::BLOB");
        }
        Dialect::Sqlite | Dialect::MySql | Dialect::MariaDb => {
            out.push_str("X'");
            for b in bytes {
                let _ = write!(out, "{b:02X}");
            }
            out.push('\'');
        }
        Dialect::ClickHouse => {
            out.push_str("unhex('");
            for b in bytes {
                let _ = write!(out, "{b:02X}");
            }
            out.push_str("')");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_quotes_column_names() {
        assert_eq!(identifier(Dialect::DuckDb, "at"), "\"at\"");
        assert_eq!(identifier(Dialect::MySql, "or`der"), "`or``der`");
        assert_eq!(identifier(Dialect::Postgres, "Amount"), "\"Amount\"");
    }

    #[test]
    fn quotes_strings_per_dialect() {
        let text = "it's a \\ path\nnext";
        for dialect in [Dialect::Postgres, Dialect::DuckDb, Dialect::Sqlite] {
            assert_eq!(string(dialect, text), "'it''s a \\ path\nnext'");
        }
        for dialect in [Dialect::MySql, Dialect::MariaDb, Dialect::ClickHouse] {
            assert_eq!(string(dialect, text), "'it''s a \\\\ path\nnext'");
        }
        assert_eq!(string(Dialect::MySql, "a\0b"), "'a\\0b'");
        assert_eq!(string(Dialect::Postgres, ""), "''");
    }

    #[test]
    fn writes_blobs_booleans_and_non_finite_numbers() {
        let bytes = [0x00, 0xab, 0x10];
        assert_eq!(blob(Dialect::Postgres, &bytes), "'\\x00ab10'::bytea");
        assert_eq!(blob(Dialect::DuckDb, &bytes), "'\\x00\\xAB\\x10'::BLOB");
        assert_eq!(blob(Dialect::Sqlite, &bytes), "X'00AB10'");
        assert_eq!(blob(Dialect::MariaDb, &bytes), "X'00AB10'");
        assert_eq!(blob(Dialect::ClickHouse, &bytes), "unhex('00AB10')");
        assert_eq!(boolean(Dialect::Sqlite, true), "1");
        assert_eq!(boolean(Dialect::Postgres, false), "FALSE");
        assert_eq!(
            non_finite(Dialect::Postgres, f64::NEG_INFINITY),
            "'-Infinity'"
        );
        assert_eq!(non_finite(Dialect::DuckDb, f64::NAN), "'NaN'");
        assert_eq!(non_finite(Dialect::Sqlite, f64::INFINITY), "9e999");
        assert_eq!(non_finite(Dialect::Sqlite, f64::NAN), "NULL");
        assert_eq!(non_finite(Dialect::MySql, f64::INFINITY), "NULL");
        assert_eq!(non_finite(Dialect::ClickHouse, f64::NEG_INFINITY), "-inf");
    }
}
