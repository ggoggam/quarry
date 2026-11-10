//! Query results held as Arrow record batches.
//!
//! Rows stay in the columnar batches the engine produced. Nothing is turned
//! into text until a cell is drawn, so a grid over millions of rows only ever
//! formats the cells on screen.

use std::sync::Arc;

use arrow::array::{Array, ArrayRef};
use arrow::datatypes::{DataType, Field, SchemaRef};
use arrow::record_batch::RecordBatch;
use arrow::util::display::{ArrayFormatter, FormatOptions};

/// Field metadata key holding the engine's own name for a column's type,
/// e.g. `jsonb` or `LowCardinality(String)`, shown in headers in place of
/// the Arrow type's generic name. Drivers set it with [`native_type`].
pub const NATIVE_TYPE: &str = "quarry.native_type";

/// Attach the engine's type name to a field.
pub fn native_type(field: Field, name: impl Into<String>) -> Field {
    let mut metadata = field.metadata().clone();
    metadata.insert(NATIVE_TYPE.to_owned(), name.into());
    field.with_metadata(metadata)
}

/// The type name to show for a field: the engine's own when the driver
/// gave one, else a short name for the Arrow type.
pub fn field_type_label(field: &Field) -> String {
    match field.metadata().get(NATIVE_TYPE) {
        Some(name) if !name.is_empty() => name.clone(),
        _ => type_label(field.data_type()),
    }
}

/// Longest text a cell renders; the full value is still in the batch.
pub const MAX_CELL_CHARS: usize = 512;

#[derive(Debug, Clone)]
pub struct ResultSet {
    schema: SchemaRef,
    batches: Vec<RecordBatch>,
    /// `starts[i]` is the global index of the first row of `batches[i]`.
    starts: Vec<usize>,
    rows: usize,
}

impl ResultSet {
    pub fn new(schema: SchemaRef) -> Self {
        Self {
            schema,
            batches: Vec::new(),
            starts: Vec::new(),
            rows: 0,
        }
    }

    pub fn schema(&self) -> &SchemaRef {
        &self.schema
    }

    pub fn row_count(&self) -> usize {
        self.rows
    }

    pub fn column_count(&self) -> usize {
        self.schema.fields().len()
    }

    pub fn batches(&self) -> &[RecordBatch] {
        &self.batches
    }

    /// Append a batch. Its columns must match the schema's in number and
    /// order, but not necessarily in type: a driver for a dynamically typed
    /// engine (SQLite) may pick each batch's types from its values. Cells
    /// are formatted per batch, so this displays correctly.
    pub fn push(&mut self, batch: RecordBatch) {
        if batch.num_rows() == 0 {
            return;
        }
        self.starts.push(self.rows);
        self.rows += batch.num_rows();
        self.batches.push(batch);
    }

    /// Keep only the first `rows` rows.
    pub fn truncate(&mut self, rows: usize) {
        if rows >= self.rows {
            return;
        }
        let (batch_ix, local) = self.locate(rows).expect("row is in range");
        self.batches.truncate(batch_ix + 1);
        self.starts.truncate(batch_ix + 1);
        if local == 0 {
            self.batches.pop();
            self.starts.pop();
        } else {
            let last = self.batches.last_mut().expect("kept one batch");
            *last = last.slice(0, local);
        }
        self.rows = rows;
    }

    /// The batch holding global row `row` and the row's index inside it.
    pub fn locate(&self, row: usize) -> Option<(usize, usize)> {
        if row >= self.rows {
            return None;
        }
        let batch_ix = self.starts.partition_point(|&start| start <= row) - 1;
        Some((batch_ix, row - self.starts[batch_ix]))
    }

    pub fn column(&self, row: usize, col: usize) -> Option<(&ArrayRef, usize)> {
        let (batch_ix, local) = self.locate(row)?;
        let array = self.batches[batch_ix].columns().get(col)?;
        Some((array, local))
    }

    pub fn is_null(&self, row: usize, col: usize) -> bool {
        self.column(row, col)
            .is_some_and(|(array, local)| array.is_null(local))
    }

    /// The cell as display text, `None` for SQL NULL.
    ///
    /// Text is cut at [`MAX_CELL_CHARS`] and newlines are shown as `⏎`, so a
    /// cell always draws on one line.
    pub fn cell_text(&self, row: usize, col: usize) -> Option<String> {
        let (array, local) = self.column(row, col)?;
        if array.is_null(local) {
            return None;
        }
        // Format no more than is shown: a cell of many megabytes costs the
        // same to draw as a short one.
        let options = FormatOptions::default().with_display_error(true);
        let mut text = Bounded::new(MAX_CELL_CHARS + 1);
        match ArrayFormatter::try_new(array.as_ref(), &options) {
            Ok(formatter) => {
                use std::fmt::Write as _;
                let _ = write!(text, "{}", formatter.value(local));
            }
            Err(err) => text.text = format!("<{err}>"),
        }
        Some(single_line(&text.text, MAX_CELL_CHARS))
    }

    /// The full, untruncated cell value as text, for copying and export.
    pub fn cell_value(&self, row: usize, col: usize) -> Option<String> {
        let (array, local) = self.column(row, col)?;
        (!array.is_null(local)).then(|| format_value(array.as_ref(), local))
    }

    pub fn column_name(&self, col: usize) -> &str {
        self.schema.field(col).name()
    }

    /// The column's type as headers show it; see [`field_type_label`].
    pub fn column_type_label(&self, col: usize) -> String {
        field_type_label(self.schema.field(col))
    }

    pub fn column_type(&self, col: usize) -> &DataType {
        self.schema.field(col).data_type()
    }
}

/// One value as full text, however long. Prefer [`CellText`] for many.
pub fn format_value(array: &dyn Array, row: usize) -> String {
    let options = FormatOptions::default().with_display_error(true);
    match ArrayFormatter::try_new(array, &options) {
        Ok(formatter) => formatter.value(row).to_string(),
        Err(err) => format!("<{err}>"),
    }
}

/// Writes cells of a result as their full text, keeping one formatter per
/// batch and column so that formatting many cells costs no setup each.
pub struct CellText<'a> {
    result: &'a ResultSet,
    options: FormatOptions<'a>,
    /// `[batch][column]`, made the first time a cell there is asked for.
    formatters: Vec<Vec<Option<Result<ArrayFormatter<'a>, String>>>>,
}

impl<'a> CellText<'a> {
    pub fn new(result: &'a ResultSet) -> Self {
        let columns = result.column_count();
        Self {
            result,
            options: FormatOptions::default().with_display_error(true),
            formatters: result
                .batches
                .iter()
                .map(|_| (0..columns).map(|_| None).collect())
                .collect(),
        }
    }

    pub fn result(&self) -> &'a ResultSet {
        self.result
    }

    /// Append the text of `(row, col)` to `out`; `false`, with nothing
    /// written, when it is NULL or out of range.
    pub fn write(&mut self, row: usize, col: usize, out: &mut String) -> bool {
        match self.result.locate(row) {
            Some((batch, local)) => self.write_at(batch, local, col, out),
            None => false,
        }
    }

    /// [`Self::write`] for a row already located in its batch.
    pub fn write_at(&mut self, batch: usize, local: usize, col: usize, out: &mut String) -> bool {
        let result = self.result;
        let Some(array) = result.batches.get(batch).and_then(|b| b.columns().get(col)) else {
            return false;
        };
        if array.is_null(local) {
            return false;
        }
        let formatter = self.formatters[batch][col].get_or_insert_with(|| {
            ArrayFormatter::try_new(array.as_ref(), &self.options).map_err(|e| e.to_string())
        });
        match formatter {
            Ok(formatter) => {
                use std::fmt::Write as _;
                let _ = write!(out, "{}", formatter.value(local));
            }
            Err(err) => {
                out.push('<');
                out.push_str(err);
                out.push('>');
            }
        }
        true
    }
}

/// A `fmt::Write` that keeps the first `max_chars` characters and then
/// stops the formatting that writes to it.
struct Bounded {
    text: String,
    left: usize,
}

impl Bounded {
    fn new(max_chars: usize) -> Self {
        Self {
            text: String::new(),
            left: max_chars,
        }
    }
}

impl std::fmt::Write for Bounded {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        for ch in s.chars() {
            if self.left == 0 {
                return Err(std::fmt::Error);
            }
            self.text.push(ch);
            self.left -= 1;
        }
        Ok(())
    }
}

fn single_line(text: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max_chars * 4));
    for (n, ch) in text.chars().enumerate() {
        if n == max_chars {
            out.push('…');
            break;
        }
        match ch {
            '\n' => out.push('⏎'),
            '\r' | '\t' => out.push(' '),
            ch => out.push(ch),
        }
    }
    out
}

/// Whether values of this type read best right-aligned.
pub fn is_numeric(data_type: &DataType) -> bool {
    data_type.is_numeric()
}

/// A short, engine-neutral name for a column type, for headers.
pub fn type_label(data_type: &DataType) -> String {
    match data_type {
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => "varchar".into(),
        DataType::Int8 => "tinyint".into(),
        DataType::Int16 => "smallint".into(),
        DataType::Int32 => "integer".into(),
        DataType::Int64 => "bigint".into(),
        DataType::UInt8 => "utinyint".into(),
        DataType::UInt16 => "usmallint".into(),
        DataType::UInt32 => "uinteger".into(),
        DataType::UInt64 => "ubigint".into(),
        DataType::Float32 => "float".into(),
        DataType::Float64 => "double".into(),
        DataType::Boolean => "boolean".into(),
        DataType::Date32 | DataType::Date64 => "date".into(),
        DataType::Timestamp(_, None) => "timestamp".into(),
        DataType::Timestamp(_, Some(_)) => "timestamptz".into(),
        DataType::Time32(_) | DataType::Time64(_) => "time".into(),
        DataType::Interval(_) => "interval".into(),
        DataType::Decimal128(p, s) | DataType::Decimal256(p, s) => format!("decimal({p},{s})"),
        DataType::Binary | DataType::LargeBinary | DataType::BinaryView => "blob".into(),
        DataType::List(_) | DataType::LargeList(_) | DataType::FixedSizeList(..) => "list".into(),
        DataType::Struct(_) => "struct".into(),
        DataType::Map(..) => "map".into(),
        DataType::Dictionary(_, value) => type_label(value),
        other => other.to_string().to_lowercase(),
    }
}

/// Convenience for tests and drivers: build a result set from batches.
pub fn result_from_batches(
    schema: SchemaRef,
    batches: impl IntoIterator<Item = RecordBatch>,
) -> ResultSet {
    let mut result = ResultSet::new(schema);
    for batch in batches {
        result.push(batch);
    }
    result
}

/// An empty schema, for statements that return no columns.
pub fn empty_schema() -> SchemaRef {
    Arc::new(arrow::datatypes::Schema::empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Int64Array, StringArray};
    use arrow::datatypes::{Field, Schema};

    fn sample() -> ResultSet {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
        ]));
        let batch = |ids: Vec<i64>, names: Vec<Option<&str>>| {
            RecordBatch::try_new(
                schema.clone(),
                vec![
                    Arc::new(Int64Array::from(ids)),
                    Arc::new(StringArray::from(names)),
                ],
            )
            .unwrap()
        };
        result_from_batches(
            schema.clone(),
            [
                batch(vec![0, 1, 2], vec![Some("a"), None, Some("line\nbreak")]),
                batch(vec![], vec![]),
                batch(vec![3, 4], vec![Some("d"), Some("e")]),
            ],
        )
    }

    #[test]
    fn prefers_the_engines_type_name() {
        let plain = Field::new("a", DataType::Utf8, true);
        assert_eq!(field_type_label(&plain), "varchar");
        let tagged = native_type(plain, "LowCardinality(String)");
        assert_eq!(field_type_label(&tagged), "LowCardinality(String)");
    }

    #[test]
    fn locates_rows_across_batches() {
        let result = sample();
        assert_eq!(result.row_count(), 5);
        assert_eq!(result.locate(0), Some((0, 0)));
        assert_eq!(result.locate(2), Some((0, 2)));
        assert_eq!(result.locate(3), Some((1, 0)));
        assert_eq!(result.locate(4), Some((1, 1)));
        assert_eq!(result.locate(5), None);
    }

    #[test]
    fn formats_cells_on_demand() {
        let result = sample();
        assert_eq!(result.cell_text(3, 0).as_deref(), Some("3"));
        assert_eq!(result.cell_text(1, 1), None);
        assert!(result.is_null(1, 1));
        assert_eq!(result.cell_text(2, 1).as_deref(), Some("line⏎break"));
        assert_eq!(result.cell_value(2, 1).as_deref(), Some("line\nbreak"));
    }

    #[test]
    fn truncates_mid_batch_and_on_boundary() {
        let mut result = sample();
        result.truncate(4);
        assert_eq!(result.row_count(), 4);
        assert_eq!(result.cell_text(3, 0).as_deref(), Some("3"));
        assert_eq!(result.locate(4), None);

        let mut result = sample();
        result.truncate(3);
        assert_eq!(result.batches().len(), 1);
        assert_eq!(result.row_count(), 3);
    }

    #[test]
    fn formats_only_what_a_cell_shows() {
        let schema = Arc::new(Schema::new(vec![Field::new("t", DataType::Utf8, true)]));
        let long = "y".repeat(5_000_000);
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(StringArray::from(vec![Some(long.as_str())]))],
        )
        .unwrap();
        let result = result_from_batches(schema, [batch]);
        let shown = result.cell_text(0, 0).unwrap();
        assert_eq!(shown.chars().count(), MAX_CELL_CHARS + 1);
        assert_eq!(result.cell_value(0, 0).unwrap().len(), 5_000_000);
        let mut cells = CellText::new(&result);
        let mut out = String::new();
        assert!(cells.write(0, 0, &mut out));
        assert_eq!(out.len(), 5_000_000);
        assert!(!cells.write(1, 0, &mut out));
    }

    #[test]
    fn cuts_long_text() {
        let text = "x".repeat(MAX_CELL_CHARS + 10);
        let cut = single_line(&text, MAX_CELL_CHARS);
        assert_eq!(cut.chars().count(), MAX_CELL_CHARS + 1);
        assert!(cut.ends_with('…'));
    }
}
