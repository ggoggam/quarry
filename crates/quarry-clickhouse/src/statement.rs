//! Just enough of a statement's shape to decide how to run it.

/// What kind of statement a piece of SQL is, as far as running it goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// `SELECT`, `WITH ... SELECT`, `(SELECT ...)`: has a result whose
    /// column types `DESCRIBE` can tell.
    Query,
    /// `INSERT`: reports the rows it wrote.
    Insert,
    /// `USE db`: changes the session's current database.
    Use,
    /// Anything else: DDL, `SHOW`, `EXPLAIN`, `SYSTEM`, ...
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Statement<'a> {
    pub kind: Kind,
    /// The statement without trailing `;`, whitespace and comments.
    pub text: &'a str,
    /// The output format named by a trailing `FORMAT name` clause.
    pub format: Option<String>,
    /// The first keyword, upper case (`SELECT`, `SHOW`, ...).
    pub keyword: String,
}

impl<'a> Statement<'a> {
    pub fn analyze(sql: &'a str) -> Self {
        let tokens = tokenize(sql);
        let text = match tokens.last() {
            Some(last) => &sql[tokens[0].start..last.end],
            None => "",
        };
        let first = tokens
            .iter()
            .find(|t| t.kind != TokenKind::Punct || t.text(sql) != "(");
        let first_word = first
            .filter(|t| t.kind == TokenKind::Word)
            .map(|t| t.text(sql).to_ascii_uppercase())
            .unwrap_or_default();
        let kind = match first_word.as_str() {
            "SELECT" | "WITH" => Kind::Query,
            "INSERT" => Kind::Insert,
            "USE" => Kind::Use,
            _ => Kind::Other,
        };
        // An INSERT's FORMAT names the format of the data that follows it.
        let format = if kind == Kind::Insert {
            None
        } else {
            output_format(sql, &tokens)
        };
        Self {
            kind,
            text,
            format,
            keyword: first_word,
        }
    }

    /// Whether the statement produces rows that a `FORMAT` clause applies to.
    pub fn returns_rows(&self) -> bool {
        self.kind == Kind::Query
            || matches!(
                self.keyword.as_str(),
                "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN" | "EXISTS" | "CHECK"
            )
    }
}

fn output_format(sql: &str, tokens: &[Token]) -> Option<String> {
    let mut depth = 0i32;
    let mut found = None;
    for (i, token) in tokens.iter().enumerate() {
        match (token.kind, token.text(sql)) {
            (TokenKind::Punct, "(") => depth += 1,
            (TokenKind::Punct, ")") => depth -= 1,
            (TokenKind::Word, word)
                if depth == 0 && i > 0 && word.eq_ignore_ascii_case("FORMAT") =>
            {
                if let Some(next) = tokens.get(i + 1)
                    && matches!(next.kind, TokenKind::Word | TokenKind::QuotedIdent)
                    && tokens.get(i + 2).is_none_or(|after| {
                        // `FORMAT x SETTINGS ...`, or the end.
                        after.kind == TokenKind::Word
                            && after.text(sql).eq_ignore_ascii_case("SETTINGS")
                    })
                {
                    let name = next.text(sql).trim_matches(|c| c == '`' || c == '"');
                    found = Some(name.to_owned());
                }
            }
            _ => {}
        }
    }
    found
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    Word,
    QuotedIdent,
    String,
    Number,
    Punct,
}

#[derive(Debug, Clone, Copy)]
struct Token {
    kind: TokenKind,
    start: usize,
    end: usize,
}

impl Token {
    fn text<'s>(&self, sql: &'s str) -> &'s str {
        &sql[self.start..self.end]
    }
}

/// ClickHouse tokens, skipping whitespace, comments and `;`.
fn tokenize(sql: &str) -> Vec<Token> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let start = i;
        match c {
            b' ' | b'\t' | b'\n' | b'\r' | b';' => i += 1,
            b'-' if bytes.get(i + 1) == Some(&b'-') => i = line_end(bytes, i),
            b'#' if matches!(bytes.get(i + 1), Some(b' ' | b'!')) => i = line_end(bytes, i),
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
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
            b'\'' | b'"' | b'`' => {
                i = quoted_end(bytes, i);
                let kind = if c == b'\'' {
                    TokenKind::String
                } else {
                    TokenKind::QuotedIdent
                };
                tokens.push(Token {
                    kind,
                    start,
                    end: i,
                });
            }
            c if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 => {
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] >= 0x80)
                {
                    i += 1;
                }
                tokens.push(Token {
                    kind: TokenKind::Word,
                    start,
                    end: i,
                });
            }
            c if c.is_ascii_digit() => {
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'.') {
                    i += 1;
                }
                tokens.push(Token {
                    kind: TokenKind::Number,
                    start,
                    end: i,
                });
            }
            _ => {
                i += 1;
                // Keep char boundaries intact for multi-byte punctuation.
                while i < bytes.len() && !sql.is_char_boundary(i) {
                    i += 1;
                }
                tokens.push(Token {
                    kind: TokenKind::Punct,
                    start,
                    end: i,
                });
            }
        }
    }
    tokens
}

fn line_end(bytes: &[u8], from: usize) -> usize {
    bytes[from..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(bytes.len(), |n| from + n)
}

fn quoted_end(bytes: &[u8], from: usize) -> usize {
    let quote = bytes[from];
    let mut i = from + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b if b == quote => {
                if bytes.get(i + 1) == Some(&quote) {
                    i += 2;
                } else {
                    return i + 1;
                }
            }
            _ => i += 1,
        }
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_statements() {
        assert_eq!(Statement::analyze("select 1").kind, Kind::Query);
        assert_eq!(
            Statement::analyze("  -- hi\n WITH x AS (SELECT 1) SELECT * FROM x").kind,
            Kind::Query
        );
        assert_eq!(
            Statement::analyze("(SELECT 1) UNION ALL (SELECT 2)").kind,
            Kind::Query
        );
        assert_eq!(
            Statement::analyze("/* a */ INSERT INTO t VALUES (1)").kind,
            Kind::Insert
        );
        assert_eq!(Statement::analyze("use logs").kind, Kind::Use);
        assert_eq!(
            Statement::analyze("CREATE TABLE t (a Int8) ENGINE = Memory").kind,
            Kind::Other
        );
        assert_eq!(Statement::analyze("SHOW TABLES").kind, Kind::Other);
        assert!(Statement::analyze("show tables").returns_rows());
        assert!(Statement::analyze("(SELECT 1)").returns_rows());
        assert!(!Statement::analyze("DROP TABLE t").returns_rows());
    }

    #[test]
    fn trims_trailing_noise() {
        assert_eq!(Statement::analyze("SELECT 1;  -- done\n").text, "SELECT 1");
        assert_eq!(Statement::analyze("\n SELECT ';' ; ").text, "SELECT ';'");
        assert_eq!(Statement::analyze("SELECT 1 /* x */").text, "SELECT 1");
    }

    #[test]
    fn finds_an_explicit_output_format() {
        let format = |sql| Statement::analyze(sql).format;
        assert_eq!(format("SELECT 1 FORMAT CSV").as_deref(), Some("CSV"));
        assert_eq!(
            format("select 1 format JSONEachRow;").as_deref(),
            Some("JSONEachRow")
        );
        assert_eq!(
            format("SELECT 1 FORMAT TSV SETTINGS max_threads = 1").as_deref(),
            Some("TSV")
        );
        assert_eq!(format("SELECT 1"), None);
        assert_eq!(format("SELECT format('{}', 'x')"), None);
        assert_eq!(format("SELECT 'FORMAT CSV'"), None);
        assert_eq!(format("SELECT * FROM (SELECT 1 FORMAT CSV)"), None);
        assert_eq!(format("SELECT 1 AS format"), None);
        assert_eq!(format("INSERT INTO t FORMAT CSV 1,2"), None);
        assert_eq!(format("SELECT 1 -- FORMAT CSV"), None);
    }
}
