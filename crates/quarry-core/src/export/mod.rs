//! Writing a result, or part of it, as CSV, TSV, JSON, Markdown or SQL
//! `INSERT` statements: for the clipboard and for files.
//!
//! Every value is written in full (the grid's cut, one-line form is only for
//! drawing), and each format keeps NULL apart from an empty string:
//!
//! | Format     | NULL                 | Escaping                                        |
//! |------------|----------------------|-------------------------------------------------|
//! | CSV, TSV   | an empty field       | RFC 4180: fields holding the separator, a quote or a line break are quoted, quotes doubled; an empty string is `""` |
//! | JSON       | `null`               | an array of objects, one per line; numbers stay numbers (NaN and infinities become `null`), JSON columns are embedded as JSON |
//! | Markdown   | an empty cell        | `\|` and `\\` escaped, line breaks as `<br>`    |
//! | SQL INSERT | `NULL`               | literals for the connection's dialect: see [`sql`] |
//!
//! Blobs are written as hex in the text formats. Rows are written one at a
//! time into a small buffer, so a file export never holds the whole file.

pub mod sql;

use std::io;

use arrow::array::{Array, AsArray as _};
use arrow::datatypes::{DataType, Field};

use crate::result::{CellText, NATIVE_TYPE};
use crate::{Dialect, ResultSet, json};

/// A way to write rows out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    Csv,
    Tsv,
    Json,
    Markdown,
    SqlInsert,
}

impl Format {
    pub const ALL: [Format; 5] = [
        Format::Csv,
        Format::Tsv,
        Format::Json,
        Format::Markdown,
        Format::SqlInsert,
    ];

    /// The name menus show: `CSV`, `SQL INSERT`.
    pub fn label(self) -> &'static str {
        match self {
            Format::Csv => "CSV",
            Format::Tsv => "TSV",
            Format::Json => "JSON",
            Format::Markdown => "Markdown",
            Format::SqlInsert => "SQL INSERT",
        }
    }

    /// The file extension, without its dot.
    pub fn extension(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Tsv => "tsv",
            Format::Json => "json",
            Format::Markdown => "md",
            Format::SqlInsert => "sql",
        }
    }
}

/// How to write.
#[derive(Debug, Clone)]
pub struct Options {
    /// A header row with the column names, for CSV and TSV. Markdown always
    /// has one, JSON and SQL name the columns anyway.
    pub headers: bool,
    /// The dialect SQL literals are written for.
    pub dialect: Dialect,
    /// The table `INSERT`s go into, as SQL (quoted as needed); `None` is
    /// `result`. Column names are always quoted.
    pub table: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            headers: true,
            dialect: Dialect::DuckDb,
            table: None,
        }
    }
}

/// What kind of value a column holds in one batch, for the formats that
/// write kinds differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Null,
    Bool,
    /// Integers and decimals: their text is a number in every format.
    Exact,
    Float,
    Text,
    Binary,
    Timestamp,
    /// Lists, structs and maps.
    Nested,
    /// Dates, times, intervals and the rest: written as their text.
    Other,
}

fn kind(data_type: &DataType) -> Kind {
    match data_type {
        DataType::Null => Kind::Null,
        DataType::Boolean => Kind::Bool,
        DataType::Float16 | DataType::Float32 | DataType::Float64 => Kind::Float,
        t if t.is_integer() => Kind::Exact,
        DataType::Decimal32(..)
        | DataType::Decimal64(..)
        | DataType::Decimal128(..)
        | DataType::Decimal256(..) => Kind::Exact,
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => Kind::Text,
        DataType::Binary
        | DataType::LargeBinary
        | DataType::BinaryView
        | DataType::FixedSizeBinary(_) => Kind::Binary,
        DataType::Timestamp(..) => Kind::Timestamp,
        DataType::Dictionary(_, value) => kind(value),
        t if t.is_nested() => Kind::Nested,
        _ => Kind::Other,
    }
}

/// Whether a column holds JSON, by the engine's type name or Arrow's JSON
/// extension type.
pub fn is_json_column(field: &Field) -> bool {
    let native = field
        .metadata()
        .get(NATIVE_TYPE)
        .map(|t| t.to_ascii_lowercase());
    let extension = field.metadata().get("ARROW:extension:name");
    matches!(native.as_deref(), Some("json" | "jsonb"))
        || native
            .as_deref()
            .is_some_and(|t| t.starts_with("object('json"))
        || extension.is_some_and(|e| e == "arrow.json")
}

/// Writes rows of a result in one [`Format`], one row at a time.
pub struct Writer<'a> {
    format: Format,
    options: Options,
    result: &'a ResultSet,
    columns: Vec<usize>,
    cells: CellText<'a>,
    /// Per column, by position in `columns`.
    json_columns: Vec<bool>,
    /// The start of each INSERT: `INSERT INTO t (a, b) VALUES (`.
    insert: String,
    rows: usize,
    text: String,
}

impl<'a> Writer<'a> {
    /// A writer of `columns` (data column indices, in the order to write).
    pub fn new(result: &'a ResultSet, columns: &[usize], format: Format, options: Options) -> Self {
        let schema = result.schema();
        let insert = if format == Format::SqlInsert {
            let names: Vec<String> = columns
                .iter()
                .map(|&c| sql::identifier(options.dialect, schema.field(c).name()))
                .collect();
            let table = options
                .table
                .clone()
                .unwrap_or_else(|| options.dialect.quote_ident("result"));
            format!("INSERT INTO {table} ({}) VALUES (", names.join(", "))
        } else {
            String::new()
        };
        Self {
            format,
            json_columns: columns
                .iter()
                .map(|&c| is_json_column(schema.field(c)))
                .collect(),
            options,
            result,
            columns: columns.to_vec(),
            cells: CellText::new(result),
            insert,
            rows: 0,
            text: String::new(),
        }
    }

    /// What comes before the rows: the header, or JSON's `[`.
    pub fn begin(&mut self, out: &mut String) {
        let names: Vec<&str> = self
            .columns
            .iter()
            .map(|&c| self.result.column_name(c))
            .collect();
        match self.format {
            Format::Csv | Format::Tsv if self.options.headers => {
                let separator = self.separator();
                for (ix, name) in names.iter().enumerate() {
                    if ix > 0 {
                        out.push(separator);
                    }
                    delimited_field(out, name, separator);
                }
                out.push('\n');
            }
            Format::Markdown => {
                out.push('|');
                for name in &names {
                    out.push(' ');
                    markdown_cell(out, name);
                    out.push_str(" |");
                }
                out.push_str("\n|");
                for &col in &self.columns {
                    let right = self.result.column_type(col).is_numeric();
                    out.push_str(if right { " ---: |" } else { " --- |" });
                }
                out.push('\n');
            }
            Format::Json => out.push('['),
            _ => {}
        }
    }

    /// Write result row `row`.
    pub fn row(&mut self, row: usize, out: &mut String) {
        let Some((batch, local)) = self.result.locate(row) else {
            return;
        };
        let arrays = self.result.batches()[batch].columns();
        // Taken out for the row, so the cells can borrow `self`.
        let columns = std::mem::take(&mut self.columns);
        match self.format {
            Format::Csv | Format::Tsv => {
                let separator = self.separator();
                for (ix, &col) in columns.iter().enumerate() {
                    if ix > 0 {
                        out.push(separator);
                    }
                    if self.text_of(batch, local, col) {
                        delimited_field(out, &self.text, separator);
                    }
                }
                out.push('\n');
            }
            Format::Markdown => {
                out.push('|');
                for &col in &columns {
                    out.push(' ');
                    if self.text_of(batch, local, col) {
                        markdown_cell(out, &self.text);
                    }
                    out.push_str(" |");
                }
                out.push('\n');
            }
            Format::Json => {
                out.push_str(if self.rows == 0 { "\n{" } else { ",\n{" });
                for (ix, &col) in columns.iter().enumerate() {
                    if ix > 0 {
                        out.push(',');
                    }
                    json::write_string(out, self.result.column_name(col));
                    out.push(':');
                    self.json_value(arrays[col].as_ref(), batch, local, col, ix, out);
                }
                out.push('}');
            }
            Format::SqlInsert => {
                out.push_str(&self.insert);
                for (ix, &col) in columns.iter().enumerate() {
                    if ix > 0 {
                        out.push_str(", ");
                    }
                    self.sql_value(arrays[col].as_ref(), batch, local, col, out);
                }
                out.push_str(");\n");
            }
        }
        self.columns = columns;
        self.rows += 1;
    }

    /// What comes after the rows.
    pub fn end(&mut self, out: &mut String) {
        if self.format == Format::Json {
            out.push_str(if self.rows == 0 { "]\n" } else { "\n]\n" });
        }
    }

    fn separator(&self) -> char {
        if self.format == Format::Tsv {
            '\t'
        } else {
            ','
        }
    }

    /// The full text of a cell into `self.text`; `false` for NULL.
    fn text_of(&mut self, batch: usize, local: usize, col: usize) -> bool {
        self.text.clear();
        self.cells.write_at(batch, local, col, &mut self.text)
    }

    fn json_value(
        &mut self,
        array: &dyn Array,
        batch: usize,
        local: usize,
        col: usize,
        position: usize,
        out: &mut String,
    ) {
        if array.is_null(local) {
            out.push_str("null");
            return;
        }
        match kind(array.data_type()) {
            Kind::Null => out.push_str("null"),
            Kind::Bool | Kind::Exact => {
                self.text_of(batch, local, col);
                out.push_str(&self.text);
            }
            Kind::Float => {
                self.text_of(batch, local, col);
                if float_value(array, local).is_some_and(f64::is_finite) {
                    out.push_str(&self.text);
                } else {
                    out.push_str("null");
                }
            }
            Kind::Text if self.json_columns[position] => {
                self.text_of(batch, local, col);
                match json::compact(&self.text) {
                    Some(compact) => out.push_str(&compact),
                    None => json::write_string(out, &self.text),
                }
            }
            Kind::Nested => nested_json(array, local, out),
            _ => {
                self.text_of(batch, local, col);
                json::write_string(out, &self.text);
            }
        }
    }

    fn sql_value(
        &mut self,
        array: &dyn Array,
        batch: usize,
        local: usize,
        col: usize,
        out: &mut String,
    ) {
        let dialect = self.options.dialect;
        if array.is_null(local) {
            out.push_str("NULL");
            return;
        }
        match kind(array.data_type()) {
            Kind::Null => out.push_str("NULL"),
            Kind::Bool => {
                let value = array.as_boolean_opt().is_some_and(|b| b.value(local));
                out.push_str(sql::boolean(dialect, value));
            }
            Kind::Exact => {
                self.text_of(batch, local, col);
                out.push_str(&self.text);
            }
            Kind::Float => match float_value(array, local) {
                Some(value) if value.is_finite() => {
                    self.text_of(batch, local, col);
                    out.push_str(&self.text);
                }
                Some(value) => out.push_str(&sql::non_finite(dialect, value)),
                None => {
                    self.text_of(batch, local, col);
                    out.push_str(&self.text);
                }
            },
            Kind::Binary => match binary_value(array, local) {
                Some(bytes) => out.push_str(&sql::blob(dialect, bytes)),
                None => out.push_str("NULL"),
            },
            Kind::Timestamp => {
                self.text_of(batch, local, col);
                let text = self.text.replacen('T', " ", 1);
                out.push_str(&sql::string(dialect, &text));
            }
            _ => {
                self.text_of(batch, local, col);
                out.push_str(&sql::string(dialect, &self.text));
            }
        }
    }
}

/// A float cell's value, for any float width.
fn float_value(array: &dyn Array, row: usize) -> Option<f64> {
    use arrow::datatypes::{Float16Type, Float32Type, Float64Type};
    match array.data_type() {
        DataType::Float64 => Some(array.as_primitive::<Float64Type>().value(row)),
        DataType::Float32 => Some(array.as_primitive::<Float32Type>().value(row) as f64),
        DataType::Float16 => Some(array.as_primitive::<Float16Type>().value(row).to_f64()),
        _ => None,
    }
}

/// A binary cell's bytes.
pub fn binary_value(array: &dyn Array, row: usize) -> Option<&[u8]> {
    match array.data_type() {
        DataType::Binary => Some(array.as_binary::<i32>().value(row)),
        DataType::LargeBinary => Some(array.as_binary::<i64>().value(row)),
        DataType::BinaryView => Some(array.as_binary_view().value(row)),
        DataType::FixedSizeBinary(_) => Some(array.as_fixed_size_binary().value(row)),
        _ => None,
    }
}

/// A list, struct or map value as JSON, recursively.
fn nested_json(array: &dyn Array, row: usize, out: &mut String) {
    if array.is_null(row) {
        out.push_str("null");
        return;
    }
    let items = |out: &mut String, values: &dyn Array, range: std::ops::Range<usize>| {
        out.push('[');
        for (n, i) in range.enumerate() {
            if n > 0 {
                out.push(',');
            }
            nested_json(values, i, out);
        }
        out.push(']');
    };
    match array.data_type() {
        DataType::List(_) => {
            let list = array.as_list::<i32>();
            let offsets = list.value_offsets();
            let range = offsets[row] as usize..offsets[row + 1] as usize;
            items(out, list.values().as_ref(), range);
        }
        DataType::LargeList(_) => {
            let list = array.as_list::<i64>();
            let offsets = list.value_offsets();
            let range = offsets[row] as usize..offsets[row + 1] as usize;
            items(out, list.values().as_ref(), range);
        }
        DataType::FixedSizeList(_, size) => {
            let list = array.as_fixed_size_list();
            let size = *size as usize;
            items(out, list.values().as_ref(), row * size..(row + 1) * size);
        }
        DataType::Struct(fields) => {
            let strukt = array.as_struct();
            out.push('{');
            for (ix, field) in fields.iter().enumerate() {
                if ix > 0 {
                    out.push(',');
                }
                json::write_string(out, field.name());
                out.push(':');
                nested_json(strukt.column(ix).as_ref(), row, out);
            }
            out.push('}');
        }
        DataType::Map(..) => {
            let map = array.as_map();
            let offsets = map.value_offsets();
            let (keys, values) = (map.keys(), map.values());
            let range = offsets[row] as usize..offsets[row + 1] as usize;
            let text_keys = matches!(
                keys.data_type(),
                DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View
            );
            if text_keys {
                out.push('{');
                for (n, i) in range.enumerate() {
                    if n > 0 {
                        out.push(',');
                    }
                    json::write_string(out, &crate::result::format_value(keys.as_ref(), i));
                    out.push(':');
                    nested_json(values.as_ref(), i, out);
                }
                out.push('}');
            } else {
                out.push('[');
                for (n, i) in range.enumerate() {
                    if n > 0 {
                        out.push(',');
                    }
                    out.push_str("{\"key\":");
                    nested_json(keys.as_ref(), i, out);
                    out.push_str(",\"value\":");
                    nested_json(values.as_ref(), i, out);
                    out.push('}');
                }
                out.push(']');
            }
        }
        data_type => {
            let text = crate::result::format_value(array, row);
            match kind(data_type) {
                Kind::Bool | Kind::Exact => out.push_str(&text),
                Kind::Float if float_value(array, row).is_some_and(f64::is_finite) => {
                    out.push_str(&text)
                }
                Kind::Float => out.push_str("null"),
                Kind::Null => out.push_str("null"),
                _ => json::write_string(out, &text),
            }
        }
    }
}

/// A CSV or TSV field: quoted when it holds the separator, a quote or a
/// line break, or is empty (so it is not read back as NULL).
fn delimited_field(out: &mut String, text: &str, separator: char) {
    let quote = text.is_empty() || text.contains([separator, '"', '\n', '\r']);
    if quote {
        out.push('"');
        out.push_str(&text.replace('"', "\"\""));
        out.push('"');
    } else {
        out.push_str(text);
    }
}

fn markdown_cell(out: &mut String, text: &str) {
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '|' => out.push_str("\\|"),
            '\\' => out.push_str("\\\\"),
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\r' => out.push_str("<br>"),
            ch => out.push(ch),
        }
    }
}

/// Write `rows` (result row indices, in order) of `columns` to `out`,
/// buffering a little at a time. `progress` hears the number of rows
/// written now and then; returning `false` stops the export with an
/// [`io::ErrorKind::Interrupted`] error. Returns the rows written.
pub fn write(
    result: &ResultSet,
    rows: impl IntoIterator<Item = usize>,
    columns: &[usize],
    format: Format,
    options: Options,
    out: &mut dyn io::Write,
    progress: &mut dyn FnMut(usize) -> bool,
) -> io::Result<usize> {
    const FLUSH_AT: usize = 256 * 1024;
    let mut writer = Writer::new(result, columns, format, options);
    let mut buffer = String::with_capacity(FLUSH_AT + 4096);
    writer.begin(&mut buffer);
    let mut written = 0;
    for row in rows {
        writer.row(row, &mut buffer);
        written += 1;
        if buffer.len() >= FLUSH_AT {
            out.write_all(buffer.as_bytes())?;
            buffer.clear();
            if !progress(written) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "export cancelled",
                ));
            }
        }
    }
    writer.end(&mut buffer);
    out.write_all(buffer.as_bytes())?;
    out.flush()?;
    progress(written);
    Ok(written)
}

/// [`write`] into a string, for the clipboard.
pub fn to_string(
    result: &ResultSet,
    rows: impl IntoIterator<Item = usize>,
    columns: &[usize],
    format: Format,
    options: Options,
) -> String {
    let mut writer = Writer::new(result, columns, format, options);
    let mut out = String::new();
    writer.begin(&mut out);
    for row in rows {
        writer.row(row, &mut out);
    }
    writer.end(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::array::{
        ArrayRef, BinaryArray, BooleanArray, Date32Array, Decimal128Array, Float64Array,
        Int32Array, Int64Array, ListArray, StringArray, StructArray, TimestampMicrosecondArray,
    };
    use arrow::datatypes::{Field, Int32Type, Schema, TimeUnit};
    use arrow::record_batch::RecordBatch;

    use super::*;
    use crate::result::{native_type, result_from_batches};

    /// One row of awkward values and one row of NULLs.
    fn awkward() -> ResultSet {
        let fields = vec![
            Field::new("id", DataType::Int64, true),
            Field::new("text", DataType::Utf8, true),
            Field::new("score", DataType::Float64, true),
            Field::new("ok", DataType::Boolean, true),
            Field::new("blob", DataType::Binary, true),
            native_type(Field::new("doc", DataType::Utf8, true), "json"),
            Field::new("price", DataType::Decimal128(10, 2), true),
        ];
        let schema = Arc::new(Schema::new(fields));
        let columns: Vec<ArrayRef> = vec![
            Arc::new(Int64Array::from(vec![Some(1), None, Some(3)])),
            Arc::new(StringArray::from(vec![
                Some("say \"hi\", | o'k\\\nnext"),
                None,
                Some(""),
            ])),
            Arc::new(Float64Array::from(vec![Some(2.5), None, Some(f64::NAN)])),
            Arc::new(BooleanArray::from(vec![Some(true), None, Some(false)])),
            Arc::new(BinaryArray::from_opt_vec(vec![
                Some(b"\x00\xff"),
                None,
                Some(b""),
            ])),
            Arc::new(StringArray::from(vec![
                Some("{ \"a\": [1, 2] }"),
                None,
                Some("not json"),
            ])),
            Arc::new(
                Decimal128Array::from(vec![Some(1999), None, Some(-5)])
                    .with_precision_and_scale(10, 2)
                    .unwrap(),
            ),
        ];
        let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
        result_from_batches(schema, [batch])
    }

    fn all(format: Format, options: Options) -> String {
        let result = awkward();
        let columns: Vec<usize> = (0..result.column_count()).collect();
        to_string(&result, 0..3, &columns, format, options)
    }

    #[test]
    fn writes_csv_and_tsv() {
        assert_eq!(
            all(Format::Csv, Options::default()),
            "id,text,score,ok,blob,doc,price\n\
             1,\"say \"\"hi\"\", | o'k\\\nnext\",2.5,true,00ff,\"{ \"\"a\"\": [1, 2] }\",19.99\n\
             ,,,,,,\n\
             3,\"\",NaN,false,\"\",not json,-0.05\n"
        );
        let tsv = all(
            Format::Tsv,
            Options {
                headers: false,
                ..Options::default()
            },
        );
        assert_eq!(
            tsv,
            "1\t\"say \"\"hi\"\", | o'k\\\nnext\"\t2.5\ttrue\t00ff\t\"{ \"\"a\"\": [1, 2] }\"\t19.99\n\
             \t\t\t\t\t\t\n\
             3\t\"\"\tNaN\tfalse\t\"\"\tnot json\t-0.05\n"
        );
    }

    #[test]
    fn writes_json_that_parses_back() {
        let out = all(Format::Json, Options::default());
        assert_eq!(
            out,
            "[\n\
             {\"id\":1,\"text\":\"say \\\"hi\\\", | o'k\\\\\\nnext\",\"score\":2.5,\"ok\":true,\"blob\":\"00ff\",\"doc\":{\"a\":[1,2]},\"price\":19.99},\n\
             {\"id\":null,\"text\":null,\"score\":null,\"ok\":null,\"blob\":null,\"doc\":null,\"price\":null},\n\
             {\"id\":3,\"text\":\"\",\"score\":null,\"ok\":false,\"blob\":\"\",\"doc\":\"not json\",\"price\":-0.05}\n\
             ]\n"
        );
        assert!(json::validate(&out).is_ok());
        let empty = to_string(&awkward(), [], &[0], Format::Json, Options::default());
        assert_eq!(empty, "[]\n");
    }

    #[test]
    fn writes_markdown() {
        let result = awkward();
        let out = to_string(
            &result,
            0..2,
            &[0, 1, 2],
            Format::Markdown,
            Options::default(),
        );
        assert_eq!(
            out,
            "| id | text | score |\n\
             | ---: | --- | ---: |\n\
             | 1 | say \"hi\", \\| o'k\\\\<br>next | 2.5 |\n\
             |  |  |  |\n"
        );
    }

    #[test]
    fn writes_inserts_for_each_dialect() {
        let insert = |dialect: Dialect| {
            all(
                Format::SqlInsert,
                Options {
                    dialect,
                    table: Some(dialect.quote_ident("Orders")),
                    ..Options::default()
                },
            )
        };
        assert_eq!(
            insert(Dialect::Postgres),
            "INSERT INTO \"Orders\" (\"id\", \"text\", \"score\", \"ok\", \"blob\", \"doc\", \"price\") VALUES \
             (1, 'say \"hi\", | o''k\\\nnext', 2.5, TRUE, '\\x00ff'::bytea, '{ \"a\": [1, 2] }', 19.99);\n\
             INSERT INTO \"Orders\" (\"id\", \"text\", \"score\", \"ok\", \"blob\", \"doc\", \"price\") VALUES \
             (NULL, NULL, NULL, NULL, NULL, NULL, NULL);\n\
             INSERT INTO \"Orders\" (\"id\", \"text\", \"score\", \"ok\", \"blob\", \"doc\", \"price\") VALUES \
             (3, '', 'NaN', FALSE, '\\x'::bytea, 'not json', -0.05);\n"
        );
        let mysql = insert(Dialect::MySql);
        assert!(
            mysql.starts_with(
                "INSERT INTO Orders (`id`, `text`, `score`, `ok`, `blob`, `doc`, `price`) VALUES \
                 (1, 'say \"hi\", | o''k\\\\\nnext', 2.5, TRUE, X'00FF', "
            ),
            "{mysql}"
        );
        assert!(mysql.contains("(3, '', NULL, FALSE, X'', 'not json', -0.05);"));
        let sqlite = insert(Dialect::Sqlite);
        assert!(sqlite.contains("2.5, 1, X'00FF'"), "{sqlite}");
        let duckdb = insert(Dialect::DuckDb);
        assert!(duckdb.contains("'\\x00\\xFF'::BLOB"), "{duckdb}");
        let clickhouse = insert(Dialect::ClickHouse);
        assert!(clickhouse.contains("o''k\\\\\nnext"), "{clickhouse}");
        assert!(clickhouse.contains("nan, false, unhex('')"), "{clickhouse}");
        let unnamed = all(Format::SqlInsert, Options::default());
        assert!(
            unnamed.starts_with("INSERT INTO result (\"id\","),
            "{unnamed}"
        );
    }

    #[test]
    fn writes_dates_timestamps_and_nested_values() {
        let item = Arc::new(Field::new("item", DataType::Int32, true));
        let list = ListArray::from_iter_primitive::<Int32Type, _, _>(vec![
            Some(vec![Some(1), None]),
            None,
        ]);
        let strukt = StructArray::from(vec![(
            Arc::new(Field::new("k", DataType::Utf8, true)),
            Arc::new(StringArray::from(vec![Some("v"), None])) as ArrayRef,
        )]);
        let schema = Arc::new(Schema::new(vec![
            Field::new("d", DataType::Date32, true),
            Field::new("ts", DataType::Timestamp(TimeUnit::Microsecond, None), true),
            Field::new("l", DataType::List(item), true),
            Field::new("s", strukt.data_type().clone(), true),
            Field::new("n", DataType::Int32, true),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Date32Array::from(vec![Some(19_723), None])),
                Arc::new(TimestampMicrosecondArray::from(vec![
                    Some(1_700_000_000_123_456),
                    None,
                ])),
                Arc::new(list),
                Arc::new(strukt),
                Arc::new(Int32Array::from(vec![Some(7), None])),
            ],
        )
        .unwrap();
        let result = result_from_batches(schema, [batch]);
        let json = to_string(
            &result,
            0..2,
            &[0, 1, 2, 3],
            Format::Json,
            Options::default(),
        );
        assert_eq!(
            json,
            "[\n{\"d\":\"2024-01-01\",\"ts\":\"2023-11-14T22:13:20.123456\",\"l\":[1,null],\"s\":{\"k\":\"v\"}},\n\
             {\"d\":null,\"ts\":null,\"l\":null,\"s\":{\"k\":null}}\n]\n"
        );
        let sql = to_string(
            &result,
            0..1,
            &[0, 1],
            Format::SqlInsert,
            Options::default(),
        );
        assert_eq!(
            sql,
            "INSERT INTO result (\"d\", \"ts\") VALUES ('2024-01-01', '2023-11-14 22:13:20.123456');\n"
        );
    }

    #[test]
    fn streams_with_progress_and_stops() {
        let result = awkward();
        let mut out = Vec::new();
        let written = write(
            &result,
            [0, 2],
            &[0],
            Format::Csv,
            Options::default(),
            &mut out,
            &mut |_| true,
        )
        .unwrap();
        assert_eq!(written, 2);
        assert_eq!(String::from_utf8(out).unwrap(), "id\n1\n3\n");
    }

    #[test]
    fn knows_json_columns() {
        assert!(is_json_column(&native_type(
            Field::new("a", DataType::Utf8, true),
            "JSONB"
        )));
        assert!(!is_json_column(&Field::new("a", DataType::Utf8, true)));
    }
}
