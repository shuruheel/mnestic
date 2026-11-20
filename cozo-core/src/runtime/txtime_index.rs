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

use crate::data::expr::{eval_bytecode, eval_bytecode_pred};
use crate::data::tuple::{Tuple, TupleT};
use crate::data::value::DataValue;
use crate::runtime::minhash_lsh::HashValues;
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

pub(crate) const AS_OF_SCAN_MAX_ROWS_DEFAULT: usize = 200_000;

#[cfg(test)]
thread_local! {
    static AS_OF_SCAN_MAX_ROWS_TEST_OVERRIDE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) fn as_of_scan_max_rows() -> usize {
    #[cfg(test)]
    {
        let ov = AS_OF_SCAN_MAX_ROWS_TEST_OVERRIDE.with(std::cell::Cell::get);
        if ov > 0 {
            return ov;
        }
    }
    AS_OF_SCAN_MAX_ROWS_DEFAULT
}

pub(crate) fn as_of_scan_limit_error(rel_name: &str, limit: usize) -> miette::Report {
    #[derive(Debug, Error, Diagnostic)]
    #[error("historical as-of scan on transaction-time relation '{0}' exceeded the {1}-row bound")]
    #[diagnostic(
        code(query::as_of_scan_limit),
        help("bind the primary key prefix or reduce the scanned range")
    )]
    struct AsOfScanLimit(String, usize);
    AsOfScanLimit(rel_name.to_string(), limit).into()
}

pub(crate) fn has_search_indexes(handle: &RelationHandle) -> bool {
    !handle.indices.is_empty()
        || !handle.fts_indices.is_empty()
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
            let need_old =
                has_search_indexes(&entry.handle) || self.has_active_shadow(entry.handle.id.0);
            for row in &write.rows {
                let key = row[..entry.handle.index_key_len()].to_vec();
                let encoded = key.encode_as_key(entry.handle.id);
                if !entry.old.contains_key(&encoded) {
                    let old = if need_old {
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
            let need_old =
                has_search_indexes(&entry.handle) || self.has_active_shadow(entry.handle.id.0);
            let encoded = evict.logical_key.encode_as_key(entry.handle.id);
            if !entry.old.contains_key(&encoded) {
                let old = if need_old {
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
            for (idx_handle, extraction_indices) in handle.indices.values() {
                for (old, new) in &changes {
                    let old_extracted = old.map(|row| {
                        extraction_indices
                            .iter()
                            .map(|&i| row[i].clone())
                            .collect::<Vec<_>>()
                    });
                    let new_extracted = new.as_ref().map(|row| {
                        extraction_indices
                            .iter()
                            .map(|&i| row[i].clone())
                            .collect::<Vec<_>>()
                    });
                    if old_extracted == new_extracted {
                        continue;
                    }
                    if let Some(old_vals) = old_extracted {
                        let key = idx_handle.encode_key_for_store(&old_vals, Default::default())?;
                        self.store_tx.del(&key)?;
                    }
                    if let Some(new_vals) = new_extracted {
                        let key = idx_handle.encode_key_for_store(&new_vals, Default::default())?;
                        self.store_tx.put(&key, &[])?;
                    }
                }
            }
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
                if !inserted.is_empty() {
                    let mut insert_cache = crate::runtime::hnsw::VectorCache::new(manifest.distance);
                    for row in inserted {
                        self.hnsw_put_with_cache(
                            manifest,
                            handle,
                            index,
                            filter,
                            &mut stack,
                            row,
                            &mut insert_cache,
                        )?;
                    }
                }
            }
            let fts_lsh_pairs: Vec<(Option<&Tuple>, Option<&Tuple>)> = changes
                .iter()
                .map(|(old, new)| (*old, new.as_ref()))
                .collect();
            self.maintain_tt_fts_lsh_pairs(
                handle,
                &fts_lsh_pairs,
                &processors,
                &hashes,
                &mut stack,
            )?;
        }
        Ok(())
    }

    /// Maintain B-tree secondary current-belief indexes for explicit `(old, new)`
    /// row pairs without reading the base table (used by commit-section
    /// maintenance, unlocked catch-up rounds, and shadow maintenance).
    pub(crate) fn maintain_tt_btree_pairs(
        &mut self,
        handle: &RelationHandle,
        changes: &[(Option<&Tuple>, Option<&Tuple>)],
    ) -> Result<()> {
        for (idx_handle, extraction_indices) in handle.indices.values() {
            for (old, new) in changes {
                let old_extracted = old.map(|row| {
                    extraction_indices
                        .iter()
                        .map(|&i| row[i].clone())
                        .collect::<Vec<_>>()
                });
                let new_extracted = new.map(|row| {
                    extraction_indices
                        .iter()
                        .map(|&i| row[i].clone())
                        .collect::<Vec<_>>()
                });
                if old_extracted == new_extracted {
                    continue;
                }
                if let Some(old_vals) = old_extracted {
                    let key = idx_handle.encode_key_for_store(&old_vals, Default::default())?;
                    self.store_tx.del(&key)?;
                }
                if let Some(new_vals) = new_extracted {
                    let key = idx_handle.encode_key_for_store(&new_vals, Default::default())?;
                    self.store_tx.put(&key, &[])?;
                }
            }
        }
        Ok(())
    }

    /// Maintain FTS and LSH current-belief indexes for explicit `(old, new)` row
    /// pairs without reading the base table (used by both commit-section
    /// maintenance and unlocked catch-up rounds).
    pub(crate) fn maintain_tt_fts_lsh_pairs(
        &mut self,
        handle: &RelationHandle,
        changes: &[(Option<&Tuple>, Option<&Tuple>)],
        processors: &std::collections::BTreeMap<
            smartstring::SmartString<smartstring::LazyCompact>,
            (
                std::sync::Arc<crate::fts::tokenizer::TextAnalyzer>,
                Vec<crate::data::expr::Bytecode>,
            ),
        >,
        hashes: &std::collections::BTreeMap<
            smartstring::SmartString<smartstring::LazyCompact>,
            crate::runtime::minhash_lsh::HashPermutations,
        >,
        stack: &mut Vec<DataValue>,
    ) -> Result<()> {
        for (old, new) in changes {
            if let Some(new) = new {
                self.replace_in_fts(handle, stack, processors, old.map(|r| r.as_slice()), new)?;
            } else if let Some(old) = old {
                self.del_in_fts(handle, stack, processors, old)?;
            }
            for (name, (index, inverse, manifest)) in &handle.lsh_indices {
                let (tokenizer, extractor) = processors.get(name).unwrap();
                if let (Some(old), Some(new)) = (old, new) {
                    let old_value = eval_bytecode(extractor, old, stack)?;
                    let new_value = eval_bytecode(extractor, new, stack)?;
                    // List element equality need not match hashing (JSON
                    // signed zero is one example). Compare actual derived
                    // signatures for lists; strings/null have exact identity.
                    let unchanged = match (&old_value, &new_value) {
                        (DataValue::Null, DataValue::Null) => true,
                        (DataValue::Str(a), DataValue::Str(b)) => a == b,
                        (DataValue::List(a), DataValue::List(b)) => {
                            let perms = hashes.get(name).unwrap();
                            HashValues::new(a.iter(), perms).0 == HashValues::new(b.iter(), perms).0
                        }
                        _ => false,
                    };
                    if unchanged {
                        continue;
                    }
                }
                if let Some(old) = old {
                    self.del_lsh_index_item(old, None, index, inverse)?;
                }
                if let Some(new) = new {
                    self.put_lsh_index_item(
                        new,
                        extractor,
                        stack,
                        tokenizer,
                        handle,
                        index,
                        inverse,
                        manifest,
                        hashes.get(name).unwrap(),
                    )?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn has_active_shadow(&self, base_id: u64) -> bool {
        let registry = self
            .tt_index_builds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry
            .builds
            .values()
            .any(|log| !log.invalid && log.base == base_id && log.shadow.is_some())
    }

    /// Maintain any active WRITE_ONLY (shadow) detached index generations on
    /// relations touched by this commit (called inside `tt_commit_lock` after
    /// base rows have advanced). Returns the pending keys claimed by this commit
    /// so `BuildCommitNotification::committed()` removes them from `pending_old`
    /// only after `store_tx.commit()` succeeds.
    pub(crate) fn maintain_shadow_tt_indexes(
        &mut self,
        touched: &BTreeMap<u64, TouchedRelation>,
    ) -> Result<Vec<ShadowCommitUpdate>> {
        // Snapshot the active shadow targets + per-touched-key prior rows under
        // the leaf `BuildRegistry` mutex without holding it across storage I/O.
        let targets: Vec<(u64, RelationHandle, Vec<(Vec<u8>, Tuple, Option<Tuple>, bool)>)> = {
            let registry = self
                .tt_index_builds
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut out = Vec::new();
            for (&token, log) in &registry.builds {
                if log.invalid {
                    continue;
                }
                let Some(shadow) = log.shadow.as_ref() else {
                    continue;
                };
                let Some(entry) = touched.get(&log.base) else {
                    continue;
                };
                if entry.old.is_empty() || !has_search_indexes(&shadow.handle) {
                    continue;
                }
                let mut rows = Vec::with_capacity(entry.old.len());
                for (encoded, (key, writer_old)) in &entry.old {
                    if let Some((_, pending_prior)) = shadow.pending_old.get(encoded) {
                        rows.push((encoded.clone(), key.clone(), pending_prior.clone(), true));
                    } else {
                        rows.push((encoded.clone(), key.clone(), writer_old.clone(), false));
                    }
                }
                out.push((token, shadow.handle.clone(), rows));
            }
            out
        };

        let mut updates = Vec::with_capacity(targets.len());
        for (token, shadow_handle, rows) in targets {
            let mut pairs: Vec<(Option<Tuple>, Option<Tuple>)> = Vec::with_capacity(rows.len());
            let mut claimed_keys = Vec::new();
            let mut count_delta: isize = 0;
            for (encoded, key, shadow_old, from_pending) in rows {
                let now = shadow_handle.get_current(self, &key)?;
                count_delta += isize::from(now.is_some()) - isize::from(shadow_old.is_some());
                if from_pending {
                    claimed_keys.push(encoded);
                }
                pairs.push((shadow_old, now));
            }
            // Error isolation: an unpublished shadow build must NEVER fail an
            // unrelated user write commit. On any shadow error, restore tx-local
            // HNSW/FTS bookkeeping, invalidate the build so it cleans up its
            // detached key range, and let the user's commit proceed cleanly.
            let saved_hnsw_incomplete = self.hnsw_mutations_incomplete;
            if self
                .maintain_shadow_pairs(&shadow_handle, token, &pairs)
                .is_err()
            {
                self.hnsw_mutations_incomplete = saved_hnsw_incomplete;
                for (idx, _) in shadow_handle.fts_indices.values() {
                    self.fts_pending.remove(&idx.id);
                    self.fts_stats.removed(idx.id);
                }
                self.tt_index_builds
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .invalidate_token(token);
                continue;
            }
            updates.push(ShadowCommitUpdate {
                token,
                claimed_keys,
                count_delta,
            });
        }
        Ok(updates)
    }

    /// Apply explicit `(old, new)` row pairs to a detached shadow generation
    /// while consulting `ShadowState::pending_old` for any untouched backfill-era
    /// neighbor vectors in HNSW.
    pub(crate) fn maintain_shadow_pairs(
        &mut self,
        handle: &RelationHandle,
        token: u64,
        pairs: &[(Option<Tuple>, Option<Tuple>)],
    ) -> Result<()> {
        if pairs.is_empty() || !has_search_indexes(handle) {
            return Ok(());
        }
        let filters = Self::make_hnsw_filters(handle)?;
        let mut stack = Vec::new();
        for (name, (index, manifest)) in &handle.hnsw_indices {
            let filter = filters.get(name);
            let mut removed: Vec<&Tuple> = Vec::new();
            let mut inserted: Vec<&Tuple> = Vec::new();
            for (old, new) in pairs {
                let old_passes = match (old.as_ref(), filter) {
                    (None, _) => false,
                    (Some(_), None) => true,
                    (Some(r), Some(code)) => {
                        eval_bytecode_pred(code, r, &mut stack, Default::default())?
                    }
                };
                let new_passes = match (new.as_ref(), filter) {
                    (None, _) => false,
                    (Some(_), None) => true,
                    (Some(r), Some(code)) => {
                        eval_bytecode_pred(code, r, &mut stack, Default::default())?
                    }
                };
                if !old_passes && !new_passes {
                    continue;
                }
                if old_passes && new_passes {
                    if let (Some(o), Some(n)) = (old.as_ref(), new.as_ref()) {
                        let same_vectors = manifest
                            .vec_fields
                            .iter()
                            .all(|&i| o.get(i) == n.get(i));
                        if same_vectors {
                            continue;
                        }
                    }
                }
                if old_passes {
                    removed.push(old.as_ref().unwrap());
                }
                if new_passes {
                    inserted.push(new.as_ref().unwrap());
                }
            }
            if !removed.is_empty() {
                let mut remove_cache = crate::runtime::hnsw::VectorCache::with_shadow(
                    manifest.distance,
                    self.tt_index_builds.clone(),
                    token,
                );
                for row in &removed {
                    remove_cache.seed_row(manifest, handle, row);
                }
                for row in removed {
                    self.hnsw_remove_with_cache(manifest, handle, index, row, &mut remove_cache)?;
                }
            }
            if !inserted.is_empty() {
                let mut insert_cache = crate::runtime::hnsw::VectorCache::with_shadow(
                    manifest.distance,
                    self.tt_index_builds.clone(),
                    token,
                );
                for row in &inserted {
                    insert_cache.seed_row(manifest, handle, row);
                }
                for row in inserted {
                    self.hnsw_put_with_cache(
                        manifest,
                        handle,
                        index,
                        filter,
                        &mut stack,
                        row,
                        &mut insert_cache,
                    )?;
                }
            }
        }
        if !handle.indices.is_empty()
            || !handle.fts_indices.is_empty()
            || !handle.lsh_indices.is_empty()
        {
            let non_hnsw_pairs: Vec<(Option<&Tuple>, Option<&Tuple>)> = pairs
                .iter()
                .map(|(old, new)| (old.as_ref(), new.as_ref()))
                .collect();
            if !handle.indices.is_empty() {
                self.maintain_tt_btree_pairs(handle, &non_hnsw_pairs)?;
            }
            if !handle.fts_indices.is_empty() || !handle.lsh_indices.is_empty() {
                let processors = self.make_fts_lsh_processors(handle)?;
                let hashes = self.make_lsh_hash_perms(handle);
                self.maintain_tt_fts_lsh_pairs(
                    handle,
                    &non_hnsw_pairs,
                    &processors,
                    &hashes,
                    &mut stack,
                )?;
            }
        }
        Ok(())
    }
}

/// Active WRITE_ONLY (shadow dual-write) state for a detached TxTime index
/// generation (Google F1 / PostgreSQL `CREATE INDEX CONCURRENTLY` pattern).
///
/// Invariant at every commit boundary while active:
/// - Keys in `pending_old` are still indexed in `handle` at their pre-backfill
///   `S_0` row (`pending_old[k].1`).
/// - Keys not in `pending_old` are indexed in `handle` at the live base row.
pub(crate) struct ShadowState {
    pub(crate) handle: RelationHandle,
    pub(crate) pending_old: BTreeMap<Vec<u8>, (Tuple, Option<Tuple>)>,
    pub(crate) count_delta: isize,
}

pub(crate) struct ShadowCommitUpdate {
    pub(crate) token: u64,
    pub(crate) claimed_keys: Vec<Vec<u8>>,
    pub(crate) count_delta: isize,
}

/// Lookup one key in a shadow build's `pending_old` map without holding the
/// `BuildRegistry` mutex across storage I/O.
pub(crate) fn lookup_shadow_pending(
    shared: &SharedBuildRegistry,
    token: u64,
    encoded: &[u8],
) -> Result<Option<Option<Tuple>>> {
    let registry = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(log) = registry.builds.get(&token) else {
        bail!("shadow index build was cancelled");
    };
    ensure!(!log.invalid, "shadow index build was invalidated");
    let Some(shadow) = log.shadow.as_ref() else {
        bail!("shadow index build is no longer active");
    };
    Ok(shadow.pending_old.get(encoded).map(|(_, old)| old.clone()))
}

/// Batch lookup in a shadow build's `pending_old` map under a single brief lock.
pub(crate) fn lookup_shadow_pending_batch(
    shared: &SharedBuildRegistry,
    token: u64,
    encoded_keys: &[Vec<u8>],
) -> Result<Vec<Option<Option<Tuple>>>> {
    let registry = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(log) = registry.builds.get(&token) else {
        bail!("shadow index build was cancelled");
    };
    ensure!(!log.invalid, "shadow index build was invalidated");
    let Some(shadow) = log.shadow.as_ref() else {
        bail!("shadow index build is no longer active");
    };
    Ok(encoded_keys
        .iter()
        .map(|k| shadow.pending_old.get(k).map(|(_, old)| old.clone()))
        .collect())
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
    shadow: Option<ShadowState>,
}

pub(crate) type SharedBuildRegistry = std::sync::Arc<std::sync::Mutex<BuildRegistry>>;

/// A commit error or unwind may be ambiguous. Invalidate catch-up logs unless
/// notification finishes, so a build cannot attach a generation missing a write.
pub(crate) struct BuildCommitNotification<'a> {
    shared: SharedBuildRegistry,
    touched: &'a BTreeMap<u64, TouchedRelation>,
    shadow_updates: Vec<ShadowCommitUpdate>,
    notified: bool,
}

impl<'a> BuildCommitNotification<'a> {
    pub(crate) fn new(
        shared: SharedBuildRegistry,
        touched: &'a BTreeMap<u64, TouchedRelation>,
        shadow_updates: Vec<ShadowCommitUpdate>,
    ) -> Self {
        Self {
            shared,
            touched,
            shadow_updates,
            notified: false,
        }
    }
    pub(crate) fn committed(mut self) {
        let mut registry = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.apply_shadow_updates(&self.shadow_updates);
        registry.notify(self.touched);
        self.notified = true;
    }
}
impl Drop for BuildCommitNotification<'_> {
    fn drop(&mut self) {
        if !self.notified {
            let mut builds = self
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for id in self.touched.keys() {
                builds.invalidate(*id, None);
            }
        }
    }
}

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
                    shadow: None,
                },
            );
            token
        };
        Self { shared, token }
    }

    pub(crate) fn token(&self) -> u64 {
        self.token
    }

    /// Transition this build from Backfill log mode into WRITE_ONLY (shadow
    /// dual-write) mode under `tt_commit_lock`.
    pub(crate) fn activate_shadow(
        &self,
        handle: RelationHandle,
        pending_old: BTreeMap<Vec<u8>, (Tuple, Option<Tuple>)>,
    ) -> Result<()> {
        let mut registry = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let log = registry.builds.get_mut(&self.token).expect("live registration");
        ensure!(
            !log.invalid,
            "transaction-time index build at {} was invalidated or exceeded its catch-up budget",
            log.watermark
        );
        log.keys.clear();
        log.bytes = 0;
        log.shadow = Some(ShadowState {
            handle,
            pending_old,
            count_delta: 0,
        });
        Ok(())
    }

    /// Peek one pending backfill-era key for the builder to drain under
    /// `tt_commit_lock`.
    pub(crate) fn peek_shadow_pending(&self) -> Result<Option<(Vec<u8>, Tuple, Option<Tuple>)>> {
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
        let shadow = log
            .shadow
            .as_ref()
            .expect("shadow mode active during drain");
        Ok(shadow
            .pending_old
            .iter()
            .next()
            .map(|(encoded, (key, old))| (encoded.clone(), key.clone(), old.clone())))
    }

    /// Mark one builder-drained shadow key completed after its commit succeeds.
    pub(crate) fn complete_shadow_key(&self, encoded: &[u8], count_delta: isize) -> Result<()> {
        let mut registry = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let log = registry.builds.get_mut(&self.token).expect("live registration");
        ensure!(
            !log.invalid,
            "transaction-time index build at {} was invalidated or exceeded its catch-up budget",
            log.watermark
        );
        if let Some(shadow) = log.shadow.as_mut() {
            shadow.pending_old.remove(encoded);
            shadow.count_delta += count_delta;
        }
        Ok(())
    }

    /// Invalidate this registration directly (e.g., on an ambiguous drain
    /// commit failure while still holding `tt_commit_lock`).
    pub(crate) fn invalidate_self(&self) {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .invalidate_token(self.token);
    }

    /// Atomically deactivate shadow mode while `tt_commit_lock` is still held
    /// right before catalog publication, verifying the build is valid and fully
    /// drained, and returning its accumulated row-count delta. Clearing
    /// `log.shadow` inside `_section` ensures no post-publication writer can
    /// observe both the published index and an active shadow targeting the same
    /// index `RelationId`s.
    pub(crate) fn finish_shadow(&self) -> Result<isize> {
        let mut registry = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let log = registry.builds.get_mut(&self.token).expect("live registration");
        ensure!(
            !log.invalid,
            "transaction-time index build at {} was invalidated or exceeded its catch-up budget",
            log.watermark
        );
        let shadow = log
            .shadow
            .take()
            .expect("shadow mode active at publication");
        ensure!(
            shadow.pending_old.is_empty(),
            "shadow index build still has {} pending key(s) at publication",
            shadow.pending_old.len()
        );
        Ok(shadow.count_delta)
    }

    /// Drain currently accumulated catch-up keys while keeping the registration
    /// live for subsequent commits (used by unlocked catch-up rounds before
    /// taking exclusive publication locks).
    pub(crate) fn take_keys(&self) -> Result<Vec<Tuple>> {
        let mut registry = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let log = registry.builds.get_mut(&self.token).expect("live registration");
        ensure!(
            !log.invalid,
            "transaction-time index build at {} was invalidated or exceeded its catch-up budget",
            log.watermark
        );
        log.bytes = 0;
        Ok(std::mem::take(&mut log.keys).into_values().collect())
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
    pub(crate) fn invalidate_token(&mut self, token: u64) {
        if let Some(log) = self.builds.get_mut(&token) {
            log.invalid = true;
            log.keys.clear();
            log.shadow = None;
        }
    }

    pub(crate) fn invalidate(&mut self, base: u64, index: Option<&str>) -> bool {
        let mut matched = false;
        for log in self.builds.values_mut() {
            if log.base == base && index.is_none_or(|name| log.names.contains(name)) {
                log.invalid = true;
                log.keys.clear();
                log.shadow = None;
                matched = true;
            }
        }
        matched
    }

    fn apply_shadow_updates(&mut self, updates: &[ShadowCommitUpdate]) {
        for update in updates {
            let Some(log) = self.builds.get_mut(&update.token) else {
                continue;
            };
            if log.invalid {
                continue;
            }
            if let Some(shadow) = log.shadow.as_mut() {
                for key in &update.claimed_keys {
                    shadow.pending_old.remove(key);
                }
                shadow.count_delta += update.count_delta;
            }
        }
    }

    pub(crate) fn notify(&mut self, touched: &BTreeMap<u64, TouchedRelation>) {
        for log in self.builds.values_mut() {
            if log.invalid || log.shadow.is_some() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DbInstance, ScriptMutability, ScriptRunOptions};

    #[test]
    fn waiting_commit_times_out_without_allocating_tt() {
        let db = DbInstance::new("mem", "", "{}").unwrap();
        db.run_default(":create doc {id:String,tt:TxTime => body:String}")
            .unwrap();
        db.run_default("::fts create doc:text {extractor:body,tokenizer:Simple}")
            .unwrap();
        let DbInstance::Mem(inner) = &db else {
            unreachable!()
        };
        let before = inner.tt_clock().peek();
        let guard = inner.tt_commit_lock.lock().unwrap();
        let error = db
            .run_script_with_options(
                "?[id,body] <- [['a','alpha']] :put doc{id=>body}",
                BTreeMap::new(),
                ScriptMutability::Mutable,
                ScriptRunOptions::new().with_timeout(0.05),
            )
            .unwrap_err();
        assert_eq!(error.code().unwrap().to_string(), "eval::timeout");
        assert_eq!(inner.tt_clock().peek(), before);
        drop(guard);
        assert!(db.run_default("?[id] := *doc{id}").unwrap().rows.is_empty());
        db.run_default("?[id,body] <- [['a','alpha']] :put doc{id=>body}")
            .unwrap();
        assert_eq!(
            db.run_default("?[id] := ~doc:text{id | query:'alpha',k:10}")
                .unwrap()
                .rows
                .len(),
            1
        );
    }

    #[test]
    fn as_of_scan_limit_fails_closed_for_base_and_vector_scans() {
        struct ResetGuard;
        impl Drop for ResetGuard {
            fn drop(&mut self) {
                AS_OF_SCAN_MAX_ROWS_TEST_OVERRIDE.with(|c| c.set(0));
            }
        }
        let _reset = ResetGuard;
        AS_OF_SCAN_MAX_ROWS_TEST_OVERRIDE.with(|c| c.set(3));

        let db = DbInstance::new("mem", "", "{}").unwrap();
        db.run_default(":create item {id:String, tt:TxTime => tag:String, emb:<F32; 2>}")
            .unwrap();
        db.run_default(
            "::hnsw create item:vec {dim:2, m:8, dtype:F32, fields:[emb], distance:L2, ef_construction:16}",
        )
        .unwrap();
        db.run_default("::index create item:by_tag {tag}").unwrap();
        db.run_default(
            "?[id, tag, emb] <- [['a','t',vec([1,0])],['b','t',vec([2,0])],['c','t',vec([3,0])],['d','t',vec([4,0])]] :put item {id => tag, emb}",
        )
        .unwrap();
        let tt = db
            .run_default("?[max(t)] := *item{tt}, t = to_int(tt)")
            .unwrap()
            .rows[0][0]
            .get_int()
            .unwrap();

        // Current read through B-tree index succeeds (not a historical base scan).
        assert_eq!(
            db.run_default("?[id] := tag = 't', *item{id, tag}")
                .unwrap()
                .rows
                .len(),
            4
        );

        // Historical secondary-key base scan exceeds the 3-row cap -> query::as_of_scan_limit.
        let base_err = db
            .run_default(&format!("?[id] := tag = 't', *item{{id, tag}} :as_of {tt}"))
            .unwrap_err();
        assert!(
            format!("{base_err:?}").contains("query::as_of_scan_limit"),
            "expected as_of_scan_limit on base scan, got: {base_err:?}"
        );

        // Historical vector scan exceeds the 3-row cap -> query::as_of_scan_limit (never falls back to ANN).
        let vec_err = db
            .run_default(&format!(
                "?[id] := ~item:vec{{id | query: vec([1,0]), k: 2, ef: 10}} :as_of {tt}"
            ))
            .unwrap_err();
        assert!(
            format!("{vec_err:?}").contains("query::as_of_scan_limit"),
            "expected as_of_scan_limit on vector scan, got: {vec_err:?}"
        );

        // Historical fixed-rule base scan exceeds the 3-row cap -> query::as_of_scan_limit.
        let fixed_err = db
            .run_default(&format!(
                "?[rank, id] <~ ReorderSort(*item[id, tt, tag, emb], out: [id]) :as_of {tt}"
            ))
            .unwrap_err();
        assert!(
            format!("{fixed_err:?}").contains("query::as_of_scan_limit"),
            "expected as_of_scan_limit on fixed-rule scan, got: {fixed_err:?}"
        );

        // Historical prefix_join on a composite logical key also enforces query::as_of_scan_limit.
        db.run_default(":create comp {tenant:String, id:String, tt:TxTime => tag:String}")
            .unwrap();
        db.run_default(
            "?[tenant, id, tag] <- [['acme','1','a'],['acme','2','b'],['acme','3','c'],['acme','4','d']] :put comp {tenant, id => tag}",
        )
        .unwrap();
        let comp_tt = db
            .run_default("?[max(t)] := *comp{tt}, t = to_int(tt)")
            .unwrap()
            .rows[0][0]
            .get_int()
            .unwrap();
        let prefix_err = db
            .run_default(&format!(
                "?[id] := tenant = 'acme', *comp{{tenant, id, tag}}, tag == 'd' :as_of {comp_tt}"
            ))
            .unwrap_err();
        assert!(
            format!("{prefix_err:?}").contains("query::as_of_scan_limit"),
            "expected as_of_scan_limit on historical prefix_join, got: {prefix_err:?}"
        );
    }
}
