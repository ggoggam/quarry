//! Lexical helpers for SQL text in the editor.
//!
//! These split a script into statements without parsing it, using the
//! dialect's tokenizer ([`crate::lex`]) to tell a `;` that ends a statement
//! from one inside a string, quoted identifier or comment.

use std::ops::Range;

use crate::dialect::{Dialect, Lexical};
use crate::lex::{TokenKind, tokenize};

/// One statement in a script, as byte offsets into the source text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementSpan {
    /// The statement text with surrounding whitespace and the terminating
    /// `;` removed.
    pub range: Range<usize>,
    /// The statement's whole slot: from the end of the previous statement
    /// (or the start of the text) up to and including its `;` (or the end
    /// of the text). Slots tile the text, so every offset is in exactly one.
    pub slot: Range<usize>,
}

impl StatementSpan {
    pub fn text<'a>(&self, sql: &'a str) -> &'a str {
        &sql[self.range.clone()]
    }
}

/// Split `sql` into its statements, with PostgreSQL and DuckDB rules.
///
/// Statements that hold only whitespace and comments are dropped, so the
/// result is what would actually be sent to the engine.
pub fn split_statements(sql: &str) -> Vec<StatementSpan> {
    split_statements_for(sql, Dialect::DuckDb)
}

/// Split `sql` into its statements, following `dialect`'s lexical rules.
pub fn split_statements_for(sql: &str, dialect: Dialect) -> Vec<StatementSpan> {
    split_with(sql, dialect.lexical())
}

fn split_with(sql: &str, lex: Lexical) -> Vec<StatementSpan> {
    let mut spans: Vec<StatementSpan> = Vec::new();
    let mut slot_start = 0;
    // The extent of real code (not whitespace or comments) in the current
    // statement, if any has been seen yet.
    let mut code: Option<Range<usize>> = None;

    for token in tokenize(sql, &lex) {
        match token.kind {
            TokenKind::Delimiter => {
                let end = token.range.end;
                if let Some(range) = code.take() {
                    spans.push(StatementSpan {
                        range,
                        slot: slot_start..end,
                    });
                } else if let Some(last) = spans.last_mut() {
                    // A stray `;` (or a comment-only statement) belongs to
                    // the statement before it, so the slots keep tiling.
                    last.slot.end = end;
                }
                slot_start = end;
            }
            kind if kind.is_trivia() => {}
            _ => match &mut code {
                Some(range) => range.end = token.range.end,
                None => code = Some(token.range),
            },
        }
    }

    if let Some(range) = code {
        spans.push(StatementSpan {
            range,
            slot: slot_start..sql.len(),
        });
    } else if let Some(last) = spans.last_mut() {
        last.slot.end = sql.len();
    }
    spans
}

/// The statement a caret at byte `offset` refers to.
///
/// A caret inside a statement's slot picks that statement. A caret in the
/// whitespace after a statement's `;` picks the statement on the line it
/// ends, which is what someone who just typed `;` means, unless the caret
/// has moved on to a later line.
pub fn statement_at(sql: &str, offset: usize) -> Option<StatementSpan> {
    statement_at_for(sql, offset, Dialect::DuckDb)
}

/// [`statement_at`] with `dialect`'s lexical rules.
pub fn statement_at_for(sql: &str, offset: usize, dialect: Dialect) -> Option<StatementSpan> {
    let spans = split_statements_for(sql, dialect);
    let offset = offset.min(sql.len());
    let ix = spans
        .iter()
        .position(|span| span.slot.contains(&offset) || span.slot.end == offset)?;

    let span = &spans[ix];
    if offset < span.range.start && ix > 0 {
        let prev = &spans[ix - 1];
        let between = &sql[prev.range.end..offset];
        if !between.contains('\n') {
            return Some(prev.clone());
        }
    }
    Some(span.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(sql: &str) -> Vec<&str> {
        split_statements(sql).iter().map(|s| s.text(sql)).collect()
    }

    #[test]
    fn splits_on_semicolons() {
        assert_eq!(
            texts("select 1; select 2 ;\n select 3"),
            ["select 1", "select 2", "select 3"]
        );
    }

    #[test]
    fn ignores_semicolons_in_strings_identifiers_and_comments() {
        let sql = "select ';', \"a;b\", E'it\\'s;' -- c;\n from t /* x; /* y; */ z; */; select 2";
        assert_eq!(
            texts(sql),
            ["select ';', \"a;b\", E'it\\'s;' -- c;\n from t", "select 2"]
        );
    }

    #[test]
    fn handles_doubled_quotes() {
        assert_eq!(
            texts("select 'it''s; fine'; select 2"),
            ["select 'it''s; fine'", "select 2"]
        );
    }

    #[test]
    fn handles_dollar_quotes_but_not_parameters() {
        assert_eq!(
            texts("select $x$ a; b $x$; select $1; select $$;$$"),
            ["select $x$ a; b $x$", "select $1", "select $$;$$"]
        );
    }

    #[test]
    fn drops_comment_only_statements() {
        assert_eq!(
            texts("-- header\n;;select 1; /* nothing */ ;"),
            ["select 1"]
        );
    }

    #[test]
    fn slots_tile_the_text() {
        let sql = "select 1;;  select 2; -- tail";
        let spans = split_statements(sql);
        assert_eq!(spans[0].slot.start, 0);
        assert_eq!(spans[0].slot.end, spans[1].slot.start);
        assert_eq!(spans[1].slot.end, sql.len());
    }

    #[test]
    fn keeps_multibyte_characters_whole() {
        assert_eq!(
            texts("select '한글'; select 한글 from t"),
            ["select '한글'", "select 한글 from t"]
        );
    }

    #[test]
    fn statement_at_caret() {
        let sql = "select 1;\nselect 2;  \n\nselect 3";
        let at = |offset| statement_at(sql, offset).map(|s| s.text(sql).to_owned());
        assert_eq!(at(0).as_deref(), Some("select 1"));
        // Right after the `;` on the same line: still statement 1.
        assert_eq!(at(9).as_deref(), Some("select 1"));
        // Start of the next line belongs to statement 2.
        assert_eq!(at(10).as_deref(), Some("select 2"));
        // Trailing spaces after `select 2;` still mean statement 2.
        assert_eq!(at(20).as_deref(), Some("select 2"));
        // A blank line before statement 3 means statement 3.
        assert_eq!(at(22).as_deref(), Some("select 3"));
        assert_eq!(at(sql.len()).as_deref(), Some("select 3"));
    }

    fn texts_for(sql: &str, dialect: Dialect) -> Vec<&str> {
        split_statements_for(sql, dialect)
            .iter()
            .map(|s| s.text(sql))
            .collect()
    }

    #[test]
    fn mysql_comments_strings_and_backticks() {
        let sql =
            "select 'it\\'s;' # hash; comment\n from `a;b`; select 1 --; not a comment\n; select 2";
        assert_eq!(
            texts_for(sql, Dialect::MySql),
            [
                "select 'it\\'s;' # hash; comment\n from `a;b`",
                "select 1 --",
                "not a comment",
                "select 2"
            ]
        );
        // `$` and E'' mean nothing special in MySQL.
        assert_eq!(
            texts_for("select $x$ a; select e'x'", Dialect::MySql),
            ["select $x$ a", "select e'x'"]
        );
    }

    #[test]
    fn mysql_delimiter_command() {
        let sql = "DELIMITER //\nCREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END//\ndelimiter ;\nCALL p();";
        assert_eq!(
            texts_for(sql, Dialect::MariaDb),
            [
                "CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END",
                "CALL p()"
            ]
        );
        // Elsewhere `delimiter` is just a word.
        assert_eq!(
            texts_for("delimiter //\nselect 1;", Dialect::Postgres).len(),
            1
        );
    }

    #[test]
    fn sqlite_brackets_and_flat_comments() {
        assert_eq!(
            texts_for(
                "select [a;b] from t /* x /* y; */ ; select 2",
                Dialect::Sqlite
            ),
            ["select [a;b] from t", "select 2"]
        );
    }

    #[test]
    fn clickhouse_hash_comments() {
        assert_eq!(
            texts_for(
                "select 1 # c; d\n; select a#b; select 2",
                Dialect::ClickHouse
            ),
            ["select 1", "select a#b", "select 2"]
        );
    }

    #[test]
    fn statement_at_empty_text() {
        assert_eq!(statement_at("", 0), None);
        assert_eq!(statement_at("  -- only a comment", 3), None);
    }
}
