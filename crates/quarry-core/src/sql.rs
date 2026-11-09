//! Lexical helpers for SQL text in the editor.
//!
//! These split a script into statements without parsing it. The scanner only
//! needs to know where a `;` really ends a statement, so it tracks the
//! constructs that can hide one: quoted strings and identifiers, escape
//! strings, dollar-quoted bodies, and line and (nested) block comments.

use std::ops::Range;

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

/// Split `sql` into its statements.
///
/// Statements that hold only whitespace and comments are dropped, so the
/// result is what would actually be sent to the engine.
pub fn split_statements(sql: &str) -> Vec<StatementSpan> {
    let bytes = sql.as_bytes();
    let mut spans = Vec::new();
    let mut slot_start = 0;
    // The extent of real code (not whitespace or comments) in the current
    // statement, if any has been seen yet.
    let mut code: Option<Range<usize>> = None;
    let mut i = 0;

    fn mark(code: &mut Option<Range<usize>>, start: usize, end: usize) {
        match code {
            Some(range) => range.end = end,
            None => *code = Some(start..end),
        }
    }

    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                i = skip_line_comment(bytes, i);
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = skip_block_comment(bytes, i);
            }
            b'\'' => {
                let end = skip_quoted(bytes, i, b'\'', false);
                mark(&mut code, i, end);
                i = end;
            }
            b'"' => {
                let end = skip_quoted(bytes, i, b'"', false);
                mark(&mut code, i, end);
                i = end;
            }
            b'e' | b'E' if bytes.get(i + 1) == Some(&b'\'') && !is_ident_byte_before(bytes, i) => {
                let end = skip_quoted(bytes, i + 1, b'\'', true);
                mark(&mut code, i, end);
                i = end;
            }
            b'$' if !is_ident_byte_before(bytes, i) => {
                if let Some(end) = skip_dollar_quoted(bytes, i) {
                    mark(&mut code, i, end);
                    i = end;
                } else {
                    mark(&mut code, i, i + 1);
                    i += 1;
                }
            }
            b';' => {
                if let Some(range) = code.take() {
                    spans.push(StatementSpan {
                        range,
                        slot: slot_start..i + 1,
                    });
                } else if let Some(last) = spans.last_mut() {
                    // A stray `;` (or a comment-only statement) belongs to
                    // the statement before it, so the slots keep tiling.
                    last.slot.end = i + 1;
                }
                slot_start = i + 1;
                i += 1;
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

/// The statement a caret at byte `offset` refers to.
///
/// A caret inside a statement's slot picks that statement. A caret in the
/// whitespace after a statement's `;` picks the statement on the line it
/// ends, which is what someone who just typed `;` means, unless the caret
/// has moved on to a later line.
pub fn statement_at(sql: &str, offset: usize) -> Option<StatementSpan> {
    let spans = split_statements(sql);
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

fn skip_block_comment(bytes: &[u8], start: usize) -> usize {
    let mut depth = 0;
    let mut i = start;
    while i < bytes.len() {
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
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
/// escaped quote; `backslash` also honours `\` escapes (E'' strings).
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

    #[test]
    fn statement_at_empty_text() {
        assert_eq!(statement_at("", 0), None);
        assert_eq!(statement_at("  -- only a comment", 3), None);
    }
}
