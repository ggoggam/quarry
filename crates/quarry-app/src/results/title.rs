//! What a statement is called on its result tab, and in one line of the
//! Output view: read from its tokens, without parsing it.
//!
//! A query is named after the table it reads (`SELECT * FROM shop.orders`
//! is `orders`, `FROM 'sales.parquet'` is `sales.parquet`), a change after
//! the table it changes, and anything else after its first words.

use quarry_core::Dialect;
use quarry_core::lex::{TokenKind, tokenize};

/// Longest title a result tab gets from its statement.
const MAX_TITLE_CHARS: usize = 32;

/// The statement's code tokens, comments and whitespace left out.
fn code(statement: &str, dialect: Dialect) -> Vec<(TokenKind, &str)> {
    tokenize(statement, &dialect.lexical())
        .into_iter()
        .filter(|t| !t.kind.is_trivia())
        .map(|t| (t.kind, t.text(statement)))
        .collect()
}

/// The first keyword, past any opening parentheses, in upper case.
fn first_keyword(tokens: &[(TokenKind, &str)]) -> Option<(usize, String)> {
    tokens
        .iter()
        .position(|(kind, text)| !(*kind == TokenKind::Bracket && *text == "("))
        .filter(|&ix| tokens[ix].0 == TokenKind::Word)
        .map(|ix| (ix, tokens[ix].1.to_ascii_uppercase()))
}

/// Whether the statement reads rather than changes: its result is rows to
/// look at, even when it has a single column.
pub fn is_query(statement: &str, dialect: Dialect) -> bool {
    let tokens = code(statement, dialect);
    first_keyword(&tokens).is_some_and(|(_, word)| {
        matches!(
            word.as_str(),
            "SELECT"
                | "WITH"
                | "VALUES"
                | "TABLE"
                | "FROM"
                | "SHOW"
                | "DESCRIBE"
                | "DESC"
                | "EXPLAIN"
                | "PRAGMA"
                | "SUMMARIZE"
                | "PIVOT"
                | "UNPIVOT"
                | "CALL"
        )
    })
}

/// A result tab's title for the statement that produced it.
pub fn title(statement: &str, dialect: Dialect) -> String {
    let tokens = code(statement, dialect);
    let named = first_keyword(&tokens).and_then(|(at, word)| {
        let rest = &tokens[at + 1..];
        match word.as_str() {
            "SELECT" | "WITH" | "VALUES" | "PIVOT" | "UNPIVOT" => after_top_level(rest, "FROM"),
            "FROM" | "TABLE" | "SUMMARIZE" => {
                let rest = skip_words(rest, &["TABLE"]);
                relation(rest)
            }
            "INSERT" | "REPLACE" | "MERGE" => after_top_level(rest, "INTO"),
            "UPDATE" => relation(skip_words(rest, &["ONLY", "OR", "IGNORE", "LOW_PRIORITY"])),
            "DELETE" => after_top_level(rest, "FROM"),
            "EXPLAIN" => {
                let rest = skip_words(rest, &["ANALYZE", "VERBOSE", "QUERY", "PLAN"]);
                Some(format!("EXPLAIN {}", title(&join(rest), dialect)))
            }
            _ => None,
        }
    });
    named
        .filter(|name| !name.trim().is_empty())
        .map(|name| cut(&name, MAX_TITLE_CHARS))
        .unwrap_or_else(|| first_words(&tokens))
}

/// The statement on one line, cut to `max_chars`: runs of whitespace become
/// one space.
pub fn preview(statement: &str, max_chars: usize) -> String {
    let mut line = String::new();
    for word in statement.split_whitespace() {
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
        if line.chars().count() > max_chars {
            break;
        }
    }
    cut(&line, max_chars)
}

fn cut(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    cut
}

/// Up to the first three words as typed, for statements named after what
/// they do: `SHOW TABLES`, `CREATE TABLE t`, `SELECT 42`.
fn first_words(tokens: &[(TokenKind, &str)]) -> String {
    let words: Vec<&str> = tokens.iter().take(3).map(|(_, text)| *text).collect();
    if words.is_empty() {
        return "Result".to_owned();
    }
    preview(&words.join(" "), MAX_TITLE_CHARS)
}

fn join(tokens: &[(TokenKind, &str)]) -> String {
    tokens
        .iter()
        .map(|(_, text)| *text)
        .collect::<Vec<_>>()
        .join(" ")
}

fn skip_words<'a, 'b>(
    mut tokens: &'b [(TokenKind, &'a str)],
    words: &[&str],
) -> &'b [(TokenKind, &'a str)] {
    while let Some(((TokenKind::Word, text), rest)) = tokens.split_first() {
        if !words.iter().any(|w| w.eq_ignore_ascii_case(text)) {
            break;
        }
        tokens = rest;
    }
    tokens
}

/// The relation after the first `keyword` outside parentheses, so a
/// subquery's or a CTE's `FROM` is not the one taken.
fn after_top_level(tokens: &[(TokenKind, &str)], keyword: &str) -> Option<String> {
    relation(tokens_after(tokens, keyword)?)
}

/// The tokens after the first `keyword` outside parentheses.
fn tokens_after<'a, 'b>(
    tokens: &'b [(TokenKind, &'a str)],
    keyword: &str,
) -> Option<&'b [(TokenKind, &'a str)]> {
    let mut depth = 0usize;
    for (ix, (kind, text)) in tokens.iter().enumerate() {
        match (kind, *text) {
            (TokenKind::Bracket, "(") => depth += 1,
            (TokenKind::Bracket, ")") => depth = depth.saturating_sub(1),
            (TokenKind::Word, word) if depth == 0 && word.eq_ignore_ascii_case(keyword) => {
                return Some(&tokens[ix + 1..]);
            }
            _ => {}
        }
    }
    None
}

/// The table a query reads, as SQL for `INSERT INTO`, spelled as the
/// statement spells it: `shop.orders`, `"Order Items"`, or `sales` for
/// `FROM 'sales.parquet'`. `None` when it
/// reads no single table (a subquery, a table function, no `FROM`).
pub fn source_table(statement: &str, dialect: Dialect) -> Option<String> {
    let tokens = code(statement, dialect);
    let (at, word) = first_keyword(&tokens)?;
    let rest = &tokens[at + 1..];
    let from = match word.as_str() {
        "SELECT" | "WITH" | "VALUES" => tokens_after(rest, "FROM")?,
        "FROM" | "TABLE" => skip_words(rest, &["TABLE"]),
        _ => return None,
    };
    let mut parts = Vec::new();
    let mut tokens = from.iter().peekable();
    while let Some((kind, text)) = tokens.next() {
        match kind {
            // As written: the statement named the table this way.
            TokenKind::Word | TokenKind::QuotedIdent => parts.push((*text).to_owned()),
            TokenKind::String if parts.is_empty() => {
                let path = unquote(text);
                let file = path.rsplit(['/', '\\']).next().unwrap_or(&path);
                let stem = file.split('.').next().unwrap_or(file);
                return (!stem.is_empty()).then(|| dialect.quote_ident(stem));
            }
            _ => return None,
        }
        match tokens.peek() {
            Some((TokenKind::Punctuation, ".")) => {
                tokens.next();
            }
            // A table function, not a table.
            Some((TokenKind::Bracket, "(")) => return None,
            _ => break,
        }
    }
    (!parts.is_empty()).then(|| parts.join("."))
}

/// The last part of a dotted name (`db.shop.orders` is `orders`), a file a
/// string names (`'data/sales.parquet'` is `sales.parquet`), or a table
/// function (`range(10)` is `range`). `None` for a subquery.
fn relation(tokens: &[(TokenKind, &str)]) -> Option<String> {
    let mut last = None;
    let mut tokens = tokens.iter().peekable();
    while let Some((kind, text)) = tokens.next() {
        let part = match kind {
            TokenKind::Word => (*text).to_owned(),
            TokenKind::QuotedIdent => unquote(text),
            TokenKind::String => {
                let path = unquote(text);
                return Some(
                    path.rsplit(['/', '\\'])
                        .next()
                        .unwrap_or(path.as_str())
                        .to_owned(),
                );
            }
            _ => return last,
        };
        last = Some(part);
        match tokens.peek() {
            Some((TokenKind::Punctuation, ".")) => {
                tokens.next();
            }
            _ => return last,
        }
    }
    last
}

/// `"name"`, `` `name` ``, `[name]` or `'text'` (also `E'text'`) without
/// its quotes, doubled quotes undone.
fn unquote(text: &str) -> String {
    let text = match text.find('\'') {
        Some(at) if !text.starts_with(['"', '`', '[']) => &text[at..],
        _ => text,
    };
    let Some(open) = text.chars().next() else {
        return String::new();
    };
    let close = match open {
        '"' | '`' | '\'' => open,
        '[' => ']',
        _ => return text.to_owned(),
    };
    let inner = text[open.len_utf8()..]
        .strip_suffix(close)
        .unwrap_or(&text[open.len_utf8()..]);
    match open {
        '[' => inner.to_owned(),
        quote => inner.replace(&format!("{quote}{quote}"), &quote.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(sql: &str) -> String {
        title(sql, Dialect::DuckDb)
    }

    #[test]
    fn names_queries_after_the_table_they_read() {
        assert_eq!(t("SELECT * FROM numbers"), "numbers");
        assert_eq!(t("select id from shop.main.orders o where 1"), "orders");
        assert_eq!(t("SELECT * FROM \"Order Items\""), "Order Items");
        assert_eq!(t("SELECT * FROM 'data/sales.parquet'"), "sales.parquet");
        assert_eq!(t("SELECT * FROM range(10) t(i)"), "range");
        assert_eq!(t("FROM numbers SELECT id"), "numbers");
        assert_eq!(t("TABLE numbers"), "numbers");
        assert_eq!(t("(SELECT a FROM x) UNION (SELECT a FROM y)"), "x");
        assert_eq!(
            title("SELECT * FROM `shop`.`orders`", Dialect::MySql),
            "orders"
        );
        assert_eq!(
            title("SELECT * FROM [order items]", Dialect::Sqlite),
            "order items"
        );
    }

    #[test]
    fn skips_subqueries_and_ctes() {
        assert_eq!(
            t("WITH recent AS (SELECT * FROM orders WHERE d > now()) SELECT * FROM recent"),
            "recent"
        );
        assert_eq!(
            t("SELECT (SELECT max(x) FROM other) AS m FROM main_table"),
            "main_table"
        );
        // A subquery in FROM has no name: the statement's first words.
        assert_eq!(t("SELECT * FROM (SELECT 1) s"), "SELECT * FROM");
    }

    #[test]
    fn names_changes_after_their_table_and_the_rest_by_their_words() {
        assert_eq!(t("INSERT INTO t VALUES (1)"), "t");
        assert_eq!(t("insert or replace into shop.t select 1"), "t");
        assert_eq!(t("UPDATE ONLY items SET a = 1"), "items");
        assert_eq!(t("DELETE FROM logs WHERE true"), "logs");
        assert_eq!(t("SHOW TABLES"), "SHOW TABLES");
        assert_eq!(t("describe   orders"), "describe orders");
        assert_eq!(t("SELECT 42"), "SELECT 42");
        assert_eq!(t("EXPLAIN ANALYZE SELECT * FROM t"), "EXPLAIN t");
        assert_eq!(t("-- note\nSELECT * FROM numbers"), "numbers");
        assert_eq!(t(""), "Result");
    }

    #[test]
    fn finds_the_table_to_insert_into() {
        let d = Dialect::DuckDb;
        assert_eq!(
            source_table("SELECT * FROM shop.orders o", d).as_deref(),
            Some("shop.orders")
        );
        assert_eq!(
            source_table("select * from \"Order Items\" where 1", d).as_deref(),
            Some("\"Order Items\"")
        );
        assert_eq!(
            source_table("FROM 'data/sales.parquet'", d).as_deref(),
            Some("sales")
        );
        assert_eq!(source_table("TABLE t", d).as_deref(), Some("t"));
        assert_eq!(
            source_table("WITH x AS (SELECT * FROM a) SELECT * FROM x", d).as_deref(),
            Some("x")
        );
        assert_eq!(source_table("SELECT * FROM range(10)", d), None);
        assert_eq!(source_table("SELECT * FROM (SELECT 1) s", d), None);
        assert_eq!(source_table("SELECT 42", d), None);
        assert_eq!(source_table("SHOW TABLES", d), None);
        assert_eq!(
            source_table("SELECT * FROM `Shop`.`Orders`", Dialect::MySql).as_deref(),
            Some("`Shop`.`Orders`")
        );
    }

    #[test]
    fn long_titles_are_cut() {
        let name = "a".repeat(80);
        let title = t(&format!("SELECT * FROM {name}"));
        assert_eq!(title.chars().count(), MAX_TITLE_CHARS);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn tells_queries_from_changes() {
        for query in [
            "SELECT 1",
            "with x as (select 1) select * from x",
            "(SELECT 1)",
            "FROM t",
            "SHOW TABLES",
            "PRAGMA version",
            "/* c */ describe t",
        ] {
            assert!(is_query(query, Dialect::DuckDb), "{query}");
        }
        for change in [
            "INSERT INTO t VALUES (1)",
            "CREATE TABLE t (a int)",
            "DROP TABLE t",
            "",
        ] {
            assert!(!is_query(change, Dialect::DuckDb), "{change}");
        }
    }

    #[test]
    fn previews_on_one_line() {
        assert_eq!(
            preview("SELECT a,\n       b\nFROM   t", 80),
            "SELECT a, b FROM t"
        );
        assert_eq!(preview("SELECT abcdef FROM t", 10), "SELECT ab…");
        assert_eq!(preview(&"x ".repeat(10_000), 5), "x x…");
    }
}
