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

#[cfg(feature = "graph-algo")]
use std::sync::atomic::AtomicUsize;
#[cfg(feature = "graph-algo")]
use std::sync::{Arc, PoisonError};

#[cfg(feature = "graph-algo")]
use graph::prelude::{DirectedCsrGraph, Graph};
#[cfg(feature = "graph-algo")]
use miette::{bail, ensure, Diagnostic, Result};
#[cfg(feature = "graph-algo")]
use smartstring::{LazyCompact, SmartString};
#[cfg(feature = "graph-algo")]
use thiserror::Error;

#[cfg(feature = "graph-algo")]
use crate::data::tuple::TupleIter;
#[cfg(feature = "graph-algo")]
use crate::data::value::{DataValue, Vector};
#[cfg(feature = "graph-algo")]
use crate::fixed_rule::{build_unweighted_csr, build_weighted_csr};
#[cfg(feature = "graph-algo")]
use crate::parse::SourceSpan;
#[cfg(feature = "graph-algo")]
use crate::runtime::db::{seconds_since_the_epoch, Poison};
#[cfg(feature = "graph-algo")]
use crate::runtime::relation::RelationHandle;
#[cfg(feature = "graph-algo")]
use crate::runtime::transact::SessionTx;

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
    /// Named projection definitions and their materialised CSR variants
    /// (§3.1, §3.2). Lock order is **registry before `rel_states`**, never the
    /// reverse; the invalidation paths take them one at a time.
    #[cfg(feature = "graph-algo")]
    registry: Mutex<Registry>,
    /// Per-`(projection, generation, variant)` single-flight slots (§3.2.8).
    /// Held across a build, so it is acquired *before* the registry and the
    /// storage snapshot, never after.
    #[cfg(feature = "graph-algo")]
    build_slots: Mutex<BTreeMap<BuildKey, Arc<Mutex<()>>>>,
    /// Number of cached variants, mirrored out of the registry so that the
    /// commit path can skip taking the registry lock when nothing is cached.
    /// This is the only cost the substrate imposes on a projection-less
    /// database beyond Phase 1's. `produce` raises it *conservatively before*
    /// reading the freshness tokens — see the ordering argument there.
    #[cfg(feature = "graph-algo")]
    entry_count: AtomicUsize,
    /// Mirror of `Registry::capacity == 0`, readable without the registry
    /// lock, so a disabled cache can skip the single-flight slot entirely and
    /// concurrent lookups build in parallel — exactly today's behaviour.
    /// `false` by default, matching the nonzero default capacity. Staleness is
    /// harmless: building outside the slot is always correct, it only loses
    /// coalescing for one lookup.
    #[cfg(feature = "graph-algo")]
    capacity_is_zero: std::sync::atomic::AtomicBool,
    /// Completed CSR builds, cached and ephemeral alike. Tests assert on it to
    /// prove that single-flight coalesces and that a hit does not rebuild.
    #[cfg(feature = "graph-algo")]
    build_count: AtomicU64,
    /// Cumulative count of build-slot acquisitions, for tests that must see
    /// whether a lookup went through single-flight at all — the slot map
    /// self-cleans, so its final state cannot witness that.
    #[cfg(test)]
    slots_acquired: AtomicU64,
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
        {
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
        // Outside the `rel_states` guard: the registry lock is only ever taken
        // *before* it, so taking it here — with nothing else held — cannot
        // invert the order. The gap is harmless. A consumer that slips in
        // between re-resolves each source name against its own snapshot: a
        // destroyed relation no longer resolves, a `:replace`d one resolves to
        // a fresh id that no entry is bound to, and a merely-written one now
        // carries a token the entry cannot match. All three miss.
        self.drop_entries_sourced_on(dirty);
    }

    /// A transaction with a non-empty dirty set went away without committing.
    /// Bump its relations' tokens — there is no `inflight` to lower, since it
    /// was never raised — so that any entry tagged against them misses.
    ///
    /// Conservative by design: `mem`'s out-of-transaction range deletes mean an
    /// abort is not proof that nothing changed.
    pub(crate) fn bump_aborted(&self, dirty: &BTreeSet<RelationId>) {
        {
            let mut states = self.states();
            for rel in dirty {
                let token = self.fresh_token();
                states.entry(*rel).or_default().token = token;
            }
        }
        self.drop_entries_sourced_on(dirty);
    }

    /// Invalidate everything this process knows about. Used by
    /// `Db::restore_backup`, which replaces the whole keyspace through
    /// `batch_put` — outside any `SessionTx`, so no dirty set can see it.
    ///
    /// Bumps rather than clears: clearing would make every relation read as
    /// `{token: 0, inflight: 0}`, i.e. *fresh*, which is the wrong direction.
    ///
    /// The cached variants go too. The definitions stay: a projection is a
    /// name, and `restore_backup` may hand back the very relations it names.
    pub(crate) fn invalidate_all(&self) {
        {
            let mut states = self.states();
            for st in states.values_mut() {
                st.token = self.bump_seq.fetch_add(1, Ordering::SeqCst) + 1;
            }
        }
        self.clear_entries();
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

    /// Raise a relation's token *without* freeing the entries built over it —
    /// simulating a future mutation path that bumps but forgets to invalidate
    /// the cache. No production path does this, and none should; the consume
    /// rule's token comparison is the net that catches it if one ever does.
    #[cfg(test)]
    pub(crate) fn bump_token_leaving_entries(&self, rel: RelationId) {
        let token = self.fresh_token();
        self.states().entry(rel).or_default().token = token;
    }

    /// Without `graph-algo` there are no cached variants to free, so the two
    /// invalidation hooks compile away entirely. The substrate above stays
    /// unconditional: cfg-gating thirteen mutation-path hooks is where a
    /// missed one would hide.
    #[cfg(not(feature = "graph-algo"))]
    fn drop_entries_sourced_on(&self, _dirty: &BTreeSet<RelationId>) {}

    #[cfg(not(feature = "graph-algo"))]
    fn clear_entries(&self) {}
}

// ===========================================================================
// Phase 2: the cache itself — registry, variants, single-flight, memory.
// Everything below is `graph-algo`-gated: it owns the `graph` crate's CSR
// types, which a `minimal` build does not compile.
// ===========================================================================

/// Default ceiling on the total size of all cached CSR variants (§3.6, signed).
/// `0` disables caching without disabling `::graph create`/`list`/`drop`.
#[cfg(feature = "graph-algo")]
pub(crate) const DEFAULT_PROJECTION_CAPACITY: usize = 512 * 1024 * 1024;

/// Charged per `BTreeMap`/`BTreeSet` entry when estimating an id map's size.
/// B-trees allocate in nodes, not per entry; this is the amortised share.
#[cfg(feature = "graph-algo")]
const BTREE_ENTRY_OVERHEAD: usize = 48;

/// Flat charge for a vertex key whose heap footprint we do not walk. Such keys
/// are pathological — a JSON blob or a compiled regex as a graph vertex — and
/// the ceiling only needs to be the right order of magnitude.
#[cfg(feature = "graph-algo")]
const OPAQUE_KEY_ESTIMATE: usize = 64;

/// Which concrete CSR a projection has materialised. A projection names its
/// sources; the variants are built lazily, one per `(direction, weighted)`
/// pair actually asked for (§3.2).
#[cfg(feature = "graph-algo")]
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct VariantKey {
    /// Each stored edge is fed to the builder in both directions.
    pub undirected: bool,
    /// Column 3 of the edge relation carries the weight, defaulting to `1.0`.
    pub weighted: bool,
}

#[cfg(feature = "graph-algo")]
impl VariantKey {
    /// The label `::graph list` prints for this variant.
    pub(crate) fn label(&self) -> &'static str {
        match (self.undirected, self.weighted) {
            (false, false) => "directed",
            (false, true) => "directed+weighted",
            (true, false) => "undirected",
            (true, true) => "undirected+weighted",
        }
    }

}

/// A built adjacency, weighted or not.
#[cfg(feature = "graph-algo")]
pub enum GraphVariant {
    /// Unweighted CSR, both directions.
    Unweighted(DirectedCsrGraph<u32>),
    /// Weighted CSR. Built permissively (`allow_negative_weights = true`) so
    /// that one variant serves both strict and permissive consumers;
    /// `has_negative` records what the scan saw, and a strict consumer —
    /// Dijkstra, Yen, Louvain, Betweenness, Closeness — rejects it loudly.
    Weighted {
        /// The adjacency.
        graph: DirectedCsrGraph<u32, (), f32>,
        /// Whether any edge weight in the source was negative.
        has_negative: bool,
    },
}

/// One materialised variant of one projection, plus the vertex-id mapping that
/// makes its dense `u32` ids meaningful. Handed to algorithms as
/// [`GraphSource`] whether it came from the cache or was just built.
///
/// **Emptiness is `indices().is_empty()`, never `node_count() == 0`.** The
/// vendored crate cannot express a zero-vertex CSR (`Csr::new` is private), so
/// an empty edge relation with no `nodes` yields a phantom one-vertex graph
/// whose single id names nothing.
#[cfg(feature = "graph-algo")]
pub struct ProjectionVariant {
    graph: GraphVariant,
    indices: Vec<DataValue>,
    inv_indices: BTreeMap<DataValue, u32>,
    est_bytes: usize,
}

#[cfg(feature = "graph-algo")]
impl ProjectionVariant {
    fn new(graph: GraphVariant, indices: Vec<DataValue>, inv_indices: BTreeMap<DataValue, u32>) -> Self {
        // `Target<u32, ()>` is a bare `u32`; `Target<u32, f32>` carries the
        // weight alongside it.
        let (nodes, edges, target_bytes) = match &graph {
            GraphVariant::Unweighted(g) => (g.node_count(), g.edge_count(), 4),
            GraphVariant::Weighted { graph: g, .. } => (g.node_count(), g.edge_count(), 8),
        };
        let est_bytes = estimate_bytes(
            nodes as usize,
            edges as usize,
            target_bytes,
            &indices,
            &inv_indices,
        );
        Self {
            graph,
            indices,
            inv_indices,
            est_bytes,
        }
    }

    /// The adjacency.
    pub fn graph(&self) -> &GraphVariant {
        &self.graph
    }
    /// Vertex values, indexed by the dense `u32` id the CSR uses.
    pub fn indices(&self) -> &[DataValue] {
        &self.indices
    }
    /// The inverse of [`indices`](Self::indices).
    pub fn inv_indices(&self) -> &BTreeMap<DataValue, u32> {
        &self.inv_indices
    }
    /// Estimated resident size, in bytes. See [`estimate_bytes`].
    pub(crate) fn est_bytes(&self) -> usize {
        self.est_bytes
    }
}

#[cfg(feature = "graph-algo")]
impl std::fmt::Debug for ProjectionVariant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (nodes, edges, weighted) = match &self.graph {
            GraphVariant::Unweighted(g) => (g.node_count(), g.edge_count(), false),
            GraphVariant::Weighted { graph, .. } => (graph.node_count(), graph.edge_count(), true),
        };
        f.debug_struct("ProjectionVariant")
            .field("nodes", &nodes)
            .field("edges", &edges)
            .field("vertices_interned", &self.indices.len())
            .field("weighted", &weighted)
            .field("est_bytes", &self.est_bytes)
            .finish()
    }
}

/// What an algorithm consumes. One shape for cache hits and for the ephemeral
/// builds that a bypass, a produce-rule refusal, or a full cache forces.
#[cfg(feature = "graph-algo")]
pub type GraphSource = Arc<ProjectionVariant>;

/// A projection's definition: the *names* of its sources, plus every variant
/// built from them so far. Names, not ids: every use re-resolves them against
/// the consuming transaction's own catalog (§3.1).
#[cfg(feature = "graph-algo")]
struct Definition {
    /// Distinguishes a re-created projection from the one it replaced, so a
    /// build begun against the old definition cannot be inserted into the new,
    /// and a lookup holding the old one cannot read the new one's entries.
    ///
    /// This guards the *definition boundary*, not correctness: today a
    /// `Definition` is fully described by its source names, so the slot-id and
    /// token checks alone already make a wrong answer impossible — an entry
    /// filed under a re-created name either binds the ids the reader resolved,
    /// in which case its content is right, or it does not, in which case the
    /// reader misses. What the generation buys is that this stays true if a
    /// `Definition` ever grows a field the build depends on (an edge filter, a
    /// weight column, a direction default), and that a build made from one
    /// definition's sources never leaks bytes into another's variant map.
    generation: u64,
    edges: SmartString<LazyCompact>,
    nodes: Option<SmartString<LazyCompact>>,
    variants: BTreeMap<VariantKey, Entry>,
}

/// A cached variant, bound to the exact relation ids and content versions it
/// was built from.
#[cfg(feature = "graph-algo")]
struct Entry {
    source: GraphSource,
    /// The id the `edges` name resolved to at build time. A consumer whose own
    /// resolution of that name yields a different id must miss: this is what
    /// defeats a multi-pair `::rename` swap, which moves no tokens at all.
    edges_id: RelationId,
    nodes_id: Option<RelationId>,
    /// `token(edges_id)` at insert time — the content version this CSR holds.
    edges_token: u64,
    /// `token(nodes_id)`, or `0` when the projection has no `nodes` slot.
    nodes_token: u64,
    est_bytes: usize,
    built_at: f64,
    last_used_at: f64,
    /// Monotone within the registry; the smallest is the LRU victim.
    lru_seq: u64,
}

#[cfg(feature = "graph-algo")]
struct Registry {
    defs: BTreeMap<SmartString<LazyCompact>, Definition>,
    capacity: usize,
    used_bytes: usize,
    lru_seq: u64,
    def_seq: u64,
}

#[cfg(feature = "graph-algo")]
impl Default for Registry {
    fn default() -> Self {
        Self {
            defs: Default::default(),
            capacity: DEFAULT_PROJECTION_CAPACITY,
            used_bytes: 0,
            lru_seq: 0,
            def_seq: 0,
        }
    }
}

#[cfg(feature = "graph-algo")]
impl Registry {
    fn entry_total(&self) -> usize {
        self.defs.values().map(|d| d.variants.len()).sum()
    }

    fn lru_victim(&self) -> Option<(SmartString<LazyCompact>, VariantKey)> {
        self.defs
            .iter()
            .flat_map(|(name, def)| def.variants.iter().map(move |(k, e)| (name, *k, e.lru_seq)))
            .min_by_key(|(_, _, seq)| *seq)
            .map(|(name, k, _)| (name.clone(), k))
    }

    /// Evict least-recently-used variants until the cache fits in `target`.
    fn evict_until(&mut self, target: usize) {
        while self.used_bytes > target {
            let Some((name, key)) = self.lru_victim() else {
                // No entries left: `used_bytes` has drifted. Re-derive rather
                // than spin.
                self.used_bytes = 0;
                break;
            };
            let Some(entry) = self.defs.get_mut(&name).and_then(|d| d.variants.remove(&key)) else {
                break;
            };
            self.used_bytes = self.used_bytes.saturating_sub(entry.est_bytes);
        }
    }
}

/// The single-flight key. Includes the generation, so a `::graph drop` +
/// re-create never shares a build slot with the projection it replaced.
#[cfg(feature = "graph-algo")]
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct BuildKey {
    name: SmartString<LazyCompact>,
    generation: u64,
    variant: VariantKey,
}

/// One row of `::graph list` (§3.1): one per built variant, or a single row
/// with null variant columns for a projection that has built nothing yet.
#[cfg(feature = "graph-algo")]
pub(crate) struct ProjectionStat {
    pub(crate) name: String,
    pub(crate) edges: String,
    pub(crate) nodes: Option<String>,
    pub(crate) variant: Option<&'static str>,
    pub(crate) est_bytes: Option<usize>,
    pub(crate) built_at: Option<f64>,
    pub(crate) last_used: Option<f64>,
}

#[cfg(feature = "graph-algo")]
#[derive(Debug, Error, Diagnostic)]
#[error("graph projection '{0}' not found")]
#[diagnostic(code(graph::projection_not_found))]
#[diagnostic(help(
    "projections are in-memory and must be re-created after restart, and after `::graph drop`"
))]
struct ProjectionNotFoundError(String);

#[cfg(feature = "graph-algo")]
#[derive(Debug, Error, Diagnostic)]
#[error("graph projection '{0}' already exists")]
#[diagnostic(code(graph::projection_exists))]
#[diagnostic(help("drop it first with `::graph drop {0}`"))]
struct ProjectionExistsError(String);

#[cfg(feature = "graph-algo")]
#[derive(Debug, Error, Diagnostic)]
#[error("cannot project from index relation '{0}'")]
#[diagnostic(code(graph::projection_index_source))]
#[diagnostic(help("use its base relation instead"))]
struct ProjectionIndexSourceError(String);

#[cfg(feature = "graph-algo")]
#[derive(Debug, Error, Diagnostic)]
#[error("cannot project from temporary relation '{0}'")]
#[diagnostic(code(graph::projection_temp_source))]
#[diagnostic(help(
    "temporary relations live for one transaction; a projection outlives every transaction"
))]
struct ProjectionTempSourceError(String);

#[cfg(feature = "graph-algo")]
#[derive(Debug, Error, Diagnostic)]
#[error("cannot project from transaction-time relation '{0}'")]
#[diagnostic(code(graph::projection_txtime_source))]
#[diagnostic(help(
    "a tt-stamped relation's default read is its current belief, but a projection would scan \
     the raw history keyspace, retracted rows included. Project from a relation without a \
     TxTime column; current-belief projections of tt relations may come later"
))]
struct ProjectionTxTimeSourceError(String);

#[cfg(feature = "graph-algo")]
#[derive(Debug, Error, Diagnostic)]
#[error("edge relation '{0}' of a graph projection has arity {1}, needs at least 2")]
#[diagnostic(code(graph::projection_bad_edge_arity))]
struct ProjectionEdgeArityError(String, usize);

#[cfg(feature = "graph-algo")]
impl ProjectionCache {
    fn registry(&self) -> std::sync::MutexGuard<'_, Registry> {
        // Same poison policy as `rel_states`: a panic under this lock can
        // corrupt the byte accounting, never the freshness tokens that decide
        // whether an entry may be served. Refusing to open it would take down
        // every later transaction over a memory-accounting slip.
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn publish_count(&self, reg: &Registry) {
        self.entry_count.store(reg.entry_total(), Ordering::Relaxed);
    }

    /// Free every cached variant sourced on a relation this transaction
    /// changed. Tokens only rise and relation ids are never reused within a
    /// process, so an entry whose source has been bumped can never satisfy the
    /// consume rule's token equality again: it is *permanently* unreachable,
    /// and keeping it would be a leak that `::graph list` would report as a
    /// live cache.
    ///
    /// Two consequences worth stating. The consume rule's token comparison is
    /// thereby defence in depth rather than the working invalidation mechanism
    /// — it is the net under this hook, and under any future mutation path that
    /// bumps a token and forgets to call here. And a transaction that pinned
    /// *after* our bump may insert a perfectly fresh entry in the window
    /// between the token bump and this call, only to have it dropped here; the
    /// cost is one rebuild on the next lookup, which is the same degraded mode
    /// write churn produces anyway.
    fn drop_entries_sourced_on(&self, dirty: &BTreeSet<RelationId>) {
        if dirty.is_empty() || self.entry_count.load(Ordering::Relaxed) == 0 {
            return;
        }
        let mut reg = self.registry();
        let mut freed = 0usize;
        for def in reg.defs.values_mut() {
            def.variants.retain(|_, entry| {
                let dead = dirty.contains(&entry.edges_id)
                    || entry.nodes_id.is_some_and(|n| dirty.contains(&n));
                if dead {
                    freed += entry.est_bytes;
                }
                !dead
            });
        }
        reg.used_bytes = reg.used_bytes.saturating_sub(freed);
        self.publish_count(&reg);
    }

    /// Drop every cached variant, keeping the definitions.
    fn clear_entries(&self) {
        if self.entry_count.load(Ordering::Relaxed) == 0 {
            return;
        }
        let mut reg = self.registry();
        for def in reg.defs.values_mut() {
            def.variants.clear();
        }
        reg.used_bytes = 0;
        self.publish_count(&reg);
    }

    /// The byte ceiling. `0` evicts everything and makes every insert a no-op
    /// while leaving `::graph create`/`list`/`drop` fully working.
    pub(crate) fn set_capacity(&self, bytes: usize) {
        let mut reg = self.registry();
        reg.capacity = bytes;
        self.capacity_is_zero
            .store(bytes == 0, std::sync::atomic::Ordering::Relaxed);
        reg.evict_until(bytes);
        self.publish_count(&reg);
    }

    #[allow(dead_code)] // read by tests and, in Phase 3, by `::graph list`.
    pub(crate) fn bytes_used(&self) -> usize {
        self.registry().used_bytes
    }

    #[allow(dead_code)] // tests assert single-flight coalescing on this.
    pub(crate) fn build_count(&self) -> u64 {
        self.build_count.load(Ordering::Relaxed)
    }

    /// The `(generation, edges, nodes)` of a registered projection.
    fn definition(
        &self,
        name: &str,
    ) -> Result<(
        u64,
        SmartString<LazyCompact>,
        Option<SmartString<LazyCompact>>,
    )> {
        let reg = self.registry();
        match reg.defs.get(name) {
            None => bail!(ProjectionNotFoundError(name.to_string())),
            Some(def) => Ok((def.generation, def.edges.clone(), def.nodes.clone())),
        }
    }

    /// Register a projection. Builds nothing: variants materialise on first use.
    pub(crate) fn create(
        &self,
        name: &str,
        edges: SmartString<LazyCompact>,
        nodes: Option<SmartString<LazyCompact>>,
    ) -> Result<()> {
        let mut reg = self.registry();
        if reg.defs.contains_key(name) {
            bail!(ProjectionExistsError(name.to_string()));
        }
        reg.def_seq += 1;
        let generation = reg.def_seq;
        reg.defs.insert(
            name.into(),
            Definition {
                generation,
                edges,
                nodes,
                variants: Default::default(),
            },
        );
        Ok(())
    }

    /// Forget a projection and free every variant it built.
    pub(crate) fn drop_projection(&self, name: &str) -> Result<()> {
        let mut reg = self.registry();
        let Some(def) = reg.defs.remove(name) else {
            bail!(ProjectionNotFoundError(name.to_string()));
        };
        let freed: usize = def.variants.values().map(|e| e.est_bytes).sum();
        reg.used_bytes = reg.used_bytes.saturating_sub(freed);
        self.publish_count(&reg);
        Ok(())
    }

    /// One row per built variant; one null-variant row for a cold projection.
    pub(crate) fn list(&self) -> Vec<ProjectionStat> {
        let reg = self.registry();
        let mut out = vec![];
        for (name, def) in reg.defs.iter() {
            if def.variants.is_empty() {
                out.push(ProjectionStat {
                    name: name.to_string(),
                    edges: def.edges.to_string(),
                    nodes: def.nodes.as_ref().map(|n| n.to_string()),
                    variant: None,
                    est_bytes: None,
                    built_at: None,
                    last_used: None,
                });
                continue;
            }
            for (key, entry) in def.variants.iter() {
                out.push(ProjectionStat {
                    name: name.to_string(),
                    edges: def.edges.to_string(),
                    nodes: def.nodes.as_ref().map(|n| n.to_string()),
                    variant: Some(key.label()),
                    est_bytes: Some(entry.est_bytes),
                    built_at: Some(entry.built_at),
                    last_used: Some(entry.last_used_at),
                });
            }
        }
        out
    }

    /// The **consume rule** (§3.3). Returns the cached variant iff, at one
    /// instant: the definition is the same one the caller resolved against,
    /// every slot's freshly-resolved id equals the id the entry is bound to,
    /// every such id still carries the token the entry was tagged with, and
    /// `FRESH(T, ·)` holds for each.
    ///
    /// Note what is *not* checked: whether some newer entry exists. It cannot,
    /// unmatched — a newer entry implies a bumped token, which fails the token
    /// equality below.
    fn consume(
        &self,
        name: &str,
        generation: u64,
        key: VariantKey,
        edges_id: RelationId,
        nodes_id: Option<RelationId>,
        watermark: u64,
    ) -> Option<GraphSource> {
        let mut reg = self.registry();
        let (source, edges_token, nodes_token) = {
            let def = reg.defs.get(name)?;
            if def.generation != generation {
                return None;
            }
            let entry = def.variants.get(&key)?;
            if entry.edges_id != edges_id || entry.nodes_id != nodes_id {
                return None;
            }
            (entry.source.clone(), entry.edges_token, entry.nodes_token)
        };

        // Registry held, `rel_states` taken inside it: the declared lock order.
        {
            let states = self.states();
            if !fresh_with_token(&states, edges_id, edges_token, watermark) {
                return None;
            }
            if let Some(n) = nodes_id {
                if !fresh_with_token(&states, n, nodes_token, watermark) {
                    return None;
                }
            }
        }

        reg.lru_seq += 1;
        let seq = reg.lru_seq;
        let now = seconds_since_the_epoch().unwrap_or(0.0);
        if let Some(entry) = reg.defs.get_mut(name).and_then(|d| d.variants.get_mut(&key)) {
            entry.lru_seq = seq;
            entry.last_used_at = now;
        }
        Some(source)
    }

    /// The **produce rule** (§3.3). Publishes a freshly built variant iff the
    /// definition still stands and `FRESH(T, ·)` holds for every source *at
    /// insert time* — tagging the entry with the tokens read at that same
    /// instant. A refusal is not an error: the caller uses its build
    /// ephemerally and the cache stays cold until a fresher transaction fills
    /// it, which in practice is the next query.
    #[allow(clippy::too_many_arguments)]
    fn produce(
        &self,
        name: &str,
        generation: u64,
        key: VariantKey,
        edges_id: RelationId,
        nodes_id: Option<RelationId>,
        watermark: u64,
        source: &GraphSource,
    ) {
        let est_bytes = source.est_bytes();
        let mut reg = self.registry();
        match reg.defs.get(name) {
            Some(def) if def.generation == generation => {}
            // Dropped, or dropped and re-created, while we were building.
            _ => return,
        }

        // Raise the commit path's fast-check mirror BEFORE reading the tokens,
        // and let `publish_count` restore it to exact on every exit below.
        // This closes the one hole in `drop_entries_sourced_on`'s lock-free
        // fast path: an insert-with-old-tokens requires this thread's
        // `rel_states` read (below) to precede the writer's bump, and the
        // writer's `entry_count` load follows its bump — so this store, being
        // sequenced before our `rel_states` acquisition, is chained ahead of
        // the writer's load through the `rel_states` mutex and the writer
        // cannot miss it. The writer then takes the registry lock, waits for
        // us, and sweeps what we inserted. Without this ordering the writer
        // could read a stale zero and strand our just-inserted, permanently
        // unreachable entry as counted-but-dead cache.
        self.entry_count.fetch_add(1, Ordering::Relaxed);

        'rules: {
            let (edges_token, nodes_token) = {
                let states = self.states();
                let Some(et) = token_if_fresh(&states, edges_id, watermark) else {
                    break 'rules;
                };
                let nt = match nodes_id {
                    None => 0,
                    Some(n) => match token_if_fresh(&states, n, watermark) {
                        None => break 'rules,
                        Some(t) => t,
                    },
                };
                (et, nt)
            };

            if est_bytes > reg.capacity {
                // A zero ceiling means caching is off, which is a choice, not
                // a problem to warn about on every query.
                if reg.capacity > 0 {
                    log::warn!(
                        "graph projection '{name}' variant {} needs ~{est_bytes} bytes, over the \
                         whole {} byte cache ceiling; building it fresh for every query. Raise \
                         the ceiling with `Db::set_graph_projection_capacity`.",
                        key.label(),
                        reg.capacity
                    );
                }
                break 'rules;
            }
            self.insert_entry(
                &mut reg,
                name,
                key,
                edges_id,
                nodes_id,
                edges_token,
                nodes_token,
                est_bytes,
                source,
            );
        }
        // On every exit — insert or refusal — restore the mirror to exact.
        self.publish_count(&reg);
    }

    /// The tail of [`produce`](Self::produce): the entry passed every rule.
    #[allow(clippy::too_many_arguments)]
    fn insert_entry(
        &self,
        reg: &mut Registry,
        name: &str,
        key: VariantKey,
        edges_id: RelationId,
        nodes_id: Option<RelationId>,
        edges_token: u64,
        nodes_token: u64,
        est_bytes: usize,
        source: &GraphSource,
    ) {

        // Replace any older entry for this exact variant before making room,
        // so its bytes are not counted twice.
        if let Some(old) = reg.defs.get_mut(name).and_then(|d| d.variants.remove(&key)) {
            reg.used_bytes = reg.used_bytes.saturating_sub(old.est_bytes);
        }
        let target = reg.capacity - est_bytes;
        reg.evict_until(target);

        reg.lru_seq += 1;
        let lru_seq = reg.lru_seq;
        let now = seconds_since_the_epoch().unwrap_or(0.0);
        let Some(def) = reg.defs.get_mut(name) else {
            return;
        };
        def.variants.insert(
            key,
            Entry {
                source: source.clone(),
                edges_id,
                nodes_id,
                edges_token,
                nodes_token,
                est_bytes,
                built_at: now,
                last_used_at: now,
                lru_seq,
            },
        );
        reg.used_bytes += est_bytes;
    }

    fn acquire_slot(&self, key: &BuildKey) -> Arc<Mutex<()>> {
        #[cfg(test)]
        self.slots_acquired.fetch_add(1, Ordering::Relaxed);
        let mut slots = self
            .build_slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        slots.entry(key.clone()).or_default().clone()
    }

    /// Drop the slot once nobody holds a clone of it. A racing `acquire_slot`
    /// keeps the strong count above one, so the slot the racer is about to
    /// block on is never pulled out from under it.
    fn release_slot(&self, key: &BuildKey) {
        let mut slots = self
            .build_slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(slot) = slots.get(key) {
            if Arc::strong_count(slot) == 1 {
                slots.remove(key);
            }
        }
    }
}

/// `FRESH(T, rel) ∧ token(rel) == expected`, under a held `rel_states` guard.
#[cfg(feature = "graph-algo")]
fn fresh_with_token(
    states: &BTreeMap<RelationId, RelState>,
    rel: RelationId,
    expected: u64,
    watermark: u64,
) -> bool {
    let st = states.get(&rel).copied().unwrap_or_default();
    st.inflight == 0 && st.token <= watermark && st.token == expected
}

/// `token(rel)` if `FRESH(T, rel)`, else `None`. Reading the token and the
/// predicate together is what makes an inserted entry's tag mean exactly "the
/// content this transaction's snapshot holds".
#[cfg(feature = "graph-algo")]
fn token_if_fresh(
    states: &BTreeMap<RelationId, RelState>,
    rel: RelationId,
    watermark: u64,
) -> Option<u64> {
    let st = states.get(&rel).copied().unwrap_or_default();
    (st.inflight == 0 && st.token <= watermark).then_some(st.token)
}

/// Estimated resident bytes of a built variant (§3.6). The CSR terms are
/// exact: `DirectedCsrGraph` holds an out- and an in-adjacency, each a
/// `Box<[u32]>` of `V + 1` offsets and a `Box<[Target]>` of `E` targets, and
/// `E` here is the post-doubling, duplicates-kept edge count the builder
/// actually stored. The id-map terms are approximations — for string vertices
/// they can rival the CSR, which is why they are counted at all.
#[cfg(feature = "graph-algo")]
fn estimate_bytes(
    node_count: usize,
    edge_count: usize,
    target_bytes: usize,
    indices: &[DataValue],
    inv_indices: &BTreeMap<DataValue, u32>,
) -> usize {
    let value = std::mem::size_of::<DataValue>() as u64;
    let offsets = 2u64 * (node_count as u64 + 1) * 4;
    let targets = 2u64 * edge_count as u64 * target_bytes as u64;
    let idx = indices.len() as u64 * value
        + indices.iter().map(|v| value_heap_bytes(v) as u64).sum::<u64>();
    let inv = inv_indices.len() as u64
        * (value + std::mem::size_of::<u32>() as u64 + BTREE_ENTRY_OVERHEAD as u64)
        + inv_indices
            .keys()
            .map(|v| value_heap_bytes(v) as u64)
            .sum::<u64>();
    offsets
        .saturating_add(targets)
        .saturating_add(idx)
        .saturating_add(inv)
        .min(usize::MAX as u64) as usize
}

/// Heap bytes owned by a vertex value, beyond its inline `DataValue`.
#[cfg(feature = "graph-algo")]
fn value_heap_bytes(v: &DataValue) -> usize {
    match v {
        DataValue::Str(s) => {
            if s.is_inline() {
                0
            } else {
                s.len()
            }
        }
        DataValue::Bytes(b) => b.len(),
        DataValue::List(l) => {
            l.len() * std::mem::size_of::<DataValue>()
                + l.iter().map(value_heap_bytes).sum::<usize>()
        }
        DataValue::Set(s) => s
            .iter()
            .map(|v| std::mem::size_of::<DataValue>() + BTREE_ENTRY_OVERHEAD + value_heap_bytes(v))
            .sum(),
        DataValue::Vec(Vector::F32(a)) => a.len() * 4,
        DataValue::Vec(Vector::F64(a)) => a.len() * 8,
        DataValue::Json(_) | DataValue::Regex(_) => OPAQUE_KEY_ESTIMATE,
        _ => 0,
    }
}

/// Validate and register `::graph create name { edges, nodes }` (§3.1).
/// Loud and immediate; builds nothing.
#[cfg(feature = "graph-algo")]
pub(crate) fn create_projection(
    tx: &SessionTx<'_>,
    name: &str,
    edges: &str,
    nodes: Option<&str>,
) -> Result<()> {
    let edges_handle = resolve_source(tx, edges)?;
    let arity = edges_handle.metadata.keys.len() + edges_handle.metadata.non_keys.len();
    ensure!(
        arity >= 2,
        ProjectionEdgeArityError(edges.to_string(), arity)
    );
    if let Some(nodes) = nodes {
        resolve_source(tx, nodes)?;
    }
    tx.projections
        .create(name, edges.into(), nodes.map(|n| n.into()))
}

/// Resolve a projection source, rejecting the three relation kinds that can
/// never be one: index relations (their name carries a `:`, and they hold the
/// index keyspace rather than the base tuples); temporary relations (their
/// ids come from a per-transaction counter and collide numerically with
/// persistent ids, and they die with the transaction); and tt-stamped
/// relations, whose engine-wide *default read* is their current belief
/// (`resolve_temporal_read` maps a selector-less read to `AsOf(MAX)` — the
/// migration invariant), while a projection build's raw scan would deliver
/// the whole history keyspace, retraction rows included. Serving that CSR
/// would violate always-fresh in the worst way: not stale, *wrong*, and
/// cached for every subsequent fresh transaction. Plain-`Validity` (vt-only)
/// relations are NOT rejected — for those, a selector-less scan returns every
/// version row on every engine path, so the projection matches the positional
/// form exactly.
///
/// Running at every use, not just at create, this also catches a tt relation
/// `::rename`d into a source name after the projection was defined.
#[cfg(feature = "graph-algo")]
fn resolve_source(tx: &SessionTx<'_>, name: &str) -> Result<RelationHandle> {
    if name.contains(':') {
        bail!(ProjectionIndexSourceError(name.to_string()));
    }
    // `Symbol::is_temp_store_name`, and the same test `get_relation` uses to
    // pick the temp store.
    if name.starts_with('_') {
        bail!(ProjectionTempSourceError(name.to_string()));
    }
    let handle = tx.get_relation(name, false)?;
    // Belt and braces: `is_temp_store_name` is the only way in today, but the
    // id collision this guards against is silent and unrecoverable.
    if handle.is_temp {
        bail!(ProjectionTempSourceError(name.to_string()));
    }
    if handle.has_txtime() {
        bail!(ProjectionTxTimeSourceError(name.to_string()));
    }
    Ok(handle)
}

/// Obtain the `key` variant of projection `name` for this transaction: a cache
/// hit, or a build. The whole freshness protocol lives here.
///
/// Three doors lead to an *ephemeral* build — one this transaction uses and
/// nobody caches:
///
/// 1. **Own uncommitted writes.** A source is in this transaction's dirty set,
///    so its snapshot differs from every other transaction's. (Marking happens
///    at mutation-function entry, i.e. at statement time, so a later statement
///    of the same imperative script sees the mark.)
/// 2. **A source this transaction cannot vouch for.** `FRESH` fails: a commit
///    landed after our watermark, or one is in flight. Note this is permanent
///    for *us* — a commit's token is minted after it finishes, hence after any
///    watermark captured before it, so an unfresh source never becomes fresh
///    again at a fixed watermark. So we do not even queue behind the build
///    slot: nothing we build could be published.
/// 3. **The produce rule refuses at insert time**, or the entry does not fit
///    under the ceiling.
#[cfg(feature = "graph-algo")]
pub(crate) fn graph_source(
    tx: &SessionTx<'_>,
    name: &str,
    key: VariantKey,
    span: SourceSpan,
    poison: &Poison,
) -> Result<GraphSource> {
    let cache = &tx.projections;
    let (generation, edges_name, nodes_name) = cache.definition(name)?;

    // Resolve names against *this* transaction's catalog, every time.
    let edges_handle = resolve_source(tx, &edges_name)?;
    let nodes_handle = match &nodes_name {
        Some(n) => Some(resolve_source(tx, n)?),
        None => None,
    };
    let edges_id = edges_handle.id;
    let nodes_id = nodes_handle.as_ref().map(|h| h.id);

    let build = || {
        let variant = build_variant(tx, &edges_handle, nodes_handle.as_ref(), key, span, poison)?;
        cache.build_count.fetch_add(1, Ordering::Relaxed);
        Ok(Arc::new(variant))
    };

    // Door 1.
    let mut sources = vec![edges_id];
    sources.extend(nodes_id);
    if sources.iter().any(|r| tx.dirty_relations.contains(r)) {
        return build();
    }

    if let Some(hit) = cache.consume(name, generation, key, edges_id, nodes_id, tx.watermark) {
        return Ok(hit);
    }

    // Door 2.
    if !cache.all_fresh(sources.iter().copied(), tx.watermark) {
        return build();
    }

    // A disabled cache never populates, so coalescing behind a slot would
    // only serialize builds that at HEAD ran in parallel. Skip single-flight
    // entirely: with the cache off, every lookup behaves exactly as today.
    if cache
        .capacity_is_zero
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return build();
    }

    // Single-flight: concurrent misses on this variant coalesce here. The
    // winner builds from its own snapshot; the losers wake, re-run the consume
    // rule against *their* watermarks, and take the entry if it is fresh for
    // them too.
    let build_key = BuildKey {
        name: name.into(),
        generation,
        variant: key,
    };
    let slot = cache.acquire_slot(&build_key);
    let outcome = {
        let _guard = slot.lock().unwrap_or_else(PoisonError::into_inner);
        match slot_action(cache, name, generation, key, edges_id, nodes_id, &sources, tx.watermark)
        {
            SlotAction::Hit(hit) => Some(Ok(hit)),
            SlotAction::BuildAndPublish => Some(build().inspect(|source| {
                // Door 3 lives inside `produce`, which simply declines.
                cache.produce(
                    name,
                    generation,
                    key,
                    edges_id,
                    nodes_id,
                    tx.watermark,
                    source,
                );
            })),
            SlotAction::BuildOutsideTheSlot => None,
        }
    };
    drop(slot);
    cache.release_slot(&build_key);
    match outcome {
        Some(result) => result,
        None => build(),
    }
}

/// What a transaction holding the build slot should do, given the cache state
/// it finds on waking. Pure decision, split out so the stale-waiter arm is
/// testable without a real thread interleaving.
#[cfg(feature = "graph-algo")]
enum SlotAction {
    /// A fresh entry appeared while queued — the single-flight payoff.
    Hit(GraphSource),
    /// Still cold, still fresh: build under the slot and offer it to the cache.
    BuildAndPublish,
    /// The sources were bumped while this transaction queued. Freshness never
    /// comes back at a fixed watermark, so its `produce` would be refused —
    /// building inside the slot would only make every queued reader (including
    /// fresh ones that *can* publish) wait out a build that publishes nothing.
    /// Same principle as Door 2, re-checked after the wait it could not
    /// foresee. Found by adversarial review 2026-07-10: without this arm, one
    /// commit landing mid-build serializes every queued reader's rebuild
    /// behind the slot, where at HEAD they ran in parallel.
    BuildOutsideTheSlot,
}

#[cfg(feature = "graph-algo")]
#[allow(clippy::too_many_arguments)]
fn slot_action(
    cache: &ProjectionCache,
    name: &str,
    generation: u64,
    key: VariantKey,
    edges_id: RelationId,
    nodes_id: Option<RelationId>,
    sources: &[RelationId],
    watermark: u64,
) -> SlotAction {
    if let Some(hit) = cache.consume(name, generation, key, edges_id, nodes_id, watermark) {
        return SlotAction::Hit(hit);
    }
    if !cache.all_fresh(sources.iter().copied(), watermark) {
        return SlotAction::BuildOutsideTheSlot;
    }
    SlotAction::BuildAndPublish
}

/// Scan the sources through this transaction's snapshot and build the CSR.
/// Weighted variants build permissively and record what they saw (§3.2.2).
#[cfg(feature = "graph-algo")]
fn build_variant(
    tx: &SessionTx<'_>,
    edges: &RelationHandle,
    nodes: Option<&RelationHandle>,
    key: VariantKey,
    span: SourceSpan,
    poison: &Poison,
) -> Result<ProjectionVariant> {
    let edge_iter: TupleIter<'_> = Box::new(edges.scan_all(tx));
    let node_iter: Option<TupleIter<'_>> =
        nodes.map(|n| Box::new(n.scan_all(tx)) as TupleIter<'_>);

    Ok(if key.weighted {
        let (graph, indices, inv_indices, has_negative) = build_weighted_csr(
            edge_iter,
            node_iter,
            key.undirected,
            true,
            span,
            span,
            span,
            poison,
        )?;
        ProjectionVariant::new(
            GraphVariant::Weighted {
                graph,
                has_negative,
            },
            indices,
            inv_indices,
        )
    } else {
        let (graph, indices, inv_indices) =
            build_unweighted_csr(edge_iter, node_iter, key.undirected, span, span, poison)?;
        ProjectionVariant::new(GraphVariant::Unweighted(graph), indices, inv_indices)
    })
}

#[cfg(feature = "graph-algo")]
impl<S> crate::Db<S> {
    /// Set the ceiling, in bytes, on the total size of cached graph
    /// projections. Default 512 MiB.
    ///
    /// Enforced immediately: variants are evicted least-recently-used first
    /// until the cache fits. `0` evicts everything and turns caching off —
    /// `::graph create`, `::graph list` and `::graph drop` keep working, and
    /// every algorithm builds its adjacency fresh, exactly as it does today.
    ///
    /// A single variant larger than the whole ceiling is never cached; it is
    /// built for each query, with a warning.
    pub fn set_graph_projection_capacity(&self, bytes: usize) {
        self.graph_projections.set_capacity(bytes);
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

/// Phase 2: the registry, the consume/produce rules, single-flight, and the
/// memory ceiling. Kept apart from the substrate's tests above because these
/// need the `graph` crate's CSR types.
#[cfg(all(test, feature = "graph-algo"))]
mod cache_tests {
    use std::collections::BTreeSet;
    use std::sync::atomic::Ordering;
    use std::sync::{mpsc, Arc, Barrier};

    use graph::prelude::Graph;

    use super::{create_projection, graph_source, GraphSource, GraphVariant, VariantKey};
    use crate::data::value::DataValue;
    use crate::parse::SourceSpan;
    use crate::runtime::db::Poison;
    use crate::runtime::relation::RelationId;
    use crate::storage::mem::MemStorage;
    use crate::{new_cozo_mem, Db, NamedRows, ScriptMutability};

    const DIRECTED: VariantKey = VariantKey {
        undirected: false,
        weighted: false,
    };
    const UNDIRECTED: VariantKey = VariantKey {
        undirected: true,
        weighted: false,
    };
    const WEIGHTED: VariantKey = VariantKey {
        undirected: false,
        weighted: true,
    };

    // Generic over the backend: two of the tests below need a store that
    // outlives the `Db` holding it, which `mem` by construction cannot offer.
    fn run<'s, S: crate::storage::Storage<'s>>(
        db: &'s Db<S>,
        script: &str,
    ) -> miette::Result<NamedRows> {
        db.run_script(script, Default::default(), ScriptMutability::Mutable)
    }

    fn rel_id<'s, S: crate::storage::Storage<'s>>(db: &'s Db<S>, name: &str) -> RelationId {
        db.transact().unwrap().get_relation(name, false).unwrap().id
    }

    fn define<'s, S: crate::storage::Storage<'s>>(
        db: &'s Db<S>,
        name: &str,
        edges: &str,
        nodes: Option<&str>,
    ) -> miette::Result<()> {
        let tx = db.transact()?;
        create_projection(&tx, name, edges, nodes)
    }

    /// One lookup, through a fresh read transaction — the shape a `FixedRule`
    /// will use in Phase 3.
    fn fetch<'s, S: crate::storage::Storage<'s>>(
        db: &'s Db<S>,
        name: &str,
        key: VariantKey,
    ) -> miette::Result<GraphSource> {
        let tx = db.transact()?;
        graph_source(&tx, name, key, SourceSpan(0, 0), &Poison::default())
    }

    fn builds<S>(db: &Db<S>) -> u64 {
        db.graph_projections.build_count()
    }

    fn cached<S>(db: &Db<S>) -> usize {
        db.graph_projections.entry_count.load(Ordering::Relaxed)
    }

    fn counts(source: &GraphSource) -> (u32, u32) {
        match source.graph() {
            GraphVariant::Unweighted(g) => (g.node_count(), g.edge_count()),
            GraphVariant::Weighted { graph, .. } => (graph.node_count(), graph.edge_count()),
        }
    }

    /// `knows` is a 4-edge, 5-vertex digraph: 1→2→3→1, and 4→5.
    fn db_with_edges() -> Db<MemStorage> {
        let db = new_cozo_mem().unwrap();
        run(&db, ":create knows {a: Int, b: Int}").unwrap();
        run(
            &db,
            "?[a, b] <- [[1, 2], [2, 3], [3, 1], [4, 5]] :put knows {a, b}",
        )
        .unwrap();
        db
    }

    fn warm(db: &Db<MemStorage>) -> GraphSource {
        define(db, "g", "knows", None).unwrap();
        fetch(db, "g", DIRECTED).unwrap()
    }

    // ---- Registration ----

    #[test]
    fn create_validates_its_sources_loudly_and_immediately() {
        let db = db_with_edges();
        run(&db, ":create one_col {a: Int}").unwrap();
        run(&db, "::index create knows:idx {b}").unwrap();

        assert!(define(&db, "g", "nonesuch", None).is_err(), "unknown edges");
        assert!(
            define(&db, "g", "knows", Some("nonesuch")).is_err(),
            "unknown nodes"
        );
        assert!(
            define(&db, "g", "one_col", None).is_err(),
            "an edge relation needs arity >= 2"
        );
        assert!(
            define(&db, "g", "knows:idx", None).is_err(),
            "index relations are barred as sources"
        );

        // A `_`-prefixed name is rejected *before* it is resolved. That matters:
        // `::graph create` runs inside a script's transaction, where an earlier
        // statement's `:create _t` would make the name resolve — to a relation
        // whose id comes from a per-transaction counter and collides with a
        // persistent one. Asserting on the diagnostic, not merely on `is_err`,
        // is what pins the order: without the guard the lookup fails with a
        // bare "cannot find requested stored relation" and the collision
        // reappears the moment a temp of that name exists.
        for (edges, nodes) in [("_temp", None), ("knows", Some("_temp"))] {
            let err = define(&db, "g", edges, nodes).unwrap_err().to_string();
            assert!(
                err.contains("temporary relation"),
                "temp relations are barred as sources, and say so: {err}"
            );
        }
        assert_eq!(
            db.graph_projections.list().len(),
            0,
            "no failed create left a definition behind"
        );

        define(&db, "g", "knows", None).unwrap();
        assert!(define(&db, "g", "knows", None).is_err(), "duplicate name");
    }

    /// A tt-stamped relation's default read is its current belief on every
    /// engine path — plain Datalog and positional fixed-rule inputs both
    /// resolve a selector-less read to `AsOf(MAX)` — but a projection build's
    /// raw scan would deliver the whole history keyspace, retracted rows
    /// included, and cache the wrong graph for every fresh transaction. Found
    /// by adversarial review 2026-07-10.
    #[test]
    fn a_transaction_time_relation_is_rejected_as_a_source() {
        let db = db_with_edges();
        run(&db, ":create tt_knows {a: Int, b: Int, tt: TxTime}").unwrap();
        run(&db, ":create bi_knows {a: Int, vt: Validity, tt: TxTime => b: Int}").unwrap();
        run(&db, ":create vt_knows {a: Int, b: Int, vt: Validity}").unwrap();

        for source in ["tt_knows", "bi_knows"] {
            let err = define(&db, "g", source, None).unwrap_err().to_string();
            assert!(
                err.contains("transaction-time"),
                "a tt-stamped edges source must be rejected by name: {err}"
            );
            let err = define(&db, "g", "knows", Some(source))
                .unwrap_err()
                .to_string();
            assert!(err.contains("transaction-time"), "the nodes slot too: {err}");
        }

        // Plain-Validity relations are fine: a selector-less scan returns
        // every version row on every engine path, so the projection matches
        // the positional form exactly.
        define(&db, "vt_ok", "vt_knows", None).unwrap();

        // The guard runs at every use, not only at create: a tt relation
        // renamed into the source name after the fact errors loudly instead
        // of silently projecting history.
        define(&db, "g", "knows", None).unwrap();
        run(&db, "::rename knows -> knows_real, tt_knows -> knows").unwrap();
        let err = fetch(&db, "g", DIRECTED).unwrap_err().to_string();
        assert!(err.contains("transaction-time"), "{err}");
    }

    #[test]
    fn an_unregistered_projection_is_a_loud_error() {
        let db = db_with_edges();
        let err = fetch(&db, "g", DIRECTED).unwrap_err().to_string();
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn dropping_a_projection_frees_its_variants() {
        let db = db_with_edges();
        warm(&db);
        assert_eq!(cached(&db), 1);
        assert!(db.graph_projections.bytes_used() > 0);

        db.graph_projections.drop_projection("g").unwrap();
        assert_eq!(cached(&db), 0);
        assert_eq!(db.graph_projections.bytes_used(), 0);
        assert!(db.graph_projections.list().is_empty());
        assert!(db.graph_projections.drop_projection("g").is_err());
    }

    // ---- Build semantics ----

    #[test]
    fn a_cold_lookup_builds_and_a_warm_one_hits() {
        let db = db_with_edges();
        let before = builds(&db);
        let cold = warm(&db);
        assert_eq!(builds(&db), before + 1);
        assert_eq!(counts(&cold), (5, 4));

        let hot = fetch(&db, "g", DIRECTED).unwrap();
        assert_eq!(builds(&db), before + 1, "a hit must not rebuild");
        assert!(Arc::ptr_eq(&cold, &hot), "and must hand back the same CSR");
    }

    #[test]
    fn each_variant_is_built_and_listed_separately() {
        let db = db_with_edges();
        define(&db, "g", "knows", None).unwrap();

        let d = fetch(&db, "g", DIRECTED).unwrap();
        let u = fetch(&db, "g", UNDIRECTED).unwrap();
        let w = fetch(&db, "g", WEIGHTED).unwrap();
        assert_eq!(builds(&db), 3);
        assert_eq!(cached(&db), 3);

        assert_eq!(counts(&d), (5, 4));
        assert_eq!(counts(&u), (5, 8), "undirected doubles the stored edges");
        assert_eq!(counts(&w), (5, 4));

        let mut labels: Vec<_> = db
            .graph_projections
            .list()
            .iter()
            .map(|s| s.variant.unwrap())
            .collect();
        labels.sort_unstable();
        assert_eq!(labels, ["directed", "directed+weighted", "undirected"]);

        // Each of the three hits, none rebuilds.
        fetch(&db, "g", DIRECTED).unwrap();
        fetch(&db, "g", UNDIRECTED).unwrap();
        fetch(&db, "g", WEIGHTED).unwrap();
        assert_eq!(builds(&db), 3);
    }

    #[test]
    fn a_cold_projection_still_lists_its_definition() {
        let db = db_with_edges();
        define(&db, "g", "knows", Some("knows")).unwrap();
        let rows = db.graph_projections.list();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].edges, "knows");
        assert_eq!(rows[0].nodes.as_deref(), Some("knows"));
        assert!(rows[0].variant.is_none());
        assert!(rows[0].est_bytes.is_none());
    }

    /// §3.2.4. A `nodes` relation registers its vertices before any edge is
    /// read, so a vertex in no edge becomes a real degree-0 vertex rather than
    /// being silently dropped.
    ///
    /// The isolate must take the **highest** dense id, which is why `person`
    /// lists every edge endpoint before `99`. Interned earlier, it would sit
    /// below some edge endpoint, `max_edge_endpoint + 1` would happen to equal
    /// the true vertex count, and the plain `.edges()` sizing would cover it by
    /// accident — leaving the test green against a builder that had dropped the
    /// `node_values` route entirely.
    #[test]
    fn a_nodes_relation_gives_isolated_vertices_real_ids() {
        let db = db_with_edges();
        run(&db, ":create person {p: Int}").unwrap();
        run(&db, "?[p] <- [[1], [2], [3], [4], [5], [99]] :put person {p}").unwrap();

        define(&db, "with_nodes", "knows", Some("person")).unwrap();
        let g = fetch(&db, "with_nodes", DIRECTED).unwrap();
        assert_eq!(counts(&g), (6, 4), "99 is a degree-0 vertex, not a dropout");

        define(&db, "no_nodes", "knows", None).unwrap();
        let g = fetch(&db, "no_nodes", DIRECTED).unwrap();
        assert_eq!(counts(&g), (5, 4), "without `nodes`, only edge endpoints");
    }

    /// An edge endpoint absent from `nodes` is still a vertex: the vertex set
    /// is the union of the two, which is why the edge list must be collected
    /// before `node_values` sizes the graph.
    #[test]
    fn an_edge_endpoint_outside_the_nodes_relation_survives() {
        let db = db_with_edges();
        run(&db, ":create person {p: Int}").unwrap();
        run(&db, "?[p] <- [[1]] :put person {p}").unwrap();

        define(&db, "g", "knows", Some("person")).unwrap();
        assert_eq!(counts(&fetch(&db, "g", DIRECTED).unwrap()), (5, 4));
    }

    /// §3.7.5, load-bearing for Phase 3: the vendored crate cannot express a
    /// zero-vertex CSR, so an empty source yields a phantom vertex. Consumers
    /// must test `indices().is_empty()`, never `node_count() == 0`.
    #[test]
    fn an_empty_edge_relation_yields_a_phantom_vertex_and_no_indices() {
        let db = new_cozo_mem().unwrap();
        run(&db, ":create knows {a: Int, b: Int}").unwrap();
        run(&db, ":create person {p: Int}").unwrap();

        define(&db, "bare", "knows", None).unwrap();
        let g = fetch(&db, "bare", DIRECTED).unwrap();
        assert!(g.indices().is_empty(), "no vertices were interned");
        assert_eq!(counts(&g).0, 1, "but the CSR claims one");

        define(&db, "with_empty_nodes", "knows", Some("person")).unwrap();
        let g = fetch(&db, "with_empty_nodes", DIRECTED).unwrap();
        assert!(g.indices().is_empty());
        assert_eq!(counts(&g).0, 1);

        run(&db, "?[p] <- [[7], [8], [9]] :put person {p}").unwrap();
        define(&db, "nodes_only", "knows", Some("person")).unwrap();
        let g = fetch(&db, "nodes_only", DIRECTED).unwrap();
        assert_eq!(counts(&g), (3, 0), "three isolated vertices, no edges");
    }

    /// §3.2.2. One weighted variant serves everyone: it builds permissively and
    /// records what it saw, so a strict consumer can reject it in Phase 3.
    #[test]
    fn a_negative_weight_is_recorded_rather_than_rejected() {
        let db = new_cozo_mem().unwrap();
        run(&db, ":create we {a: Int, b: Int => w: Float}").unwrap();
        run(&db, "?[a, b, w] <- [[1, 2, 2.5]] :put we {a, b => w}").unwrap();
        define(&db, "pos", "we", None).unwrap();
        match fetch(&db, "pos", WEIGHTED).unwrap().graph() {
            GraphVariant::Weighted { has_negative, .. } => assert!(!has_negative),
            _ => panic!("asked for a weighted variant"),
        }

        run(&db, "?[a, b, w] <- [[3, 4, -1.0]] :put we {a, b => w}").unwrap();
        define(&db, "neg", "we", None).unwrap();
        match fetch(&db, "neg", WEIGHTED).unwrap().graph() {
            GraphVariant::Weighted { has_negative, .. } => {
                assert!(has_negative, "the scan must record the negative weight")
            }
            _ => panic!("asked for a weighted variant"),
        }
    }

    /// A missing third column defaults to weight 1.0, so a 2-column relation
    /// projects into a weighted variant without complaint. The weights are
    /// read back out of the CSR: asserting only on counts would stay green
    /// under any wrong default.
    #[test]
    fn an_unweighted_source_yields_unit_weights() {
        use graph::prelude::DirectedNeighborsWithValues;

        let db = db_with_edges();
        define(&db, "g", "knows", None).unwrap();
        match fetch(&db, "g", WEIGHTED).unwrap().graph() {
            GraphVariant::Weighted {
                graph,
                has_negative,
            } => {
                assert!(!has_negative);
                assert_eq!(graph.edge_count(), 4);
                let weights: Vec<f32> = (0..graph.node_count())
                    .flat_map(|v| graph.out_neighbors_with_values(v))
                    .map(|t| t.value)
                    .collect();
                assert_eq!(weights.len(), 4);
                assert!(
                    weights.iter().all(|w| *w == 1.0),
                    "a missing weight column must default to exactly 1.0: {weights:?}"
                );
            }
            _ => panic!("asked for a weighted variant"),
        }
    }

    // ---- Invalidation ----

    #[test]
    fn writing_a_source_frees_its_entry_and_forces_a_rebuild() {
        let db = db_with_edges();
        warm(&db);
        assert_eq!(cached(&db), 1);

        run(&db, "?[a, b] <- [[5, 6]] :put knows {a, b}").unwrap();
        assert_eq!(cached(&db), 0, "a bumped source's entries are unreachable");
        assert_eq!(
            db.graph_projections.bytes_used(),
            0,
            "and their bytes are given back to the ceiling"
        );

        let before = builds(&db);
        let g = fetch(&db, "g", DIRECTED).unwrap();
        assert_eq!(builds(&db), before + 1);
        assert_eq!(counts(&g), (6, 5), "and the rebuild sees the new edge");
    }

    #[test]
    fn writing_an_unrelated_relation_leaves_the_entry_alone() {
        let db = db_with_edges();
        run(&db, ":create other {x: Int}").unwrap();
        warm(&db);

        run(&db, "?[x] <- [[1]] :put other {x}").unwrap();
        assert_eq!(cached(&db), 1);
        let before = builds(&db);
        fetch(&db, "g", DIRECTED).unwrap();
        assert_eq!(builds(&db), before, "still a hit");
    }

    #[test]
    fn writing_the_nodes_relation_invalidates_too() {
        let db = db_with_edges();
        run(&db, ":create person {p: Int}").unwrap();
        run(&db, "?[p] <- [[1]] :put person {p}").unwrap();
        define(&db, "g", "knows", Some("person")).unwrap();
        fetch(&db, "g", DIRECTED).unwrap();
        assert_eq!(cached(&db), 1);

        run(&db, "?[p] <- [[42]] :put person {p}").unwrap();
        assert_eq!(cached(&db), 0);
        assert_eq!(
            counts(&fetch(&db, "g", DIRECTED).unwrap()),
            (6, 4),
            "42 joins as an isolated vertex"
        );
    }

    /// An aborted transaction bumps conservatively, so its entries go too:
    /// `mem`'s range deletes write straight through the transaction cache.
    #[test]
    fn an_aborted_write_also_frees_the_entries() {
        let db = db_with_edges();
        warm(&db);
        assert_eq!(cached(&db), 1);

        // One script, one transaction: the put lands in the transaction cache
        // and marks `knows` dirty, the second statement fails, the transaction
        // is dropped uncommitted.
        run(
            &db,
            "{?[a, b] <- [[7, 7]] :put knows {a, b}}
             {?[x] <- [[1]] :put no_such_relation {x}}",
        )
        .unwrap_err();
        assert_eq!(cached(&db), 0);
        assert_eq!(
            counts(&fetch(&db, "g", DIRECTED).unwrap()),
            (5, 4),
            "and the rebuild sees the rolled-back relation"
        );
    }

    #[test]
    fn replace_mints_a_new_id_so_the_old_entry_dies() {
        let db = db_with_edges();
        warm(&db);
        let old = rel_id(&db, "knows");

        run(&db, "?[a, b] <- [[8, 9]] :replace knows {a: Int, b: Int}").unwrap();
        assert_ne!(rel_id(&db, "knows"), old, "`:replace` mints a fresh id");
        assert_eq!(cached(&db), 0);
        assert_eq!(counts(&fetch(&db, "g", DIRECTED).unwrap()), (2, 1));
    }

    #[test]
    fn removing_a_source_frees_the_entry_and_the_name_stops_resolving() {
        let db = db_with_edges();
        warm(&db);

        run(&db, "::remove knows").unwrap();
        assert_eq!(cached(&db), 0);
        let err = fetch(&db, "g", DIRECTED).unwrap_err().to_string();
        assert!(err.contains("knows"), "{err}");
    }

    #[test]
    fn renaming_a_source_away_is_a_loud_error() {
        let db = db_with_edges();
        warm(&db);
        run(&db, "::rename knows -> acquaintances").unwrap();

        assert!(
            fetch(&db, "g", DIRECTED).is_err(),
            "the definition names `knows`, which no longer exists"
        );
    }

    /// `::rename` bumps no token, so nothing but the edges slot's id binding
    /// stands between a rotated name and a stale graph.
    ///
    /// The store is **reopened** on purpose. In a process that wrote the
    /// relations itself, every one of them carries a distinct token, and the
    /// consume rule's token comparison would catch the swap all on its own —
    /// so a mem fixture cannot tell the id binding apart from the token check.
    /// A `Db` opened over an existing store has no `RelState` for anything: all
    /// tokens read `0`, the token comparison passes for *every* relation, and
    /// only the slot-id binding is left standing.
    #[cfg(feature = "storage-sqlite")]
    #[test]
    fn swapping_two_edge_relations_forces_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("swap.sqlite");
        {
            let db = crate::new_cozo_sqlite(&path).unwrap();
            run(&db, ":create e {a: Int, b: Int}").unwrap();
            run(&db, ":create f {a: Int, b: Int}").unwrap();
            run(&db, "?[a, b] <- [[1, 2]] :put e {a, b}").unwrap();
            run(&db, "?[a, b] <- [[5, 6], [6, 7]] :put f {a, b}").unwrap();
        }

        let db = crate::new_cozo_sqlite(&path).unwrap();
        let e = rel_id(&db, "e");
        let f = rel_id(&db, "f");
        assert_eq!(db.graph_projections.rel_state(e).token, 0);
        assert_eq!(
            db.graph_projections.rel_state(f).token,
            0,
            "a reopened store has written nothing in this process, so the token \
             comparison cannot distinguish these two relations"
        );

        define(&db, "g", "e", None).unwrap();
        assert_eq!(counts(&fetch(&db, "g", DIRECTED).unwrap()), (2, 1));
        let before = builds(&db);

        run(&db, "::rename e -> swap_tmp, f -> e, swap_tmp -> f").unwrap();
        assert_eq!(cached(&db), 1, "a rename invalidates nothing");
        assert_eq!(rel_id(&db, "e"), f, "`e` now names what `f` used to");

        let after = fetch(&db, "g", DIRECTED).unwrap();
        assert_eq!(builds(&db), before + 1, "so the entry cannot be served");
        assert_eq!(counts(&after), (3, 2));
    }

    /// The strictest form of the binding, and the one the spec's forbidden
    /// alternative actually fails: rotate the edges and nodes relations OF THE
    /// SAME projection into each other's slots. The *set* of resolved ids is
    /// unchanged — only their assignment to slots moved — so an
    /// unordered-set comparison in the consume rule would serve the old entry
    /// with its edges and nodes exchanged. Tokens cannot catch it here either:
    /// on a reopened store both read 0, and on a same-process fixture their
    /// distinct values would mask the id check entirely (which is why the two
    /// sibling tests alone were insufficient — found by adversarial review
    /// 2026-07-10). Only a per-slot id comparison stands.
    #[cfg(feature = "storage-sqlite")]
    #[test]
    fn rotating_the_edges_and_nodes_slots_of_one_projection_forces_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rotate.sqlite");
        {
            let db = crate::new_cozo_sqlite(&path).unwrap();
            run(&db, ":create e {a: Int, b: Int}").unwrap();
            run(&db, ":create n {p: Int, q: Int}").unwrap();
            run(&db, "?[a, b] <- [[1, 2]] :put e {a, b}").unwrap();
            run(&db, "?[p, q] <- [[5, 6], [6, 7]] :put n {p, q}").unwrap();
        }

        let db = crate::new_cozo_sqlite(&path).unwrap();
        let e = rel_id(&db, "e");
        let n = rel_id(&db, "n");
        assert_eq!(db.graph_projections.rel_state(e).token, 0);
        assert_eq!(db.graph_projections.rel_state(n).token, 0);

        define(&db, "g", "e", Some("n")).unwrap();
        let before_rotation = fetch(&db, "g", DIRECTED).unwrap();
        assert_eq!(
            counts(&before_rotation),
            (4, 1),
            "vertices 5, 6 (from n) and 1, 2 (edge endpoints); one edge"
        );
        let before = builds(&db);

        run(&db, "::rename e -> rot_tmp, n -> e, rot_tmp -> n").unwrap();
        assert_eq!(cached(&db), 1, "a rename invalidates nothing");
        assert_eq!(rel_id(&db, "e"), n, "the ids rotated with the names");
        assert_eq!(rel_id(&db, "n"), e);

        let after = fetch(&db, "g", DIRECTED).unwrap();
        assert_eq!(
            builds(&db),
            before + 1,
            "the id SET is unchanged but the slot assignment is not — \
             serving the old entry would swap edges and nodes"
        );
        assert_eq!(
            counts(&after),
            (4, 2),
            "edges now come from old n (5→6, 6→7); nodes from old e's column a"
        );
    }

    /// The same, one slot over: the nodes slot's id binding is checked too, and
    /// again only a reopened store puts it under load. See the sibling test.
    #[cfg(feature = "storage-sqlite")]
    #[test]
    fn swapping_the_nodes_relation_with_another_forces_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("swap_nodes.sqlite");
        {
            let db = crate::new_cozo_sqlite(&path).unwrap();
            run(&db, ":create knows {a: Int, b: Int}").unwrap();
            run(
                &db,
                "?[a, b] <- [[1, 2], [2, 3], [3, 1], [4, 5]] :put knows {a, b}",
            )
            .unwrap();
            run(&db, ":create person {p: Int}").unwrap();
            run(&db, ":create city {p: Int}").unwrap();
            run(&db, "?[p] <- [[1]] :put person {p}").unwrap();
            run(&db, "?[p] <- [[70], [80]] :put city {p}").unwrap();
        }

        let db = crate::new_cozo_sqlite(&path).unwrap();
        assert_eq!(db.graph_projections.rel_state(rel_id(&db, "person")).token, 0);
        assert_eq!(db.graph_projections.rel_state(rel_id(&db, "city")).token, 0);

        define(&db, "g", "knows", Some("person")).unwrap();
        assert_eq!(counts(&fetch(&db, "g", DIRECTED).unwrap()), (5, 4));
        let before = builds(&db);

        run(
            &db,
            "::rename person -> swap_tmp, city -> person, swap_tmp -> city",
        )
        .unwrap();
        assert_eq!(cached(&db), 1);

        let after = fetch(&db, "g", DIRECTED).unwrap();
        assert_eq!(
            builds(&db),
            before + 1,
            "the edges slot is unchanged, but the nodes slot is not"
        );
        assert_eq!(counts(&after), (7, 4), "70 and 80 joined as isolates");
    }

    /// Defence in depth. Every path that bumps a token also frees the entries
    /// built over that relation, so the consume rule's token comparison never
    /// fires in production. It is the net under the invalidation hooks: a
    /// future mutation path that bumps and forgets to free must still miss,
    /// not serve a stale CSR.
    #[test]
    fn a_token_bump_alone_denies_the_hit_even_if_the_entry_survives() {
        let db = db_with_edges();
        warm(&db);
        let cache = &db.graph_projections;
        let (generation, ..) = cache.definition("g").unwrap();
        let edges = rel_id(&db, "knows");

        cache.bump_token_leaving_entries(edges);
        assert_eq!(cached(&db), 1, "the entry is still there, by construction");

        let watermark = cache.watermark();
        assert!(
            cache.all_fresh(std::iter::once(edges), watermark),
            "and the relation is fresh again for a reader pinning now"
        );
        assert!(
            cache
                .consume("g", generation, DIRECTED, edges, None, watermark)
                .is_none(),
            "yet the entry's token no longer names the relation's content"
        );
    }

    /// §3.1, §3.3. `::rename` moves catalog rows and bumps nothing, so tokens
    /// cannot see a swap. The consume rule binds each *slot* to the id it
    /// resolved to; comparing the entry's ids as an unordered set — the shape
    /// the spec forbids — would hand back a graph with its edges and nodes
    /// exchanged, since the *set* of ids is unchanged by the swap.
    #[test]
    fn the_slot_id_binding_defeats_a_multi_pair_swap_rename() {
        let db = new_cozo_mem().unwrap();
        run(&db, ":create e {a: Int, b: Int}").unwrap();
        run(&db, ":create n {a: Int, b: Int}").unwrap();
        run(&db, "?[a, b] <- [[1, 2]] :put e {a, b}").unwrap();
        run(&db, "?[a, b] <- [[5, 5], [6, 6]] :put n {a, b}").unwrap();

        define(&db, "g", "e", Some("n")).unwrap();
        let before = fetch(&db, "g", DIRECTED).unwrap();
        assert_eq!(counts(&before), (4, 1), "vertices 5,6,1,2 and edge 1→2");
        let builds_before = builds(&db);

        // A three-pair rotation: the engine refuses to rename onto a live name,
        // so a swap goes through a scratch name. No relation's contents — and
        // no relation's id — changes.
        run(&db, "::rename e -> swap_tmp, n -> e, swap_tmp -> n").unwrap();
        assert_eq!(cached(&db), 1, "a rename invalidates nothing");

        let after = fetch(&db, "g", DIRECTED).unwrap();
        assert_eq!(
            builds(&db),
            builds_before + 1,
            "the slots now resolve to different ids, so the entry cannot be served"
        );
        assert_eq!(counts(&after), (3, 2), "vertices 5,6,1 and the two loops");
    }

    #[test]
    fn invalidate_all_clears_the_variants_and_keeps_the_definitions() {
        let db = db_with_edges();
        warm(&db);

        db.graph_projections.invalidate_all();
        assert_eq!(cached(&db), 0);
        assert_eq!(db.graph_projections.bytes_used(), 0);

        let rows = db.graph_projections.list();
        assert_eq!(rows.len(), 1, "the definition outlives its variants");
        assert!(rows[0].variant.is_none());
    }

    // ---- The consume and produce rules ----

    /// The direction the draft protocol got wrong: an entry built from a newer
    /// snapshot must never be served to a transaction that pinned before it.
    /// The end-to-end interleaving needs a long-lived read transaction, which
    /// `mem` cannot host (its read transaction holds a lock guard for its whole
    /// life, so no writer can run) — that test is rocksdb-scoped, Phase 4. Here
    /// the rule itself is driven directly.
    #[test]
    fn an_entry_is_never_served_to_a_transaction_that_pinned_before_it() {
        let db = db_with_edges();
        warm(&db);
        let cache = &db.graph_projections;
        let (generation, ..) = cache.definition("g").unwrap();
        let edges = rel_id(&db, "knows");
        let token = cache.rel_state(edges).token;
        assert!(token > 0, "the fixture's `:put` gave `knows` a token");

        assert!(
            cache
                .consume("g", generation, DIRECTED, edges, None, token)
                .is_some(),
            "a reader that pinned at or after the entry's token hits"
        );
        assert!(
            cache
                .consume("g", generation, DIRECTED, edges, None, token - 1)
                .is_none(),
            "a reader that pinned before it must miss and rebuild"
        );
    }

    /// A transaction that pinned before the source's last commit cannot vouch
    /// for what it read, so its build stays private to it.
    #[test]
    fn an_unfresh_producer_publishes_nothing() {
        let db = db_with_edges();
        let source = warm(&db);
        define(&db, "h", "knows", None).unwrap(); // a second, still-cold projection
        let cache = &db.graph_projections;
        let (h_generation, ..) = cache.definition("h").unwrap();
        let edges = rel_id(&db, "knows");
        let token = cache.rel_state(edges).token;
        assert!(token > 0, "the fixture's writes gave `knows` a token");
        assert_eq!(cached(&db), 1);

        cache.produce(
            "h",
            h_generation,
            DIRECTED,
            edges,
            None,
            token - 1,
            &source,
        );
        assert_eq!(cached(&db), 1, "an unfresh producer must not publish");

        cache.produce("h", h_generation, DIRECTED, edges, None, token, &source);
        assert_eq!(cached(&db), 2, "a fresh one may");
    }

    /// A build begun against a definition that has since been dropped and
    /// re-created belongs to neither. It may not publish into the new
    /// definition, and a lookup holding the old generation may not read what
    /// the new one has cached.
    #[test]
    fn a_stale_definition_generation_neither_reads_nor_writes_the_cache() {
        let db = db_with_edges();
        define(&db, "g", "knows", None).unwrap();
        let cache = &db.graph_projections;
        let (old_generation, ..) = cache.definition("g").unwrap();
        let edges = rel_id(&db, "knows");
        let token = cache.rel_state(edges).token;
        let source = fetch(&db, "g", DIRECTED).unwrap();

        cache.drop_projection("g").unwrap();
        define(&db, "g", "knows", None).unwrap();
        let (new_generation, ..) = cache.definition("g").unwrap();
        assert_ne!(old_generation, new_generation);

        cache.produce("g", old_generation, DIRECTED, edges, None, token, &source);
        assert_eq!(cached(&db), 0, "the stale generation must not publish");

        // Now let the new definition populate, and check the stale reader is
        // still shut out of an entry whose ids and tokens both match.
        fetch(&db, "g", DIRECTED).unwrap();
        assert_eq!(cached(&db), 1);
        assert!(
            cache
                .consume("g", new_generation, DIRECTED, edges, None, token)
                .is_some(),
            "the live generation reads it"
        );
        assert!(
            cache
                .consume("g", old_generation, DIRECTED, edges, None, token)
                .is_none(),
            "the stale one does not"
        );
    }

    /// A transaction's own uncommitted writes are invisible to everyone else,
    /// so it must neither consult the cache nor populate it.
    #[test]
    fn a_transactions_own_uncommitted_writes_bypass_the_cache_entirely() {
        let db = db_with_edges();
        warm(&db);
        assert_eq!(cached(&db), 1);
        let before = builds(&db);

        {
            let mut tx = db.transact_write().unwrap();
            let handle = tx.get_relation("knows", false).unwrap();
            tx.mark_dirty(&handle);
            let g = graph_source(&tx, "g", DIRECTED, SourceSpan(0, 0), &Poison::default()).unwrap();
            assert_eq!(builds(&db), before + 1, "the writer built its own graph");
            assert_eq!(counts(&g), (5, 4));
            assert_eq!(cached(&db), 1, "and published nothing of its own");
            // Dropped without committing: bumps conservatively, so the entry
            // the *reader* left behind goes too.
        }
        assert_eq!(cached(&db), 0);
    }

    /// The produce half of Door 1, pinned on its own. In the warm-cache test
    /// above, "published nothing" also follows from the consume hit never
    /// reaching a build at all — so a mutant that lets a dirty transaction
    /// fall through to `produce` stays green there. Here the cache is COLD:
    /// the dirty transaction must build, its tokens are unmoved (nothing has
    /// committed), so `FRESH` holds and the produce rule alone would happily
    /// publish a CSR built from uncommitted rows. Only Door 1 stands in the
    /// way. Found by adversarial review 2026-07-10.
    #[test]
    fn a_dirty_transaction_publishes_nothing_even_into_a_cold_cache() {
        let db = db_with_edges();
        define(&db, "g", "knows", None).unwrap();
        assert_eq!(cached(&db), 0, "the cache must start cold");
        let before = builds(&db);

        {
            let mut tx = db.transact_write().unwrap();
            let handle = tx.get_relation("knows", false).unwrap();
            tx.mark_dirty(&handle);
            let g = graph_source(&tx, "g", DIRECTED, SourceSpan(0, 0), &Poison::default()).unwrap();
            assert_eq!(builds(&db), before + 1, "the dirty transaction built");
            assert_eq!(counts(&g), (5, 4));
            assert_eq!(
                cached(&db),
                0,
                "and must not publish: its snapshot may hold uncommitted rows \
                 that no other transaction can see"
            );
        }
    }

    /// Spec §5, interleaving test (a), on the consume rule. A writer parked
    /// between its durable commit and its token bump has moved no token, so
    /// only `inflight` stands between a reader and a stale hit — and only
    /// `inflight` denies the produce rule too.
    #[test]
    fn a_commit_in_flight_denies_both_the_hit_and_the_insert() {
        let db = Arc::new(db_with_edges());
        let source = warm(&db);
        let cache = &db.graph_projections;
        let (generation, ..) = cache.definition("g").unwrap();
        let edges = rel_id(&db, "knows");
        let reader_watermark = cache.watermark();

        // A second, still-cold projection over the same relation, so the
        // produce rule has somewhere to insert if it wrongly allows it.
        define(&db, "h", "knows", None).unwrap();
        let (h_generation, ..) = cache.definition("h").unwrap();

        assert!(
            cache
                .consume("g", generation, DIRECTED, edges, None, reader_watermark)
                .is_some(),
            "the entry is servable before the writer starts"
        );

        let (parked_tx, parked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::sync_channel::<()>(0);
        let release_rx = std::sync::Mutex::new(release_rx);
        cache.set_commit_fence(Some(Arc::new(move |dirty: &BTreeSet<RelationId>| {
            parked_tx.send(dirty.clone()).unwrap();
            let _ = release_rx.lock().unwrap().recv();
        })));

        let writer = {
            let db = db.clone();
            std::thread::spawn(move || {
                run(&db, "?[a, b] <- [[7, 7]] :put knows {a, b}").unwrap();
            })
        };
        let dirty = parked_rx.recv().unwrap();
        assert!(dirty.contains(&edges));

        // The commit is durable; the token has not moved; the entry is still in
        // the registry, still tagged with the token the relation still carries.
        assert_eq!(cached(&db), 1);
        assert!(
            cache
                .consume("g", generation, DIRECTED, edges, None, reader_watermark)
                .is_none(),
            "`inflight` alone must deny the hit"
        );
        assert!(
            !cache.all_fresh(std::iter::once(edges), reader_watermark),
            "and deny the `graph_source` fast path its build slot"
        );
        cache.produce(
            "h",
            h_generation,
            DIRECTED,
            edges,
            None,
            reader_watermark,
            &source,
        );
        assert_eq!(
            cached(&db),
            1,
            "`inflight` must deny the insert too: a build made from a snapshot \
             taken before a durable-but-unbumped commit may not be published"
        );

        release_tx.send(()).unwrap();
        writer.join().unwrap();
        cache.set_commit_fence(None);

        assert_eq!(cached(&db), 0, "the bump then freed the entry");
        assert_eq!(counts(&fetch(&db, "g", DIRECTED).unwrap()), (6, 5));
    }

    // ---- Single-flight ----

    /// The three-way decision a slot holder makes on waking, driven directly
    /// at the watermarks a real interleaving would produce (`mem` cannot host
    /// the interleaving itself — a read transaction holds a lock guard for its
    /// life). The load-bearing arm is the third: a waiter whose sources were
    /// bumped while it queued must build *outside* the slot — its produce
    /// would be refused, so building inside would serialize every queued
    /// reader behind a build that publishes nothing.
    #[test]
    fn a_waiter_that_went_stale_while_queued_builds_outside_the_slot() {
        use super::{slot_action, SlotAction};

        let db = db_with_edges();
        define(&db, "g", "knows", None).unwrap();
        let cache = &db.graph_projections;
        let (generation, ..) = cache.definition("g").unwrap();
        let edges = rel_id(&db, "knows");
        let sources = [edges];
        let watermark = cache.watermark();

        // Cold and fresh: the winner's case.
        assert!(matches!(
            slot_action(cache, "g", generation, DIRECTED, edges, None, &sources, watermark),
            SlotAction::BuildAndPublish
        ));

        // An entry landed while queued, still fresh for us: the payoff case.
        fetch(&db, "g", DIRECTED).unwrap();
        assert!(matches!(
            slot_action(cache, "g", generation, DIRECTED, edges, None, &sources, watermark),
            SlotAction::Hit(_)
        ));

        // The sources were bumped while we queued: our watermark predates the
        // bump, freshness never comes back, produce would refuse — get out of
        // everyone's way and build outside.
        cache.bump_token_leaving_entries(edges);
        assert!(matches!(
            slot_action(cache, "g", generation, DIRECTED, edges, None, &sources, watermark),
            SlotAction::BuildOutsideTheSlot
        ));
    }

    /// §3.2.8. N concurrent cold readers coalesce into exactly one build: the
    /// winner holds the per-variant slot across its build, and the losers wake
    /// to a populated, fresh-for-them entry.
    #[test]
    fn concurrent_cold_readers_coalesce_into_one_build() {
        let db = new_cozo_mem().unwrap();
        run(&db, ":create knows {a: Int, b: Int}").unwrap();
        let rows: Vec<Vec<DataValue>> = (0..5_000i64)
            .map(|i| vec![DataValue::from(i), DataValue::from((i + 1) % 5_000)])
            .collect();
        db.import_relations(
            [(
                "knows".to_string(),
                NamedRows::new(vec!["a".to_string(), "b".to_string()], rows),
            )]
            .into(),
        )
        .unwrap();
        define(&db, "g", "knows", None).unwrap();

        let db = Arc::new(db);
        let threads = 8;
        let barrier = Arc::new(Barrier::new(threads));
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let db = db.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let tx = db.transact().unwrap();
                    graph_source(&tx, "g", DIRECTED, SourceSpan(0, 0), &Poison::default()).unwrap()
                })
            })
            .collect();

        let sources: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(builds(&db), 1, "one build served all {threads} readers");
        assert_eq!(cached(&db), 1);
        for source in &sources {
            assert!(
                Arc::ptr_eq(source, &sources[0]),
                "every reader got the same CSR"
            );
            assert_eq!(counts(source), (5_000, 5_000));
        }
    }

    // ---- The memory ceiling ----

    #[test]
    fn capacity_zero_disables_caching_but_not_the_registry() {
        let db = db_with_edges();
        warm(&db);
        assert_eq!(cached(&db), 1);

        db.set_graph_projection_capacity(0);
        assert_eq!(cached(&db), 0, "the setter enforces immediately");
        assert_eq!(db.graph_projections.bytes_used(), 0);

        let before = builds(&db);
        let g = fetch(&db, "g", DIRECTED).unwrap();
        assert_eq!(counts(&g), (5, 4), "lookups still answer, from fresh builds");
        assert_eq!(builds(&db), before + 1);
        assert_eq!(cached(&db), 0, "and never populate");

        // create/list/drop keep working with the cache off.
        define(&db, "h", "knows", None).unwrap();
        assert_eq!(db.graph_projections.list().len(), 2);
        db.graph_projections.drop_projection("h").unwrap();

        db.set_graph_projection_capacity(1 << 20);
        fetch(&db, "g", DIRECTED).unwrap();
        assert_eq!(cached(&db), 1, "raising the ceiling re-enables caching");
    }

    /// With the cache off, lookups must not touch single-flight at all: a
    /// disabled cache never populates, so a slot could only serialize builds
    /// that run in parallel today. (The slot map self-cleans, so only the
    /// cumulative acquisition counter can witness this.) Found by adversarial
    /// review 2026-07-10.
    #[test]
    fn capacity_zero_skips_the_build_slot() {
        let db = db_with_edges();
        define(&db, "g", "knows", None).unwrap();
        db.set_graph_projection_capacity(0);

        let slots_before = db.graph_projections.slots_acquired.load(Ordering::Relaxed);
        fetch(&db, "g", DIRECTED).unwrap();
        fetch(&db, "g", DIRECTED).unwrap();
        assert_eq!(
            db.graph_projections.slots_acquired.load(Ordering::Relaxed),
            slots_before,
            "a disabled cache must bypass single-flight"
        );

        db.set_graph_projection_capacity(1 << 20);
        fetch(&db, "g", DIRECTED).unwrap();
        assert_eq!(
            db.graph_projections.slots_acquired.load(Ordering::Relaxed),
            slots_before + 1,
            "re-enabling brings the slot back"
        );
    }

    #[test]
    fn a_variant_larger_than_the_whole_ceiling_is_built_ephemerally() {
        let db = db_with_edges();
        define(&db, "g", "knows", None).unwrap();
        db.set_graph_projection_capacity(1);

        let before = builds(&db);
        assert_eq!(counts(&fetch(&db, "g", DIRECTED).unwrap()), (5, 4));
        assert_eq!(cached(&db), 0, "it cannot fit, so it is never cached");
        assert_eq!(builds(&db), before + 1);
        fetch(&db, "g", DIRECTED).unwrap();
        assert_eq!(builds(&db), before + 2, "and rebuilds every query");
    }

    #[test]
    fn shrinking_the_ceiling_evicts_the_least_recently_used_variant() {
        let db = db_with_edges();
        define(&db, "g", "knows", None).unwrap();
        fetch(&db, "g", DIRECTED).unwrap();
        fetch(&db, "g", UNDIRECTED).unwrap();
        let total = db.graph_projections.bytes_used();
        assert_eq!(cached(&db), 2);

        db.set_graph_projection_capacity(total - 1);
        assert_eq!(cached(&db), 1);
        let survivors = db.graph_projections.list();
        assert_eq!(
            survivors[0].variant,
            Some("undirected"),
            "`directed` was built first and never touched again"
        );
    }

    #[test]
    fn a_hit_moves_its_variant_to_the_head_of_the_lru() {
        let db = db_with_edges();
        define(&db, "g", "knows", None).unwrap();
        fetch(&db, "g", DIRECTED).unwrap();
        fetch(&db, "g", UNDIRECTED).unwrap();
        let total = db.graph_projections.bytes_used();

        fetch(&db, "g", DIRECTED).unwrap(); // a hit, which must move it up
        db.set_graph_projection_capacity(total - 1);

        let survivors = db.graph_projections.list();
        assert_eq!(cached(&db), 1);
        assert_eq!(
            survivors[0].variant,
            Some("directed"),
            "the touched variant outlives the older one"
        );
    }

    /// An insert makes room for itself, evicting the LRU until it fits — it
    /// does not simply refuse. The two variants here share an id map and differ
    /// only in their target arrays, so the directed one is strictly smaller and
    /// fits alone under a ceiling sized for the undirected one.
    #[test]
    fn an_insert_evicts_until_it_fits() {
        let db = db_with_edges();
        define(&db, "g", "knows", None).unwrap();
        let undirected = fetch(&db, "g", UNDIRECTED).unwrap();
        let ceiling = db.graph_projections.bytes_used();
        assert_eq!(undirected.est_bytes(), ceiling);

        // Room for exactly one undirected variant, and nothing beside it.
        db.set_graph_projection_capacity(ceiling);
        assert_eq!(cached(&db), 1);

        let directed = fetch(&db, "g", DIRECTED).unwrap();
        assert!(directed.est_bytes() < ceiling, "and the newcomer does fit");
        assert_eq!(cached(&db), 1, "so it evicted the one in its way");
        assert_eq!(db.graph_projections.list()[0].variant, Some("directed"));
        assert_eq!(db.graph_projections.bytes_used(), directed.est_bytes());
    }

    #[test]
    fn re_publishing_a_variant_does_not_double_count_its_bytes() {
        let db = db_with_edges();
        warm(&db);
        let once = db.graph_projections.bytes_used();

        let cache = &db.graph_projections;
        let (generation, ..) = cache.definition("g").unwrap();
        let edges = rel_id(&db, "knows");
        let token = cache.rel_state(edges).token;
        let source = fetch(&db, "g", DIRECTED).unwrap();
        cache.produce("g", generation, DIRECTED, edges, None, token, &source);

        assert_eq!(cached(&db), 1);
        assert_eq!(cache.bytes_used(), once, "the older entry's bytes were freed");
    }
}
