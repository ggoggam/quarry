//! Turning SQLite's dynamically typed rows into Arrow batches.
//!
//! A SQLite column has no fixed type: any row may hold an integer, a float,
//! text, a blob or NULL, whatever the column was declared as. So each batch
//! picks each column's Arrow type from the values actually in it, and
//! batches of one result may disagree, which [`quarry_core::ResultSet`]
//! allows. The result's schema takes its types from the declared types
//! where there are some.

use std::sync::Arc;

use arrow::array::{ArrayRef, BinaryArray, Float64Array, Int64Array, StringArray, new_null_array};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::error::ArrowError;
use arrow::record_batch::RecordBatch;
use rusqlite::types::ValueRef;

/// The largest integer every `f64` holds exactly.
const MAX_EXACT_F64_INT: i64 = 1 << 53;

/// One value as it came out of SQLite.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Cell {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl From<ValueRef<'_>> for Cell {
    fn from(value: ValueRef<'_>) -> Self {
        match value {
            ValueRef::Null => Cell::Null,
            ValueRef::Integer(i) => Cell::Integer(i),
            ValueRef::Real(f) => Cell::Real(f),
            ValueRef::Text(bytes) => Cell::Text(String::from_utf8_lossy(bytes).into_owned()),
            ValueRef::Blob(bytes) => Cell::Blob(bytes.to_vec()),
        }
    }
}

/// The Arrow type for a declared column type, by SQLite's type affinity
/// rules (<https://sqlite.org/datatype3.html#determination_of_column_affinity>),
/// applied in their order. `None` for NUMERIC affinity, which holds integers,
/// floats or text (dates) alike, and for columns without a declared type:
/// those take their type from the values.
pub(crate) fn declared_type(decl: &str) -> Option<DataType> {
    let upper = decl.to_ascii_uppercase();
    if upper.trim().is_empty() {
        None
    } else if upper.contains("INT") {
        Some(DataType::Int64)
    } else if ["CHAR", "CLOB", "TEXT"].iter().any(|s| upper.contains(s)) {
        Some(DataType::Utf8)
    } else if upper.contains("BLOB") {
        Some(DataType::Binary)
    } else if ["REAL", "FLOA", "DOUB"].iter().any(|s| upper.contains(s)) {
        Some(DataType::Float64)
    } else {
        None
    }
}

/// Rows collected column by column until they are flushed as a batch.
pub(crate) struct BatchBuilder {
    columns: Vec<Vec<Cell>>,
    rows: usize,
}

impl BatchBuilder {
    pub fn new(columns: usize) -> Self {
        Self {
            columns: vec![Vec::new(); columns],
            rows: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// Add one value to column `col`; call [`BatchBuilder::end_row`] after
    /// the row's last column.
    pub fn push(&mut self, col: usize, cell: Cell) {
        self.columns[col].push(cell);
    }

    pub fn end_row(&mut self) {
        self.rows += 1;
    }

    /// Build arrays from the collected rows and start over.
    pub fn take_arrays(&mut self) -> Vec<ArrayRef> {
        self.rows = 0;
        self.columns
            .iter_mut()
            .map(|cells| build_column(std::mem::take(cells)))
            .collect()
    }
}

/// The result's schema: each column's declared type where it maps to one,
/// else the type its first batch chose (`Utf8` when there are no rows).
pub(crate) fn result_schema(
    names: &[String],
    decls: &[Option<String>],
    first: Option<&[ArrayRef]>,
) -> SchemaRef {
    let fields: Vec<Field> = names
        .iter()
        .zip(decls)
        .enumerate()
        .map(|(ix, (name, decl))| {
            let data_type = decl
                .as_deref()
                .and_then(declared_type)
                .or_else(|| first.map(|arrays| arrays[ix].data_type().clone()))
                .unwrap_or(DataType::Utf8);
            let field = Field::new(name, data_type, true);
            match decl {
                Some(decl) if !decl.trim().is_empty() => {
                    quarry_core::result::native_type(field, decl.trim())
                }
                _ => field,
            }
        })
        .collect();
    Arc::new(Schema::new(fields))
}

/// A batch whose fields are the result schema's, retyped to its arrays.
pub(crate) fn make_batch(
    schema: &SchemaRef,
    arrays: Vec<ArrayRef>,
) -> Result<RecordBatch, ArrowError> {
    let fields: Vec<Field> = schema
        .fields()
        .iter()
        .zip(&arrays)
        .map(|(field, array)| {
            field
                .as_ref()
                .clone()
                .with_data_type(array.data_type().clone())
        })
        .collect();
    RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays)
}

/// One column's values as the narrowest Arrow array that holds them all:
/// integers as `Int64`, numbers as `Float64`, text as `Utf8`, blobs as
/// `Binary`, and anything mixed with text or blobs as `Utf8`, blobs written
/// as `x'..'` hex. A column of only NULLs is a `Utf8` array of nulls.
pub(crate) fn build_column(cells: Vec<Cell>) -> ArrayRef {
    let (mut ints, mut reals, mut texts, mut blobs) = (false, false, false, false);
    let mut inexact = false;
    for cell in &cells {
        match cell {
            Cell::Null => {}
            Cell::Integer(i) => {
                ints = true;
                inexact |= !(-MAX_EXACT_F64_INT..=MAX_EXACT_F64_INT).contains(i);
            }
            Cell::Real(_) => reals = true,
            Cell::Text(_) => texts = true,
            Cell::Blob(_) => blobs = true,
        }
    }
    match (ints, reals, texts, blobs) {
        (false, false, false, false) => new_null_array(&DataType::Utf8, cells.len()),
        (true, false, false, false) => Arc::new(Int64Array::from_iter(cells.into_iter().map(
            |cell| match cell {
                Cell::Integer(i) => Some(i),
                _ => None,
            },
        ))),
        // Integers too large for an f64 would print rounded, so a column
        // mixing them with floats is shown as text instead.
        (_, true, false, false) if !inexact => Arc::new(Float64Array::from_iter(
            cells.into_iter().map(|cell| match cell {
                Cell::Integer(i) => Some(i as f64),
                Cell::Real(f) => Some(f),
                _ => None,
            }),
        )),
        (false, false, true, false) => Arc::new(StringArray::from_iter(cells.into_iter().map(
            |cell| match cell {
                Cell::Text(s) => Some(s),
                _ => None,
            },
        ))),
        (false, false, false, true) => {
            Arc::new(BinaryArray::from_iter(cells.iter().map(
                |cell| match cell {
                    Cell::Blob(b) => Some(b.as_slice()),
                    _ => None,
                },
            )))
        }
        _ => Arc::new(StringArray::from_iter(cells.into_iter().map(as_text))),
    }
}

fn as_text(cell: Cell) -> Option<String> {
    match cell {
        Cell::Null => None,
        Cell::Integer(i) => Some(i.to_string()),
        Cell::Real(f) => Some(format!("{f:?}")),
        Cell::Text(s) => Some(s),
        Cell::Blob(bytes) => Some(hex_literal(&bytes)),
    }
}

/// A blob as SQLite's blob literal, e.g. `x'cafe'`.
pub(crate) fn hex_literal(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2 + 3);
    out.push_str("x'");
    for byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0xf)] as char);
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use arrow::array::Array;

    use super::*;

    fn kind(cells: Vec<Cell>) -> DataType {
        build_column(cells).data_type().clone()
    }

    #[test]
    fn maps_declared_types_by_affinity() {
        assert_eq!(declared_type("INTEGER"), Some(DataType::Int64));
        assert_eq!(declared_type("bigint unsigned"), Some(DataType::Int64));
        // The rules run in order, so INT wins over CHAR, as in SQLite.
        assert_eq!(declared_type("CHARINT"), Some(DataType::Int64));
        assert_eq!(declared_type("VARCHAR(20)"), Some(DataType::Utf8));
        assert_eq!(declared_type("text"), Some(DataType::Utf8));
        assert_eq!(declared_type("BLOB"), Some(DataType::Binary));
        assert_eq!(declared_type("DOUBLE PRECISION"), Some(DataType::Float64));
        assert_eq!(declared_type("float"), Some(DataType::Float64));
        assert_eq!(declared_type("NUMERIC"), None);
        assert_eq!(declared_type("DATETIME"), None);
        assert_eq!(declared_type("ANY"), None);
        assert_eq!(declared_type(""), None);
    }

    #[test]
    fn picks_the_narrowest_type_per_column() {
        use Cell::*;
        assert_eq!(kind(vec![Integer(1), Null]), DataType::Int64);
        assert_eq!(kind(vec![Integer(1), Real(0.5)]), DataType::Float64);
        assert_eq!(kind(vec![Text("a".into()), Null]), DataType::Utf8);
        assert_eq!(kind(vec![Blob(vec![1]), Null]), DataType::Binary);
        assert_eq!(kind(vec![Null, Null]), DataType::Utf8);
        assert_eq!(kind(vec![Integer(1), Text("a".into())]), DataType::Utf8);
        assert_eq!(kind(vec![Blob(vec![1]), Text("a".into())]), DataType::Utf8);
        assert_eq!(kind(vec![Integer(i64::MAX), Real(0.5)]), DataType::Utf8);
    }

    #[test]
    fn writes_mixed_values_as_text() {
        use Cell::*;
        let array = build_column(vec![
            Integer(7),
            Real(1.0),
            Text("x".into()),
            Blob(vec![0xca, 0xfe]),
            Null,
        ]);
        let strings = array.as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(strings.value(0), "7");
        assert_eq!(strings.value(1), "1.0");
        assert_eq!(strings.value(2), "x");
        assert_eq!(strings.value(3), "x'cafe'");
        assert!(strings.is_null(4));
    }

    #[test]
    fn all_null_columns_are_null_strings() {
        let array = build_column(vec![Cell::Null; 3]);
        assert_eq!(array.len(), 3);
        assert_eq!(array.null_count(), 3);
        assert!(array.is_null(1));
    }
}
