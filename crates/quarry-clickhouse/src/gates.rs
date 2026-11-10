//! What differs between ClickHouse releases, and how Quarry adapts.
//!
//! Each gate names the first release with the behavior, checked against
//! real servers (23.8, 25.8 and 26.10) and ClickHouse's own
//! `system.settings_changes`. When the exact release is unknown the gate
//! sits at the first release confirmed to have it, so an older server gets
//! the safe path (text instead of a native Arrow type, or no setting).
//!
//! Sending a setting a server does not know fails the query, so settings
//! are only sent to releases that have them.

use quarry_core::{Flavor, Version};

/// `output_format_arrow_compression_method` defaults to `lz4_frame`.
/// Older releases send uncompressed batches by default.
pub const ARROW_LZ4_DEFAULT: Version = Version::new(23, 3, 0);

/// `output_format_arrow_string_as_string` defaults to on. Quarry turns it
/// off and converts bytes to text itself, because a String holding invalid
/// UTF-8 would otherwise make the whole Arrow stream invalid.
pub const STRINGS_AS_ARROW_STRINGS: Version = Version::new(24, 3, 0);

/// `output_format_arrow_use_64_bit_indexes_for_dictionary` and
/// `..._use_signed_indexes_for_dictionary` exist. Before them, LowCardinality
/// columns written as dictionaries take their index type from the first
/// block and silently corrupt values once the dictionary outgrows it, so
/// older releases get plain values instead of dictionaries.
pub const SAFE_ARROW_DICTIONARIES: Version = Version::new(24, 1, 0);

/// `Date` is written as Arrow `Date32` (`output_format_arrow_date_as_uint16`
/// defaults to off). Before, it is a bare `UInt16` day number.
pub const DATE_AS_DATE32: Version = Version::new(26, 2, 0);

/// `output_format_arrow_unsupported_types=text` writes types Arrow has no
/// counterpart for (JSON, Dynamic, BFloat16, ...) as text. Before, they fail
/// the query (or, from 26.4, arrive as opaque bytes).
pub const UNSUPPORTED_TYPES_AS_TEXT: Version = Version::new(26, 10, 0);

/// `Time`, `Time64` and `Interval*` have Arrow counterparts (25.8 rejects
/// them; confirmed working in 26.10).
pub const TIME_IN_ARROW: Version = Version::new(26, 10, 0);

/// Settings for statements whose result Quarry reads as Arrow.
pub fn output_settings(flavor: &Flavor) -> Vec<(&'static str, String)> {
    let mut settings = vec![("default_format", "ArrowStream".to_owned())];
    let mut set = |name: &'static str, value: &str| settings.push((name, value.to_owned()));
    if flavor.supports(ARROW_LZ4_DEFAULT) {
        set("output_format_arrow_compression_method", "lz4_frame");
    }
    if flavor.supports(STRINGS_AS_ARROW_STRINGS) {
        set("output_format_arrow_string_as_string", "0");
    }
    if lowcardinality_as_dictionary(flavor) {
        set("output_format_arrow_low_cardinality_as_dictionary", "1");
        set("output_format_arrow_use_64_bit_indexes_for_dictionary", "1");
        set("output_format_arrow_use_signed_indexes_for_dictionary", "1");
    }
    if flavor.supports(DATE_AS_DATE32) {
        set("output_format_arrow_date_as_uint16", "0");
    }
    if flavor.supports(UNSUPPORTED_TYPES_AS_TEXT) {
        set("output_format_arrow_unsupported_types", "text");
    }
    settings
}

/// Whether LowCardinality columns arrive as Arrow dictionaries.
pub fn lowcardinality_as_dictionary(flavor: &Flavor) -> bool {
    flavor.supports(SAFE_ARROW_DICTIONARIES)
}

/// Whether `system.tables` has `total_rows` (since 19.x; every release
/// Quarry meets has it, but an unknown version gets the benefit).
pub fn tables_have_total_rows(flavor: &Flavor) -> bool {
    flavor.supports(Version::new(20, 1, 0))
}

/// Whether `system.tables` has `is_temporary`.
pub fn tables_have_is_temporary(flavor: &Flavor) -> bool {
    flavor.supports(Version::new(19, 1, 0))
}

/// How a result column is sent so it reads well once decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Treatment {
    /// The Arrow writer handles the type.
    Keep,
    /// A top-level `Date` written as a day number: send `toDate32(c)`.
    Date32,
    /// A top-level `DateTime`, which every release writes as a bare
    /// `UInt32`: send `toDateTime64(c, 0)`, an Arrow timestamp.
    DateTime64,
    /// No faithful Arrow form: send `toString(c)`.
    Text,
    /// Always NULL (`Nullable(Nothing)`), which no release's Arrow writer
    /// before 26.10 takes and `toString` keeps: send a NULL string.
    Null,
}

/// How to send a column of ClickHouse type `type_name` to this server.
/// `settings_allowed` is false for `readonly = 1` users, who cannot send
/// [`output_settings`], so only the server's defaults apply.
pub fn treatment(flavor: &Flavor, type_name: &str, settings_allowed: bool) -> Treatment {
    let text_mode = settings_allowed && flavor.supports(UNSUPPORTED_TYPES_AS_TEXT);
    match parse_type(type_name) {
        Some(ty) => treat(flavor, text_mode, &ty, true),
        None => Treatment::Text,
    }
}

/// `text_mode`: `output_format_arrow_unsupported_types=text` is in effect.
fn treat(flavor: &Flavor, text_mode: bool, ty: &Type, top: bool) -> Treatment {
    // A nested type that needs converting can only be converted along with
    // the whole column, as text.
    let children_keep = |ty: &Type| {
        if ty
            .types()
            .all(|t| treat(flavor, text_mode, t, false) == Treatment::Keep)
        {
            Treatment::Keep
        } else {
            Treatment::Text
        }
    };
    match ty.name.as_str() {
        "Nullable" | "LowCardinality" => match ty.types().next() {
            Some(inner) if inner.name == "Nothing" && top => {
                if text_mode {
                    Treatment::Keep
                } else {
                    Treatment::Null
                }
            }
            Some(inner) => treat(flavor, text_mode, inner, top),
            None => Treatment::Text,
        },
        "Array" | "Map" | "Tuple" | "Nested" => children_keep(ty),
        "SimpleAggregateFunction" => match ty.types().last() {
            Some(inner) => treat(flavor, text_mode, inner, top),
            None => Treatment::Text,
        },
        "String" | "FixedString" | "Bool" | "UInt8" | "UInt16" | "UInt32" | "UInt64" | "Int8"
        | "Int16" | "Int32" | "Int64" | "Float32" | "Float64" | "Decimal" | "Decimal32"
        | "Decimal64" | "Decimal128" | "Decimal256" | "Date32" | "DateTime64" | "Point"
        | "Ring" | "LineString" | "MultiLineString" | "Polygon" | "MultiPolygon" => Treatment::Keep,
        "Date" if flavor.supports(DATE_AS_DATE32) => Treatment::Keep,
        "Date" if top => Treatment::Date32,
        "DateTime" if top => Treatment::DateTime64,
        "JSON" | "Object" | "Dynamic" | "BFloat16" | "AggregateFunction" | "Nothing"
            if text_mode =>
        {
            Treatment::Keep
        }
        "Time" | "Time64" if flavor.supports(TIME_IN_ARROW) => Treatment::Keep,
        name if name.starts_with("Interval") && flavor.supports(TIME_IN_ARROW) => Treatment::Keep,
        // UUID, IPv4/IPv6 and Enum come out as bare numbers or bytes, wide
        // integers as bytes, Variant as a union Quarry cannot draw; the
        // rest has no Arrow form on this release, or is unknown to Quarry.
        _ => Treatment::Text,
    }
}

/// Rewrite `query` so columns that need it are converted on the server:
/// `SELECT * REPLACE (toString(c) AS c) FROM (<query>)`. `None` when no
/// column needs it, or when duplicate names make the rewrite ambiguous.
pub fn rewrite(
    query: &str,
    columns: &[(String, String)],
    treatments: &[Treatment],
) -> Option<String> {
    let replaced: Vec<String> = columns
        .iter()
        .zip(treatments)
        .filter_map(|((name, _), treatment)| {
            let quoted = quote(name);
            let expr = match treatment {
                Treatment::Keep => return None,
                Treatment::Date32 => format!("toDate32({quoted})"),
                Treatment::DateTime64 => format!("toDateTime64({quoted}, 0)"),
                Treatment::Text => format!("toString({quoted})"),
                Treatment::Null => "CAST(NULL, 'Nullable(String)')".to_owned(),
            };
            Some(format!("{expr} AS {quoted}"))
        })
        .collect();
    if replaced.is_empty() {
        return None;
    }
    let mut names: Vec<&str> = columns.iter().map(|(n, _)| n.as_str()).collect();
    names.sort_unstable();
    if names.windows(2).any(|w| w[0] == w[1]) {
        return None;
    }
    Some(format!(
        "SELECT * REPLACE ({}) FROM (\n{query}\n)",
        replaced.join(", ")
    ))
}

fn quote(name: &str) -> String {
    format!("`{}`", name.replace('\\', "\\\\").replace('`', "\\`"))
}

/// A parsed ClickHouse type name, e.g. `Map(String, Array(Nullable(Int8)))`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Type {
    name: String,
    args: Vec<Arg>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Arg {
    Type(Type),
    /// A named tuple element: `a UInt8`.
    Named(String, Type),
    /// `3`, `'UTC'`, `'a' = 1`, ...
    Literal,
}

impl Type {
    /// The types among the arguments (named tuple elements included).
    fn types(&self) -> impl Iterator<Item = &Type> {
        self.args.iter().filter_map(|arg| match arg {
            Arg::Type(t) | Arg::Named(_, t) => Some(t),
            Arg::Literal => None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Ident(String),
    Literal,
    Open,
    Close,
    Comma,
    Equals,
}

fn tokenize_type(text: &str) -> Option<Vec<Token>> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\n' => i += 1,
            '(' => {
                tokens.push(Token::Open);
                i += 1;
            }
            ')' => {
                tokens.push(Token::Close);
                i += 1;
            }
            ',' => {
                tokens.push(Token::Comma);
                i += 1;
            }
            '=' => {
                tokens.push(Token::Equals);
                i += 1;
            }
            '\'' | '`' | '"' => {
                let quote = c;
                let mut value = String::new();
                i += 1;
                loop {
                    let c = *chars.get(i)?;
                    i += 1;
                    match c {
                        '\\' => {
                            value.push(*chars.get(i)?);
                            i += 1;
                        }
                        c if c == quote => break,
                        c => value.push(c),
                    }
                }
                tokens.push(if quote == '\'' {
                    Token::Literal
                } else {
                    Token::Ident(value)
                });
            }
            c if c.is_ascii_digit() || c == '-' || c == '.' => {
                while i < chars.len()
                    && (chars[i].is_ascii_alphanumeric() || "-.+".contains(chars[i]))
                {
                    i += 1;
                }
                tokens.push(Token::Literal);
            }
            c if c.is_alphanumeric() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                tokens.push(Token::Ident(chars[start..i].iter().collect()));
            }
            _ => return None,
        }
    }
    Some(tokens)
}

fn parse_type(text: &str) -> Option<Type> {
    let tokens = tokenize_type(text)?;
    let mut pos = 0;
    let ty = parse_type_at(&tokens, &mut pos)?;
    (pos == tokens.len()).then_some(ty)
}

fn parse_type_at(tokens: &[Token], pos: &mut usize) -> Option<Type> {
    let Token::Ident(name) = tokens.get(*pos)? else {
        return None;
    };
    *pos += 1;
    let mut args = Vec::new();
    if tokens.get(*pos) == Some(&Token::Open) {
        *pos += 1;
        if tokens.get(*pos) == Some(&Token::Close) {
            *pos += 1;
        } else {
            loop {
                args.push(parse_arg(tokens, pos)?);
                match tokens.get(*pos)? {
                    Token::Comma => *pos += 1,
                    Token::Close => {
                        *pos += 1;
                        break;
                    }
                    _ => return None,
                }
            }
        }
    }
    Some(Type {
        name: name.clone(),
        args,
    })
}

fn parse_arg(tokens: &[Token], pos: &mut usize) -> Option<Arg> {
    match tokens.get(*pos)? {
        Token::Literal => {
            *pos += 1;
            // Enum values: 'a' = 1.
            if tokens.get(*pos) == Some(&Token::Equals) {
                *pos += 1;
                if tokens.get(*pos) != Some(&Token::Literal) {
                    return None;
                }
                *pos += 1;
            }
            Some(Arg::Literal)
        }
        Token::Ident(name) => {
            // `name Type` is a named tuple element.
            if let Some(Token::Ident(_)) = tokens.get(*pos + 1) {
                *pos += 1;
                Some(Arg::Named(name.clone(), parse_type_at(tokens, pos)?))
            } else {
                Some(Arg::Type(parse_type_at(tokens, pos)?))
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use quarry_core::Dialect;

    use super::*;

    fn ch(version: &str) -> Flavor {
        Flavor::new(Dialect::ClickHouse, version)
    }

    fn names(settings: &[(&str, String)]) -> Vec<String> {
        settings.iter().map(|(n, _)| n.to_string()).collect()
    }

    #[test]
    fn sends_only_settings_the_release_has() {
        let old = names(&output_settings(&ch("23.8.16.16")));
        assert_eq!(
            old,
            ["default_format", "output_format_arrow_compression_method"]
        );
        let lts = names(&output_settings(&ch("25.8.33.6")));
        assert!(lts.contains(&"output_format_arrow_string_as_string".into()));
        assert!(lts.contains(&"output_format_arrow_use_64_bit_indexes_for_dictionary".into()));
        assert!(!lts.contains(&"output_format_arrow_date_as_uint16".into()));
        assert!(!lts.contains(&"output_format_arrow_unsupported_types".into()));
        let new = names(&output_settings(&ch("26.10.3.7")));
        assert!(new.contains(&"output_format_arrow_date_as_uint16".into()));
        assert!(new.contains(&"output_format_arrow_unsupported_types".into()));
        let ancient = names(&output_settings(&ch("22.8.1.1")));
        assert_eq!(ancient, ["default_format"]);
    }

    #[test]
    fn dictionaries_only_where_they_are_safe() {
        assert!(!lowcardinality_as_dictionary(&ch("23.8.16.16")));
        assert!(lowcardinality_as_dictionary(&ch("24.1.1.1")));
        assert!(lowcardinality_as_dictionary(&Flavor::unversioned(
            Dialect::ClickHouse
        )));
    }

    #[test]
    fn parses_type_names() {
        let ty = parse_type("Map(String, Array(Nullable(Int8)))").unwrap();
        assert_eq!(ty.name, "Map");
        assert_eq!(ty.types().count(), 2);
        let ty = parse_type("Tuple(a UInt8, `b c` Nullable(String))").unwrap();
        assert!(
            matches!(&ty.args[1], Arg::Named(name, t) if name == "b c" && t.name == "Nullable")
        );
        assert!(parse_type("Enum8('a' = 1, 'b\\'s' = -2)").is_some());
        assert!(parse_type("DateTime64(3, 'Asia/Seoul')").is_some());
        assert!(parse_type("AggregateFunction(quantiles(0.5, 0.9), UInt64)").is_some());
        assert!(parse_type("Decimal(38, 10)").is_some());
        assert!(parse_type("Array(").is_none());
    }

    #[test]
    fn keeps_what_arrow_carries() {
        let lts = ch("25.8.33.6");
        for ty in [
            "UInt64",
            "Nullable(String)",
            "LowCardinality(Nullable(String))",
            "Decimal(18, 2)",
            "DateTime64(3, 'UTC')",
            "Array(Nullable(Float64))",
            "Map(String, Array(UInt8))",
            "Tuple(a UInt8, b String)",
            "Date32",
            "FixedString(4)",
            "SimpleAggregateFunction(sum, UInt64)",
        ] {
            assert_eq!(treatment(&lts, ty, true), Treatment::Keep, "{ty}");
        }
    }

    #[test]
    fn converts_what_arrow_mangles() {
        let lts = ch("25.8.33.6");
        assert_eq!(treatment(&lts, "Date", true), Treatment::Date32);
        assert_eq!(treatment(&lts, "Nullable(Date)", true), Treatment::Date32);
        assert_eq!(
            treatment(&lts, "DateTime('Asia/Seoul')", true),
            Treatment::DateTime64
        );
        assert_eq!(treatment(&lts, "Array(Date)", true), Treatment::Text);
        assert_eq!(treatment(&lts, "Array(DateTime)", true), Treatment::Text);
        assert_eq!(treatment(&lts, "Nullable(Nothing)", true), Treatment::Null);
        assert_eq!(treatment(&lts, "Array(Nothing)", true), Treatment::Text);
        assert_eq!(
            treatment(&ch("26.10.3.7"), "Nullable(Nothing)", true),
            Treatment::Keep
        );
        for ty in [
            "UUID",
            "IPv4",
            "Nullable(IPv6)",
            "Enum8('a' = 1, 'b' = 2)",
            "Int128",
            "UInt256",
            "Variant(String, UInt8)",
            "JSON",
            "Dynamic",
            "Map(String, UUID)",
            "Tuple(id UUID, n UInt8)",
            "AggregateFunction(uniq, UInt8)",
            "IntervalDay",
            "Time",
            "SomethingNew(1)",
        ] {
            assert_eq!(treatment(&lts, ty, true), Treatment::Text, "{ty}");
        }
    }

    #[test]
    fn newer_releases_need_fewer_conversions() {
        let new = ch("26.10.3.7");
        assert_eq!(treatment(&new, "Date", true), Treatment::Keep);
        assert_eq!(treatment(&new, "Array(Date)", true), Treatment::Keep);
        assert_eq!(treatment(&new, "JSON", true), Treatment::Keep);
        assert_eq!(treatment(&new, "Array(JSON)", true), Treatment::Keep);
        assert_eq!(treatment(&new, "Time64(3)", true), Treatment::Keep);
        // Still no faithful Arrow form.
        assert_eq!(treatment(&new, "UUID", true), Treatment::Text);
        assert_eq!(
            treatment(&new, "Variant(String, UInt8)", true),
            Treatment::Text
        );
        assert_eq!(treatment(&new, "DateTime", true), Treatment::DateTime64);
        // A readonly user cannot ask for text, so JSON is converted by hand;
        // Date32 is 26.2's default and needs no setting.
        assert_eq!(treatment(&new, "JSON", false), Treatment::Text);
        assert_eq!(treatment(&new, "Date", false), Treatment::Keep);
    }

    #[test]
    fn rewrites_only_what_needs_it() {
        let columns = vec![
            ("id".to_owned(), "UInt64".to_owned()),
            ("u`id".to_owned(), "UUID".to_owned()),
            ("day".to_owned(), "Date".to_owned()),
        ];
        let lts = ch("25.8.33.6");
        let treatments: Vec<_> = columns
            .iter()
            .map(|(_, t)| treatment(&lts, t, true))
            .collect();
        assert_eq!(
            rewrite("SELECT 1", &columns, &treatments).unwrap(),
            "SELECT * REPLACE (toString(`u\\`id`) AS `u\\`id`, toDate32(`day`) AS `day`) \
             FROM (\nSELECT 1\n)"
        );
        assert_eq!(rewrite("SELECT 1", &columns[..1], &treatments[..1]), None);
        let dup = vec![columns[1].clone(), columns[1].clone()];
        assert_eq!(rewrite("SELECT 1", &dup, &[Treatment::Text; 2]), None);
    }
}
