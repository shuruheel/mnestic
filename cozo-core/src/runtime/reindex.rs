/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `::reindex <relation>` — rebuild a relation's HNSW / FTS / LSH indexes in
//! place, from the index configuration the database already stores (mnestic fork,
//! 0.12.1).
//!
//! # Why this exists
//!
//! Three separate stories converge on one missing operation:
//!
//! 1. **The FTS posting leak** (fixed in the same release). Every database from
//!    0.8.x through 0.12.0 that updated a row in place on an FTS-only relation
//!    carries ghost postings and skewed BM25 statistics *today*. Fixing the write
//!    path stops new leakage; it cannot evict what has already been written. This
//!    is the repair.
//! 2. **The bulk-load paths.** `import_relations` and `import_from_backup` do not
//!    maintain these indexes — that is what makes them fast — so rows they load
//!    are invisible to search until a rebuild. Both used to say "drop + recreate",
//!    which meant reconstructing the original `::hnsw`/`::fts` creation script
//!    (extractor, tokenizer, filters, `ef_construction`, `m_neighbours`…) by hand
//!    from `::indices` output. Now they say `::reindex`.
//! 3. **Damaged indexes.** A rebuild is the blunt cure for an index whose contents
//!    have drifted from its base relation, whatever the cause.
//!
//! # Why *in place*, and not drop + recreate
//!
//! Drop-and-recreate is the obvious implementation and it is wrong, because the
//! stored manifest is not a faithful round-trip of the creation config for LSH:
//! `MinHashLshIndexManifest` keeps the *derived* `n_bands` / `n_rows_in_band` /
//! `perms`, but **not** the `false_positive_weight` / `false_negative_weight` that
//! produced them. Recreating from a reconstructed config would silently recompute
//! that geometry from defaults and hand the user an index with a different
//! recall/precision profile than the one they asked for. A repair operation may
//! not do that.
//!
//! So each index is rebuilt against its own stored manifest: its rows are deleted
//! and re-derived from the base relation through **the same per-row maintenance
//! functions `:put` uses** (`put_in_fts`, `put_in_lsh`, `update_in_hnsw`). A
//! rebuilt index is therefore byte-identical to an incrementally-maintained one —
//! that equivalence is the whole correctness argument, and it is what
//! `reindex.rs`'s tests assert.
//!
//! # Semantics
//!
//! - One write transaction. A crash mid-rebuild rolls back to the old (possibly
//!   ghost-carrying, but intact) index; re-running `::reindex` is always the cure.
//! - Holds the relation's **write lock** for the duration. On a large relation
//!   this is minutes: it is a maintenance operation, not an online one.
//! - **Nothing auto-invokes it.** The import paths point at it; they do not run
//!   it. (House rule, learned expensively: repair and rebuild stay explicit —
//!   never auto-start a rebuild inside another operation.)
//! - A relation with no HNSW/FTS/LSH index is a loud no-op, not an error: it
//!   returns a row saying so, which keeps `::reindex` scriptable across a set of
//!   relations without the caller having to know which ones carry search indexes.

use miette::Result;
use smartstring::{LazyCompact, SmartString};

use crate::data::tuple::{Tuple, TupleT};
use crate::data::value::DataValue;
use crate::runtime::relation::RelationHandle;
use crate::runtime::transact::SessionTx;
use crate::utils::TempCollector;

/// What one index rebuild did, for the status rows `::reindex` returns.
pub(crate) struct ReindexReport {
    pub(crate) index: SmartString<LazyCompact>,
    pub(crate) kind: &'static str,
    pub(crate) rows: usize,
}

impl SessionTx<'_> {
    /// Delete every row of an index relation, within this transaction.
    ///
    /// Deliberately *not* `del_range_from_persisted`, which the index-*removal*
    /// path uses: that writes straight through to the store, outside the
    /// transaction, so a failure afterwards would leave the index truncated and
    /// unrecoverable. Here the deletes are part of the transaction, so a crash
    /// mid-rebuild rolls back to the intact old index.
    fn clear_index_relation(&mut self, idx_handle: &RelationHandle) -> Result<()> {
        let lower = Tuple::default().encode_as_key(idx_handle.id);
        let upper = Tuple::default().encode_as_key(idx_handle.id.next());

        let mut keys = TempCollector::default();
        for pair in self.store_tx.range_scan(&lower, &upper) {
            let (k, _) = pair?;
            keys.push(k);
        }
        for k in keys.into_iter() {
            self.store_tx.del(&k)?;
        }
        Ok(())
    }

    /// Rebuild every HNSW / FTS / LSH index on `rel_name` from its stored
    /// manifest. See the module docs for why this is in place rather than
    /// drop + recreate.
    pub(crate) fn reindex_relation(&mut self, rel_name: &str) -> Result<Vec<ReindexReport>> {
        let rel_handle = self.get_relation(rel_name, true)?;

        let mut reports = vec![];
        if rel_handle.hnsw_indices.is_empty()
            && rel_handle.fts_indices.is_empty()
            && rel_handle.lsh_indices.is_empty()
        {
            return Ok(reports);
        }

        // The base rows, read once and shared by every index below.
        let mut base = TempCollector::default();
        for tuple in rel_handle.scan_all(self) {
            base.push(tuple?);
        }
        let tuples: Vec<Tuple> = base.into_iter().collect();

        // Rebuild through the *same* per-row maintenance path `:put` uses, so a
        // rebuilt index is byte-identical to an incrementally-maintained one.
        // These builders resolve extractors/tokenizers/filters/permutations from
        // the relation's stored manifests — no config is reconstructed anywhere.
        let hnsw_filters = Self::make_hnsw_filters(&rel_handle)?;
        let fts_lsh_processors = self.make_fts_lsh_processors(&rel_handle)?;
        let lsh_perms = self.make_lsh_hash_perms(&rel_handle);
        let mut stack = vec![];

        for (name, (idx_handle, _)) in rel_handle.fts_indices.iter() {
            self.clear_index_relation(idx_handle)?;
            reports.push(ReindexReport {
                index: name.clone(),
                kind: "fts",
                rows: tuples.len(),
            });
        }
        for (name, (idx_handle, inv_idx_handle, _)) in rel_handle.lsh_indices.iter() {
            self.clear_index_relation(idx_handle)?;
            self.clear_index_relation(inv_idx_handle)?;
            reports.push(ReindexReport {
                index: name.clone(),
                kind: "lsh",
                rows: tuples.len(),
            });
        }
        for (name, (idx_handle, _)) in rel_handle.hnsw_indices.iter() {
            self.clear_index_relation(idx_handle)?;
            reports.push(ReindexReport {
                index: name.clone(),
                kind: "hnsw",
                rows: tuples.len(),
            });
        }

        for tuple in &tuples {
            self.update_in_hnsw(&rel_handle, &mut stack, &hnsw_filters, tuple)?;
            self.put_in_fts(&rel_handle, &mut stack, &fts_lsh_processors, tuple)?;
            self.put_in_lsh(
                &rel_handle,
                &mut stack,
                &fts_lsh_processors,
                tuple,
                &lsh_perms,
            )?;
        }

        // Re-derive the BM25 corpus counter from the index we just built.
        //
        // This is load-bearing, and its failure mode is quiet enough to be worth
        // spelling out. The counter (`total_tokens`, `n_docs`) lives in a
        // process-level cache that `put_in_fts` bumps incrementally. Clearing the
        // index rows above does *not* decrement it — so the re-puts add the whole
        // corpus a second time, on top of a count that still includes whatever the
        // stale index contained. The counter is consumed as the ratio
        // `total_tokens / n_docs` (avgdl), which is why this hides so well: if the
        // corpus happens to be re-derived unchanged, the doubled numerator and
        // denominator cancel and nothing looks wrong. It only surfaces when the
        // rebuild actually *changes* the corpus — i.e. exactly when the index was
        // stale, i.e. exactly when someone ran `::reindex` to repair it. Measured:
        // a repaired single-document index scored 0.349 against a clean index's
        // 0.288 without this line. `tests/reindex.rs::reindex_evicts_ghost_postings`
        // pins it (and needs documents of *unequal* length to see it at all).
        for (idx_handle, _) in rel_handle.fts_indices.values() {
            self.rebuild_fts_doc_stats(idx_handle)?;
        }

        Ok(reports)
    }
}

/// Render the reports as the `NamedRows` payload `::reindex` returns.
pub(crate) fn reindex_rows(reports: Vec<ReindexReport>) -> (Vec<String>, Vec<Vec<DataValue>>) {
    let headers = vec!["index".to_string(), "kind".to_string(), "rows".to_string()];
    let rows = reports
        .into_iter()
        .map(|r| {
            vec![
                DataValue::from(r.index.as_str()),
                DataValue::from(r.kind),
                DataValue::from(r.rows as i64),
            ]
        })
        .collect();
    (headers, rows)
}
