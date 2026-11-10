//! PostgreSQL's own error text, rather than the client library's.

use tokio_postgres::error::ErrorPosition;

/// The server's message with its DETAIL, HINT and CONTEXT lines, the
/// position in `sql` when the server gave one, and the SQLSTATE. Errors that
/// did not come from the server (I/O, TLS) keep tokio-postgres's text.
pub fn describe(err: &tokio_postgres::Error, sql: Option<&str>) -> String {
    let Some(db) = err.as_db_error() else {
        // I/O and TLS errors hide their cause under the top-level message,
        // e.g. the rustls reason a certificate was rejected.
        let mut text = err.to_string();
        let mut cause = std::error::Error::source(err);
        while let Some(next) = cause {
            let message = next.to_string();
            if !text.contains(&message) {
                text.push_str(": ");
                text.push_str(&message);
            }
            cause = next.source();
        }
        return text;
    };
    let mut out = String::new();
    if matches!(db.severity(), "FATAL" | "PANIC") {
        out.push_str(db.severity());
        out.push_str(": ");
    }
    out.push_str(db.message());
    for (label, value) in [
        ("DETAIL", db.detail()),
        ("HINT", db.hint()),
        ("CONTEXT", db.where_()),
    ] {
        if let Some(value) = value {
            out.push('\n');
            out.push_str(label);
            out.push_str(": ");
            out.push_str(value);
        }
    }
    out.push_str("\nSQLSTATE ");
    out.push_str(db.code().code());
    if let (Some(ErrorPosition::Original(position)), Some(sql)) = (db.position(), sql)
        && let Some((line, column)) = line_and_column(sql, *position as usize)
    {
        out.push_str(&format!(" at line {line}, column {column}"));
    }
    out
}

/// The 1-based line and column of the 1-based character `position`.
fn line_and_column(sql: &str, position: usize) -> Option<(usize, usize)> {
    let (mut line, mut column) = (1, 1);
    for (n, ch) in sql.chars().enumerate() {
        if n + 1 == position {
            return Some((line, column));
        }
        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_lines_and_columns() {
        let sql = "SELECT 1,\n  nope\nFROM t";
        assert_eq!(line_and_column(sql, 1), Some((1, 1)));
        assert_eq!(line_and_column(sql, 13), Some((2, 3)));
        assert_eq!(line_and_column("é nope", 3), Some((1, 3)));
        assert_eq!(line_and_column(sql, 99), None);
    }
}
