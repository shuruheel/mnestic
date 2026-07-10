/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 * Portions Copyright 2022, The Cozo Project Authors (the `Arc`-of-shared-state
 * idiom cloned into every `SessionTx` follows `fts_doc_stats_cache` and
 * `tt_clock` in runtime/db.rs).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Freshness substrate for the cached graph projection (mnestic fork;
//! `docs/specs/graph-projection.md` §3.3–3.4).
//!
//! A projection is a named, in-memory CSR adjacency built from stored
//! relations and reused across `FixedRule` calls. The signed guarantee is
//! **always-fresh**: a projection never serves data that differs from what the
//! consuming transaction's own scan of the sources would return, in *either*
//! direction — no stale entries to fresh snapshots, and no fresh entries to
//! stale snapshots. Under write churn it degrades to build-per-query, never
//! worse than today.
//!
//! # The watermark protocol
//!
//! State: a process-global monotone [`ProjectionCache::bump_seq`], and a
//! per-relation [`RelState`] holding the token of that relation's last
//! observed mutation plus a count of commits currently in flight against it.
//! An absent map entry reads as `{token: 0, inflight: 0}` — "never mutated in
//! this process" — which is sound because entries are validated against exact
//! resolved relation ids, and destroyed ids are purged and never re-resolved
//! (`relation_store_id` is monotone).
//!
//! Every [`SessionTx`](crate::runtime::transact::SessionTx) captures
//! `watermark = bump_seq.load()` **strictly before** the storage transaction
//! opens, because opening *is* pinning: the snapshot is a side effect of the
//! constructor. A bump landing between the load and the pin only pushes that
//! relation's token above the watermark, i.e. a conservative miss.
//!
//! Writers, per the dirty set collected during the transaction:
//! [`begin_commit`](ProjectionCache::begin_commit) raises `inflight` before
//! the storage commit; [`finish_commit`](ProjectionCache::finish_commit)
//! assigns a fresh token and lowers `inflight` after it returns — **on failure
//! as well as success**, because `mem`'s `del_range_from_persisted` mutates the
//! persisted store outside the transaction cache, so a failed destructive
//! transaction may still have changed state. A transaction dropped without
//! committing takes [`bump_aborted`](ProjectionCache::bump_aborted) for the
//! same reason.
//!
//! The freshness predicate, evaluated under the map mutex, is
//!
//! ```text
//! FRESH(T, R)  ≜  inflight(R) == 0  ∧  token(R) <= T.watermark
//! ```
//!
//! Both consuming a cached entry and inserting one require `FRESH` on every
//! source relation. **Claim**: `FRESH(T, R)` implies R's content in T's
//! snapshot equals R's content committed as of the moment of evaluation. Let W
//! be any committed write to R.
//!
//! * *W committed before T's pin* (so W **is** in T's snapshot): either W's
//!   bump completed before T's watermark load — then `token(R) <= watermark`
//!   and W is counted on both sides — or it had not, in which case at
//!   evaluation time the bump has since run (`token(R) > watermark`: a
//!   `fetch_add` sequenced after the load returns a strictly larger value) or
//!   it still has not (`inflight >= 1`). Either way `FRESH` fails.
//! * *W committed after T's pin* (so W is **not** in T's snapshot): its bump
//!   follows its commit, which follows the pin, which follows the load, so
//!   `token(R) > watermark` once bumped and `inflight >= 1` until then.
//!   `FRESH` fails.
//!
//! So under `FRESH` the snapshot holds exactly the committed state. Insertion
//! tags an entry with tokens denoting exactly that content version; a consumer
//! demands the same tokens *and* `FRESH` for itself, so entry content =
//! current content = consumer-snapshot content. Both staleness directions are
//! closed. Every race degrades to a spurious miss, and aborts, failures and
//! drops only add more of them.
//!
//! # Scope
//!
//! This module is **not** behind `graph-algo`. The dirty-set hooks live in
//! thirteen mutation paths across `query/stored.rs`, `runtime/relation.rs` and
//! `runtime/db.rs`; conditionally compiling them is where a silently missed
//! hook — the one failure mode this protocol exists to prevent — would hide.
//! The cost of maintaining the substrate with no projections in existence is
//! one atomic load per transaction, one `BTreeSet` insert per mutation
//! *statement* (not per row), and one uncontended mutex acquisition per
//! *writing* commit. The cached entries themselves, which own the
//! `graph`-crate CSR types, arrive in Phase 2 behind `graph-algo`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::runtime::relation::RelationId;

/// Per-relation freshness bookkeeping. Absent from the map ⇔ `RelState::default()`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RelState {
    /// Token of the last observed mutation of this relation. Monotone,
    /// process-unique, and directly comparable with a transaction's watermark.
    pub(crate) token: u64,
    /// Commits currently between `begin_commit` and `finish_commit` for this
    /// relation. Non-zero ⇒ its committed content is indeterminate to us.
    pub(crate) inflight: u32,
}

/// A test-only parking point inside `commit_tx`, invoked after the storage
/// commit returns and before the token bump — precisely the window in which
/// `inflight` is the only thing standing between a concurrent reader and a
/// stale hit. See `docs/specs/graph-projection.md` §5.
#[cfg(any(test, feature = "test-hooks"))]
pub(crate) type CommitFence = std::sync::Arc<dyn Fn(&BTreeSet<RelationId>) + Send + Sync>;

/// Process-global freshness state for graph projections; one per [`Db`], held
/// behind an `Arc` and cloned into every `SessionTx`.
///
/// [`Db`]: crate::Db
#[derive(Default)]
pub(crate) struct ProjectionCache {
    /// Monotone token source. Also the watermark clock: a transaction's
    /// watermark is a snapshot of this counter taken before its storage
    /// transaction pins.
    bump_seq: AtomicU64,
    rel_states: Mutex<BTreeMap<RelationId, RelState>>,
    #[cfg(any(test, feature = "test-hooks"))]
    commit_fence: Mutex<Option<CommitFence>>,
}

impl ProjectionCache {
    /// The value a transaction opening *now* would capture. Must be read
    /// before the storage transaction opens (§3.3).
    pub(crate) fn watermark(&self) -> u64 {
        self.bump_seq.load(Ordering::SeqCst)
    }

    /// Mint a token strictly greater than every watermark captured before this
    /// call. `SeqCst` is load-bearing: the correctness argument needs a single
    /// total order over `bump_seq` in which a `fetch_add` sequenced after a
    /// `load` returns a strictly larger value.
    fn fresh_token(&self) -> u64 {
        self.bump_seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn states(&self) -> std::sync::MutexGuard<'_, BTreeMap<RelationId, RelState>> {
        // Poison recovery: a panic while holding this lock can only have left
        // a relation's token low or its `inflight` high. A low token is
        // impossible (tokens are only ever raised), and a stuck-high
        // `inflight` denies hits forever — degraded, never unsound. Refusing
        // to open the map would instead take down every later transaction.
        self.rel_states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `FRESH(T, R)` for a single relation. Prefer [`all_fresh`] for a source
    /// set: it evaluates the whole set under one acquisition, so no commit can
    /// interleave between two of its relations.
    ///
    /// [`all_fresh`]: ProjectionCache::all_fresh
    // Phase 2 (lookup + insert) is the production consumer; Phase 1 ships the
    // predicate with its tests.
    #[allow(dead_code)]
    pub(crate) fn is_fresh(&self, rel: RelationId, watermark: u64) -> bool {
        self.all_fresh(std::iter::once(rel), watermark)
    }

    /// `∀R ∈ rels. FRESH(T, R)`, evaluated atomically with respect to commits.
    #[allow(dead_code)]
    pub(crate) fn all_fresh(&self, rels: impl Iterator<Item = RelationId>, watermark: u64) -> bool {
        let states = self.states();
        rels.into_iter().all(|rel| match states.get(&rel) {
            None => true, // never mutated in this process: token 0, inflight 0
            Some(st) => st.inflight == 0 && st.token <= watermark,
        })
    }

    /// Raise `inflight` for every relation this transaction is about to write.
    /// Called *before* the storage commit; pairs with [`finish_commit`].
    ///
    /// [`finish_commit`]: ProjectionCache::finish_commit
    pub(crate) fn begin_commit(&self, dirty: &BTreeSet<RelationId>) {
        let mut states = self.states();
        for rel in dirty {
            states.entry(*rel).or_default().inflight += 1;
        }
    }

    /// Assign fresh tokens and lower `inflight`, then — only if the commit
    /// actually landed — purge the relations this transaction destroyed.
    ///
    /// The bump runs whether or not the commit succeeded: a failed destructive
    /// transaction on `mem` may already have mutated the persisted store
    /// through `del_range_from_persisted`, which does not go through the
    /// transaction cache. The purge does not, because a failed commit leaves
    /// the catalog row — and hence the relation id — alive.
    ///
    /// Purging *after* the bump is required by §3.3: a purge at the hook site
    /// would be resurrected by this function's own `entry(..).or_default()`.
    pub(crate) fn finish_commit(
        &self,
        dirty: &BTreeSet<RelationId>,
        retired: &BTreeSet<RelationId>,
        committed: bool,
    ) {
        let mut states = self.states();
        for rel in dirty {
            let token = self.fresh_token();
            let st = states.entry(*rel).or_default();
            st.token = token;
            st.inflight = st.inflight.saturating_sub(1);
        }
        if committed {
            for rel in retired {
                states.remove(rel);
            }
        }
    }

    /// A transaction with a non-empty dirty set went away without committing.
    /// Bump its relations' tokens — there is no `inflight` to lower, since it
    /// was never raised — so that any entry tagged against them misses.
    ///
    /// Conservative by design: `mem`'s out-of-transaction range deletes mean an
    /// abort is not proof that nothing changed.
    pub(crate) fn bump_aborted(&self, dirty: &BTreeSet<RelationId>) {
        let mut states = self.states();
        for rel in dirty {
            let token = self.fresh_token();
            states.entry(*rel).or_default().token = token;
        }
    }

    /// Invalidate everything this process knows about. Used by
    /// `Db::restore_backup`, which replaces the whole keyspace through
    /// `batch_put` — outside any `SessionTx`, so no dirty set can see it.
    ///
    /// Bumps rather than clears: clearing would make every relation read as
    /// `{token: 0, inflight: 0}`, i.e. *fresh*, which is the wrong direction.
    /// Phase 2 also drops the cached entries here.
    pub(crate) fn invalidate_all(&self) {
        let mut states = self.states();
        for st in states.values_mut() {
            st.token = self.bump_seq.fetch_add(1, Ordering::SeqCst) + 1;
        }
    }

    /// Install the test-only commit fence. Returns the previous one.
    #[cfg(any(test, feature = "test-hooks"))]
    pub(crate) fn set_commit_fence(&self, fence: Option<CommitFence>) -> Option<CommitFence> {
        let mut slot = self
            .commit_fence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::replace(&mut *slot, fence)
    }

    /// Run the fence, if installed. Called from `commit_tx` between the
    /// storage commit and the token bump. The fence is cloned out of its lock
    /// before running so that it may itself touch this cache.
    #[cfg(any(test, feature = "test-hooks"))]
    pub(crate) fn run_commit_fence(&self, dirty: &BTreeSet<RelationId>) {
        let fence = {
            let slot = self
                .commit_fence
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slot.clone()
        };
        if let Some(fence) = fence {
            fence(dirty);
        }
    }

    /// Snapshot of a relation's state, for tests and `::graph list`.
    #[allow(dead_code)] // Phase 2: `::graph list` reports it.
    pub(crate) fn rel_state(&self, rel: RelationId) -> RelState {
        self.states().get(&rel).copied().unwrap_or_default()
    }
}

#[cfg(feature = "test-hooks")]
impl<S> crate::Db<S> {
    /// Install a callback run inside every *writing* `commit_tx`, after the
    /// storage commit has returned and before the freshness tokens are bumped.
    /// Passing `None` removes it; the previous fence is dropped.
    ///
    /// This exists so a test can park a writer inside the one window where a
    /// commit is durable but its token has not moved, and check that a
    /// concurrent reader is denied a cache hit by `inflight` alone. It is
    /// **not** a supported API: it is compiled only under the internal
    /// `test-hooks` feature, and it lets arbitrary code run mid-commit.
    #[doc(hidden)]
    pub fn set_commit_fence_for_tests(
        &self,
        fence: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    ) {
        let adapted: Option<CommitFence> =
            fence.map(|f| std::sync::Arc::new(move |_: &BTreeSet<RelationId>| f()) as CommitFence);
        self.graph_projections.set_commit_fence(adapted);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::mpsc;
    use std::sync::Arc;

    use super::{ProjectionCache, RelState};
    use crate::data::value::DataValue;
    use crate::runtime::relation::RelationId;
    use crate::storage::mem::MemStorage;
    use crate::{new_cozo_mem, Db, NamedRows, ScriptMutability};

    fn ids(raw: &[u64]) -> BTreeSet<RelationId> {
        raw.iter().copied().map(RelationId).collect()
    }

    // ---- Protocol algebra: the predicate and the writer rule, no storage ----

    /// An absent map entry is `{token: 0, inflight: 0}`, so a relation nobody
    /// has ever mutated is fresh to a transaction that opened at watermark 0.
    #[test]
    fn a_never_mutated_relation_is_fresh_at_any_watermark() {
        let c = ProjectionCache::default();
        assert_eq!(c.watermark(), 0);
        assert!(c.is_fresh(RelationId(7), 0));
        assert_eq!(c.rel_state(RelationId(7)), RelState::default());
    }

    /// The core of the protocol: a commit's token lands strictly above every
    /// watermark captured before it, and at or below every watermark captured
    /// after. This is what denies a fresh entry to a stale snapshot.
    #[test]
    fn a_commit_token_separates_watermarks_taken_before_and_after_it() {
        let c = ProjectionCache::default();
        let r = ids(&[1]);
        let before = c.watermark();

        c.begin_commit(&r);
        c.finish_commit(&r, &Default::default(), true);
        let after = c.watermark();

        assert!(before < after);
        assert!(
            !c.is_fresh(RelationId(1), before),
            "stale to the old reader"
        );
        assert!(c.is_fresh(RelationId(1), after), "fresh to the new reader");
    }

    /// Between `begin_commit` and `finish_commit` the relation's committed
    /// content is indeterminate: the storage commit may or may not have landed.
    /// `inflight` denies freshness regardless of how high the watermark is.
    #[test]
    fn an_inflight_commit_denies_freshness_at_every_watermark() {
        let c = ProjectionCache::default();
        let r = ids(&[1]);

        c.begin_commit(&r);
        assert!(!c.is_fresh(RelationId(1), u64::MAX));
        assert_eq!(c.rel_state(RelationId(1)).inflight, 1);

        c.finish_commit(&r, &Default::default(), true);
        assert_eq!(c.rel_state(RelationId(1)).inflight, 0);
        assert!(c.is_fresh(RelationId(1), c.watermark()));
    }

    /// Overlapping writers to one relation: freshness returns only when the
    /// last of them has finished, not the first.
    #[test]
    fn inflight_counts_overlapping_writers() {
        let c = ProjectionCache::default();
        let r = ids(&[1]);

        c.begin_commit(&r);
        c.begin_commit(&r);
        c.finish_commit(&r, &Default::default(), true);
        assert!(
            !c.is_fresh(RelationId(1), u64::MAX),
            "one writer is still in flight"
        );

        c.finish_commit(&r, &Default::default(), true);
        assert!(c.is_fresh(RelationId(1), c.watermark()));
    }

    /// A failed commit bumps too. `mem`'s `del_range_from_persisted` writes
    /// outside the transaction cache, so an error is not proof of no change.
    #[test]
    fn a_failed_commit_still_bumps_the_token() {
        let c = ProjectionCache::default();
        let r = ids(&[1]);
        let before = c.watermark();

        c.begin_commit(&r);
        c.finish_commit(&r, &Default::default(), false);

        assert!(!c.is_fresh(RelationId(1), before));
        assert_eq!(c.rel_state(RelationId(1)).inflight, 0);
    }

    /// A destroyed relation's state is purged only when the destroying commit
    /// landed — and always *after* the bump, never at the hook site, or
    /// `finish_commit`'s own `entry(..).or_default()` would resurrect it.
    #[test]
    fn retired_state_is_purged_on_commit_and_kept_on_failure() {
        let c = ProjectionCache::default();
        let r = ids(&[1]);

        c.begin_commit(&r);
        c.finish_commit(&r, &r, false);
        assert_ne!(
            c.rel_state(RelationId(1)),
            RelState::default(),
            "a failed destroy leaves the catalog row, so the id lives on"
        );

        c.begin_commit(&r);
        c.finish_commit(&r, &r, true);
        assert_eq!(c.rel_state(RelationId(1)), RelState::default());
    }

    /// `all_fresh` is a conjunction over the source set, evaluated under one
    /// lock acquisition so no commit can interleave between two sources.
    #[test]
    fn all_fresh_requires_every_source() {
        let c = ProjectionCache::default();
        let w = c.watermark();
        assert!(c.all_fresh(ids(&[1, 2]).into_iter(), w));

        c.begin_commit(&ids(&[2]));
        c.finish_commit(&ids(&[2]), &Default::default(), true);

        assert!(c.is_fresh(RelationId(1), w));
        assert!(!c.all_fresh(ids(&[1, 2]).into_iter(), w));
    }

    /// A dropped transaction bumps without touching `inflight` — it never
    /// raised it.
    #[test]
    fn bump_aborted_raises_the_token_without_touching_inflight() {
        let c = ProjectionCache::default();
        let before = c.watermark();

        c.bump_aborted(&ids(&[1]));

        assert!(!c.is_fresh(RelationId(1), before));
        assert_eq!(c.rel_state(RelationId(1)).inflight, 0);
    }

    /// `invalidate_all` must raise tokens, never clear the map: clearing would
    /// make every relation read as `{token: 0, inflight: 0}` — i.e. *fresh*,
    /// the opposite of invalidation.
    #[test]
    fn invalidate_all_raises_tokens_rather_than_clearing_them() {
        let c = ProjectionCache::default();
        let r = ids(&[1]);
        c.begin_commit(&r);
        c.finish_commit(&r, &Default::default(), true);
        let settled = c.watermark();
        assert!(c.is_fresh(RelationId(1), settled));

        c.invalidate_all();

        assert!(
            !c.is_fresh(RelationId(1), settled),
            "a watermark taken before the invalidation must no longer be fresh"
        );
        assert!(c.rel_state(RelationId(1)).token > settled);
    }

    // ---- The dirty-set hooks, driven through a real Db ----

    fn run(db: &Db<MemStorage>, script: &str) -> miette::Result<NamedRows> {
        db.run_script(script, Default::default(), ScriptMutability::Mutable)
    }

    fn rel_id(db: &Db<MemStorage>, name: &str) -> RelationId {
        db.transact().unwrap().get_relation(name, false).unwrap().id
    }

    /// Watermark of a transaction opened *now*, i.e. what a reader starting
    /// here would carry.
    fn watermark(db: &Db<MemStorage>) -> u64 {
        db.graph_projections.watermark()
    }

    /// Did `script` invalidate `rel`? A reader that pinned before the script
    /// ran must find the relation stale afterwards, and a reader that pins
    /// after it must find it fresh again.
    #[track_caller]
    fn assert_invalidates(db: &Db<MemStorage>, rel: RelationId, script: &str) {
        let before = watermark(db);
        run(db, script).unwrap();
        assert!(
            !db.graph_projections.is_fresh(rel, before),
            "script did not invalidate {rel:?}: {script}"
        );
        assert!(
            db.graph_projections.is_fresh(rel, watermark(db)),
            "invalidation left {rel:?} permanently stale: {script}"
        );
    }

    #[track_caller]
    fn assert_preserves(db: &Db<MemStorage>, rel: RelationId, script: &str) {
        let before = watermark(db);
        run(db, script).unwrap();
        assert!(
            db.graph_projections.is_fresh(rel, before),
            "script needlessly invalidated {rel:?}: {script}"
        );
    }

    fn plain_db() -> Db<MemStorage> {
        let db = new_cozo_mem().unwrap();
        run(&db, ":create r {a: Int => b: Int}").unwrap();
        db
    }

    /// §3.4 rows 1–3: the three row-level mutation paths each dirty their
    /// relation. Also row 6: a trigger's writes ride the same `SessionTx`, so
    /// the hooks fire for the trigger's target too.
    #[test]
    fn row_level_mutations_invalidate() {
        let db = plain_db();
        let r = rel_id(&db, "r");

        assert_invalidates(&db, r, "?[a, b] <- [[1, 1]] :put r {a => b}");
        assert_invalidates(&db, r, "?[a, b] <- [[2, 2]] :insert r {a => b}");
        assert_invalidates(&db, r, "?[a, b] <- [[1, 9]] :update r {a => b}");
        assert_invalidates(&db, r, "?[a] <- [[1]] :rm r {a}");
        assert_invalidates(&db, r, "?[a] <- [[2]] :delete r {a}");
    }

    /// §3.4 row 6: a `:put` trigger writing into a second relation dirties
    /// *that* relation, because the trigger's script runs on the same
    /// transaction and therefore through the same hooks.
    #[test]
    fn trigger_writes_invalidate_the_triggered_relation() {
        let db = plain_db();
        run(&db, ":create audit {a: Int}").unwrap();
        run(
            &db,
            "::set_triggers r
             on put { ?[a] := _new[a, _] :put audit {a} }",
        )
        .unwrap();
        let audit = rel_id(&db, "audit");

        assert_invalidates(&db, audit, "?[a, b] <- [[5, 5]] :put r {a => b}");
        assert_eq!(
            run(&db, "?[a] := *audit[a]").unwrap().rows.len(),
            1,
            "the trigger really did write"
        );
    }

    /// §3.4 row 5: `:replace` destroys the relation and creates a new one with
    /// a fresh id. The *old* id must be dirtied (entries sourced on it are
    /// dropped in Phase 2) and then purged, because nothing will ever resolve
    /// to it again. The new id is a different relation entirely.
    #[test]
    fn replace_retires_the_old_id_and_mints_a_new_one() {
        let db = plain_db();
        let old = rel_id(&db, "r");
        let before = watermark(&db);

        run(&db, "?[a, b] <- [[1, 1]] :replace r {a: Int => b: Int}").unwrap();

        let new = rel_id(&db, "r");
        assert_ne!(old, new, ":replace mints a new relation id");
        assert_eq!(
            db.graph_projections.rel_state(old),
            RelState::default(),
            "the retired id's state is purged after the commit-time bump"
        );
        assert!(
            !db.graph_projections.is_fresh(new, before),
            "the new relation is itself dirtied by the put"
        );
    }

    /// §3.4 row 9: `::remove` dirties the relation and then retires its id.
    ///
    /// The oracle is deliberately two-sided. `is_fresh` cannot serve here: a
    /// purged id reads as `{token: 0, inflight: 0}`, i.e. *fresh*, and that is
    /// correct — nothing will ever resolve to it again, so no entry can name
    /// it. What must hold is that the id was bumped (so Phase 2's eager drop
    /// has a dirty set to work from) and *then* purged. Seeding a token first
    /// is what makes the purge distinguishable from never having marked it.
    #[test]
    fn remove_relation_dirties_then_retires_the_id() {
        let db = plain_db();
        let r = rel_id(&db, "r");
        run(&db, "?[a, b] <- [[1, 1]] :put r {a => b}").unwrap();
        assert_ne!(db.graph_projections.rel_state(r), RelState::default());
        let before = watermark(&db);

        run(&db, "::remove r").unwrap();

        assert!(
            watermark(&db) > before,
            "::remove must bump the relation it destroys"
        );
        assert_eq!(
            db.graph_projections.rel_state(r),
            RelState::default(),
            "and then purge it, after the bump — a hook-site purge would be \
             resurrected by finish_commit's or_default()"
        );
    }

    /// §3.4 row 10.
    #[test]
    fn repair_corrupt_invalidates() {
        let db = plain_db();
        let r = rel_id(&db, "r");
        assert_invalidates(&db, r, "::repair_corrupt r");
    }

    /// §3.4 rows 11 and 12. `::evict` writes audit rows into a *second* stored
    /// relation, which must be dirtied as well — it is a legal projection
    /// source like any other.
    #[test]
    fn history_gc_and_evict_invalidate_both_relations_they_touch() {
        let db = new_cozo_mem().unwrap();
        run(&db, ":create hist {k: Int, tt: TxTime => v: Int}").unwrap();
        run(&db, "?[k, v] <- [[1, 10]] :put hist {k => v}").unwrap();
        let hist = rel_id(&db, "hist");

        assert_invalidates(&db, hist, "::history_gc hist 99999999999999");

        run(&db, "?[k, v] <- [[2, 20]] :put hist {k => v}").unwrap();
        let before = watermark(&db);
        run(&db, "::evict hist [[2]]").unwrap();
        let audit = rel_id(&db, "mnestic_evict_audit");

        assert!(
            !db.graph_projections.is_fresh(hist, before),
            "::evict must dirty the evicted relation"
        );
        assert!(
            !db.graph_projections.is_fresh(audit, before),
            "::evict must also dirty mnestic_evict_audit, which it writes to"
        );
    }

    /// §3.4 row 4: `:reconcile` only buffers rows at statement time; they reach
    /// storage in `stamp_pending_tt_writes` at commit. The hook sits at the
    /// buffering function, so the mark survives to the commit either way.
    #[test]
    fn reconcile_invalidates() {
        let db = new_cozo_mem().unwrap();
        run(&db, ":create belief {k: Int, tt: TxTime => v: Int}").unwrap();
        run(&db, "?[k, v] <- [[1, 10]] :put belief {k => v}").unwrap();
        let belief = rel_id(&db, "belief");

        assert_invalidates(
            &db,
            belief,
            "?[k, v] <- [[1, 11]] :reconcile belief {k => v}",
        );
    }

    /// §3.4 row 7: `import_relations` bypasses the row-level mutation paths
    /// entirely, writing through `store_tx.put`. Both the plain-put mode and
    /// the `-`-prefixed delete mode are marked at relation resolution.
    #[test]
    fn import_relations_invalidates_in_both_directions() {
        let db = plain_db();
        let r = rel_id(&db, "r");

        let rows = NamedRows::new(
            vec!["a".to_string(), "b".to_string()],
            vec![vec![DataValue::from(1), DataValue::from(1)]],
        );
        let before = watermark(&db);
        db.import_relations([("r".to_string(), rows.clone())].into())
            .unwrap();
        assert!(!db.graph_projections.is_fresh(r, before));

        let before = watermark(&db);
        db.import_relations([("-r".to_string(), rows)].into())
            .unwrap();
        assert!(
            !db.graph_projections.is_fresh(r, before),
            "the '-'-prefixed delete mode is the same hook"
        );
    }

    /// §3.4 row 7, TxTime branch: rows are buffered into `pending_tt_writes`
    /// and stamped at commit, never passing through `put_into_relation`.
    #[test]
    fn import_relations_invalidates_a_txtime_relation() {
        let db = new_cozo_mem().unwrap();
        run(&db, ":create hist {k: Int, tt: TxTime => v: Int}").unwrap();
        let hist = rel_id(&db, "hist");

        let rows = NamedRows::new(
            vec!["k".to_string(), "v".to_string()],
            vec![vec![DataValue::from(1), DataValue::from(10)]],
        );
        let before = watermark(&db);
        db.import_relations([("hist".to_string(), rows)].into())
            .unwrap();

        assert!(!db.graph_projections.is_fresh(hist, before));
    }

    /// Explicitly non-invalidating (§3.4). `::rename` moves a catalog row and
    /// nothing else — the consume rule's slot-id binding, not a hook, is what
    /// defeats a swap-rename. `:ensure` asserts. `::compact` and index creation
    /// do not change any relation's contents.
    #[test]
    fn non_mutating_paths_preserve_freshness() {
        let db = plain_db();
        run(&db, "?[a, b] <- [[1, 1]] :put r {a => b}").unwrap();
        let r = rel_id(&db, "r");

        assert_preserves(&db, r, "?[a, b] <- [[1, 1]] :ensure r {a => b}");
        assert_preserves(&db, r, "?[a, b] <- [[9, 9]] :ensure_not r {a => b}");
        assert_preserves(&db, r, "::compact");
        assert_preserves(&db, r, "::index create r:idx {b}");
        assert_preserves(&db, r, "?[a] := *r[a, _]");
        // The rename is a catalog write; `r`'s content — and its id — are
        // untouched, so a projection over an *unrelated* renamed relation
        // keeps hitting.
        run(&db, ":create other {x: Int}").unwrap();
        assert_preserves(&db, r, "::rename other -> other2");
    }

    /// Temp relations get their ids from a per-transaction counter, so they
    /// collide numerically with persistent ids. Dirtying a temp id would
    /// invalidate an unrelated persistent relation that happens to share the
    /// number — here, `r` itself.
    #[test]
    fn temp_relation_writes_never_dirty_the_persistent_id_they_collide_with() {
        let db = plain_db();
        let r = rel_id(&db, "r");
        let before = watermark(&db);
        let token_before = db.graph_projections.rel_state(r).token;

        // `_t` is the first relation created in its transaction, so its id
        // comes off the per-transaction `temp_store_id` counter as 1 — the same
        // number `r` carries in the persistent catalog.
        assert_eq!(r, RelationId(1), "the fixture must reproduce the collision");
        run(
            &db,
            "{:create _t {a: Int}}
             {?[a] <- [[1]] :put _t {a}}
             {?[a] <- [[1]] :rm _t {a}}
             {?[a] <- [[2]] :replace _t {a: Int}}",
        )
        .unwrap();

        assert!(
            db.graph_projections.is_fresh(r, before),
            "temp-relation put/rm/replace must not touch persistent relation ids"
        );
        assert_eq!(
            db.graph_projections.rel_state(r).token,
            token_before,
            "and must not advance the colliding id's token"
        );
    }

    /// A transaction that mutated relations and then failed bumps anyway: on
    /// `mem`, `del_range_from_persisted` has already written through, and in
    /// general an abort is not proof that nothing changed.
    #[test]
    fn a_dropped_transaction_bumps_conservatively() {
        let db = plain_db();
        let r = rel_id(&db, "r");
        let before = watermark(&db);

        // One script, one transaction: the put lands in the transaction cache,
        // the second statement fails, the transaction is dropped uncommitted.
        run(
            &db,
            "{?[a, b] <- [[1, 1]] :put r {a => b}}
             {?[x] <- [[1]] :put no_such_relation {x}}",
        )
        .unwrap_err();

        assert_eq!(
            run(&db, "?[a] := *r[a, _]").unwrap().rows.len(),
            0,
            "the write was rolled back"
        );
        assert!(
            !db.graph_projections.is_fresh(r, before),
            "but the token was bumped anyway"
        );
    }

    /// The watermark is captured when the transaction opens, not when a lookup
    /// happens. A transaction that pinned before a write must carry a watermark
    /// that leaves the written relation stale for its whole life — otherwise a
    /// long-lived reader would be handed an entry built from data its own
    /// snapshot cannot see (the direction the draft protocol got wrong, §6.2).
    ///
    /// The ordering *within* `Db::transact` — capture strictly before the
    /// storage transaction pins — is not observable from here; `mem` pins by
    /// taking a read guard, so a concurrent writer cannot even run. Phase 4
    /// carries the interleaved version on rocksdb.
    #[test]
    fn a_transaction_carries_the_watermark_it_opened_with() {
        let db = plain_db();
        let r = rel_id(&db, "r");

        let opened_at = watermark(&db);
        let tx = db.transact().unwrap();
        assert_eq!(tx.watermark, opened_at);
        drop(tx); // `mem` holds a read guard for the tx's life

        run(&db, "?[a, b] <- [[1, 1]] :put r {a => b}").unwrap();

        assert!(
            !db.graph_projections.is_fresh(r, opened_at),
            "the write is invisible to that snapshot, so no entry over r may serve it"
        );
        assert!(db.graph_projections.is_fresh(r, watermark(&db)));
    }

    /// §3.4 row 8: `import_from_backup` writes rows straight through
    /// `store_tx.put` on the destination, never touching a mutation path.
    #[cfg(feature = "storage-sqlite")]
    #[test]
    fn import_from_backup_invalidates_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let backup = dir.path().join("backup.db");

        let src = plain_db();
        run(&src, "?[a, b] <- [[1, 1]] :put r {a => b}").unwrap();
        src.backup_db(&backup).unwrap();

        let dst = plain_db();
        let r = rel_id(&dst, "r");
        let before = watermark(&dst);

        dst.import_from_backup(&backup, &["r".to_string()]).unwrap();

        assert!(!dst.graph_projections.is_fresh(r, before));
        assert_eq!(
            run(&dst, "?[a] := *r[a, _]").unwrap().rows.len(),
            1,
            "the import really did write"
        );
    }

    /// §3.4 row 13: `restore_backup` replaces the whole keyspace through
    /// `batch_put`, outside any `SessionTx`, so no dirty set can see it. Its
    /// empty-store precondition means no projection can exist in practice — but
    /// the whole-cache invalidation runs at function entry regardless, before
    /// the precondition that would reject this call.
    #[cfg(feature = "storage-sqlite")]
    #[test]
    fn restore_backup_invalidates_before_it_checks_its_precondition() {
        let dir = tempfile::tempdir().unwrap();
        let backup = dir.path().join("backup.db");
        plain_db().backup_db(&backup).unwrap();

        let db = plain_db();
        let r = rel_id(&db, "r");
        let before = watermark(&db);

        db.restore_backup(&backup)
            .expect_err("the store is not empty");

        assert!(
            !db.graph_projections.is_fresh(r, before),
            "the invalidation must precede the precondition check"
        );
    }

    // ---- The commit fence ----

    /// Spec §5, interleaving test (a). A writer parked between its storage
    /// commit and its token bump is invisible to `token`, so only `inflight`
    /// can deny a concurrent reader the stale hit. Released, the token denies
    /// it instead. This is the window the whole `inflight` counter exists for.
    #[test]
    fn a_writer_parked_at_the_commit_fence_denies_hits_via_inflight() {
        let db = Arc::new(plain_db());
        let r = rel_id(&db, "r");
        let reader_watermark = watermark(&db);
        let token_before = db.graph_projections.rel_state(r).token;

        let (parked_tx, parked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::sync_channel::<()>(0);
        let release_rx = std::sync::Mutex::new(release_rx);
        db.graph_projections.set_commit_fence(Some(Arc::new(
            move |dirty: &BTreeSet<RelationId>| {
                parked_tx.send(dirty.clone()).unwrap();
                let _ = release_rx.lock().unwrap().recv();
            },
        )));

        let writer = {
            let db = db.clone();
            std::thread::spawn(move || {
                run(&db, "?[a, b] <- [[1, 1]] :put r {a => b}").unwrap();
            })
        };

        let dirty = parked_rx.recv().unwrap();
        assert_eq!(dirty, ids(&[r.0]), "the fence sees the writer's dirty set");
        assert_eq!(
            db.graph_projections.rel_state(r),
            RelState {
                token: token_before,
                inflight: 1
            },
            "committed to storage, not yet bumped: only inflight guards the window"
        );
        assert!(!db.graph_projections.is_fresh(r, u64::MAX));

        drop(release_tx);
        writer.join().unwrap();

        assert_eq!(db.graph_projections.rel_state(r).inflight, 0);
        assert!(
            !db.graph_projections.is_fresh(r, reader_watermark),
            "once released, the token denies the stale reader"
        );
    }

    /// A panic unwinding through the commit — modelled here by a panicking
    /// fence, which sits inside exactly the same window — must not strand
    /// `inflight` above zero. A stranded counter would deny that relation
    /// every future cache hit for the life of the process.
    #[test]
    fn a_panic_inside_the_commit_window_does_not_strand_inflight() {
        let db = plain_db();
        let r = rel_id(&db, "r");

        db.graph_projections
            .set_commit_fence(Some(Arc::new(|_: &BTreeSet<RelationId>| {
                panic!("commit blew up");
            })));
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = run(&db, "?[a, b] <- [[1, 1]] :put r {a => b}");
        }));
        assert!(panicked.is_err());
        db.graph_projections.set_commit_fence(None);

        assert_eq!(
            db.graph_projections.rel_state(r).inflight,
            0,
            "SessionTx::drop balanced the begin_commit increment"
        );
        assert!(db.graph_projections.is_fresh(r, watermark(&db)));
    }
}
