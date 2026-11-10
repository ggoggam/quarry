//! What a statement is, from its text, before or without running it.

/// The first keyword, upper-cased, past whitespace, comments and opening
/// parentheses: `"SELECT"` for `/* hi */ (select 1)`.
pub fn leading_keyword(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b if b.is_ascii_whitespace() || b == b'(' => i += 1,
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                // PostgreSQL block comments nest.
                let mut depth = 0;
                while i < bytes.len() {
                    if bytes[i..].starts_with(b"/*") {
                        depth += 1;
                        i += 2;
                    } else if bytes[i..].starts_with(b"*/") {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            _ => break,
        }
    }
    sql[i..]
        .chars()
        .take_while(|c| c.is_ascii_alphabetic() || *c == '_')
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Statements that only read, so stopping one early with a protocol cancel
/// loses nothing. A `WITH` counts only when no data-modifying keyword
/// appears anywhere in it, which errs toward draining.
pub fn is_read_only(sql: &str) -> bool {
    match leading_keyword(sql).as_str() {
        "SELECT" | "VALUES" | "TABLE" | "SHOW" => true,
        "WITH" => !sql
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|word| {
                ["INSERT", "UPDATE", "DELETE", "MERGE"]
                    .iter()
                    .any(|dml| word.eq_ignore_ascii_case(dml))
            }),
        _ => false,
    }
}

/// Whether a statement that returned no rows changed some, so its command
/// tag's count is worth showing. A `WITH` that returns no rows ends in one
/// of these too, and `EXECUTE` can only run statements that `PREPARE`
/// accepts, which are queries or DML.
pub fn reports_rows_affected(sql: &str) -> bool {
    matches!(
        leading_keyword(sql).as_str(),
        "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "WITH" | "EXECUTE"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_leading_keyword() {
        assert_eq!(leading_keyword("select 1"), "SELECT");
        assert_eq!(
            leading_keyword("  -- note\n/* a /* b */ c */ ((Values (1)))"),
            "VALUES"
        );
        assert_eq!(leading_keyword("insert_x"), "INSERT_X");
        assert_eq!(leading_keyword(""), "");
        assert_eq!(leading_keyword("/* unterminated"), "");
    }

    #[test]
    fn tells_reads_from_writes() {
        assert!(is_read_only("SELECT * FROM t"));
        assert!(is_read_only("with x as (select 1) select * from x"));
        assert!(!is_read_only(
            "with x as (delete from t returning *) select * from x"
        ));
        assert!(!is_read_only("INSERT INTO t SELECT 1 RETURNING *"));
        assert!(!is_read_only("EXPLAIN ANALYZE SELECT 1"));
        assert!(reports_rows_affected("update t set a = 1"));
        assert!(reports_rows_affected("WITH x AS (SELECT 1) DELETE FROM t"));
        assert!(!reports_rows_affected("CREATE TABLE t (a int)"));
        assert!(!reports_rows_affected("BEGIN"));
    }
}
