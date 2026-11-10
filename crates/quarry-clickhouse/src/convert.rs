//! Turning ClickHouse's Arrow output into what Quarry displays well.
//!
//! ClickHouse strings are bytes, so its Arrow writer can send them as
//! `Binary`, which displays as hex. They are converted to `Utf8` here, at
//! any depth (inside arrays, maps, tuples and dictionaries), replacing
//! invalid UTF-8 rather than failing the query.

use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, AsArray, FixedSizeBinaryArray, LargeStringArray, ListArray, MapArray,
    StringArray, StructArray,
};
use arrow::datatypes::{DataType, FieldRef, Fields, SchemaRef};
use arrow::error::ArrowError;
use arrow::record_batch::{RecordBatch, RecordBatchOptions};

/// Whether `data_type` holds bytes that should become text.
pub(crate) fn has_bytes(data_type: &DataType) -> bool {
    match data_type {
        DataType::Binary | DataType::LargeBinary | DataType::FixedSizeBinary(_) => true,
        DataType::List(f) | DataType::LargeList(f) | DataType::Map(f, _) => {
            has_bytes(f.data_type())
        }
        DataType::Struct(fields) => fields.iter().any(|f| has_bytes(f.data_type())),
        DataType::Dictionary(_, value) => has_bytes(value),
        _ => false,
    }
}

/// `data_type` with every byte type replaced by its text type.
pub(crate) fn text_type(data_type: &DataType) -> DataType {
    match data_type {
        DataType::Binary | DataType::FixedSizeBinary(_) => DataType::Utf8,
        DataType::LargeBinary => DataType::LargeUtf8,
        DataType::List(f) => DataType::List(text_field(f)),
        DataType::LargeList(f) => DataType::LargeList(text_field(f)),
        DataType::Map(f, sorted) => DataType::Map(text_field(f), *sorted),
        DataType::Struct(fields) => DataType::Struct(text_fields(fields)),
        DataType::Dictionary(key, value) => {
            DataType::Dictionary(key.clone(), Box::new(text_type(value)))
        }
        other => other.clone(),
    }
}

fn text_field(field: &FieldRef) -> FieldRef {
    if has_bytes(field.data_type()) {
        Arc::new(
            field
                .as_ref()
                .clone()
                .with_data_type(text_type(field.data_type())),
        )
    } else {
        field.clone()
    }
}

fn text_fields(fields: &Fields) -> Fields {
    fields.iter().map(text_field).collect()
}

#[cfg(test)]
fn text_schema(schema: &arrow::datatypes::Schema) -> arrow::datatypes::Schema {
    arrow::datatypes::Schema::new_with_metadata(
        text_fields(schema.fields()),
        schema.metadata().clone(),
    )
}

/// Convert a batch's byte columns to text and give it `schema`, whose
/// fields must be [`text_schema`] of the batch's (plus metadata).
pub(crate) fn to_text(batch: &RecordBatch, schema: &SchemaRef) -> Result<RecordBatch, ArrowError> {
    let columns = batch
        .columns()
        .iter()
        .map(|column| to_text_array(column))
        .collect::<Result<Vec<_>, _>>()?;
    RecordBatch::try_new_with_options(
        schema.clone(),
        columns,
        &RecordBatchOptions::new().with_row_count(Some(batch.num_rows())),
    )
}

fn to_text_array(array: &ArrayRef) -> Result<ArrayRef, ArrowError> {
    if !has_bytes(array.data_type()) {
        return Ok(array.clone());
    }
    Ok(match array.data_type() {
        DataType::Binary => {
            let (offsets, values, nulls) = array.as_binary::<i32>().clone().into_parts();
            match StringArray::try_new(offsets, values, nulls) {
                Ok(text) => Arc::new(text),
                Err(_) => Arc::new(
                    array
                        .as_binary::<i32>()
                        .iter()
                        .map(|v| v.map(String::from_utf8_lossy))
                        .collect::<StringArray>(),
                ),
            }
        }
        DataType::LargeBinary => {
            let (offsets, values, nulls) = array.as_binary::<i64>().clone().into_parts();
            match LargeStringArray::try_new(offsets, values, nulls) {
                Ok(text) => Arc::new(text),
                Err(_) => Arc::new(
                    array
                        .as_binary::<i64>()
                        .iter()
                        .map(|v| v.map(String::from_utf8_lossy))
                        .collect::<LargeStringArray>(),
                ),
            }
        }
        DataType::FixedSizeBinary(_) => {
            let fixed: &FixedSizeBinaryArray = array.as_fixed_size_binary();
            // FixedString pads with zero bytes; show the text without them.
            Arc::new(
                fixed
                    .iter()
                    .map(|v| {
                        v.map(|bytes| {
                            let end = bytes.iter().rposition(|&b| b != 0).map_or(0, |n| n + 1);
                            String::from_utf8_lossy(&bytes[..end])
                        })
                    })
                    .collect::<StringArray>(),
            )
        }
        DataType::List(field) => {
            let list = array.as_list::<i32>();
            Arc::new(ListArray::try_new(
                text_field(field),
                list.offsets().clone(),
                to_text_array(list.values())?,
                list.nulls().cloned(),
            )?)
        }
        DataType::LargeList(field) => {
            let list = array.as_list::<i64>();
            Arc::new(arrow::array::LargeListArray::try_new(
                text_field(field),
                list.offsets().clone(),
                to_text_array(list.values())?,
                list.nulls().cloned(),
            )?)
        }
        DataType::Struct(_) => {
            let (fields, columns, nulls) = array.as_struct().clone().into_parts();
            let columns = columns
                .iter()
                .map(to_text_array)
                .collect::<Result<Vec<_>, _>>()?;
            Arc::new(StructArray::try_new(text_fields(&fields), columns, nulls)?)
        }
        DataType::Map(field, sorted) => {
            let map: &MapArray = array.as_map();
            let entries = to_text_array(&(Arc::new(map.entries().clone()) as ArrayRef))?;
            Arc::new(MapArray::try_new(
                text_field(field),
                map.offsets().clone(),
                entries.as_struct().clone(),
                map.nulls().cloned(),
                *sorted,
            )?)
        }
        DataType::Dictionary(..) => {
            let dictionary = array.as_any_dictionary();
            dictionary.with_values(to_text_array(dictionary.values())?)
        }
        _ => array.clone(),
    })
}

#[cfg(test)]
mod tests {
    use arrow::array::{BinaryArray, BinaryBuilder, DictionaryArray, Int32Array, ListBuilder};
    use arrow::datatypes::{Field, Int8Type, Schema};

    use super::*;

    fn convert(array: ArrayRef) -> ArrayRef {
        let field = Field::new("c", array.data_type().clone(), true);
        let schema = Arc::new(text_schema(&Schema::new(vec![field.clone()])));
        let batch = RecordBatch::try_new(Arc::new(Schema::new(vec![field])), vec![array]).unwrap();
        to_text(&batch, &schema).unwrap().column(0).clone()
    }

    #[test]
    fn turns_bytes_into_text() {
        let array: ArrayRef = Arc::new(BinaryArray::from(vec![
            Some(b"hello".as_ref()),
            None,
            Some("한글".as_bytes()),
        ]));
        let text = convert(array);
        let text = text.as_string::<i32>();
        assert_eq!(text.value(0), "hello");
        assert!(text.is_null(1));
        assert_eq!(text.value(2), "한글");
    }

    #[test]
    fn replaces_invalid_utf8() {
        let array: ArrayRef = Arc::new(BinaryArray::from(vec![b"ok".as_ref(), &[0xff, b'x']]));
        let text = convert(array);
        assert_eq!(text.as_string::<i32>().value(1), "\u{fffd}x");
    }

    #[test]
    fn trims_fixed_string_padding() {
        let array: ArrayRef = Arc::new(
            FixedSizeBinaryArray::try_from_iter([b"ab\0\0".as_ref(), b"abcd"].into_iter()).unwrap(),
        );
        let text = convert(array);
        assert_eq!(text.as_string::<i32>().value(0), "ab");
        assert_eq!(text.as_string::<i32>().value(1), "abcd");
    }

    #[test]
    fn converts_nested_and_dictionary_values() {
        let mut builder = ListBuilder::new(BinaryBuilder::new());
        builder.values().append_value(b"a");
        builder.values().append_value(b"b");
        builder.append(true);
        builder.append(false);
        let list = convert(Arc::new(builder.finish()));
        assert_eq!(
            list.data_type(),
            &DataType::List(Arc::new(Field::new("item", DataType::Utf8, true)))
        );
        assert_eq!(
            list.as_list::<i32>().value(0).as_string::<i32>().value(1),
            "b"
        );

        let dictionary = DictionaryArray::<Int8Type>::try_new(
            vec![1i8, 0, 1].into(),
            Arc::new(BinaryArray::from(vec![b"x".as_ref(), b"y"])),
        )
        .unwrap();
        let converted = convert(Arc::new(dictionary));
        assert_eq!(
            converted.data_type(),
            &DataType::Dictionary(Box::new(DataType::Int8), Box::new(DataType::Utf8))
        );

        let ints: ArrayRef = Arc::new(Int32Array::from(vec![1]));
        assert!(Arc::ptr_eq(&convert(ints.clone()), &ints));
    }
}
