/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file, You can obtain one at
 * https://mozilla.org/MPL/2.0/.
 */

//! Current-belief index maintenance inside the transaction-time commit section.
use std::collections::BTreeMap;

use miette::{bail, ensure, Diagnostic, Result};
use thiserror::Error;

use crate::data::expr::eval_bytecode_pred;
use crate::data::tuple::{Tuple, TupleT};
use crate::runtime::relation::RelationHandle;
use crate::runtime::transact::SessionTx;

pub(crate) struct PendingTtEvict {
    pub(crate) handle: RelationHandle,
    pub(crate) logical_key: Tuple,
    pub(crate) physical_keys: Vec<Vec<u8>>,
}

pub(crate) struct TouchedRelation {
    pub(crate) handle: RelationHandle,
    pub(crate) old: BTreeMap<Vec<u8>, (Tuple, Option<Tuple>)>,
}

pub(crate) fn has_search_indexes(handle: &RelationHandle) -> bool {
    !handle.fts_indices.is_empty()
        || !handle.hnsw_indices.is_empty()
        || !handle.lsh_indices.is_empty()
}

pub(crate) fn check_evict_limit(handle: &RelationHandle, count: usize) -> Result<()> {
    if has_search_indexes(handle) && count > 1000 {
        #[derive(Debug, Error, Diagnostic)]
        #[error("eviction of indexed relation {0} exceeds 1,000 logical keys")]
        #[diagnostic(code(eval::evict_batch_limit))]
        struct EvictBatchLimit(String);
        bail!(EvictBatchLimit(handle.name.to_string()));
    }
    Ok(())
}

pub(crate) fn reject_tt_index_columns(
    handle: &RelationHandle,
    columns: impl IntoIterator<Item = String>,
) -> Result<()> {
    if !handle.is_tt_only() {
        return Ok(());
    }
    let tt_name = &handle.metadata.keys.last().unwrap().name;
    if columns.into_iter().any(|name| name == tt_name.as_str()) {
        #[derive(Debug, Error, Diagnostic)]
        #[error("transaction-time column {0} cannot be indexed or used by an index expression")]
        #[diagnostic(code(eval::txtime_index_column))]
        struct TxTimeIndexColumn(String);
        bail!(TxTimeIndexColumn(tt_name.to_string()));
    }
    Ok(())
}

impl SessionTx<'_> {
    pub(crate) fn ensure_tt_usable(&self) -> Result<()> {
        ensure!(
            !self.tt_commit_failed,
            "cannot use a transaction after a failed transaction-time commit; abort it"
        );
        Ok(())
    }

    /// Resolve current metadata by name and immutable identity. The caller owns
    /// the commit section; catalog reads here must not wait on metadata locks.
    pub(crate) fn collect_tt_index_old_rows(&self) -> Result<BTreeMap<u64, TouchedRelation>> {
        let mut touched: BTreeMap<u64, TouchedRelation> = BTreeMap::new();
        for handle in self
            .pending_tt_writes
            .iter()
            .map(|w| &w.handle)
            .chain(self.pending_tt_evicts.iter().map(|e| &e.handle))
        {
            if touched.contains_key(&handle.id.0) {
                continue;
            }
            let current = self.get_relation(&handle.name, false)?;
            ensure!(
                current.id == handle.id,
                "transaction-time relation identity changed before commit: {}",
                handle.name
            );
            touched.insert(
                current.id.0,
                TouchedRelation {
                    handle: current,
                    old: BTreeMap::new(),
                },
            );
        }
        for write in &self.pending_tt_writes {
            let entry = touched.get_mut(&write.handle.id.0).unwrap();
            if !entry.handle.is_tt_only() {
                continue;
            }
            for row in &write.rows {
                let key = row[..entry.handle.index_key_len()].to_vec();
                let encoded = key.encode_as_key(entry.handle.id);
                if !entry.old.contains_key(&encoded) {
                    let old = if has_search_indexes(&entry.handle) {
                        entry.handle.get_current(self, &key)?
                    } else {
                        None
                    };
                    entry.old.insert(encoded, (key, old));
                }
            }
        }
        let mut evict_counts = BTreeMap::<u64, usize>::new();
        for evict in &self.pending_tt_evicts {
            let entry = touched.get_mut(&evict.handle.id.0).unwrap();
            let count = evict_counts.entry(entry.handle.id.0).or_default();
            *count += 1;
            check_evict_limit(&entry.handle, *count)?;
            let encoded = evict.logical_key.encode_as_key(entry.handle.id);
            if !entry.old.contains_key(&encoded) {
                let old = if has_search_indexes(&entry.handle) {
                    entry.handle.get_current(self, &evict.logical_key)?
                } else {
                    None
                };
                entry.old.insert(encoded, (evict.logical_key.clone(), old));
            }
        }
        Ok(touched)
    }

    pub(crate) fn apply_tt_evicts(&mut self) -> Result<()> {
        for evict in std::mem::take(&mut self.pending_tt_evicts) {
            for key in evict.physical_keys {
                self.store_tx.del(&key)?;
            }
        }
        Ok(())
    }

    /// The base rows have already advanced. Cache all stale vectors before
    /// removing any graph vertices, then add new vertices against the new base.
    pub(crate) fn maintain_tt_indexes(
        &mut self,
        touched: &BTreeMap<u64, TouchedRelation>,
    ) -> Result<()> {
        for entry in touched.values() {
            if entry.old.is_empty() || !has_search_indexes(&entry.handle) {
                continue;
            }
            let handle = &entry.handle;
            let changes = entry
                .old
                .values()
                .map(|(key, old)| Ok((old.as_ref(), handle.get_current(self, key)?)))
                .collect::<Result<Vec<_>>>()?;
            let processors = self.make_fts_lsh_processors(handle)?;
            let hashes = self.make_lsh_hash_perms(handle);
            let filters = Self::make_hnsw_filters(handle)?;
            let mut stack = Vec::new();
            for (name, (index, manifest)) in &handle.hnsw_indices {
                let filter = filters.get(name);
                let mut removed = Vec::new();
                let mut inserted = Vec::new();
                for (old, new) in &changes {
                    if let (Some(old), Some(new)) = (old, new) {
                        let same_vectors = manifest
                            .vec_fields
                            .iter()
                            .all(|&i| old.get(i) == new.get(i));
                        let same_filter = match filter {
                            None => true,
                            Some(code) => {
                                eval_bytecode_pred(code, old, &mut stack, Default::default())?
                                    == eval_bytecode_pred(
                                        code,
                                        new,
                                        &mut stack,
                                        Default::default(),
                                    )?
                            }
                        };
                        if same_vectors && same_filter {
                            continue;
                        }
                    }
                    if let Some(old) = old {
                        removed.push(*old);
                    }
                    if let Some(new) = new {
                        inserted.push(new);
                    }
                }
                if !removed.is_empty() {
                    self.hnsw_remove_snapshot_rows(manifest, handle, index, &removed)?;
                }
                for row in inserted {
                    self.hnsw_put(manifest, handle, index, filter, &mut stack, row)?;
                }
            }
            for (old, new) in &changes {
                if let Some(new) = new {
                    self.replace_in_fts(
                        handle,
                        &mut stack,
                        &processors,
                        old.map(|r| r.as_slice()),
                        new,
                    )?;
                } else if let Some(old) = old {
                    self.del_in_fts(handle, &mut stack, &processors, old)?;
                }
                if let Some(old) = old {
                    self.del_in_lsh(handle, old)?;
                }
                if let Some(new) = new {
                    self.put_in_lsh(handle, &mut stack, &processors, new, &hashes)?;
                }
            }
        }
        Ok(())
    }
}

// This log is only a catch-up aid. Losing or overflowing a log fails its build;
// it must never fail a write whose storage commit has already succeeded.
pub(crate) struct BuildRegistry {
    next: u64,
    pub(crate) key_limit: usize,
    builds: BTreeMap<u64, BuildLog>,
}

impl Default for BuildRegistry {
    fn default() -> Self {
        Self {
            next: 0,
            key_limit: 50_000,
            builds: BTreeMap::new(),
        }
    }
}

struct BuildLog {
    base: u64,
    watermark: i64,
    names: std::collections::BTreeSet<String>,
    keys: BTreeMap<Vec<u8>, Tuple>,
    bytes: usize,
    invalid: bool,
}

pub(crate) type SharedBuildRegistry = std::sync::Arc<std::sync::Mutex<BuildRegistry>>;

pub(crate) struct BuildRegistration {
    shared: SharedBuildRegistry,
    token: u64,
}

impl BuildRegistration {
    /// Called while the commit section still protects the pinned snapshot.
    pub(crate) fn register(
        shared: SharedBuildRegistry,
        base: u64,
        watermark: i64,
        names: std::collections::BTreeSet<String>,
    ) -> Self {
        let token = {
            let mut registry = shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            registry.next += 1;
            let token = registry.next;
            registry.builds.insert(
                token,
                BuildLog {
                    base,
                    watermark,
                    names,
                    keys: Default::default(),
                    bytes: 0,
                    invalid: false,
                },
            );
            token
        };
        Self { shared, token }
    }

    /// Called under the commit section immediately before publication.
    pub(crate) fn keys(&self) -> Result<Vec<Tuple>> {
        let registry = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let log = registry.builds.get(&self.token).expect("live registration");
        ensure!(
            !log.invalid,
            "transaction-time index build at {} was invalidated or exceeded its catch-up budget",
            log.watermark
        );
        Ok(log.keys.values().cloned().collect())
    }
}

impl Drop for BuildRegistration {
    fn drop(&mut self) {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .builds
            .remove(&self.token);
    }
}

impl BuildRegistry {
    pub(crate) fn invalidate(&mut self, base: u64, index: Option<&str>) -> bool {
        let mut matched = false;
        for log in self.builds.values_mut() {
            if log.base == base && index.is_none_or(|name| log.names.contains(name)) {
                log.invalid = true;
                log.keys.clear();
                matched = true;
            }
        }
        matched
    }

    pub(crate) fn notify(&mut self, touched: &BTreeMap<u64, TouchedRelation>) {
        for log in self.builds.values_mut() {
            if log.invalid {
                continue;
            }
            let Some(relation) = touched.get(&log.base) else {
                continue;
            };
            for (encoded, (key, _)) in &relation.old {
                if log.keys.contains_key(encoded) {
                    continue;
                }
                let bytes = encoded.len();
                if log.keys.len() >= self.key_limit
                    || bytes > (32 * 1024 * 1024usize).saturating_sub(log.bytes)
                {
                    log.invalid = true;
                    log.keys.clear();
                    break;
                }
                log.bytes += bytes;
                log.keys.insert(encoded.clone(), key.clone());
            }
        }
    }
}
