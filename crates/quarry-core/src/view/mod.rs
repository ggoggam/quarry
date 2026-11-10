//! Views of a result: its rows sorted, filtered and searched, without
//! copying them.
//!
//! A view is a list of row indices into the [`ResultSet`], in the order to
//! show them; the grid, copying and export read rows through it. Sorting
//! compares the typed Arrow arrays with Arrow's kernels, filtering evaluates
//! a small expression per batch, and finding walks the view's rows, so none
//! of them turns the result into rows of strings.
//!
//! - [`sort`]: the order of one or more sort keys.
//! - [`filter`]: the filter expression language, parsed and evaluated.
//! - [`find`]: matching cells for find in results.
//!
//! All of it is plain functions over a [`ResultSet`], meant to run off the
//! UI thread: long ones report progress and can be stopped.

pub mod filter;
pub mod find;
pub mod sort;

use std::sync::Arc;

use arrow::array::{Array, ArrayRef, StringBuilder};
use arrow::compute::cast;
use arrow::datatypes::DataType;
use arrow::util::display::{ArrayFormatter, FormatOptions};

use crate::ResultSet;

/// How far a long computation has got, from 0 to 1, and whether to go on:
/// returning `false` stops it.
pub type Progress<'a> = &'a mut dyn FnMut(f32) -> bool;

/// It was stopped by its [`Progress`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stopped;

/// Rows `order` keeps where `keep` says so; all of them without a mask.
/// With neither, `None`: the result's own order, unfiltered.
pub fn combine(rows: usize, order: Option<&[u32]>, keep: Option<&[bool]>) -> Option<Vec<u32>> {
    match (order, keep) {
        (None, None) => None,
        (Some(order), None) => Some(order.to_vec()),
        (Some(order), Some(keep)) => Some(
            order
                .iter()
                .copied()
                .filter(|&row| keep.get(row as usize).copied().unwrap_or(false))
                .collect(),
        ),
        (None, Some(keep)) => Some(
            (0..rows.min(keep.len()))
                .filter(|&row| keep[row])
                .map(|row| row as u32)
                .collect(),
        ),
    }
}

/// A column's arrays in each batch, dictionaries decoded to their values.
fn column_arrays(result: &ResultSet, col: usize) -> Vec<ArrayRef> {
    result
        .batches()
        .iter()
        .map(|batch| decode(batch.column(col)))
        .collect()
}

/// A dictionary array as its values; any other array as it is.
fn decode(array: &ArrayRef) -> ArrayRef {
    match array.data_type() {
        DataType::Dictionary(_, value) => cast(array, value).unwrap_or_else(|_| text_array(array)),
        _ => array.clone(),
    }
}

fn is_string(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View
    )
}

fn is_binary(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Binary
            | DataType::LargeBinary
            | DataType::BinaryView
            | DataType::FixedSizeBinary(_)
    )
}

/// An array as the text the grid shows for it, for types that are compared
/// or matched as text. A string array is returned as it is.
fn text_array(array: &ArrayRef) -> ArrayRef {
    if is_string(array.data_type()) {
        return array.clone();
    }
    let options = FormatOptions::default().with_display_error(true);
    let mut builder = StringBuilder::with_capacity(array.len(), array.len() * 8);
    match ArrayFormatter::try_new(array.as_ref(), &options) {
        Ok(formatter) => {
            let mut text = String::new();
            for row in 0..array.len() {
                if array.is_null(row) {
                    builder.append_null();
                } else {
                    use std::fmt::Write as _;
                    text.clear();
                    let _ = write!(text, "{}", formatter.value(row));
                    builder.append_value(&text);
                }
            }
        }
        Err(_) => {
            for _ in 0..array.len() {
                builder.append_null();
            }
        }
    }
    Arc::new(builder.finish())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::result::result_from_batches;
    use arrow::array::{BinaryArray, Float64Array, Int64Array, StringArray};
    use arrow::datatypes::{Field, Schema};
    use arrow::record_batch::RecordBatch;

    /// `id` (int, NULL at 3), `name` (text, NULL at 1), `score` (float),
    /// across two batches: rows 0..3 and 3..6.
    pub fn people() -> ResultSet {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, true),
            Field::new("name", DataType::Utf8, true),
            Field::new("score", DataType::Float64, true),
        ]));
        let batch = |ids: Vec<Option<i64>>, names: Vec<Option<&str>>, scores: Vec<Option<f64>>| {
            RecordBatch::try_new(
                schema.clone(),
                vec![
                    Arc::new(Int64Array::from(ids)),
                    Arc::new(StringArray::from(names)),
                    Arc::new(Float64Array::from(scores)),
                ],
            )
            .unwrap()
        };
        result_from_batches(
            schema.clone(),
            [
                batch(
                    vec![Some(3), Some(1), Some(2)],
                    vec![Some("carol"), None, Some("Bob")],
                    vec![Some(2.5), Some(-1.0), Some(2.5)],
                ),
                batch(
                    vec![None, Some(10), Some(-4)],
                    vec![Some("alice"), Some("dave"), Some("bob")],
                    vec![Some(f64::NAN), None, Some(0.0)],
                ),
            ],
        )
    }

    /// A SQLite-like column whose batches disagree on its type.
    pub fn mixed() -> ResultSet {
        let schema = Arc::new(Schema::new(vec![Field::new("v", DataType::Utf8, true)]));
        let batch = |array: ArrayRef| {
            let schema = Arc::new(Schema::new(vec![Field::new(
                "v",
                array.data_type().clone(),
                true,
            )]));
            RecordBatch::try_new(schema, vec![array]).unwrap()
        };
        result_from_batches(
            schema,
            [
                batch(Arc::new(Int64Array::from(vec![Some(5), None, Some(-2)]))),
                batch(Arc::new(StringArray::from(vec!["b", "a"]))),
                batch(Arc::new(Float64Array::from(vec![1.5]))),
                batch(Arc::new(BinaryArray::from_vec(vec![b"\x00"]))),
                batch(Arc::new(arrow::array::NullArray::new(1))),
            ],
        )
    }

    #[test]
    fn combines_order_and_mask() {
        let order = [2u32, 0, 1, 3];
        let keep = [true, false, true, true];
        assert_eq!(combine(4, None, None), None);
        assert_eq!(combine(4, Some(&order), None), Some(vec![2, 0, 1, 3]));
        assert_eq!(combine(4, Some(&order), Some(&keep)), Some(vec![2, 0, 3]));
        assert_eq!(combine(4, None, Some(&keep)), Some(vec![0, 2, 3]));
    }

    #[test]
    fn formats_arrays_as_shown() {
        let array: ArrayRef = Arc::new(Float64Array::from(vec![Some(1.5), None]));
        let text = text_array(&array);
        let text = text.as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(text.value(0), "1.5");
        assert!(text.is_null(1));
    }
}
