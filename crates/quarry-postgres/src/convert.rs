//! PostgreSQL text-format values into typed Arrow arrays.
//!
//! Rows arrive in the text format, which every type has, so nothing is ever
//! undisplayable. Common types are parsed into their Arrow equivalents; the
//! rest stay text, tagged with the PostgreSQL type name. When a batch holds
//! a value the Arrow type cannot carry (`'infinity'::date`, `NaN::numeric`,
//! an unusual `DateStyle`), that batch's column falls back to text, which
//! [`quarry_core::ResultSet`] allows: cells are formatted per batch.

use std::sync::Arc;

use arrow::array::{
    ArrayRef, BinaryArray, BooleanArray, Date32Array, Decimal128Array, Float32Array, Float64Array,
    Int16Array, Int32Array, Int64Array, IntervalMonthDayNanoArray, StringArray,
    Time64MicrosecondArray, TimestampMicrosecondArray, UInt32Array,
};
use arrow::datatypes::{DataType, IntervalMonthDayNano, IntervalUnit, TimeUnit};
use tokio_postgres::types::{Kind, Type};

/// The zone timestamptz values are given in. An offset rather than `"UTC"`:
/// Arrow can only display named zones with its chrono-tz feature.
const UTC: &str = "+00:00";

/// Largest precision a Decimal128 holds.
const MAX_DECIMAL_DIGITS: usize = 38;

/// How a column's text values become an Arrow array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnKind {
    Bool,
    Int16,
    Int32,
    Int64,
    Oid,
    Float32,
    Float64,
    /// `numeric(p,s)` with `p <= 38`.
    Decimal {
        precision: u8,
        scale: i8,
    },
    /// Unconstrained `numeric`: each batch takes the scale its values need.
    Numeric,
    Date,
    Time,
    Timestamp,
    TimestampTz,
    Interval,
    Bytea,
    Text,
}

impl ColumnKind {
    /// The kind for a column of type `ty` with type modifier `typmod`.
    pub fn of(ty: &Type, typmod: i32) -> Self {
        if let Kind::Domain(base) = ty.kind() {
            // A domain prints as its base type.
            return Self::of(base, -1);
        }
        match *ty {
            Type::BOOL => Self::Bool,
            Type::INT2 => Self::Int16,
            Type::INT4 => Self::Int32,
            Type::INT8 => Self::Int64,
            Type::OID => Self::Oid,
            Type::FLOAT4 => Self::Float32,
            Type::FLOAT8 => Self::Float64,
            Type::NUMERIC => match numeric_typmod(typmod) {
                None => Self::Numeric,
                Some((precision, scale)) => {
                    // PostgreSQL 15+ allows a scale above the precision
                    // (numeric(3,5)) or below zero (numeric(5,-2)).
                    let digits = precision.max(scale.max(0) as u16);
                    match (u8::try_from(digits), i8::try_from(scale)) {
                        (Ok(precision), Ok(scale))
                            if (1..=MAX_DECIMAL_DIGITS as u8).contains(&precision)
                                && scale >= -(MAX_DECIMAL_DIGITS as i8) =>
                        {
                            Self::Decimal { precision, scale }
                        }
                        _ => Self::Text,
                    }
                }
            },
            Type::DATE => Self::Date,
            Type::TIME => Self::Time,
            Type::TIMESTAMP => Self::Timestamp,
            Type::TIMESTAMPTZ => Self::TimestampTz,
            Type::INTERVAL => Self::Interval,
            Type::BYTEA => Self::Bytea,
            _ => Self::Text,
        }
    }

    /// The Arrow type the schema announces. Batches use it unless a value
    /// forces text, or (for unconstrained numeric) they need another scale.
    pub fn data_type(self) -> DataType {
        match self {
            Self::Bool => DataType::Boolean,
            Self::Int16 => DataType::Int16,
            Self::Int32 => DataType::Int32,
            Self::Int64 => DataType::Int64,
            Self::Oid => DataType::UInt32,
            Self::Float32 => DataType::Float32,
            Self::Float64 => DataType::Float64,
            Self::Decimal { precision, scale } => DataType::Decimal128(precision, scale),
            Self::Numeric => DataType::Decimal128(MAX_DECIMAL_DIGITS as u8, 0),
            Self::Date => DataType::Date32,
            Self::Time => DataType::Time64(TimeUnit::Microsecond),
            Self::Timestamp => DataType::Timestamp(TimeUnit::Microsecond, None),
            Self::TimestampTz => DataType::Timestamp(TimeUnit::Microsecond, Some(UTC.into())),
            Self::Interval => DataType::Interval(IntervalUnit::MonthDayNano),
            Self::Bytea => DataType::Binary,
            Self::Text => DataType::Utf8,
        }
    }
}

/// `(precision, scale)` from a numeric type modifier; `None` when the
/// column is unconstrained. Uses the 15+ encoding, which reads older
/// modifiers the same since their scale is never negative.
pub fn numeric_typmod(typmod: i32) -> Option<(u16, i16)> {
    let t = typmod.checked_sub(4).filter(|_| typmod >= 4)?;
    let precision = ((t >> 16) & 0xffff) as u16;
    let scale = (((t & 0x7ff) ^ 1024) - 1024) as i16;
    Some((precision, scale))
}

/// The type's name as PostgreSQL users know it, e.g. `numeric(10,2)`,
/// `varchar(20)`, `timestamptz`, `jsonb`, `int4[]`, or an enum's name.
pub fn native_name(ty: &Type, typmod: i32) -> String {
    if let Kind::Array(element) = ty.kind() {
        return format!("{}[]", native_name(element, typmod));
    }
    let name = ty.name();
    match *ty {
        Type::NUMERIC => match numeric_typmod(typmod) {
            Some((p, s)) => format!("numeric({p},{s})"),
            None => name.into(),
        },
        Type::BPCHAR if typmod >= 4 => format!("char({})", typmod - 4),
        Type::BPCHAR => "bpchar".into(),
        Type::VARCHAR if typmod >= 4 => format!("varchar({})", typmod - 4),
        Type::BIT
        | Type::VARBIT
        | Type::TIME
        | Type::TIMETZ
        | Type::TIMESTAMP
        | Type::TIMESTAMPTZ
            if typmod >= 0 =>
        {
            format!("{name}({typmod})")
        }
        // The single-byte `"char"`, quoted as format_type() does.
        Type::CHAR => "\"char\"".into(),
        _ => name.into(),
    }
}

/// Build one column of a batch, falling back to text when a value does not
/// fit the kind's Arrow type.
pub fn column(kind: ColumnKind, values: &[Option<&str>]) -> ArrayRef {
    typed(kind, values).unwrap_or_else(|| Arc::new(StringArray::from(values.to_vec())))
}

fn typed(kind: ColumnKind, values: &[Option<&str>]) -> Option<ArrayRef> {
    Some(match kind {
        ColumnKind::Text => return None,
        ColumnKind::Bool => Arc::new(BooleanArray::from(parse_all(values, parse_bool)?)),
        ColumnKind::Int16 => Arc::new(Int16Array::from(parse_all(values, parse_int)?)),
        ColumnKind::Int32 => Arc::new(Int32Array::from(parse_all(values, parse_int)?)),
        ColumnKind::Int64 => Arc::new(Int64Array::from(parse_all(values, parse_int)?)),
        ColumnKind::Oid => Arc::new(UInt32Array::from(parse_all(values, parse_int)?)),
        ColumnKind::Float32 => Arc::new(Float32Array::from(parse_all(values, parse_float)?)),
        ColumnKind::Float64 => Arc::new(Float64Array::from(parse_all(values, parse_float)?)),
        ColumnKind::Decimal { precision, scale } => Arc::new(
            Decimal128Array::from(parse_all(values, |s| parse_decimal(s, scale))?)
                .with_precision_and_scale(precision, scale)
                .ok()?,
        ),
        ColumnKind::Numeric => {
            let scale = common_scale(values)?;
            Arc::new(
                Decimal128Array::from(parse_all(values, |s| parse_decimal(s, scale))?)
                    .with_precision_and_scale(MAX_DECIMAL_DIGITS as u8, scale)
                    .ok()?,
            )
        }
        ColumnKind::Date => Arc::new(Date32Array::from(parse_all(values, parse_date)?)),
        ColumnKind::Time => Arc::new(Time64MicrosecondArray::from(parse_all(values, parse_time)?)),
        ColumnKind::Timestamp => {
            Arc::new(TimestampMicrosecondArray::from(parse_all(values, |s| {
                parse_timestamp(s, false)
            })?))
        }
        ColumnKind::TimestampTz => Arc::new(
            TimestampMicrosecondArray::from(parse_all(values, |s| parse_timestamp(s, true))?)
                .with_timezone(UTC),
        ),
        ColumnKind::Interval => Arc::new(IntervalMonthDayNanoArray::from(parse_all(
            values,
            parse_interval,
        )?)),
        ColumnKind::Bytea => {
            let bytes = parse_all(values, parse_bytea)?;
            Arc::new(BinaryArray::from_iter(bytes))
        }
    })
}

/// Parse every non-null value, or give up on the first that does not parse.
fn parse_all<T>(
    values: &[Option<&str>],
    parse: impl Fn(&str) -> Option<T>,
) -> Option<Vec<Option<T>>> {
    values
        .iter()
        .map(|value| match value {
            None => Some(None),
            Some(text) => parse(text).map(Some),
        })
        .collect()
}

fn parse_bool(text: &str) -> Option<bool> {
    match text {
        "t" => Some(true),
        "f" => Some(false),
        _ => None,
    }
}

fn parse_int<T: std::str::FromStr>(text: &str) -> Option<T> {
    text.parse().ok()
}

fn parse_float<T: std::str::FromStr + From<f32>>(text: &str) -> Option<T> {
    match text {
        "NaN" => Some(T::from(f32::NAN)),
        "Infinity" => Some(T::from(f32::INFINITY)),
        "-Infinity" => Some(T::from(f32::NEG_INFINITY)),
        _ => text.parse().ok(),
    }
}

/// A plain decimal's parts: sign, integer digits, fraction digits.
fn decimal_parts(text: &str) -> Option<(bool, &str, &str)> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
    let all_digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    (!int.is_empty() && all_digits(int) && all_digits(frac)).then_some((negative, int, frac))
}

/// The value scaled to an integer at `scale` fraction digits, or `None` if
/// it has more digits than that, is `NaN` or `Infinity`, or does not fit.
pub fn parse_decimal(text: &str, scale: i8) -> Option<i128> {
    let (negative, int, frac) = decimal_parts(text)?;
    let int = int.trim_start_matches('0');
    let mut digits = String::with_capacity(int.len() + frac.len().max(scale.max(0) as usize));
    digits.push_str(int);
    if scale >= 0 {
        let scale = scale as usize;
        if frac.len() > scale {
            return None;
        }
        digits.push_str(frac);
        digits.extend(std::iter::repeat_n('0', scale - frac.len()));
    } else {
        // A negative scale: the value is a multiple of 10^-scale.
        if frac.bytes().any(|b| b != b'0') {
            return None;
        }
        let drop = (-scale) as usize;
        let kept = digits.len().checked_sub(drop);
        match kept {
            Some(kept) if digits[kept..].bytes().all(|b| b == b'0') => digits.truncate(kept),
            _ if digits.is_empty() => {}
            _ => return None,
        }
    }
    let digits = digits.trim_start_matches('0');
    if digits.len() > MAX_DECIMAL_DIGITS {
        return None;
    }
    let value: i128 = if digits.is_empty() {
        0
    } else {
        digits.parse().ok()?
    };
    Some(if negative { -value } else { value })
}

/// The scale an unconstrained numeric batch needs: its longest fraction,
/// if every value then still fits 38 digits.
fn common_scale(values: &[Option<&str>]) -> Option<i8> {
    let (mut int_digits, mut scale) = (0, 0);
    for text in values.iter().flatten() {
        let (_, int, frac) = decimal_parts(text)?;
        int_digits = int_digits.max(int.trim_start_matches('0').len());
        scale = scale.max(frac.len());
    }
    (int_digits + scale <= MAX_DECIMAL_DIGITS).then_some(scale as i8)
}

fn strip_bc(text: &str) -> (&str, bool) {
    match text.strip_suffix(" BC") {
        Some(rest) => (rest, true),
        None => (text, false),
    }
}

fn digits<T: std::str::FromStr>(text: &str) -> Option<T> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Days since 1970-01-01 in the proleptic Gregorian calendar, which
/// PostgreSQL uses. `year` is astronomical: 1 BC is year 0.
fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    let leap = year.rem_euclid(4) == 0 && (year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1..=12).contains(&month) || day == 0 || day > month_days[month as usize - 1] {
        return None;
    }
    // Howard Hinnant's days_from_civil.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// `YYYY-MM-DD`, years past 9999 and ` BC` included.
fn parse_days(text: &str, bc: bool) -> Option<i64> {
    let mut parts = text.splitn(3, '-');
    let year: i64 = digits(parts.next()?)?;
    let month = digits(parts.next()?)?;
    let day = digits(parts.next()?)?;
    let year = if bc { 1 - year } else { year };
    days_from_civil(year, month, day)
}

pub fn parse_date(text: &str) -> Option<i32> {
    let (text, bc) = strip_bc(text);
    parse_days(text, bc)?.try_into().ok()
}

/// `HH:MM:SS[.ffffff]` as microseconds. PostgreSQL's `time` also accepts
/// `24:00:00`, which Arrow's cannot hold, so that falls back to text.
fn parse_clock(text: &str) -> Option<i64> {
    let mut parts = text.splitn(3, ':');
    let hours: i64 = digits(parts.next()?)?;
    let minutes: i64 = digits(parts.next()?)?;
    let (seconds, micros) = split_seconds(parts.next()?)?;
    if hours > 23 || minutes > 59 || seconds > 60 {
        return None;
    }
    Some(((hours * 60 + minutes) * 60 + seconds) * 1_000_000 + micros)
}

/// `SS[.ffffff]` as whole seconds and microseconds.
fn split_seconds(text: &str) -> Option<(i64, i64)> {
    let (whole, frac) = text.split_once('.').unwrap_or((text, ""));
    let seconds = digits(whole)?;
    if frac.len() > 6 || (!frac.is_empty() && digits::<u32>(frac).is_none()) {
        return None;
    }
    let micros = if frac.is_empty() {
        0
    } else {
        format!("{frac:0<6}").parse().ok()?
    };
    Some((seconds, micros))
}

pub fn parse_time(text: &str) -> Option<i64> {
    parse_clock(text)
}

/// `YYYY-MM-DD HH:MM:SS[.ffffff][+HH[:MM[:SS]]][ BC]` as microseconds since
/// the epoch, in UTC when `with_zone`.
pub fn parse_timestamp(text: &str, with_zone: bool) -> Option<i64> {
    let (text, bc) = strip_bc(text);
    let (date, time) = text.split_once(' ')?;
    let (clock, offset) = if with_zone {
        let at = time.find(['+', '-'])?;
        let (clock, zone) = time.split_at(at);
        (clock, parse_offset(zone)?)
    } else {
        (time, 0)
    };
    let days = parse_days(date, bc)?;
    let micros = parse_clock(clock)?;
    days.checked_mul(86_400_000_000)?
        .checked_add(micros)?
        .checked_sub(offset.checked_mul(1_000_000)?)
}

/// `+HH[:MM[:SS]]` as seconds east of UTC.
fn parse_offset(text: &str) -> Option<i64> {
    let (sign, rest) = match text.as_bytes().first()? {
        b'+' => (1, &text[1..]),
        b'-' => (-1, &text[1..]),
        _ => return None,
    };
    let mut seconds = 0;
    let mut scale = 3600;
    for part in rest.split(':') {
        if scale == 0 {
            return None;
        }
        seconds += digits::<i64>(part)? * scale;
        scale /= 60;
    }
    Some(sign * seconds)
}

/// The default `IntervalStyle` (`postgres`): `1 year 2 mons -3 days
/// 04:05:06.789`. Other styles fall back to text.
pub fn parse_interval(text: &str) -> Option<IntervalMonthDayNano> {
    let (mut months, mut days, mut nanos) = (0i32, 0i32, 0i64);
    let mut words = text.split(' ');
    while let Some(word) = words.next() {
        if word.contains(':') {
            let (negative, clock) = match word.as_bytes().first()? {
                b'-' => (true, &word[1..]),
                b'+' => (false, &word[1..]),
                _ => (false, word),
            };
            let mut parts = clock.splitn(3, ':');
            let hours: i64 = digits(parts.next()?)?;
            let minutes: i64 = digits(parts.next()?)?;
            let (seconds, micros) = split_seconds(parts.next()?)?;
            let total = ((hours * 60 + minutes) * 60 + seconds)
                .checked_mul(1_000_000_000)?
                .checked_add(micros * 1000)?;
            nanos = nanos.checked_add(if negative { -total } else { total })?;
            continue;
        }
        let count: i32 = word.parse().ok()?;
        match words.next()? {
            "year" | "years" => months = months.checked_add(count.checked_mul(12)?)?,
            "mon" | "mons" => months = months.checked_add(count)?,
            "day" | "days" => days = days.checked_add(count)?,
            _ => return None,
        }
    }
    Some(IntervalMonthDayNano::new(months, days, nanos))
}

/// `bytea_output = hex` (`\x0aff`) or `escape` (`a\\b\001`).
pub fn parse_bytea(text: &str) -> Option<Vec<u8>> {
    if let Some(hex) = text.strip_prefix("\\x") {
        if hex.len() % 2 != 0 {
            return None;
        }
        return hex
            .as_bytes()
            .chunks(2)
            .map(|pair| {
                let hi = (pair[0] as char).to_digit(16)?;
                let lo = (pair[1] as char).to_digit(16)?;
                Some((hi * 16 + lo) as u8)
            })
            .collect();
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
        } else if bytes.get(i + 1) == Some(&b'\\') {
            out.push(b'\\');
            i += 2;
        } else {
            let octal = bytes.get(i + 1..i + 4)?;
            if !octal.iter().all(|b| (b'0'..=b'7').contains(b)) {
                return None;
            }
            let value = octal
                .iter()
                .fold(0u32, |acc, b| acc * 8 + u32::from(b - b'0'));
            out.push(u8::try_from(value).ok()?);
            i += 4;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use arrow::array::Array;
    use arrow::util::display::{ArrayFormatter, FormatOptions};

    use super::*;

    fn shown(array: &ArrayRef) -> Vec<String> {
        let formatter = ArrayFormatter::try_new(array.as_ref(), &FormatOptions::default()).unwrap();
        (0..array.len())
            .map(|i| {
                if array.is_null(i) {
                    "NULL".into()
                } else {
                    formatter.value(i).to_string()
                }
            })
            .collect()
    }

    #[test]
    fn maps_types_to_kinds() {
        assert_eq!(ColumnKind::of(&Type::INT4, -1), ColumnKind::Int32);
        assert_eq!(ColumnKind::of(&Type::JSONB, -1), ColumnKind::Text);
        assert_eq!(ColumnKind::of(&Type::NUMERIC, -1), ColumnKind::Numeric);
        // numeric(10,2): ((10 << 16) | 2) + 4
        let typmod = ((10 << 16) | 2) + 4;
        assert_eq!(
            ColumnKind::of(&Type::NUMERIC, typmod),
            ColumnKind::Decimal {
                precision: 10,
                scale: 2
            }
        );
        assert_eq!(native_name(&Type::NUMERIC, typmod), "numeric(10,2)");
        // numeric(50,2) does not fit Decimal128.
        assert_eq!(
            ColumnKind::of(&Type::NUMERIC, ((50 << 16) | 2) + 4),
            ColumnKind::Text
        );
        // numeric(5,-2), PostgreSQL 15+: the scale is an 11-bit signed field.
        let negative = ((5 << 16) | (-2i32 & 0x7ff)) + 4;
        assert_eq!(numeric_typmod(negative), Some((5, -2)));
        assert_eq!(native_name(&Type::NUMERIC, negative), "numeric(5,-2)");
        // numeric(3,5): more scale than precision.
        assert_eq!(
            ColumnKind::of(&Type::NUMERIC, ((3 << 16) | 5) + 4),
            ColumnKind::Decimal {
                precision: 5,
                scale: 5
            }
        );
    }

    #[test]
    fn names_types_like_postgres() {
        assert_eq!(native_name(&Type::TIMESTAMPTZ, -1), "timestamptz");
        assert_eq!(native_name(&Type::TIMESTAMPTZ, 3), "timestamptz(3)");
        assert_eq!(native_name(&Type::VARCHAR, 24), "varchar(20)");
        assert_eq!(native_name(&Type::BPCHAR, 7), "char(3)");
        assert_eq!(native_name(&Type::INT4_ARRAY, -1), "int4[]");
        assert_eq!(native_name(&Type::VARCHAR_ARRAY, 14), "varchar(10)[]");
        assert_eq!(native_name(&Type::UUID, -1), "uuid");
        assert_eq!(native_name(&Type::CHAR, -1), "\"char\"");
    }

    #[test]
    fn parses_scalars() {
        let bools = column(ColumnKind::Bool, &[Some("t"), None, Some("f")]);
        assert_eq!(shown(&bools), ["true", "NULL", "false"]);
        let ints = column(ColumnKind::Int64, &[Some("-9223372036854775808"), None]);
        assert_eq!(ints.data_type(), &DataType::Int64);
        assert_eq!(shown(&ints), ["-9223372036854775808", "NULL"]);
        let floats = column(
            ColumnKind::Float64,
            &[Some("0.1"), Some("NaN"), Some("-Infinity"), Some("1e+300")],
        );
        assert_eq!(shown(&floats), ["0.1", "NaN", "-inf", "1e300"]);
        let oids = column(ColumnKind::Oid, &[Some("4294967295")]);
        assert_eq!(oids.data_type(), &DataType::UInt32);
    }

    #[test]
    fn parses_decimals() {
        assert_eq!(parse_decimal("123.45", 2), Some(12345));
        assert_eq!(parse_decimal("-0.5", 3), Some(-500));
        assert_eq!(parse_decimal("7", 2), Some(700));
        assert_eq!(parse_decimal("1.234", 2), None);
        assert_eq!(parse_decimal("NaN", 2), None);
        assert_eq!(parse_decimal("Infinity", 0), None);
        assert_eq!(parse_decimal("12300", -2), Some(123));
        assert_eq!(parse_decimal("0", -2), Some(0));
        assert_eq!(parse_decimal("12345", -2), None);

        let fixed = column(
            ColumnKind::Decimal {
                precision: 10,
                scale: 2,
            },
            &[Some("123.45"), None, Some("-0.10")],
        );
        assert_eq!(fixed.data_type(), &DataType::Decimal128(10, 2));
        assert_eq!(shown(&fixed), ["123.45", "NULL", "-0.10"]);

        // Unconstrained numeric takes the scale the batch needs.
        let free = column(
            ColumnKind::Numeric,
            &[Some("1.5"), Some("2.25"), Some("-3")],
        );
        assert_eq!(free.data_type(), &DataType::Decimal128(38, 2));
        assert_eq!(shown(&free), ["1.50", "2.25", "-3.00"]);
        // ...and falls back to text for values Arrow cannot hold.
        let special = column(ColumnKind::Numeric, &[Some("1.5"), Some("NaN")]);
        assert_eq!(special.data_type(), &DataType::Utf8);
        assert_eq!(shown(&special), ["1.5", "NaN"]);
        let huge = "9".repeat(40);
        let wide = column(ColumnKind::Numeric, &[Some(&huge)]);
        assert_eq!(wide.data_type(), &DataType::Utf8);
    }

    #[test]
    fn parses_dates_and_times() {
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("2024-02-29"), Some(19782));
        assert_eq!(parse_date("2023-02-29"), None);
        assert_eq!(parse_date("infinity"), None);
        // 1 BC is astronomical year 0, a leap year.
        assert_eq!(
            parse_date("0001-02-29 BC"),
            parse_days("0000-02-29", false).map(|d| d as i32)
        );
        assert_eq!(parse_date("12345-01-01"), Some(3_789_391));

        assert_eq!(parse_time("00:00:00"), Some(0));
        assert_eq!(parse_time("12:34:56.789"), Some(45_296_789_000));
        assert_eq!(parse_time("24:00:00"), None);

        let ts = parse_timestamp("2024-01-31 12:34:56.123456", false).unwrap();
        assert_eq!(ts, 1_706_704_496_123_456);
        // The same instant written in Seoul and in India.
        assert_eq!(
            parse_timestamp("2024-01-31 21:34:56.123456+09", true),
            Some(ts)
        );
        assert_eq!(
            parse_timestamp("2024-01-31 18:04:56.123456+05:30", true),
            Some(ts)
        );
        assert_eq!(
            parse_timestamp("2024-01-31 12:34:56.123456-00", true),
            Some(ts)
        );
        // Local mean time offsets carry seconds.
        assert_eq!(
            parse_timestamp("1900-01-01 00:00:00+08:05:43", true),
            parse_timestamp("1899-12-31 15:54:17", false)
        );
        assert_eq!(parse_timestamp("infinity", true), None);

        let column = column(
            ColumnKind::TimestampTz,
            &[Some("2024-01-31 21:34:56.5+09"), None],
        );
        assert_eq!(shown(&column), ["2024-01-31T12:34:56.500Z", "NULL"]);
        let dates = super::column(ColumnKind::Date, &[Some("2024-01-31"), Some("infinity")]);
        assert_eq!(dates.data_type(), &DataType::Utf8);
    }

    #[test]
    fn parses_intervals() {
        let hour = 3_600_000_000_000;
        assert_eq!(
            parse_interval("1 year 2 mons 3 days 04:05:06.5"),
            Some(IntervalMonthDayNano::new(
                14,
                3,
                4 * hour + 5 * 60_000_000_000 + 6_500_000_000
            ))
        );
        assert_eq!(
            parse_interval("-1 days +02:00:00"),
            Some(IntervalMonthDayNano::new(0, -1, 2 * hour))
        );
        assert_eq!(
            parse_interval("-00:00:01"),
            Some(IntervalMonthDayNano::new(0, 0, -1_000_000_000))
        );
        assert_eq!(
            parse_interval("100:00:00"),
            Some(IntervalMonthDayNano::new(0, 0, 100 * hour))
        );
        assert_eq!(parse_interval("P1Y2M"), None);
        assert_eq!(parse_interval("@ 1 year"), None);
    }

    #[test]
    fn parses_bytea() {
        assert_eq!(parse_bytea("\\x00ff10"), Some(vec![0, 255, 16]));
        assert_eq!(parse_bytea("\\x"), Some(vec![]));
        assert_eq!(parse_bytea("\\x0"), None);
        assert_eq!(parse_bytea("ab\\\\c\\001"), Some(b"ab\\c\x01".to_vec()));
        let array = column(ColumnKind::Bytea, &[Some("\\xdead"), None]);
        assert_eq!(array.data_type(), &DataType::Binary);
    }
}
