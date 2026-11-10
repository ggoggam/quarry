//! ClickHouse's error messages, made readable.

/// The error code ClickHouse uses when a session is still busy with an
/// earlier query (`SESSION_IS_LOCKED`).
pub(crate) const SESSION_IS_LOCKED: u32 = 373;
/// `session_timeout` above the server's `max_session_timeout`.
pub(crate) const INVALID_SESSION_TIMEOUT: u32 = 374;

/// ClickHouse's message without the stack trace, the `(version ...)` suffix
/// and surrounding whitespace, e.g.
/// `Code: 60. DB::Exception: Table default.x does not exist. (UNKNOWN_TABLE)`.
pub(crate) fn clean_message(raw: &str) -> String {
    let mut text = raw.trim();
    // HTTP replies of some releases carry the stack trace after the message.
    for marker in [
        "\nStack trace",
        ", Stack trace",
        "Stack trace (when copying",
    ] {
        if let Some(at) = text.find(marker) {
            text = &text[..at];
        }
    }
    let mut text = text.trim_end().trim_end_matches(',').trim_end();
    // "... (UNKNOWN_TABLE) (version 23.8.16.40 (official build))", maybe
    // followed by the framing of an exception written into a stream.
    if let Some(at) = text.find(" (version ") {
        text = text[..at].trim_end();
    }
    // Old releases wrap the message as `e.displayText() = DB::Exception: ...`.
    let text = text.replace("e.displayText() = ", "");
    text.trim().to_owned()
}

/// The error code at the start of a ClickHouse message (`Code: 60. ...`).
pub(crate) fn error_code(message: &str) -> Option<u32> {
    let rest = message.trim_start().strip_prefix("Code: ")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// An exception ClickHouse wrote into a response after some of the result
/// had been sent, found in the last bytes of the body.
///
/// 23.x appends `Code: N. DB::Exception: ...` to the bytes so far, 25.8
/// prefixes it with `__exception__`, and releases that send an
/// `X-ClickHouse-Exception-Tag` header (26.10) frame it as
/// `__exception__\r\n<tag>\r\n<message>\r\n<len> <tag>\r\n__exception__`
/// and break the chunked encoding so the reply cannot pass for complete.
pub(crate) fn trailing_exception(tail: &[u8], tag: Option<&str>) -> Option<String> {
    let text = String::from_utf8_lossy(tail);
    if let Some(tag) = tag.filter(|t| !t.is_empty()) {
        let opening = format!("__exception__\r\n{tag}\r\n");
        if let Some(start) = text.rfind(&opening) {
            let body = &text[start + opening.len()..];
            let end = body.find(&format!(" {tag}\r\n")).map_or(body.len(), |end| {
                // Drop the "<len>" before the closing tag.
                body[..end].rfind("\r\n").unwrap_or(end)
            });
            return Some(clean_message(&body[..end]));
        }
    }
    let start = find_last_exception_start(&text)?;
    Some(clean_message(&text[start..]))
}

fn find_last_exception_start(text: &str) -> Option<usize> {
    let mut found = None;
    let mut from = 0;
    while let Some(at) = text[from..].find("Code: ") {
        let start = from + at;
        let rest = &text[start + 6..];
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && rest[digits..].starts_with(". DB::Exception") {
            found = Some(start);
        }
        from = start + 6;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_version_and_stack_trace() {
        let raw = "Code: 60. DB::Exception: Table default.x does not exist. (UNKNOWN_TABLE) \
                   (version 23.8.16.40 (official build))\n";
        assert_eq!(
            clean_message(raw),
            "Code: 60. DB::Exception: Table default.x does not exist. (UNKNOWN_TABLE)"
        );
        let raw = "Code: 62. DB::Exception: Syntax error: failed at position 1 ('SELEC'). \
                   (SYNTAX_ERROR) (version 25.8.1.1 (official build))\nStack trace:\n\n\
                   0. DB::Exception::Exception() @ 0x0\n1. foo\n";
        assert_eq!(
            clean_message(raw),
            "Code: 62. DB::Exception: Syntax error: failed at position 1 ('SELEC'). (SYNTAX_ERROR)"
        );
        let raw = "Code: 395. DB::Exception: boom. (X) (version 26.10.3.7 (official build))\r\n\
                   284 ekxsgkowgzfkapzi\r\n__exception__\r\n";
        assert_eq!(clean_message(raw), "Code: 395. DB::Exception: boom. (X)");
        let raw = "Code: 516. DB::Exception: default: Authentication failed: password is \
                   incorrect, or there is no user with such name. (AUTHENTICATION_FAILED) \
                   (version 24.8.4.13 (official build))";
        assert!(clean_message(raw).ends_with("(AUTHENTICATION_FAILED)"));
        assert_eq!(
            clean_message("Code: 1. e.displayText() = DB::Exception: x, Stack trace: 0. y"),
            "Code: 1. DB::Exception: x"
        );
    }

    #[test]
    fn reads_error_codes() {
        assert_eq!(
            error_code("Code: 373. DB::Exception: Session is locked"),
            Some(373)
        );
        assert_eq!(error_code("something else"), None);
    }

    #[test]
    fn finds_an_exception_appended_to_a_stream() {
        let mut body = vec![0xff, 0xff, 0xff, 0xff, 0x10, 0, 0, 0, b'C', b'o'];
        body.extend_from_slice(
            b"Code: 395. DB::Exception: Value passed to 'throwIf' function is non-zero: \
              while executing 'FUNCTION throwIf(...)'. (FUNCTION_THROW_IF_VALUE_IS_NON_ZERO) \
              (version 23.8.16.40 (official build))\n",
        );
        let message = trailing_exception(&body, None).unwrap();
        assert!(message.starts_with("Code: 395. DB::Exception: Value passed"));
        assert!(message.ends_with("(FUNCTION_THROW_IF_VALUE_IS_NON_ZERO)"));
        assert_eq!(trailing_exception(b"\x00\x01plain bytes", None), None);
    }

    #[test]
    fn finds_a_tagged_exception() {
        let tag = "abcdef0123456789";
        let message = "Code: 395. DB::Exception: boom. (FUNCTION_THROW_IF_VALUE_IS_NON_ZERO) \
                       (version 25.8.1.1 (official build))";
        let mut body = b"\x00\x00arrow bytes".to_vec();
        body.extend_from_slice(
            format!(
                "__exception__\r\n{tag}\r\n{message}\r\n{} {tag}\r\n__exception__\r\n",
                message.len()
            )
            .as_bytes(),
        );
        assert_eq!(
            trailing_exception(&body, Some(tag)).as_deref(),
            Some("Code: 395. DB::Exception: boom. (FUNCTION_THROW_IF_VALUE_IS_NON_ZERO)")
        );
    }
}
