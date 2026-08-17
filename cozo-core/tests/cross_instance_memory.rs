/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Cross-instance shared memory tests (docs/specs/cross-instance-memory.md §8):
//! one shared block cache + WriteBufferManager envelope across several
//! embedded RocksDB instances in one process.

#![cfg(feature = "storage-rocksdb")]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once, OnceLock};

use cozo::{
    new_cozo_rocksdb, new_cozo_rocksdb_with_memory, DataValue, Db, DbInstance, NamedRows,
    RocksDbStorage, RocksMemoryConfig, RocksMemoryConfigError, RocksMemoryResources,
    ScriptMutability,
};

// ---------------------------------------------------------------------------
// helpers

fn run(db: &Db<RocksDbStorage>, script: &str) -> NamedRows {
    db.run_script(script, Default::default(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed: {e:?}\n--- script ---\n{script}"))
}

fn create_kv(db: &Db<RocksDbStorage>) {
    run(db, ":create kv {k: Int => v: String}");
}

/// Import `n` rows of roughly `payload_len` bytes each into `kv`.
fn import_rows(db: &Db<RocksDbStorage>, start: i64, n: i64, payload_len: usize) {
    let payload = "x".repeat(payload_len);
    let mut to_import = BTreeMap::new();
    to_import.insert(
        "kv".to_string(),
        NamedRows {
            headers: vec!["k".to_string(), "v".to_string()],
            rows: (start..start + n)
                .map(|i| vec![DataValue::from(i), DataValue::from(payload.as_str())])
                .collect(),
            next: None,
        },
    );
    db.import_relations(to_import).unwrap();
}

fn count_kv(db: &Db<RocksDbStorage>) -> usize {
    db.run_script(
        "?[count(k)] := *kv[k, _]",
        Default::default(),
        ScriptMutability::Immutable,
    )
    .unwrap()
    .rows[0][0]
        .get_int()
        .unwrap() as usize
}

/// Newest RocksDB-generated OPTIONS file in `<db>/data`.
fn newest_options_file(db_dir: &Path) -> PathBuf {
    let data_dir = db_dir.join("data");
    let mut candidates: Vec<_> = fs::read_dir(&data_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().starts_with("OPTIONS-"))
                .unwrap_or(false)
        })
        .collect();
    candidates.sort();
    candidates.pop().expect("RocksDB wrote an OPTIONS file")
}

// ---------------------------------------------------------------------------
// log capture (same pattern as tests/import_index_staleness.rs): the override
// is surfaced as a `log` warning, asserted via a capturing logger.

fn captured() -> &'static Mutex<Vec<String>> {
    static CAPTURED: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    CAPTURED.get_or_init(|| Mutex::new(Vec::new()))
}

struct CaptureLogger;

impl log::Log for CaptureLogger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, record: &log::Record<'_>) {
        if record.level() <= log::Level::Warn {
            captured().lock().unwrap().push(record.args().to_string());
        }
    }
    fn flush(&self) {}
}

fn init_capture() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        log::set_logger(&CaptureLogger).expect("no other logger may be installed");
        log::set_max_level(log::LevelFilter::Warn);
    });
}

// ---------------------------------------------------------------------------
// §8.1 — bit-parity default: no handle, plain open keeps working (the wider
// suite pins full behavior; this is the smoke half).

#[test]
fn no_handle_smoke() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_cozo_rocksdb(dir.path()).unwrap();
    create_kv(&db);
    import_rows(&db, 0, 100, 32);
    assert_eq!(count_kv(&db), 100);
    // Property reads work without a handle too (per-instance cache).
    let stats = db.rocksdb_memory_stats();
    assert!(stats.block_cache_usage.is_some());
    assert!(stats.cur_size_all_mem_tables.is_some());
    assert!(stats.estimate_table_readers_mem.is_some());
}

// ---------------------------------------------------------------------------
// §8.2 — sharing is real: two instances, one handle; writes to A move the
// handle-level WBM/cache metrics, and both instances see the same shared
// cache through the per-instance property read.

#[test]
fn sharing_is_real() {
    let handle = RocksMemoryResources::new(RocksMemoryConfig {
        total_bytes: 64 << 20,
        memtable_fraction: 0.25,
    })
    .unwrap();
    assert_eq!(handle.cache_capacity(), 64 << 20);
    assert_eq!(handle.buffer_size(), 16 << 20);

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let db_a = new_cozo_rocksdb_with_memory(dir_a.path(), &handle).unwrap();
    let db_b = new_cozo_rocksdb_with_memory(dir_b.path(), &handle).unwrap();
    create_kv(&db_a);
    create_kv(&db_b);

    let base_wbm = handle.memory_usage();
    let base_cache = handle.cache_usage();

    // ~2 MB into A's memtables.
    import_rows(&db_a, 0, 4000, 500);
    assert_eq!(count_kv(&db_a), 4000);

    // Handle-level: memtable bytes moved, and they were charged into the
    // shared cache as dummy entries (cost-to-cache).
    assert!(
        handle.memory_usage() > base_wbm,
        "WBM memory usage should grow with A's memtables: {} -> {}",
        base_wbm,
        handle.memory_usage()
    );
    assert!(handle.mutable_memtable_memory_usage() > 0);
    assert!(
        handle.dummy_entries_in_cache_usage() > 0,
        "memtable bytes must be charged into the shared cache"
    );
    assert!(
        handle.cache_usage() > base_cache,
        "shared cache usage should grow with the WBM dummy entries: {} -> {}",
        base_cache,
        handle.cache_usage()
    );

    // Per-instance property reads are plausible and nonzero.
    let stats_a = db_a.rocksdb_memory_stats();
    assert!(stats_a.cur_size_all_mem_tables.unwrap() > 0);
    assert!(stats_a.block_cache_usage.unwrap() > 0);
    assert!(stats_a.estimate_table_readers_mem.is_some());

    // B sees the SAME cache: its block-cache-usage read reports the shared
    // object, matching both A's read and the handle's own view. (Background
    // work may move the number between reads; allow a few retries.)
    let mut ok = false;
    for _ in 0..10 {
        let a = db_a.rocksdb_memory_stats().block_cache_usage.unwrap();
        let b = db_b.rocksdb_memory_stats().block_cache_usage.unwrap();
        let h = handle.cache_usage();
        if a == b && b == h {
            ok = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        ok,
        "A, B, and the handle must report the same shared cache usage"
    );

    // B is a working instance of its own.
    import_rows(&db_b, 0, 100, 32);
    assert_eq!(count_kv(&db_b), 100);
    assert_eq!(count_kv(&db_a), 4000);
}

// ---------------------------------------------------------------------------
// §8.3 — process-default conflict semantics via the options JSON. One test
// owns the process-wide default (it is per-process state).

#[test]
fn process_default_conflict_semantics() {
    let opts = r#"{"shared_memory": {"total_bytes": 33554432, "memtable_fraction": 0.3}}"#;

    let dir1 = tempfile::tempdir().unwrap();
    let db1 = DbInstance::new("rocksdb", dir1.path(), opts).unwrap();
    // First use fixes the canonical config; property reads flow through.
    assert!(db1.rocksdb_memory_stats().block_cache_usage.is_some());

    // Identical config joins.
    let dir2 = tempfile::tempdir().unwrap();
    let _db2 = DbInstance::new("rocksdb", dir2.path(), opts).unwrap();

    // Differing total_bytes: typed conflict naming both configs.
    let dir3 = tempfile::tempdir().unwrap();
    let err = DbInstance::new(
        "rocksdb",
        dir3.path(),
        r#"{"shared_memory": {"total_bytes": 16777216, "memtable_fraction": 0.3}}"#,
    )
    .err()
    .expect("differing shared_memory config must conflict");
    let msg = format!("{err:?}");
    assert!(msg.contains("conflicting"), "unexpected error: {msg}");
    assert!(
        msg.contains("33554432"),
        "must name the existing config: {msg}"
    );
    assert!(
        msg.contains("16777216"),
        "must name the requested config: {msg}"
    );

    // Omitted memtable_fraction defaults to 0.25 — which differs from the
    // canonical 0.3, so the conflict message proves the default applied.
    let dir4 = tempfile::tempdir().unwrap();
    let err = DbInstance::new(
        "rocksdb",
        dir4.path(),
        r#"{"shared_memory": {"total_bytes": 33554432}}"#,
    )
    .err()
    .expect("defaulted fraction differs from canonical, must conflict");
    let msg = format!("{err:?}");
    assert!(msg.contains("0.25"), "default fraction must be 0.25: {msg}");

    // No shared_memory key: plain open, untouched by the process default.
    let dir5 = tempfile::tempdir().unwrap();
    let _db5 = DbInstance::new("rocksdb", dir5.path(), "").unwrap();

    // Non-rocksdb engines ignore the key entirely.
    let _mem = DbInstance::new("mem", "", opts).unwrap();
}

// ---------------------------------------------------------------------------
// §8.4 — validation: incoherent configs are a typed error at handle
// construction, never a silent clamp.

#[test]
fn validation_errors() {
    let err = RocksMemoryResources::new(RocksMemoryConfig {
        total_bytes: 0,
        memtable_fraction: 0.25,
    })
    .unwrap_err();
    assert_eq!(err, RocksMemoryConfigError::ZeroTotalBytes);

    for bad in [0.0, 1.0, -0.5, 1.5, f64::NAN] {
        let err = RocksMemoryResources::new(RocksMemoryConfig {
            total_bytes: 1 << 20,
            memtable_fraction: bad,
        })
        .unwrap_err();
        assert!(
            matches!(err, RocksMemoryConfigError::MemtableFractionOutOfRange(_)),
            "fraction {bad} must be rejected, got {err:?}"
        );
    }

    // Strictly inside (0,1) is fine, and the default helper validates.
    RocksMemoryResources::new(RocksMemoryConfig {
        total_bytes: 1 << 20,
        memtable_fraction: 0.999,
    })
    .unwrap();
    RocksMemoryResources::new(RocksMemoryConfig::with_total_bytes(1 << 20)).unwrap();
}

// ---------------------------------------------------------------------------
// §8.5 + §8.6 — an options file declaring its own block_cache still opens
// with a handle attached; the shared cache wins and the override is logged
// loudly with both capacities.

#[test]
fn options_file_block_cache_override_is_loud() {
    init_capture();

    let dir = tempfile::tempdir().unwrap();
    {
        let db = new_cozo_rocksdb(dir.path()).unwrap();
        create_kv(&db);
        import_rows(&db, 0, 10, 32);
    }

    // RocksDB never serialises block_cache (kDontSerialize), so patch the
    // generated OPTIONS file to declare one, the way a tuning user would.
    let generated = fs::read_to_string(newest_options_file(dir.path())).unwrap();
    let bbt_header = generated
        .lines()
        .find(|l| l.trim_start().starts_with("[TableOptions/BlockBasedTable"))
        .expect("generated OPTIONS file has a BlockBasedTable section")
        .to_string();
    let patched = generated.replace(&bbt_header, &format!("{bbt_header}\n  block_cache=4194304"));
    fs::write(dir.path().join("options"), patched).unwrap();

    let handle = RocksMemoryResources::new(RocksMemoryConfig {
        total_bytes: 32 << 20,
        memtable_fraction: 0.25,
    })
    .unwrap();
    let db = new_cozo_rocksdb_with_memory(dir.path(), &handle).unwrap();

    // The override fired and named both capacities.
    let warnings = captured().lock().unwrap().clone();
    let hit = warnings.iter().find(|w| {
        w.contains("overrides the options-file block_cache")
            && w.contains("4194304")
            && w.contains(&(32u64 << 20).to_string())
    });
    assert!(
        hit.is_some(),
        "expected a loud override warning naming both capacities, got: {warnings:?}"
    );

    // The instance is attached to the shared cache and fully functional.
    assert_eq!(count_kv(&db), 10);
    import_rows(&db, 10, 100, 32);
    assert_eq!(count_kv(&db), 110);
    let mut ok = false;
    for _ in 0..10 {
        if db.rocksdb_memory_stats().block_cache_usage.unwrap() == handle.cache_usage() {
            ok = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(ok, "instance must report the shared cache's usage");
}

// §8.6 — CF-loop regression: descriptors whose table factory is absent or
// not block-based no longer dereference null when a cache is to be attached.
// Two expressible cases: an UNKNOWN factory name in the TableOptions section
// (the options parser resets `table_factory` to null and carries on —
// "deserialization is optional"), and a PlainTable factory (non-null, but
// `GetOptions<BlockBasedTableOptions>()` returns null). The old loop
// dereferenced both; reaching the open at all is the regression assertion,
// and the engine's unconditional bloom-filter rebuild then restores a
// block-based factory, so the open also succeeds.

#[test]
fn options_file_non_block_based_factory_no_crash() {
    for replacement_header in [
        "[TableOptions/NoSuchTableFactory \"default\"]",
        "[TableOptions/PlainTable \"default\"]",
    ] {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = new_cozo_rocksdb(dir.path()).unwrap();
            create_kv(&db);
            import_rows(&db, 0, 10, 32);
        }

        let generated = fs::read_to_string(newest_options_file(dir.path())).unwrap();
        // Swap the BlockBasedTable section for the replacement factory and
        // drop the (now-mismatched) section body.
        let mut patched = String::new();
        let mut in_bbt_section = false;
        for line in generated.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with('[') {
                in_bbt_section = trimmed.starts_with("[TableOptions/BlockBasedTable");
                if in_bbt_section {
                    patched.push_str(replacement_header);
                    patched.push('\n');
                }
            }
            if !in_bbt_section {
                patched.push_str(line);
                patched.push('\n');
            }
        }
        fs::write(dir.path().join("options"), patched).unwrap();

        let handle = RocksMemoryResources::new(RocksMemoryConfig {
            total_bytes: 16 << 20,
            memtable_fraction: 0.25,
        })
        .unwrap();
        // Old code: null dereference inside the CF cache-attach loop. New
        // code: the loop skips such descriptors; whether the open then
        // succeeds depends only on ordinary options handling.
        match new_cozo_rocksdb_with_memory(dir.path(), &handle) {
            Ok(db) => {
                assert_eq!(count_kv(&db), 10);
            }
            Err(e) => {
                // A clean error is acceptable; crashing is not.
                eprintln!("open with {replacement_header} options file errored cleanly: {e:?}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// §8.7 — property reads on non-rocksdb backends return None, never error.

#[test]
fn non_rocksdb_property_reads_return_none() {
    let db = DbInstance::new("mem", "", "").unwrap();
    let stats = db.rocksdb_memory_stats();
    assert_eq!(stats.block_cache_usage, None);
    assert_eq!(stats.cur_size_all_mem_tables, None);
    assert_eq!(stats.estimate_table_readers_mem, None);
}

// ---------------------------------------------------------------------------
// §8.8 — stall posture: allow_stall=false means the WBM never wedges writers;
// a bounded write storm over budget on two instances keeps making progress
// (WBM-forced flushes are fine, deadlock/stall is not).

#[test]
fn write_storm_over_budget_makes_progress() {
    // Tiny envelope: 8 MB total, 4 MB WBM budget.
    let handle = RocksMemoryResources::new(RocksMemoryConfig {
        total_bytes: 8 << 20,
        memtable_fraction: 0.5,
    })
    .unwrap();
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let db_a = new_cozo_rocksdb_with_memory(dir_a.path(), &handle).unwrap();
    let db_b = new_cozo_rocksdb_with_memory(dir_b.path(), &handle).unwrap();
    create_kv(&db_a);
    create_kv(&db_b);

    // ~4 MB per instance in interleaved batches — crosses the WBM budget
    // several times over the storm.
    let batches = 20i64;
    let rows_per_batch = 200i64;
    for batch in 0..batches {
        let start = batch * rows_per_batch;
        import_rows(&db_a, start, rows_per_batch, 1024);
        import_rows(&db_b, start, rows_per_batch, 1024);
    }
    assert_eq!(count_kv(&db_a), (batches * rows_per_batch) as usize);
    assert_eq!(count_kv(&db_b), (batches * rows_per_batch) as usize);
}
