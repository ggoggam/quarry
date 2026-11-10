//! Syntax highlighting for one dialect at one engine release.
//!
//! Tokens come from [`crate::lex`], so strings, comments and quoting follow
//! the dialect exactly, and half-typed SQL highlights as well as finished
//! SQL. Words are colored from the dialect's [`Vocabulary`] for the
//! connected release: a function the server does not have stays plain, and
//! [`analyze`] reports a [`VersionNote`] where a statement uses one.

use std::ops::Range;

use crate::dialect::Flavor;
use crate::lex::{Token, TokenKind, tokenize};
use crate::vocabulary::{Availability, Entry, Vocabulary, WordClass};

/// What a span of SQL is, for coloring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Highlight {
    Keyword,
    Function,
    Type,
    String,
    Number,
    Boolean,
    /// `NULL`.
    Constant,
    Comment,
    Operator,
    Parameter,
    Bracket,
    Punctuation,
    /// The mysql client's `DELIMITER` line.
    Directive,
}

impl Highlight {
    /// The syntax theme's name for this highlight.
    pub fn theme_name(self) -> &'static str {
        match self {
            Highlight::Keyword => "keyword",
            Highlight::Function => "function",
            Highlight::Type => "type",
            Highlight::String => "string",
            Highlight::Number => "number",
            Highlight::Boolean => "boolean",
            Highlight::Constant => "constant",
            Highlight::Comment => "comment",
            Highlight::Operator => "operator",
            Highlight::Parameter => "variable.special",
            Highlight::Bracket => "punctuation.bracket",
            Highlight::Punctuation => "punctuation.delimiter",
            Highlight::Directive => "preproc",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub range: Range<usize>,
    pub highlight: Highlight,
}

/// A word the connected release does not support.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionNote {
    pub range: Range<usize>,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Analysis {
    /// Highlighted spans in order. Text between spans has no highlight.
    pub spans: Vec<Span>,
    pub notes: Vec<VersionNote>,
}

/// Highlight `sql` as `flavor`'s engine reads it.
pub fn analyze(sql: &str, flavor: &Flavor) -> Analysis {
    let tokens = tokenize(sql, &flavor.dialect.lexical());
    Classifier {
        sql,
        flavor,
        vocabulary: Vocabulary::of(flavor.dialect),
        tokens: &tokens,
        significant: tokens
            .iter()
            .enumerate()
            .filter(|(_, t)| !t.kind.is_trivia())
            .map(|(i, _)| i)
            .collect(),
        parens: Vec::new(),
        out: Analysis::default(),
    }
    .run()
}

/// What an open parenthesis belongs to, which decides whether words inside
/// are types.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Paren {
    /// A function call; `name` is lowercase.
    Call(String),
    /// A column or parameter list: `CREATE TABLE t (`, `FUNCTION f(`.
    Definition,
    /// A type's arguments: `Nullable(`, `varchar(`.
    TypeArgs,
    Plain,
}

/// Keywords after which `name (` names an object, not a function call.
const DEFINES: &[&str] = &[
    "table",
    "into",
    "references",
    "index",
    "key",
    "view",
    "exists",
    "on",
    "type",
    "function",
    "procedure",
    "macro",
    "dictionary",
    "trigger",
    "sequence",
];

/// Keywords after which `name type` declares a column or variable.
const DECLARES: &[&str] = &["column", "add", "modify", "declare", "returns"];

const CASTS: &[&str] = &["cast", "try_cast", "safe_cast", "convert", "cast_as"];

struct Classifier<'a> {
    sql: &'a str,
    flavor: &'a Flavor,
    vocabulary: &'static Vocabulary,
    tokens: &'a [Token],
    /// Indices of non-trivia tokens.
    significant: Vec<usize>,
    parens: Vec<Paren>,
    out: Analysis,
}

impl Classifier<'_> {
    fn run(mut self) -> Analysis {
        let mut word_classes: Vec<Option<Highlight>> = vec![None; self.tokens.len()];
        let mut statement_start = true;
        for (pos, &ix) in self.significant.clone().iter().enumerate() {
            let token = &self.tokens[ix];
            let highlight = match token.kind {
                TokenKind::Word => {
                    let class = self.classify_word(pos, statement_start);
                    word_classes[ix] = class;
                    class
                }
                TokenKind::String => Some(Highlight::String),
                TokenKind::Number => Some(Highlight::Number),
                TokenKind::Parameter => Some(Highlight::Parameter),
                TokenKind::Operator => Some(Highlight::Operator),
                TokenKind::Punctuation | TokenKind::Delimiter => Some(Highlight::Punctuation),
                TokenKind::Bracket => {
                    self.track_paren(pos, &word_classes);
                    Some(Highlight::Bracket)
                }
                TokenKind::QuotedIdent => None,
                TokenKind::Whitespace
                | TokenKind::LineComment
                | TokenKind::BlockComment
                | TokenKind::DelimiterCommand => unreachable!("trivia is skipped"),
            };
            statement_start = token.kind == TokenKind::Delimiter;
            if token.kind == TokenKind::Delimiter {
                self.parens.clear();
            }
            if let Some(highlight) = highlight {
                self.push(token.range.clone(), highlight);
            }
        }
        // Comments and directives, merged in order.
        for token in self.tokens {
            let highlight = match token.kind {
                TokenKind::LineComment | TokenKind::BlockComment => Highlight::Comment,
                TokenKind::DelimiterCommand => Highlight::Directive,
                _ => continue,
            };
            self.out.spans.push(Span {
                range: token.range.clone(),
                highlight,
            });
        }
        self.out.spans.sort_by_key(|s| s.range.start);
        self.out
    }

    fn push(&mut self, range: Range<usize>, highlight: Highlight) {
        self.out.spans.push(Span { range, highlight });
    }

    fn token_at(&self, pos: Option<usize>) -> Option<&Token> {
        pos.and_then(|p| self.significant.get(p))
            .map(|&ix| &self.tokens[ix])
    }

    fn text(&self, token: &Token) -> &str {
        token.text(self.sql)
    }

    fn is(&self, pos: Option<usize>, kind: TokenKind, text: &str) -> bool {
        self.token_at(pos)
            .is_some_and(|t| t.kind == kind && self.text(t).eq_ignore_ascii_case(text))
    }

    fn is_keyword_in(&self, pos: Option<usize>, words: &[&str]) -> bool {
        self.token_at(pos).is_some_and(|t| {
            t.kind == TokenKind::Word && words.iter().any(|w| self.text(t).eq_ignore_ascii_case(w))
        })
    }

    fn track_paren(&mut self, pos: usize, word_classes: &[Option<Highlight>]) {
        let token = &self.tokens[self.significant[pos]];
        match self.text(token) {
            "(" => {
                let prev = pos.checked_sub(1);
                let paren = match self.token_at(prev) {
                    Some(t) if t.kind == TokenKind::Word => {
                        let ix = self.significant[pos - 1];
                        match word_classes[ix] {
                            Some(Highlight::Function) => Paren::Call(self.text(t).to_lowercase()),
                            Some(Highlight::Type) => Paren::TypeArgs,
                            _ if self.is_keyword_in(pos.checked_sub(2), DEFINES) => {
                                Paren::Definition
                            }
                            _ => Paren::Plain,
                        }
                    }
                    Some(t) if t.kind == TokenKind::QuotedIdent => {
                        if self.is_keyword_in(pos.checked_sub(2), DEFINES) {
                            Paren::Definition
                        } else {
                            Paren::Plain
                        }
                    }
                    _ => Paren::Plain,
                };
                self.parens.push(paren);
            }
            ")" => {
                self.parens.pop();
            }
            _ => {}
        }
    }

    /// Whether the word at `pos` sits where a type name goes.
    fn in_type_position(&self, pos: usize) -> bool {
        let prev = pos.checked_sub(1);
        if self.is(prev, TokenKind::Operator, "::") {
            return true;
        }
        let paren = self.parens.last();
        if self.is_keyword_in(prev, &["as"])
            && matches!(paren, Some(Paren::Call(name)) if CASTS.contains(&name.as_str()))
        {
            return true;
        }
        if matches!(paren, Some(Paren::TypeArgs)) {
            return true;
        }
        if self.is_keyword_in(prev, &["returns"]) {
            return true;
        }
        // `name TYPE` in a column list or after ADD COLUMN.
        let Some(name) = self.token_at(prev) else {
            return false;
        };
        if !matches!(name.kind, TokenKind::Word | TokenKind::QuotedIdent) {
            return false;
        }
        let before = pos.checked_sub(2);
        let opens_item = self.is(before, TokenKind::Bracket, "(")
            || self.is(before, TokenKind::Punctuation, ",");
        (opens_item && matches!(paren, Some(Paren::Definition)))
            || self.is_keyword_in(before, DECLARES)
    }

    fn classify_word(&mut self, pos: usize, statement_start: bool) -> Option<Highlight> {
        let token = self.tokens[self.significant[pos]].clone();
        let word = self.text(&token).to_owned();
        let prev = pos.checked_sub(1);
        let next = Some(pos + 1);
        let called = self.is(next, TokenKind::Bracket, "(");
        let typed_literal = self
            .token_at(next)
            .is_some_and(|t| t.kind == TokenKind::String);

        // Qualified names: `schema.table`, `t.column`, `schema.func()`.
        let after_dot = self.is(prev, TokenKind::Punctuation, ".");
        if self.is(next, TokenKind::Punctuation, ".") {
            return None;
        }
        if after_dot {
            return called.then_some(Highlight::Function);
        }

        let vocabulary = self.vocabulary;
        if vocabulary.is(&word, WordClass::Type)
            && (self.in_type_position(pos) || typed_literal)
            && self.available(&token, &word, WordClass::Type, false)
        {
            return Some(Highlight::Type);
        }

        if called {
            if vocabulary.is(&word, WordClass::Function) {
                return self
                    .available(&token, &word, WordClass::Function, true)
                    .then_some(Highlight::Function);
            }
            if vocabulary.is(&word, WordClass::Keyword) {
                return self.keyword(&token, &word, statement_start);
            }
            // `CREATE TABLE t (`: an object being defined, not a call.
            if self.is_keyword_in(prev, DEFINES) {
                return None;
            }
            return Some(Highlight::Function);
        }

        if vocabulary.is(&word, WordClass::Keyword) {
            return self.keyword(&token, &word, statement_start);
        }
        None
    }

    fn keyword(&mut self, token: &Token, word: &str, statement_start: bool) -> Option<Highlight> {
        let lower = word.to_ascii_lowercase();
        let entry = *self.vocabulary.get(word, WordClass::Keyword)?;
        let report = statement_start || entry.strict;
        if !self.check(token, word, &entry, report) {
            return None;
        }
        Some(match lower.as_str() {
            "true" | "false" => Highlight::Boolean,
            "null" => Highlight::Constant,
            _ => Highlight::Keyword,
        })
    }

    fn available(&mut self, token: &Token, word: &str, class: WordClass, report: bool) -> bool {
        match self.vocabulary.get(word, class) {
            Some(entry) => {
                let entry = *entry;
                self.check(token, word, &entry, report)
            }
            None => false,
        }
    }

    /// Whether `entry` exists in the connected release; records a note when
    /// it does not and `report` is set.
    fn check(&mut self, token: &Token, word: &str, entry: &Entry, report: bool) -> bool {
        let availability = entry.availability(self.flavor);
        if availability == Availability::Available {
            return true;
        }
        if report {
            let engine = self.flavor.dialect.name();
            let current = &self.flavor.version_text;
            let what = match entry.class {
                WordClass::Function => format!("{word}()"),
                _ => word.to_uppercase(),
            };
            let message = match availability {
                Availability::Since(since) => format!(
                    "{what} needs {engine} {} or later; this server is {current}",
                    since.short()
                ),
                Availability::Removed(until) => format!(
                    "{what} was removed in {engine} {}; this server is {current}",
                    until.short()
                ),
                Availability::Available => unreachable!(),
            };
            self.out.notes.push(VersionNote {
                range: token.range.clone(),
                message,
            });
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::Dialect;

    fn spans(sql: &str, flavor: &Flavor) -> Vec<(&'static str, String)> {
        analyze(sql, flavor)
            .spans
            .into_iter()
            .map(|s| (s.highlight.theme_name(), sql[s.range].to_owned()))
            .collect()
    }

    fn of<'a>(spans: &'a [(&'static str, String)], name: &str) -> Vec<&'a str> {
        spans
            .iter()
            .filter(|(n, _)| *n == name)
            .map(|(_, t)| t.as_str())
            .collect()
    }

    #[test]
    fn colors_words_by_role() {
        let pg = Flavor::new(Dialect::Postgres, "17.2");
        let s = spans(
            "SELECT count(*), my_fn(x), t.date, CAST(a AS integer), b::text, DATE '2024-01-01', null, TRUE\n\
             FROM public.t JOIN s ON t.id = s.id -- note\nWHERE x > $1;",
            &pg,
        );
        assert_eq!(
            of(&s, "keyword"),
            ["SELECT", "AS", "FROM", "JOIN", "ON", "WHERE"]
        );
        assert_eq!(of(&s, "function"), ["count", "my_fn", "CAST"]);
        assert_eq!(of(&s, "type"), ["integer", "text", "DATE"]);
        assert_eq!(of(&s, "constant"), ["null"]);
        assert_eq!(of(&s, "boolean"), ["TRUE"]);
        assert_eq!(of(&s, "comment"), ["-- note"]);
        assert_eq!(of(&s, "variable.special"), ["$1"]);
        assert_eq!(of(&s, "string"), ["'2024-01-01'"]);
    }

    #[test]
    fn types_in_column_definitions() {
        let pg = Flavor::new(Dialect::Postgres, "17.2");
        let s = spans(
            "CREATE TABLE orders (id bigint NOT NULL, total numeric(10, 2), \"Note\" text)",
            &pg,
        );
        assert_eq!(of(&s, "type"), ["bigint", "numeric", "text"]);
        // `orders (` defines a table, it is not a call.
        assert!(of(&s, "function").is_empty(), "{s:?}");
        // A column named like a type is not a type outside definitions.
        let s = spans("SELECT date, text FROM t", &pg);
        assert!(of(&s, "type").is_empty(), "{s:?}");
    }

    #[test]
    fn clickhouse_nested_types() {
        let ch = Flavor::new(Dialect::ClickHouse, "25.8.1.1");
        let s = spans(
            "CREATE TABLE t (id UInt64, tags Array(LowCardinality(String))) ENGINE = MergeTree",
            &ch,
        );
        assert_eq!(
            of(&s, "type"),
            ["UInt64", "Array", "LowCardinality", "String"]
        );
    }

    #[test]
    fn mysql_strings_and_comments() {
        let mysql = Flavor::new(Dialect::MySql, "8.4.3");
        let s = spans("# hi\nSELECT \"str\", `col`, @v FROM t", &mysql);
        assert_eq!(of(&s, "comment"), ["# hi"]);
        assert_eq!(of(&s, "string"), ["\"str\""]);
        assert_eq!(of(&s, "variable.special"), ["@v"]);
    }

    #[test]
    fn functions_follow_the_release() {
        let sql = "SELECT regexp_count(s, 'a') FROM t";
        let pg15 = Flavor::new(Dialect::Postgres, "15.8");
        let analysis = analyze(sql, &pg15);
        assert!(analysis.notes.is_empty());
        assert!(
            analysis
                .spans
                .iter()
                .any(|s| s.highlight == Highlight::Function)
        );

        let pg14 = Flavor::new(Dialect::Postgres, "14.13");
        let analysis = analyze(sql, &pg14);
        assert!(
            !analysis
                .spans
                .iter()
                .any(|s| s.highlight == Highlight::Function)
        );
        assert_eq!(
            analysis.notes,
            [VersionNote {
                range: 7..19,
                message: "regexp_count() needs PostgreSQL 15 or later; this server is 14.13".into()
            }]
        );
    }

    #[test]
    fn removed_functions_and_statement_keywords() {
        let mysql8 = Flavor::new(Dialect::MySql, "8.0.39");
        let notes = analyze("SELECT PASSWORD('x')", &mysql8).notes;
        assert_eq!(
            notes[0].message,
            "PASSWORD() was removed in MySQL 8.0.11; this server is 8.0.39"
        );
        assert!(
            analyze(
                "SELECT PASSWORD('x')",
                &Flavor::new(Dialect::MySql, "5.7.44")
            )
            .notes
            .is_empty()
        );

        // MERGE starts a statement: worth a note. As a column name: not.
        let pg14 = Flavor::new(Dialect::Postgres, "14.13");
        let notes = analyze("SELECT 1; MERGE INTO t USING s ON true", &pg14).notes;
        assert_eq!(notes.len(), 1);
        assert!(notes[0].message.starts_with("MERGE needs PostgreSQL 15"));
        assert!(analyze("SELECT merge FROM t", &pg14).notes.is_empty());

        // Strict keywords note anywhere.
        let mysql8013 = Flavor::new(Dialect::MySql, "8.0.13");
        let notes = analyze("SELECT * FROM a, LATERAL (SELECT 1) b", &mysql8013).notes;
        assert_eq!(notes.len(), 1, "{notes:?}");
    }

    #[test]
    fn unknown_version_assumes_everything() {
        let flavor = Flavor::unversioned(Dialect::Postgres);
        let analysis = analyze("MERGE INTO t USING s ON regexp_like(a, b)", &flavor);
        assert!(analysis.notes.is_empty());
    }

    #[test]
    fn spans_are_ordered_and_disjoint() {
        let sql = "/* a */ SELECT 1 -- b\n; DELIMITER //";
        for dialect in Dialect::ALL {
            let analysis = analyze(sql, &Flavor::unversioned(dialect));
            for pair in analysis.spans.windows(2) {
                assert!(pair[0].range.end <= pair[1].range.start, "{dialect}");
            }
        }
    }
}
