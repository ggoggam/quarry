//! A dialect-aware SQL tokenizer.
//!
//! It does not parse. It only knows enough to find where every string,
//! quoted identifier, comment and statement terminator starts and ends,
//! which is what statement splitting ([`crate::sql`]) and highlighting
//! ([`crate::highlight`]) need. Unlike a grammar it never fails: any input,
//! half-typed or not, tokenizes, and the tokens tile the text exactly.

use std::ops::Range;

use crate::dialect::{HashComments, Lexical};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Whitespace,
    LineComment,
    BlockComment,
    /// A string literal of any form: `'..'`, `E'..'`, `$tag$..$tag$`,
    /// `X'..'`, MySQL's `".."`.
    String,
    /// `".."`, `` `..` `` or `[..]`.
    QuotedIdent,
    Number,
    /// A bare word: a keyword, function or identifier.
    Word,
    /// A placeholder or variable: `?`, `$1`, `:name`, `@var`, `{p:UInt8}`.
    Parameter,
    Operator,
    /// `(`, `)`, `[`, `]`, `{`, `}`.
    Bracket,
    /// `,`, `.` and a `;` that does not end a statement.
    Punctuation,
    /// The statement terminator: `;`, or whatever `DELIMITER` set.
    Delimiter,
    /// A whole `DELIMITER <token>` line, which the mysql client consumes.
    DelimiterCommand,
}

impl TokenKind {
    /// Whitespace and comments: text the engine ignores between tokens.
    pub fn is_trivia(self) -> bool {
        matches!(
            self,
            TokenKind::Whitespace
                | TokenKind::LineComment
                | TokenKind::BlockComment
                | TokenKind::DelimiterCommand
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub range: Range<usize>,
}

impl Token {
    pub fn text<'a>(&self, sql: &'a str) -> &'a str {
        &sql[self.range.clone()]
    }
}

/// Split `sql` into tokens that tile it: each byte is in exactly one token,
/// and every range is on character boundaries.
pub fn tokenize(sql: &str, lex: &Lexical) -> Vec<Token> {
    let mut lexer = Lexer {
        sql,
        bytes: sql.as_bytes(),
        lex,
        delimiter: b";".to_vec(),
        statement_start: true,
    };
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < lexer.bytes.len() {
        let (kind, end) = lexer.next(i);
        debug_assert!(end > i && sql.is_char_boundary(end));
        match kind {
            TokenKind::Delimiter => lexer.statement_start = true,
            kind if !kind.is_trivia() => lexer.statement_start = false,
            _ => {}
        }
        tokens.push(Token {
            kind,
            range: i..end,
        });
        i = end;
    }
    tokens
}

struct Lexer<'a> {
    sql: &'a str,
    bytes: &'a [u8],
    lex: &'a Lexical,
    delimiter: Vec<u8>,
    /// Nothing but trivia since the last terminator.
    statement_start: bool,
}

const DELIMITER: &[u8] = b"delimiter";

impl Lexer<'_> {
    /// The token starting at `i`: its kind and end offset.
    fn next(&mut self, i: usize) -> (TokenKind, usize) {
        let bytes = self.bytes;
        let lex = self.lex;
        let b = bytes[i];
        let next = bytes.get(i + 1).copied();

        if lex.delimiter_command && self.statement_start && self.is_delimiter_command(i) {
            let end = line_end(bytes, i);
            let token = self.sql[i + DELIMITER.len()..end].trim();
            if !token.is_empty() {
                self.delimiter = token.as_bytes().to_vec();
            }
            return (TokenKind::DelimiterCommand, end);
        }
        if bytes[i..].starts_with(&self.delimiter) {
            return (TokenKind::Delimiter, i + self.delimiter.len());
        }

        match b {
            b if b.is_ascii_whitespace() => {
                let end = bytes[i..]
                    .iter()
                    .position(|b| !b.is_ascii_whitespace())
                    .map_or(bytes.len(), |p| i + p);
                (TokenKind::Whitespace, end)
            }
            b'-' if next == Some(b'-')
                && (!lex.double_dash_needs_space
                    || bytes.get(i + 2).is_none_or(|c| c.is_ascii_whitespace())) =>
            {
                (TokenKind::LineComment, line_end(bytes, i))
            }
            b'#' if match lex.hash_comments {
                HashComments::None => false,
                HashComments::Always => true,
                HashComments::BeforeSpaceOrBang => matches!(next, Some(b' ' | b'!')),
            } =>
            {
                (TokenKind::LineComment, line_end(bytes, i))
            }
            b'/' if next == Some(b'*') => (
                TokenKind::BlockComment,
                skip_block_comment(bytes, i, lex.nested_block_comments),
            ),
            b'\'' => (
                TokenKind::String,
                skip_quoted(bytes, i, b'\'', lex.backslash_escapes),
            ),
            b'"' if lex.double_quote_strings => (
                TokenKind::String,
                skip_quoted(bytes, i, b'"', lex.backslash_escapes),
            ),
            b'"' => (
                TokenKind::QuotedIdent,
                skip_quoted(bytes, i, b'"', lex.backslash_escapes),
            ),
            b'`' if lex.backtick_identifiers => {
                (TokenKind::QuotedIdent, skip_quoted(bytes, i, b'`', false))
            }
            b'[' if lex.bracket_identifiers => {
                let end = bytes[i..]
                    .iter()
                    .position(|&b| b == b']')
                    .map_or(bytes.len(), |p| i + p + 1);
                (TokenKind::QuotedIdent, end)
            }
            // E'..' escape strings; N'..', X'..', B'..' prefixed literals.
            b'e' | b'E' if lex.escape_strings && next == Some(b'\'') => {
                (TokenKind::String, skip_quoted(bytes, i + 1, b'\'', true))
            }
            b'n' | b'N' | b'x' | b'X' | b'b' | b'B' if next == Some(b'\'') => (
                TokenKind::String,
                skip_quoted(bytes, i + 1, b'\'', lex.backslash_escapes),
            ),
            b'u' | b'U' if next == Some(b'&') && bytes.get(i + 2) == Some(&b'\'') => {
                (TokenKind::String, skip_quoted(bytes, i + 2, b'\'', false))
            }
            b'$' => {
                if lex.dollar_quotes
                    && let Some(end) = skip_dollar_quoted(bytes, i)
                {
                    return (TokenKind::String, end);
                }
                let end = run(bytes, i + 1, is_ident_byte);
                if end > i + 1 && (lex.numbered_parameters || lex.colon_parameters) {
                    (TokenKind::Parameter, end)
                } else {
                    (TokenKind::Operator, i + 1)
                }
            }
            b'?' if lex.question_parameters => (
                TokenKind::Parameter,
                run(bytes, i + 1, |b| b.is_ascii_digit()),
            ),
            b':' if lex.colon_parameters && next.is_some_and(is_ident_start) => {
                (TokenKind::Parameter, run(bytes, i + 1, is_ident_byte))
            }
            b'@' if lex.at_variables => {
                let start = if next == Some(b'@') { i + 2 } else { i + 1 };
                let end = run(bytes, start, |b| is_ident_byte(b) || b == b'.');
                (TokenKind::Parameter, end.max(i + 1))
            }
            b'{' if lex.brace_parameters
                && next.is_some_and(is_ident_start)
                && let Some(close) = brace_parameter_end(bytes, i) =>
            {
                (TokenKind::Parameter, close)
            }
            b'0'..=b'9' => (TokenKind::Number, number_end(bytes, i)),
            b'.' if next.is_some_and(|b| b.is_ascii_digit()) => {
                (TokenKind::Number, number_end(bytes, i))
            }
            b if is_ident_start(b) => {
                // Like the mysql client, end a word where the delimiter
                // starts: `END$$` is `END` then `$$`.
                let end = run(bytes, i, is_ident_byte);
                let end = (i + 1..end)
                    .find(|&k| bytes[k..].starts_with(&self.delimiter))
                    .unwrap_or(end);
                (TokenKind::Word, end)
            }
            b'(' | b')' | b'[' | b']' | b'{' | b'}' => (TokenKind::Bracket, i + 1),
            b',' | b'.' | b';' => (TokenKind::Punctuation, i + 1),
            _ if is_operator_byte(b) => {
                // A run of operator characters, stopped before anything
                // that starts a comment.
                let mut end = i + 1;
                while end < bytes.len()
                    && is_operator_byte(bytes[end])
                    && !bytes[end..].starts_with(b"--")
                    && !bytes[end..].starts_with(b"/*")
                    && !bytes[end..].starts_with(&self.delimiter)
                {
                    end += 1;
                }
                (TokenKind::Operator, end)
            }
            _ => (TokenKind::Operator, i + utf8_len(b)),
        }
    }

    /// `DELIMITER <token>` at the start of a line.
    fn is_delimiter_command(&self, i: usize) -> bool {
        let bytes = self.bytes;
        let at_line_start = bytes[..i]
            .iter()
            .rev()
            .take_while(|&&b| b != b'\n')
            .all(u8::is_ascii_whitespace);
        at_line_start
            && bytes.len() > i + DELIMITER.len()
            && bytes[i..i + DELIMITER.len()].eq_ignore_ascii_case(DELIMITER)
            && matches!(bytes[i + DELIMITER.len()], b' ' | b'\t')
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

fn is_operator_byte(b: u8) -> bool {
    matches!(
        b,
        b'+' | b'-'
            | b'*'
            | b'/'
            | b'<'
            | b'>'
            | b'='
            | b'~'
            | b'!'
            | b'@'
            | b'#'
            | b'%'
            | b'^'
            | b'&'
            | b'|'
            | b'?'
            | b':'
    )
}

/// The end of the run of bytes matching `pred` from `start`.
fn run(bytes: &[u8], start: usize, pred: impl Fn(u8) -> bool) -> usize {
    bytes[start.min(bytes.len())..]
        .iter()
        .position(|&b| !pred(b))
        .map_or(bytes.len(), |p| start + p)
}

/// End of a line comment, before the newline.
fn line_end(bytes: &[u8], start: usize) -> usize {
    bytes[start..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(bytes.len(), |p| start + p)
}

fn number_end(bytes: &[u8], start: usize) -> usize {
    if bytes[start] == b'0'
        && let Some(b'x' | b'X' | b'b' | b'B' | b'o' | b'O') = bytes.get(start + 1)
    {
        return run(bytes, start + 2, |b| b.is_ascii_hexdigit() || b == b'_');
    }
    let digits = |b: u8| b.is_ascii_digit() || b == b'_';
    let mut i = run(bytes, start, digits);
    if bytes.get(i) == Some(&b'.') && bytes.get(i + 1) != Some(&b'.') {
        i = run(bytes, i + 1, digits);
    }
    if let Some(b'e' | b'E') = bytes.get(i) {
        let mut j = i + 1;
        if let Some(b'+' | b'-') = bytes.get(j) {
            j += 1;
        }
        if bytes.get(j).is_some_and(u8::is_ascii_digit) {
            i = run(bytes, j, |b| b.is_ascii_digit());
        }
    }
    i
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
            // Step over the escaped character whole.
            i += 1 + bytes.get(i + 1).map_or(0, |&b| utf8_len(b));
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
            .position(|&b| !(b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80))?;
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

/// `{name:Type}`: the end after `}`, if the braces hold `ident:...` on one
/// line.
fn brace_parameter_end(bytes: &[u8], start: usize) -> Option<usize> {
    let name_end = run(bytes, start + 1, is_ident_byte);
    if bytes.get(name_end) != Some(&b':') {
        return None;
    }
    let close = bytes[name_end..]
        .iter()
        .position(|&b| b == b'}' || b == b'\n')
        .map(|p| name_end + p)?;
    (bytes[close] == b'}').then_some(close + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::Dialect;

    fn kinds(sql: &str, dialect: Dialect) -> Vec<(TokenKind, &str)> {
        tokenize(sql, &dialect.lexical())
            .into_iter()
            .filter(|t| t.kind != TokenKind::Whitespace)
            .map(|t| (t.kind, &sql[t.range]))
            .collect()
    }

    #[test]
    fn tokens_tile_the_text() {
        let sql = "select 'a''b', \"q\", $x$ ; $x$, 1.5e3, x::int -- c\n/* b */ from t;  한글 ";
        for dialect in Dialect::ALL {
            let tokens = tokenize(sql, &dialect.lexical());
            let mut at = 0;
            for token in &tokens {
                assert_eq!(token.range.start, at, "{dialect}: {tokens:?}");
                at = token.range.end;
            }
            assert_eq!(at, sql.len());
        }
    }

    #[test]
    fn postgres_tokens() {
        use TokenKind::*;
        assert_eq!(
            kinds(
                "SELECT E'a\\'b', $f$x$f$, $1, x::text, \"Id\" FROM t;",
                Dialect::Postgres
            ),
            [
                (Word, "SELECT"),
                (String, "E'a\\'b'"),
                (Punctuation, ","),
                (String, "$f$x$f$"),
                (Punctuation, ","),
                (Parameter, "$1"),
                (Punctuation, ","),
                (Word, "x"),
                (Operator, "::"),
                (Word, "text"),
                (Punctuation, ","),
                (QuotedIdent, "\"Id\""),
                (Word, "FROM"),
                (Word, "t"),
                (Delimiter, ";"),
            ]
        );
        // `?` is an operator on jsonb.
        assert_eq!(kinds("j ? 'k'", Dialect::Postgres)[1], (Operator, "?"));
    }

    #[test]
    fn mysql_tokens() {
        use TokenKind::*;
        assert_eq!(
            kinds(
                "# note\nSELECT \"s\", `c`, @v, @@sql_mode, ? -- x",
                Dialect::MySql
            ),
            [
                (LineComment, "# note"),
                (Word, "SELECT"),
                (String, "\"s\""),
                (Punctuation, ","),
                (QuotedIdent, "`c`"),
                (Punctuation, ","),
                (Parameter, "@v"),
                (Punctuation, ","),
                (Parameter, "@@sql_mode"),
                (Punctuation, ","),
                (Parameter, "?"),
                (LineComment, "-- x"),
            ]
        );
        // `--` without a space is two minus signs.
        assert_eq!(kinds("1--2", Dialect::MySql)[1], (Operator, "--"));
    }

    #[test]
    fn delimiter_command() {
        use TokenKind::*;
        let sql = "DELIMITER $$\nBEGIN SELECT 1; END$$\nDELIMITER ;\nSELECT 2;";
        let tokens = kinds(sql, Dialect::MySql);
        assert_eq!(tokens[0], (DelimiterCommand, "DELIMITER $$"));
        assert!(tokens.contains(&(Punctuation, ";")));
        assert!(tokens.contains(&(Delimiter, "$$")));
        assert_eq!(tokens.last(), Some(&(Delimiter, ";")));
    }

    #[test]
    fn clickhouse_and_sqlite_parameters() {
        use TokenKind::*;
        assert_eq!(
            kinds("SELECT {id:UInt64}, {x}", Dialect::ClickHouse)[1],
            (Parameter, "{id:UInt64}")
        );
        assert_eq!(kinds("SELECT {x}", Dialect::ClickHouse)[1], (Bracket, "{"));
        assert_eq!(
            kinds("SELECT :a, @b, $c, ?1, [col]", Dialect::Sqlite)
                .into_iter()
                .filter(|(k, _)| *k != Punctuation)
                .collect::<Vec<_>>(),
            [
                (Word, "SELECT"),
                (Parameter, ":a"),
                (Parameter, "@b"),
                (Parameter, "$c"),
                (Parameter, "?1"),
                (QuotedIdent, "[col]"),
            ]
        );
    }

    #[test]
    fn numbers() {
        use TokenKind::*;
        for n in ["1", "1.5", ".5", "1e10", "1.5E-3", "0x1F", "1_000_000"] {
            assert_eq!(kinds(n, Dialect::DuckDb), [(Number, n)], "{n}");
        }
        // A range in a slice is not a decimal point.
        assert_eq!(kinds("1..2", Dialect::DuckDb)[0], (Number, "1"));
    }
}
