//! Lexical helpers for SQL text in the editor.
//!
//! These split a script into statements without parsing it. The scanner only
//! needs to know where a `;` really ends a statement, so it tracks the
//! constructs that can hide one: quoted strings and identifiers, escape
//! strings, dollar-quoted bodies, and line and (nested) block comments. Which
//! of those exist depends on the [`Dialect`].

use std::ops::Range;

use crate::dialect::{Dialect, HashComments, Lexical};

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
    let bytes = sql.as_bytes();
    let mut spans = Vec::new();
    let mut slot_start = 0;
    // The extent of real code (not whitespace or comments) in the current
    // statement, if any has been seen yet.
    let mut code: Option<Range<usize>> = None;
    // What ends a statement; the mysql client's `DELIMITER` changes it.
    let mut delimiter: Vec<u8> = b";".to_vec();
    let mut i = 0;

    fn mark(code: &mut Option<Range<usize>>, start: usize, end: usize) {
        match code {
            Some(range) => range.end = end,
            None => *code = Some(start..end),
        }
    }

    while i < bytes.len() {
        let b = bytes[i];
        if lex.delimiter_command && code.is_none() && is_delimiter_command(bytes, i) {
            let line_end = bytes[i..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(bytes.len(), |p| i + p);
            let token = sql[i + DELIMITER.len()..line_end].trim();
            if !token.is_empty() {
                delimiter = token.as_bytes().to_vec();
            }
            i = line_end;
            continue;
        }
        if bytes[i..].starts_with(&delimiter) {
            let end = i + delimiter.len();
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
            i = end;
            continue;
        }
        let next = bytes.get(i + 1).copied();
        match b {
            b'-' if next == Some(b'-')
                && (!lex.double_dash_needs_space
                    || bytes.get(i + 2).is_none_or(|c| c.is_ascii_whitespace())) =>
            {
                i = skip_line_comment(bytes, i);
            }
            b'#' if match lex.hash_comments {
                HashComments::None => false,
                HashComments::Always => true,
                HashComments::BeforeSpaceOrBang => matches!(next, Some(b' ' | b'!')),
            } =>
            {
                i = skip_line_comment(bytes, i);
            }
            b'/' if next == Some(b'*') => {
                i = skip_block_comment(bytes, i, lex.nested_block_comments);
            }
            b'\'' | b'"' => {
                let end = skip_quoted(bytes, i, b, lex.backslash_escapes);
                mark(&mut code, i, end);
                i = end;
            }
            b'`' if lex.backtick_identifiers => {
                let end = skip_quoted(bytes, i, b'`', false);
                mark(&mut code, i, end);
                i = end;
            }
            b'[' if lex.bracket_identifiers => {
                let end = bytes[i..]
                    .iter()
                    .position(|&b| b == b']')
                    .map_or(bytes.len(), |p| i + p + 1);
                mark(&mut code, i, end);
                i = end;
            }
            b'e' | b'E'
                if lex.escape_strings && next == Some(b'\'') && !is_ident_byte_before(bytes, i) =>
            {
                let end = skip_quoted(bytes, i + 1, b'\'', true);
                mark(&mut code, i, end);
                i = end;
            }
            b'$' if lex.dollar_quotes && !is_ident_byte_before(bytes, i) => {
                if let Some(end) = skip_dollar_quoted(bytes, i) {
                    mark(&mut code, i, end);
                    i = end;
                } else {
                    mark(&mut code, i, i + 1);
                    i += 1;
                }
            }
            b if b.is_ascii_whitespace() => i += 1,
            _ => {
                // Advance a whole UTF-8 character so ranges stay on
                // character boundaries.
                let len = utf8_len(b);
                mark(&mut code, i, i + len);
                i += len;
            }
        }
    }

    if let Some(range) = code {
        spans.push(StatementSpan {
            range,
            slot: slot_start..bytes.len(),
        });
    } else if let Some(last) = spans.last_mut() {
        last.slot.end = bytes.len();
    }
    spans
}

const DELIMITER: &str = "delimiter";

/// `DELIMITER <token>` at the start of a line.
fn is_delimiter_command(bytes: &[u8], i: usize) -> bool {
    let at_line_start = bytes[..i]
        .iter()
        .rev()
        .take_while(|&&b| b != b'\n')
        .all(|b| b.is_ascii_whitespace());
    at_line_start
        && bytes.len() > i + DELIMITER.len()
        && bytes[i..i + DELIMITER.len()].eq_ignore_ascii_case(DELIMITER.as_bytes())
        && matches!(bytes[i + DELIMITER.len()], b' ' | b'\t')
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

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

fn is_ident_byte_before(bytes: &[u8], i: usize) -> bool {
    i > 0 && is_ident_byte(bytes[i - 1])
}

fn skip_line_comment(bytes: &[u8], start: usize) -> usize {
    bytes[start..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(bytes.len(), |p| start + p + 1)
}

fn skip_block_comment(bytes: &[u8], start: usize, nested: bool) -> usize {
    let mut depth = 0;
    let mut i = start;
    while i < bytes.len() {
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') && (nested || depth == 0) {
            depth += 1;
            i += 2;
        } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    bytes.len()
}

/// Skip a quoted run starting at the opening quote. A doubled quote is an
/// escaped quote; `backslash` also honours `\` escapes (E'' strings, and
/// every string in MySQL and ClickHouse).
fn skip_quoted(bytes: &[u8], start: usize, quote: u8, backslash: bool) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        let b = bytes[i];
        if backslash && b == b'\\' {
            i += 2;
        } else if b == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
            } else {
                return i + 1;
            }
        } else {
            i += 1;
        }
    }
    bytes.len()
}

/// Skip `$tag$ ... $tag$`. Returns `None` when `$` does not open a
/// dollar-quote (for example a `$1` parameter).
fn skip_dollar_quoted(bytes: &[u8], start: usize) -> Option<usize> {
    let tag_end = start
        + 1
        + bytes[start + 1..]
            .iter()
            .position(|&b| !(b.is_ascii_alphanumeric() || b == b'_'))?;
    if bytes[tag_end] != b'$' {
        return None;
    }
    let tag = &bytes[start + 1..tag_end];
    if tag.first().is_some_and(u8::is_ascii_digit) {
        return None;
    }
    let delimiter = &bytes[start..=tag_end];
    let body = tag_end + 1;
    let close = bytes[body..]
        .windows(delimiter.len())
        .position(|w| w == delimiter)
        .map_or(bytes.len(), |p| body + p + delimiter.len());
    Some(close)
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
