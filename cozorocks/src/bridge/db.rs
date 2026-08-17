/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use cxx::*;
use std::path::Path;

use crate::bridge::ffi::*;
use crate::bridge::iter::IterBuilder;
use crate::bridge::tx::{PinSlice, TxBuilder};

#[derive(Default, Clone)]
pub struct DbBuilder {
    pub opts: DbOpts,
}

fn path2buf(path: impl AsRef<Path>) -> Vec<u8> {
    #[cfg(target_os = "windows")]
    {
        // It seems RocksDB expects UTF-8 strings as path even in Windows!
        path.as_ref().to_string_lossy().as_bytes().to_vec()
    }
    #[cfg(not(target_os = "windows"))]
    {
        use std::os::unix::ffi::OsStrExt;
        let path_arr = path.as_ref().as_os_str().as_bytes();
        path_arr.to_vec()
    }
}

impl Default for DbOpts {
    fn default() -> Self {
        Self {
            db_path: vec![],
            options_path: vec![],
            prepare_for_bulk_load: false,
            increase_parallelism: 0,
            optimize_level_style_compaction: false,
            create_if_missing: false,
            paranoid_checks: true,
            enable_blob_files: false,
            min_blob_size: 0,
            blob_file_size: 1 << 28,
            enable_blob_garbage_collection: false,
            use_bloom_filter: false,
            bloom_filter_bits_per_key: 0.0,
            bloom_filter_whole_key_filtering: false,
            use_capped_prefix_extractor: false,
            capped_prefix_extractor_len: 0,
            use_fixed_prefix_extractor: false,
            fixed_prefix_extractor_len: 0,
            destroy_on_exit: false,
            block_cache_size: 0,
        }
    }
}

impl DbBuilder {
    pub fn path(mut self, path: impl AsRef<Path>) -> Self {
        self.opts.db_path = path2buf(path);
        self
    }
    pub fn options_path(mut self, path: impl AsRef<Path>) -> Self {
        self.opts.options_path = path2buf(path);
        self
    }
    pub fn prepare_for_bulk_load(mut self, val: bool) -> Self {
        self.opts.prepare_for_bulk_load = val;
        self
    }
    pub fn increase_parallelism(mut self, val: usize) -> Self {
        self.opts.increase_parallelism = val;
        self
    }
    pub fn optimize_level_style_compaction(mut self, val: bool) -> Self {
        self.opts.optimize_level_style_compaction = val;
        self
    }
    pub fn create_if_missing(mut self, val: bool) -> Self {
        self.opts.create_if_missing = val;
        self
    }
    pub fn paranoid_checks(mut self, val: bool) -> Self {
        self.opts.paranoid_checks = val;
        self
    }
    pub fn enable_blob_files(
        mut self,
        enable: bool,
        min_blob_size: usize,
        blob_file_size: usize,
        garbage_collection: bool,
    ) -> Self {
        self.opts.enable_blob_files = enable;
        self.opts.min_blob_size = min_blob_size;
        self.opts.blob_file_size = blob_file_size;
        self.opts.enable_blob_garbage_collection = garbage_collection;
        self
    }
    pub fn use_bloom_filter(
        mut self,
        enable: bool,
        bits_per_key: f64,
        whole_key_filtering: bool,
    ) -> Self {
        self.opts.use_bloom_filter = enable;
        self.opts.bloom_filter_bits_per_key = bits_per_key;
        self.opts.bloom_filter_whole_key_filtering = whole_key_filtering;
        self
    }
    pub fn use_capped_prefix_extractor(mut self, enable: bool, len: usize) -> Self {
        self.opts.use_capped_prefix_extractor = enable;
        self.opts.capped_prefix_extractor_len = len;
        self
    }
    pub fn use_fixed_prefix_extractor(mut self, enable: bool, len: usize) -> Self {
        self.opts.use_fixed_prefix_extractor = enable;
        self.opts.fixed_prefix_extractor_len = len;
        self
    }
    pub fn build(self) -> Result<RocksDb, RocksDbStatus> {
        let mut status = RocksDbStatus::default();

        let result = open_db(&self.opts, &mut status);
        if status.is_ok() {
            Ok(RocksDb { inner: result })
        } else {
            Err(status)
        }
    }

    /// Open with a shared cross-instance memory handle (mnestic fork,
    /// docs/specs/cross-instance-memory.md §4). The handle's cache and
    /// WriteBufferManager are installed on this instance; any `block_cache`
    /// the options file declares is overridden, which the returned
    /// [`DbOpenReport`] records so the caller can log it loudly.
    pub fn build_with_memory(
        self,
        memory: &RocksMemoryResources,
    ) -> Result<(RocksDb, DbOpenReport), RocksDbStatus> {
        let mut status = RocksDbStatus::default();
        let mut report = DbOpenReport::default();

        let result =
            open_db_with_resources(&self.opts, memory.inner.clone(), &mut status, &mut report);
        if status.is_ok() {
            Ok((RocksDb { inner: result }, report))
        } else {
            Err(status)
        }
    }
}

/// Configuration for a shared cross-instance memory envelope (mnestic fork,
/// docs/specs/cross-instance-memory.md §3). `total_bytes` is THE envelope: one
/// LRU block cache of this capacity is shared by every participating
/// instance, and the WriteBufferManager's budget
/// (`total_bytes * memtable_fraction`) is charged INTO that cache as dummy
/// entries (cost-to-cache) — a carve-out of the envelope, not an addition
/// beside it. Under full memtable pressure, block/index capacity degrades
/// toward `total_bytes * (1 - memtable_fraction)`.
///
/// Charged into the envelope: data blocks, index/filter blocks (high-priority
/// pool), memtable bytes (as WBM dummy entries). NOT charged: table-reader
/// memory outside the cache, WAL, allocator retention, iterators/pinned
/// blocks beyond cache accounting, and everything non-RocksDB. This is a
/// soft, RocksDB-managed envelope, not a whole-process memory limit.
///
/// Pinned in v1 (not configurable): `strict_capacity_limit=false`,
/// `high_pri_pool_ratio` = RocksDB default (0.5),
/// `cache_index_and_filter_blocks_with_high_priority=true` on participating
/// instances, `allow_stall=false`.
#[derive(Debug, Clone, PartialEq)]
pub struct RocksMemoryConfig {
    /// Total size of the envelope in bytes. Must be nonzero.
    pub total_bytes: usize,
    /// Fraction of the envelope carved out for memtables (the
    /// WriteBufferManager's buffer size). Must lie strictly within (0, 1) —
    /// strictly below 1 so block/index capacity survives full memtable
    /// pressure. Default: [`Self::DEFAULT_MEMTABLE_FRACTION`].
    pub memtable_fraction: f64,
}

impl RocksMemoryConfig {
    /// Default memtable carve-out when only a total budget is given.
    pub const DEFAULT_MEMTABLE_FRACTION: f64 = 0.25;

    /// A config with the default memtable carve-out.
    pub fn with_total_bytes(total_bytes: usize) -> Self {
        Self {
            total_bytes,
            memtable_fraction: Self::DEFAULT_MEMTABLE_FRACTION,
        }
    }

    /// Explicit and total validation: incoherent configs are a typed error at
    /// handle construction, never a silent clamp.
    pub fn validate(&self) -> Result<(), RocksMemoryConfigError> {
        if self.total_bytes == 0 {
            return Err(RocksMemoryConfigError::ZeroTotalBytes);
        }
        if !(self.memtable_fraction > 0.0 && self.memtable_fraction < 1.0) {
            return Err(RocksMemoryConfigError::MemtableFractionOutOfRange(
                self.memtable_fraction,
            ));
        }
        Ok(())
    }
}

/// Typed validation error for [`RocksMemoryConfig`].
#[derive(Debug, Clone, PartialEq)]
pub enum RocksMemoryConfigError {
    /// `total_bytes` must be nonzero.
    ZeroTotalBytes,
    /// `memtable_fraction` must lie strictly within (0, 1); carries the
    /// rejected value. (NaN is rejected by the same comparison.)
    MemtableFractionOutOfRange(f64),
}

impl std::fmt::Display for RocksMemoryConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RocksMemoryConfigError::ZeroTotalBytes => {
                write!(f, "shared memory config: total_bytes must be nonzero")
            }
            RocksMemoryConfigError::MemtableFractionOutOfRange(v) => write!(
                f,
                "shared memory config: memtable_fraction must lie strictly within (0, 1), got {v}"
            ),
        }
    }
}

impl std::error::Error for RocksMemoryConfigError {}

/// A live shared cross-instance memory handle (mnestic fork,
/// docs/specs/cross-instance-memory.md §3): one LRU block cache + one
/// WriteBufferManager charged into it, constructed once from a validated
/// [`RocksMemoryConfig`]. Clones are refcounted handles to the same live
/// objects; the handle is immutable after construction. Pass it to
/// [`DbBuilder::build_with_memory`] for every instance that should share the
/// envelope.
#[derive(Clone)]
pub struct RocksMemoryResources {
    pub(crate) inner: SharedPtr<crate::bridge::ffi::RocksMemoryResources>,
    config: RocksMemoryConfig,
}

impl RocksMemoryResources {
    /// Validates `config` and constructs the shared cache + WriteBufferManager.
    pub fn new(config: RocksMemoryConfig) -> Result<Self, RocksMemoryConfigError> {
        config.validate()?;
        let inner = new_memory_resources(config.total_bytes, config.memtable_fraction);
        Ok(Self { inner, config })
    }

    /// The canonical config this handle was constructed from.
    pub fn config(&self) -> &RocksMemoryConfig {
        &self.config
    }

    /// Total memory used by memtables under this WriteBufferManager (bytes).
    pub fn memory_usage(&self) -> u64 {
        self.inner.wbm_memory_usage()
    }

    /// Memory used by active (mutable) memtables (bytes).
    pub fn mutable_memtable_memory_usage(&self) -> u64 {
        self.inner.wbm_mutable_memtable_memory_usage()
    }

    /// Bytes the WriteBufferManager has charged into the shared cache as
    /// dummy entries (the cost-to-cache carve-out, visible inside
    /// [`Self::cache_usage`]).
    pub fn dummy_entries_in_cache_usage(&self) -> u64 {
        self.inner.wbm_dummy_entries_in_cache_usage()
    }

    /// The WriteBufferManager's budget (`total_bytes * memtable_fraction`).
    pub fn buffer_size(&self) -> u64 {
        self.inner.wbm_buffer_size()
    }

    /// The shared cache's capacity — the envelope (`total_bytes`).
    pub fn cache_capacity(&self) -> u64 {
        self.inner.cache_capacity()
    }

    /// The shared cache's current usage (real blocks + WBM dummy entries).
    pub fn cache_usage(&self) -> u64 {
        self.inner.cache_usage()
    }
}

impl std::fmt::Debug for RocksMemoryResources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RocksMemoryResources")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

// SAFETY: the handle only wraps `std::shared_ptr`s to RocksDB's `Cache` and
// `WriteBufferManager`, both of which are internally synchronised and
// explicitly designed to be shared across DB instances and threads; the
// shared_ptr control block is thread-safe. Same discipline as `RocksDb`.
unsafe impl Send for RocksMemoryResources {}

unsafe impl Sync for RocksMemoryResources {}

#[derive(Clone)]
pub struct RocksDb {
    inner: SharedPtr<RocksDbBridge>,
}

impl RocksDb {
    pub fn db_path(&self) -> std::string::String {
        self.inner.get_db_path().to_string_lossy().to_string()
    }
    pub fn transact(&self) -> TxBuilder {
        TxBuilder {
            inner: self.inner.transact(),
        }
    }
    /// Snapshot-pinned read-only surface (mnestic fork): plain snapshot reads
    /// on the base DB — no lock manager, no transaction write-batch overlay.
    /// Iterators created from the reader must be dropped before it.
    pub fn snapshot_read(&self) -> SnapReader {
        SnapReader {
            inner: self.inner.snapshot_read(),
            mg_lock: std::sync::Mutex::new(()),
        }
    }
    #[inline]
    pub fn range_del(&self, lower: &[u8], upper: &[u8]) -> Result<(), RocksDbStatus> {
        let mut status = RocksDbStatus::default();
        self.inner.del_range(lower, upper, &mut status);
        if status.is_ok() {
            Ok(())
        } else {
            Err(status)
        }
    }
    #[inline]
    pub fn raw_put(&self, key: &[u8], val: &[u8]) -> Result<(), RocksDbStatus> {
        let mut status = RocksDbStatus::default();
        self.inner.put(key, val, &mut status);
        if status.is_ok() {
            Ok(())
        } else {
            Err(status)
        }
    }
    #[inline]
    pub fn range_compact(&self, lower: &[u8], upper: &[u8]) -> Result<(), RocksDbStatus> {
        let mut status = RocksDbStatus::default();
        self.inner.compact_range(lower, upper, &mut status);
        if status.is_ok() {
            Ok(())
        } else {
            Err(status)
        }
    }
    pub fn get_sst_writer(&self, path: &str) -> Result<SstWriter, RocksDbStatus> {
        let mut status = RocksDbStatus::default();
        let ret = self.inner.get_sst_writer(path, &mut status);
        if status.is_ok() {
            Ok(SstWriter { inner: ret })
        } else {
            Err(status)
        }
    }
    pub fn ingest_sst_file(&self, path: &str) -> Result<(), RocksDbStatus> {
        let mut status = RocksDbStatus::default();
        self.inner.ingest_sst(path, &mut status);
        if status.is_ok() {
            Ok(())
        } else {
            Err(status)
        }
    }
    /// Int-property read on the base DB (mnestic fork,
    /// docs/specs/cross-instance-memory.md §5). Errors with `kNotFound` when
    /// RocksDB does not recognise the property or has no value for it.
    fn int_property(&self, name: &str) -> Result<u64, RocksDbStatus> {
        let mut status = RocksDbStatus::default();
        let value = self.inner.get_int_property(name, &mut status);
        if status.is_ok() {
            Ok(value)
        } else {
            Err(status)
        }
    }
    /// `rocksdb.block-cache-usage`: memory size of the entries residing in
    /// this instance's block cache (with a shared handle this is the SHARED
    /// cache's usage, including the WriteBufferManager's dummy entries — every
    /// participating instance reports the same value).
    pub fn block_cache_usage(&self) -> Result<u64, RocksDbStatus> {
        self.int_property("rocksdb.block-cache-usage")
    }
    /// `rocksdb.cur-size-all-mem-tables`: approximate size of this instance's
    /// active and unflushed immutable memtables (bytes).
    pub fn cur_size_all_mem_tables(&self) -> Result<u64, RocksDbStatus> {
        self.int_property("rocksdb.cur-size-all-mem-tables")
    }
    /// `rocksdb.estimate-table-readers-mem`: estimated memory this instance
    /// uses for reading SST tables, EXCLUDING block cache (this share is NOT
    /// charged into the shared envelope).
    pub fn estimate_table_readers_mem(&self) -> Result<u64, RocksDbStatus> {
        self.int_property("rocksdb.estimate-table-readers-mem")
    }
}

pub struct SstWriter {
    inner: UniquePtr<SstFileWriterBridge>,
}

impl SstWriter {
    #[inline]
    pub fn put(&mut self, key: &[u8], val: &[u8]) -> Result<(), RocksDbStatus> {
        let mut status = RocksDbStatus::default();
        self.inner.pin_mut().put(key, val, &mut status);
        if status.is_ok() {
            Ok(())
        } else {
            Err(status)
        }
    }
    pub fn finish(&mut self) -> Result<(), RocksDbStatus> {
        let mut status = RocksDbStatus::default();
        self.inner.pin_mut().finish(&mut status);
        if status.is_ok() {
            Ok(())
        } else {
            Err(status)
        }
    }
}

unsafe impl Send for RocksDb {}

unsafe impl Sync for RocksDb {}

/// Read-only view of the database pinned to one snapshot (mnestic fork).
pub struct SnapReader {
    pub(crate) inner: UniquePtr<SnapshotReadBridge>,
    /// Serialises `multi_get`: the bridge keeps per-call scratch (pinned
    /// values) inside the C++ object, so concurrent batch calls must not
    /// interleave. Point reads and iterators are unaffected.
    mg_lock: std::sync::Mutex<()>,
}

impl SnapReader {
    #[inline]
    pub fn get(&self, key: &[u8]) -> Result<Option<PinSlice>, RocksDbStatus> {
        let mut status = RocksDbStatus::default();
        let ret = self.inner.get(key, &mut status);
        match status.code {
            StatusCode::kOk => Ok(Some(PinSlice { inner: ret })),
            StatusCode::kNotFound => Ok(None),
            _ => Err(status),
        }
    }
    #[inline]
    pub fn exists(&self, key: &[u8]) -> Result<bool, RocksDbStatus> {
        let mut status = RocksDbStatus::default();
        self.inner.exists(key, &mut status);
        match status.code {
            StatusCode::kOk => Ok(true),
            StatusCode::kNotFound => Ok(false),
            _ => Err(status),
        }
    }
    /// One RocksDB `MultiGet` over all `keys` (shared filter probes, batched
    /// block reads). Returns one `Option<Vec<u8>>` per key, in order.
    pub fn multi_get(&self, keys: &[&[u8]]) -> Result<Vec<Option<Vec<u8>>>, RocksDbStatus> {
        if keys.is_empty() {
            return Ok(vec![]);
        }
        let mut concat = Vec::with_capacity(keys.iter().map(|k| k.len()).sum());
        let mut lens = Vec::with_capacity(keys.len());
        for k in keys {
            concat.extend_from_slice(k);
            lens.push(k.len() as u64);
        }
        let _guard = self.mg_lock.lock().unwrap();
        let mut status = RocksDbStatus::default();
        self.inner.multi_get(&concat, &lens, &mut status);
        if !status.is_ok() {
            return Err(status);
        }
        let mut out = Vec::with_capacity(keys.len());
        for i in 0..keys.len() {
            let mut val_status = RocksDbStatus::default();
            let val = self.inner.multi_get_val(i as u64, &mut val_status);
            match val_status.code {
                StatusCode::kOk => out.push(Some(val.to_vec())),
                StatusCode::kNotFound => out.push(None),
                _ => return Err(val_status),
            }
        }
        Ok(out)
    }
    #[inline]
    pub fn iterator(&self) -> IterBuilder {
        IterBuilder {
            inner: self.inner.iterator(),
        }
        .auto_prefix_mode(true)
    }
}

// SAFETY: the bridge object is heap-allocated with no thread affinity;
// RocksDB snapshots may be used and released from any thread. The multi_get
// scratch buffers make `&self` calls non-reentrant across threads, but the
// consumer (`RocksDbTx`) is used behind exclusive or externally-synchronised
// access, matching the existing `Tx` discipline.
unsafe impl Send for SnapReader {}
unsafe impl Sync for SnapReader {}
