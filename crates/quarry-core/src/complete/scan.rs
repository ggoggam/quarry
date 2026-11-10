//! The text around the caret, as tokens.
//!
//! The statement is the run of tokens between the dialect's terminators
//! around the caret, the same rule [`crate::sql`] splits scripts by. It is
//! cut from one tokenize of the whole text rather than from
//! [`crate::sql::statement_at_for`], for two reasons: the tokenizer must see
//! everything before the caret to know what a MySQL `DELIMITER` changed the
//! terminator to, and a caret right after a `;` is already in the next
//! statement, where "run the statement at the caret" deliberately looks
//! back at the one just ended.

use std::ops::Range;

use crate::dialect::Lexical;
use crate::lex::{Token, TokenKind, tokenize};

/// The caret's statement, with the word being completed taken out.
#[derive(Debug)]
pub(super) struct Scan<'a> {
    pub sql: &'a str,
    /// The statement's significant tokens, without the word at the caret
    /// and the names qualifying it.
    pub tokens: Vec<Token>,
    /// Where the word sits among `tokens`: `tokens[..at]` come before it.
    pub at: usize,
    pub word: Word,
    /// The names before the word, outermost first: `["public"]` for
    /// `public.ord`, `["o"]` for `o.`. Unquoted.
    pub qualifier: Vec<String>,
}

/// The word being completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Word {
    /// What is typed of it, up to the caret, unquoted.
    pub prefix: String,
    /// The whole word, which an item replaces; empty at the caret when
    /// nothing is typed yet.
    pub range: Range<usize>,
    /// The quote it was opened with, when it is a quoted identifier.
    pub quote: Option<char>,
}

impl Scan<'_> {
    pub fn text(&self, ix: usize) -> &str {
        self.tokens[ix].text(self.sql)
    }

    /// Whether token `ix` is the word `keyword`, ignoring case.
    pub fn is(&self, ix: usize, keyword: &str) -> bool {
        self.tokens.get(ix).is_some_and(|t| {
            t.kind == TokenKind::Word && t.text(self.sql).eq_ignore_ascii_case(keyword)
        })
    }

    /// Whether token `ix` is the punctuation or bracket `symbol`.
    pub fn is_symbol(&self, ix: usize, symbol: &str) -> bool {
        self.tokens.get(ix).is_some_and(|t| {
            matches!(
                t.kind,
                TokenKind::Punctuation | TokenKind::Bracket | TokenKind::Operator
            ) && t.text(self.sql) == symbol
        })
    }

    /// The name token `ix` spells, unquoted, if it is an identifier.
    pub fn name(&self, ix: usize) -> Option<String> {
        let token = self.tokens.get(ix)?;
        match token.kind {
            TokenKind::Word => Some(token.text(self.sql).to_owned()),
            TokenKind::QuotedIdent => Some(unquote(token.text(self.sql))),
            _ => None,
        }
    }
}

/// Read the caret's surroundings, or `None` where nothing should be offered:
/// in a string, comment, number or parameter, or after a `.` that follows
/// something other than a name.
pub(super) fn scan<'a>(sql: &'a str, caret: usize, lex: &Lexical) -> Option<Scan<'a>> {
    let all = tokenize(sql, lex);
    let start = all
        .iter()
        .rposition(|t| t.kind == TokenKind::Delimiter && t.range.end <= caret)
        .map_or(0, |ix| ix + 1);
    let end = all[start..]
        .iter()
        .position(|t| t.kind == TokenKind::Delimiter && t.range.start >= caret)
        .map_or(all.len(), |ix| start + ix);
    let statement = &all[start..end];

    let mut word = Word {
        prefix: String::new(),
        range: caret..caret,
        quote: None,
    };
    if let Some(token) = statement
        .iter()
        .find(|t| t.range.start < caret && caret <= t.range.end)
    {
        let text = token.text(sql);
        match token.kind {
            TokenKind::Word => {
                word.prefix = sql[token.range.start..caret].to_owned();
                word.range = token.range.clone();
            }
            TokenKind::QuotedIdent => {
                let closed = is_closed_ident(text);
                if caret == token.range.end && closed && token.range.len() > 1 {
                    // Right after a finished quoted name: complete it whole.
                    word.prefix = unquote(text);
                } else {
                    word.prefix = unquote(&sql[token.range.start..caret]);
                }
                word.range = token.range.clone();
                word.quote = text.chars().next();
            }
            TokenKind::String if caret < token.range.end || !is_closed_string(text) => {
                return None;
            }
            TokenKind::LineComment => return None,
            TokenKind::BlockComment
                if caret < token.range.end || !(text.len() >= 4 && text.ends_with("*/")) =>
            {
                return None;
            }
            TokenKind::Number | TokenKind::Parameter | TokenKind::DelimiterCommand => {
                return None;
            }
            _ => {}
        }
    }

    let significant: Vec<&Token> = statement.iter().filter(|t| !t.kind.is_trivia()).collect();
    // Tokens wholly before the word, and the first one after it.
    let mut before = significant
        .iter()
        .take_while(|t| t.range.end <= word.range.start)
        .count();
    let after = significant
        .iter()
        .position(|t| t.range.start >= word.range.end.max(caret))
        .unwrap_or(significant.len())
        .max(before);

    // `a.b.` before the word, each part touching the next.
    let mut qualifier = Vec::new();
    let mut edge = word.range.start;
    while before >= 1 {
        let dot = significant[before - 1];
        if !(dot.kind == TokenKind::Punctuation && dot.text(sql) == "." && dot.range.end == edge) {
            break;
        }
        let name = before.checked_sub(2).map(|ix| significant[ix]).filter(|t| {
            matches!(t.kind, TokenKind::Word | TokenKind::QuotedIdent)
                && t.range.end == dot.range.start
        });
        let Some(name) = name else {
            // `1.`, `).`, `..`: not a qualified name.
            return None;
        };
        qualifier.insert(
            0,
            match name.kind {
                TokenKind::QuotedIdent => unquote(name.text(sql)),
                _ => name.text(sql).to_owned(),
            },
        );
        edge = name.range.start;
        before -= 2;
    }

    let tokens: Vec<Token> = significant[..before]
        .iter()
        .chain(&significant[after..])
        .map(|t| (*t).clone())
        .collect();
    Some(Scan {
        sql,
        tokens,
        at: before,
        word,
        qualifier,
    })
}

/// A quoted identifier's name: the quotes taken off, doubled quotes undone.
/// An unfinished one (`"Ord`) is read up to where it ends.
pub(super) fn unquote(text: &str) -> String {
    let Some(open) = text.chars().next() else {
        return String::new();
    };
    let close = if open == '[' { ']' } else { open };
    let inner = &text[open.len_utf8()..];
    let inner = if is_closed_ident(text) {
        &inner[..inner.len() - close.len_utf8()]
    } else {
        inner
    };
    if close == ']' {
        inner.to_owned()
    } else {
        inner.replace(&format!("{close}{close}"), &close.to_string())
    }
}

/// Whether a quoted identifier's text ends with its closing quote.
fn is_closed_ident(text: &str) -> bool {
    let Some(open) = text.chars().next() else {
        return false;
    };
    let close = if open == '[' { ']' } else { open };
    if text.len() < 2 || !text.ends_with(close) {
        return false;
    }
    if close == ']' {
        return true;
    }
    // A run of quotes at the end closes the name when it is odd: `"a"""`
    // is closed, `"a""` is a doubled quote with the name still open.
    let inner = &text[1..];
    let run = inner.chars().rev().take_while(|&c| c == close).count();
    run % 2 == 1
}

/// Whether a string literal's text ends with its closing quote, so a caret
/// right after it is outside.
fn is_closed_string(text: &str) -> bool {
    if let Some(rest) = text.strip_prefix('$') {
        let Some(tag_end) = rest.find('$') else {
            return false;
        };
        let tag = &text[..tag_end + 2];
        return text.len() >= 2 * tag.len() && text.ends_with(tag);
    }
    let Some(open) = text.find(['\'', '"']) else {
        return false;
    };
    let quote = text.as_bytes()[open];
    let body = &text.as_bytes()[open + 1..];
    if body.last() != Some(&quote) {
        return false;
    }
    let run = body.iter().rev().take_while(|&&b| b == quote).count();
    run % 2 == 1
}
