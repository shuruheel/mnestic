/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Feature-gated Parquet and Arrow IPC copy-in boundary.
//!
//! This module deliberately exposes only mnestic-owned option/report types.
//! Arrow types stay behind the `columnar-io` feature and never become part of
//! the public semver surface.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow_array::types::{
    Int16Type, Int32Type, Int64Type, Int8Type, UInt16Type, UInt32Type, UInt64Type, UInt8Type,
};
use arrow_array::{
    Array, BinaryArray, BinaryViewArray, BooleanArray, DictionaryArray, FixedSizeBinaryArray,
    FixedSizeListArray, Float16Array, Float32Array, Float64Array, Int16Array, Int32Array,
    Int64Array, Int8Array, LargeBinaryArray, LargeListArray, LargeStringArray, ListArray,
    RecordBatch, RunArray, StringArray, StringViewArray, UInt16Array, UInt32Array, UInt64Array,
    UInt8Array,
};
use arrow_ipc::reader::{FileReader, StreamReader};
use arrow_schema::extension::{Json as ArrowJson, Uuid as ArrowUuid};
use arrow_schema::{DataType, Field, FieldRef, Schema, SchemaRef};
use itertools::Itertools;
use miette::{Diagnostic, Result};
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};
use parquet::arrow::ProjectionMask;
use parquet::basic::{ConvertedType, LogicalType};
use parquet::schema::types::SchemaDescriptor;
use serde_json::Value as JsonValue;
use smartstring::SmartString;
use thiserror::Error;
use uuid::Uuid;

use crate::data::functions::current_validity;
use crate::data::relation::{ColType, ColumnDef, NullableColType, VecElementType};
use crate::data::value::{DataValue, JsonData, UuidWrapper};
use crate::runtime::db::{stranded_index_names, warn_if_indexes_stranded, write_import_tuple, Db};
use crate::runtime::relation::{AccessLevel, InsufficientAccessLevel};
use crate::storage::Storage;
use crate::Num;

const DEFAULT_BATCH_ROWS: usize = 8_192;
const DEFAULT_MAX_NESTING_DEPTH: usize = 16;
const CHECK_INTERVAL_ROWS: u64 = 4_096;

/// File encoding accepted by [`Db::import_columnar_file`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnarFileFormat {
    Parquet,
    ArrowIpcFile,
    ArrowIpcStream,
}

/// Resource, mapping, and format controls for one atomic columnar import.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct ColumnarImportOptions {
    format: ColumnarFileFormat,
    columns: BTreeMap<String, String>,
    batch_rows: usize,
    timeout: Option<Duration>,
    max_source_bytes: Option<u64>,
    max_rows: Option<u64>,
    max_decoded_batch_bytes: Option<usize>,
    max_value_bytes: Option<usize>,
    max_nesting_depth: usize,
}

impl ColumnarImportOptions {
    pub fn new(format: ColumnarFileFormat) -> Self {
        Self {
            format,
            columns: BTreeMap::new(),
            batch_rows: DEFAULT_BATCH_ROWS,
            timeout: None,
            max_source_bytes: None,
            max_rows: None,
            max_decoded_batch_bytes: None,
            max_value_bytes: None,
            max_nesting_depth: DEFAULT_MAX_NESTING_DEPTH,
        }
    }

    pub fn with_columns(mut self, columns: BTreeMap<String, String>) -> Self {
        self.columns = columns;
        self
    }

    pub fn with_batch_rows(mut self, batch_rows: usize) -> Self {
        self.batch_rows = batch_rows;
        self
    }

    pub fn with_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_max_source_bytes(mut self, max_source_bytes: Option<u64>) -> Self {
        self.max_source_bytes = max_source_bytes;
        self
    }

    pub fn with_max_rows(mut self, max_rows: Option<u64>) -> Self {
        self.max_rows = max_rows;
        self
    }

    pub fn with_max_decoded_batch_bytes(mut self, max_decoded_batch_bytes: Option<usize>) -> Self {
        self.max_decoded_batch_bytes = max_decoded_batch_bytes;
        self
    }

    pub fn with_max_value_bytes(mut self, max_value_bytes: Option<usize>) -> Self {
        self.max_value_bytes = max_value_bytes;
        self
    }

    pub fn with_max_nesting_depth(mut self, max_nesting_depth: usize) -> Self {
        self.max_nesting_depth = max_nesting_depth;
        self
    }

    pub fn format(&self) -> ColumnarFileFormat {
        self.format
    }

    pub fn columns(&self) -> &BTreeMap<String, String> {
        &self.columns
    }

    pub fn batch_rows(&self) -> usize {
        self.batch_rows
    }

    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    pub fn max_source_bytes(&self) -> Option<u64> {
        self.max_source_bytes
    }

    pub fn max_rows(&self) -> Option<u64> {
        self.max_rows
    }

    pub fn max_decoded_batch_bytes(&self) -> Option<usize> {
        self.max_decoded_batch_bytes
    }

    pub fn max_value_bytes(&self) -> Option<usize> {
        self.max_value_bytes
    }

    pub fn max_nesting_depth(&self) -> usize {
        self.max_nesting_depth
    }

    fn validate(&self) -> Result<()> {
        if self.batch_rows == 0 {
            return Err(ColumnarError::InvalidOptions(
                "batch_rows must be greater than zero".to_string(),
            )
            .into());
        }
        if self.timeout.is_some_and(|v| v.is_zero()) {
            return Err(ColumnarError::InvalidOptions(
                "timeout must be greater than zero when set".to_string(),
            )
            .into());
        }
        if self.max_source_bytes == Some(0)
            || self.max_rows == Some(0)
            || self.max_decoded_batch_bytes == Some(0)
            || self.max_value_bytes == Some(0)
            || self.max_nesting_depth == 0
        {
            return Err(ColumnarError::InvalidOptions(
                "row, byte, and nesting limits must be greater than zero when set".to_string(),
            )
            .into());
        }
        Ok(())
    }
}

/// Summary returned only after the whole source commits successfully.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnarImportReport {
    pub rows_processed: u64,
    pub batches_processed: u64,
    pub search_indexes_requiring_rebuild: Vec<String>,
}

#[derive(Debug, Error, Diagnostic)]
enum ColumnarError {
    #[error("invalid columnar import options: {0}")]
    #[diagnostic(code(columnar::invalid_options))]
    InvalidOptions(String),
    #[error("cannot open columnar source: {0}")]
    #[diagnostic(code(columnar::open))]
    Open(String),
    #[error("columnar source is not a regular file")]
    #[diagnostic(code(columnar::not_a_file))]
    NotAFile,
    #[error("columnar source is {actual} bytes, exceeding max_source_bytes={limit}")]
    #[diagnostic(code(columnar::source_limit))]
    SourceLimit { actual: u64, limit: u64 },
    #[error("invalid columnar schema: {0}")]
    #[diagnostic(code(columnar::schema))]
    Schema(String),
    #[error("unsupported Arrow type for source field '{field}': {data_type}")]
    #[diagnostic(code(columnar::unsupported_type))]
    UnsupportedType { field: String, data_type: String },
    #[error("failed to decode or validate columnar data: {0}")]
    #[diagnostic(code(columnar::decode))]
    Decode(String),
    #[error("decoded batch uses {actual} bytes, exceeding max_decoded_batch_bytes={limit}")]
    #[diagnostic(code(columnar::decoded_batch_limit))]
    DecodedBatchLimit { actual: usize, limit: usize },
    #[error("columnar source contains more than max_rows={limit} rows")]
    #[diagnostic(code(columnar::row_limit))]
    RowLimit { limit: u64 },
    #[error("source value uses {actual} bytes, exceeding max_value_bytes={limit}")]
    #[diagnostic(code(columnar::value_limit))]
    ValueLimit { actual: usize, limit: usize },
    #[error("columnar import exceeded its timeout")]
    #[diagnostic(code(columnar::timeout))]
    Timeout,
    #[error(
        "columnar conversion failed for relation '{relation}', target '{target}', source \
         '{source_column}', row {row}, Arrow type {source_type}, target type {target_type}: {reason}"
    )]
    #[diagnostic(code(columnar::conversion))]
    Conversion {
        relation: String,
        target: String,
        source_column: String,
        row: u64,
        source_type: String,
        target_type: String,
        reason: String,
    },
}

#[derive(Debug, Error)]
enum ScalarError {
    #[error("{0}")]
    Conversion(String),
    #[error("source value uses {actual} bytes, exceeding max_value_bytes={limit}")]
    ValueLimit { actual: usize, limit: usize },
}

impl From<String> for ScalarError {
    fn from(value: String) -> Self {
        Self::Conversion(value)
    }
}

enum ColumnarReader {
    Parquet(ParquetRecordBatchReader),
    ArrowIpcFile(FileReader<File>),
    ArrowIpcStream(StreamReader<File>),
}

enum ColumnarSource {
    Parquet(ParquetRecordBatchReaderBuilder<File>),
    Ready(ColumnarReader),
}

impl ColumnarSource {
    fn build(
        self,
        options: &ColumnarImportOptions,
        bindings: &mut [ColumnBinding],
    ) -> Result<ColumnarReader> {
        match self {
            Self::Parquet(builder) => {
                let source_indices = bindings
                    .iter()
                    .map(|binding| binding.source_index)
                    .collect::<BTreeSet<_>>();
                let projected_positions = source_indices
                    .iter()
                    .enumerate()
                    .map(|(projected, source)| (*source, projected))
                    .collect::<BTreeMap<_, _>>();
                for binding in bindings {
                    binding.source_index = projected_positions[&binding.source_index];
                }
                let projection = ProjectionMask::roots(builder.parquet_schema(), source_indices);
                let reader = builder
                    .with_batch_size(options.batch_rows)
                    .with_projection(projection)
                    .build()
                    .map_err(|e| ColumnarError::Decode(e.to_string()))?;
                Ok(ColumnarReader::Parquet(reader))
            }
            Self::Ready(reader) => Ok(reader),
        }
    }
}

impl ColumnarReader {
    fn next_batch(&mut self) -> Result<Option<RecordBatch>> {
        let next = match self {
            Self::Parquet(reader) => reader.next().map(|v| v.map_err(|e| e.to_string())),
            Self::ArrowIpcFile(reader) => reader.next().map(|v| v.map_err(|e| e.to_string())),
            Self::ArrowIpcStream(reader) => reader.next().map(|v| v.map_err(|e| e.to_string())),
        };
        next.transpose()
            .map_err(|e| ColumnarError::Decode(e).into())
    }
}

#[derive(Clone)]
struct ColumnBinding {
    source_index: usize,
    source_field: FieldRef,
    target: ColumnDef,
}

impl<'s, S: Storage<'s>> Db<S> {
    /// Atomically copy one local Parquet or Arrow IPC file into an existing
    /// non-`TxTime` stored relation.
    pub fn import_columnar_file(
        &'s self,
        relation: &str,
        path: impl AsRef<Path>,
        options: &ColumnarImportOptions,
    ) -> Result<ColumnarImportReport> {
        options.validate()?;
        let started = Instant::now();
        let deadline = options.timeout.and_then(|v| started.checked_add(v));
        check_deadline(deadline)?;

        let (schema, source) = open_reader(path.as_ref(), options)?;
        check_deadline(deadline)?;

        if relation.contains(':') {
            return Err(ColumnarError::Schema(format!(
                "cannot import into index relation '{relation}'"
            ))
            .into());
        }

        let relation_name = SmartString::from(relation);
        let locks = self.obtain_relation_locks(std::iter::once(&relation_name));
        let _guards = locks.iter().map(|lock| lock.read().unwrap()).collect_vec();
        let mut tx = self.transact_write()?;
        let handle = tx.get_relation(relation, false)?;

        if handle.has_txtime() {
            return Err(ColumnarError::Schema(format!(
                "relation '{relation}' contains an engine-assigned TxTime column; Batch A rejects TxTime targets"
            ))
            .into());
        }
        if handle.access_level < AccessLevel::Protected {
            return Err(InsufficientAccessLevel(
                handle.name.to_string(),
                "columnar data import".to_string(),
                handle.access_level,
            )
            .into());
        }

        let mut bindings = resolve_bindings(
            relation,
            &schema,
            &handle.metadata.keys,
            &handle.metadata.non_keys,
            options,
        )?;
        let mut reader = source.build(options, &mut bindings)?;
        let stale_indexes = stranded_index_names(&handle);
        tx.mark_dirty(&handle);
        let cur_vld = current_validity();

        let mut rows_processed = 0_u64;
        let mut batches_processed = 0_u64;
        while let Some(batch) = reader.next_batch()? {
            check_deadline(deadline)?;
            batches_processed = batches_processed.checked_add(1).ok_or_else(|| {
                ColumnarError::Decode("record-batch counter overflow".to_string())
            })?;
            validate_batch(&batch)?;

            let batch_bytes = batch.get_array_memory_size();
            if let Some(limit) = options.max_decoded_batch_bytes {
                if batch_bytes > limit {
                    return Err(ColumnarError::DecodedBatchLimit {
                        actual: batch_bytes,
                        limit,
                    }
                    .into());
                }
            }
            let next_total = rows_processed
                .checked_add(batch.num_rows() as u64)
                .ok_or(ColumnarError::RowLimit { limit: u64::MAX })?;
            if let Some(limit) = options.max_rows {
                if next_total > limit {
                    return Err(ColumnarError::RowLimit { limit }.into());
                }
            }

            for offset in (0..batch.num_rows()).step_by(options.batch_rows) {
                let len = options.batch_rows.min(batch.num_rows() - offset);
                let slice = batch.slice(offset, len);
                for row in 0..slice.num_rows() {
                    if rows_processed % CHECK_INTERVAL_ROWS == 0 {
                        check_deadline(deadline)?;
                    }
                    let absolute_row = rows_processed;
                    let mut tuple = Vec::with_capacity(bindings.len());
                    for binding in &bindings {
                        let source_type = binding.source_field.data_type().to_string();
                        let value = scalar_to_value(
                            slice.column(binding.source_index).as_ref(),
                            binding.source_field.as_ref(),
                            row,
                            options.max_value_bytes,
                        )
                        .map_err(|error| match error {
                            ScalarError::Conversion(reason) => conversion_error(
                                relation,
                                binding,
                                absolute_row,
                                &source_type,
                                reason,
                            ),
                            ScalarError::ValueLimit { actual, limit } => {
                                ColumnarError::ValueLimit { actual, limit }
                            }
                        })?;
                        precheck_conversion(
                            &value,
                            binding.source_field.data_type(),
                            &binding.target.typing,
                        )
                        .map_err(|e| {
                            conversion_error(relation, binding, absolute_row, &source_type, e)
                        })?;
                        let coerced =
                            binding.target.typing.coerce(value, cur_vld).map_err(|e| {
                                conversion_error(
                                    relation,
                                    binding,
                                    absolute_row,
                                    &source_type,
                                    e.to_string(),
                                )
                            })?;
                        tuple.push(coerced);
                    }
                    write_import_tuple(&mut tx, &handle, tuple, false)?;
                    rows_processed += 1;
                }
            }
            debug_assert_eq!(rows_processed, next_total);
        }

        check_deadline(deadline)?;
        self.commit_tx_with_test_hook(&mut tx)?;
        warn_if_indexes_stranded(&handle, relation, "columnar import into");
        self.flush_warnings();
        Ok(ColumnarImportReport {
            rows_processed,
            batches_processed,
            search_indexes_requiring_rebuild: stale_indexes,
        })
    }
}

fn conversion_error(
    relation: &str,
    binding: &ColumnBinding,
    row: u64,
    source_type: &str,
    reason: String,
) -> ColumnarError {
    ColumnarError::Conversion {
        relation: relation.to_string(),
        target: binding.target.name.to_string(),
        source_column: binding.source_field.name().to_string(),
        row,
        source_type: source_type.to_string(),
        target_type: binding.target.typing.to_string(),
        reason,
    }
}

fn check_deadline(deadline: Option<Instant>) -> Result<()> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(ColumnarError::Timeout.into());
    }
    Ok(())
}

fn open_reader(
    path: &Path,
    options: &ColumnarImportOptions,
) -> Result<(SchemaRef, ColumnarSource)> {
    let file = File::open(path).map_err(|e| ColumnarError::Open(e.to_string()))?;
    let metadata = file
        .metadata()
        .map_err(|e| ColumnarError::Open(e.to_string()))?;
    if !metadata.is_file() {
        return Err(ColumnarError::NotAFile.into());
    }
    if let Some(limit) = options.max_source_bytes {
        if metadata.len() > limit {
            return Err(ColumnarError::SourceLimit {
                actual: metadata.len(),
                limit,
            }
            .into());
        }
    }

    match options.format {
        ColumnarFileFormat::Parquet => {
            let builder = ParquetRecordBatchReaderBuilder::try_new(file)
                .map_err(|e| ColumnarError::Decode(e.to_string()))?;
            let schema = authoritative_parquet_schema(builder.schema(), builder.parquet_schema())?;
            Ok((schema, ColumnarSource::Parquet(builder)))
        }
        ColumnarFileFormat::ArrowIpcFile => {
            let reader = FileReader::try_new(file, None)
                .map_err(|e| ColumnarError::Decode(e.to_string()))?;
            let schema = reader.schema();
            Ok((
                schema,
                ColumnarSource::Ready(ColumnarReader::ArrowIpcFile(reader)),
            ))
        }
        ColumnarFileFormat::ArrowIpcStream => {
            let reader = StreamReader::try_new(file, None)
                .map_err(|e| ColumnarError::Decode(e.to_string()))?;
            let schema = reader.schema();
            Ok((
                schema,
                ColumnarSource::Ready(ColumnarReader::ArrowIpcStream(reader)),
            ))
        }
    }
}

/// Attach UUID/JSON meaning from the Parquet schema descriptor itself. The
/// Arrow conversion currently attaches the same canonical extension metadata,
/// but that is a crate-version behavior rather than the file's authority.
fn authoritative_parquet_schema(
    arrow_schema: &SchemaRef,
    parquet_schema: &SchemaDescriptor,
) -> Result<SchemaRef> {
    let parquet_fields = parquet_schema.root_schema().get_fields();
    if parquet_fields.len() != arrow_schema.fields().len() {
        return Err(ColumnarError::Schema(format!(
            "Parquet root has {} fields but its Arrow schema has {}",
            parquet_fields.len(),
            arrow_schema.fields().len()
        ))
        .into());
    }

    let mut fields = arrow_schema
        .fields()
        .iter()
        .map(|field| field.as_ref().clone())
        .collect_vec();
    for (index, parquet_field) in parquet_fields.iter().enumerate() {
        if !parquet_field.is_primitive() {
            continue;
        }
        let info = parquet_field.get_basic_info();
        match info.logical_type_ref() {
            Some(LogicalType::Uuid) => fields[index]
                .try_with_extension_type(ArrowUuid)
                .map_err(|error| ColumnarError::Schema(error.to_string()))?,
            Some(LogicalType::Json) => fields[index]
                .try_with_extension_type(ArrowJson::default())
                .map_err(|error| ColumnarError::Schema(error.to_string()))?,
            Some(LogicalType::String | LogicalType::Integer(_) | LogicalType::Float16) => {}
            Some(other) => {
                return Err(ColumnarError::UnsupportedType {
                    field: parquet_field.name().to_string(),
                    data_type: format!("Parquet logical type {other:?}"),
                }
                .into())
            }
            None => match info.converted_type() {
                ConvertedType::JSON => fields[index]
                    .try_with_extension_type(ArrowJson::default())
                    .map_err(|error| ColumnarError::Schema(error.to_string()))?,
                ConvertedType::NONE
                | ConvertedType::UTF8
                | ConvertedType::UINT_8
                | ConvertedType::UINT_16
                | ConvertedType::UINT_32
                | ConvertedType::UINT_64
                | ConvertedType::INT_8
                | ConvertedType::INT_16
                | ConvertedType::INT_32
                | ConvertedType::INT_64 => {}
                other => {
                    return Err(ColumnarError::UnsupportedType {
                        field: parquet_field.name().to_string(),
                        data_type: format!("Parquet converted type {other:?}"),
                    }
                    .into())
                }
            },
        }
    }
    Ok(Arc::new(Schema::new_with_metadata(
        fields,
        arrow_schema.metadata().clone(),
    )))
}

fn resolve_bindings(
    relation: &str,
    schema: &SchemaRef,
    keys: &[ColumnDef],
    values: &[ColumnDef],
    options: &ColumnarImportOptions,
) -> Result<Vec<ColumnBinding>> {
    let targets: BTreeSet<&str> = keys
        .iter()
        .chain(values.iter())
        .map(|column| column.name.as_str())
        .collect();
    for target in options.columns.keys() {
        if !targets.contains(target.as_str()) {
            return Err(ColumnarError::Schema(format!(
                "column mapping names unknown target '{target}' for relation '{relation}'"
            ))
            .into());
        }
    }

    let mut bindings = Vec::with_capacity(targets.len());
    for target in keys.iter().chain(values.iter()) {
        let source = options
            .columns
            .get(target.name.as_str())
            .map(String::as_str)
            .unwrap_or(target.name.as_str());
        let matches = schema
            .fields()
            .iter()
            .enumerate()
            .filter(|(_, field)| field.name() == source)
            .collect_vec();
        if matches.len() != 1 {
            return Err(ColumnarError::Schema(format!(
                "source field '{source}' for target '{}' occurs {} times; exactly one is required",
                target.name,
                matches.len()
            ))
            .into());
        }
        let (source_index, field) = matches[0];
        validate_field(field.as_ref(), 1, options.max_nesting_depth)?;
        bindings.push(ColumnBinding {
            source_index,
            source_field: field.clone(),
            target: target.clone(),
        });
    }
    Ok(bindings)
}

fn validate_field(field: &Field, depth: usize, max_depth: usize) -> Result<()> {
    if depth > max_depth {
        return Err(ColumnarError::Schema(format!(
            "source field '{}' exceeds max_nesting_depth={max_depth}",
            field.name()
        ))
        .into());
    }
    if let Some(extension) = field.extension_type_name() {
        match extension {
            "arrow.uuid" => field
                .try_extension_type::<ArrowUuid>()
                .map(|_| ())
                .map_err(|e| ColumnarError::Schema(e.to_string()))?,
            "arrow.json" => field
                .try_extension_type::<ArrowJson>()
                .map(|_| ())
                .map_err(|e| ColumnarError::Schema(e.to_string()))?,
            other => {
                return Err(ColumnarError::UnsupportedType {
                    field: field.name().to_string(),
                    data_type: format!("unknown extension {other} over {}", field.data_type()),
                }
                .into())
            }
        }
    }

    match field.data_type() {
        DataType::Null
        | DataType::Boolean
        | DataType::Int8
        | DataType::Int16
        | DataType::Int32
        | DataType::Int64
        | DataType::UInt8
        | DataType::UInt16
        | DataType::UInt32
        | DataType::UInt64
        | DataType::Float16
        | DataType::Float32
        | DataType::Float64
        | DataType::Utf8
        | DataType::LargeUtf8
        | DataType::Utf8View
        | DataType::Binary
        | DataType::LargeBinary
        | DataType::BinaryView
        | DataType::FixedSizeBinary(_) => Ok(()),
        DataType::List(child) | DataType::LargeList(child) | DataType::FixedSizeList(child, _) => {
            validate_field(child, depth + 1, max_depth)
        }
        DataType::Dictionary(key, value) => {
            if !matches!(
                key.as_ref(),
                DataType::Int8
                    | DataType::Int16
                    | DataType::Int32
                    | DataType::Int64
                    | DataType::UInt8
                    | DataType::UInt16
                    | DataType::UInt32
                    | DataType::UInt64
            ) {
                return Err(ColumnarError::UnsupportedType {
                    field: field.name().to_string(),
                    data_type: field.data_type().to_string(),
                }
                .into());
            }
            let child = synthetic_child(field, value.as_ref().clone());
            validate_field(&child, depth + 1, max_depth)
        }
        DataType::RunEndEncoded(run_ends, values) => {
            if !matches!(
                run_ends.data_type(),
                DataType::Int16 | DataType::Int32 | DataType::Int64
            ) {
                return Err(ColumnarError::UnsupportedType {
                    field: field.name().to_string(),
                    data_type: field.data_type().to_string(),
                }
                .into());
            }
            validate_field(values, depth + 1, max_depth)
        }
        other => Err(ColumnarError::UnsupportedType {
            field: field.name().to_string(),
            data_type: other.to_string(),
        }
        .into()),
    }
}

fn validate_batch(batch: &RecordBatch) -> Result<()> {
    for (index, column) in batch.columns().iter().enumerate() {
        column.to_data().validate_full().map_err(|e| {
            ColumnarError::Decode(format!(
                "field '{}' failed full array validation: {e}",
                batch.schema().field(index).name()
            ))
        })?;
    }
    Ok(())
}

fn synthetic_child(parent: &Field, data_type: DataType) -> Field {
    Field::new(parent.name(), data_type, true).with_metadata(parent.metadata().clone())
}

fn scalar_to_value(
    array: &dyn Array,
    field: &Field,
    row: usize,
    max_value_bytes: Option<usize>,
) -> std::result::Result<DataValue, ScalarError> {
    if array.is_null(row) {
        return Ok(DataValue::Null);
    }

    if let Some(extension) = field.extension_type_name() {
        match extension {
            "arrow.uuid" => {
                field
                    .try_extension_type::<ArrowUuid>()
                    .map_err(|e| e.to_string())?;
                let bytes = downcast::<FixedSizeBinaryArray>(array)?.value(row);
                check_value_bytes(bytes.len(), max_value_bytes)?;
                let uuid = Uuid::from_slice(bytes).map_err(|e| e.to_string())?;
                return Ok(DataValue::Uuid(UuidWrapper(uuid)));
            }
            "arrow.json" => {
                field
                    .try_extension_type::<ArrowJson>()
                    .map_err(|e| e.to_string())?;
                let json = string_value(array, row, max_value_bytes)?;
                let parsed: JsonValue = serde_json::from_str(json).map_err(|e| e.to_string())?;
                return Ok(DataValue::Json(JsonData(parsed)));
            }
            other => return Err(format!("unsupported extension '{other}'").into()),
        }
    }

    macro_rules! signed {
        ($array:ty) => {
            Ok(DataValue::Num(Num::Int(
                downcast::<$array>(array)?.value(row) as i64,
            )))
        };
    }
    macro_rules! unsigned {
        ($array:ty) => {{
            let value = downcast::<$array>(array)?.value(row);
            let value = i64::try_from(value)
                .map_err(|_| format!("unsigned value {value} exceeds i64::MAX"))?;
            Ok(DataValue::Num(Num::Int(value)))
        }};
    }
    macro_rules! float {
        ($array:ty) => {
            Ok(DataValue::Num(Num::Float(
                downcast::<$array>(array)?.value(row) as f64,
            )))
        };
    }

    match array.data_type() {
        DataType::Null => Ok(DataValue::Null),
        DataType::Boolean => Ok(DataValue::Bool(downcast::<BooleanArray>(array)?.value(row))),
        DataType::Int8 => signed!(Int8Array),
        DataType::Int16 => signed!(Int16Array),
        DataType::Int32 => signed!(Int32Array),
        DataType::Int64 => signed!(Int64Array),
        DataType::UInt8 => unsigned!(UInt8Array),
        DataType::UInt16 => unsigned!(UInt16Array),
        DataType::UInt32 => unsigned!(UInt32Array),
        DataType::UInt64 => unsigned!(UInt64Array),
        DataType::Float16 => Ok(DataValue::Num(Num::Float(
            downcast::<Float16Array>(array)?.value(row).to_f64(),
        ))),
        DataType::Float32 => float!(Float32Array),
        DataType::Float64 => float!(Float64Array),
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => {
            Ok(DataValue::from(string_value(array, row, max_value_bytes)?))
        }
        DataType::Binary => {
            let value = downcast::<BinaryArray>(array)?.value(row);
            check_value_bytes(value.len(), max_value_bytes)?;
            Ok(DataValue::Bytes(value.to_vec()))
        }
        DataType::LargeBinary => {
            let value = downcast::<LargeBinaryArray>(array)?.value(row);
            check_value_bytes(value.len(), max_value_bytes)?;
            Ok(DataValue::Bytes(value.to_vec()))
        }
        DataType::BinaryView => {
            let value = downcast::<BinaryViewArray>(array)?.value(row);
            check_value_bytes(value.len(), max_value_bytes)?;
            Ok(DataValue::Bytes(value.to_vec()))
        }
        DataType::FixedSizeBinary(_) => {
            let value = downcast::<FixedSizeBinaryArray>(array)?.value(row);
            check_value_bytes(value.len(), max_value_bytes)?;
            Ok(DataValue::Bytes(value.to_vec()))
        }
        DataType::List(child) => {
            let values = downcast::<ListArray>(array)?.value(row);
            list_to_value(values.as_ref(), child, max_value_bytes)
        }
        DataType::LargeList(child) => {
            let values = downcast::<LargeListArray>(array)?.value(row);
            list_to_value(values.as_ref(), child, max_value_bytes)
        }
        DataType::FixedSizeList(child, _) => {
            let values = downcast::<FixedSizeListArray>(array)?.value(row);
            list_to_value(values.as_ref(), child, max_value_bytes)
        }
        DataType::Dictionary(key, value) => {
            let child = synthetic_child(field, value.as_ref().clone());
            macro_rules! dictionary {
                ($key_ty:ty) => {{
                    let dictionary = downcast::<DictionaryArray<$key_ty>>(array)?;
                    let index = dictionary
                        .key(row)
                        .ok_or_else(|| "null dictionary key".to_string())?;
                    scalar_to_value(dictionary.values().as_ref(), &child, index, max_value_bytes)
                }};
            }
            match key.as_ref() {
                DataType::Int8 => dictionary!(Int8Type),
                DataType::Int16 => dictionary!(Int16Type),
                DataType::Int32 => dictionary!(Int32Type),
                DataType::Int64 => dictionary!(Int64Type),
                DataType::UInt8 => dictionary!(UInt8Type),
                DataType::UInt16 => dictionary!(UInt16Type),
                DataType::UInt32 => dictionary!(UInt32Type),
                DataType::UInt64 => dictionary!(UInt64Type),
                other => Err(format!("unsupported dictionary key type {other}").into()),
            }
        }
        DataType::RunEndEncoded(run_ends, values) => {
            macro_rules! run_array {
                ($run_ty:ty) => {{
                    let runs = downcast::<RunArray<$run_ty>>(array)?;
                    let index = runs.get_physical_index(row);
                    scalar_to_value(runs.values().as_ref(), values, index, max_value_bytes)
                }};
            }
            match run_ends.data_type() {
                DataType::Int16 => run_array!(Int16Type),
                DataType::Int32 => run_array!(Int32Type),
                DataType::Int64 => run_array!(Int64Type),
                other => Err(format!("unsupported run-end type {other}").into()),
            }
        }
        other => Err(format!("unsupported Arrow type {other}").into()),
    }
}

fn downcast<T: 'static>(array: &dyn Array) -> std::result::Result<&T, ScalarError> {
    array.as_any().downcast_ref::<T>().ok_or_else(|| {
        ScalarError::Conversion(format!(
            "array payload does not match declared type {}",
            array.data_type()
        ))
    })
}

fn string_value(
    array: &dyn Array,
    row: usize,
    max_value_bytes: Option<usize>,
) -> std::result::Result<&str, ScalarError> {
    let value = match array.data_type() {
        DataType::Utf8 => downcast::<StringArray>(array)?.value(row),
        DataType::LargeUtf8 => downcast::<LargeStringArray>(array)?.value(row),
        DataType::Utf8View => downcast::<StringViewArray>(array)?.value(row),
        other => {
            return Err(format!("expected string storage for logical value, got {other}").into())
        }
    };
    check_value_bytes(value.len(), max_value_bytes)?;
    Ok(value)
}

fn list_to_value(
    values: &dyn Array,
    child: &FieldRef,
    max_value_bytes: Option<usize>,
) -> std::result::Result<DataValue, ScalarError> {
    let mut result = Vec::with_capacity(values.len());
    for index in 0..values.len() {
        result.push(scalar_to_value(
            values,
            child.as_ref(),
            index,
            max_value_bytes,
        )?);
    }
    Ok(DataValue::List(result))
}

fn check_value_bytes(actual: usize, limit: Option<usize>) -> std::result::Result<(), ScalarError> {
    if let Some(limit) = limit {
        if actual > limit {
            return Err(ScalarError::ValueLimit { actual, limit });
        }
    }
    Ok(())
}

fn precheck_conversion(
    value: &DataValue,
    source: &DataType,
    target: &NullableColType,
) -> std::result::Result<(), String> {
    if matches!(value, DataValue::Null) || matches!(target.coltype, ColType::Any) {
        return Ok(());
    }

    let source = unwrap_encoding(source);
    match (&target.coltype, value) {
        (ColType::Bool, DataValue::Bool(_))
        | (ColType::String, DataValue::Str(_))
        | (ColType::Bytes, DataValue::Bytes(_))
        | (ColType::Uuid, DataValue::Uuid(_) | DataValue::Str(_))
        | (ColType::Validity, DataValue::Validity(_) | DataValue::Str(_) | DataValue::List(_))
        | (ColType::Json, DataValue::Json(_)) => Ok(()),
        (ColType::Int, DataValue::Num(Num::Int(_))) => Ok(()),
        (ColType::Int, DataValue::Num(Num::Float(value))) => {
            if value.is_finite()
                && value.fract() == 0.0
                && *value >= i64::MIN as f64
                && *value < -(i64::MIN as f64)
            {
                Ok(())
            } else {
                Err(format!(
                    "float {value} cannot be represented losslessly as Int"
                ))
            }
        }
        (ColType::Float, DataValue::Num(Num::Float(_))) => Ok(()),
        (ColType::Float, DataValue::Num(Num::Int(value))) => {
            if (*value as f64) as i128 == *value as i128 {
                Ok(())
            } else {
                Err(format!(
                    "integer {value} cannot be represented losslessly as Float"
                ))
            }
        }
        (ColType::List { eltype, .. }, DataValue::List(values)) => {
            let child =
                child_type(source).ok_or_else(|| format!("source type {source} is not a list"))?;
            for value in values {
                precheck_conversion(value, child, eltype)?;
            }
            Ok(())
        }
        (ColType::Tuple(types), DataValue::List(values)) => {
            let child =
                child_type(source).ok_or_else(|| format!("source type {source} is not a list"))?;
            if values.len() != types.len() {
                return Err(format!(
                    "source list has {} values, target tuple expects {}",
                    values.len(),
                    types.len()
                ));
            }
            for (value, target) in values.iter().zip(types) {
                precheck_conversion(value, child, target)?;
            }
            Ok(())
        }
        (ColType::Vec { eltype, .. }, DataValue::List(values)) => {
            let child =
                child_type(source).ok_or_else(|| format!("source type {source} is not a list"))?;
            for value in values {
                precheck_vector_element(value, child, *eltype)?;
            }
            Ok(())
        }
        (ColType::TxTime, _) => Err("TxTime is engine-assigned".to_string()),
        (ColType::Bytes, DataValue::Str(_)) => {
            Err("implicit base64 string-to-Bytes conversion is disabled".to_string())
        }
        (ColType::Vec { .. }, DataValue::Str(_)) => {
            Err("base64 string-to-vector conversion is disabled".to_string())
        }
        (ColType::Json, _) => {
            Err("Json targets require an arrow.json logical annotation".to_string())
        }
        (target, value) => Err(format!(
            "source value kind {} is not accepted by target {target:?}",
            value_kind(value)
        )),
    }
}

fn precheck_vector_element(
    value: &DataValue,
    _source: &DataType,
    target: VecElementType,
) -> std::result::Result<(), String> {
    match (target, value) {
        (VecElementType::F64, DataValue::Num(Num::Float(_))) => Ok(()),
        (VecElementType::F64, DataValue::Num(Num::Int(value))) => {
            if (*value as f64) as i128 == *value as i128 {
                Ok(())
            } else {
                Err(format!(
                    "integer {value} cannot be represented losslessly as f64"
                ))
            }
        }
        (VecElementType::F32, DataValue::Num(Num::Float(value))) => {
            if !value.is_finite() || (*value as f32) as f64 == *value {
                Ok(())
            } else {
                Err(format!(
                    "float {value} cannot be represented losslessly as f32"
                ))
            }
        }
        (VecElementType::F32, DataValue::Num(Num::Int(value))) => {
            if (*value as f32) as i128 == *value as i128 {
                Ok(())
            } else {
                Err(format!(
                    "integer {value} cannot be represented losslessly as f32"
                ))
            }
        }
        (_, other) => Err(format!(
            "vector element {} is not numeric",
            value_kind(other)
        )),
    }
}

fn unwrap_encoding(data_type: &DataType) -> &DataType {
    match data_type {
        DataType::Dictionary(_, value) => unwrap_encoding(value),
        DataType::RunEndEncoded(_, values) => unwrap_encoding(values.data_type()),
        other => other,
    }
}

fn child_type(data_type: &DataType) -> Option<&DataType> {
    match unwrap_encoding(data_type) {
        DataType::List(child) | DataType::LargeList(child) | DataType::FixedSizeList(child, _) => {
            Some(child.data_type())
        }
        _ => None,
    }
}

fn value_kind(value: &DataValue) -> &'static str {
    match value {
        DataValue::Null => "Null",
        DataValue::Bool(_) => "Bool",
        DataValue::Num(Num::Int(_)) => "Int",
        DataValue::Num(Num::Float(_)) => "Float",
        DataValue::Str(_) => "String",
        DataValue::Bytes(_) => "Bytes",
        DataValue::Uuid(_) => "Uuid",
        DataValue::List(_) => "List",
        DataValue::Vec(_) => "Vector",
        DataValue::Json(_) => "Json",
        DataValue::Validity(_) => "Validity",
        DataValue::Regex(_) | DataValue::Set(_) | DataValue::Bot => "internal",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::types::{Float16Type, Int32Type};
    use std::collections::HashMap;

    use arrow_array::{ArrayRef, NullArray};

    fn scalar(array: &dyn Array, field: &Field, row: usize) -> DataValue {
        scalar_to_value(array, field, row, None).unwrap()
    }

    #[test]
    fn supported_scalar_encodings_preserve_value_kinds() {
        let cases: Vec<(Box<dyn Array>, Field, DataValue)> = vec![
            (
                Box::new(NullArray::new(1)),
                Field::new("v", DataType::Null, true),
                DataValue::Null,
            ),
            (
                Box::new(BooleanArray::from(vec![true])),
                Field::new("v", DataType::Boolean, false),
                DataValue::Bool(true),
            ),
            (
                Box::new(Int8Array::from(vec![i8::MIN])),
                Field::new("v", DataType::Int8, false),
                DataValue::from(i8::MIN as i64),
            ),
            (
                Box::new(Int16Array::from(vec![i16::MIN])),
                Field::new("v", DataType::Int16, false),
                DataValue::from(i16::MIN as i64),
            ),
            (
                Box::new(Int32Array::from(vec![i32::MIN])),
                Field::new("v", DataType::Int32, false),
                DataValue::from(i32::MIN as i64),
            ),
            (
                Box::new(Int64Array::from(vec![i64::MIN])),
                Field::new("v", DataType::Int64, false),
                DataValue::from(i64::MIN),
            ),
            (
                Box::new(UInt8Array::from(vec![u8::MAX])),
                Field::new("v", DataType::UInt8, false),
                DataValue::from(u8::MAX as i64),
            ),
            (
                Box::new(UInt16Array::from(vec![u16::MAX])),
                Field::new("v", DataType::UInt16, false),
                DataValue::from(u16::MAX as i64),
            ),
            (
                Box::new(UInt32Array::from(vec![u32::MAX])),
                Field::new("v", DataType::UInt32, false),
                DataValue::from(u32::MAX as i64),
            ),
            (
                Box::new(UInt64Array::from(vec![i64::MAX as u64])),
                Field::new("v", DataType::UInt64, false),
                DataValue::from(i64::MAX),
            ),
            (
                Box::new(Float16Array::from_iter_values([
                    <Float16Type as arrow_array::types::ArrowPrimitiveType>::Native::default(),
                ])),
                Field::new("v", DataType::Float16, false),
                DataValue::from(0.0_f64),
            ),
            (
                Box::new(Float32Array::from(vec![f32::INFINITY])),
                Field::new("v", DataType::Float32, false),
                DataValue::from(f64::INFINITY),
            ),
            (
                Box::new(Float64Array::from(vec![f64::NEG_INFINITY])),
                Field::new("v", DataType::Float64, false),
                DataValue::from(f64::NEG_INFINITY),
            ),
            (
                Box::new(StringArray::from(vec!["utf8"])),
                Field::new("v", DataType::Utf8, false),
                DataValue::from("utf8"),
            ),
            (
                Box::new(LargeStringArray::from(vec!["large"])),
                Field::new("v", DataType::LargeUtf8, false),
                DataValue::from("large"),
            ),
            (
                Box::new(StringViewArray::from(vec!["view"])),
                Field::new("v", DataType::Utf8View, false),
                DataValue::from("view"),
            ),
            (
                Box::new(BinaryArray::from_vec(vec![b"bin"])),
                Field::new("v", DataType::Binary, false),
                DataValue::Bytes(b"bin".to_vec()),
            ),
            (
                Box::new(LargeBinaryArray::from_vec(vec![b"large"])),
                Field::new("v", DataType::LargeBinary, false),
                DataValue::Bytes(b"large".to_vec()),
            ),
            (
                Box::new(BinaryViewArray::from_iter_values([b"view".as_slice()])),
                Field::new("v", DataType::BinaryView, false),
                DataValue::Bytes(b"view".to_vec()),
            ),
            (
                Box::new(
                    FixedSizeBinaryArray::try_from_iter([b"fixed".as_slice()].into_iter()).unwrap(),
                ),
                Field::new("v", DataType::FixedSizeBinary(5), false),
                DataValue::Bytes(b"fixed".to_vec()),
            ),
        ];

        for (array, field, expected) in cases {
            assert_eq!(scalar(array.as_ref(), &field, 0), expected, "{field:?}");
        }

        let nan = Float64Array::from(vec![f64::NAN]);
        assert!(matches!(
            scalar(&nan, &Field::new("v", DataType::Float64, false), 0),
            DataValue::Num(Num::Float(value)) if value.is_nan()
        ));
        assert!(matches!(
            scalar_to_value(
                &UInt64Array::from(vec![u64::MAX]),
                &Field::new("v", DataType::UInt64, false),
                0,
                None,
            ),
            Err(ScalarError::Conversion(message)) if message.contains("i64::MAX")
        ));
    }

    #[test]
    fn list_dictionary_and_run_end_encodings_are_recursive() {
        let list =
            ListArray::from_iter_primitive::<Int32Type, _, _>([Some(vec![Some(1), None, Some(3)])]);
        assert_eq!(
            scalar(&list, &Field::new("v", list.data_type().clone(), false), 0,),
            DataValue::List(vec![
                DataValue::from(1_i64),
                DataValue::Null,
                DataValue::from(3_i64),
            ])
        );

        let large =
            LargeListArray::from_iter_primitive::<Int32Type, _, _>([Some(vec![Some(4), Some(5)])]);
        assert_eq!(
            scalar(
                &large,
                &Field::new("v", large.data_type().clone(), false),
                0,
            ),
            DataValue::List(vec![DataValue::from(4_i64), DataValue::from(5_i64)])
        );

        let fixed = FixedSizeListArray::from_iter_primitive::<Int32Type, _, _>(
            [Some(vec![Some(6), Some(7)])],
            2,
        );
        assert_eq!(
            scalar(
                &fixed,
                &Field::new("v", fixed.data_type().clone(), false),
                0,
            ),
            DataValue::List(vec![DataValue::from(6_i64), DataValue::from(7_i64)])
        );

        let dictionary = DictionaryArray::<Int32Type>::from_iter(["red", "red", "blue"]);
        assert_eq!(
            scalar(
                &dictionary,
                &Field::new("v", dictionary.data_type().clone(), false),
                2,
            ),
            DataValue::from("blue")
        );

        let run_ends = Int32Array::from(vec![2, 5]);
        let run_values: ArrayRef = Arc::new(StringArray::from(vec!["a", "b"]));
        let runs = RunArray::<Int32Type>::try_new(&run_ends, &run_values).unwrap();
        assert_eq!(
            scalar(&runs, &Field::new("v", runs.data_type().clone(), false), 3,),
            DataValue::from("b")
        );
    }

    #[test]
    fn unsupported_types_and_extensions_fail_schema_preflight() {
        let unsupported = [
            DataType::Decimal128(10, 2),
            DataType::Date32,
            DataType::Timestamp(arrow_schema::TimeUnit::Microsecond, None),
            DataType::Struct(vec![Field::new("x", DataType::Int64, false)].into()),
            DataType::Map(
                Arc::new(Field::new(
                    "entries",
                    DataType::Struct(
                        vec![
                            Field::new("key", DataType::Utf8, false),
                            Field::new("value", DataType::Int64, true),
                        ]
                        .into(),
                    ),
                    false,
                )),
                false,
            ),
            DataType::Union(
                arrow_schema::UnionFields::try_new([0], [Field::new("x", DataType::Int64, false)])
                    .unwrap(),
                arrow_schema::UnionMode::Sparse,
            ),
        ];
        for data_type in unsupported {
            let error = validate_field(&Field::new("v", data_type, true), 1, 16).unwrap_err();
            assert!(format!("{error:?}").contains("columnar::unsupported_type"));
        }

        let unknown = Field::new("v", DataType::Utf8, false).with_metadata(HashMap::from([
            (
                "ARROW:extension:name".to_string(),
                "example.unknown".to_string(),
            ),
            ("ARROW:extension:metadata".to_string(), String::new()),
        ]));
        let error = validate_field(&unknown, 1, 16).unwrap_err();
        assert!(format!("{error:?}").contains("columnar::unsupported_type"));
    }

    #[test]
    fn numeric_prechecks_reject_lossy_cross_kind_conversions() {
        let int = NullableColType {
            coltype: ColType::Int,
            nullable: false,
        };
        let float = NullableColType {
            coltype: ColType::Float,
            nullable: false,
        };
        assert!(precheck_conversion(&DataValue::from(42.0_f64), &DataType::Float64, &int).is_ok());
        assert!(precheck_conversion(&DataValue::from(42.5_f64), &DataType::Float64, &int).is_err());
        assert!(precheck_conversion(
            &DataValue::from(9_007_199_254_740_993_i64),
            &DataType::Int64,
            &float
        )
        .is_err());

        let bytes = NullableColType {
            coltype: ColType::Bytes,
            nullable: false,
        };
        let vector = NullableColType {
            coltype: ColType::Vec {
                eltype: VecElementType::F32,
                len: 2,
            },
            nullable: false,
        };
        let json = NullableColType {
            coltype: ColType::Json,
            nullable: false,
        };
        let text = DataValue::from("AQID");
        assert!(precheck_conversion(&text, &DataType::Utf8, &bytes).is_err());
        assert!(precheck_conversion(&text, &DataType::Utf8, &vector).is_err());
        assert!(precheck_conversion(&text, &DataType::Utf8, &json).is_err());
    }
}
