/*
 * Parseable Server (C) 2022 - 2024 Parseable, Inc.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of the
 * License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU Affero General Public License for more details.
 *
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 *
 *
 */

//! example function for concat recordbatch(may not work)
//! ```rust
//! # use arrow::record_batch::RecordBatch;
//! # use arrow::error::Result;
//!
//! fn concat_batches(batch1: RecordBatch, batch2: RecordBatch) -> Result<RecordBatch> {
//!     let schema = batch1.schema();
//!     let columns = schema
//!         .fields()
//!         .iter()
//!         .enumerate()
//!         .map(|(i, _)| -> Result<_> {
//!             let array1 = batch1.column(i);
//!             let array2 = batch2.column(i);
//!             let array = arrow::compute::concat(&[array1.as_ref(), array2.as_ref()])?;
//!             Ok(array)
//!         })
//!         .collect::<Result<Vec<_>>>()?;
//!
//!     RecordBatch::try_new(schema.clone(), columns)
//! }
//! ```

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use arrow_array::{
    Array, ArrayRef, RecordBatch, StringArray, TimestampMillisecondArray, UInt64Array,
};
use arrow_schema::{ArrowError, DataType, Field, Schema, TimeUnit};
use arrow_select::take::take;
use chrono::{DateTime, Utc};
use itertools::Itertools;

pub mod batch_adapter;
pub mod flight;

use anyhow::Result;
pub use batch_adapter::adapt_batch;
use serde_json::{Map, Value};

use crate::event::DEFAULT_TIMESTAMP_KEY;

/// Replaces columns in a record batch with new arrays.
///
/// # Arguments
///
/// * `schema` - The schema of the record batch.
/// * `batch` - The record batch to modify.
/// * `indexes` - The indexes of the columns to replace.
/// * `arrays` - The new arrays to replace the columns with.
///
/// # Returns
///
/// The modified record batch with the columns replaced.
pub fn replace_columns(
    schema: Arc<Schema>,
    batch: &RecordBatch,
    indexes: &[usize],
    arrays: &[Arc<dyn Array + 'static>],
) -> RecordBatch {
    let mut batch_arrays = batch.columns().iter().map(Arc::clone).collect_vec();
    for (&index, arr) in indexes.iter().zip(arrays.iter()) {
        batch_arrays[index] = Arc::clone(arr);
    }
    RecordBatch::try_new(schema, batch_arrays).unwrap()
}

/// Converts a slice of record batches to JSON.
///
/// # Arguments
///
/// * `records` - The record batches to convert.
///
/// # Returns
/// * Result<Vec<Map<String, Value>>>
///
/// A vector of JSON objects representing the record batches.
pub fn record_batches_to_json(records: &[RecordBatch]) -> Result<Vec<Map<String, Value>>> {
    let buf = vec![];
    let mut writer = arrow_json::ArrayWriter::new(buf);
    for record in records {
        writer.write(record)?;
    }
    writer.finish()?;

    let buf = writer.into_inner();

    let json_rows: Vec<Map<String, Value>> =
        serde_json::from_reader(buf.as_slice()).unwrap_or_default();

    Ok(json_rows)
}

/// Retrieves a field from a slice of fields by name.
///
/// # Arguments
///
/// * `fields` - The slice of fields to search.
/// * `name` - The name of the field to retrieve.
///
/// # Returns
///
/// An optional reference to the field if found, or `None` if not found.
pub fn get_field<'a>(
    fields: &'a [impl AsRef<arrow_schema::Field>],
    name: &str,
) -> Option<&'a arrow_schema::Field> {
    fields
        .iter()
        .map(|x| x.as_ref())
        .find(|field| field.name() == name)
}

/// Constructs an array of the current timestamp.
///
/// # Arguments
///
/// * `size` - The number of rows for which timestamp values are to be added.
///
/// # Returns
///
/// A column in arrow, containing the current timestamp in millis.
pub fn get_timestamp_array(p_timestamp: DateTime<Utc>, size: usize) -> TimestampMillisecondArray {
    TimestampMillisecondArray::from_value(p_timestamp.timestamp_millis(), size)
}

pub fn add_parseable_fields(
    rb: RecordBatch,
    p_timestamp: DateTime<Utc>,
    p_custom_fields: &HashMap<String, String>,
) -> Result<RecordBatch, ArrowError> {
    // Return Result for proper error handling

    // Add custom fields in sorted order
    let mut sorted_keys: Vec<&String> = p_custom_fields.keys().collect();
    sorted_keys.sort();

    let schema = rb.schema();
    let row_count = rb.num_rows();

    let mut fields = schema
        .fields()
        .iter()
        .map(|f| f.as_ref().clone())
        .collect_vec();
    let mut field_names: HashSet<String> = fields.iter().map(|f| f.name().to_string()).collect();

    fields.insert(
        0,
        Field::new(
            DEFAULT_TIMESTAMP_KEY,
            DataType::Timestamp(TimeUnit::Millisecond, None),
            true,
        ),
    );
    let mut columns = rb.columns().iter().map(Arc::clone).collect_vec();
    columns.insert(
        0,
        Arc::new(get_timestamp_array(p_timestamp, row_count)) as ArrayRef,
    );

    //ignore the duplicate fields, no need to add them again
    for key in sorted_keys {
        if !field_names.contains(key) {
            fields.push(Field::new(key, DataType::Utf8, true));
            field_names.insert(key.to_string());

            let value = p_custom_fields.get(key).unwrap();
            columns.push(Arc::new(StringArray::from_iter_values(
                std::iter::repeat(value).take(row_count),
            )) as ArrayRef);
        }
    }

    // Create the new schema and batch
    let new_schema = Arc::new(Schema::new(fields));
    RecordBatch::try_new(new_schema, columns)
}

pub fn reverse(rb: &RecordBatch) -> RecordBatch {
    let indices = UInt64Array::from_iter_values((0..rb.num_rows()).rev().map(|x| x as u64));
    let arrays = rb
        .columns()
        .iter()
        .map(|col| take(&col, &indices, None).unwrap())
        .collect();
    RecordBatch::try_new(rb.schema(), arrays).unwrap()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_array::{Array, Int32Array, RecordBatch};
    use arrow_schema::{DataType, Field, Schema};

    use super::*;

    #[test]
    fn check_replace() {
        let schema = Schema::new(vec![
            Field::new("a", DataType::Int32, false),
            Field::new("b", DataType::Int32, false),
            Field::new("c", DataType::Int32, false),
        ]);

        let schema_ref = Arc::new(schema);

        let rb = RecordBatch::try_new(
            schema_ref.clone(),
            vec![
                Arc::new(Int32Array::from_value(0, 3)),
                Arc::new(Int32Array::from_value(0, 3)),
                Arc::new(Int32Array::from_value(0, 3)),
            ],
        )
        .unwrap();

        let arr: Arc<dyn Array + 'static> = Arc::new(Int32Array::from_value(0, 3));

        let new_rb = replace_columns(schema_ref.clone(), &rb, &[2], &[arr]);

        assert_eq!(new_rb.schema(), schema_ref);
        assert_eq!(new_rb.num_columns(), 3);
        assert_eq!(new_rb.num_rows(), 3)
    }

    #[test]
    fn check_empty_json_to_record_batches() {
        let r = RecordBatch::new_empty(Arc::new(Schema::empty()));
        let rb = vec![r];
        let batches = record_batches_to_json(&rb).unwrap();
        assert_eq!(batches, vec![]);
    }

    #[test]
    fn test_timestamp_array_has_correct_size_and_value() {
        let size = 5;
        let now = Utc::now();

        let array = get_timestamp_array(now, size);

        assert_eq!(array.len(), size);
        for i in 0..size {
            assert!(array.value(i) >= now.timestamp_millis());
        }
    }

    #[test]
    fn test_timestamp_array_with_zero_size() {
        let array = get_timestamp_array(Utc::now(), 0);

        assert_eq!(array.len(), 0);
        assert!(array.is_empty());
    }
}
