//! JSON in cell values: telling it apart from other text, and laying it out.
//!
//! Values are checked and re-indented token by token, never parsed into a
//! tree, so key order, number spelling and string escapes come out exactly as
//! they went in, and a value of many megabytes costs one pass.

use std::fmt::Write as _;

/// Nesting deeper than this is not treated as JSON, so checking a value can
/// never run out of stack.
const MAX_DEPTH: usize = 256;

/// Whether `text` is a JSON object or array, and nothing else. Scalars
/// (`42`, `"x"`, `true`) are valid JSON too, but reading them as JSON would
/// only get in the way.
pub fn is_json_document(text: &str) -> bool {
    let trimmed = text.trim_start();
    (trimmed.starts_with('{') || trimmed.starts_with('[')) && validate(text).is_ok()
}

/// Where a value stops being JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonError {
    /// Byte offset of the problem.
    pub offset: usize,
    pub message: &'static str,
}

/// Check that `text` holds exactly one JSON value (RFC 8259), with only
/// whitespace around it.
pub fn validate(text: &str) -> Result<(), JsonError> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        at: 0,
    };
    parser.whitespace();
    parser.value(0)?;
    parser.whitespace();
    if parser.at != parser.bytes.len() {
        return Err(parser.error("unexpected text after the value"));
    }
    Ok(())
}

/// `text` re-indented with `indent` spaces per level, or `None` when it is
/// not JSON. Empty objects and arrays stay on one line (`{}`, `[]`).
pub fn pretty(text: &str, indent: usize) -> Option<String> {
    validate(text).ok()?;
    Some(layout(text, Some(indent)))
}

/// `text` with the whitespace between tokens removed, or `None` when it is
/// not JSON.
pub fn compact(text: &str) -> Option<String> {
    validate(text).ok()?;
    Some(layout(text, None))
}

/// Append `value` to `out` as a JSON string, quotes included.
pub fn write_string(out: &mut String, value: &str) {
    out.reserve(value.len() + 2);
    out.push('"');
    let mut start = 0;
    for (at, ch) in value.char_indices() {
        let escape = match ch {
            '"' => "\\\"",
            '\\' => "\\\\",
            '\n' => "\\n",
            '\r' => "\\r",
            '\t' => "\\t",
            '\u{08}' => "\\b",
            '\u{0c}' => "\\f",
            ch if (ch as u32) < 0x20 => {
                out.push_str(&value[start..at]);
                let _ = write!(out, "\\u{:04x}", ch as u32);
                start = at + ch.len_utf8();
                continue;
            }
            _ => continue,
        };
        out.push_str(&value[start..at]);
        out.push_str(escape);
        start = at + ch.len_utf8();
    }
    out.push_str(&value[start..]);
    out.push('"');
}

/// What a piece of JSON text is, for highlighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    /// A string that is an object's key.
    Key,
    String,
    Number,
    /// `true`, `false` or `null`.
    Literal,
    /// Brackets, braces, commas and colons.
    Punctuation,
}

/// The tokens of `text` (best effort: it need not be valid), as byte
/// ranges, in order. Whitespace is left out.
pub fn tokens(text: &str) -> Vec<(std::ops::Range<usize>, Token)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b' ' | b'\t' | b'\n' | b'\r' => at += 1,
            b'"' => {
                let end = string_end(bytes, at);
                let next = skip_whitespace(bytes, end);
                let kind = if bytes.get(next) == Some(&b':') {
                    Token::Key
                } else {
                    Token::String
                };
                out.push((at..end, kind));
                at = end;
            }
            b'{' | b'}' | b'[' | b']' | b',' | b':' => {
                out.push((at..at + 1, Token::Punctuation));
                at += 1;
            }
            _ => {
                let start = at;
                while at < bytes.len()
                    && !matches!(
                        bytes[at],
                        b' ' | b'\t' | b'\n' | b'\r' | b',' | b':' | b'}' | b']' | b'"'
                    )
                {
                    at += 1;
                }
                if at == start {
                    at += 1;
                    continue;
                }
                let kind = if bytes[start] == b'-' || bytes[start].is_ascii_digit() {
                    Token::Number
                } else {
                    Token::Literal
                };
                // Stay on character boundaries for odd input.
                if text.is_char_boundary(start) && text.is_char_boundary(at) {
                    out.push((start..at, kind));
                }
            }
        }
    }
    out
}

/// Re-lay out valid JSON: pretty with `Some(indent)`, compact with `None`.
fn layout(text: &str, indent: Option<usize>) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    let mut depth = 0usize;
    let mut at = 0;
    let newline = |out: &mut String, depth: usize| {
        if let Some(indent) = indent {
            out.push('\n');
            out.extend(std::iter::repeat_n(' ', depth * indent));
        }
    };
    while at < bytes.len() {
        let byte = bytes[at];
        match byte {
            b' ' | b'\t' | b'\n' | b'\r' => at += 1,
            b'"' => {
                let end = string_end(bytes, at);
                out.push_str(&text[at..end]);
                at = end;
            }
            b'{' | b'[' => {
                let close = if byte == b'{' { b'}' } else { b']' };
                let next = skip_whitespace(bytes, at + 1);
                if bytes.get(next) == Some(&close) {
                    out.push(byte as char);
                    out.push(close as char);
                    at = next + 1;
                } else {
                    out.push(byte as char);
                    depth += 1;
                    newline(&mut out, depth);
                    at += 1;
                }
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(byte as char);
                at += 1;
            }
            b',' => {
                out.push(',');
                newline(&mut out, depth);
                at += 1;
            }
            b':' => {
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
                at += 1;
            }
            _ => {
                // A number or a literal: up to the next delimiter.
                let start = at;
                while at < bytes.len()
                    && !matches!(
                        bytes[at],
                        b' ' | b'\t' | b'\n' | b'\r' | b',' | b':' | b'}' | b']'
                    )
                {
                    at += 1;
                }
                out.push_str(&text[start..at]);
            }
        }
    }
    out
}

fn skip_whitespace(bytes: &[u8], mut at: usize) -> usize {
    while at < bytes.len() && matches!(bytes[at], b' ' | b'\t' | b'\n' | b'\r') {
        at += 1;
    }
    at
}

/// The offset just past the string starting at `at` (on its opening quote).
fn string_end(bytes: &[u8], at: usize) -> usize {
    let mut i = at + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn error(&self, message: &'static str) -> JsonError {
        JsonError {
            offset: self.at,
            message,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn whitespace(&mut self) {
        self.at = skip_whitespace(self.bytes, self.at);
    }

    fn value(&mut self, depth: usize) -> Result<(), JsonError> {
        if depth > MAX_DEPTH {
            return Err(self.error("nested too deeply"));
        }
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string(),
            Some(b't') => self.literal(b"true"),
            Some(b'f') => self.literal(b"false"),
            Some(b'n') => self.literal(b"null"),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error("expected a value")),
            None => Err(self.error("unexpected end")),
        }
    }

    fn object(&mut self, depth: usize) -> Result<(), JsonError> {
        self.at += 1;
        self.whitespace();
        if self.peek() == Some(b'}') {
            self.at += 1;
            return Ok(());
        }
        loop {
            self.whitespace();
            if self.peek() != Some(b'"') {
                return Err(self.error("expected a key"));
            }
            self.string()?;
            self.whitespace();
            if self.peek() != Some(b':') {
                return Err(self.error("expected ':'"));
            }
            self.at += 1;
            self.whitespace();
            self.value(depth + 1)?;
            self.whitespace();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    return Ok(());
                }
                _ => return Err(self.error("expected ',' or '}'")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<(), JsonError> {
        self.at += 1;
        self.whitespace();
        if self.peek() == Some(b']') {
            self.at += 1;
            return Ok(());
        }
        loop {
            self.whitespace();
            self.value(depth + 1)?;
            self.whitespace();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b']') => {
                    self.at += 1;
                    return Ok(());
                }
                _ => return Err(self.error("expected ',' or ']'")),
            }
        }
    }

    fn string(&mut self) -> Result<(), JsonError> {
        self.at += 1;
        loop {
            match self.peek() {
                None => return Err(self.error("unterminated string")),
                Some(b'"') => {
                    self.at += 1;
                    return Ok(());
                }
                Some(b'\\') => {
                    self.at += 1;
                    match self.peek() {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {
                            self.at += 1
                        }
                        Some(b'u') => {
                            self.at += 1;
                            for _ in 0..4 {
                                match self.peek() {
                                    Some(b) if b.is_ascii_hexdigit() => self.at += 1,
                                    _ => return Err(self.error("bad \\u escape")),
                                }
                            }
                        }
                        _ => return Err(self.error("bad escape")),
                    }
                }
                Some(b) if b < 0x20 => return Err(self.error("control character in string")),
                Some(_) => self.at += 1,
            }
        }
    }

    fn literal(&mut self, word: &[u8]) -> Result<(), JsonError> {
        if self.bytes[self.at..].starts_with(word) {
            self.at += word.len();
            Ok(())
        } else {
            Err(self.error("expected a value"))
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.at;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.at += 1;
        }
        self.at - start
    }

    fn number(&mut self) -> Result<(), JsonError> {
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        match self.peek() {
            Some(b'0') => self.at += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return Err(self.error("bad number")),
        }
        if self.peek() == Some(b'.') {
            self.at += 1;
            if self.digits() == 0 {
                return Err(self.error("bad number"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if self.digits() == 0 {
                return Err(self.error("bad number"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_documents_from_other_text() {
        for json in [
            r#"{"a": 1}"#,
            "[1, 2.5e-3, -0, true, null]",
            r#"  {"nested": {"x": [{}, []]}, "s": "a\"b\\cé"}  "#,
            "[]",
        ] {
            assert!(is_json_document(json), "{json}");
        }
        for text in [
            "42",
            r#""text""#,
            "{a: 1}",
            "[1, 2,]",
            "[01]",
            r#"{"a": 1} trailing"#,
            "[1.]",
            r#"["tab	inside"]"#,
            "{",
            "",
            "not json",
        ] {
            assert!(!is_json_document(text), "{text}");
        }
    }

    #[test]
    fn refuses_absurd_nesting() {
        let deep = format!("{}{}", "[".repeat(MAX_DEPTH + 2), "]".repeat(MAX_DEPTH + 2));
        assert!(validate(&deep).is_err());
        let fine = format!("{}{}", "[".repeat(10), "]".repeat(10));
        assert!(validate(&fine).is_ok());
    }

    #[test]
    fn pretty_prints_keeping_order_and_spelling() {
        let text = r#"{"z":1,"a":[1.50,{"k":"v, \"q\" ]"}],"e":{},"l":[ ]}"#;
        assert_eq!(
            pretty(text, 2).unwrap(),
            "{\n  \"z\": 1,\n  \"a\": [\n    1.50,\n    {\n      \"k\": \"v, \\\"q\\\" ]\"\n    }\n  ],\n  \"e\": {},\n  \"l\": []\n}"
        );
        assert_eq!(pretty("not json", 2), None);
    }

    #[test]
    fn compacts() {
        assert_eq!(
            compact("{ \"a\" : [ 1 , 2 ] ,\n \"b\" : \"x y\" }").unwrap(),
            r#"{"a":[1,2],"b":"x y"}"#
        );
        // Pretty and back again is the compact form.
        let text = r#"{"a":[1,{"b":null}],"c":"d"}"#;
        assert_eq!(compact(&pretty(text, 4).unwrap()).unwrap(), text);
    }

    #[test]
    fn tokenizes_for_highlighting() {
        let text = r#"{"k": ["v", 1.5, true]}"#;
        let kinds: Vec<(&str, Token)> = tokens(text)
            .into_iter()
            .map(|(r, k)| (&text[r], k))
            .collect();
        assert_eq!(
            kinds,
            [
                ("{", Token::Punctuation),
                ("\"k\"", Token::Key),
                (":", Token::Punctuation),
                ("[", Token::Punctuation),
                ("\"v\"", Token::String),
                (",", Token::Punctuation),
                ("1.5", Token::Number),
                (",", Token::Punctuation),
                ("true", Token::Literal),
                ("]", Token::Punctuation),
                ("}", Token::Punctuation),
            ]
        );
    }

    #[test]
    fn escapes_strings() {
        let mut out = String::new();
        write_string(&mut out, "a\"b\\c\nd\te\u{1}é");
        assert_eq!(out, r#""a\"b\\c\nd\te\u0001é""#);
        assert!(validate(&out).is_ok());
    }
}
