//! Filtering a result's rows with a small, SQL-like expression.
//!
//! The language is a `WHERE` clause without functions, plus plain words:
//!
//! - A word or a `'quoted phrase'` on its own keeps the rows where any
//!   column contains it, ignoring case: `alice`, `'new york'`, `2024-01`.
//! - `column op value`, with `=`, `!=` (or `<>`), `<`, `<=`, `>`, `>=`:
//!   `amount >= 100`, `status = 'paid'`, `created < '2024-06-01'`. A value is
//!   a number, a `'string'` (`''` for a quote), `TRUE`/`FALSE`, or a bare
//!   word read as a string (`status = paid`). Values are read as the
//!   column's type, so dates and timestamps compare as dates.
//! - `column IS [NOT] NULL`, `column [NOT] LIKE 'pattern'` (`%` and `_`;
//!   `ILIKE` ignores case), `column [NOT] IN (v1, v2, …)`,
//!   `column [NOT] BETWEEN low AND high`.
//! - `AND`, `OR`, `NOT` and parentheses combine them; terms side by side
//!   mean AND (`paid 2024` keeps rows containing both).
//!
//! Keywords ignore case. Column names are bare words or quoted with `"…"`,
//! `` `…` `` or `[…]`, and match exactly or else ignoring case. NULL compares
//! as SQL does: `x <> 1` drops rows where `x` is NULL.
//!
//! Evaluation runs per batch over the typed Arrow arrays with Arrow's
//! comparison kernels; text matching of non-text columns formats one batch
//! column at a time and lets it go.

use std::fmt;
use std::sync::Arc;

use arrow::array::StringArray;
use arrow::array::{Array, ArrayRef, BooleanArray, Datum, Float64Array, Int64Array, Scalar};
use arrow::compute::kernels::cmp;
use arrow::compute::kernels::comparison::{ilike, like, nilike, nlike};
use arrow::compute::{CastOptions, and_kleene, cast, cast_with_options, is_not_null, is_null};
use arrow::compute::{not, or_kleene};
use arrow::datatypes::{DataType, Schema};

use super::find::{Needle, matches_in};
use super::{Progress, Stopped, decode, is_binary, is_string, text_array};
use crate::ResultSet;

/// A parsed filter, ready to evaluate.
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    expr: Expr,
}

#[derive(Debug, Clone, PartialEq)]
enum Expr {
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    /// Any column contains the text, ignoring case.
    Contains(String),
    Compare {
        column: usize,
        op: Op,
        value: Literal,
    },
    IsNull {
        column: usize,
        negated: bool,
    },
    Like {
        column: usize,
        pattern: String,
        ignore_case: bool,
        negated: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
}

impl Op {
    fn symbol(self) -> &'static str {
        match self {
            Op::Eq => "=",
            Op::NotEq => "<>",
            Op::Lt => "<",
            Op::LtEq => "<=",
            Op::Gt => ">",
            Op::GtEq => ">=",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Literal {
    /// As written, so it can be read as the column's own numeric type.
    Number(String),
    String(String),
    Bool(bool),
}

/// Why a filter could not be parsed, and where (a character offset).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterError {
    pub message: String,
    pub at: usize,
}

impl fmt::Display for FilterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Filter {
    /// Parse `text` against the columns of `schema`. Blank text is `None`:
    /// no filter.
    pub fn parse(text: &str, schema: &Schema) -> Result<Option<Filter>, FilterError> {
        let tokens = tokenize(text)?;
        if tokens.is_empty() {
            return Ok(None);
        }
        let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        let mut parser = Parser {
            tokens: &tokens,
            at: 0,
            names: &names,
            end: text.chars().count(),
        };
        let expr = parser.or()?;
        if let Some(token) = parser.peek() {
            return Err(FilterError {
                message: format!("Unexpected {}", token.kind),
                at: token.at,
            });
        }
        Ok(Some(Filter { expr }))
    }

    /// Whether the filter is an `OR` at its top, so adding a condition
    /// with `AND` needs parentheses around it.
    pub fn is_disjunction(&self) -> bool {
        matches!(self.expr, Expr::Or(_))
    }

    /// For each row of `result`, whether the filter keeps it.
    pub fn evaluate(&self, result: &ResultSet, progress: Progress) -> Result<Vec<bool>, Error> {
        let mut keep = Vec::with_capacity(result.row_count());
        let total = result.row_count().max(1) as f32;
        // A value that cannot be read as a column's type matches nothing in
        // that batch; it is an error only when it fits no batch at all (a
        // SQLite column may be numbers in one batch and text in the next).
        let (mut failed, mut evaluated) = (None, false);
        for batch in result.batches() {
            if !progress(keep.len() as f32 / total) {
                return Err(Error::Stopped);
            }
            match eval(&self.expr, batch.columns(), batch.num_rows()) {
                Ok(mask) => {
                    evaluated = true;
                    keep.extend((0..mask.len()).map(|row| mask.is_valid(row) && mask.value(row)));
                }
                Err(Error::Invalid(message)) => {
                    failed.get_or_insert(message);
                    keep.extend(std::iter::repeat_n(false, batch.num_rows()));
                }
                Err(Error::Stopped) => return Err(Error::Stopped),
            }
        }
        match failed {
            Some(message) if !evaluated => Err(Error::Invalid(message)),
            _ => Ok(keep),
        }
    }
}

/// Why evaluating a filter stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A value could not be read as a column's type, or similar.
    Invalid(String),
    Stopped,
}

impl From<Stopped> for Error {
    fn from(_: Stopped) -> Self {
        Error::Stopped
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Invalid(message) => f.write_str(message),
            Error::Stopped => f.write_str("stopped"),
        }
    }
}

// ── Quick filters ────────────────────────────────────────────────────────────

/// A column name as the filter language reads it: bare when it is a plain
/// word, else in double quotes.
pub fn quote_column(name: &str) -> String {
    let plain = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && keyword(name).is_none();
    if plain {
        name.to_owned()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// `'text'`, with quotes doubled.
pub fn quote_string(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// The condition that keeps rows whose `col` equals the value at `row`
/// (`exclude`: differs from it), e.g. `status = 'paid'`, `id <> 42`,
/// `note IS NULL`.
pub fn value_condition(result: &ResultSet, row: usize, col: usize, exclude: bool) -> String {
    let column = quote_column(result.column_name(col));
    let Some((array, local)) = result.column(row, col) else {
        return String::new();
    };
    if array.is_null(local) {
        let not = if exclude { " NOT" } else { "" };
        return format!("{column} IS{not} NULL");
    }
    let op = if exclude { "<>" } else { "=" };
    let text = result.cell_value(row, col).unwrap_or_default();
    let data_type = array.data_type();
    let value = match data_type {
        DataType::Boolean => text.to_ascii_uppercase(),
        t if t.is_numeric() && text.parse::<f64>().is_ok_and(f64::is_finite) => text,
        _ => quote_string(&text),
    };
    format!("{column} {op} {value}")
}

/// `existing` narrowed by `condition`.
pub fn and(existing: Option<&str>, condition: &str, schema: &Schema) -> String {
    let existing = existing.map(str::trim).filter(|e| !e.is_empty());
    match existing {
        None => condition.to_owned(),
        Some(existing) => {
            let wrap = Filter::parse(existing, schema)
                .ok()
                .flatten()
                .is_none_or(|f| f.is_disjunction());
            if wrap {
                format!("({existing}) AND {condition}")
            } else {
                format!("{existing} AND {condition}")
            }
        }
    }
}

// ── Tokens ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Kind {
    Word(String),
    /// A quoted identifier.
    Ident(String),
    String(String),
    Op(Op),
    Keyword(Keyword),
    Open,
    Close,
    Comma,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Kind::Word(w) => write!(f, "`{w}`"),
            Kind::Ident(i) => write!(f, "\"{i}\""),
            Kind::String(s) => write!(f, "'{s}'"),
            Kind::Op(op) => write!(f, "`{}`", op.symbol()),
            Kind::Keyword(k) => write!(f, "{k}"),
            Kind::Open => f.write_str("`(`"),
            Kind::Close => f.write_str("`)`"),
            Kind::Comma => f.write_str("`,`"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keyword {
    And,
    Or,
    Not,
    Is,
    Null,
    Like,
    Ilike,
    In,
    Between,
    True,
    False,
}

impl fmt::Display for Keyword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", format!("{self:?}").to_uppercase())
    }
}

fn keyword(word: &str) -> Option<Keyword> {
    Some(match word.to_ascii_uppercase().as_str() {
        "AND" => Keyword::And,
        "OR" => Keyword::Or,
        "NOT" => Keyword::Not,
        "IS" => Keyword::Is,
        "NULL" => Keyword::Null,
        "LIKE" => Keyword::Like,
        "ILIKE" => Keyword::Ilike,
        "IN" => Keyword::In,
        "BETWEEN" => Keyword::Between,
        "TRUE" => Keyword::True,
        "FALSE" => Keyword::False,
        _ => return None,
    })
}

#[derive(Debug, Clone)]
struct Token {
    kind: Kind,
    /// Character offset.
    at: usize,
}

fn tokenize(text: &str) -> Result<Vec<Token>, FilterError> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let at = i;
        let mut push = |kind| tokens.push(Token { kind, at });
        match c {
            c if c.is_whitespace() => i += 1,
            '(' => {
                push(Kind::Open);
                i += 1;
            }
            ')' => {
                push(Kind::Close);
                i += 1;
            }
            ',' => {
                push(Kind::Comma);
                i += 1;
            }
            '=' => {
                push(Kind::Op(Op::Eq));
                i += if chars.get(i + 1) == Some(&'=') { 2 } else { 1 };
            }
            '!' if chars.get(i + 1) == Some(&'=') => {
                push(Kind::Op(Op::NotEq));
                i += 2;
            }
            '<' => match chars.get(i + 1) {
                Some('=') => {
                    push(Kind::Op(Op::LtEq));
                    i += 2;
                }
                Some('>') => {
                    push(Kind::Op(Op::NotEq));
                    i += 2;
                }
                _ => {
                    push(Kind::Op(Op::Lt));
                    i += 1;
                }
            },
            '>' => {
                if chars.get(i + 1) == Some(&'=') {
                    push(Kind::Op(Op::GtEq));
                    i += 2;
                } else {
                    push(Kind::Op(Op::Gt));
                    i += 1;
                }
            }
            '\'' | '"' | '`' | '[' => {
                let close = if c == '[' { ']' } else { c };
                let mut value = String::new();
                let mut j = i + 1;
                loop {
                    match chars.get(j) {
                        None => {
                            return Err(FilterError {
                                message: format!("Missing the closing {close}"),
                                at,
                            });
                        }
                        Some(&ch) if ch == close => {
                            if close != ']' && chars.get(j + 1) == Some(&close) {
                                value.push(close);
                                j += 2;
                            } else {
                                j += 1;
                                break;
                            }
                        }
                        Some(&ch) => {
                            value.push(ch);
                            j += 1;
                        }
                    }
                }
                push(if c == '\'' {
                    Kind::String(value)
                } else {
                    Kind::Ident(value)
                });
                i = j;
            }
            _ => {
                let mut j = i;
                while j < chars.len()
                    && !chars[j].is_whitespace()
                    && !matches!(
                        chars[j],
                        '(' | ')' | ',' | '=' | '<' | '>' | '\'' | '"' | '`'
                    )
                    && !(chars[j] == '!' && chars.get(j + 1) == Some(&'='))
                {
                    j += 1;
                }
                let word: String = chars[i..j].iter().collect();
                push(match keyword(&word) {
                    Some(k) => Kind::Keyword(k),
                    None => Kind::Word(word),
                });
                i = j;
            }
        }
    }
    Ok(tokens)
}

// ── Parsing ──────────────────────────────────────────────────────────────────

struct Parser<'a> {
    tokens: &'a [Token],
    at: usize,
    names: &'a [&'a str],
    /// Character length of the text, for errors at its end.
    end: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn peek_kind(&self) -> Option<&Kind> {
        self.peek().map(|t| &t.kind)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.at).cloned();
        self.at += token.is_some() as usize;
        token
    }

    fn here(&self) -> usize {
        self.peek().map_or(self.end, |t| t.at)
    }

    fn error(&self, message: impl Into<String>) -> FilterError {
        FilterError {
            message: message.into(),
            at: self.here(),
        }
    }

    fn eat(&mut self, keyword: Keyword) -> bool {
        if self.peek_kind() == Some(&Kind::Keyword(keyword)) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn or(&mut self) -> Result<Expr, FilterError> {
        let mut terms = vec![self.and()?];
        while self.eat(Keyword::Or) {
            terms.push(self.and()?);
        }
        Ok(if terms.len() == 1 {
            terms.pop().expect("one term")
        } else {
            Expr::Or(terms)
        })
    }

    fn and(&mut self) -> Result<Expr, FilterError> {
        let mut terms = vec![self.not()?];
        loop {
            if self.eat(Keyword::And) {
                terms.push(self.not()?);
                continue;
            }
            // Terms side by side mean AND.
            match self.peek_kind() {
                None | Some(Kind::Close) | Some(Kind::Keyword(Keyword::Or)) => break,
                _ => terms.push(self.not()?),
            }
        }
        Ok(if terms.len() == 1 {
            terms.pop().expect("one term")
        } else {
            Expr::And(terms)
        })
    }

    fn not(&mut self) -> Result<Expr, FilterError> {
        if self.eat(Keyword::Not) {
            return Ok(Expr::Not(Box::new(self.not()?)));
        }
        self.atom()
    }

    fn atom(&mut self) -> Result<Expr, FilterError> {
        let Some(token) = self.next() else {
            return Err(self.error("Expected a condition"));
        };
        match token.kind {
            Kind::Open => {
                let expr = self.or()?;
                match self.next() {
                    Some(Token {
                        kind: Kind::Close, ..
                    }) => Ok(expr),
                    _ => Err(FilterError {
                        message: "Missing `)`".into(),
                        at: token.at,
                    }),
                }
            }
            Kind::Word(word) | Kind::Ident(word) if self.starts_predicate() => {
                let column = self.column(&word, token.at)?;
                self.predicate(column)
            }
            Kind::Word(text) | Kind::String(text) | Kind::Ident(text) => Ok(Expr::Contains(text)),
            Kind::Keyword(Keyword::True | Keyword::False | Keyword::Null) => {
                Ok(Expr::Contains(token_text(&token.kind)))
            }
            other => Err(FilterError {
                message: format!("Expected a condition, found {other}"),
                at: token.at,
            }),
        }
    }

    /// Whether the next token makes the word before it a column.
    fn starts_predicate(&self) -> bool {
        match self.peek_kind() {
            Some(Kind::Op(_)) => true,
            Some(Kind::Keyword(
                Keyword::Is | Keyword::Like | Keyword::Ilike | Keyword::In | Keyword::Between,
            )) => true,
            Some(Kind::Keyword(Keyword::Not)) => matches!(
                self.tokens.get(self.at + 1).map(|t| &t.kind),
                Some(Kind::Keyword(
                    Keyword::Like | Keyword::Ilike | Keyword::In | Keyword::Between
                ))
            ),
            _ => false,
        }
    }

    fn column(&self, name: &str, at: usize) -> Result<usize, FilterError> {
        if let Some(ix) = self.names.iter().position(|n| *n == name) {
            return Ok(ix);
        }
        let folded: Vec<usize> = (0..self.names.len())
            .filter(|&ix| self.names[ix].eq_ignore_ascii_case(name))
            .collect();
        match folded.as_slice() {
            [ix] => Ok(*ix),
            [] => Err(FilterError {
                message: format!("No column named `{name}`"),
                at,
            }),
            _ => Err(FilterError {
                message: format!("More than one column is named `{name}`; quote it exactly"),
                at,
            }),
        }
    }

    fn predicate(&mut self, column: usize) -> Result<Expr, FilterError> {
        let token = self.next().expect("starts_predicate saw a token");
        match token.kind {
            Kind::Op(op) => {
                let value = self.literal()?;
                Ok(Expr::Compare { column, op, value })
            }
            Kind::Keyword(Keyword::Is) => {
                let negated = self.eat(Keyword::Not);
                if !self.eat(Keyword::Null) {
                    return Err(self.error("Expected NULL after IS"));
                }
                Ok(Expr::IsNull { column, negated })
            }
            Kind::Keyword(Keyword::Not) => {
                let inner = self.predicate(column)?;
                Ok(Expr::Not(Box::new(inner)))
            }
            Kind::Keyword(k @ (Keyword::Like | Keyword::Ilike)) => {
                let pattern = match self.literal()? {
                    Literal::String(s) | Literal::Number(s) => s,
                    Literal::Bool(b) => b.to_string(),
                };
                Ok(Expr::Like {
                    column,
                    pattern,
                    ignore_case: k == Keyword::Ilike,
                    negated: false,
                })
            }
            Kind::Keyword(Keyword::In) => {
                if !matches!(self.next().map(|t| t.kind), Some(Kind::Open)) {
                    return Err(self.error("Expected `(` after IN"));
                }
                let mut values = Vec::new();
                loop {
                    values.push(Expr::Compare {
                        column,
                        op: Op::Eq,
                        value: self.literal()?,
                    });
                    match self.next().map(|t| t.kind) {
                        Some(Kind::Comma) => {}
                        Some(Kind::Close) => break,
                        _ => return Err(self.error("Expected `,` or `)` in the IN list")),
                    }
                }
                Ok(Expr::Or(values))
            }
            Kind::Keyword(Keyword::Between) => {
                let low = self.literal()?;
                if !self.eat(Keyword::And) {
                    return Err(self.error("Expected AND in BETWEEN"));
                }
                let high = self.literal()?;
                Ok(Expr::And(vec![
                    Expr::Compare {
                        column,
                        op: Op::GtEq,
                        value: low,
                    },
                    Expr::Compare {
                        column,
                        op: Op::LtEq,
                        value: high,
                    },
                ]))
            }
            _ => unreachable!("starts_predicate checked the token"),
        }
    }

    fn literal(&mut self) -> Result<Literal, FilterError> {
        let Some(token) = self.next() else {
            return Err(self.error("Expected a value"));
        };
        match token.kind {
            Kind::String(s) => Ok(Literal::String(s)),
            Kind::Word(w) if is_number(&w) => Ok(Literal::Number(w)),
            Kind::Word(w) => Ok(Literal::String(w)),
            Kind::Keyword(Keyword::True) => Ok(Literal::Bool(true)),
            Kind::Keyword(Keyword::False) => Ok(Literal::Bool(false)),
            Kind::Keyword(Keyword::Null) => Err(FilterError {
                message: "Nothing equals NULL; use IS NULL or IS NOT NULL".into(),
                at: token.at,
            }),
            other => Err(FilterError {
                message: format!("Expected a value, found {other}"),
                at: token.at,
            }),
        }
    }
}

fn token_text(kind: &Kind) -> String {
    match kind {
        Kind::Keyword(k) => k.to_string(),
        other => other.to_string(),
    }
}

fn is_number(word: &str) -> bool {
    let digits = word.strip_prefix(['-', '+']).unwrap_or(word);
    digits.starts_with(|c: char| c.is_ascii_digit() || c == '.')
        && word.parse::<f64>().is_ok_and(f64::is_finite)
}

// ── Evaluation ───────────────────────────────────────────────────────────────

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

fn arrow_error(err: arrow::error::ArrowError) -> Error {
    invalid(err.to_string())
}

fn eval(expr: &Expr, columns: &[ArrayRef], rows: usize) -> Result<BooleanArray, Error> {
    match expr {
        Expr::And(terms) => fold(terms, columns, rows, and_kleene),
        Expr::Or(terms) => fold(terms, columns, rows, or_kleene),
        Expr::Not(inner) => not(&eval(inner, columns, rows)?).map_err(arrow_error),
        Expr::Contains(text) => {
            let needle = Needle::contains(text, false);
            let mut any = vec![false; rows];
            for column in columns {
                matches_in(column, &needle, &mut any);
            }
            Ok(BooleanArray::from(any))
        }
        Expr::IsNull { column, negated } => {
            let array = &columns[*column];
            if *negated {
                is_not_null(array.as_ref()).map_err(arrow_error)
            } else {
                is_null(array.as_ref()).map_err(arrow_error)
            }
        }
        Expr::Like {
            column,
            pattern,
            ignore_case,
            negated,
        } => {
            let array = decode(&columns[*column]);
            let text = text_array(&array);
            let pattern = string_scalar(pattern, text.data_type())?;
            let kernel = match (ignore_case, negated) {
                (false, false) => like,
                (true, false) => ilike,
                (false, true) => nlike,
                (true, true) => nilike,
            };
            kernel(&text, &pattern).map_err(arrow_error)
        }
        Expr::Compare { column, op, value } => compare(&columns[*column], *op, value),
    }
}

fn fold(
    terms: &[Expr],
    columns: &[ArrayRef],
    rows: usize,
    combine: fn(&BooleanArray, &BooleanArray) -> Result<BooleanArray, arrow::error::ArrowError>,
) -> Result<BooleanArray, Error> {
    let mut acc: Option<BooleanArray> = None;
    for term in terms {
        let mask = eval(term, columns, rows)?;
        acc = Some(match acc {
            None => mask,
            Some(acc) => combine(&acc, &mask).map_err(arrow_error)?,
        });
    }
    Ok(acc.unwrap_or_else(|| BooleanArray::from(vec![true; rows])))
}

/// A one-value string array of `data_type` (a string type), as a scalar.
fn string_scalar(text: &str, data_type: &DataType) -> Result<Scalar<ArrayRef>, Error> {
    let array: ArrayRef = Arc::new(StringArray::from(vec![text]));
    let array = cast(&array, data_type).map_err(arrow_error)?;
    Ok(Scalar::new(array))
}

fn run(op: Op, left: &dyn Datum, right: &dyn Datum) -> Result<BooleanArray, Error> {
    match op {
        Op::Eq => cmp::eq(left, right),
        Op::NotEq => cmp::neq(left, right),
        Op::Lt => cmp::lt(left, right),
        Op::LtEq => cmp::lt_eq(left, right),
        Op::Gt => cmp::gt(left, right),
        Op::GtEq => cmp::gt_eq(left, right),
    }
    .map_err(arrow_error)
}

/// `array op value`, reading the value as the array's type.
fn compare(array: &ArrayRef, op: Op, value: &Literal) -> Result<BooleanArray, Error> {
    let array = decode(array);
    let data_type = array.data_type().clone();
    let text = match value {
        Literal::Number(n) | Literal::String(n) => n.clone(),
        Literal::Bool(b) => b.to_string(),
    };
    match &data_type {
        DataType::Null => Ok(BooleanArray::from(vec![None; array.len()])),
        // Blobs, lists and structs compare as the text the grid shows.
        t if is_binary(t) || t.is_nested() || !super::sort::can_order(t) => {
            let text_values = text_array(&array);
            run(op, &text_values, &string_scalar(&text, &DataType::Utf8)?)
        }
        t if is_string(t) => run(op, &array, &string_scalar(&text, t)?),
        t if t.is_numeric() => {
            let Ok(number) = text.parse::<f64>() else {
                return Err(invalid(format!("{} is not a number", quote_string(&text))));
            };
            // An integer column against an integer that fits compares in
            // its own type; anything else compares as doubles.
            let integral = t.is_integer() && text.parse::<i64>().is_ok();
            if integral {
                let literal: ArrayRef = Arc::new(Int64Array::from(vec![text.parse::<i64>().ok()]));
                let strict = CastOptions {
                    safe: false,
                    ..Default::default()
                };
                if let Ok(literal) = cast_with_options(&literal, t, &strict) {
                    return run(op, &array, &Scalar::new(literal));
                }
            }
            let doubles = cast(&array, &DataType::Float64).map_err(arrow_error)?;
            let literal = Float64Array::from(vec![number]);
            run(op, &doubles, &Scalar::new(literal))
        }
        t => {
            // Booleans, dates, times, timestamps, intervals: the value is
            // read as the column's type, the way a SQL literal would be.
            let literal: ArrayRef = Arc::new(StringArray::from(vec![text.as_str()]));
            let strict = CastOptions {
                safe: false,
                ..Default::default()
            };
            match cast_with_options(&literal, t, &strict) {
                Ok(literal) => run(op, &array, &Scalar::new(literal)),
                Err(_) => Err(invalid(format!(
                    "{} is not a {}",
                    quote_string(&text),
                    crate::result::type_label(t)
                ))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::tests::{mixed, people};
    use arrow::array::{Date32Array, TimestampMicrosecondArray};
    use arrow::datatypes::{Field, TimeUnit};
    use arrow::record_batch::RecordBatch;

    fn rows(result: &ResultSet, text: &str) -> Vec<usize> {
        let filter = Filter::parse(text, result.schema())
            .unwrap_or_else(|e| panic!("{text}: {e}"))
            .expect("not blank");
        let keep = filter.evaluate(result, &mut |_| true).unwrap();
        keep.iter()
            .enumerate()
            .filter(|(_, k)| **k)
            .map(|(i, _)| i)
            .collect()
    }

    fn error(result: &ResultSet, text: &str) -> FilterError {
        Filter::parse(text, result.schema()).unwrap_err()
    }

    // people: (id, name, score)
    // 0: 3, carol, 2.5   1: 1, NULL, -1   2: 2, Bob, 2.5
    // 3: NULL, alice, NaN   4: 10, dave, NULL   5: -4, bob, 0

    #[test]
    fn plain_words_search_every_column() {
        let p = people();
        assert_eq!(rows(&p, "bob"), [2, 5]);
        assert_eq!(rows(&p, "BOB"), [2, 5]);
        assert_eq!(rows(&p, "2.5"), [0, 2]);
        assert_eq!(rows(&p, "'o' 2.5"), [0, 2]);
        assert_eq!(rows(&p, "ar"), [0]);
        assert_eq!(rows(&p, "nothing-like-it"), Vec::<usize>::new());
        assert!(Filter::parse("   ", p.schema()).unwrap().is_none());
    }

    #[test]
    fn compares_typed_values() {
        let p = people();
        assert_eq!(rows(&p, "id > 2"), [0, 4]);
        assert_eq!(rows(&p, "id >= 2.5"), [0, 4]);
        assert_eq!(rows(&p, "id = 2.0"), [2]);
        assert_eq!(rows(&p, "ID <> 3"), [1, 2, 4, 5]);
        assert_eq!(rows(&p, "id != 3"), [1, 2, 4, 5]);
        assert_eq!(rows(&p, "score < 1"), [1, 5]);
        assert_eq!(rows(&p, "name = 'bob'"), [5]);
        assert_eq!(rows(&p, "name = bob"), [5]);
        assert_eq!(rows(&p, "name > 'b'"), [0, 4, 5]);
        assert_eq!(rows(&p, "id=10"), [4]);
        assert_eq!(rows(&p, "id == 10"), [4]);
        assert_eq!(rows(&p, "id = 99999999999999999999"), Vec::<usize>::new());
    }

    #[test]
    fn handles_nulls_like_sql() {
        let p = people();
        assert_eq!(rows(&p, "id IS NULL"), [3]);
        assert_eq!(rows(&p, "name is not null"), [0, 2, 3, 4, 5]);
        assert_eq!(rows(&p, "NOT id = 3"), [1, 2, 4, 5]);
        assert_eq!(rows(&p, "NOT (id = 3 OR id IS NULL)"), [1, 2, 4, 5]);
        assert!(error(&p, "id = NULL").message.contains("IS NULL"));
    }

    #[test]
    fn combines_conditions() {
        let p = people();
        assert_eq!(rows(&p, "id > 0 AND score > 0"), [0, 2]);
        assert_eq!(rows(&p, "id > 5 OR name = 'alice'"), [3, 4]);
        assert_eq!(
            rows(&p, "(id > 5 OR name = 'alice') AND score IS NULL"),
            [4]
        );
        assert_eq!(rows(&p, "id > 0 bob"), [2]);
        assert_eq!(rows(&p, "id in (1, 2, 99)"), [1, 2]);
        assert_eq!(rows(&p, "id NOT IN (1, 2)"), [0, 4, 5]);
        assert_eq!(rows(&p, "id BETWEEN 1 AND 3"), [0, 1, 2]);
        assert_eq!(rows(&p, "id not between 1 and 3"), [4, 5]);
    }

    #[test]
    fn matches_patterns() {
        let p = people();
        assert_eq!(rows(&p, "name LIKE 'b%'"), [5]);
        assert_eq!(rows(&p, "name ILIKE 'b%'"), [2, 5]);
        assert_eq!(rows(&p, "name NOT LIKE '%o%'"), [3, 4]);
        assert_eq!(rows(&p, "name like '_ob'"), [2, 5]);
        // On a number column, the pattern matches its text.
        assert_eq!(rows(&p, "score LIKE '2.%'"), [0, 2]);
    }

    #[test]
    fn reads_values_as_dates_and_timestamps() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("d", DataType::Date32, true),
            Field::new("ts", DataType::Timestamp(TimeUnit::Microsecond, None), true),
        ]));
        let day = 19_723; // 2024-01-01
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Date32Array::from(vec![day, day + 31, day + 400])),
                Arc::new(TimestampMicrosecondArray::from(vec![
                    0,
                    86_400_000_000,
                    1_700_000_000_000_000,
                ])),
            ],
        )
        .unwrap();
        let result = crate::result::result_from_batches(schema, [batch]);
        assert_eq!(rows(&result, "d >= '2024-02-01'"), [1, 2]);
        assert_eq!(rows(&result, "d = 2024-01-01"), [0]);
        assert_eq!(rows(&result, "ts < '1970-01-02 00:00:01'"), [0, 1]);
        assert_eq!(rows(&result, "2024-02"), [1]);
        let filter = Filter::parse("d > 'soon'", result.schema())
            .unwrap()
            .unwrap();
        let err = filter.evaluate(&result, &mut |_| true).unwrap_err();
        assert_eq!(err, Error::Invalid("'soon' is not a date".into()));
    }

    #[test]
    fn works_across_batches_of_mixed_types() {
        let m = mixed();
        // 5, NULL, -2 | 'b', 'a' | 1.5 | x'00' | NULL
        assert_eq!(rows(&m, "v IS NULL"), [1, 7]);
        assert_eq!(rows(&m, "v = 'a'"), [4]);
        assert_eq!(rows(&m, "b"), [3]);
    }

    #[test]
    fn reports_where_it_cannot_parse() {
        let p = people();
        let e = error(&p, "nope = 1");
        assert_eq!((e.message.as_str(), e.at), ("No column named `nope`", 0));
        let e = error(&p, "id >");
        assert_eq!((e.message.as_str(), e.at), ("Expected a value", 4));
        let e = error(&p, "(id > 1");
        assert_eq!(e.message, "Missing `)`");
        let e = error(&p, "name = 'open");
        assert_eq!((e.message.as_str(), e.at), ("Missing the closing '", 7));
        let e = error(&p, "id > 1 )");
        assert_eq!(e.at, 7);
        assert!(error(&p, "id IS 3").message.contains("NULL"));
    }

    #[test]
    fn quotes_columns_and_values_for_quick_filters() {
        assert_eq!(quote_column("amount"), "amount");
        assert_eq!(quote_column("Order Id"), "\"Order Id\"");
        assert_eq!(quote_column("or"), "\"or\"");
        assert_eq!(quote_column("a\"b"), "\"a\"\"b\"");
        let p = people();
        assert_eq!(value_condition(&p, 0, 0, false), "id = 3");
        assert_eq!(value_condition(&p, 0, 1, false), "name = 'carol'");
        assert_eq!(value_condition(&p, 1, 1, false), "name IS NULL");
        assert_eq!(value_condition(&p, 1, 1, true), "name IS NOT NULL");
        assert_eq!(value_condition(&p, 0, 2, true), "score <> 2.5");
        // NaN is no number to type in; it is quoted, and still matches.
        assert_eq!(value_condition(&p, 3, 2, false), "score = 'NaN'");
        for (row, col) in [(0, 0), (0, 1), (2, 2), (5, 1)] {
            let condition = value_condition(&p, row, col, false);
            assert!(rows(&p, &condition).contains(&row), "{condition}");
        }
        assert_eq!(and(None, "id = 1", p.schema()), "id = 1");
        assert_eq!(and(Some("bob"), "id = 1", p.schema()), "bob AND id = 1");
        assert_eq!(
            and(Some("id = 2 OR id = 3"), "score > 0", p.schema()),
            "(id = 2 OR id = 3) AND score > 0"
        );
    }
}
