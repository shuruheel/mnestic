/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file, You can obtain one at
 * https://mozilla.org/MPL/2.0/.
 */

//! Detached current-belief search-index generations and atomic publication.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use miette::{ensure, Result};

use crate::data::tuple::{Tuple, TupleT};
use crate::parse::sys::SysOp;
use crate::runtime::db::{Db, Poison};
use crate::runtime::relation::{AccessLevel, RelationHandle};
use crate::runtime::transact::SessionTx;
use crate::runtime::txtime_index::{BuildRegistration, TouchedRelation};
use crate::storage::Storage;
use crate::{DataValue, NamedRows};

pub(crate) fn section<'a>(lock: &'a Mutex<()>, poison: &Poison) -> Result<MutexGuard<'a, ()>> {
    loop {
        poison.check()?;
        match lock.try_lock() {
            Ok(guard) => {
                poison.check()?;
                return Ok(guard);
            }
            Err(TryLockError::Poisoned(error)) => {
                poison.check()?;
                return Ok(error.into_inner());
            }
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(2)),
        }
    }
}

fn relation_write<'a>(
    lock: &'a crossbeam::sync::ShardedLock<()>,
    poison: &Poison,
) -> Result<crossbeam::sync::ShardedLockWriteGuard<'a, ()>> {
    loop {
        poison.check()?;
        match lock.try_write() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(error)) => return Ok(error.into_inner()),
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(2)),
        }
    }
}

fn index_ids(handle: &RelationHandle, name: &str) -> Option<Vec<u64>> {
    if let Some((index, _)) = handle.fts_indices.get(name) {
        return Some(vec![index.id.0]);
    }
    if let Some((index, _)) = handle.hnsw_indices.get(name) {
        return Some(vec![index.id.0]);
    }
    if let Some((index, inverse, _)) = handle.lsh_indices.get(name) {
        return Some(vec![index.id.0, inverse.id.0]);
    }
    if let Some((index, _)) = handle.indices.get(name) {
        return Some(vec![index.id.0]);
    }
    None
}

fn names(handle: &RelationHandle) -> BTreeSet<String> {
    handle
        .fts_indices
        .keys()
        .chain(handle.hnsw_indices.keys())
        .chain(handle.lsh_indices.keys())
        .map(ToString::to_string)
        .collect()
}

fn handles(handle: &RelationHandle) -> Vec<&RelationHandle> {
    handle
        .fts_indices
        .values()
        .map(|(i, _)| i)
        .chain(handle.hnsw_indices.values().map(|(i, _)| i))
        .chain(handle.lsh_indices.values().flat_map(|(i, j, _)| [i, j]))
        .collect()
}

fn validate(
    original: &RelationHandle,
    current: &RelationHandle,
    build: &RelationHandle,
) -> Result<()> {
    ensure!(
        current.id == original.id && current.metadata == original.metadata,
        "transaction-time index build base relation changed identity or schema"
    );
    ensure!(
        current.access_level >= AccessLevel::Normal,
        "insufficient access to publish transaction-time index"
    );
    for name in names(build) {
        ensure!(
            index_ids(original, &name) == index_ids(current, &name),
            "index generation changed during transaction-time build: {}",
            name
        );
    }
    Ok(())
}

impl SessionTx<'_> {
    fn prepare_tt_indexes(
        &mut self,
        original: &RelationHandle,
        op: &SysOp,
    ) -> Result<RelationHandle> {
        ensure!(
            original.access_level >= AccessLevel::Normal,
            "insufficient access to create transaction-time index"
        );
        let mut definition = match op {
            SysOp::Reindex(_) => {
                let mut rebuilt = original.clone();
                for (index, _) in rebuilt.fts_indices.values_mut() {
                    *index = self.reserve_detached_index(index)?;
                }
                for (index, _) in rebuilt.hnsw_indices.values_mut() {
                    *index = self.reserve_detached_index(index)?;
                }
                for (index, inverse, _) in rebuilt.lsh_indices.values_mut() {
                    *index = self.reserve_detached_index(index)?;
                    *inverse = self.reserve_detached_index(inverse)?;
                }
                rebuilt
            }
            _ => {
                self.preparing_tt_index = true;
                match op {
                    SysOp::CreateFtsIndex(config) => self.create_fts_index(config)?,
                    SysOp::CreateVectorIndex(config) => {
                        self.create_hnsw_index(config)?;
                    }
                    SysOp::CreateMinHashLshIndex(config) => {
                        self.create_minhash_lsh_index(config)?
                    }
                    _ => unreachable!(),
                }
                self.preparing_tt_index = false;
                self.prepared_tt_index
                    .take()
                    .expect("prepared index definition")
            }
        };
        // Retain only the newly reserved generations. Existing indexes remain
        // attached and maintained by ordinary commits throughout backfill.
        definition.indices.clear();
        definition.fts_indices.retain(|name, (index, _)| {
            original
                .fts_indices
                .get(name)
                .is_none_or(|(old, _)| old.id != index.id)
        });
        definition.hnsw_indices.retain(|name, (index, _)| {
            original
                .hnsw_indices
                .get(name)
                .is_none_or(|(old, _)| old.id != index.id)
        });
        definition.lsh_indices.retain(|name, (index, _, _)| {
            original
                .lsh_indices
                .get(name)
                .is_none_or(|(old, _, _)| old.id != index.id)
        });
        Ok(definition)
    }

    fn populate_tt_indexes(
        &mut self,
        build: &RelationHandle,
        rows: &[Tuple],
        poison: &Poison,
    ) -> Result<()> {
        let filters = Self::make_hnsw_filters(build)?;
        let processors = self.make_fts_lsh_processors(build)?;
        let perms = self.make_lsh_hash_perms(build);
        for (name, (index, manifest)) in &build.hnsw_indices {
            poison.check()?;
            self.hnsw_build_index(manifest, build, index, filters.get(name), rows)?;
        }
        let mut stack = Vec::new();
        for row in rows {
            poison.check()?;
            self.put_in_fts(build, &mut stack, &processors, row)?;
            self.put_in_lsh(build, &mut stack, &processors, row, &perms)?;
        }
        Ok(())
    }

    fn publish_tt_indexes(
        &mut self,
        mut current: RelationHandle,
        build: &RelationHandle,
    ) -> Result<()> {
        for index in handles(build) {
            self.publish_relation_metadata(index)?;
        }
        current.fts_indices.extend(build.fts_indices.clone());
        current.hnsw_indices.extend(build.hnsw_indices.clone());
        current.lsh_indices.extend(build.lsh_indices.clone());
        self.publish_relation_metadata(&current)
    }
}

fn result_rows(op: &SysOp, build: &RelationHandle, count: usize) -> NamedRows {
    if !matches!(op, SysOp::Reindex(_)) {
        return NamedRows::new(vec!["status".into()], vec![vec![DataValue::from("OK")]]);
    }
    let reports: Vec<_> = build
        .fts_indices
        .keys()
        .map(|n| (n, "fts"))
        .chain(build.hnsw_indices.keys().map(|n| (n, "hnsw")))
        .chain(build.lsh_indices.keys().map(|n| (n, "lsh")))
        .map(|(name, kind)| crate::runtime::reindex::ReindexReport {
            index: name.clone(),
            kind,
            rows: count,
        })
        .collect();
    if reports.is_empty() {
        return NamedRows::new(
            vec!["status".into()],
            vec![vec![DataValue::from(
                "no HNSW/FTS/LSH index on this relation — nothing to rebuild",
            )]],
        );
    }
    let (headers, rows) = crate::runtime::reindex::reindex_rows(reports);
    NamedRows::new(headers, rows)
}

impl<'s, S: Storage<'s>> Db<S> {
    /// Handles only tt-only search create/rebuild. Other operations retain their
    /// existing dispatch and diagnostics, including bitemporal rejection.
    pub(crate) fn run_tt_index_op(
        &'s self,
        op: &SysOp,
        read_only: bool,
        deadline: Option<Instant>,
    ) -> Result<Option<NamedRows>> {
        let name = match op {
            SysOp::CreateFtsIndex(config) => config.base_relation.as_str(),
            SysOp::CreateVectorIndex(config) => config.base_relation.as_str(),
            SysOp::CreateMinHashLshIndex(config) => config.base_relation.as_str(),
            SysOp::Reindex(name) => name.name.as_str(),
            _ => return Ok(None),
        };
        if read_only {
            return Ok(None);
        }
        let original = {
            let tx = self.transact()?;
            tx.get_relation(name, false)?
        };
        if !original.is_tt_only() {
            return Ok(None);
        }
        let poison = Poison {
            deadline,
            ..Default::default()
        };
        let locks = self.obtain_relation_locks(std::iter::once(&original.name));
        if !self.db.supports_sst_ingest() {
            // Admission before the commit section: SQLite/mem may already have
            // a writer waiting for that section. Never invert those locks.
            let _relation = relation_write(&locks[0], &poison)?;
            let mut tx = self.transact_write()?;
            let current = tx.get_relation(name, true)?;
            let _section = section(&self.tt_commit_lock, &poison)?;
            let build = tx.prepare_tt_indexes(&current, op)?;
            let rows = current.scan_current(&tx).collect::<Result<Vec<_>>>()?;
            tx.populate_tt_indexes(&build, &rows, &poison)?;
            tx.publish_tt_indexes(current.clone(), &build)?;
            poison.check()?;
            self.commit_tx_with_test_hook(&mut tx)?;
            let result = result_rows(op, &build, rows.len());
            drop(tx);
            drop(_section);
            drop(_relation);
            self.cleanup_tt_generations(&current, &names(&build));
            return Ok(Some(result));
        }

        // Durable reservation only; no public index name exists yet.
        let (original, build) = {
            let _relation = relation_write(&locks[0], &poison)?;
            let mut tx = self.transact_write()?;
            let current = tx.get_relation(name, true)?;
            ensure!(
                current.id == original.id,
                "base relation changed before index preparation"
            );
            let build = tx.prepare_tt_indexes(&current, op)?;
            self.commit_tx_with_test_hook(&mut tx)?;
            (current, build)
        };
        let mut publication_attempted = false;
        let outcome = (|| {
            // Pin the actual read snapshot and registration in one commit gap.
            let (snapshot, registration) = {
                let _section = section(&self.tt_commit_lock, &poison)?;
                let snapshot = self.transact()?;
                let current = snapshot.get_relation(name, false)?;
                validate(&original, &current, &build)?;
                let registration = BuildRegistration::register(
                    self.tt_index_builds.clone(),
                    original.id.0,
                    snapshot.read_persisted_tt_hwm()?.unwrap_or_default(),
                    names(&build),
                );
                (snapshot, registration)
            };
            #[cfg(feature = "test-hooks")]
            self.run_tt_index_build_fence("registered");
            let rows = original
                .scan_current(&snapshot)
                .collect::<Result<Vec<_>>>()?;
            let mut count = rows.len();
            {
                let mut tx = self.transact_write()?;
                tx.populate_tt_indexes(&build, &rows, &poison)?;
                self.commit_tx_with_test_hook(&mut tx)?;
            }
            drop(rows);
            #[cfg(feature = "test-hooks")]
            self.run_tt_index_build_fence("populated");
            {
                let _relation = relation_write(&locks[0], &poison)?;
                let mut tx = self.transact_write()?;
                // Catalog lock before the commit section, never while owning it.
                #[cfg(feature = "test-hooks")]
                self.run_tt_index_build_fence("publication_before_catalog");
                let current = tx.get_relation(name, true)?;
                // Descriptions/access metadata may lock the public index
                // catalog independently of the base. Never wait for those
                // locks while owning the database-wide TT commit section.
                let mut index_names: Vec<_> =
                    handles(&build).into_iter().map(|i| &i.name).collect();
                index_names.sort();
                for index_name in index_names {
                    let key = vec![DataValue::Str(index_name.clone())]
                        .encode_as_key(crate::runtime::relation::RelationId::SYSTEM);
                    tx.store_tx.get(&key, true)?;
                }
                let _section = section(&self.tt_commit_lock, &poison)?;
                validate(&original, &current, &build)?;
                let mut old = BTreeMap::new();
                for key in registration.keys()? {
                    let prior = original.get_current(&snapshot, &key)?;
                    let now = current.get_current(&tx, &key)?;
                    count = count + usize::from(now.is_some()) - usize::from(prior.is_some());
                    old.insert(key.encode_as_key(build.id), (key, prior));
                }
                let changes = BTreeMap::from([(
                    build.id.0,
                    TouchedRelation {
                        handle: build.clone(),
                        old,
                    },
                )]);
                tx.maintain_tt_indexes(&changes)?;
                tx.publish_tt_indexes(current, &build)?;
                poison.check()?;
                publication_attempted = true;
                self.commit_tx_with_test_hook(&mut tx)?;
            }
            drop(registration);
            Ok(result_rows(op, &build, count))
        })();
        match outcome {
            Ok(rows) => {
                self.cleanup_tt_generations(&original, &names(&build));
                Ok(Some(rows))
            }
            Err(error) => {
                // Storage commit errors can be ambiguous. Never erase a
                // generation that might already be attached in the catalog.
                if !publication_attempted {
                    self.cleanup_tt_generations(&build, &names(&build));
                }
                Err(error)
            }
        }
    }

    /// Detached or retired ids only. After successful attachment a cleanup
    /// failure cannot change the reported commit outcome. RocksDB tombstones
    /// preserve old readers' pinned snapshots.
    fn cleanup_tt_generations(&'s self, handle: &RelationHandle, selected: &BTreeSet<String>) {
        let result = (|| {
            let mut tx = self.transact_write()?;
            for index in handles(handle) {
                let belongs = selected.iter().any(|name| {
                    index_ids(handle, name).is_some_and(|ids| ids.contains(&index.id.0))
                });
                if belongs {
                    let lower = Tuple::new().encode_as_key(index.id);
                    let upper = Tuple::new().encode_as_key(index.id.next());
                    tx.store_tx.del_range_from_persisted(&lower, &upper)?;
                }
            }
            tx.commit_tx()
        })();
        if let Err(error) = result {
            crate::runtime::diagnostics::emit(
                "txtime.index_cleanup_deferred",
                format!("Detached index cleanup failed: {error}"),
                "Retry maintenance after resolving the storage error".to_string(),
            );
        }
    }
}

impl<'s, S: Storage<'s>> Db<S> {
    pub(crate) fn run_tt_metadata_op(
        &'s self,
        op: &SysOp,
        read_only: bool,
        deadline: Option<Instant>,
    ) -> Result<Option<NamedRows>> {
        if read_only {
            return Ok(None);
        }
        let mut targets = BTreeSet::new();
        match op {
            SysOp::RemoveIndex(base, _) | SysOp::RepairCorrupt(base) => {
                targets.insert(base.name.clone());
            }
            SysOp::RemoveRelation(relations) => {
                targets.extend(relations.iter().map(|s| s.name.clone()))
            }
            SysOp::RenameRelation(pairs) => {
                for (old, new) in pairs {
                    targets.insert(old.name.clone());
                    targets.insert(new.name.clone());
                }
            }
            _ => return Ok(None),
        }
        let temporal = {
            let tx = self.transact()?;
            targets
                .iter()
                .any(|name| tx.get_relation(name, false).is_ok_and(|h| h.has_txtime()))
        };
        if !temporal {
            return Ok(None);
        }
        let poison = Poison {
            deadline,
            ..Default::default()
        };
        let locks = self.obtain_relation_locks(targets.iter());
        let mut guards = Vec::new();
        for lock in &locks {
            guards.push(relation_write(lock, &poison)?);
        }
        let mut tx = self.transact_write()?;
        // Lock catalog keys before the commit section. A transaction holding
        // one of these keys may itself be queued for that section.
        let mut catalog_names = targets.clone();
        for name in &targets {
            if let Ok(handle) = tx.get_relation(name, false) {
                catalog_names.extend(handles(&handle).into_iter().map(|i| i.name.clone()));
            }
        }
        if let SysOp::RenameRelation(pairs) = op {
            for (old, new) in pairs {
                if let Ok(handle) = tx.get_relation(old, false) {
                    let prefix = format!("{}:", old.name);
                    for index in handles(&handle) {
                        if let Some(suffix) = index.name.strip_prefix(&prefix) {
                            catalog_names.insert(format!("{}:{}", new.name, suffix).into());
                        }
                    }
                }
            }
        }
        for name in catalog_names {
            let key = vec![DataValue::from(name.as_str())]
                .encode_as_key(crate::runtime::relation::RelationId::SYSTEM);
            tx.store_tx.get(&key, true)?;
        }
        let guard = section(&self.tt_commit_lock, &poison)?;
        tx.defer_tt_cleanup = true;
        let rows = self.run_sys_op_with_tx(&mut tx, op, false, true)?;
        poison.check()?;
        self.commit_tx_with_test_hook(&mut tx)?;
        let cleanup = std::mem::take(&mut tx.tt_cleanup_ranges);
        drop(tx);
        drop(guard);
        drop(guards);
        self.cleanup_tt_ranges(cleanup);
        Ok(Some(rows))
    }

    fn cleanup_tt_ranges(&'s self, ranges: Vec<(Vec<u8>, Vec<u8>)>) {
        if ranges.is_empty() {
            return;
        }
        let result = (|| {
            let mut tx = self.transact_write()?;
            for (lower, upper) in ranges {
                tx.store_tx.del_range_from_persisted(&lower, &upper)?;
            }
            tx.commit_tx()
        })();
        if let Err(error) = result {
            crate::runtime::diagnostics::emit(
                "txtime.index_cleanup_deferred",
                format!("Retired relation cleanup failed: {error}"),
                "Retry maintenance after resolving the storage error".to_string(),
            );
        }
    }
}
