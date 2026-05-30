# mnestic fork changelog

Divergences from upstream CozoDB `481af05` (2024-12-04). See `FORK.md` for
provenance and licensing.

## 0.8.0 — 2026-05-30

First fork release. Lineage: cozo 0.7.6 + 30 unreleased upstream commits (our fork
point). Bumped to **0.8.0** (not 0.7.7) to mark the fork's identity as a maintained,
agentic-memory-focused engine: it ships real planner/parser fixes *and* the first
hybrid-retrieval primitives (RRF, MMR). All changes are pure `cozo-core` (Rust) —
no `cozorocks` bridge changes — so the rocksdb feature still resolves to upstream
`cozorocks 0.1.7`. 169 inherited lib tests + 68 air_routes + all fork/feature tests
pass.

### Fork bootstrap
- Forked from `cozodb/cozo` at `481af05`; full history preserved, upstream
  remote retained as `upstream`, fork point tagged `fork-base`.
- Renamed brand and core package from `cozo` to `mnestic`. Original MPL-2.0
  per-file copyright notices preserved.
- Added `FORK.md` (provenance/attribution) and this changelog.

### Audit of MindGraph's drafted upstream bugs against the fork point

The drafts in `mindgraph-rs/docs/upstream_bugs/` were written against the
**crates.io 0.7.6 release**. Our fork point (`481af05`) is the upstream `main`
HEAD, which is **30 commits ahead of the `v0.7.6` tag** (Ziyang merged fixes
after the release but never cut another version). Each draft was therefore
re-verified against HEAD with a backend-correct reproducer:

| # | Bug | Status at fork HEAD | Evidence |
|---|-----|---------------------|----------|
| 3 | `mat_join` joins on wrong symbols (silent 0 rows) | **Already fixed** by the PR #286 "fix-stored-prefix-join" cluster | `tests/matjoin_regression.rs` passes; plan now emits `stored_prefix_join {"**2":"uid"}` instead of `stored_mat_join {"**2":"**0"}` |
| 1 | Equality post-filter → full scan, not prefix lookup | **Was present; now FIXED in the fork** (measured ~28–29× at 5k rows) | fix in `query/reorder.rs`; `tests/fork_regressions.rs::equality_post_filter_uses_prefix_lookup` now active — see "Shipped" below |
| 2 | top-level `:create _foo` silent no-op | **Present, but is a scoping nuance** — `_`-relations are legitimate transaction-scoped temporaries (work across imperative `{...}` blocks). Only the top-level form is a silent trap. Fix is a design call. | `tests/fork_regressions.rs::top_level_create_underscore_relation_is_a_silent_noop` (ignored) |
| 4 | Secondary-index puts: N separate `.put()` calls | **Present** (perf 2-3×) | `cozo-core/src/query/stored.rs` still loops `store_tx.put()` per index |
| 7 | `hnsw` serial neighbor fetches | **Present** (perf 10-20% HNSW) | `cozo-core/src/runtime/hnsw.rs::VectorCache::ensure_key` still single-key `handle.get()` |

Note: the fork being 30 commits ahead of the released 0.7.6 means simply
adopting mnestic already gives MindGraph bug #3's fix (and the other unreleased
fixes) for free.

### Shipped in the fork

#### Phase 1 — agentic-memory features
- **`ReciprocalRankFusion` fixed rule (hybrid retrieval, Bet 1)** —
  `cozo-core/src/fixed_rule/utilities/rrf.rs`, aliased `RRF`. Fuses several ranked
  result lists (vector/HNSW + full-text/FTS + graph traversal) into one ranking
  via `Σ_lists 1/(k + rank_in_list)`. Input is a single relation
  `[list_id, item, score]`; rows are grouped by `list_id`, ranked within each list
  by `score` (`descending` option, default true), and the reciprocal-rank
  contributions are summed per item. Options: `k` (default 60), `descending`.
  Output `[item, fused_score]`, composable in further Datalog. Not feature-gated.
  Rationale: Datalog can already *sum* reciprocal contributions but cannot assign
  a *rank position within a group* — that intra-list ranking is the missing
  primitive. Tests: `cozo-core/tests/rrf.rs` (fusion math, `k` smoothing,
  ascending direction, alias, default-k).
- **`MaximalMarginalRelevance` fixed rule (diversity rerank, Bet 1)** —
  `cozo-core/src/fixed_rule/utilities/mmr.rs`, aliased `MMR`. Re-ranks a candidate
  set to balance relevance against diversity (avoids recalling near-duplicate
  memories). Input `[item, relevance, vector]`; greedily selects
  `argmax(λ·relevance − (1−λ)·max cosine_sim to already-selected)`. Options:
  `lambda` (default 0.5, clamped to [0,1]), `k` (default 0 = all). Output
  `[item, rank]` (selection order). Tests: `cozo-core/tests/mmr.rs`.
- **End-to-end hybrid retrieval test** — `cozo-core/tests/hybrid_retrieval_e2e.rs`
  runs a real HNSW (vector) search + a real FTS (keyword) search over one
  relation, fuses with `ReciprocalRankFusion`, then reranks with
  `MaximalMarginalRelevance` — proving the full hybrid path composes, not just
  synthetic ranked lists.
- Next on Bet 1: a higher-level one-call convenience + a LangChain/LlamaIndex
  adapter once the surface stabilises.
- **Pre-release review hardening** (all guarded by tests): RRF/MMR reject
  non-finite (NaN/inf) scores; MMR rejects inconsistent vector dimensions instead
  of panicking and now uses the true max cosine (not a 0 floor) so anti-correlated
  candidates are rewarded; `ulid_timestamp` rejects malformed/non-canonical ULIDs
  (wrong length, invalid char, leading char > 7) instead of silently truncating.
- **ULID functions (`rand_ulid`, `ulid_timestamp`)** — `data/functions.rs`,
  upstream cozo #296. `rand_ulid()` returns a lexicographically-sortable 26-char
  Crockford-base32 ULID (48-bit ms timestamp + 80-bit randomness); sortable string
  IDs are ideal keys for time-ordered agentic-memory scans (unlike random UUIDv4).
  `ulid_timestamp(s)` extracts the embedded Unix-ms timestamp. Tests:
  `cozo-core/tests/ulid.rs` (format, two hand-derivable decode vectors, recency,
  sortability, distinctness).

#### Phase 0 — fixes
- **#1 equality-pushdown for stored relations** (`query/reorder.rs`). Equality
  post-filters on a stored relation — `*rel[k, ..], k == <ground>` and
  `*rel{k, ..}, k == <ground>` — now compile to a keyed `stored_prefix_join`,
  identical to the binding-first form `k = <ground>, *rel{..}`. Upstream left
  these as a full `load_stored` scan + `eq(..)` post-filter. Implemented as a
  pre-pass (`push_equality_filters_to_bindings`) that converts qualifying
  `eq(var, ground)` predicates into unifications and hoists only those converted
  unifications ahead of the relation that produces the variable; the existing
  well-ordering logic then emits the prefix lookup. Pure optimization — result
  sets are unchanged. (NB: the fix is in `reorder.rs`, not `relation.rs::choose_index`
  as originally guessed — `choose_index` only selects *secondary* indices.)
  - **Correctness boundary (numeric grounds are NOT pushed):** `op_eq` treats
    `Int(n) == Float(n)` as equal across types, but a keyed lookup uses the index's
    strict `Num` ordering where `Int(n) != Float(n)`. Converting a numeric equality
    would silently drop cross-type matches, so the conversion is gated to non-numeric
    ground values (str/uuid/bytes/bool/null); numeric `==` keeps full `op_eq`
    post-filter semantics. User-written unifications are never reordered. Guarded by
    `tests/fork_regressions.rs::numeric_equality_keeps_cross_type_semantics`.
  - **Measured** (criterion, SQLite backend, 5000-row relation, single-row PK
    lookup): positional post-filter **1.746 ms → 61.1 µs (~28.6×)**; brace
    post-filter **1.756 ms → 59.4 µs (~29.5×)**; binding-first unchanged
    (~48–51 µs). The speedup scales with row count (O(N) scan → O(log N) lookup).
  - Tests: `tests/fork_regressions.rs::equality_post_filter_uses_prefix_lookup`
    (now active, was `#[ignore]`); baseline bench `benches/point_lookup.rs`.
- **#281 keyword-prefixed identifiers now parse** (`cozo-core/src/cozoscript.pest`).
  An identifier starting with a keyword literal — `nullable_column`, `trueValue`,
  `falsey` — failed to parse in value positions (`*rel{col: nullable_column}`)
  because `term` tries `literal` before `var` and `null`/`boolean` had no word
  boundary, so `null` greedily matched the `null`-prefix and the parse aborted.
  Added an identifier-boundary lookahead (`~ !("_" | XID_CONTINUE)`) to the `null`
  and `boolean` rules. Closes upstream cozo #281. Tests:
  `tests/fork_regressions.rs::keyword_prefixed_identifiers_parse` (includes a guard
  that real `null`/`true`/`false` literals still parse).
- **#287 `env_logger` moved to a dev-dependency** (`cozo-core/Cargo.toml`). It was
  a hard dependency but is only used by `runtime/tests.rs` (cfg(test)). Closes
  upstream cozo #287; trims downstream build graphs.
- `tests/matjoin_regression.rs` — regression guard pinning the #3 fix.
- `benches/point_lookup.rs` — first stable/CI-runnable criterion bench (the
  upstream pokec/wiki/time_travel benches need nightly `#![feature(test)]` +
  external datasets, so they don't run in CI).

### Next (ordered by value/confidence)
- **#4** batch secondary-index writes into a single `WriteBatch` — perf, needs the cozorocks bridge to expose batch put.
- **#7** `multi_get` for HNSW neighbor fetches — perf, needs bridge support.
- **bit-rot (deferred, low value)**: #307 — the upstream pokec/wiki/time_travel
  benches need nightly `#![feature(test)]` + external datasets, so they can't run
  in CI and can't be compile-verified on a stable toolchain; superseded by the
  criterion harness (`benches/point_lookup.rs`). #298 — newer rayon raises MSRV;
  builds fine on our toolchain (rustc 1.93.1, rayon 1.10/core 1.12.1), only bites
  users below rayon's MSRV, not reproducible for us. Revisit if either becomes
  load-bearing.
- **#2** decide + implement the top-level temp-create behavior (warn vs error vs surface scope).
