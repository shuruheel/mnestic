/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

#![cfg(all(feature = "columnar-io", feature = "storage-sqlite"))]

use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use arrow_array::{
    Array, ArrayRef, Date32Array, FixedSizeBinaryArray, Int64Array, RecordBatch, StringArray,
    UInt64Array,
};
use arrow_ipc::writer::{FileWriter, StreamWriter};
use arrow_schema::extension::{Json as ArrowJson, Uuid as ArrowUuid};
use arrow_schema::{DataType, Field, Schema};
use cozo::{
    ColumnarFileFormat, ColumnarImportOptions, DataValue, DbInstance, NamedRows, ScriptMutability,
};
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;

fn new_db(path: &Path) -> DbInstance {
    DbInstance::new("sqlite", path.to_str().unwrap(), Default::default()).unwrap()
}

fn run(db: &DbInstance, script: &str) -> NamedRows {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|error| panic!("script failed: {error:?}\n--- script ---\n{script}"))
}

fn people_batch() -> RecordBatch {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("name", DataType::Utf8, false),
    ]));
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(vec![1, 2, 3])) as ArrayRef,
            Arc::new(StringArray::from(vec!["Ada", "Grace", "Evelyn"])) as ArrayRef,
        ],
    )
    .unwrap()
}

fn write_ipc_file(path: &Path, batches: &[RecordBatch]) {
    let file = File::create(path).unwrap();
    let mut writer = FileWriter::try_new(file, batches[0].schema_ref()).unwrap();
    for batch in batches {
        writer.write(batch).unwrap();
    }
    writer.finish().unwrap();
}

fn write_ipc_stream(path: &Path, batches: &[RecordBatch]) {
    let file = File::create(path).unwrap();
    let mut writer = StreamWriter::try_new(file, batches[0].schema_ref()).unwrap();
    for batch in batches {
        writer.write(batch).unwrap();
    }
    writer.finish().unwrap();
}

fn write_parquet(path: &Path, batches: &[RecordBatch]) {
    write_parquet_compressed(path, batches, Compression::UNCOMPRESSED);
}

fn write_parquet_snappy(path: &Path, batches: &[RecordBatch]) {
    write_parquet_compressed(path, batches, Compression::SNAPPY);
}

fn write_parquet_zstd(path: &Path, batches: &[RecordBatch]) {
    write_parquet_compressed(path, batches, Compression::ZSTD(Default::default()));
}

fn write_parquet_compressed(path: &Path, batches: &[RecordBatch], compression: Compression) {
    let file = File::create(path).unwrap();
    let properties = WriterProperties::builder()
        .set_compression(compression)
        .build();
    let mut writer = ArrowWriter::try_new(file, batches[0].schema(), Some(properties)).unwrap();
    for batch in batches {
        writer.write(batch).unwrap();
    }
    writer.close().unwrap();
}

#[test]
fn parquet_and_both_ipc_encodings_import_the_same_rows() {
    let dir = tempfile::tempdir().unwrap();
    let batch = people_batch();
    let cases = [
        (
            ColumnarFileFormat::Parquet,
            dir.path().join("people.parquet"),
            write_parquet as fn(&Path, &[RecordBatch]),
        ),
        (
            ColumnarFileFormat::Parquet,
            dir.path().join("people-snappy.parquet"),
            write_parquet_snappy,
        ),
        (
            ColumnarFileFormat::Parquet,
            dir.path().join("people-zstd.parquet"),
            write_parquet_zstd,
        ),
        (
            ColumnarFileFormat::ArrowIpcFile,
            dir.path().join("people.arrow"),
            write_ipc_file,
        ),
        (
            ColumnarFileFormat::ArrowIpcStream,
            dir.path().join("people.stream"),
            write_ipc_stream,
        ),
    ];

    for (index, (format, source, write)) in cases.into_iter().enumerate() {
        write(&source, std::slice::from_ref(&batch));
        let db = new_db(&dir.path().join(format!("case-{index}.db")));
        run(&db, ":create people {id: Int => name: String}");
        let report = db
            .import_columnar_file(
                "people",
                &source,
                &ColumnarImportOptions::new(format).with_batch_rows(1),
            )
            .unwrap();
        assert_eq!(report.rows_processed, 3);
        let expected_batches = if matches!(format, ColumnarFileFormat::Parquet) {
            3
        } else {
            1
        };
        assert_eq!(report.batches_processed, expected_batches);
        assert!(report.search_indexes_requiring_rebuild.is_empty());

        let rows = run(&db, "?[id, name] := *people{id, name} :sort id").rows;
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0],
            vec![DataValue::from(1_i64), DataValue::from("Ada")]
        );
        assert_eq!(
            rows[2],
            vec![DataValue::from(3_i64), DataValue::from("Evelyn")]
        );
    }
}

#[test]
fn a_late_conversion_failure_rolls_back_earlier_rows() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("late-failure.arrow");
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("score", DataType::UInt64, false),
    ]));
    let first = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(Int64Array::from(vec![1])) as ArrayRef,
            Arc::new(UInt64Array::from(vec![7])) as ArrayRef,
        ],
    )
    .unwrap();
    let second = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(vec![2])) as ArrayRef,
            Arc::new(UInt64Array::from(vec![u64::MAX])) as ArrayRef,
        ],
    )
    .unwrap();
    write_ipc_file(&source, &[first, second]);

    let db = new_db(&dir.path().join("rollback.db"));
    run(&db, ":create scores {id: Int => score: Int}");
    let error = db
        .import_columnar_file(
            "scores",
            &source,
            &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile).with_batch_rows(1),
        )
        .expect_err("u64::MAX must fail after the first row was staged");
    let rendered = format!("{error:?}");
    assert!(rendered.contains("columnar::conversion"), "{rendered}");
    assert!(rendered.contains("row 1"), "{rendered}");
    assert!(run(&db, "?[id, score] := *scores{id, score}")
        .rows
        .is_empty());
}

#[test]
fn mapping_and_row_limit_are_precommit_contracts() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("mapped.arrow");
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Int64, false),
        Field::new("label", DataType::Utf8, false),
        Field::new("ignored", DataType::Int64, false),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(vec![10, 11])) as ArrayRef,
            Arc::new(StringArray::from(vec!["xx", "y"])) as ArrayRef,
            Arc::new(Int64Array::from(vec![99, 99])) as ArrayRef,
        ],
    )
    .unwrap();
    write_ipc_file(&source, &[batch]);

    let db = new_db(&dir.path().join("mapping.db"));
    run(&db, ":create mapped {id: Int => name: String}");
    let columns = BTreeMap::from([
        ("id".to_string(), "external_id".to_string()),
        ("name".to_string(), "label".to_string()),
    ]);
    let limited = ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile)
        .with_columns(columns.clone())
        .with_max_rows(Some(1));
    let error = db
        .import_columnar_file("mapped", &source, &limited)
        .expect_err("the batch exceeds max_rows");
    assert!(format!("{error:?}").contains("columnar::row_limit"));
    assert!(run(&db, "?[id] := *mapped{id}").rows.is_empty());

    let value_limited = ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile)
        .with_columns(columns.clone())
        .with_max_value_bytes(Some(1));
    let error = db
        .import_columnar_file("mapped", &source, &value_limited)
        .expect_err("the first label exceeds max_value_bytes");
    assert!(format!("{error:?}").contains("columnar::value_limit"));
    assert!(run(&db, "?[id] := *mapped{id}").rows.is_empty());

    let report = db
        .import_columnar_file(
            "mapped",
            &source,
            &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile)
                .with_columns(columns)
                .with_max_rows(Some(2)),
        )
        .unwrap();
    assert_eq!(report.rows_processed, 2);
}

#[test]
fn canonical_uuid_and_json_extensions_preserve_meaning() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("extensions.arrow");
    let uuid_bytes = [
        0x12, 0x34, 0x56, 0x78, 0x12, 0x34, 0x56, 0x78, 0x90, 0xab, 0xcd, 0xef, 0x12, 0x34, 0x56,
        0x78,
    ];
    let uuid_field =
        Field::new("id", DataType::FixedSizeBinary(16), false).with_extension_type(ArrowUuid);
    let json_field =
        Field::new("payload", DataType::Utf8, false).with_extension_type(ArrowJson::default());
    let batch = RecordBatch::try_new(
        Arc::new(Schema::new(vec![uuid_field, json_field])),
        vec![
            Arc::new(
                FixedSizeBinaryArray::try_from_iter([uuid_bytes.as_slice()].into_iter()).unwrap(),
            ) as ArrayRef,
            Arc::new(StringArray::from(vec![r#"{"ok":true}"#])) as ArrayRef,
        ],
    )
    .unwrap();
    write_ipc_file(&source, std::slice::from_ref(&batch));

    let db = new_db(&dir.path().join("extensions.db"));
    run(&db, ":create docs {id: Uuid => payload: Json}");
    db.import_columnar_file(
        "docs",
        &source,
        &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile),
    )
    .unwrap();
    let rows = run(&db, "?[id, payload] := *docs{id, payload}").rows;
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][0], DataValue::Uuid(_)));
    assert!(matches!(rows[0][1], DataValue::Json(_)));

    // Parquet's own logical descriptors are authoritative even if the
    // parquet-to-Arrow conversion behavior changes in a future pinned release.
    let parquet_source = dir.path().join("extensions.parquet");
    write_parquet(&parquet_source, &[batch]);
    db.import_columnar_file(
        "docs",
        &parquet_source,
        &ColumnarImportOptions::new(ColumnarFileFormat::Parquet),
    )
    .unwrap();
    assert_eq!(run(&db, "?[id] := *docs{id}").rows.len(), 1);

    let plain_source = dir.path().join("plain-json.arrow");
    let plain_batch = RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("payload", DataType::Utf8, false),
        ])),
        vec![
            Arc::new(Int64Array::from(vec![1])) as ArrayRef,
            Arc::new(StringArray::from(vec![r#"{"looks":"json"}"#])) as ArrayRef,
        ],
    )
    .unwrap();
    write_ipc_file(&plain_source, &[plain_batch]);
    let plain_db = new_db(&dir.path().join("plain-json.db"));
    run(&plain_db, ":create docs {id: Int => payload: Json}");
    let error = plain_db
        .import_columnar_file(
            "docs",
            &plain_source,
            &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile),
        )
        .expect_err("unannotated text must not silently become JSON");
    assert!(format!("{error:?}").contains("columnar::conversion"));
    assert!(run(&plain_db, "?[id] := *docs{id}").rows.is_empty());
}

#[test]
fn parquet_logical_types_do_not_fall_through_to_physical_integers() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("date.parquet");
    let batch = RecordBatch::try_new(
        Arc::new(Schema::new(vec![Field::new("id", DataType::Date32, false)])),
        vec![Arc::new(Date32Array::from(vec![20_000])) as ArrayRef],
    )
    .unwrap();
    write_parquet(&source, &[batch]);

    let db = new_db(&dir.path().join("date.db"));
    run(&db, ":create dates {id: Int}");
    let error = db
        .import_columnar_file(
            "dates",
            &source,
            &ColumnarImportOptions::new(ColumnarFileFormat::Parquet),
        )
        .expect_err("Parquet DATE must not be imported as its physical i32");
    assert!(format!("{error:?}").contains("columnar::unsupported_type"));
    assert!(run(&db, "?[id] := *dates{id}").rows.is_empty());
}

#[test]
fn zero_rows_and_multi_chunk_sources_have_exact_counts() {
    let dir = tempfile::tempdir().unwrap();
    let empty_source = dir.path().join("empty.arrow");
    let empty = RecordBatch::try_new(
        Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)])),
        vec![Arc::new(Int64Array::from(Vec::<i64>::new())) as ArrayRef],
    )
    .unwrap();
    write_ipc_file(&empty_source, &[empty]);

    let db = new_db(&dir.path().join("counts.db"));
    run(&db, ":create items {id: Int}");
    let empty_report = db
        .import_columnar_file(
            "items",
            &empty_source,
            &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile).with_batch_rows(2),
        )
        .unwrap();
    assert_eq!(empty_report.rows_processed, 0);
    assert!(empty_report.batches_processed <= 1);

    let source = dir.path().join("chunks.arrow");
    let batch = RecordBatch::try_new(
        Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)])),
        vec![Arc::new(Int64Array::from((0..9).collect::<Vec<_>>())) as ArrayRef],
    )
    .unwrap();
    write_ipc_file(&source, &[batch]);
    let report = db
        .import_columnar_file(
            "items",
            &source,
            &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile).with_batch_rows(2),
        )
        .unwrap();
    assert_eq!(report.rows_processed, 9);
    assert_eq!(run(&db, "?[id] := *items{id}").rows.len(), 9);
}

#[test]
fn resource_limits_cover_source_batch_and_nesting_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("limits.arrow");
    let batch = people_batch();
    let decoded_bytes = batch.get_array_memory_size();
    write_ipc_file(&source, std::slice::from_ref(&batch));
    let source_bytes = source.metadata().unwrap().len();

    for (name, options, code) in [
        (
            "source",
            ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile)
                .with_max_source_bytes(Some(source_bytes - 1)),
            "columnar::source_limit",
        ),
        (
            "batch",
            ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile)
                .with_max_decoded_batch_bytes(Some(decoded_bytes - 1)),
            "columnar::decoded_batch_limit",
        ),
    ] {
        let db = new_db(&dir.path().join(format!("{name}.db")));
        run(&db, ":create people {id: Int => name: String}");
        let error = db
            .import_columnar_file("people", &source, &options)
            .expect_err("below-boundary limit must reject the source");
        assert!(format!("{error:?}").contains(code));
        assert!(run(&db, "?[id] := *people{id}").rows.is_empty());
    }

    let db = new_db(&dir.path().join("equal.db"));
    run(&db, ":create people {id: Int => name: String}");
    db.import_columnar_file(
        "people",
        &source,
        &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile)
            .with_max_source_bytes(Some(source_bytes)),
    )
    .unwrap();

    let nested_source = dir.path().join("nested.arrow");
    let list = arrow_array::ListArray::from_iter_primitive::<arrow_array::types::Int64Type, _, _>(
        [Some(vec![Some(1), Some(2)])],
    );
    let nested_batch = RecordBatch::try_new(
        Arc::new(Schema::new(vec![Field::new(
            "values",
            list.data_type().clone(),
            false,
        )])),
        vec![Arc::new(list) as ArrayRef],
    )
    .unwrap();
    write_ipc_file(&nested_source, &[nested_batch]);
    let nested_db = new_db(&dir.path().join("nesting.db"));
    run(&nested_db, ":create lists {values: [Int]}");
    let error = nested_db
        .import_columnar_file(
            "lists",
            &nested_source,
            &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile).with_max_nesting_depth(1),
        )
        .expect_err("list child is at depth two");
    assert!(format!("{error:?}").contains("columnar::schema"));
    nested_db
        .import_columnar_file(
            "lists",
            &nested_source,
            &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile).with_max_nesting_depth(2),
        )
        .unwrap();
}

#[test]
fn mapping_variants_and_schema_errors_are_preflighted() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("mapping-contract.arrow");
    let schema = Arc::new(Schema::new(vec![
        Field::new("value", DataType::Int64, false),
        Field::new("source", DataType::Int64, false),
        Field::new("ignored", DataType::Utf8, false),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(vec![10])) as ArrayRef,
            Arc::new(Int64Array::from(vec![1])) as ArrayRef,
            Arc::new(StringArray::from(vec!["x"])) as ArrayRef,
        ],
    )
    .unwrap();
    write_ipc_file(&source, &[batch]);

    let db = new_db(&dir.path().join("mapping-contract.db"));
    run(&db, ":create mapped {id: Int => copied: Int, value: Int}");
    let columns = BTreeMap::from([
        ("id".to_string(), "source".to_string()),
        ("copied".to_string(), "source".to_string()),
    ]);
    db.import_columnar_file(
        "mapped",
        &source,
        &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile).with_columns(columns),
    )
    .unwrap();
    assert_eq!(
        run(&db, "?[id, copied, value] := *mapped{id, copied, value}").rows[0],
        vec![
            DataValue::from(1_i64),
            DataValue::from(1_i64),
            DataValue::from(10_i64),
        ]
    );

    for (name, columns) in [
        (
            "unknown-target",
            BTreeMap::from([("no_such_target".to_string(), "source".to_string())]),
        ),
        (
            "missing-source",
            BTreeMap::from([("id".to_string(), "no_such_source".to_string())]),
        ),
    ] {
        let other = new_db(&dir.path().join(format!("{name}.db")));
        run(
            &other,
            ":create mapped {id: Int => copied: Int, value: Int}",
        );
        let error = other
            .import_columnar_file(
                "mapped",
                &source,
                &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile).with_columns(columns),
            )
            .unwrap_err();
        assert!(format!("{error:?}").contains("columnar::schema"));
        assert!(run(&other, "?[id] := *mapped{id}").rows.is_empty());
    }
}

#[test]
fn parquet_projects_ignored_columns_before_decoded_batch_accounting() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("projection.parquet");
    let large_ignored = [vec![7_u8; 128 * 1024]];
    let batch = RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("ignored", DataType::Binary, false),
        ])),
        vec![
            Arc::new(Int64Array::from(vec![1])) as ArrayRef,
            Arc::new(arrow_array::BinaryArray::from_iter_values(
                large_ignored.iter().map(Vec::as_slice),
            )) as ArrayRef,
        ],
    )
    .unwrap();
    assert!(batch.get_array_memory_size() > 100_000);
    write_parquet(&source, &[batch]);

    let db = new_db(&dir.path().join("projection.db"));
    run(&db, ":create items {id: Int}");
    db.import_columnar_file(
        "items",
        &source,
        &ColumnarImportOptions::new(ColumnarFileFormat::Parquet)
            .with_max_decoded_batch_bytes(Some(4_096)),
    )
    .expect("the ignored binary field must be projected before Arrow allocation accounting");
    assert_eq!(run(&db, "?[id] := *items{id}").rows.len(), 1);
}

#[test]
fn nested_vectors_validity_and_invalid_json_follow_explicit_mappings() {
    let dir = tempfile::tempdir().unwrap();
    let vector_source = dir.path().join("vectors.arrow");
    let vectors = arrow_array::FixedSizeListArray::from_iter_primitive::<
        arrow_array::types::Float32Type,
        _,
        _,
    >([Some(vec![Some(1.0_f32), Some(2.0_f32)])], 2);
    let batch = RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("embedding", vectors.data_type().clone(), false),
        ])),
        vec![
            Arc::new(Int64Array::from(vec![1])) as ArrayRef,
            Arc::new(vectors) as ArrayRef,
        ],
    )
    .unwrap();
    write_ipc_file(&vector_source, &[batch]);

    let db = new_db(&dir.path().join("vectors.db"));
    run(&db, ":create vectors {id: Int => embedding: <F32; 2>}");
    db.import_columnar_file(
        "vectors",
        &vector_source,
        &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile),
    )
    .unwrap();
    assert_eq!(run(&db, "?[id] := *vectors{id}").rows.len(), 1);

    let wrong = new_db(&dir.path().join("wrong-vector.db"));
    run(&wrong, ":create vectors {id: Int => embedding: <F32; 3>}");
    let error = wrong
        .import_columnar_file(
            "vectors",
            &vector_source,
            &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile),
        )
        .expect_err("vector width mismatch must fail");
    assert!(format!("{error:?}").contains("columnar::conversion"));
    assert!(run(&wrong, "?[id] := *vectors{id}").rows.is_empty());

    let validity_source = dir.path().join("validity.arrow");
    let validity_batch = RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("at", DataType::Utf8, false),
        ])),
        vec![
            Arc::new(Int64Array::from(vec![1])) as ArrayRef,
            Arc::new(StringArray::from(vec!["2026-08-18T12:00:00Z"])) as ArrayRef,
        ],
    )
    .unwrap();
    write_ipc_file(&validity_source, &[validity_batch]);
    let validity = new_db(&dir.path().join("validity.db"));
    run(&validity, ":create events {id: Int, at: Validity}");
    validity
        .import_columnar_file(
            "events",
            &validity_source,
            &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile),
        )
        .unwrap();
    assert_eq!(run(&validity, "?[id] := *events{id}").rows.len(), 1);

    let invalid_json_source = dir.path().join("invalid-json.arrow");
    let json_field =
        Field::new("payload", DataType::Utf8, false).with_extension_type(ArrowJson::default());
    let json_batch = RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            json_field,
        ])),
        vec![
            Arc::new(Int64Array::from(vec![1])) as ArrayRef,
            Arc::new(StringArray::from(vec!["{not-json"])) as ArrayRef,
        ],
    )
    .unwrap();
    write_ipc_file(&invalid_json_source, &[json_batch]);
    let invalid_json = new_db(&dir.path().join("invalid-json.db"));
    run(&invalid_json, ":create docs {id: Int => payload: Json}");
    let error = invalid_json
        .import_columnar_file(
            "docs",
            &invalid_json_source,
            &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile),
        )
        .expect_err("invalid canonical JSON must fail conversion");
    assert!(format!("{error:?}").contains("columnar::conversion"));
    assert!(run(&invalid_json, "?[id] := *docs{id}").rows.is_empty());
}

#[test]
fn malformed_and_non_file_sources_fail_without_writes() {
    let dir = tempfile::tempdir().unwrap();
    let malformed = dir.path().join("malformed.arrow");
    std::fs::write(&malformed, b"not an Arrow file").unwrap();
    let db = new_db(&dir.path().join("malformed.db"));
    run(&db, ":create items {id: Int}");

    for path in [malformed.as_path(), dir.path()] {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            db.import_columnar_file(
                "items",
                path,
                &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile),
            )
        }));
        let error = result
            .expect("malformed sources must never panic")
            .unwrap_err();
        assert!(format!("{error:?}").contains("columnar::"));
    }

    for path in ["https://example.invalid/data.arrow", "*.arrow"] {
        let error = db
            .import_columnar_file(
                "items",
                path,
                &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile),
            )
            .unwrap_err();
        assert!(format!("{error:?}").contains("columnar::open"));
    }
    assert!(run(&db, "?[id] := *items{id}").rows.is_empty());
}

#[test]
fn duplicate_keys_match_import_relations_put_semantics_and_btree_state() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("duplicates.arrow");
    let batch = RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, false),
        ])),
        vec![
            Arc::new(Int64Array::from(vec![1, 1, 2])) as ArrayRef,
            Arc::new(StringArray::from(vec!["old", "new", "two"])) as ArrayRef,
        ],
    )
    .unwrap();
    write_ipc_file(&source, &[batch]);

    let columnar = new_db(&dir.path().join("columnar.db"));
    let rowwise = new_db(&dir.path().join("rowwise.db"));
    for db in [&columnar, &rowwise] {
        run(db, ":create items {id: Int => name: String}");
        run(db, "::index create items:by_name {name}");
        run(db, "?[id, name] <- [[1, 'before']] :put items {id => name}");
    }
    columnar
        .import_columnar_file(
            "items",
            &source,
            &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile),
        )
        .unwrap();
    rowwise
        .import_relations(BTreeMap::from([(
            "items".to_string(),
            NamedRows::new(
                vec!["id".to_string(), "name".to_string()],
                vec![
                    vec![DataValue::from(1_i64), DataValue::from("old")],
                    vec![DataValue::from(1_i64), DataValue::from("new")],
                    vec![DataValue::from(2_i64), DataValue::from("two")],
                ],
            ),
        )]))
        .unwrap();

    let columnar_dump = columnar
        .export_relations(["items", "items:by_name"].into_iter())
        .unwrap();
    let rowwise_dump = rowwise
        .export_relations(["items", "items:by_name"].into_iter())
        .unwrap();
    assert_eq!(
        columnar_dump.keys().collect::<Vec<_>>(),
        rowwise_dump.keys().collect::<Vec<_>>()
    );
    for relation in columnar_dump.keys() {
        let left = &columnar_dump[relation];
        let right = &rowwise_dump[relation];
        assert_eq!(left.headers, right.headers, "headers for {relation}");
        assert_eq!(left.rows, right.rows, "rows for {relation}");
    }
    assert_eq!(
        run(&columnar, "?[id, name] := *items{id, name}, name = 'new'").rows,
        vec![vec![DataValue::from(1_i64), DataValue::from("new")]]
    );
}

#[test]
fn lifecycle_contract_rejects_access_and_txtime_and_skips_callbacks() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("lifecycle.arrow");
    let batch = people_batch();
    write_ipc_file(&source, &[batch]);

    let db = new_db(&dir.path().join("lifecycle.db"));
    run(&db, ":create people {id: Int => name: String}");
    let (_callback_id, receiver) = db.register_callback("people", None);
    db.import_columnar_file(
        "people",
        &source,
        &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile),
    )
    .unwrap();
    assert!(
        receiver.try_recv().is_err(),
        "bulk import must not fire callbacks"
    );

    run(&db, "::access_level read_only people");
    let error = db
        .import_columnar_file(
            "people",
            &source,
            &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile),
        )
        .unwrap_err();
    assert!(format!("{error:?}").contains("access"));
    run(&db, "::access_level normal people");
    assert_eq!(run(&db, "?[id] := *people{id}").rows.len(), 3);

    run(&db, ":create history {id: Int, tt: TxTime => name: String}");
    let error = db
        .import_columnar_file(
            "history",
            &source,
            &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile),
        )
        .unwrap_err();
    assert!(format!("{error:?}").contains("TxTime"));
}

#[test]
fn search_indexes_are_reported_once_and_timeout_is_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("search.arrow");
    let batch = people_batch();
    write_ipc_file(&source, &[batch]);
    let db = new_db(&dir.path().join("search.db"));
    run(&db, ":create people {id: Int => name: String}");
    run(
        &db,
        "::fts create people:name_fts { extractor: name, tokenizer: Simple, filters: [Lowercase] }",
    );
    let report = db
        .import_columnar_file(
            "people",
            &source,
            &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile),
        )
        .unwrap();
    assert_eq!(
        report.search_indexes_requiring_rebuild,
        vec!["name_fts".to_string()]
    );
    let warnings = match &db {
        DbInstance::Sqlite(inner) => inner.recent_warnings(),
        _ => unreachable!(),
    };
    let stranded = warnings
        .iter()
        .filter(|(_, warning)| warning.code == "import.stranded_indexes")
        .count();
    assert_eq!(stranded, 1);

    let timeout_db = new_db(&dir.path().join("timeout.db"));
    run(&timeout_db, ":create people {id: Int => name: String}");
    let error = timeout_db
        .import_columnar_file(
            "people",
            &source,
            &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile)
                .with_timeout(Some(Duration::from_nanos(1))),
        )
        .unwrap_err();
    assert!(format!("{error:?}").contains("columnar::timeout"));
    assert!(run(&timeout_db, "?[id] := *people{id}").rows.is_empty());
}

#[cfg(feature = "test-hooks")]
#[test]
fn injected_commit_failure_returns_no_report_and_no_rows() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("commit-failure.arrow");
    write_ipc_file(&source, &[people_batch()]);
    let db = new_db(&dir.path().join("commit-failure.db"));
    run(&db, ":create people {id: Int => name: String}");
    db.fail_next_commit_for_tests();
    let error = db
        .import_columnar_file(
            "people",
            &source,
            &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile),
        )
        .expect_err("injected commit failure must escape the API");
    let rendered = format!("{error:?}");
    assert!(rendered.contains("injected commit failure"), "{rendered}");
    assert!(run(&db, "?[id] := *people{id}").rows.is_empty());
}

#[cfg(all(feature = "test-hooks", feature = "graph-algo"))]
#[test]
fn committed_import_invalidates_graph_projection_cache() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("edges.arrow");
    let batch = RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("a", DataType::Int64, false),
            Field::new("b", DataType::Int64, false),
        ])),
        vec![
            Arc::new(Int64Array::from(vec![1, 2])) as ArrayRef,
            Arc::new(Int64Array::from(vec![2, 3])) as ArrayRef,
        ],
    )
    .unwrap();
    write_ipc_file(&source, &[batch]);

    let db = new_db(&dir.path().join("projection-cache.db"));
    run(&db, ":create edges {a: Int, b: Int}");
    run(&db, "?[a, b] <- [[1, 2]] :put edges {a, b}");
    run(&db, "::graph create g {edges: edges}");
    run(&db, "?[n, component] <~ ConnectedComponents(graph: 'g')");
    let builds_before = match &db {
        DbInstance::Sqlite(inner) => inner.graph_projection_builds_for_tests(),
        _ => unreachable!(),
    };

    db.import_columnar_file(
        "edges",
        &source,
        &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile),
    )
    .unwrap();
    let rows = run(
        &db,
        "?[n, component] <~ ConnectedComponents(graph: 'g') :sort n",
    )
    .rows;
    assert_eq!(rows.len(), 3, "the rebuilt projection must contain node 3");
    let builds_after = match &db {
        DbInstance::Sqlite(inner) => inner.graph_projection_builds_for_tests(),
        _ => unreachable!(),
    };
    assert_eq!(builds_after, builds_before + 1);
}

#[cfg(feature = "storage-rocksdb")]
#[test]
fn rocksdb_late_failure_rolls_back_a_completed_batch() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("rocks-late-failure.arrow");
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("score", DataType::UInt64, false),
    ]));
    let first = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(Int64Array::from(vec![1])) as ArrayRef,
            Arc::new(UInt64Array::from(vec![7])) as ArrayRef,
        ],
    )
    .unwrap();
    let second = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(vec![2])) as ArrayRef,
            Arc::new(UInt64Array::from(vec![u64::MAX])) as ArrayRef,
        ],
    )
    .unwrap();
    write_ipc_file(&source, &[first, second]);

    let db = DbInstance::new(
        "rocksdb",
        dir.path().join("rocks.db").to_str().unwrap(),
        Default::default(),
    )
    .unwrap();
    run(&db, ":create scores {id: Int => score: Int}");
    let error = db
        .import_columnar_file(
            "scores",
            &source,
            &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile),
        )
        .expect_err("RocksDB must roll back the first decoded batch");
    assert!(format!("{error:?}").contains("columnar::conversion"));
    assert!(run(&db, "?[id] := *scores{id}").rows.is_empty());
}

#[cfg(feature = "storage-rocksdb")]
#[test]
fn rocksdb_concurrent_imports_resolve_without_mixed_state() {
    let dir = tempfile::tempdir().unwrap();
    let row_count = 10_000_i64;
    let mut sources = Vec::new();
    for owner in ["first", "second"] {
        let source = dir.path().join(format!("{owner}.arrow"));
        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("id", DataType::Int64, false),
                Field::new("owner", DataType::Utf8, false),
            ])),
            vec![
                Arc::new(Int64Array::from((0..row_count).collect::<Vec<_>>())) as ArrayRef,
                Arc::new(StringArray::from_iter_values((0..row_count).map(|_| owner))) as ArrayRef,
            ],
        )
        .unwrap();
        write_ipc_file(&source, &[batch]);
        sources.push(source);
    }

    let db = DbInstance::new(
        "rocksdb",
        dir.path().join("concurrent-rocks.db").to_str().unwrap(),
        Default::default(),
    )
    .unwrap();
    run(&db, ":create items {id: Int => owner: String}");
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let workers = sources
        .into_iter()
        .map(|source| {
            let worker_db = db.clone();
            let worker_barrier = barrier.clone();
            std::thread::spawn(move || {
                worker_barrier.wait();
                worker_db.import_columnar_file(
                    "items",
                    source,
                    &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile)
                        .with_batch_rows(64),
                )
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let results = workers
        .into_iter()
        .map(|worker| worker.join().expect("import thread must not panic"))
        .collect::<Vec<_>>();
    assert!(
        results.iter().any(Result::is_ok),
        "at least one RocksDB transaction must commit: {results:?}"
    );

    let rows = run(&db, "?[id, owner] := *items{id, owner}").rows;
    assert_eq!(rows.len(), row_count as usize);
    let first_count = rows
        .iter()
        .filter(|row| row[1] == DataValue::from("first"))
        .count();
    assert!(
        first_count == 0 || first_count == row_count as usize,
        "the final relation must be one complete import, never a mixed partial state"
    );
}

#[test]
fn sqlite_concurrent_imports_and_scripted_write_serialize_without_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("concurrent.arrow");
    let row_count = 20_000_i64;
    let batch = RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, false),
        ])),
        vec![
            Arc::new(Int64Array::from((0..row_count).collect::<Vec<_>>())) as ArrayRef,
            Arc::new(StringArray::from_iter_values(
                (0..row_count).map(|_| "imported"),
            )) as ArrayRef,
        ],
    )
    .unwrap();
    write_ipc_file(&source, &[batch]);

    let db = new_db(&dir.path().join("concurrent.db"));
    run(&db, ":create items {id: Int => name: String}");
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let import_db = db.clone();
    let import_source = source.clone();
    let import_barrier = barrier.clone();
    let importer = std::thread::spawn(move || {
        import_barrier.wait();
        import_db.import_columnar_file(
            "items",
            &import_source,
            &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile).with_batch_rows(64),
        )
    });
    barrier.wait();
    let scripted = db.run_script(
        "?[id, name] <- [[100000, 'scripted']] :put items {id => name}",
        BTreeMap::new(),
        ScriptMutability::Mutable,
    );
    let imported = importer.join().expect("import thread must not panic");

    assert!(
        imported.is_ok() || scripted.is_ok(),
        "SQLite may reject one contender, but not both: import={imported:?}, script={scripted:?}"
    );
    let rows = run(&db, "?[id, name] := *items{id, name}").rows;
    let imported_rows = rows
        .iter()
        .filter(|row| row[1] == DataValue::from("imported"))
        .count();
    assert!(
        imported_rows == 0 || imported_rows == row_count as usize,
        "an import must be wholly absent or wholly committed"
    );
    let scripted_rows = rows
        .iter()
        .filter(|row| row[0] == DataValue::from(100_000_i64))
        .count();
    assert_eq!(scripted_rows, usize::from(scripted.is_ok()));

    let first_source = dir.path().join("first.arrow");
    let second_source = dir.path().join("second.arrow");
    for (path, start) in [(&first_source, 200_000_i64), (&second_source, 201_000_i64)] {
        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("id", DataType::Int64, false),
                Field::new("name", DataType::Utf8, false),
            ])),
            vec![
                Arc::new(Int64Array::from((start..start + 1_000).collect::<Vec<_>>())) as ArrayRef,
                Arc::new(StringArray::from_iter_values(
                    (0..1_000).map(|_| "parallel"),
                )) as ArrayRef,
            ],
        )
        .unwrap();
        write_ipc_file(path, &[batch]);
    }
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let mut workers = Vec::new();
    for path in [first_source, second_source] {
        let worker_db = db.clone();
        let worker_barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            worker_barrier.wait();
            worker_db.import_columnar_file(
                "items",
                path,
                &ColumnarImportOptions::new(ColumnarFileFormat::ArrowIpcFile),
            )
        }));
    }
    barrier.wait();
    let results = workers
        .into_iter()
        .map(|worker| worker.join().expect("import thread must not panic"))
        .collect::<Vec<_>>();
    let successful = results.iter().filter(|result| result.is_ok()).count();
    assert!(successful >= 1, "both imports failed: {results:?}");
    let parallel_rows = run(&db, "?[id] := *items{id, name: 'parallel'}").rows.len();
    assert_eq!(parallel_rows, successful * 1_000);
}
