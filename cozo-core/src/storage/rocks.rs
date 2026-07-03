/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use log::info;
use miette::{miette, IntoDiagnostic, Result, WrapErr};

use cozorocks::{DbBuilder, DbIter, RocksDb, SnapReader, Tx};

use crate::data::tuple::{check_key_for_validity, Tuple};
use crate::data::value::ValidityTs;
use crate::runtime::db::{BadDbInit, DbManifest};
use crate::runtime::relation::{decode_tuple_from_kv, extend_tuple_from_v};
use crate::storage::{Storage, StoreTx};
use crate::utils::swap_option_result;
use crate::Db;

const KEY_PREFIX_LEN: usize = 9;
const CURRENT_STORAGE_VERSION: u64 = 3;

/// Creates a RocksDB database object.
/// This is currently the fastest persistent storage and it can
/// sustain huge concurrency.
/// Supports concurrent readers and writers.
pub fn new_cozo_rocksdb(path: impl AsRef<Path>) -> Result<Db<RocksDbStorage>> {
    let builder = DbBuilder::default().path(path.as_ref());
    fs::create_dir_all(path.as_ref()).map_err(|err| {
        BadDbInit(format!(
            "cannot create directory {}: {}",
            path.as_ref().to_string_lossy(),
            err
        ))
    })?;
    let path_buf = PathBuf::from(path.as_ref());

    let is_new = {
        let mut manifest_path = path_buf.clone();
        manifest_path.push("manifest");

        if manifest_path.exists() {
            let existing: DbManifest = rmp_serde::from_slice(
                &fs::read(manifest_path)
                    .into_diagnostic()
                    .wrap_err_with(|| "when reading manifest")?,
            )
            .into_diagnostic()
            .wrap_err_with(|| "when reading manifest")?;
            assert_eq!(
                existing.storage_version, CURRENT_STORAGE_VERSION,
                "Unknown storage version {}",
                existing.storage_version
            );

            false
        } else {
            fs::write(
                manifest_path,
                rmp_serde::to_vec_named(&DbManifest {
                    storage_version: CURRENT_STORAGE_VERSION,
                })
                .into_diagnostic()
                .wrap_err_with(|| "when serializing manifest")?,
            )
            .into_diagnostic()
            .wrap_err_with(|| "when serializing manifest")?;
            true
        }
    };

    let mut store_path = path_buf.clone();
    store_path.push("data");

    let store_path = store_path
        .to_str()
        .ok_or_else(|| miette!("bad path name"))?;

    let mut options_path = path_buf.clone();
    options_path.push("options");

    let options_path = if Path::exists(&options_path) {
        info!(
            "RockDB storage engine will use options file {}",
            options_path.to_string_lossy()
        );
        options_path
            .to_str()
            .ok_or_else(|| miette!("bad path name"))?
    } else {
        ""
    };

    let db_builder = builder
        .create_if_missing(is_new)
        .use_capped_prefix_extractor(true, KEY_PREFIX_LEN)
        .use_bloom_filter(true, 9.9, true)
        .path(store_path)
        .options_path(options_path);

    let db = db_builder.build()?;

    let ret = Db::new(RocksDbStorage::new(db))?;
    ret.initialize()?;
    Ok(ret)
}

/// RocksDB storage engine
#[derive(Clone)]
pub struct RocksDbStorage {
    db: RocksDb,
}

impl RocksDbStorage {
    pub(crate) fn new(db: RocksDb) -> Self {
        Self { db }
    }
}

impl Storage<'_> for RocksDbStorage {
    type Tx = RocksDbTx;

    fn storage_kind(&self) -> &'static str {
        "rocksdb"
    }

    fn transact(&self, write: bool) -> Result<Self::Tx> {
        // Read-only scripts read the base DB through a plain snapshot instead
        // of a pessimistic transaction (mnestic fork): same consistent view as
        // before — the old read path also pinned one snapshot — but with no
        // lock-manager bookkeeping and no write-batch overlay consulted on
        // every read. Writes keep the pessimistic transaction unchanged.
        let inner = if write {
            RocksTxInner::Txn(self.db.transact().set_snapshot(true).start())
        } else {
            RocksTxInner::Snap(self.db.snapshot_read())
        };
        Ok(RocksDbTx { inner })
    }

    fn range_compact(&self, lower: &[u8], upper: &[u8]) -> Result<()> {
        self.db.range_compact(lower, upper).into_diagnostic()
    }

    fn batch_put<'a>(
        &'a self,
        data: Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>,
    ) -> Result<()> {
        for result in data {
            let (key, val) = result?;
            self.db.raw_put(&key, &val)?;
        }
        Ok(())
    }

    fn supports_sst_ingest(&self) -> bool {
        true
    }

    fn ingest_sorted<'a>(
        &'a self,
        entries: Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>,
    ) -> Result<()> {
        // Build the SST in a sibling of the RocksDB data directory (same
        // filesystem → cheap ingest copy, and a cozo-managed dir RocksDB won't
        // scan). A process-unique, monotonic name avoids collisions; index
        // builds are also serialised by the per-relation write lock. (mnestic fork)
        static SST_SEQ: AtomicU64 = AtomicU64::new(0);
        let data_dir = self.db.db_path();
        let staging_dir = Path::new(&data_dir)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from(&data_dir));
        let seq = SST_SEQ.fetch_add(1, Ordering::Relaxed);
        let sst_path = staging_dir.join(format!("idx_build_{}_{}.sst", std::process::id(), seq));
        let sst_path_str = sst_path
            .to_str()
            .ok_or_else(|| miette!("bad SST staging path"))?;

        let mut writer = self
            .db
            .get_sst_writer(sst_path_str)
            .into_diagnostic()
            .wrap_err("creating SST writer for index build")?;
        let mut wrote_any = false;
        for result in entries {
            let (key, val) = result?;
            writer.put(&key, &val).into_diagnostic()?;
            wrote_any = true;
        }
        if !wrote_any {
            // An empty SST cannot be finished/ingested; nothing to publish.
            return Ok(());
        }
        writer
            .finish()
            .into_diagnostic()
            .wrap_err("finalising SST for index build")?;
        let ingest_res = self
            .db
            .ingest_sst_file(sst_path_str)
            .into_diagnostic()
            .wrap_err("ingesting SST for index build");
        // Best-effort cleanup of the staging file (ingest copies it).
        let _ = fs::remove_file(&sst_path);
        ingest_res
    }
}

pub struct RocksDbTx {
    inner: RocksTxInner,
}

enum RocksTxInner {
    /// Pessimistic transaction — all writing scripts.
    Txn(Tx),
    /// Plain snapshot reads — read-only scripts (mnestic fork).
    Snap(SnapReader),
}

unsafe impl Sync for RocksDbTx {}

impl RocksDbTx {
    #[inline]
    fn read_only_write_err() -> miette::Report {
        miette!("write operation attempted in a read-only transaction")
    }

    #[inline]
    fn iter_builder(&self) -> cozorocks::IterBuilder {
        match &self.inner {
            RocksTxInner::Txn(tx) => tx.iterator(),
            RocksTxInner::Snap(snap) => snap.iterator(),
        }
    }
}

impl<'s> StoreTx<'s> for RocksDbTx {
    #[inline]
    fn get(&self, key: &[u8], for_update: bool) -> Result<Option<Vec<u8>>> {
        match &self.inner {
            RocksTxInner::Txn(tx) => Ok(tx.get(key, for_update)?.map(|v| v.to_vec())),
            RocksTxInner::Snap(snap) => {
                if for_update {
                    return Err(Self::read_only_write_err());
                }
                Ok(snap.get(key)?.map(|v| v.to_vec()))
            }
        }
    }

    fn multi_get(&self, keys: &[Vec<u8>], for_update: bool) -> Result<Vec<Option<Vec<u8>>>> {
        match &self.inner {
            RocksTxInner::Txn(tx) => keys
                .iter()
                .map(|k| Ok(tx.get(k, for_update)?.map(|v| v.to_vec())))
                .collect(),
            RocksTxInner::Snap(snap) => {
                if for_update {
                    return Err(Self::read_only_write_err());
                }
                let refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
                Ok(snap.multi_get(&refs)?)
            }
        }
    }

    #[inline]
    fn put(&mut self, key: &[u8], val: &[u8]) -> Result<()> {
        match &self.inner {
            RocksTxInner::Txn(tx) => Ok(tx.put(key, val)?),
            RocksTxInner::Snap(_) => Err(Self::read_only_write_err()),
        }
    }

    fn put_externally_serialized(&mut self, key: &[u8], val: &[u8]) -> Result<()> {
        match &mut self.inner {
            RocksTxInner::Txn(tx) => {
                // Drop the begin-snapshot so this put validates against the
                // latest state instead of conflicting with a concurrent
                // committer of the same key (see StoreTx doc). Safe: this is
                // the transaction's final write before commit, all user keys
                // were locked (and validated) earlier, and the caller holds
                // the process-wide serialization lock for this key.
                tx.clear_snapshot();
                Ok(tx.put(key, val)?)
            }
            RocksTxInner::Snap(_) => Err(Self::read_only_write_err()),
        }
    }

    fn supports_par_put(&self) -> bool {
        matches!(self.inner, RocksTxInner::Txn(_))
    }

    #[inline]
    fn par_put(&self, key: &[u8], val: &[u8]) -> Result<()> {
        match &self.inner {
            RocksTxInner::Txn(tx) => Ok(tx.put(key, val)?),
            RocksTxInner::Snap(_) => Err(Self::read_only_write_err()),
        }
    }

    #[inline]
    fn del(&mut self, key: &[u8]) -> Result<()> {
        match &self.inner {
            RocksTxInner::Txn(tx) => Ok(tx.del(key)?),
            RocksTxInner::Snap(_) => Err(Self::read_only_write_err()),
        }
    }

    #[inline]
    fn par_del(&self, key: &[u8]) -> Result<()> {
        match &self.inner {
            RocksTxInner::Txn(tx) => Ok(tx.del(key)?),
            RocksTxInner::Snap(_) => Err(Self::read_only_write_err()),
        }
    }

    fn del_range_from_persisted(&mut self, lower: &[u8], upper: &[u8]) -> Result<()> {
        let RocksTxInner::Txn(tx) = &self.inner else {
            return Err(Self::read_only_write_err());
        };
        let mut inner = tx.iterator().upper_bound(upper).start();
        inner.seek(lower);
        while let Some(key) = inner.key()? {
            if key >= upper {
                break;
            }
            tx.del(key)?;
            inner.next();
        }
        Ok(())
    }

    #[inline]
    fn exists(&self, key: &[u8], for_update: bool) -> Result<bool> {
        match &self.inner {
            RocksTxInner::Txn(tx) => Ok(tx.exists(key, for_update)?),
            RocksTxInner::Snap(snap) => {
                if for_update {
                    return Err(Self::read_only_write_err());
                }
                Ok(snap.exists(key)?)
            }
        }
    }

    fn commit(&mut self) -> Result<()> {
        match &mut self.inner {
            RocksTxInner::Txn(tx) => Ok(tx.commit()?),
            // Nothing to commit: the snapshot read view simply ends.
            RocksTxInner::Snap(_) => Ok(()),
        }
    }

    fn range_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<Tuple>>>
    where
        's: 'a,
    {
        let mut inner = self.iter_builder().upper_bound(upper).start();
        inner.seek(lower);
        Box::new(RocksDbIterator {
            inner,
            started: false,
            upper_bound: upper.to_vec(),
        })
    }

    fn range_skip_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
        valid_at: ValidityTs,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        let inner = self.iter_builder().upper_bound(upper).start();
        Box::new(RocksDbSkipIterator {
            inner,
            upper_bound: upper.to_vec(),
            next_bound: lower.to_owned(),
            valid_at,
        })
    }

    fn range_scan<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>>>
    where
        's: 'a,
    {
        let mut inner = self.iter_builder().upper_bound(upper).start();
        inner.seek(lower);
        Box::new(RocksDbIteratorRaw {
            inner,
            started: false,
            upper_bound: upper.to_vec(),
        })
    }

    fn range_count<'a>(&'a self, lower: &[u8], upper: &[u8]) -> Result<usize>
    where
        's: 'a,
    {
        let mut inner = self.iter_builder().upper_bound(upper).start();
        inner.seek(lower);
        let mut count = 0;
        while let Some(k) = inner.key()? {
            if k >= upper {
                break;
            }
            count += 1;
            inner.next();
        }
        Ok(count)
    }

    fn total_scan<'a>(&'a self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>
    where
        's: 'a,
    {
        self.range_scan(&[], &[u8::MAX])
    }
}

pub(crate) struct RocksDbIterator {
    inner: DbIter,
    started: bool,
    upper_bound: Vec<u8>,
}

impl RocksDbIterator {
    #[inline]
    fn next_inner(&mut self) -> Result<Option<Tuple>> {
        if self.started {
            self.inner.next()
        } else {
            self.started = true;
        }
        Ok(match self.inner.pair()? {
            None => None,
            Some((k_slice, v_slice)) => {
                if self.upper_bound.as_slice() <= k_slice {
                    None
                } else {
                    // upper bound is exclusive
                    Some(decode_tuple_from_kv(k_slice, v_slice, None))
                }
            }
        })
    }
}

impl Iterator for RocksDbIterator {
    type Item = Result<Tuple>;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        swap_option_result(self.next_inner())
    }
}

pub(crate) struct RocksDbSkipIterator {
    inner: DbIter,
    upper_bound: Vec<u8>,
    next_bound: Vec<u8>,
    valid_at: ValidityTs,
}

impl RocksDbSkipIterator {
    #[inline]
    fn next_inner(&mut self) -> Result<Option<Tuple>> {
        loop {
            self.inner.seek(&self.next_bound);
            match self.inner.pair()? {
                None => return Ok(None),
                Some((k_slice, v_slice)) => {
                    if self.upper_bound.as_slice() <= k_slice {
                        return Ok(None);
                    }

                    let (ret, nxt_bound) = check_key_for_validity(k_slice, self.valid_at, None);
                    self.next_bound = nxt_bound;
                    if let Some(mut tup) = ret {
                        extend_tuple_from_v(&mut tup, v_slice);
                        return Ok(Some(tup));
                    }
                }
            }
        }
    }
}

impl Iterator for RocksDbSkipIterator {
    type Item = Result<Tuple>;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        swap_option_result(self.next_inner())
    }
}

pub(crate) struct RocksDbIteratorRaw {
    inner: DbIter,
    started: bool,
    upper_bound: Vec<u8>,
}

impl RocksDbIteratorRaw {
    #[inline]
    fn next_inner(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        if self.started {
            self.inner.next()
        } else {
            self.started = true;
        }
        Ok(match self.inner.pair()? {
            None => None,
            Some((k_slice, v_slice)) => {
                if self.upper_bound.as_slice() <= k_slice {
                    // upper bound is exclusive
                    None
                } else {
                    Some((k_slice.to_vec(), v_slice.to_vec()))
                }
            }
        })
    }
}

impl Iterator for RocksDbIteratorRaw {
    type Item = Result<(Vec<u8>, Vec<u8>)>;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        swap_option_result(self.next_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::value::{DataValue, Validity};
    use crate::runtime::db::ScriptMutability;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn setup_test_db() -> Result<(TempDir, Db<RocksDbStorage>)> {
        let temp_dir = TempDir::new().into_diagnostic()?;
        let db = new_cozo_rocksdb(temp_dir.path())?;

        // Create test tables with proper ScriptMutability parameter
        db.run_script(
            r#"
            {:create plain {k: Int => v}}
            {:create tt_test {k: Int, vld: Validity => v}}
            "#,
            Default::default(),
            ScriptMutability::Mutable,
        )?;

        Ok((temp_dir, db))
    }

    #[test]
    fn test_basic_operations() -> Result<()> {
        let (_temp_dir, db) = setup_test_db()?;

        // Test data insertion
        let mut to_import = BTreeMap::new();
        to_import.insert(
            "plain".to_string(),
            crate::NamedRows {
                headers: vec!["k".to_string(), "v".to_string()],
                rows: (0..100)
                    .map(|i| vec![DataValue::from(i), DataValue::from(i * 2)])
                    .collect(),
                next: None,
            },
        );
        db.import_relations(to_import)?;

        // Test simple query with ScriptMutability parameter
        let result = db.run_script(
            "?[v] := *plain{k: 5, v}",
            Default::default(),
            ScriptMutability::Immutable,
        )?;

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], DataValue::from(10));

        Ok(())
    }
    #[test]
    fn test_time_travel() -> Result<()> {
        let (_temp_dir, db) = setup_test_db()?;

        // Insert time travel data
        let mut to_import = BTreeMap::new();
        to_import.insert(
            "tt_test".to_string(),
            crate::NamedRows {
                headers: vec!["k".to_string(), "vld".to_string(), "v".to_string()],
                rows: vec![
                    vec![
                        DataValue::from(1),
                        DataValue::Validity(Validity::from((0, true))),
                        DataValue::from(100),
                    ],
                    vec![
                        DataValue::from(1),
                        DataValue::Validity(Validity::from((1, true))),
                        DataValue::from(200),
                    ],
                ],
                next: None,
            },
        );
        db.import_relations(to_import)?;

        // Query at different timestamps
        let result = db.run_script(
            "?[v] := *tt_test{k: 1, v @ 0}",
            Default::default(),
            ScriptMutability::Immutable,
        )?;
        assert_eq!(result.rows[0][0], DataValue::from(100));

        let result = db.run_script(
            "?[v] := *tt_test{k: 1, v @ 1}",
            Default::default(),
            ScriptMutability::Immutable,
        )?;
        assert_eq!(result.rows[0][0], DataValue::from(200));

        Ok(())
    }

    #[test]
    fn test_range_operations() -> Result<()> {
        let (_temp_dir, db) = setup_test_db()?;

        // Insert test data
        let mut to_import = BTreeMap::new();
        to_import.insert(
            "plain".to_string(),
            crate::NamedRows {
                headers: vec!["k".to_string(), "v".to_string()],
                rows: (0..10)
                    .map(|i| vec![DataValue::from(i), DataValue::from(i)])
                    .collect(),
                next: None,
            },
        );
        db.import_relations(to_import)?;

        // Test range query
        let result = db.run_script(
            "?[k, v] := *plain{k, v}, k >= 3, k < 7",
            Default::default(),
            ScriptMutability::Immutable,
        )?;

        assert_eq!(result.rows.len(), 4);
        assert_eq!(result.rows[0][0], DataValue::from(3));
        assert_eq!(result.rows[3][0], DataValue::from(6));

        Ok(())
    }
}
