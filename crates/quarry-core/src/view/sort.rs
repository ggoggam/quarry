//! Sorting a result by one or more columns.
//!
//! Each key column is gathered from its batches into one typed array and the
//! row order comes from Arrow's lexicographic sort over them, with the
//! original row number as the last key so equal rows keep their order (the
//! sort is stable). NULLs go last whichever the direction, as in DuckDB.
//! Text compares by its bytes (code points), not by a locale.
//!
//! A column whose batches disagree on its type (SQLite picks types per
//! batch) is sorted the way SQLite orders mixed values: numbers (compared
//! as numbers), then text, then blobs.

use std::sync::Arc;

use arrow::array::{Array, ArrayRef, UInt8Array, UInt32Array, new_null_array};
use arrow::compute::{SortColumn, SortOptions, cast, concat, lexsort_to_indices};
use arrow::datatypes::DataType;

use super::{column_arrays, is_binary, is_string, text_array};
use crate::ResultSet;

/// One key of a sort: a data column and its direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SortKey {
    pub column: usize,
    pub descending: bool,
}

impl SortKey {
    pub fn ascending(column: usize) -> Self {
        Self {
            column,
            descending: false,
        }
    }

    pub fn descending(column: usize) -> Self {
        Self {
            column,
            descending: true,
        }
    }
}

/// What clicking a column's sort control does to `keys`. `add` keeps the
/// other keys (a shift- or option-click), so the column becomes, or stays,
/// one of several; otherwise it becomes the only key. A column cycles
/// ascending, descending, then out of the sort.
pub fn toggle(keys: &[SortKey], column: usize, add: bool) -> Vec<SortKey> {
    let current = keys.iter().find(|k| k.column == column).copied();
    let next = match current {
        None => Some(SortKey::ascending(column)),
        Some(key) if !key.descending => Some(SortKey::descending(column)),
        Some(_) => None,
    };
    if !add {
        // A plain click on a column of a multi-key sort starts over with it.
        if current.is_some() && keys.len() > 1 {
            return vec![SortKey::ascending(column)];
        }
        return next.into_iter().collect();
    }
    match (current, next) {
        (Some(_), Some(next)) => keys
            .iter()
            .map(|k| if k.column == column { next } else { *k })
            .collect(),
        (Some(_), None) => keys
            .iter()
            .copied()
            .filter(|k| k.column != column)
            .collect(),
        (None, Some(next)) => keys.iter().copied().chain([next]).collect(),
        (None, None) => keys.to_vec(),
    }
}

/// Set `column` to sort `descending`: as the only key, or with `add` as a
/// further key after the others (replacing its old place if it had one).
pub fn set(keys: &[SortKey], column: usize, descending: bool, add: bool) -> Vec<SortKey> {
    let key = SortKey { column, descending };
    if !add {
        return vec![key];
    }
    if keys.iter().any(|k| k.column == column) {
        return keys
            .iter()
            .map(|k| if k.column == column { key } else { *k })
            .collect();
    }
    keys.iter().copied().chain([key]).collect()
}

/// The result's rows in the order of `keys`, as row indices. An empty
/// `keys` is the result's own order.
pub fn sort_indices(result: &ResultSet, keys: &[SortKey]) -> Result<Vec<u32>, String> {
    let rows = result.row_count();
    if rows > u32::MAX as usize {
        return Err("results of more than 4,294,967,295 rows cannot be sorted".into());
    }
    if keys.is_empty() || rows == 0 {
        return Ok((0..rows as u32).collect());
    }
    let mut columns = Vec::new();
    for key in keys {
        if key.column >= result.column_count() {
            return Err(format!("there is no column {}", key.column + 1));
        }
        let options = SortOptions {
            descending: key.descending,
            nulls_first: false,
        };
        for values in key_arrays(result, key.column)? {
            columns.push(SortColumn {
                values,
                options: Some(options),
            });
        }
    }
    // The original order breaks ties, which makes the sort stable.
    columns.push(SortColumn {
        values: Arc::new(UInt32Array::from_iter_values(0..rows as u32)),
        options: None,
    });
    let indices = lexsort_to_indices(&columns, None).map_err(|e| e.to_string())?;
    Ok(indices.values().to_vec())
}

/// The arrays to sort `col` by, each as long as the result: usually its
/// values gathered into one array; for mixed types a type class first.
fn key_arrays(result: &ResultSet, col: usize) -> Result<Vec<ArrayRef>, String> {
    let arrays = column_arrays(result, col);
    let mut types: Vec<&DataType> = arrays
        .iter()
        .map(|a| a.data_type())
        .filter(|t| **t != DataType::Null)
        .collect();
    types.dedup();
    types.sort_by_key(|t| t.to_string());
    types.dedup();
    let common = match types.as_slice() {
        // Only NULLs: every row ties.
        [] => return Ok(Vec::new()),
        [only] => Some((*only).clone()),
        many if many.iter().all(|t| t.is_numeric()) => Some(
            if many.iter().all(|t| t.is_integer()) && !many.contains(&&DataType::UInt64) {
                DataType::Int64
            } else {
                DataType::Float64
            },
        ),
        _ => None,
    };
    let Some(common) = common else {
        return mixed_keys(&arrays);
    };
    let unified: Vec<ArrayRef> = arrays
        .iter()
        .map(|array| match array.data_type() {
            DataType::Null => Ok(new_null_array(&common, array.len())),
            t if *t == common => Ok(array.clone()),
            _ => cast(array, &common).map_err(|e| e.to_string()),
        })
        .collect::<Result<_, _>>()?;
    let joined = join(&unified)?;
    if can_order(&common) {
        Ok(vec![joined])
    } else {
        // Types Arrow has no ordering for (maps, unions) sort by their text.
        Ok(vec![text_array(&joined)])
    }
}

/// SQLite's order for a column of mixed types: a class key (numbers, text,
/// blobs), then the value within each class.
fn mixed_keys(arrays: &[ArrayRef]) -> Result<Vec<ArrayRef>, String> {
    const NUMBER: u8 = 0;
    const TEXT: u8 = 1;
    const BLOB: u8 = 2;
    let mut classes = Vec::with_capacity(arrays.len());
    let mut numbers = Vec::with_capacity(arrays.len());
    let mut texts = Vec::with_capacity(arrays.len());
    let mut blobs = Vec::with_capacity(arrays.len());
    for array in arrays {
        let len = array.len();
        let data_type = array.data_type();
        let class = if data_type.is_numeric() {
            Some(NUMBER)
        } else if is_binary(data_type) {
            Some(BLOB)
        } else if *data_type == DataType::Null {
            None
        } else {
            Some(TEXT)
        };
        classes.push(Arc::new(UInt8Array::from_iter(
            (0..len).map(|row| class.filter(|_| array.is_valid(row))),
        )) as ArrayRef);
        let none = |t: &DataType| new_null_array(t, len);
        numbers.push(match class {
            Some(NUMBER) => cast(array, &DataType::Float64).map_err(|e| e.to_string())?,
            _ => none(&DataType::Float64),
        });
        texts.push(match class {
            Some(TEXT) if is_string(data_type) => {
                cast(array, &DataType::Utf8).map_err(|e| e.to_string())?
            }
            Some(TEXT) => text_array(array),
            _ => none(&DataType::Utf8),
        });
        blobs.push(match class {
            Some(BLOB) => cast(array, &DataType::LargeBinary).map_err(|e| e.to_string())?,
            _ => none(&DataType::LargeBinary),
        });
    }
    Ok(vec![
        join(&classes)?,
        join(&numbers)?,
        join(&texts)?,
        join(&blobs)?,
    ])
}

fn join(arrays: &[ArrayRef]) -> Result<ArrayRef, String> {
    match arrays {
        [one] => Ok(one.clone()),
        _ => {
            let refs: Vec<&dyn Array> = arrays.iter().map(|a| a.as_ref()).collect();
            concat(&refs).map_err(|e| e.to_string())
        }
    }
}

/// Whether Arrow can order values of a type.
pub(crate) fn can_order(data_type: &DataType) -> bool {
    let empty = arrow::array::new_empty_array(data_type);
    arrow::array::make_comparator(empty.as_ref(), empty.as_ref(), SortOptions::default()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::tests::{mixed, people};

    fn sorted(result: &ResultSet, keys: &[SortKey]) -> Vec<u32> {
        sort_indices(result, keys).unwrap()
    }

    #[test]
    fn sorts_integers_with_nulls_last_both_ways() {
        let result = people();
        // ids: 3, 1, 2, NULL, 10, -4
        assert_eq!(
            sorted(&result, &[SortKey::ascending(0)]),
            [5, 1, 2, 0, 4, 3]
        );
        assert_eq!(
            sorted(&result, &[SortKey::descending(0)]),
            [4, 0, 2, 1, 5, 3]
        );
    }

    #[test]
    fn sorts_text_by_code_point() {
        // names: carol, NULL, Bob, alice, dave, bob
        let result = people();
        assert_eq!(
            sorted(&result, &[SortKey::ascending(1)]),
            [2, 3, 5, 0, 4, 1]
        );
    }

    #[test]
    fn sorts_floats_with_nan_after_numbers() {
        // scores: 2.5, -1, 2.5, NaN, NULL, 0
        let result = people();
        assert_eq!(
            sorted(&result, &[SortKey::ascending(2)]),
            [1, 5, 0, 2, 3, 4]
        );
    }

    #[test]
    fn is_stable_and_takes_more_keys() {
        let result = people();
        // Rows 0 and 2 tie on score; the original order stays.
        let by_score = sorted(&result, &[SortKey::descending(2)]);
        assert_eq!(by_score, [3, 0, 2, 5, 1, 4]);
        // A second key orders the tie: id descending puts 3 (row 0) first,
        // ascending puts 2 (row 2) first.
        assert_eq!(
            sorted(&result, &[SortKey::descending(2), SortKey::ascending(0)]),
            [3, 2, 0, 5, 1, 4]
        );
    }

    #[test]
    fn sorts_mixed_types_as_sqlite_does() {
        // 5, NULL, -2 | 'b', 'a' | 1.5 | x'00' | NULL
        let result = mixed();
        assert_eq!(
            sorted(&result, &[SortKey::ascending(0)]),
            [2, 5, 0, 4, 3, 6, 1, 7]
        );
        assert_eq!(
            sorted(&result, &[SortKey::descending(0)]),
            [6, 3, 4, 0, 5, 2, 1, 7]
        );
    }

    #[test]
    fn empty_keys_keep_the_order() {
        assert_eq!(sorted(&people(), &[]), [0, 1, 2, 3, 4, 5]);
        assert!(sort_indices(&people(), &[SortKey::ascending(9)]).is_err());
    }

    #[test]
    fn clicks_cycle_and_add_keys() {
        let a = SortKey::ascending;
        let d = SortKey::descending;
        assert_eq!(toggle(&[], 1, false), [a(1)]);
        assert_eq!(toggle(&[a(1)], 1, false), [d(1)]);
        assert_eq!(toggle(&[d(1)], 1, false), []);
        assert_eq!(toggle(&[a(1)], 2, false), [a(2)]);
        assert_eq!(toggle(&[a(1)], 2, true), [a(1), a(2)]);
        assert_eq!(toggle(&[a(1), a(2)], 2, true), [a(1), d(2)]);
        assert_eq!(toggle(&[a(1), d(2)], 2, true), [a(1)]);
        assert_eq!(toggle(&[a(1), d(2)], 2, false), [a(2)]);
        assert_eq!(set(&[a(1), a(2)], 1, true, true), [d(1), a(2)]);
        assert_eq!(set(&[a(1)], 3, true, true), [a(1), d(3)]);
        assert_eq!(set(&[a(1), a(2)], 3, false, false), [a(3)]);
    }
}
