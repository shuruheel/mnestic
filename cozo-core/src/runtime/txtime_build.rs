/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file, You can obtain one at
 * https://mozilla.org/MPL/2.0/.
 */

//! Detached current-belief search-index generations and atomic publication.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, TryLockError};
#[cfg(not(feature = "test-hooks"))]
use std::sync::MutexGuard;
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

#[cfg(feature = "test-hooks")]
type SectionGuard<'a> = crate::measurement::SectionGuard<'a>;
#[cfg(not(feature = "test-hooks"))]
type SectionGuard<'a> = MutexGuard<'a, ()>;

pub(crate) struct CommitSection<'a> {
    // Field drop order is declaration order: drop the inner mutex guard before
    // releasing the outer FIFO turn so the next queued waiter acquires immediately.
    _guard: SectionGuard<'a>,
    _turn: crate::runtime::relation_lock::RelationGuard<'a>,
}

pub(crate) fn section<'a>(
    queue: &'a crate::runtime::relation_lock::RelationLock,
    lock: &'a Mutex<()>,
    poison: &Poison,
) -> Result<CommitSection<'a>> {
    #[cfg(feature = "test-hooks")]
    let wait = crate::measurement::Span::new("tt_ddl_section");
    let turn = queue.write_with_poison(poison)?;
    loop {
        poison.check()?;
        match lock.try_lock() {
            Ok(guard) => {
                #[cfg(feature = "test-hooks")]
                let guard = wait.acquired(guard);
                poison.check()?;
                return Ok(CommitSection {
                    _guard: guard,
                    _turn: turn,
                });
            }
            Err(TryLockError::Poisoned(error)) => {
                let guard = error.into_inner();
                #[cfg(feature = "test-hooks")]
                let guard = wait.acquired(guard);
                poison.check()?;
                return Ok(CommitSection {
                    _guard: guard,
                    _turn: turn,
                });
            }
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(2)),
        }
    }
}

fn relation_write<'a>(
    lock: &'a crate::runtime::relation_lock::RelationLock,
    poison: &Poison,
) -> Result<crate::runtime::relation_lock::RelationGuard<'a>> {
    #[cfg(feature = "test-hooks")]
    let admission = crate::measurement::Span::new("relation_write_admission");
    let guard = lock.write_with_poison(poison)?;
    #[cfg(feature = "test-hooks")]
    admission.finish();
    Ok(guard)
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
        .indices
        .keys()
        .chain(handle.fts_indices.keys())
        .chain(handle.hnsw_indices.keys())
        .chain(handle.lsh_indices.keys())
        .map(ToString::to_string)
        .collect()
}

fn handles(handle: &RelationHandle) -> Vec<&RelationHandle> {
    handle
        .indices
        .values()
        .map(|(i, _)| i)
        .chain(handle.fts_indices.values().map(|(i, _)| i))
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
                    SysOp::CreateIndex(rel, idx, cols) => self.create_index(rel, idx, cols)?,
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
        definition.indices.retain(|name, (index, _)| {
            original
                .indices
                .get(name)
                .is_none_or(|(old, _)| old.id != index.id)
        });
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
        if !build.hnsw_indices.is_empty() {
            let filters = Self::make_hnsw_filters(build)?;
            for (name, (index, manifest)) in &build.hnsw_indices {
                poison.check()?;
                self.hnsw_build_index(manifest, build, index, filters.get(name), rows)?;
            }
        }
        self.populate_tt_non_hnsw_indexes(build, rows, poison)
    }

    fn populate_tt_non_hnsw_indexes(
        &mut self,
        build: &RelationHandle,
        rows: &[Tuple],
        poison: &Poison,
    ) -> Result<()> {
        let processors = self.make_fts_lsh_processors(build)?;
        let perms = self.make_lsh_hash_perms(build);
        let mut stack = Vec::new();
        for row in rows {
            poison.check()?;
            for (idx_handle, extraction_indices) in build.indices.values() {
                if extraction_indices.iter().any(|&i| i >= row.len()) {
                    continue;
                }
                let extracted: Vec<_> = extraction_indices
                    .iter()
                    .map(|&i| row[i].clone())
                    .collect();
                let key = idx_handle.encode_key_for_store(&extracted, Default::default())?;
                self.store_tx.put(&key, &[])?;
            }
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
        current.indices.extend(build.indices.clone());
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
            SysOp::CreateIndex(base, _, _) => base.name.as_str(),
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
        if matches!(op, SysOp::Reindex(_))
            && original.fts_indices.is_empty()
            && original.hnsw_indices.is_empty()
            && original.lsh_indices.is_empty()
        {
            return Ok(Some(result_rows(op, &original, 0)));
        }
        let poison = Poison {
            deadline,
            ..Default::default()
        };
        let locks = self.obtain_relation_locks(std::iter::once(&original.name));
        if !self.db.supports_sst_ingest() {
            // Admission before the commit section: SQLite/mem may already have
            // a writer waiting for that section. Never invert those locks.
            let admission = self.admit_relation_maintenance(&poison)?;
            let _relation = relation_write(&locks[0], &poison)?;
            drop(admission);
            let mut tx = self.transact_write()?;
            let current = tx.get_relation(name, true)?;
            let _section = section(&self.tt_commit_queue, &self.tt_commit_lock, &poison)?;
            #[cfg(feature = "test-hooks")]
            let registration_span = crate::measurement::Span::new("build_registration");
            let build = tx.prepare_tt_indexes(&current, op)?;
            #[cfg(feature = "test-hooks")]
            registration_span.finish();
            #[cfg(feature = "test-hooks")]
            let backfill_span = crate::measurement::Span::new("build_backfill");
            let rows = current.scan_current_uncached(&tx).collect::<Result<Vec<_>>>()?;
            tx.populate_tt_indexes(&build, &rows, &poison)?;
            #[cfg(feature = "test-hooks")]
            backfill_span.finish();
            #[cfg(feature = "test-hooks")]
            let publication_span = crate::measurement::Span::new("build_publication");
            tx.publish_tt_indexes(current.clone(), &build)?;
            poison.check()?;
            self.commit_tx_with_test_hook(&mut tx)?;
            self.tt_recent_mutations.invalidate_all();
            #[cfg(feature = "test-hooks")]
            publication_span.finish();
            let result = result_rows(op, &build, rows.len());
            drop(tx);
            drop(_section);
            drop(_relation);
            self.cleanup_tt_generations(&current, &names(&build));
            return Ok(Some(result));
        }

        #[cfg(feature = "test-hooks")]
        let reservation_span = crate::measurement::Span::new("build_reservation");
        // Durable reservation only; no public index name exists yet.
        let (original, build) = {
            let admission = self.admit_relation_maintenance(&poison)?;
            let _relation = relation_write(&locks[0], &poison)?;
            drop(admission);
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
        #[cfg(feature = "test-hooks")]
        reservation_span.finish();
        let mut publication_attempted = false;
        // Keep `registration` outside `outcome` so on error/abort it is only
        // dropped under `tt_commit_lock`, preventing an uncoordinated drop race
        // with an in-flight writer inside `commit_tx_with_tt`.
        let mut registration_slot: Option<BuildRegistration> = None;
        let outcome = (|| {
            // Pin the actual read snapshot and registration in one commit gap.
            #[cfg(feature = "test-hooks")]
            let registration_span = crate::measurement::Span::new("build_registration");
            let snapshot = {
                let _section = section(&self.tt_commit_queue, &self.tt_commit_lock, &poison)?;
                let snapshot = self.transact()?;
                let current = snapshot.get_relation(name, false)?;
                validate(&original, &current, &build)?;
                registration_slot = Some(BuildRegistration::register(
                    self.tt_index_builds.clone(),
                    original.id.0,
                    snapshot.read_persisted_tt_hwm()?.unwrap_or_default(),
                    names(&build),
                ));
                snapshot
            };
            let registration = registration_slot.as_ref().expect("registered");
            #[cfg(feature = "test-hooks")]
            registration_span.finish();
            #[cfg(feature = "test-hooks")]
            self.run_tt_index_build_fence("registered");
            #[cfg(feature = "test-hooks")]
            let backfill_span = crate::measurement::Span::new("build_backfill");
            let mut count = 0usize;
            {
                // Stream base rows once on SST-ingest backends (`self.db.supports_sst_ingest()`
                // is guaranteed here because non-SST backends returned early above):
                // - HNSW graphs stream into `FlatHnswBuilder` -> `SortedKvArena` -> `ingest_sorted`.
                // - FTS postings and B-tree secondary keys stream into `SortedKvArena` -> `ingest_sorted`,
                //   eliminating monolithic `WriteBatch` commits that stall RocksDB's write thread.
                // - LSH (rare) buffers only its rows and commits in small 64-row chunks.
                use crate::fts::indexing::encode_fts_rows_for_tuple;
                use crate::runtime::hnsw::SortedKvArena;
                use crate::runtime::hnsw_build::{build_threads, FlatHnswBuilder, NodeMeta};

                let hnsw_filters = if !build.hnsw_indices.is_empty() {
                    SessionTx::make_hnsw_filters(&build)?
                } else {
                    BTreeMap::new()
                };
                let key_len = build.index_key_len();
                let mut hnsw_builders: Vec<(
                    &RelationHandle,
                    &crate::runtime::hnsw::HnswIndexManifest,
                    Option<&Vec<crate::data::expr::Bytecode>>,
                    FlatHnswBuilder,
                )> = build
                    .hnsw_indices
                    .iter()
                    .map(|(name, (index, manifest))| {
                        (
                            index,
                            manifest,
                            hnsw_filters.get(name),
                            FlatHnswBuilder::new(manifest),
                        )
                    })
                    .collect();
                let fts_processors =
                    if !build.fts_indices.is_empty() || !build.lsh_indices.is_empty() {
                        snapshot.make_fts_lsh_processors(&build)?
                    } else {
                        BTreeMap::new()
                    };
                // Keep one `SortedKvArena` per detached index so each ingested
                // SST covers only that index's own `RelationId` range and never
                // spans an unrelated concurrent relation's `RelationId`.
                let mut btree_arenas: Vec<SortedKvArena> = (0..build.indices.len())
                    .map(|_| SortedKvArena::new())
                    .collect();
                let mut fts_arenas: Vec<SortedKvArena> = (0..build.fts_indices.len())
                    .map(|_| SortedKvArena::new())
                    .collect();
                let need_lsh = !build.lsh_indices.is_empty();
                let mut lsh_rows: Vec<Tuple> = Vec::new();
                let mut stack = Vec::new();

                for row_res in original.scan_current_uncached(&snapshot) {
                    if count % 256 == 0 {
                        poison.check()?;
                    }
                    let tuple = row_res?;
                    count += 1;

                    for (_, manifest, filter, builder) in &mut hnsw_builders {
                        if let Some(code) = filter {
                            if !crate::data::expr::eval_bytecode_pred(
                                code,
                                &tuple,
                                &mut stack,
                                Default::default(),
                            )? {
                                continue;
                            }
                        }
                        for &idx in &manifest.vec_fields {
                            match tuple.get(idx) {
                                Some(DataValue::Vec(v)) => {
                                    builder.add_node(
                                        NodeMeta {
                                            key: tuple[..key_len].to_vec(),
                                            field_idx: idx,
                                            sub_idx: -1,
                                            hash: v.get_hash().as_ref().to_vec(),
                                        },
                                        v,
                                    )?;
                                }
                                Some(DataValue::List(l)) => {
                                    for (sidx, v) in l.iter().enumerate() {
                                        if let DataValue::Vec(v) = v {
                                            builder.add_node(
                                                NodeMeta {
                                                    key: tuple[..key_len].to_vec(),
                                                    field_idx: idx,
                                                    sub_idx: sidx as i32,
                                                    hash: v.get_hash().as_ref().to_vec(),
                                                },
                                                v,
                                            )?;
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }

                    for ((idx_handle, extraction_indices), arena) in
                        build.indices.values().zip(btree_arenas.iter_mut())
                    {
                        if extraction_indices.iter().any(|&i| i >= tuple.len()) {
                            continue;
                        }
                        let extracted: Vec<_> = extraction_indices
                            .iter()
                            .map(|&i| tuple[i].clone())
                            .collect();
                        let key =
                            idx_handle.encode_key_for_store(&extracted, Default::default())?;
                        arena.push(&key, &[]);
                    }

                    for ((fts_name, (idx_handle, _)), arena) in
                        build.fts_indices.iter().zip(fts_arenas.iter_mut())
                    {
                        let (tokenizer, extractor) = fts_processors.get(fts_name).unwrap();
                        let (postings, _tok_count) = encode_fts_rows_for_tuple(
                            &tuple,
                            extractor,
                            &mut stack,
                            tokenizer,
                            &build,
                            idx_handle,
                        )?;
                        for (k, v) in postings {
                            arena.push(&k, &v);
                        }
                    }

                    if need_lsh {
                        lsh_rows.push(tuple);
                    }
                }

                for (index, manifest, _, builder) in hnsw_builders {
                    poison.check()?;
                    if builder.is_empty() {
                        continue;
                    }
                    let graph = builder.build(manifest, build_threads());
                    poison.check()?;
                    let arena = SessionTx::hnsw_serialise_graph_sorted(&build, index, graph)?;
                    poison.check()?;
                    self.db.ingest_sorted(Box::new(arena.iter_owned()))?;
                }

                for mut arena in btree_arenas.into_iter().chain(fts_arenas) {
                    if arena.is_empty() {
                        continue;
                    }
                    poison.check()?;
                    arena.sort();
                    poison.check()?;
                    self.db.ingest_sorted(Box::new(arena.iter_owned()))?;
                }

                if !lsh_rows.is_empty() {
                    const LSH_BACKFILL_CHUNK: usize = 64;
                    let mut lsh_only = build.clone();
                    lsh_only.indices.clear();
                    lsh_only.fts_indices.clear();
                    lsh_only.hnsw_indices.clear();
                    for chunk in lsh_rows.chunks(LSH_BACKFILL_CHUNK) {
                        poison.check()?;
                        let mut tx = self.transact_write()?;
                        let processors = tx.make_fts_lsh_processors(&lsh_only)?;
                        let perms = tx.make_lsh_hash_perms(&lsh_only);
                        let mut lsh_stack = Vec::new();
                        for row in chunk {
                            poison.check()?;
                            tx.put_in_lsh(&lsh_only, &mut lsh_stack, &processors, row, &perms)?;
                        }
                        self.commit_tx_with_test_hook(&mut tx)?;
                    }
                }
            }
            #[cfg(feature = "test-hooks")]
            backfill_span.finish();
            #[cfg(feature = "test-hooks")]
            self.run_tt_index_build_fence("populated");
            #[cfg(feature = "test-hooks")]
            let catchup_span = crate::measurement::Span::new("build_catchup");
            let profile = std::env::var_os("MNESTIC_BUILD_PROFILE").is_some();
            let mut snapshot = Some(snapshot);
            let mut shadow_active = false;
            if !build.hnsw_indices.is_empty() {
                // WRITE_ONLY (shadow dual-write) catch-up for HNSW builds
                // (Google F1 / PostgreSQL CREATE INDEX CONCURRENTLY pattern):
                // activate shadow mode immediately after backfill so writers
                // maintain all post-backfill mutations directly (avoiding
                // backlog growth when writers are faster than single-threaded
                // replay), while backfill-era keys drain in small pre-warmed
                // batches per brief `tt_commit_lock` turn.
                poison.check()?;
                let initial_keys = {
                    let _section = section(&self.tt_commit_queue, &self.tt_commit_lock, &poison)?;
                    registration.keys()?
                };
                let snap = snapshot.as_ref().expect("snapshot live");
                let mut preloaded: BTreeMap<Vec<u8>, (Tuple, Option<Tuple>)> = BTreeMap::new();
                for key in initial_keys {
                    poison.check()?;
                    let encoded = key.encode_as_key(build.id);
                    let prior = original.get_current(snap, &key)?;
                    preloaded.insert(encoded, (key, prior));
                }
                let activated_len = {
                    let _section = section(&self.tt_commit_queue, &self.tt_commit_lock, &poison)?;
                    let current_snap = self.transact()?;
                    let current = current_snap.get_relation(name, false)?;
                    validate(&original, &current, &build)?;
                    for key in registration.keys()? {
                        let encoded = key.encode_as_key(build.id);
                        if let std::collections::btree_map::Entry::Vacant(e) =
                            preloaded.entry(encoded)
                        {
                            let prior = original.get_current(snap, &key)?;
                            e.insert((key, prior));
                        }
                    }
                    let len = preloaded.len();
                    registration.activate_shadow(build.clone(), preloaded)?;
                    len
                };
                shadow_active = true;
                // `pending_old` now carries every S_0 prior row; release the
                // RocksDB snapshot immediately so compaction is not pinned.
                snapshot = None;
                let t_shadow = Instant::now();
                let mut drained_by_builder = 0usize;
                #[cfg(feature = "test-hooks")]
                self.run_tt_index_build_fence("shadow_active");
                if activated_len > 0 {
                    const MAX_SHADOW_DRAIN_CACHE_VECTORS: usize = 50_000;
                    let drain_filters = SessionTx::make_hnsw_filters(&build)?;
                    // Keep one persistent VectorCache per HNSW index across the
                    // shadow drain loop instead of rebuilding it cold on every
                    // key, evicting any keys mutated since `cache_seq` via
                    // `tt_recent_mutations` before each pre-warm pass.
                    let mut cache_seq = self.tt_recent_mutations.seq();
                    let mut drain_caches: BTreeMap<
                        smartstring::SmartString<smartstring::LazyCompact>,
                        crate::runtime::hnsw::VectorCache<'static, 'static>,
                    > = build
                        .hnsw_indices
                        .iter()
                        .map(|(idx_name, (_, manifest))| {
                            (
                                idx_name.clone(),
                                crate::runtime::hnsw::VectorCache::with_shadow(
                                    manifest.distance,
                                    self.tt_index_builds.clone(),
                                    registration.token(),
                                ),
                            )
                        })
                        .collect();
                    loop {
                        poison.check()?;
                        // Peek the next pending key and pre-search/pre-warm on a
                        // read snapshot OUTSIDE `tt_commit_lock` so the builder
                        // holds `tt_commit_lock` only for 1-key graph mutation + commit.
                        let Some((encoded, key, prior)) = registration.peek_shadow_pending()?
                        else {
                            break;
                        };
                        let mut prewarm = {
                            let pre_tx = self.transact()?;
                            let mutated_opt = self
                                .tt_recent_mutations
                                .mutated_since(cache_seq, original.id.0);
                            cache_seq = pre_tx.tt_pre_seq;
                            let mut pw = crate::runtime::txtime_index::PrewarmedTtHnsw {
                                pre_seq: pre_tx.tt_pre_seq,
                                attached: BTreeMap::new(),
                                shadow: BTreeMap::new(),
                            };
                            let now_spec = original.get_current(&pre_tx, &key).ok().flatten();
                            let mut stack = Vec::new();
                            for (idx_name, (idx_handle, manifest)) in &build.hnsw_indices {
                                let mut vec_cache =
                                    drain_caches.remove(idx_name).unwrap_or_else(|| {
                                        crate::runtime::hnsw::VectorCache::with_shadow(
                                            manifest.distance,
                                            self.tt_index_builds.clone(),
                                            registration.token(),
                                        )
                                    });
                                match mutated_opt.as_ref() {
                                    Some(mutated) => {
                                        vec_cache.evict_logical_keys(manifest, mutated);
                                    }
                                    None => vec_cache.clear(),
                                }
                                if vec_cache.cache.len() > MAX_SHADOW_DRAIN_CACHE_VECTORS {
                                    vec_cache.clear();
                                }
                                let filter = drain_filters.get(idx_name);
                                let new_passes = match (now_spec.as_ref(), filter) {
                                    (None, _) => false,
                                    (Some(_), None) => true,
                                    (Some(r), Some(code)) => {
                                        crate::data::expr::eval_bytecode_pred(
                                            code,
                                            r,
                                            &mut stack,
                                            Default::default(),
                                        )
                                        .unwrap_or(false)
                                    }
                                };
                                let new_for_hnsw =
                                    new_passes.then(|| now_spec.as_ref().unwrap());
                                if let Some(p) = prior.as_ref() {
                                    vec_cache.seed_row(manifest, &build, p);
                                }
                                let mut presearch_map = BTreeMap::new();
                                let mut removals_map = BTreeMap::new();
                                if let Ok(found) = pre_tx.hnsw_presearch_and_warm(
                                    prior.as_ref(),
                                    new_for_hnsw,
                                    manifest,
                                    &build,
                                    idx_handle,
                                    &mut vec_cache,
                                ) {
                                    if !found.inserts.is_empty() {
                                        presearch_map.insert(key.clone(), found.inserts);
                                    }
                                    if !found.removals.is_empty() {
                                        removals_map.insert(key.clone(), found.removals);
                                    }
                                }
                                pw.shadow.insert(
                                    (registration.token(), idx_name.clone()),
                                    crate::runtime::txtime_index::PrewarmedIndex {
                                        vec_cache,
                                        presearch: presearch_map,
                                        removals: removals_map,
                                    },
                                );
                            }
                            pw
                        };
                        let _section = section(&self.tt_commit_queue, &self.tt_commit_lock, &poison)?;
                        if !registration.is_shadow_key_pending(&encoded)? {
                            // Concurrent writer already claimed this key while we pre-warmed;
                            // reclaim the warmed caches for the next iteration.
                            for ((_, idx_name), pw_idx) in prewarm.shadow {
                                drain_caches.insert(idx_name, pw_idx.vec_cache);
                            }
                            continue;
                        }
                        let mut tx = self.transact_write()?;
                        let now = original.get_current(&tx, &key)?;
                        let delta =
                            isize::from(now.is_some()) - isize::from(prior.is_some());
                        tx.maintain_shadow_pairs_with_prewarm(
                            &build,
                            registration.token(),
                            &[(prior, now)],
                            Some(&mut prewarm),
                        )?;
                        poison.check()?;
                        if let Err(err) = self.commit_tx_with_test_hook(&mut tx) {
                            // Invalidate under `_section` so no writer can enter
                            // shadow mode after an ambiguous drain commit error.
                            registration.invalidate_self();
                            return Err(err);
                        }
                        let complete_res = registration.complete_shadow_key(&encoded, delta);
                        self.tt_recent_mutations.record_key(original.id.0, key);
                        complete_res?;
                        for ((_, idx_name), pw_idx) in prewarm.shadow {
                            drain_caches.insert(idx_name, pw_idx.vec_cache);
                        }
                        drained_by_builder += 1;
                        if profile && drained_by_builder % 500 == 0 {
                            eprintln!(
                                "tt shadow drain progress: builder_drained={drained_by_builder}/{activated_len} elapsed={:?}",
                                t_shadow.elapsed()
                            );
                        }
                    }
                }
                if profile && activated_len > 0 {
                    eprintln!(
                        "tt shadow drain: initial={activated_len}, builder_drained={drained_by_builder}, elapsed={:?}",
                        t_shadow.elapsed()
                    );
                }
            } else {
                // Iterative unlocked catch-up rounds for FTS/LSH-only builds:
                // advance the detached generation in small committed batches
                // against pinned read snapshots before acquiring exclusive locks.
                const MAX_UNLOCKED_CATCHUP_ROUNDS: usize = 16;
                const UNLOCKED_CATCHUP_STOP_KEYS: usize = 8;
                let mut prev_pending: Option<usize> = None;
                for round in 0..MAX_UNLOCKED_CATCHUP_ROUNDS {
                    poison.check()?;
                    let (next_snapshot, pending) = {
                        let _section = section(&self.tt_commit_queue, &self.tt_commit_lock, &poison)?;
                        let peek_len = registration.keys()?.len();
                        if peek_len == 0 {
                            break;
                        }
                        if prev_pending.is_some_and(|prev| peek_len * 4 >= prev * 3) {
                            break;
                        }
                        let pending = registration.take_keys()?;
                        let next_snapshot = self.transact()?;
                        let current = next_snapshot.get_relation(name, false)?;
                        validate(&original, &current, &build)?;
                        (next_snapshot, pending)
                    };
                    let pending_len = pending.len();
                    let t_round = Instant::now();
                    count = self.replay_tt_catchup_unlocked(
                        &original,
                        &build,
                        snapshot.as_ref().expect("snapshot live"),
                        &next_snapshot,
                        &pending,
                        count,
                        &poison,
                    )?;
                    if profile {
                        eprintln!(
                            "tt catchup round {round}: replayed {pending_len} keys in {:?}",
                            t_round.elapsed()
                        );
                    }
                    snapshot = Some(next_snapshot);
                    if pending_len <= UNLOCKED_CATCHUP_STOP_KEYS {
                        break;
                    }
                    prev_pending = Some(pending_len);
                }
            }
            {
                #[cfg(feature = "test-hooks")]
                let publication_span = crate::measurement::Span::new("build_publication");
                let admission = self.admit_relation_maintenance(&poison)?;
                let _relation = relation_write(&locks[0], &poison)?;
                drop(admission);
                if !shadow_active {
                    const FINAL_LOCKED_MAX_KEYS: usize = 32;
                    let drain_batch = {
                        let _section = section(&self.tt_commit_queue, &self.tt_commit_lock, &poison)?;
                        let peek_len = registration.keys()?.len();
                        if peek_len > FINAL_LOCKED_MAX_KEYS {
                            let pending = registration.take_keys()?;
                            let next_snapshot = self.transact()?;
                            let current = next_snapshot.get_relation(name, false)?;
                            validate(&original, &current, &build)?;
                            Some((next_snapshot, pending))
                        } else {
                            None
                        }
                    };
                    if let Some((next_snapshot, pending)) = drain_batch {
                        let n = pending.len();
                        let t_drain = Instant::now();
                        count = self.replay_tt_catchup_unlocked(
                            &original,
                            &build,
                            snapshot.as_ref().expect("snapshot live"),
                            &next_snapshot,
                            &pending,
                            count,
                            &poison,
                        )?;
                        if profile {
                            eprintln!(
                                "tt relation_write drain: replayed {n} keys in {:?}",
                                t_drain.elapsed()
                            );
                        }
                        snapshot = Some(next_snapshot);
                    }
                }
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
                let _section = section(&self.tt_commit_queue, &self.tt_commit_lock, &poison)?;
                validate(&original, &current, &build)?;
                if shadow_active {
                    let delta = registration.finish_shadow()?;
                    count = (count as isize + delta).max(0) as usize;
                } else {
                    let snap = snapshot.as_ref().expect("snapshot live");
                    let mut old = BTreeMap::new();
                    for key in registration.keys()? {
                        let prior = original.get_current(snap, &key)?;
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
                    let mut no_prewarm = crate::runtime::txtime_index::PrewarmedTtHnsw::default();
                    tx.maintain_tt_indexes(&changes, &mut no_prewarm)?;
                }
                #[cfg(feature = "test-hooks")]
                catchup_span.finish();
                tx.publish_tt_indexes(current, &build)?;
                poison.check()?;
                publication_attempted = true;
                self.commit_tx_with_test_hook(&mut tx)?;
                self.tt_recent_mutations.invalidate_all();
                #[cfg(feature = "test-hooks")]
                publication_span.finish();
            }
            Ok(result_rows(op, &build, count))
        })();
        match outcome {
            Ok(rows) => {
                drop(registration_slot.take());
                self.cleanup_tt_generations(&original, &names(&build));
                Ok(Some(rows))
            }
            Err(error) => {
                // Drop `registration` only while holding `tt_commit_lock` so no
                // in-flight writer inside `commit_tx_with_tt` can race with its
                // removal or land stray shadow writes after cleanup.
                {
                    let _turn = self
                        .tt_commit_queue
                        .write_with_poison(&Poison::default())
                        .ok();
                    let _drain = self
                        .tt_commit_lock
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    drop(registration_slot.take());
                }
                // Storage commit errors can be ambiguous. Never erase a
                // generation that might already be attached in the catalog.
                if !publication_attempted {
                    self.cleanup_tt_generations(&build, &names(&build));
                }
                Err(error)
            }
        }
    }

    /// Replay catch-up keys into a detached generation without holding
    /// `relation_write` or `tt_commit_lock`, using small committed RocksDB
    /// transactions and pinned read snapshots:
    /// - Phase 1 (HNSW removals) reads neighbor vectors from `snapshot`, where
    ///   every vertex still in `build` has its exact pre-round vector.
    /// - Phase 2 (HNSW insertions) reads neighbor vectors from `next_snapshot`,
    ///   where every surviving/inserted vertex has its exact post-round vector.
    /// - FTS and LSH replay explicit `(old, new)` pairs in chunked transactions.
    #[allow(clippy::too_many_arguments)]
    fn replay_tt_catchup_unlocked(
        &'s self,
        original: &RelationHandle,
        build: &RelationHandle,
        snapshot: &SessionTx<'_>,
        next_snapshot: &SessionTx<'_>,
        pending: &[Tuple],
        mut count: usize,
        poison: &Poison,
    ) -> Result<usize> {
        const HNSW_CATCHUP_CHUNK: usize = 8;
        const FTS_LSH_CATCHUP_CHUNK: usize = 64;
        const MAX_CACHE_VECTORS: usize = 50_000;

        let mut pairs: Vec<(Option<Tuple>, Option<Tuple>)> = Vec::with_capacity(pending.len());
        for key in pending {
            poison.check()?;
            let prior = original.get_current(snapshot, key)?;
            let now = original.get_current(next_snapshot, key)?;
            count = count + usize::from(now.is_some()) - usize::from(prior.is_some());
            pairs.push((prior, now));
        }

        let filters = SessionTx::make_hnsw_filters(build)?;
        let mut stack = Vec::new();
        for (idx_name, (index, manifest)) in &build.hnsw_indices {
            let filter = filters.get(idx_name);
            let mut removed: Vec<&Tuple> = Vec::new();
            let mut inserted: Vec<&Tuple> = Vec::new();
            for (old, new) in &pairs {
                let old_passes = match (old.as_ref(), filter) {
                    (None, _) => false,
                    (Some(_), None) => true,
                    (Some(r), Some(code)) => crate::data::expr::eval_bytecode_pred(
                        code,
                        r,
                        &mut stack,
                        Default::default(),
                    )?,
                };
                let new_passes = match (new.as_ref(), filter) {
                    (None, _) => false,
                    (Some(_), None) => true,
                    (Some(r), Some(code)) => crate::data::expr::eval_bytecode_pred(
                        code,
                        r,
                        &mut stack,
                        Default::default(),
                    )?,
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
                let t_rem = Instant::now();
                let mut remove_cache =
                    crate::runtime::hnsw::VectorCache::with_base_tx(manifest.distance, snapshot);
                for chunk in removed.chunks(HNSW_CATCHUP_CHUNK) {
                    poison.check()?;
                    if remove_cache.cache.len() > MAX_CACHE_VECTORS {
                        remove_cache.cache.clear();
                    }
                    for row in chunk {
                        remove_cache.seed_row(manifest, build, row);
                    }
                    let mut tx = self.transact_write()?;
                    for row in chunk {
                        tx.hnsw_remove_with_cache(manifest, build, index, row, &mut remove_cache)?;
                    }
                    self.commit_tx_with_test_hook(&mut tx)?;
                }
                if std::env::var_os("MNESTIC_BUILD_PROFILE").is_some() {
                    eprintln!(
                        "  hnsw catchup remove: {} rows in {:?}",
                        removed.len(),
                        t_rem.elapsed()
                    );
                }
            }

            if !inserted.is_empty() {
                let t_ins = Instant::now();
                let mut insert_cache = crate::runtime::hnsw::VectorCache::with_base_tx(
                    manifest.distance,
                    next_snapshot,
                );
                for chunk in inserted.chunks(HNSW_CATCHUP_CHUNK) {
                    poison.check()?;
                    if insert_cache.cache.len() > MAX_CACHE_VECTORS {
                        insert_cache.cache.clear();
                    }
                    let mut tx = self.transact_write()?;
                    for row in chunk {
                        tx.hnsw_put_with_cache(
                            manifest,
                            build,
                            index,
                            filter,
                            &mut stack,
                            row,
                            &mut insert_cache,
                        )?;
                    }
                    self.commit_tx_with_test_hook(&mut tx)?;
                }
                if std::env::var_os("MNESTIC_BUILD_PROFILE").is_some() {
                    eprintln!(
                        "  hnsw catchup insert: {} rows in {:?}",
                        inserted.len(),
                        t_ins.elapsed()
                    );
                }
            }
        }

        if !build.indices.is_empty()
            || !build.fts_indices.is_empty()
            || !build.lsh_indices.is_empty()
        {
            for chunk in pairs.chunks(FTS_LSH_CATCHUP_CHUNK) {
                poison.check()?;
                let mut tx = self.transact_write()?;
                let processors = tx.make_fts_lsh_processors(build)?;
                let hashes = tx.make_lsh_hash_perms(build);
                let chunk_pairs: Vec<(Option<&Tuple>, Option<&Tuple>)> = chunk
                    .iter()
                    .map(|(old, new)| (old.as_ref(), new.as_ref()))
                    .collect();
                if !build.indices.is_empty() {
                    tx.maintain_tt_btree_pairs(build, &chunk_pairs)?;
                }
                if !build.fts_indices.is_empty() || !build.lsh_indices.is_empty() {
                    tx.maintain_tt_fts_lsh_pairs(
                        build,
                        &chunk_pairs,
                        &processors,
                        &hashes,
                        &mut stack,
                    )?;
                }
                self.commit_tx_with_test_hook(&mut tx)?;
            }
        }

        Ok(count)
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
            SysOp::RemoveIndex(base, _)
            | SysOp::RepairCorrupt(base)
            | SysOp::SetTriggers(base, _, _, _) => {
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
        let admission = self.admit_relation_maintenance(&poison)?;
        let mut guards = Vec::new();
        for lock in &locks {
            guards.push(relation_write(lock, &poison)?);
        }
        drop(admission);
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
        let guard = section(&self.tt_commit_queue, &self.tt_commit_lock, &poison)?;
        tx.defer_tt_cleanup = true;
        let rows = self.run_sys_op_with_tx(&mut tx, op, false, true, true)?;
        poison.check()?;
        self.commit_tx_with_test_hook(&mut tx)?;
        self.tt_recent_mutations.invalidate_all();
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

#[cfg(all(test, feature = "test-hooks"))]
mod admission_measurement_tests {
    use super::*;
    use crate::measurement::capture;

    #[test]
    fn admission_timeout_and_later_success_are_distinguishable() {
        let lock = crate::runtime::relation_lock::RelationLock::default();
        std::thread::scope(|scope| {
            let reader = lock.read().unwrap();
            scope.spawn(|| {
                let poison = Poison { deadline: Some(Instant::now() + Duration::from_millis(15)), ..Default::default() };
                let (result, trace) = capture(1, true, || relation_write(&lock, &poison).map(drop));
                assert!(result.is_err());
                assert_eq!(trace.events.len(), 1);
                assert_eq!(trace.events[0].phase, "relation_write_admission");
                assert!(!trace.events[0].completed);
            }).join().unwrap();
            drop(reader);
        });
        let (result, trace) = capture(2, true, || relation_write(&lock, &Poison::default()).map(drop));
        result.unwrap();
        assert!(trace.events[0].completed);
    }
}
