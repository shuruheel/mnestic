/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::any::Any;
use std::cmp::Ordering;
use std::fmt::Debug;
use std::sync::Arc;

use miette::{bail, Result};

use crate::data::tuple::Tuple;
use crate::data::value::ValidityTs;
use crate::try_decode_tuple_from_kv;

// `storage-rocksdb` links a RocksDB built from the vendored submodule through `cozorocks`.
// `storage-new-rocksdb` and `storage-layered` link a second one through `librocksdb-sys`. Both
// are static, both export the same C++ symbols, and they are built from different RocksDB
// releases, so a binary holding both resolves calls across mismatched layouts and dies with a
// segmentation fault rather than a test failure.
//
// Cargo features are additive and cannot express the exclusion, so it is rejected here. This
// makes `--all-features` fail to build, which is the point: it used to segfault.
#[cfg(all(feature = "storage-rocksdb", feature = "storage-new-rocksdb"))]
compile_error!(
    "features `storage-rocksdb` and `storage-new-rocksdb` cannot be enabled together: each \
     links its own static build of RocksDB, and the two export the same symbols"
);
#[cfg(all(feature = "storage-rocksdb", feature = "storage-layered"))]
compile_error!(
    "features `storage-rocksdb` and `storage-layered` cannot be enabled together: each links \
     its own static build of RocksDB, and the two export the same symbols"
);

#[cfg(feature = "storage-layered")]
pub mod layered;
pub(crate) mod mem;
#[cfg(feature = "storage-new-rocksdb")]
pub mod newrocks;
#[cfg(feature = "storage-rocksdb")]
pub(crate) mod rocks;
#[cfg(feature = "storage-sled")]
pub(crate) mod sled;
#[cfg(feature = "storage-sqlite")]
pub(crate) mod sqlite;
pub(crate) mod temp;
#[cfg(feature = "storage-tikv")]
pub(crate) mod tikv;
// pub(crate) mod re;

/// An engine's identity for one of the views it presents.
///
/// Implemented by the engine, never inspected outside it. The only thing anything else may do
/// with one is compare it to another, and the only promise an engine makes is that two of its
/// transactions get equal identities exactly when they see equal content.
///
/// An implementor orders only against its own type and answers `None` otherwise; [`StorageView`]
/// settles those by concrete type instead. So no implementor has to invent an order against
/// types it has never heard of, and the common case costs one downcast rather than a type
/// comparison followed by one.
pub trait ViewIdentity: Any + Debug + Send + Sync {
    /// Order this identity against another, or `None` if `other` is not the same type.
    fn view_cmp(&self, other: &dyn ViewIdentity) -> Option<Ordering>;
}

/// Which view of the store a transaction reads.
///
/// Engine-level caches key their entries on relation identity plus a content version, which is
/// sound only while every transaction sees the same content for a relation at a given version.
/// An engine whose transactions can disagree without any write between them breaks that, so it
/// distinguishes its views here and the caches key on this as well.
///
/// The identity type belongs to the engine: what divides a store is the engine's business, and
/// differs entirely between one that shards, one that reads a replica with lag, and one that
/// composes layers. An engine that presents its store whole reports [`StorageView::undivided`],
/// which is what the default implementation does.
#[derive(Clone, Debug, Default)]
pub struct StorageView(Option<Arc<dyn ViewIdentity>>);

impl StorageView {
    /// The store presented whole: one view, nothing to tell apart.
    pub fn undivided() -> Self {
        StorageView(None)
    }

    /// An engine's identity for one of several views it presents.
    pub fn of(identity: Arc<dyn ViewIdentity>) -> Self {
        StorageView(Some(identity))
    }
}

impl Ord for StorageView {
    fn cmp(&self, other: &Self) -> Ordering {
        match (&self.0, &other.0) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (Some(a), Some(b)) => a.view_cmp(b.as_ref()).unwrap_or_else(|| {
                // Different engines' identities, which only meet if a process runs more than
                // one engine. Ordering by concrete type keeps the total order total; nothing
                // depends on which type sorts first.
                Any::type_id(a.as_ref()).cmp(&Any::type_id(b.as_ref()))
            }),
        }
    }
}

impl PartialOrd for StorageView {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for StorageView {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for StorageView {}

/// Swappable storage trait for Cozo's storage engine
pub trait Storage<'s>: Send + Sync + Clone {
    /// The associated transaction type used by this engine
    type Tx: StoreTx<'s>;

    /// Returns a string that identifies the storage kind
    fn storage_kind(&self) -> &'static str;

    /// What `'NOW'` means to this engine when a script is run.
    ///
    /// The default is wall-clock time, which is what a single-store engine has to use. An
    /// engine that assigns validity itself (the layered engine stamps commit order) returns
    /// its own marker instead, so that scripts run through the ordinary entry points are
    /// stamped the same way as those run through the engine's own.
    fn now_validity(&self) -> ValidityTs {
        crate::data::functions::current_validity()
    }

    /// Create a transaction object. Write ops will only be called when `write == true`.
    fn transact(&'s self, write: bool) -> Result<Self::Tx>;

    /// Compact the key range. Can be a no-op if the storage engine does not
    /// have the concept of compaction.
    fn range_compact(&'s self, lower: &[u8], upper: &[u8]) -> Result<()>;

    /// Put multiple key-value pairs into the database.
    /// No duplicate data will be sent, and the order data come in is strictly ascending.
    /// There will be no other access to the database while this function is running.
    fn batch_put<'a>(
        &'a self,
        data: Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>,
    ) -> Result<()>;

    /// Whether this engine can bulk-publish a freshly-built index by ingesting a
    /// sorted-string table (SST) file — see [`ingest_sorted`](Self::ingest_sorted).
    /// Defaults to `false`; only the RocksDB backend overrides this. (mnestic fork)
    fn supports_sst_ingest(&self) -> bool {
        false
    }

    /// Request that write transactions **fsync on commit** from now on (or stop
    /// doing so, with `false`). Returns `true` iff this backend honors the
    /// knob; the default implementation is a no-op returning `false`, so
    /// third-party [`Storage`] impls keep compiling. (mnestic fork)
    ///
    /// The contract this makes explicit: the **RocksDB backend commits without
    /// syncing its WAL by default** — the log is written but not fsynced, so an
    /// OS crash or power loss can lose the most recent commits (a mere process
    /// crash cannot; the OS still has the pages). With `set_durable_writes
    /// (true)` every subsequent write transaction's commit fsyncs first —
    /// slower, but a power cut loses nothing acknowledged.
    ///
    /// Per-backend behavior:
    /// - `rocksdb` — honored (returns `true`); flips per-transaction
    ///   `WriteOptions.sync`. NOTE: the bulk paths (`batch_put`,
    ///   `ingest_sorted`, i.e. `import_relations`/restore) are separate
    ///   non-transactional channels and are **not** covered by the knob.
    /// - `sqlite` — not honored (returns `false`), but writes are already
    ///   durable: it runs with SQLite's default `synchronous=FULL`.
    /// - `mem` — nothing to sync; not persistent. Returns `false`.
    /// - `newrocksdb`, `sled`, `tikv` — not wired up (returns `false`).
    fn set_durable_writes(&self, durable: bool) -> bool {
        let _ = durable;
        false
    }

    /// Bulk-publish strictly-ascending key-value `entries` into the *live*
    /// database by building an SST file and atomically ingesting it, bypassing
    /// the transaction write-batch overlay entirely. The engine manages the
    /// temporary file. Keys MUST arrive in strictly ascending order. (mnestic fork)
    ///
    /// Unlike a transactional `put`, ingested keys become visible to new reads
    /// as soon as this returns, independent of any open transaction's commit.
    /// Callers relying on this for index publishing must therefore ingest the
    /// index data *before* the metadata that references it becomes visible.
    ///
    /// The default implementation errors; engines without SST support should use
    /// the per-key `put` path instead.
    fn ingest_sorted<'a>(
        &'a self,
        _entries: Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>,
    ) -> Result<()> {
        bail!("this storage engine does not support SST ingest")
    }
}

/// Trait for the associated transaction type of a storage engine.
/// A transaction needs to guarantee MVCC semantics for all operations.
pub trait StoreTx<'s>: Sync {
    /// Get a key. If `for_update` is `true` (only possible in a write transaction),
    /// then the database needs to guarantee that `commit()` can only succeed if
    /// the key has not been modified outside the transaction.
    fn get(&self, key: &[u8], for_update: bool) -> Result<Option<Vec<u8>>>;

    /// Get multiple keys. If `for_update` is `true` (only possible in a write transaction),
    /// then the database needs to guarantee that `commit()` can only succeed if
    /// the keys have not been modified outside the transaction.
    /// Which view of the store this transaction reads, for caches that would otherwise
    /// conflate two transactions seeing different content. Defaults to
    /// [`StorageView::undivided`]; only the layered backend composes more than one view.
    fn storage_view(&self) -> StorageView {
        StorageView::undivided()
    }

    fn multi_get(&self, keys: &[Vec<u8>], for_update: bool) -> Result<Vec<Option<Vec<u8>>>> {
        keys.iter().map(|k| self.get(k, for_update)).collect()
    }

    /// Put a key-value pair into the storage. In case of existing key,
    /// the storage engine needs to overwrite the old value.
    fn put(&mut self, key: &[u8], val: &[u8]) -> Result<()>;

    /// Put a key whose writes are serialized by a higher-level authority
    /// (mnestic fork; sole current user: the tt commit clock's high-water
    /// mark, `runtime/tt_clock.rs`). Backends whose write transactions
    /// snapshot-validate first-locks (RocksDB pessimistic transactions) must
    /// skip that validation for this key — otherwise any two temporally
    /// overlapping transactions writing it make the later committer abort
    /// spuriously (`Resource busy`; the 0.8.4 `avgdl` hot-key failure mode).
    /// Default: a plain `put` (sqlite/mem take storage-level write locks, so
    /// overlapping write transactions cannot exist there).
    fn put_externally_serialized(&mut self, key: &[u8], val: &[u8]) -> Result<()> {
        self.put(key, val)
    }

    /// Should return true if the engine supports parallel put, false otherwise.
    fn supports_par_put(&self) -> bool;

    /// Put a key-value pair into the storage. In case of existing key,
    /// the storage engine needs to overwrite the old value.
    /// The difference between this one and `put` is the mutability of self.
    /// It is OK to always panic if `supports_par_put` returns `false`.
    fn par_put(&self, _key: &[u8], _val: &[u8]) -> Result<()> {
        panic!("par_put is not supported")
    }

    /// Delete a key-value pair from the storage.
    fn del(&mut self, key: &[u8]) -> Result<()>;

    /// Delete a key-value pair from the storage.
    /// The difference between this one and `del` is the mutability of self.
    /// It is OK to always panic if `supports_par_put` returns `false`.
    fn par_del(&self, _key: &[u8]) -> Result<()> {
        panic!("par_del is not supported")
    }

    /// Delete a range from persisted data only.
    fn del_range_from_persisted(&mut self, lower: &[u8], upper: &[u8]) -> Result<()>;

    /// Check if a key exists. If `for_update` is `true` (only possible in a write transaction),
    /// then the database needs to guarantee that `commit()` can only succeed if
    /// the key has not been modified outside the transaction.
    fn exists(&self, key: &[u8], for_update: bool) -> Result<bool>;

    /// Commit a transaction. Must return an `Err` if MVCC consistency cannot be guaranteed,
    /// and discard all changes introduced by this transaction.
    fn commit(&mut self) -> Result<()>;

    /// Scan on a range. `lower` is inclusive whereas `upper` is exclusive.
    /// The default implementation calls [`range_scan_owned`](Self::range_scan) and converts the results.
    ///
    /// The implementation must call
    /// [`try_decode_tuple_from_kv`](crate::try_decode_tuple_from_kv) to obtain a decoded tuple in
    /// the loop of the iterator.
    fn range_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a>
    where
        's: 'a,
    {
        let it = self.range_scan(lower, upper);
        Box::new(it.map(|pair| {
            let (key, value) = pair?;
            try_decode_tuple_from_kv(&key, &value, None)
        }))
    }

    /// Scan on a range with a certain validity.
    ///
    /// `lower` is inclusive whereas `upper` is exclusive.
    /// For tuples that differ only with respect to their validity, which must be at
    /// the last slot of the key,
    /// only the tuple that has validity equal to or earlier than (i.e. greater by the comparator)
    /// `valid_at` should be considered for returning, and only those with an assertive validity
    /// should be returned. Every other tuple should be skipped.
    ///
    /// Ideally, implementations should take advantage of seeking capabilities of the
    /// underlying storage so that not every tuple within the `lower` and `upper` range
    /// need to be looked at.
    ///
    /// For custom implementations, it is OK to return an iterator that always error out,
    /// in which case the database with the engine does not support time travelling.
    /// You should indicate this clearly in your error message.
    fn range_skip_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
        valid_at: ValidityTs,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a>;

    /// Two-level bitemporal scan (mnestic fork, bitemporality step 4b; see
    /// `data/bitemporal.rs`). The default implementation drives the generic
    /// probe loop over `range_scan` — one fresh range per probe. Correct on
    /// every backend; hot backends may override with a pinned-iterator seek
    /// loop (step 6 measures before optimizing).
    fn range_bitemporal_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
        vt_at: Option<ValidityTs>,
        tt_at: ValidityTs,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        let upper = upper.to_vec();
        Box::new(crate::data::bitemporal::BitemporalIter::new(
            move |bound: &[u8], _far: bool| -> Result<Option<(Vec<u8>, Vec<u8>)>> {
                // A probe bound past the range end means the walk is done —
                // never hand an inverted range to the backend (mem's
                // BTreeMap::range panics on start > end).
                if bound >= upper.as_slice() {
                    return Ok(None);
                }
                match self.range_scan(bound, &upper).next() {
                    None => Ok(None),
                    Some(kv) => Ok(Some(kv?)),
                }
            },
            lower.to_vec(),
            vt_at,
            tt_at,
        ))
    }

    /// Scan on a range and return the raw results.
    /// `lower` is inclusive whereas `upper` is exclusive.
    fn range_scan<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>
    where
        's: 'a;

    /// Return the number of rows in the range.
    fn range_count<'a>(&'a self, lower: &[u8], upper: &[u8]) -> Result<usize>
    where
        's: 'a;

    /// Scan for all rows. The rows are required to be in ascending order.
    fn total_scan<'a>(&'a self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>
    where
        's: 'a;
}

#[cfg(test)]
mod view_tests {
    use std::cmp::Ordering;
    use std::sync::Arc;

    use super::{StorageView, ViewIdentity};

    /// Two engines, so the cross-type path is reachable. Neither knows the other exists, which
    /// is the point: each answers only for its own type.
    #[derive(Debug)]
    struct Alpha(u64);
    #[derive(Debug)]
    struct Beta(u64);

    impl ViewIdentity for Alpha {
        fn view_cmp(&self, other: &dyn ViewIdentity) -> Option<Ordering> {
            (other as &dyn std::any::Any)
                .downcast_ref::<Alpha>()
                .map(|other| self.0.cmp(&other.0))
        }
    }
    impl ViewIdentity for Beta {
        fn view_cmp(&self, other: &dyn ViewIdentity) -> Option<Ordering> {
            (other as &dyn std::any::Any)
                .downcast_ref::<Beta>()
                .map(|other| self.0.cmp(&other.0))
        }
    }

    fn alpha(n: u64) -> StorageView {
        StorageView::of(Arc::new(Alpha(n)))
    }
    fn beta(n: u64) -> StorageView {
        StorageView::of(Arc::new(Beta(n)))
    }

    #[test]
    fn an_engine_orders_its_own_views() {
        assert_eq!(alpha(1), alpha(1));
        assert_ne!(alpha(1), alpha(2));
        assert!(alpha(1) < alpha(2));
    }

    #[test]
    fn the_undivided_store_is_its_own_view() {
        assert_eq!(StorageView::undivided(), StorageView::undivided());
        assert_ne!(StorageView::undivided(), alpha(0));
        assert!(StorageView::undivided() < alpha(0));
    }

    /// Identities from different engines never compare equal, and the order between them is
    /// antisymmetric. A `BTreeMap` keyed on these would corrupt silently otherwise.
    #[test]
    fn identities_from_different_engines_are_ordered_not_conflated() {
        assert_ne!(alpha(7), beta(7), "same number, different engines");
        assert_eq!(
            alpha(7).cmp(&beta(7)).reverse(),
            beta(7).cmp(&alpha(7)),
            "the order between two engines is not antisymmetric"
        );
        // Whichever way the types sort, it must be consistent across values.
        let first = alpha(0).cmp(&beta(0));
        for (a, b) in [(0, 99), (99, 0), (5, 5)] {
            assert_eq!(
                alpha(a).cmp(&beta(b)),
                first,
                "type ordering must not depend on the values"
            );
        }
    }

    /// The ordering is total, which is what a map keyed on it requires.
    #[test]
    fn mixed_identities_sort_into_a_stable_total_order() {
        use std::collections::BTreeSet;
        let mut set = BTreeSet::new();
        for v in [alpha(2), beta(1), StorageView::undivided(), alpha(1), beta(2)] {
            assert!(set.insert(v), "a distinct view collided with one already present");
        }
        assert_eq!(set.len(), 5);
        // Re-inserting an equal identity must be recognised as already present.
        assert!(!set.insert(alpha(1)));
        assert!(!set.insert(StorageView::undivided()));
    }
}
