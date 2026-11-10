//! Text-protocol values to typed Arrow arrays.
//!
//! The text protocol sends every value as bytes, so each column is parsed
//! by the type its metadata declares. A value that does not parse as that
//! type (MySQL's zero date `0000-00-00`, a date with a zero month allowed
//! by `ALLOW_INVALID_DATES`) never fails the query: that batch's column is
//! built as text instead, holding exactly what the server sent.
//! `quarry_core::ResultSet` formats cells per batch, so this displays right
//! even though the schema announced the typed column.

use std::sync::Arc;

use arrow::array::{
    ArrayRef, BinaryBuilder, Date32Builder, Decimal128Builder, Float32Builder, Float64Builder,
    Int8Builder, Int16Builder, Int32Builder, Int64Builder, StringBuilder,
    TimestampMicrosecondBuilder, UInt8Builder, UInt16Builder, UInt32Builder, UInt64Builder,
};
use arrow::datatypes::{DataType, Field, TimeUnit};
use mysql::consts::{ColumnFlags, ColumnType};
use quarry_core::result::native_type;

/// The `binary` character set: bytes, not text.
pub const BINARY_CHARSET: u16 = 63;

/// The Arrow representation picked for a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Int8,
    Int16,
    Int32,
    Int64,
    UInt8,
    UInt16,
    UInt32,
    UInt64,
    Float32,
    Float64,
    Decimal {
        precision: u8,
        scale: i8,
    },
    Date,
    DateTime,
    Text,
    Binary,
    /// `BIT(n)`, sent as big-endian bytes.
    Bit,
    /// Spatial values, sent as a 4-byte SRID and WKB; shown as WKT.
    Geometry,
}

impl Kind {
    pub fn data_type(self) -> DataType {
        match self {
            Kind::Int8 => DataType::Int8,
            Kind::Int16 => DataType::Int16,
            Kind::Int32 => DataType::Int32,
            Kind::Int64 => DataType::Int64,
            Kind::UInt8 => DataType::UInt8,
            Kind::UInt16 => DataType::UInt16,
            Kind::UInt32 => DataType::UInt32,
            Kind::UInt64 | Kind::Bit => DataType::UInt64,
            Kind::Float32 => DataType::Float32,
            Kind::Float64 => DataType::Float64,
            Kind::Decimal { precision, scale } => DataType::Decimal128(precision, scale),
            Kind::Date => DataType::Date32,
            Kind::DateTime => DataType::Timestamp(TimeUnit::Microsecond, None),
            Kind::Text | Kind::Geometry => DataType::Utf8,
            Kind::Binary => DataType::Binary,
        }
    }
}

/// What the driver knows about a result column: its wire metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMeta {
    pub name: String,
    pub column_type: ColumnType,
    pub flags: ColumnFlags,
    pub charset: u16,
    pub length: u32,
    pub decimals: u8,
    /// The engine's type name when known better than the wire says (a
    /// MariaDB `json` column read from the catalog).
    pub type_override: Option<String>,
}

impl ColumnMeta {
    pub fn from_column(column: &mysql::Column) -> Self {
        Self {
            name: column.name_str().into_owned(),
            column_type: column.column_type(),
            flags: column.flags(),
            charset: column.character_set(),
            length: column.column_length(),
            decimals: column.decimals(),
            type_override: None,
        }
    }

    #[cfg(test)]
    pub fn new(column_type: ColumnType) -> Self {
        Self {
            name: "c".into(),
            column_type,
            flags: ColumnFlags::empty(),
            charset: 255,
            length: 0,
            decimals: 0,
            type_override: None,
        }
    }

    fn unsigned(&self) -> bool {
        self.flags.contains(ColumnFlags::UNSIGNED_FLAG)
    }

    fn binary(&self) -> bool {
        self.charset == BINARY_CHARSET
    }

    /// Fractional-second digits of a temporal column, when it has any.
    fn fsp(&self) -> Option<u8> {
        (1..=6).contains(&self.decimals).then_some(self.decimals)
    }

    /// `DECIMAL(p,s)`: the wire length counts a sign and a point.
    fn decimal_precision(&self) -> u32 {
        let overhead = u32::from(self.decimals > 0) + u32::from(!self.unsigned());
        self.length.saturating_sub(overhead).max(1)
    }

    pub fn kind(&self) -> Kind {
        use ColumnType::*;
        let unsigned = self.unsigned();
        match self.column_type {
            MYSQL_TYPE_TINY if unsigned => Kind::UInt8,
            MYSQL_TYPE_TINY => Kind::Int8,
            MYSQL_TYPE_SHORT if unsigned => Kind::UInt16,
            MYSQL_TYPE_SHORT => Kind::Int16,
            MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG if unsigned => Kind::UInt32,
            MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG => Kind::Int32,
            MYSQL_TYPE_LONGLONG if unsigned => Kind::UInt64,
            MYSQL_TYPE_LONGLONG => Kind::Int64,
            MYSQL_TYPE_FLOAT => Kind::Float32,
            MYSQL_TYPE_DOUBLE => Kind::Float64,
            MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => {
                let precision = self.decimal_precision();
                if precision <= 38 && self.decimals <= 30 {
                    Kind::Decimal {
                        precision: precision as u8,
                        scale: self.decimals as i8,
                    }
                } else {
                    Kind::Text
                }
            }
            MYSQL_TYPE_YEAR => Kind::UInt16,
            MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => Kind::Date,
            MYSQL_TYPE_DATETIME
            | MYSQL_TYPE_DATETIME2
            | MYSQL_TYPE_TIMESTAMP
            | MYSQL_TYPE_TIMESTAMP2 => Kind::DateTime,
            // TIME spans -838:59:59 to 838:59:59, beyond any time of day.
            MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => Kind::Text,
            MYSQL_TYPE_BIT => Kind::Bit,
            MYSQL_TYPE_GEOMETRY => Kind::Geometry,
            MYSQL_TYPE_JSON | MYSQL_TYPE_ENUM | MYSQL_TYPE_SET | MYSQL_TYPE_NULL => Kind::Text,
            MYSQL_TYPE_TINY_BLOB
            | MYSQL_TYPE_MEDIUM_BLOB
            | MYSQL_TYPE_LONG_BLOB
            | MYSQL_TYPE_BLOB
            | MYSQL_TYPE_VAR_STRING
            | MYSQL_TYPE_VARCHAR
            | MYSQL_TYPE_STRING
            | MYSQL_TYPE_VECTOR
                if self.binary() && !self.is_enum_or_set() =>
            {
                Kind::Binary
            }
            _ => Kind::Text,
        }
    }

    fn is_enum_or_set(&self) -> bool {
        self.flags
            .intersects(ColumnFlags::ENUM_FLAG | ColumnFlags::SET_FLAG)
    }

    /// The engine's name for the column's type, as result headers show it.
    pub fn native_type(&self) -> String {
        use ColumnType::*;
        if let Some(name) = &self.type_override {
            return name.clone();
        }
        let unsigned = if self.unsigned() { " unsigned" } else { "" };
        let with_fsp = |name: &str| match self.fsp() {
            Some(fsp) => format!("{name}({fsp})"),
            None => name.to_owned(),
        };
        match self.column_type {
            // BOOL is TINYINT(1); keep the width so it reads as a flag.
            MYSQL_TYPE_TINY if self.length == 1 => format!("tinyint(1){unsigned}"),
            MYSQL_TYPE_TINY => format!("tinyint{unsigned}"),
            MYSQL_TYPE_SHORT => format!("smallint{unsigned}"),
            MYSQL_TYPE_INT24 => format!("mediumint{unsigned}"),
            MYSQL_TYPE_LONG => format!("int{unsigned}"),
            MYSQL_TYPE_LONGLONG => format!("bigint{unsigned}"),
            MYSQL_TYPE_FLOAT => format!("float{unsigned}"),
            MYSQL_TYPE_DOUBLE => format!("double{unsigned}"),
            MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => format!(
                "decimal({},{}){unsigned}",
                self.decimal_precision(),
                self.decimals
            ),
            MYSQL_TYPE_NULL => "null".into(),
            MYSQL_TYPE_YEAR => "year".into(),
            MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "date".into(),
            MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => with_fsp("time"),
            MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => with_fsp("datetime"),
            MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => with_fsp("timestamp"),
            MYSQL_TYPE_BIT => format!("bit({})", self.length.max(1)),
            MYSQL_TYPE_JSON => "json".into(),
            MYSQL_TYPE_GEOMETRY => "geometry".into(),
            MYSQL_TYPE_VECTOR => "vector".into(),
            MYSQL_TYPE_ENUM => "enum".into(),
            MYSQL_TYPE_SET => "set".into(),
            _ if self.flags.contains(ColumnFlags::ENUM_FLAG) => "enum".into(),
            _ if self.flags.contains(ColumnFlags::SET_FLAG) => "set".into(),
            MYSQL_TYPE_TINY_BLOB
            | MYSQL_TYPE_MEDIUM_BLOB
            | MYSQL_TYPE_LONG_BLOB
            | MYSQL_TYPE_BLOB => {
                // Servers send all four as BLOB and tell them apart by the
                // byte length, which for text is chars × bytes per char (up
                // to 4), so the bands below hold for every character set.
                let size = match self.length {
                    0..=1020 => "tiny",
                    1021..=262_140 => "",
                    262_141..=67_108_860 => "medium",
                    _ => "long",
                };
                let base = if self.binary() { "blob" } else { "text" };
                format!("{size}{base}")
            }
            MYSQL_TYPE_STRING if self.binary() => "binary".into(),
            MYSQL_TYPE_STRING => "char".into(),
            _ if self.binary() => "varbinary".into(),
            _ => "varchar".into(),
        }
    }

    pub fn field(&self) -> Field {
        native_type(
            Field::new(self.name.clone(), self.kind().data_type(), true),
            self.native_type(),
        )
    }
}

/// One column's values for the batch being collected, as the server sent
/// them.
pub type RawColumn = Vec<Option<Vec<u8>>>;

/// Build a column of `kind`, or a text column when a value does not parse.
pub fn build_array(kind: Kind, values: &[Option<Vec<u8>>]) -> ArrayRef {
    try_build(kind, values).unwrap_or_else(|| text_array(values))
}

fn text_array(values: &[Option<Vec<u8>>]) -> ArrayRef {
    let mut builder = StringBuilder::with_capacity(values.len(), 0);
    for value in values {
        builder.append_option(value.as_deref().map(String::from_utf8_lossy));
    }
    Arc::new(builder.finish())
}

fn text(value: &[u8]) -> Option<&str> {
    std::str::from_utf8(value).ok()
}

macro_rules! parsed {
    ($builder:ty, $values:expr, $parse:expr) => {{
        let mut builder = <$builder>::with_capacity($values.len());
        for value in $values {
            match value {
                None => builder.append_null(),
                Some(bytes) => builder.append_value($parse(bytes.as_slice())?),
            }
        }
        Arc::new(builder.finish()) as ArrayRef
    }};
}

fn number<T: std::str::FromStr>(bytes: &[u8]) -> Option<T> {
    text(bytes)?.trim().parse().ok()
}

fn try_build(kind: Kind, values: &[Option<Vec<u8>>]) -> Option<ArrayRef> {
    Some(match kind {
        Kind::Int8 => parsed!(Int8Builder, values, number::<i8>),
        Kind::Int16 => parsed!(Int16Builder, values, number::<i16>),
        Kind::Int32 => parsed!(Int32Builder, values, number::<i32>),
        Kind::Int64 => parsed!(Int64Builder, values, number::<i64>),
        Kind::UInt8 => parsed!(UInt8Builder, values, number::<u8>),
        Kind::UInt16 => parsed!(UInt16Builder, values, number::<u16>),
        Kind::UInt32 => parsed!(UInt32Builder, values, number::<u32>),
        Kind::UInt64 => parsed!(UInt64Builder, values, number::<u64>),
        Kind::Float32 => parsed!(Float32Builder, values, number::<f32>),
        Kind::Float64 => parsed!(Float64Builder, values, number::<f64>),
        Kind::Bit => parsed!(UInt64Builder, values, |b: &[u8]| parse_bit(b)),
        Kind::Date => parsed!(Date32Builder, values, |b: &[u8]| parse_date(text(b)?)),
        Kind::DateTime => {
            parsed!(TimestampMicrosecondBuilder, values, |b: &[u8]| {
                parse_datetime(text(b)?)
            })
        }
        Kind::Decimal { precision, scale } => {
            let mut builder = Decimal128Builder::with_capacity(values.len());
            for value in values {
                match value {
                    None => builder.append_null(),
                    Some(bytes) => {
                        builder.append_value(parse_decimal(text(bytes)?, precision, scale)?)
                    }
                }
            }
            Arc::new(
                builder
                    .finish()
                    .with_precision_and_scale(precision, scale)
                    .ok()?,
            )
        }
        Kind::Binary => {
            let mut builder = BinaryBuilder::with_capacity(values.len(), 0);
            for value in values {
                builder.append_option(value.as_deref());
            }
            Arc::new(builder.finish())
        }
        Kind::Geometry => {
            let mut builder = StringBuilder::with_capacity(values.len(), 0);
            for value in values {
                builder.append_option(value.as_deref().map(geometry_text));
            }
            Arc::new(builder.finish())
        }
        Kind::Text => text_array(values),
    })
}

/// `BIT(n)` values arrive as big-endian bytes, at most 8.
pub fn parse_bit(bytes: &[u8]) -> Option<u64> {
    if bytes.len() > 8 {
        return None;
    }
    Some(bytes.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b)))
}

/// A decimal in text, scaled to `scale` digits, within `precision`.
pub fn parse_decimal(text: &str, precision: u8, scale: i8) -> Option<i128> {
    let text = text.trim();
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    let scale = usize::try_from(scale).ok()?;
    if whole.is_empty() && fraction.is_empty()
        || fraction.len() > scale
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let significant = whole.trim_start_matches('0').len() + scale;
    if significant > usize::from(precision) {
        return None;
    }
    let mut value: i128 = 0;
    for b in whole.bytes().chain(fraction.bytes()) {
        value = value.checked_mul(10)?.checked_add(i128::from(b - b'0'))?;
    }
    for _ in fraction.len()..scale {
        value = value.checked_mul(10)?;
    }
    Some(if negative { -value } else { value })
}

/// Days from 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year =
        (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn fixed_digits(text: &str, len: usize) -> Option<u32> {
    (text.len() == len && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

/// `YYYY-MM-DD` as days since the epoch; `None` for zero or invalid dates
/// such as `0000-00-00` or `2024-02-30`.
pub fn parse_date(text: &str) -> Option<i32> {
    let mut parts = text.split('-');
    let year = fixed_digits(parts.next()?, 4)?;
    let month = fixed_digits(parts.next()?, 2)?;
    let day = fixed_digits(parts.next()?, 2)?;
    if parts.next().is_some()
        || year == 0
        || !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
    {
        return None;
    }
    i32::try_from(days_from_civil(i64::from(year), month, day)).ok()
}

/// `YYYY-MM-DD HH:MM:SS[.ffffff]` as microseconds since the epoch.
pub fn parse_datetime(text: &str) -> Option<i64> {
    let (date, time) = text.split_once(' ').unwrap_or((text, "00:00:00"));
    let days = i64::from(parse_date(date)?);
    let (clock, fraction) = time.split_once('.').unwrap_or((time, ""));
    let mut parts = clock.split(':');
    let hour = fixed_digits(parts.next()?, 2)?;
    let minute = fixed_digits(parts.next()?, 2)?;
    let second = fixed_digits(parts.next()?, 2)?;
    if parts.next().is_some() || hour > 23 || minute > 59 || second > 59 || fraction.len() > 6 {
        return None;
    }
    let micros = if fraction.is_empty() {
        0
    } else {
        let digits = fixed_digits(fraction, fraction.len())?;
        i64::from(digits) * 10i64.pow(6 - fraction.len() as u32)
    };
    let seconds = days * 86_400 + i64::from(hour * 3600 + minute * 60 + second);
    Some(seconds * 1_000_000 + micros)
}

/// A spatial value as WKT (`POINT(1 2)`), prefixed with its SRID when it
/// has one (`SRID=4326;POINT(1 2)`), or as hex when it does not decode.
pub fn geometry_text(bytes: &[u8]) -> String {
    geometry_wkt(bytes).unwrap_or_else(|| {
        let mut hex = String::with_capacity(2 + bytes.len() * 2);
        hex.push_str("0x");
        for b in bytes {
            hex.push_str(&format!("{b:02X}"));
        }
        hex
    })
}

fn geometry_wkt(bytes: &[u8]) -> Option<String> {
    let srid = u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?);
    let mut reader = Wkb {
        bytes: &bytes[4..],
        little: true,
    };
    let mut out = String::new();
    if srid != 0 {
        out.push_str(&format!("SRID={srid};"));
    }
    reader.geometry(&mut out)?;
    reader.bytes.is_empty().then_some(out)
}

struct Wkb<'a> {
    bytes: &'a [u8],
    little: bool,
}

impl Wkb<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, rest) = self.bytes.split_at_checked(N)?;
        self.bytes = rest;
        head.try_into().ok()
    }

    fn u32(&mut self) -> Option<u32> {
        let raw = self.take::<4>()?;
        Some(if self.little {
            u32::from_le_bytes(raw)
        } else {
            u32::from_be_bytes(raw)
        })
    }

    fn f64(&mut self) -> Option<f64> {
        let raw = self.take::<8>()?;
        Some(if self.little {
            f64::from_le_bytes(raw)
        } else {
            f64::from_be_bytes(raw)
        })
    }

    fn point(&mut self, out: &mut String) -> Option<()> {
        let (x, y) = (self.f64()?, self.f64()?);
        out.push_str(&format!("{x} {y}"));
        Some(())
    }

    fn list(
        &mut self,
        out: &mut String,
        mut item: impl FnMut(&mut Self, &mut String) -> Option<()>,
    ) -> Option<()> {
        let count = self.u32()?;
        // Each item takes at least 8 bytes; refuse counts the data cannot hold.
        if count as usize > self.bytes.len() / 8 + 1 {
            return None;
        }
        if count == 0 {
            out.push_str(" EMPTY");
            return Some(());
        }
        out.push('(');
        for n in 0..count {
            if n > 0 {
                out.push(',');
            }
            item(self, out)?;
        }
        out.push(')');
        Some(())
    }

    fn ring(&mut self, out: &mut String) -> Option<()> {
        self.list(out, |r, out| r.point(out))
    }

    fn polygon(&mut self, out: &mut String) -> Option<()> {
        self.list(out, |r, out| r.ring(out))
    }

    fn geometry(&mut self, out: &mut String) -> Option<()> {
        self.little = match self.take::<1>()?[0] {
            0 => false,
            1 => true,
            _ => return None,
        };
        let kind = self.u32()?;
        let name = match kind {
            1 => "POINT",
            2 => "LINESTRING",
            3 => "POLYGON",
            4 => "MULTIPOINT",
            5 => "MULTILINESTRING",
            6 => "MULTIPOLYGON",
            7 => "GEOMETRYCOLLECTION",
            _ => return None,
        };
        out.push_str(name);
        match kind {
            1 => {
                out.push('(');
                self.point(out)?;
                out.push(')');
                Some(())
            }
            2 => self.ring(out),
            3 => self.polygon(out),
            // Members of multi-geometries are whole WKB geometries; WKT
            // drops their type names except in collections.
            4 => self.list(out, |r, out| r.member(out, 1)),
            5 => self.list(out, |r, out| r.member(out, 2)),
            6 => self.list(out, |r, out| r.member(out, 3)),
            _ => self.list(out, |r, out| r.geometry(out)),
        }
    }

    fn member(&mut self, out: &mut String, expected: u32) -> Option<()> {
        self.little = match self.take::<1>()?[0] {
            0 => false,
            1 => true,
            _ => return None,
        };
        if self.u32()? != expected {
            return None;
        }
        match expected {
            1 => {
                out.push('(');
                self.point(out)?;
                out.push(')');
                Some(())
            }
            2 => self.ring(out),
            _ => self.polygon(out),
        }
    }
}

#[cfg(test)]
mod tests {
    use arrow::array::{Array, AsArray};
    use arrow::datatypes::{Date32Type, Decimal128Type, Int64Type, TimestampMicrosecondType};

    use super::*;

    fn col(column_type: ColumnType) -> ColumnMeta {
        ColumnMeta::new(column_type)
    }

    fn raw(values: &[Option<&str>]) -> RawColumn {
        values
            .iter()
            .map(|v| v.map(|s| s.as_bytes().to_vec()))
            .collect()
    }

    #[test]
    fn names_native_types() {
        use ColumnType::*;
        let mut c = col(MYSQL_TYPE_LONGLONG);
        assert_eq!(c.native_type(), "bigint");
        c.flags = ColumnFlags::UNSIGNED_FLAG;
        assert_eq!(c.native_type(), "bigint unsigned");
        assert_eq!(c.kind(), Kind::UInt64);

        let mut tiny = col(MYSQL_TYPE_TINY);
        tiny.length = 1;
        assert_eq!(tiny.native_type(), "tinyint(1)");
        assert_eq!(tiny.kind(), Kind::Int8);

        // DECIMAL(10,2): 10 digits + point + sign.
        let mut dec = col(MYSQL_TYPE_NEWDECIMAL);
        dec.length = 12;
        dec.decimals = 2;
        assert_eq!(dec.native_type(), "decimal(10,2)");
        assert_eq!(
            dec.kind(),
            Kind::Decimal {
                precision: 10,
                scale: 2
            }
        );
        dec.flags = ColumnFlags::UNSIGNED_FLAG;
        dec.length = 11;
        assert_eq!(dec.native_type(), "decimal(10,2) unsigned");
        // DECIMAL(65,0) does not fit Decimal128.
        let mut wide = col(MYSQL_TYPE_NEWDECIMAL);
        wide.length = 66;
        assert_eq!(wide.native_type(), "decimal(65,0)");
        assert_eq!(wide.kind(), Kind::Text);

        let mut dt = col(MYSQL_TYPE_DATETIME);
        dt.decimals = 6;
        assert_eq!(dt.native_type(), "datetime(6)");
        assert_eq!(dt.kind(), Kind::DateTime);
        assert_eq!(col(MYSQL_TYPE_TIMESTAMP).native_type(), "timestamp");
        assert_eq!(col(MYSQL_TYPE_TIME).kind(), Kind::Text);
        assert_eq!(col(MYSQL_TYPE_JSON).native_type(), "json");

        let mut e = col(MYSQL_TYPE_STRING);
        e.flags = ColumnFlags::ENUM_FLAG;
        assert_eq!(e.native_type(), "enum");
        e.charset = BINARY_CHARSET;
        assert_eq!(e.kind(), Kind::Text);

        let mut blob = col(MYSQL_TYPE_BLOB);
        blob.charset = BINARY_CHARSET;
        blob.length = 65_535;
        assert_eq!(blob.native_type(), "blob");
        assert_eq!(blob.kind(), Kind::Binary);
        let mut text = col(MYSQL_TYPE_BLOB);
        text.length = u32::MAX;
        assert_eq!(text.native_type(), "longtext");
        assert_eq!(text.kind(), Kind::Text);
        text.length = 262_140; // TEXT in utf8mb4
        assert_eq!(text.native_type(), "text");
        text.length = 255; // TINYTEXT in latin1
        assert_eq!(text.native_type(), "tinytext");

        let mut vb = col(MYSQL_TYPE_VAR_STRING);
        assert_eq!(vb.native_type(), "varchar");
        vb.charset = BINARY_CHARSET;
        assert_eq!(vb.native_type(), "varbinary");

        let mut bit = col(MYSQL_TYPE_BIT);
        bit.length = 8;
        assert_eq!(bit.native_type(), "bit(8)");
        assert_eq!(bit.kind().data_type(), DataType::UInt64);

        let mut maria_json = col(MYSQL_TYPE_BLOB);
        maria_json.type_override = Some("json".into());
        assert_eq!(maria_json.native_type(), "json");
        let field = maria_json.field();
        assert_eq!(quarry_core::result::field_type_label(&field), "json");
    }

    #[test]
    fn parses_numbers_and_nulls() {
        let array = build_array(
            Kind::Int64,
            &raw(&[Some("-5"), None, Some("9223372036854775807")]),
        );
        let ints = array.as_primitive::<Int64Type>();
        assert_eq!(ints.value(0), -5);
        assert!(ints.is_null(1));
        assert_eq!(ints.value(2), i64::MAX);

        let array = build_array(Kind::UInt64, &raw(&[Some("18446744073709551615")]));
        assert_eq!(array.data_type(), &DataType::UInt64);

        let array = build_array(Kind::Float32, &raw(&[Some("1.5e30"), Some("-0.25")]));
        assert_eq!(array.data_type(), &DataType::Float32);
    }

    #[test]
    fn parses_decimals() {
        assert_eq!(parse_decimal("123.45", 10, 2), Some(12345));
        assert_eq!(parse_decimal("-0.50", 10, 2), Some(-50));
        assert_eq!(parse_decimal("7", 10, 2), Some(700));
        assert_eq!(parse_decimal("12345678.90", 10, 2), Some(1_234_567_890));
        assert_eq!(parse_decimal("123456789.00", 10, 2), None);
        assert_eq!(parse_decimal("1.234", 10, 2), None);
        assert_eq!(parse_decimal("abc", 10, 2), None);
        assert_eq!(parse_decimal("", 10, 2), None);
        let max38 = "9".repeat(38);
        assert!(parse_decimal(&max38, 38, 0).is_some());

        let array = build_array(
            Kind::Decimal {
                precision: 10,
                scale: 2,
            },
            &raw(&[Some("-1.05"), None]),
        );
        let dec = array.as_primitive::<Decimal128Type>();
        assert_eq!(dec.value(0), -105);
        assert_eq!(dec.value_as_string(0), "-1.05");
        assert!(dec.is_null(1));
    }

    #[test]
    fn parses_dates_and_times() {
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("2000-03-01"), Some(11_017));
        assert_eq!(parse_date("1969-12-31"), Some(-1));
        assert_eq!(parse_date("2024-02-29"), Some(19_782));
        assert_eq!(parse_date("2023-02-29"), None);
        assert_eq!(parse_date("0000-00-00"), None);
        assert_eq!(parse_date("2024-00-10"), None);
        assert_eq!(parse_date("9999-12-31"), Some(2_932_896));

        assert_eq!(parse_datetime("1970-01-01 00:00:01"), Some(1_000_000));
        assert_eq!(
            parse_datetime("2024-05-06 07:08:09.123456"),
            Some(1_714_979_289_123_456)
        );
        assert_eq!(parse_datetime("1970-01-01 00:00:00.5"), Some(500_000));
        assert_eq!(parse_datetime("0000-00-00 00:00:00"), None);
        assert_eq!(parse_datetime("1970-01-01 24:00:00"), None);
    }

    #[test]
    fn falls_back_to_text_for_zero_dates() {
        let good = build_array(Kind::Date, &raw(&[Some("2024-01-02"), None]));
        assert_eq!(good.as_primitive::<Date32Type>().value(0), 19_724);

        let zero = build_array(Kind::Date, &raw(&[Some("2024-01-02"), Some("0000-00-00")]));
        assert_eq!(zero.data_type(), &DataType::Utf8);
        assert_eq!(zero.as_string::<i32>().value(1), "0000-00-00");

        let ts = build_array(Kind::DateTime, &raw(&[Some("2024-01-02 03:04:05.000006")]));
        assert_eq!(
            ts.as_primitive::<TimestampMicrosecondType>().value(0) % 1_000_000,
            6
        );
    }

    #[test]
    fn reads_bits_and_binary() {
        assert_eq!(parse_bit(&[0x01]), Some(1));
        assert_eq!(parse_bit(&[0x01, 0x00]), Some(256));
        assert_eq!(parse_bit(&[]), Some(0));
        assert_eq!(parse_bit(&[0; 9]), None);
        let blob = build_array(Kind::Binary, &[Some(vec![0, 255]), None]);
        assert_eq!(blob.as_binary::<i32>().value(0), &[0, 255]);
        // Invalid UTF-8 in a text column is replaced, never an error.
        let text = build_array(Kind::Text, &[Some(vec![b'a', 0xff])]);
        assert_eq!(text.as_string::<i32>().value(0), "a\u{fffd}");
    }

    #[test]
    fn renders_geometry_as_wkt() {
        // SRID 0, little-endian POINT(1 2).
        let mut point = vec![0, 0, 0, 0, 1, 1, 0, 0, 0];
        point.extend(1f64.to_le_bytes());
        point.extend(2f64.to_le_bytes());
        assert_eq!(geometry_text(&point), "POINT(1 2)");

        let mut srid = point.clone();
        srid[..4].copy_from_slice(&4326u32.to_le_bytes());
        assert_eq!(geometry_text(&srid), "SRID=4326;POINT(1 2)");

        // LINESTRING(0 0,1.5 -1)
        let mut line = vec![0, 0, 0, 0, 1, 2, 0, 0, 0, 2, 0, 0, 0];
        for v in [0f64, 0.0, 1.5, -1.0] {
            line.extend(v.to_le_bytes());
        }
        assert_eq!(geometry_text(&line), "LINESTRING(0 0,1.5 -1)");

        // Truncated data falls back to hex.
        assert_eq!(geometry_text(&[1, 2, 3]), "0x010203");
    }
}
