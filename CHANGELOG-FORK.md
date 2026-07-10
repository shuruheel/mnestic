# mnestic fork changelog

Divergences from upstream CozoDB `481af05` (2024-12-04). See `FORK.md` for
provenance and licensing.

## Unreleased

Post-0.10.7 work not yet cut to a release. Keep this section current as
divergences land (see `CLAUDE.md` release rules) so a release never has to
reconstruct them.

**Cached graph projections.** `::graph create G { edges: knows, nodes: person }`
names an in-memory adjacency over stored relations that twelve graph algorithms
reuse across queries instead of rebuilding on every call:

```
::graph create g { edges: knows, nodes: person }

?[node, group] <~ ConnectedComponents(graph: 'g')
?[node, rank]  <~ PageRank(graph: 'g', iterations: 20)
```

It is **always fresh**. A projection never serves a transaction data that differs
from what that transaction's own scan of the sources would return — in either
direction, so a long-lived reader is not handed an entry newer than its snapshot
either. Writing to a source frees the adjacencies built from it. Under continuous
write churn the cache degrades to build-per-query; it never goes stale.

Measured on a 400,000-edge graph (release build; *cold* is the positional form,
i.e. today's behaviour):

| kernel | cold | warm | |
|---|---|---|---|
| `ConnectedComponents` | 127 ms | 7.9 ms | **16×** |
| `PageRank`, 20 iterations | 150 ms | 10 ms | **15×** |
| `ClusteringCoefficients` | 169 ms | 56 ms | 3× |

What is cached is the *setup* — scanning the edge relation and building the CSR —
so the gain shrinks as the kernel itself dominates: `ClusteringCoefficients`
spends its time counting triangles, and `CommunityDetectionLouvain` rebuilds a
coarsened graph per level internally, so only its first build is reused.

A database that defines no projections pays one atomic load per transaction, one
set insert per mutation *statement* (not per row), and one uncontended mutex
acquisition per *writing* commit. Read-only commits touch none of it.

### Added

- **`::graph create NAME { edges: R, nodes: R2 }`** — `nodes` optional; sources
  may be written bare or quoted. Validated on the spot: the relations must exist
  and `edges` must have arity ≥ 2. Index, temporary and transaction-time
  relations are refused with an explanation. Nothing is built at create time;
  adjacencies materialise per `(direction, weighted)` variant on first use.
- **`::graph drop NAME`** frees the definition and every adjacency built from it.
- **`::graph list`** reports one row per built variant —
  `name, edges, nodes, variant, est_bytes, built_at, last_used` — and one
  null-variant row for a projection that has built nothing yet.
- **`graph: 'G'` on twelve algorithms**, in place of their positional edge
  relation: `ConnectedComponents`, `StronglyConnectedComponents` / `SCC`,
  `PageRank`, `ClusteringCoefficients`, `TopSort`, `BetweennessCentrality`,
  `ClosenessCentrality`, `ShortestPathDijkstra`, `KShortestPathYen`,
  `MinimumSpanningTreePrim`, `MinimumSpanningForestKruskal`, `LabelPropagation`,
  `CommunityDetectionLouvain`. Their remaining positional inputs shift down by
  one, and optional trailing inputs stay optional.
- **`nodes:` makes isolated vertices real.** A vertex named by the node relation
  but by no edge becomes a genuine degree-0 vertex: `PageRank` counts it in `N`
  and ranks it, `ConnectedComponents` emits it as its own component. The vertex
  set is the union of the two relations.
- **A 512 MiB memory ceiling**, enforced on the spot by least-recently-used
  eviction. Set it with `Db::set_graph_projection_capacity(bytes)`,
  `DbInstance::set_graph_projection_capacity(bytes)`, or, from Python,
  `CozoDbPy.set_graph_projection_capacity(bytes)`. `0` turns caching off while
  leaving `::graph create`/`list`/`drop` working. A single variant larger than
  the whole ceiling is built for each query, with a warning.
- **Concurrent cold callers coalesce into one build** per variant rather than
  each building their own.
- **`PageRank` accepts an optional second input naming the vertices**, as
  `ConnectedComponents` already did. Passing it changes the ranks: isolated
  vertices enter `N`, so every rank moves. On LDBC SNB sf1, 694 of 10,620
  persons have no `knows` edge and were previously absent from the ranking
  altogether. Single-input queries are unaffected.

### Changed

- **BREAKING (results): `PageRank`'s default `iterations` is now 20, up from
  10.** Ten was a below-upstream override — the `graph` crate's own default is
  20 — and it is measurably non-convergent: on sf1 the ranks still move by
  `2.1e-4` at iteration 10 against the default `epsilon` of `1e-4`, versus
  `1.6e-6` at iteration 20. **Pass `iterations: 10` to restore the old numbers.**
- **`PageRank` now warns** (`log::warn`) when the `iterations` cap stops it short
  of `epsilon`, instead of silently returning unconverged ranks. `epsilon: 0.0`
  means "run exactly `iterations`" and stays quiet.
- **A `graph:` option on a rule that cannot consume one is now a parse error.**
  Unknown fixed-rule options are ignored engine-wide, so this would otherwise
  silently rebuild the graph the slow way. `BFS`, `DFS`, `ShortestPathBFS`,
  `RandomWalk`, `ShortestPathAStar` and `DegreeCentrality` are excluded by
  design: they evaluate per-tuple condition, heuristic and weight expressions
  against the edge relation, which a compressed adjacency does not carry.
  **If you registered a custom `FixedRule` that reads an option named `graph`,
  override the new `supports_projection()` to return `true`.** `SimpleFixedRule`
  is exempt automatically — it forwards every option to its closure.
- **`PageRank` rejects a positional nodes relation combined with `graph:`** — a
  cached variant's vertex ids are fixed when it is built. Declare them on the
  projection with `nodes:`. `ConnectedComponents` still takes a positional nodes
  relation, as an overlay on top of the projection.
- **`ConnectedComponents` and `SCC` group ids renumber** when a projection
  declares `nodes:` — isolated vertices interleave rather than being appended.
  The partition is unchanged, and the labels always were arbitrary.
- **`MinimumSpanningTreePrim` starts from the lowest vertex with an edge**, not
  vertex 0, so a nodes-bearing projection whose first vertex is isolated still
  spans a tree. A supplied starting relation is unaffected, diagnostics included.
- Errors raised while scanning edges (`algo::not_an_edge`,
  `algo::invalid_edge_weight`) now surface eagerly rather than after the graph
  has been built.

### Fixed

- **An empty edge relation panicked seven graph algorithms**, aborting the
  process: `TopSort`, `ConnectedComponents`, `StronglyConnectedComponents`,
  `ClusteringCoefficients`, `BetweennessCentrality`, `ClosenessCentrality` and
  `LabelPropagation`. They now return no rows, as `PageRank` already did. The
  builder sizes a graph from its largest edge endpoint, and `max` over an empty
  edge list is `0`, so an empty relation produced a *one-vertex* graph with an
  empty id map and the first index aborted. Four of these algorithms already
  carried a `node_count() == 0` guard for exactly this case; it could never
  fire. Present in upstream CozoDB.
- **`multi_transaction` could deadlock the process.** It ran its transaction
  loop on a `rayon` global-pool worker, which the loop then parked in a blocking
  receive for the transaction's whole life. With as many open transactions as
  the pool has workers — one, on a single-core host or under a CPU quota — every
  parallel query in the process blocked forever. It now uses a dedicated thread,
  as its documentation always claimed. Affects every caller, not just graph
  algorithms.
- **An oversized graph wrapped silently.** The CSR indexes vertices and edge
  offsets with `u32`; both counts are now checked before the build
  (`algo::graph_too_large`).
- **Long CSR builds were un-interruptible.** The scan now checks the poison flag
  every 4096 tuples, so `::kill` and `:timeout` abort a large build instead of
  waiting it out. Previously the flag was only observed once the algorithm
  proper began, which on a 300k-edge graph was most of the query.

### API and compatibility

- New public types under `graph-algo`: `VariantSpec`, `VariantKey`,
  `GraphVariant`, `ProjectionVariant`, `GraphSource`, and
  `FixedRulePayload::graph_input`.
- New defaulted trait method `FixedRule::supports_projection` — non-breaking for
  out-of-tree implementations; see the parse-error note above.
- New on `FixedRuleInputRelation`: `as_directed_graph_checked` and
  `as_directed_weighted_graph_checked`, which take an optional node relation and
  a `Poison`. The existing `as_directed_graph` / `as_directed_weighted_graph` are
  unchanged and now delegate to them.
- **`graph` is now a public dependency.** Its adjacency types are re-exported
  under `graph-algo` (`DirectedCsrGraph`, `Graph`, `DirectedNeighbors`,
  `DirectedNeighborsWithValues`), so moving off `graph = "0.3"` is a
  semver-major event for mnestic.
- New internal `test-hooks` feature exposing `Db::set_commit_fence_for_tests`,
  `Db::set_graph_build_fence_for_tests` and
  `Db::graph_projection_builds_for_tests`, which let a test park a transaction
  inside the freshness protocol's race windows. **Not a supported API**; never
  enable it in production.

### Known limitations

- **Projections are not persisted.** After a restart, using one is a loud error
  naming the fix; re-create them at startup.
- **Transaction-time relations cannot be projection sources.** Everywhere else
  in the engine, a selector-less read of a tt-stamped relation means its
  *current belief*, while a projection's raw scan would deliver the whole
  history keyspace, retracted rows included. Plain `Validity` relations project
  fine. Current-belief projections of tt relations may come later.
- **Weighted variants are built permissively**, so one adjacency serves every
  consumer. An algorithm whose result is undefined under negative weights —
  `ShortestPathDijkstra`, `KShortestPathYen`, `CommunityDetectionLouvain`,
  `BetweennessCentrality`, `ClosenessCentrality` — fails loudly if it meets one,
  naming itself and the projection.

Design and correctness argument: [`docs/specs/graph-projection.md`](docs/specs/graph-projection.md)
and the module docs in `cozo-core/src/runtime/graph_projection.rs`. Proven by 58
end-to-end tests, 6 RocksDB interleaving tests that drive the protocol's race
windows directly, and 37 mutations verified to turn those tests red.


## 0.10.7 — 2026-07-08

A plan-quality fix for the 0.10.5 greedy join reorder, plus a Python-facing
binding addition and a docs note. The headline corrects a tie-break that could
demote a full-key filter to a partial-key expansion — pulling a high-fan-out
edge ahead of a selective atom and producing a strictly worse plan on
high-fan-out cyclic joins. Engine (`cozo-core`) plus the Python binding; no
`cozorocks`/`mnestic-rocks` change and **no query-result change** (join reorder
is result-invariant under set semantics).

- **Fix: the greedy join reorder no longer demotes a full-key filter to a
  partial-key expansion.** The greedy tie-break shipped in 0.10.5
  (`query/reorder.rs`) rewarded any atom whose *leading* composite-key column was
  bound. On a `knows{src, dst}`-style composite key, binding only `src` scored as
  if it were a point lookup — but it is a keyed *expansion* over every neighbour
  of `src` (the highest-fan-out relation in a graph), not a filter. So the pass
  could pull a fan-out `knows` edge ahead of a selective membership atom and
  produce a strictly worse plan: an external benchmarker (LDBC-SNB LSQB) measured
  Q3 — a same-country `knows` triangle — go from ~19 s to a >120 s timeout at
  SF0.1, while the tie-break won nothing across the other eight queries at two
  scale factors. The tie-break helper is renamed
  `bound_key_prefix_len` → `full_key_lookup_bonus`: it now rewards ONLY a
  *complete*-key point lookup (all key columns bound — an existence filter that
  matches ≤1 tuple and cannot increase cardinality) and scores a *partial* prefix
  `0`, so a partial-key tie falls back to the written order. **No result change**
  (conjunction is commutative under set semantics), and 0.10.5's ~54.5×
  "min-new-vars" win is preserved — it is driven by the new-vars criterion, not
  this tie-break. Regression-guarded by a new high-fan-out integration test
  (`tests/join_reorder.rs::partial_key_prefix_not_pulled_forward_{sqlite,mem}`,
  which FAILS on the pre-fix engine) plus rewritten unit tests
  (`greedy_prefers_full_key_lookup_on_tie`,
  `greedy_ignores_partial_key_prefix_on_tie`). Anyone relying on the default
  `:reorder greedy` over high-fan-out cyclic joins benefits.
- **Python binding: `set_query_factorization` / `query_factorization` are now
  exposed on `CozoDbPy`.** The 0.10.5 factorized-`count()` rewrite shipped behind
  a Db-wide kill switch (`Db::set_query_factorization(bool)`, default OFF "to
  soak") that was reachable only from Rust `DbInstance`. Python callers can now
  toggle it exactly like the timeout methods that already crossed:
  `db.set_query_factorization(True)` and `db.query_factorization()` (returns the
  current state). Purely additive; the default stays OFF. This lets Python-based
  benchmarks generate the soak evidence the 0.10.5 changelog said default-on is
  waiting on. (`cozo-lib-python/src/lib.rs`.)
- **Docs: an "algebra ⟷ fixed-rule map"** in
  `docs/concepts/semirings-and-fixedrules.md` records which built-in graph
  fixed-rules are expressible as semiring recursion
  (`ShortestPathDijkstra`/`ConnectedComponents` validated identical to `min_cost`
  / min-label forms) and corrects two easy conflations (community
  `LabelPropagation` is NOT a meet; `KShortestPathYen` ≈ `min_cost_k` only
  approximately).

## 0.10.6 — 2026-07-08

An urgent upgrade-safety patch. The headline fixes a data-availability
regression the 0.10.0 bitemporality work introduced: relation catalogs last
written before 0.10.0 (or by an index/rename/destroy path) could fail to open,
taking the whole database down — **anyone who upgraded a pre-0.10.0 database to
any of 0.10.0–0.10.5 should upgrade to 0.10.6.** Two internal items ride along.
Engine (`cozo-core`) plus Python-wheel CI; no `cozorocks`/`mnestic-rocks` change
and no query-behavior change.

- **Fix: relation catalogs written before 0.10.0 no longer fail to open
  ("Cannot deserialize relation metadata from bytes").** The bitemporality
  work (0.10.0) inserted `RelationHandle::tt_gc_floor` *mid-struct*. rmp_serde
  encodes structs positionally on the pre-`with_struct_map` catalog-write
  paths, and `#[serde(default)]` only rescues a *missing trailing* element — so
  every relation whose catalog was last written as a 13-field positional array
  (any graph created before 0.10.0, or updated by an index/rename/destroy path)
  failed to deserialize on open, taking the whole database down. This silently
  took down a production multi-tenant deployment on its 0.10.0 upgrade. Two-part
  fix: (1) `tt_gc_floor` moved to the **last** field of `RelationHandle` so the
  trailing default applies to legacy arrays; (2) the seven catalog-rewrite paths
  (`::index`/HNSW/FTS/LSH create, relation rename, index destroy) now serialize
  with `.with_struct_map()` like the create path, so catalogs are uniformly
  self-describing maps and future field additions can't reintroduce this class
  of bug. No migration: legacy arrays stay readable, and re-canonicalize to
  maps on their next write. Regression-guarded by a real pre-0.10.0 `edge`
  catalog fixture (`runtime/relation.rs::catalog_compat_tests`).
- **Greedy join reorder is now a pure function over a resolved `SchemaView`.**
  Internal refactor of the deterministic join-reorder pass shipped in 0.10.5
  (`query/reorder.rs`): the reorder no longer reads mutable planner state,
  making it independently unit-testable. No query-plan or behavior change.
- **Python wheel CI hardened for `storage-rocksdb`.** The x86_64 manylinux leg
  now builds on `manylinux_2_28` and installs `libclang` (`clang-devel`) so
  zstd-sys's bindgen resolves; the aarch64, macOS and Windows legs are
  unaffected. Wheel-build only — no engine change.

## 0.10.5 — 2026-07-07

A liveness + performance release responding to an external
Ladybug-vs-mnestic benchmark. Two themes: queries you can always stop
(interruptible `::kill`/`:timeout` + a per-query wall-clock budget), and
naively-ordered queries that stop being pathological (a deterministic join
reorder + an opt-in factorized-count rewrite). Engine (`cozo-core`) plus the
Python binding and its wheel CI; no `cozorocks`/`mnestic-rocks` change.

- **`::kill` and `:timeout` now actually interrupt a running query.** Two
  defects the benchmark surfaced: (1) `::running`/`::kill` opened a storage
  transaction before dispatching, so on the mem/sqlite backends a `::kill`
  queued behind the very read query it was trying to kill and blocked for that
  query's entire remaining runtime — they now dispatch before any transaction
  (they touch only the in-memory running-query registry). (2) The poison flag
  was only checked between rule applications, never inside the relational-algebra
  enumeration, so a naive single-rule join that yields no output for a long time
  went uninterruptible — the per-query `Poison` is now threaded through
  `RelAlgebra::iter` and checked every 4096 pulls at every operator boundary and
  raw scan (the signature change makes coverage compiler-enforced). Overhead is
  within noise on the `point_lookup` baseline.

- **Per-query wall-clock budget.** A query can carry a deadline three ways —
  the in-script `:timeout <secs>` option, a per-call
  `Db::run_script_with_options(payload, params, mutability, ScriptRunOptions { timeout })`,
  and a Db-wide default `Db::set_default_query_timeout(Option<f64>)`. The
  effective deadline is the **minimum** of whichever are set, computed once per
  `run_script` call and shared across every statement of an imperative/multi-
  statement script and any triggers it fires — a `:timeout` (or per-call
  timeout) can only *tighten* the budget, never extend past the Db default.
  Expiry raises a distinct **`eval::timeout`** diagnostic (a `::kill` still
  raises `eval::killed`). `Poison` gained an `Option<Instant>` deadline and lost
  its per-timed-query detached timer thread — no more thread leak — riding the
  interruptibility fix's 4096-pull check cadence so it aborts promptly inside
  long enumerations. Absurd or non-finite budgets (`:timeout 1e300`, an HTTP
  `timeout` of `1e400`) clamp to "no deadline" (still `::kill`-able) instead of
  panicking. On wasm, which has no `std` monotonic clock, a query carries no
  wall-clock budget rather than panicking on `Instant::now()`. Exposed on
  `DbInstance` and in cozo-bin (`timeout` field on the HTTP query payload;
  `--default-query-timeout` CLI flag). A budget-aborted mutable script rolls
  back with no partial commit. **In-tree breaking:** `Poison` is now a
  named-field struct — use `Poison::check()`/`Default`, not `.0`.

- **Deterministic greedy join reorder** (default ON; `:reorder written` opts
  out; spec `docs/specs/join-reorder.md`). No pass considered join order, so a
  naively-ordered conjunction — exactly what an LLM agent authors — could spin
  on an N³ intermediate (the benchmark's members-first same-group triangle). A
  stat-free min-new-vars greedy pre-pass (`query/reorder.rs`, after the #1
  equality-pushdown) reorders the positive relation atoms of an eligible
  conjunction (measured 54.5× on the repro; N³→N²). Eligible = ≥3 stored
  `Relation` atoms, no rule-application or Hnsw/Fts/Lsh atom, no multi-valued
  `in`-unification, and not a bare `:limit` without `:sort`. Results are
  unchanged: conjunction is commutative under set semantics, the binding-before-
  use pass remains the correctness arbiter (with fallback to written order if a
  reordered body fails to compile), and the pass is the **identity** on any
  stepwise-greedy-consistent written order, so hand-tuned plans stay byte-
  identical. The multi-valued-`in` exclusion is load-bearing — that construct is
  a multiplicity injector (generator vs. filter by position) that would
  otherwise silently change a non-idempotent aggregation (`count`/`sum`/
  `collect`). A residual Cartesian step (a genuinely disconnected conjunction)
  is `log::warn!`-ed and annotated `<op> (cartesian)` in `::explain`.

- **Automatic factorized `count()` rewrite** (opt-in, **default OFF**; spec
  `docs/specs/cardinality-algebra.md`). `count()` over a join streams every
  match (O(#matches)); the benchmark measured 4–342× vs an optimizer that counts
  without enumerating. A normal-form pre-pass (`query/factorize.rs`) rewrites an
  eligible single-clause `count()`-over-positive-join into Yannakakis-style
  per-key counting sub-rules — a bit-identical (exact-i64, `Int`-typed) answer
  computed without materializing the join. It fires only on shapes it can prove
  exact (declining cyclic, non-free-connex, repeated-variable, negated,
  recursive, bitemporal, `count_unique`/mixed-aggregate, and any body with a
  `!=` predicate) and
  accumulates in an internal `int_sum_prod` aggregate that **errors, never
  wraps,** on overflow. Behind a Db kill switch `set_query_factorization(bool)`,
  default OFF this release to soak before any default-on consideration; verified
  by a 400-case differential harness (naive vs factorized, mem + sqlite, exact
  row+type equality). An always-on companion detector surfaces a factorization
  advisory in `::explain` / `log::info!`.

- **Bulk `import_relations` into an index-bearing relation now warns.** The bulk
  path maintains B-tree secondary indexes but not HNSW/FTS/LSH, so imported rows
  silently stay invisible to vector/text search until the index is rebuilt — a
  `log::warn!` now flags it (a warning, not a hard error: consumers legitimately
  import a snapshot then reindex).

- **RocksDB in the PyPI `mnestic` wheel.** `CozoDbPy("rocksdb", path)` failed
  from `pip install mnestic` because the wheel build pinned `features=compact` —
  the binding already forwarded the engine string, so this was purely a build-
  feature gap. Wheels now compile `storage-rocksdb` on all five platform legs
  (feature list moved onto the maturin CLI as a per-leg matrix knob; mac legs
  pin `MACOSX_DEPLOYMENT_TARGET=11.0`), gated by a new `test-wheels` smoke job.
  The sdist stays compact (no rocksdb sources), so the persist engine is
  wheel-only.

- **Python binding: interior-mutable `CozoDbPy`.** `close()` took `&mut self`
  while an in-flight `run_script(&self)` held a shared PyCell borrow, so a
  concurrent `close()` raised "Already borrowed". The handle is now
  `RwLock<Option<DbInstance>>`; every method takes `&self` and reads clone the
  Arc-backed `DbInstance` out of a momentary guard, released before the blocking
  engine call. `run_script` gains an optional `timeout=None` kwarg plus
  `set_default_query_timeout`/`default_query_timeout`.

- **New spec: `docs/specs/cardinality-algebra.md`** — the manual factorized-
  counting authoring patterns (join-tree count DP, star product, `!=` and
  anti-join inclusion-exclusion) with their exactness conditions, the reference
  for shapes the automatic rewrite declines.

## 0.10.1 — 2026-07-06

A small additive release on top of 0.10.0 — two new query primitives (the
antichain/skyline aggregate and interval predicates) plus a correctness fix to
the `bit_and`/`bit_or` meet aggregates. Pure `cozo-core`; no
`cozorocks`/`mnestic-rocks` change.

- **Follow-up review fixes (0.10.0 hardening)**: (1) `bit_and`/`bit_or` meet
  aggregates now report whether the value actually CHANGED — the byte loop
  returned true unconditionally, the same defect family as the 0.10.0
  `and`/`or` inverted-changed-bit fix, so a non-changing fold (e.g.
  `[0xF0] & [0xFF]`) re-entered the semi-naive delta every epoch. (2) The
  bounded-meet divergence cap counts TOTAL changed epochs instead of a
  consecutive streak that reset on quiet epochs. Behaviorally identical
  today: the stratifier poisons every cross-rule edge into or out of an
  aggregated rule except direct self-recursion (in-SCC poisoned edges are
  rejected as unstratifiable — pinned by
  `bounded_meet_relay_recursion_unstratifiable` — and cross-SCC ones are
  forced across a stratum boundary) and exempts aggregated rules from
  magic-set rewriting, so a bounded rule's only in-stratum input is its
  own delta and its changed epochs form a contiguous prefix. The total
  count keeps the guard sound if the isolation is ever relaxed — a
  displacement cycle improving the k-set only every other epoch evades a
  resetting streak forever. (3) `bit_and`/`bit_or` `init_val` documented as
  a lazy identity: the true ⊕-identity (all-ones for `bit_and`, all-zeros
  for `bit_or`) has runtime-determined width, so empty bytes is a seed
  sentinel consumed by `update`'s first-contact branch, not the algebra's
  identity element.

- **Dominance bounded-meet registration — the antichain / skyline aggregate**
  (spec: `docs/specs/antichain-bounded-meet.md`, signed off + implemented
  2026-07-04): `register_bounded_meet_aggr(name, dominates, max_survivors)`
  opens the bounded-meet category (R1's recorded deferred item) to a
  host-registered strict partial order. The head form `name(operand)` keeps,
  per group, the non-dominated set of operands — each survivor its own
  output row — riding `AggrKind::BoundedMeet`, the stratifier permit, and
  the 4096-epoch divergence cap exactly like `min_cost_k`. New
  `DominanceMeetStore` (BNL in-buffer insert: structural-equality dedup,
  reject-if-dominated, multi-removal eviction; survivors kept in memcmp
  order so output is canonical, not arrival-dependent; equality-dedup-only
  delta twin). `max_survivors` is a mandatory resource guard — overflow is
  a loud error, never a silent truncation (an antichain has no canonical
  k-subset). Debug builds probe irreflexivity + asymmetry of the registered
  closure; call-site args are rejected at parse time (the cap lives in the
  registration). Trigger scripts keep rejecting custom aggregates at
  `::set_triggers`. Name reservation now also covers builtin FUNCTION names,
  retrofitted onto `register_custom_aggr` (one token, two semantics — the
  `coalesce` lesson). Rust-embedded-only v1 (host closures do not cross the
  Python/served surfaces; see the spec §5). Pinned by the §6 matrix in
  `cozo-core/tests/antichain_bounded_meet.rs` — Pareto frontier, recursion
  with a cycle, permuted-input confluence, cap overflow, lawless-closure
  probes, registration policy, persistence-without-registration.

- **Interval primitives** (spec: `docs/specs/cozoscript-extensions.md` §3.4 v1):
  `interval_overlaps(a, b)` builtin function and `interval_coalesce(span)`
  aggregate over half-open `[start, end)` list intervals. Deliberately plain
  list utilities decoupled from the vt axis (point-event `Validity` storage is
  unchanged). Touching spans do not overlap but do coalesce
  (`[0,5)` + `[5,10)` = `[0,10)`); empty spans `[x, x)` overlap nothing;
  mixed int/float bounds compare numerically (not by `Num`'s storage order);
  malformed spans (start > end, non-numeric or NaN bounds, non-list operands)
  are loud errors, never silent falses.
  `interval_coalesce` is the spec-mandated rename away from the original
  `coalesce` proposal, which silently collides with the shipped null-coalescing
  builtin / `~` operator. Pinned by `cozo-core/tests/spec_doc_validation.rs`,
  which also pins every validated listing in the companion spec.

## 0.10.0 — 2026-07-04
The release has two pillars, in order of impact:

1. **Bitemporality** — engine-assigned transaction time alongside Cozo's
   valid time. System-versioned (`tt: TxTime`) and fully bitemporal
   (`vld: Validity, tt: TxTime`) relations: a crash-safe monotone commit
   clock stamps every write; reads default to the current belief and
   time-travel with `@ (vt: …, tt: …)` or `:as_of`; existence-checking
   writes target the resolved current belief; `::history` /
   `::history_gc` (persisted floor) / `::evict` (audited hard deletion)
   manage the record's lifecycle; and a measured performance gate keeps
   current-belief reads within ~4–12% of the single-axis baseline.
   "What did we believe at time T about period Y" — in-engine.
   Spec: `docs/specs/bitemporality.md`.
2. **Provenance semirings** — the same recursive rules compute existence,
   cost, confidence, or evidence: `register_custom_aggr` for user-defined
   absorptive combines in recursion; `min_cost_k` bounded-meet aggregates
   returning the k best derivations per answer WITH the evidence chains
   that justify them; annotation persistence (resolved: no format change
   needed); and `:reconcile` — recompute-based belief revision that keeps
   derived annotations consistent under base-fact retraction, composed
   with the tt axis. Spec: `docs/specs/provenance-semirings.md`.

Plus **four upstream CozoDB bugs fixed** along the way: the inverted
changed-bit in `and`/`or` meet aggregates, a panic on negated validity
atoms, wrong answers from prefix-truncated temporal-column joins, and the
braced-`%return` imperative parse panic.

Detailed entries below, pillar by pillar.

### New — transaction-time commit clock (bitemporality step 2; internal, no user surface)
- The engine-assigned transaction-time (tt) allocator for the bitemporality
  work (`docs/specs/bitemporality.md` §5, decisions §13.10): a wall-clock-floored
  strictly monotonic commit counter `tt = max(now_µs, last_tt + 1)` held as an
  `AtomicI64` high-water mark on `Db` (`runtime/tt_clock.rs`), seeded at open
  from `max(persisted mark, wall clock)` and persisted as a system key
  (`[Null, "TT_HWM"]` under `RelationId::SYSTEM`, the `STORAGE_VERSION` idiom)
  **inside the committing transaction** — no crash window between advancing and
  persisting. `SessionTx::commit_tx_with_tt` allocates under a per-`Db`
  critical section so tt order == commit order == visibility order; values
  advanced by transactions that abort are burned, preserving monotonicity.
  Hardened by adversarial review: the HWM put goes through a new
  `StoreTx::put_externally_serialized` (RocksDB override clears the
  pessimistic transaction's begin-snapshot for this one key — otherwise any
  two temporally-overlapping tt commits would make the later one abort with
  `Resource busy`, the 0.8.4 `avgdl` hot-key failure mode; default impl is a
  plain put for sqlite/mem, whose storage-level write locks preclude
  overlapping write transactions). Seeding is monotone (`fetch_max`, so
  re-`initialize` and the step-3 restore re-seed can never regress the
  authority); a malformed persisted mark refuses to open (loud, mirroring the
  version-mismatch bail) instead of silently degrading to wall-clock seeding;
  the commit section recovers from mutex poisoning (sound: a mid-section
  panic only burns a never-committed value); the wall-clock read is
  cfg-guarded for wasm32 like `current_validity`. Nothing in the write path
  calls the clock yet — step 3 (schema opt-in + buffered stamping) routes
  tt-stamped commits through it, and **owes**: the restore/import re-seed
  (`max(persisted, max restored tt, wall clock)` after `restore_backup`) and
  the HWM+data-rows same-tx atomicity test. Pinned by tests: same-µs strict
  monotonicity, backward-clock step, concurrent per-caller monotonicity +
  global uniqueness, restart re-seeding from the persisted mark (sqlite),
  corrupt-mark open refusal, abort-doesn't-persist, mem-backend operation,
  and overlapping-commit non-conflict on RocksDB.
### New — TxTime relations: schema opt-in + write path (bitemporality step 3)
- **`TxTime` column type** (`docs/specs/bitemporality.md` §4): a relation whose
  last key column is `tt: TxTime` is transaction-time-stamped — tt-only
  (`{k, tt: TxTime => v}`, system-versioned) or bitemporal
  (`{k, v: Validity, tt: TxTime => x}`). The engine stamps every write with the
  commit clock's tt at commit; user-supplied tt values are rejected on every
  path (put specs, `:create`-with-rows headers, imports). **Relations declaring
  `TxTime` are unreadable by pre-fork/upstream builds.**
- **Temporal-axis rule enforced at `:create`** — axes are the trailing key
  columns, vt-then-tt, at most one of each; seven malformed shapes are
  `:create`-time errors whose message contains the copy-pasteable corrected
  declaration (deliberately stricter than `Validity`'s shipped query-time-only
  check).
- **Buffered commit-time stamping**: writes to tt-stamped relations buffer on
  the transaction and are stamped + written inside the commit critical section,
  so the whole transaction shares one tt (one belief event) and tt order ==
  commit order. Consequences, all pinned by tests: same-transaction writes are
  **not visible to later reads in the same script**; a key cannot be both
  asserted and retracted in one transaction (unbreakable resolution tie —
  rejected across statements, including tt-only `:put`+`:rm` mixes); double-puts
  of one (key [, vt]) in one transaction collapse last-write-wins; `::remove` of
  a relation with pending tt writes in the same script is rejected.
- **`:rm` on tt-only relations appends a retraction** at commit-tt (never a
  physical delete); values snapshot the key's latest row at statement time;
  `:rm` of a missing key is a no-op; `:delete` of a missing or believed-deleted
  key fails its existence assertion. **Deviations recorded:** on *bitemporal*
  relations `:rm` is rejected in v1 (removal is a valid-time statement — use
  `:put` with vt `"RETRACT"`; the spec-§6 remap is deferred to step 4);
  `:insert`/`:update`/`:ensure`/`:ensure_not` are rejected pending the as-of
  read path (step 4); `:replace` of an existing TxTime relation is rejected
  (destroy-and-recreate would silently drop history); triggers, secondary and
  search indexes, callbacks, and `:returning` are rejected on TxTime relations
  (indexes: step 5). A put-trigger on a *plain* relation may write into a
  TxTime relation (buffers and stamps normally; pinned).
- **Imports**: `import_relations` stamps **one tt per batch** (one belief
  event; spec §13.3), rejects tt columns in payload headers and delete-imports;
  `restore_backup` preserves tt bytes verbatim and **re-seeds the commit
  clock** from the backup's persisted mark; `import_from_backup` is rejected
  for TxTime relations (its rows would carry a foreign clock's tts — restore
  the full store or re-ingest). Export→import therefore does not round-trip
  TxTime relations; use backup/restore.
- **Interim read behavior**: bare scans work (all versions, newest tt first —
  the tt column decodes as a Validity value; use `:order` for chronological
  output); any `@` as-of read on a TxTime relation fails with a distinct
  "lands in step 4" error, so vt semantics never silently apply to the tt axis.
- Closes step 2's owed items: the restore re-seed and the HWM+rows same-tx
  atomicity test (an aborted transaction leaves neither rows nor a persisted
  mark). 24 txtime/tt_clock tests; hardened against a three-lens adversarial
  review (cross-statement conflict detection, `::remove`-with-pending-writes
  orphan bytes, believed-deleted `:delete` consistency were review catches).

### New — system-versioned relations complete: tt-only reads (bitemporality step 4a)
- **The labeled temporal selector** `@ (vt: …)` / `@ (tt: …)` / order-free
  `@ (vt: …, tt: …)` parses on every relation-access form; bare `@ E` still
  means valid time, everywhere, forever. tt tokens: `"NOW"`/`"END"` =
  end-of-tt-time (current belief — deliberately not the wall clock), numeric
  µs, ISO-8601 (bare dates now accepted as midnight UTC — on the vt axis too;
  strict RFC3339 previously rejected the docs' own `@ "2026-01-01"` examples).
- **tt-only relations are now fully readable**: the default read is the
  CURRENT STATE (one seek per key; believed-deleted keys absent) — replacing
  step 3's all-versions interim scan and completing the §4 migration
  invariant: adding `tt: TxTime` to a relation changes no existing query's
  results. `@ (tt: T)` reads the state as of any past commit time. Rides the
  existing single-axis skip-scan; fixed-rule inputs resolve the same way.
  (Bitemporal relations gained their reads in step 4b below — migrating vt
  relations to vt+tt is now supported.)
- **Fixed: negation against versioned scans panicked** (`unreachable!()` in
  `NegJoin`) — with the current-state default this would have made every
  `not *audit{…}` against a tt-only relation a crash that poisons the Db
  handle; `StoredWithValidityRA` gained `neg_join` (skip-scan mirror of the
  stored one). This also fixes the **pre-existing upstream panic** on negated
  vt atoms with `@` selectors (`not *rel{k @ 'NOW'}`).
- Also: nullable `Validity` is rejected when `TxTime` is declared (the 4b
  resolution has no semantics for a null vt); `choose_index`'s validity flag
  is honest for tt-stamped reads (inert until step 5 legalizes indexes on tt
  relations); a projected tt column currently renders as a `[ts, flag]` pair —
  timestamp-only rendering arrives with `::history` (step 5).

### New — bitemporal reads complete: the two-level (vt, tt) resolution (bitemporality step 4b)
- **Bitemporal relations are now fully readable.** The §3 resolution algorithm
  is live: per key, vt-groups are walked newest-first from the selected valid
  time, each group resolved to its greatest `tt ≤ T` belief **across both
  is_assert runs** (a later-recorded cessation at the same vt is never
  shadowed by the assert run); assertions answer, retractions mean
  believed-deleted (no shine-through), empty-at-T groups fall through; equal
  `(vt, tt)` ties resolve to the assertion. All four §4 selector forms work:
  bare scan = every vt record at current belief (retract rows included — the
  migration invariant: a vt relation's results are unchanged by adding
  `tt: TxTime`, pinned comparatively); `@ V` = the belief now about V;
  `@ (tt: T)` = the whole relation as it stood at T; `@ (vt: V, tt: T)` = the
  full quadrant. Implemented as a probe-driven scan (`data/bitemporal.rs`)
  with a generic default over every backend; per-backend seek-loop overrides
  are step-6 work (measured ~5x a plain scan on sqlite for correction-heavy
  data — acceptable until then).
- **`:as_of <t>`** — one query option pinning the default transaction time for
  every tt-stamped relation atom in the block that lacks an explicit
  selector (explicit wins; plain/vt relations untouched; using it in a query
  that references no tt-stamped relation is an error). The spec's #1 use case:
  re-run a report exactly as it would have answered at T.
- **Fixed: joins binding temporal columns silently truncated resolution** —
  a prefix join whose join columns reached into the trailing temporal key
  columns clamped the scan to one version: superseded/ceased values were
  resurrected on sqlite and mem panicked (`BTreeMap::range` inversion).
  Dispatch now falls back to a materialized join over the resolved scan
  whenever the join prefix leaves the plain key columns — **including the
  pre-existing upstream variant on vt-only relations** (`@`-selected joins
  binding the Validity column had the same wrong answers/panic since before
  the fork). Defensively, the bitemporal probe treats out-of-range bounds as
  exhausted. Pinned on mem and sqlite.
- Recorded deferrals: the unquoted-date parse lint (no warning channel in the
  engine yet) and the §6 write ops owed by step 3 (`:insert`/`:update`/
  `:ensure`/`:ensure_not`/bitemporal `:rm` remap) move to **step 4c**, now
  unblocked by the read path; error messages updated to say so.

### New — existence-checking writes on TxTime relations (bitemporality step 4c)
- The §6 write ops owed by step 3 are live, all evaluated against the
  **resolved current belief** ((vt=NOW, tt=current) on bitemporal relations):
  `:insert` (tt-only: current-belief absence — re-inserting a believed-deleted
  key is legal; **bitemporal: no records at any valid time** — a NOW-only gate
  would let an "insert" silently rewrite past or future vt-groups; duplicate
  keys within one statement rejected); `:update` (merges provided value
  columns over the current belief; the correction lands in that belief's own
  vt-group; binding the vt column is rejected — use `:put` to correct a
  specific version); `:ensure`/`:ensure_not` (assertions about the current
  belief; binding vt or tt is rejected — a silently retargeted assertion is
  worse than none; a key rewritten by the same transaction is an ambiguous
  target and errors; pending writes/removals count as existing for
  `:ensure_not`); and the **bitemporal `:rm {k, vt}` remap** — a cessation:
  buffers a vt-retraction with values copied from the belief at that valid
  time (no belief → no-op; `:delete` asserts one exists). One belief event
  per transaction throughout: writes and existence-checks of one key cannot
  mix in one transaction.
- `:replace` on TxTime relations stays rejected (destroy-and-recreate would
  drop history); triggers/indexes/callbacks remain step-5 work.

### New — bitemporal system operations (bitemporality step 5)
- **`::history rel [[k]…] [limit] [offset]`** — the introspection surface: every
  (vt, tt) record of the given keys, raw. Columns `keys…, vt_ts, op, tt,
  values…` (`op` ∈ assert/retract; `vt_ts` absent on tt-only relations; both
  timestamps as integer µs); ordering key-asc, vt-desc, tt-desc. Errors on
  non-TxTime relations.
- **`::history_gc rel <cutoff-tt>`** — drops superseded records below the
  cutoff while preserving, per (key, vt-group), exactly the record the
  resolution would pick at tt = cutoff — so as-of reads at or above the
  cutoff are unchanged. Persists a per-relation **gc floor**: an as-of read
  below it errors instead of silently returning a post-hoc reconstruction as
  if it were the historical belief. Read-only-guarded. *(v1 runs in one
  transaction; chunked online gc is deferred until real store sizes pull it.)*
- **`::evict rel [[k]…] [unredacted]`** — hard-deletes every record of the
  given keys (the one deliberate break of append-only, for GDPR), writing an
  audit row (relation, key marker, eviction tt, rows deleted) to the reserved
  `mnestic_evict_audit` relation **in the same transaction**. The key marker
  is a **salted hash** by default — storing the key itself would re-enshrine
  the PII the eviction removes; `unredacted` opts out. Read-only-guarded.
- Recorded deviation: **B-tree index legalization on TxTime relations is
  deferred** (spec §10 step 5 listed it) — statement-time index maintenance is
  structurally incompatible with buffered commit-time stamping, and there are
  zero consumers; the rejection message now says so without a step number.
- Adversarial-review hardening (all empirically confirmed before fixing):
  `::evict`/`::history_gc` bail when the same transaction holds pending tt
  writes for the target relation (they'd be stamped after the deletes and
  resurrect the evicted keys); imperative-only `{::evict}`/`{::history_gc}`
  programs now get a write transaction + per-relation locks (previously an
  error on RocksDB, an unlocked mutation elsewhere); all three ops enforce
  access levels (history ≥ read_only, gc/evict ≥ normal); the
  `mnestic_evict_audit` name is enforced as reserved (a pre-existing relation
  with a divergent schema, indices, or triggers is rejected — raw audit puts
  would corrupt/diverge it); duplicate keys in one `::evict` no longer
  overwrite the audit row with `rows_deleted = 0`; `::history`/`::evict` keys
  coerce through the column types (a mistyped key errors instead of silently
  matching nothing); `::history_gc` reports the *effective* floor, refuses
  future cutoffs, and no longer raises the floor on a no-op run (nothing
  deleted ⇒ every read below the cutoff is still exact — and the floor is
  irreversible); its keeper tie-break now reads the vt flag on bitemporal
  relations (the tt flag byte is reserved-0 there) and bails on a corrupt vt
  column instead of silently merging adjacent groups; `::history` output is
  key-ascending, rejects header collisions with user columns named
  `op`/`tt`/`vt_ts`, and its limit/offset are strict `pos_int` tokens (`2 -1`
  no longer silently parses as the single limit `2 - 1`); the burned audit tt
  is covered by the persisted clock HWM (evict transactions commit through
  the tt path).

### Perf — temporal-read budget: pinned-cursor bitemporal scans + the bench gate (bitemporality step 6)
- **`benches/time_travel.rs` rewritten** from the nightly-only `#![feature(test)]`
  relic into a stable criterion bench (registered `harness = false`;
  `autobenches = false` keeps the remaining nightly relics pokec/wiki_pagerank
  from breaking a bare `cargo bench`). Matrix per §9: versions-per-key
  (1/10/100) × corrections-depth (0/2), point reads + full-scan aggregation +
  an as-of-past-tt read, against the named baseline "the identical workload on
  a vt-only relation at `@ 'NOW'`", plus tt-only parity and a plain
  non-temporal reference. Setup sanity-asserts the gate cells answer
  identically. `MNESTIC_BACKEND=mem|rocksdb` selects the backend.
- **The generic probe default measured 4–8× the baseline on scans** (a fresh
  `range_scan` per probe: statement prepare / iterator construction
  dominated). Three step-6 changes brought it inside or near the §9 envelope:
  - **per-backend pinned-cursor overrides** of `range_bitemporal_scan_tuple`
    (sqlite: one prepared statement, reset+rebind per seek; rocksdb: one
    pinned iterator) driven through a shared `HybridProbe` — cache-hit →
    one speculative sequential `step()` → real seek, with a `far` hint from
    the walk so positional skips (past a whole key/group) seek directly;
  - **byte-spliced probe bounds** in `BitemporalIter` (a `Validity` key
    component is exactly 10 bytes) instead of tuple re-encoding, and landings
    decode only the two temporal axes — the full tuple is decoded only for
    emitted rows;
  - **landing reuse**: a landing that already answers the next (monotone)
    bound is reused without touching the backend.
- **Measured (medians, 1000 keys; sqlite / rocksdb; end-to-end `run_script`
  incl. parse — the same basis as the baseline and the AeonG envelope; the
  storage-layer-only delta is proportionally larger)** — point reads at
  (vt: NOW, current belief), the §10 fast-path-parity gate: **+3.8–8.8% /
  +8.2–11.5%** vs the vt-only baseline (≤ ~10% ✓). tt-only current reads:
  at-or-below baseline on both backends (parity ✓). Non-temporal relations:
  untouched dispatch (zero by construction). Full scans: v1 **beats the
  baseline ~2×** on both backends (the sequential walk out-runs the skip
  scan's per-key seeks); deeper version counts run over — c0 +21–53%, c2 up
  to ~2× — a **recorded deviation**: the two-level walk has a structural
  floor of two backend probes per key (assert + retract run) where the
  single-axis scan needs one, and corrections are physical rows the vt-only
  baseline cannot even represent. Revisit only if a real scan-heavy workload
  on deep-version relations shows up.

### Fixed — `::history` output order now matches spec §7 (step-5 follow-up)
- Rows were emitted in physical scan order, which interleaves a vt-group's
  assert and retract RUNS — a belief timeline read top-to-bottom misordered
  cessations against corrections (surfaced by the R3 review). Output is now
  key-asc, vt-desc, tt-desc as §7 documents.

### New — custom aggregate registration (provenance semirings R0b)
- **`Db::register_custom_aggr(name, is_meet, factory)` / `unregister_custom_aggr`**
  (+ `DbInstance` dispatchers): register a user-supplied ⊕ operator
  (`MeetAggrObj`, re-exported with `NormalAggrObj`/`RegisteredAggr`) usable in
  rule heads by name — the registration slot of the provenance-semirings plan
  (`docs/specs/provenance-semirings.md` §5 R0b). With `is_meet = true` the
  aggregate is admitted into **recursive rules**, riding the existing
  stratifier guard and `changed`-bit saturation with zero stratifier change;
  the ⊕ must then be an absorptive semilattice operation (the registrant's
  obligation; a **debug-build probe** in the meet path re-applies operands on
  custom aggregates and panics on observed non-idempotence). Outside recursion
  a custom aggregate runs through a derived normal-path adapter (state = 0̄,
  set = ⊕). Registry is in-memory and `Db`-scoped (persistence is R2); builtin
  names are reserved; duplicates error (unregister to replace — already-parsed
  programs keep their factory); names must be lowercase identifiers; custom
  aggregates take no arguments in R0; ⊗ stays ordinary rule-body arithmetic.
  Factories and operators must not panic (no `catch_unwind` in the engine) and
  factories must be cheap (called O(rules × epochs) per query).
- **Custom aggregates are rejected in trigger scripts** at `::set_triggers`
  time (a trigger is persisted CozoScript re-parsed on every write; a fresh
  `Db` open would lack the registration — unsupported until R2).
- **Breaking (fork-internal API):** `parse::parse_script` gains a
  `custom_aggrs` parameter.

### New — bounded-meet aggregates: `min_cost_k` top-k proofs (provenance semirings R1)
- **A third aggregate category, `AggrKind::BoundedMeet`** — the genuinely-new
  engine work of the semirings feature (spec §6/§9.4): a recursive aggregate
  that keeps **up to k rows per group**, so a query returns the k best whole
  derivations for each answer instead of one. Rows flow through recursion as
  ordinary tuples (⊗ stays rule-body arithmetic, exactly like `min_cost`),
  while the ENGINE owns truncation at every fixpoint step: the new
  `BoundedMeetStore` insert-sorts each candidate under the aggregate's total
  order, deduplicates on `Ordering::Equal` (the `○=` equivalence), and
  truncates to k. NOT the meet path — displacement means rows can leave the
  store, which the idempotent-semilattice assumptions of `Meet` never allow.
- **Shipped instance: `min_cost_k([payload, cost], k)`** — the k lowest-cost
  packs per group, one output row each, cost-ordered (ties break on the whole
  pack; exact duplicates collapse). K-shortest-paths is the direct idiom:
  the `min_cost` recursion with `min_cost_k(pack, k)` in the head. The
  finance/audit shape: "the k most-likely paths plus the exact evidence
  chains that justify them".
- **Convergence guard**: the changed-bit is only a saturation check for
  non-idempotent tags, so the evaluator bails after 4096 CONSECUTIVE epochs
  in which some bounded k-set changed — catching cost-decreasing cycles
  loudly instead of hanging (review must-fix: an earlier stratum-wide cap
  falsely killed converged bounded rules co-stratified with unrelated long
  recursions; the consecutive-change counter caps only live divergence).
  Known limit: a legitimate bounded recursion deeper than 4096 epochs also
  trips the cap (the error says so).
- Semantics documented: **Scallop-style approximate top-k** — upstream
  truncation prunes derivations, and candidates already consumed by earlier
  epochs are not retracted; the k-set at fixpoint = the k best candidates
  ever surfaced. Cost-order of the k rows is guaranteed only when the
  bounded aggregate is the entry head (downstream rules re-sort into value
  order). NaN costs are admitted and deterministically rank worst.
- v1 restrictions (validated with a loud error): the bounded aggregate must
  be the single aggregated column, in the last head position; mutual
  recursion between bounded rules stays unstratifiable; custom registration
  of bounded-meet operators is deferred (the R0b registry still only takes
  meet/normal aggregates).
- Adversarially reviewed: 16-scenario probe battery (relay displacement,
  DAG-exactness vs brute force, zero-cost cycles converge, equal-cost
  lexicographic divergence hits the cap, stratification shapes, `::explain`,
  `:limit`/`:order` post-application) — all sound after the must-fix.

### Resolved — annotation persistence needs no storage-format change (provenance semirings R2)
- The spec anticipated persisting semiring tags via a row-format change. The
  tags-as-columns architecture (R0/R1) made that unnecessary: annotation
  values are ordinary `DataValue`s, so **`:put` of an annotated query output
  already persists them** in the existing memcomparable row format — the R2
  acceptance criterion ("an annotated derivation is materialized and
  queryable without recompute") holds by construction. Pinned by
  `semiring_tags_persist_in_rows` across a real reopen:
  - meet-annotated derivations (`min_cost` packs) round-trip;
  - bounded-meet outputs materialize as k rows per group (pack in the key);
  - **composition with the tt axis**: materializing an annotated derivation
    into a `TxTime` relation yields *annotated belief history* — `::history`
    shows each materialization with its engine-stamped tt, and an as-of read
    returns the annotation as believed at that time ("what did we believe,
    and why, as of T" — the persistence half of the R3 story);
  - custom-aggregate outputs stay readable after reopen with NO
    re-registration; re-computing without the registration errors loudly.
- Recorded decisions: a hidden per-row tag slot (Scallop-style) is overbuild
  — no consumer, contradicts tags-as-columns; custom aggregates in trigger
  scripts stay rejected **permanently** (factories are process-scoped Rust
  closures and cannot persist — the doc comment no longer promises R2).

### New — `:reconcile`: recompute-based belief revision (provenance semirings R3)
- **`:reconcile rel {…}`** — declare a query output to BE a TxTime relation's
  new complete current belief. The engine diffs the output against the
  resolved current belief and records, as ONE belief event at commit-tt:
  assertions for new/changed keys; retractions (tt-only) or vt-cessations
  with values copied (bitemporal) for currently-believed keys absent from
  the output. Unchanged keys record nothing — an identical re-reconcile is
  a true no-op (no tt burned, no history bloat). This is the R3
  truth-maintenance step in its honest recompute form: retract or append
  base facts, re-derive, `:reconcile` the derived (annotated) relation —
  derived annotations stay consistent with the revised base, and
  `::history` + as-of reads answer **"what did we believe, and why, as of
  T"** across the revision (pinned end-to-end with a `min_cost_k`-annotated
  path relation surviving a base-edge retraction). **Truth maintenance is
  user-driven**: no automatic base→derived propagation; incremental
  (DRed/counting) maintenance is recorded future work.
- **The declaration is protected transaction-wide** (review must-fix,
  empirically probed): a reconciled relation admits no other write in the
  same transaction, before or after the reconcile — including cases where
  an idempotent reconcile buffers no rows and would otherwise leave no
  pending trace (`{reconcile} {rm}` would silently empty the relation the
  reconcile just declared). Witnessed by a transaction-scoped
  reconciled-relations set, not the pending-write buffer.
- Documented contracts: TxTime relations only (plain relations keep
  `:replace`); the revision is invisible to later reads in the same script
  (§5 one-belief-event); duplicate keys with conflicting values in one
  output error; bitemporal inputs must carry assert-flagged, explicit vt
  timestamps (`'NOW'` mints a fresh group per run); value columns with
  non-constant defaults defeat idempotence if omitted; cost is
  O(relation) per call.

### Fixed — `and`/`or` meet aggregates reported an inverted changed-bit (upstream bug)
- `MeetAggrAnd`/`MeetAggrOr::update` returned `true` when the value was
  **stable** and `false` when it **changed** — so in recursive rules a real
  change never propagated through the semi-naive delta (wrong results) and
  stable values were re-enqueued. Found by the R0b adversarial review while
  auditing the new idempotence probe; fixed to report change, pinned by a
  unit test.

### Fixed — `%return { <query> }` panicked in imperative scripts (upstream bug)
- The match arm expected `query_script_inner` where the grammar delivers
  `imperative_clause`; any braced clause in `%return` hit `unreachable!()`.


## 0.9.0 — 2026-06-28

Adds the **read-only Cypher query surface** (the headline feature) and bundles
the corrupt-database tooling that was banked as 0.8.6 but never published
(0.8.5 → 0.9.0 ships both; there is no separate 0.8.6 crates.io artifact).

### New — read-only Cypher query surface (alpha, feature `cypher`, off by default)
- Translate a subset of **openCypher** to CozoScript so the engine can be
  evaluated and adopted without first learning Datalog. Datalog stays the native,
  full-power language; this is a **read-only** on-ramp (no write clauses). New API:
  `DbInstance::run_cypher` / `cypher_to_script` (+ Python `run_cypher`), driven by
  a caller-supplied `CypherGraphSchema` / `NodeMap` / `EdgeMap` mapping the
  property-graph model onto stored relations — supporting both the
  relation-per-label and the shared-relation-with-discriminator conventions
  (the latter matches MindGraph's reified `node`/`edge` model).
- v1 subset: `MATCH` (fixed-length, directed, labels, types, inline property
  maps), `WHERE` (comparisons, `AND`/`OR`/`NOT`, `IN`, `IS NULL`, `STARTS`/`ENDS
  WITH`, `CONTAINS`), `RETURN` (`DISTINCT`, aliases, aggregates
  `count`/`sum`/`avg`/`min`/`max`/`collect`), `ORDER BY` / `SKIP` / `LIMIT`.
  Literals pass as params and every interpolated identifier is validated. True
  bag semantics (`count(*)`/`LIMIT` match openCypher), **null-aware `WHERE`**
  (a null operand drops the row instead of aborting the query), and per-`MATCH`
  edge-isomorphism. Module `cozo-core/src/cypher/`; design + scope in
  `docs/specs/cypher-read.md`; hardened against a multi-agent adversarial review.
  Off by default — enable the
  `cypher` feature (the published PyPI wheel ships without it for now; build with
  `--features cypher` to get the Python `run_cypher`). Deferred with explicit
  errors: undirected relationships, the
  schema `filter` field, variable-length paths, `OPTIONAL MATCH`, `WITH`. Known
  divergence: `sum` over an integer column returns a float (engine accumulator is
  f64).

### Fixed — `cozo-bin` token-table bearer auth ignored on query-string URLs
- The server's `authorize` evaluated `Authorization: Bearer` against the token
  table only in the `uri().query() == None` branch, so any endpoint that takes
  query params (`/transact?write=true`, `/rules/name?arity=2`) rejected valid
  bearer tokens. Bearer auth is now evaluated independently of the query string
  (a `?auth=<secret>` credential still takes precedence, then bearer falls
  through regardless of other query params). CORS also now allows the
  `Authorization` header so browser bearer clients pass preflight.

### Fixed — `cozo-bin` had no default feature, failed to build bare
- `cargo build -p cozo-bin` (no features) failed to resolve `rayon` — cozo-core
  uses rayon unconditionally on non-wasm, but it is pulled in only via the
  `graph-algo` feature, which `cozo-bin` did not enable by default — and any
  binary it did produce had no storage backend. `cozo-bin` now defaults to the
  `compact` combo (sqlite + requests + graph-algo): a runnable server/REPL out
  of the box. Opt into rocksdb explicitly.

### Fixed — `::index create` no longer panics on corrupt tuples
- Index population extracted columns by position with no bounds check: one
  truncated stored tuple (e.g. from an interrupted write) panicked the whole
  build — and since applications may (re)create indexes while initializing a
  database, one bad row made the database unopenable (observed in production
  2026-06-12). Corrupt tuples are now skipped with a loud error naming the
  relation, the index, and the arity mismatch, plus a build-level summary
  ("N corrupt tuple(s) skipped — the base relation needs repair").

### New — `::repair_corrupt <relation>`: surgically delete truncated tuples
- Tuples whose stored arity is shorter than the schema (truncated value bytes
  from interrupted writes) are deleted by their intact store keys; returns the
  removed count. Gives applications a surgical alternative to dropping a
  database that fails integrity checks — the motivating incident (2026-06-12)
  saw an application-level "repair" delete a production database over 15 bad
  rows. Pinned by `tests/fork_regressions.rs`.

## 0.8.5 — 2026-06-12

### Changed — flat in-RAM parallel HNSW bulk build (`::hnsw create`)
- `::hnsw create` now constructs the graph in flat, integer-indexed memory
  (one contiguous vector slab + per-node adjacency arrays, the
  hnswlib/pgvector/Lucene layout) instead of the temp-store BTreeMap of
  encoded tuples, and inserts in parallel with per-node locks. A profile of
  the old build showed >50% of CPU in tuple encode/decode, `CompoundKey`
  hashing and allocator traffic — all gone. The finished graph is serialised
  into the index relation's existing tuple format in one pass: the on-disk
  layout, the search path, incremental maintenance (`hnsw_put`/`hnsw_remove`),
  and the non-blocking Phase A–D build/reconcile orchestration are unchanged.
- `MNESTIC_INDEX_BUILD_THREADS` controls worker count (unset/0 = all cores;
  1 = serial insertion in scan order, the old behaviour). Parallel insertion
  makes the built graph non-deterministic across runs (link sets vary with
  interleaving, as in hnswlib/pgvector); recall agreement is guarded by
  `tests/hnsw_build.rs::parallel_build_recall_agreement`.
- Two parallel-only divergences from the serial insertion algorithm, both
  required for lock-safety: neighbour-overflow shrinking never extends
  candidates (would need two node locks at once), and a node's own link-list
  write merges with concurrently-arrived backlinks instead of replacing them
  (replacement severed edges and, on chain-shaped data, broke connectivity).

### Changed — plain-snapshot read path for read-only scripts (RocksDB)
- Read-only scripts no longer open a pessimistic transaction. They read the
  base DB through a plain snapshot (`SnapshotReadBridge` in `mnestic-rocks`
  0.1.9): the same consistent view as before — the old read path also pinned
  one snapshot — but with no lock-manager bookkeeping and no transaction
  write-batch overlay consulted on every read. This is the standard MVCC
  read pattern (TiKV, CockroachDB). Writing scripts keep the pessimistic
  transaction unchanged. Isolation semantics are pinned by
  `tests/snapshot_reads.rs` (uncommitted writes invisible, read transactions
  keep their snapshot view across concurrent commits).
- Measured (RocksDB, immutable scripts, 50k rows): keyed point read p50
  **28.5 → 23.9 µs** (−16%), p99 −19%; 20-row prefix scan p50 **46.0 →
  41.5 µs**. Retrieval-scale queries (40–150 ms) on a block-cache-resident
  corpus showed no measurable change, as expected — per-script transaction
  overhead is µs-scale. Parse/compile (~20 µs) now dominates point reads,
  which is the stored-queries → plan-cache item's territory.
- A write attempted through a read-only transaction now errors explicitly
  instead of silently succeeding inside a transaction that had no business
  existing. (No in-tree path did this; the error guards against future ones.)

### Changed — batched HNSW neighbour reads on the search path
- `hnsw_search_level` fetches all unvisited neighbours' vectors per expansion
  step through one `StoreTx::multi_get` — a true RocksDB `MultiGet` (shared
  bloom-filter probes, batched block reads) on the snapshot read path —
  instead of one serial point get per neighbour (`VectorCache::ensure_key`).
  Other backends fall back to a serial loop via the trait default. Neutral on
  a block-cache-resident corpus (fused p50 unchanged); the win case is
  cold-cache / larger-than-RAM data, where serial point gets pay one block
  read each.

### Changed — FTS bulk build (`::fts create`)
- The populate loop no longer runs a del pass per row: the index relation is
  freshly created and empty, so the old code tokenised every document a
  second time to delete postings that could not exist.
- Tokenisation + posting-row encoding now fan out across worker threads
  (same `MNESTIC_INDEX_BUILD_THREADS` control); the row format is unchanged.
- Corpus doc-stats (`avgdl`) are counted exactly during the build and seeded
  directly, replacing the post-build full index scan.

### Fixed — `::describe` was unreachable upstream; now parses and is read-only-guarded
- Upstream defines `describe_relation_op` in the grammar and implements
  `SysOp::DescribeRelation`, but never wired the rule into the `sys_script`
  alternations — `::describe rel 'note'` always failed to parse. The op is
  now reachable (top-level and inside imperative blocks).
- `::describe` writes relation metadata; it was also the only mutating sys op
  without a read-only guard. It now rejects `ScriptMutability::Immutable`
  with a clear error, like its siblings, instead of falling through to the
  storage layer. Pinned by `tests/fork_regressions.rs`.

### Tests — bulk-build coverage from the post-ship bug-hunt audit
- The audit (three independent reviews of the flat HNSW build, the snapshot
  read path/FFI bridge, and the FTS bulk build) found no correctness bug; it
  found untested live paths, now covered: HNSW flat build over a
  **list-of-vectors column** (`[<F32; N>]`, the `sub_idx` branch), an
  **F64 + Cosine** flat-build recall guard, and bulk-vs-incremental FTS
  doc-stats score equality on a **multi-column-PK** relation.
- Documented in the `hnsw_build.rs` module header: the flat build omits
  pruned edges where the incremental path writes tombstoned rows —
  indistinguishable to search (`include_deleted=false`); the degree counter
  it feeds was already approximate upstream.

## 0.8.4 — 2026-06-10

Fifth fork release: a defect fix for 0.8.3's concurrent-write regression plus
the per-leg fusion detail that powers MindGraph's "why retrieved" surface.

### New — per-leg retrieval detail: `detailed` on RRF and `HybridSearch`
- `ReciprocalRankFusion(..., detailed: true)` switches the output from
  `[item, fused_score]` to the long-format
  `[item, fused_score, list_id, leg_rank, leg_score]` — one row per
  *(item, contributing list)*. `leg_rank` is the 1-based within-list rank the
  fusion actually used (after best-score dedup); `leg_score` is that
  deduplicated raw score; lists an item did not appear in contribute no row.
  `detailed` must be a constant boolean (output arity depends on it). This is
  the mechanism behind a consumer's "why was this retrieved" surface: the rows
  reconstruct the fused score exactly (`Σ 1/(k + leg_rank)`).
- `HybridSearch::detailed: bool` plumbs it through the one-call helper. Without
  MMR the head is `[id, score, list_id, leg_rank, leg_score]` and the row limit
  widens to `limit × leg-count` so the top `limit` items are always fully
  covered; with MMR the per-leg detail is joined onto MMR's selection and the
  head is `[id, rank, score, list_id, leg_rank, leg_score]`.
- Python binding: pass `detailed: True` in the `hybrid_search` dict.

### Fixed — concurrency regression in the 0.8.3 doc-stats counter
- 0.8.3's durable `avgdl` counter was one shared storage key per FTS index,
  read (without a lock) and rewritten inside **every** document transaction.
  Under RocksDB pessimistic transactions this made all concurrent writers to
  an FTS-indexed relation conflict on a single row lock (held until commit),
  and the unlocked read-modify-write also lost updates, silently drifting the
  counter. Concurrent ingest produced `Resource busy`-class storage errors.
- The counter is now **process-cached and scan-seeded**: one deduplicated
  full scan per index per process (the path 0.8.3 already used for legacy
  indices), maintained incrementally in memory on every put/delete, with no
  shared storage key in the hot path. Per-query `avgdl` stays O(1); the
  pinned behaviours (deletes net correctly, scores identical across reopen,
  BM25 denominator) are unchanged and covered by `tests/fts_avgdl.rs`.
  Index rebuilds reseed the cache and delete any legacy 0.8.3 counter key.
- Deltas from rolled-back transactions are not undone; the drift is
  negligible for a smoothing denominator and clears on restart or rebuild.

### Fixed
- `log::error` import in `jlines.rs` is now gated behind the `requests`
  feature (was an unused-import warning under minimal feature sets).

## 0.8.3 — 2026-05-31

Fourth fork release. Two agentic-memory wedge features land together and are
**validated end-to-end** on the `mnestic-benchmarks` hybrid suite (2026-05-31,
SQLite-backed wheel, vs SQLite/DuckDB/LanceDB/Kuzu): **native 3-way fused recall**
(Bet 1a) and **BM25-correct FTS with O(1) `avgdl`** (Bet 1b). All 169 inherited
lib tests + feature suites pass; `cargo clippy -p mnestic -- -D warnings` is clean.

> **Heads-up — the FTS default scorer changed.** The default `::fts` score kind
> moves from `tf_idf` to Okapi `bm25` (a behaviour change). `tf` and `tf_idf`
> stay selectable for byte-identical upstream scoring.

**Measured (2026-05-31):**
- **BM25 + O(1) `avgdl`:** fused recall **0.75 → 0.954** (parity with DuckDB
  0.957 / SQLite 1.0); decomposed-path p50 **927 → 175 ms** and the cold p99 tail
  **2,900 → 258 ms**. (The tail was the per-query `avgdl` scan, *not* cold HNSW as
  first assumed — the vector leg even got faster cold→warm, 117 → 23 ms, unchanged.)
- **Native 3-way:** the one fused call runs vector+FTS+graph at **41.55 ms p50**
  (recall 0.873) — **~4× faster** than the 175 ms hand-decomposed path, fusing a
  signal (graph) no other engine here has (LanceDB native is 2-way only: recall
  0.456). The one-call advantage reappeared exactly as predicted once the `avgdl`
  fix removed the FTS scan that had masked it.

### New — native 3-way fused recall: typed `GraphLeg` on `HybridSearch`
- `HybridSearch::graph_legs: Vec<GraphLeg>` (new `GraphLeg` type, re-exported from
  the crate root). Each leg expands from a set of `seeds` over a stored edge
  relation up to `max_hops`, scores every reached node by its **minimum hop
  distance** (closer ⇒ higher rank), and contributes that ranked list to the
  *same* Reciprocal Rank Fusion as the vector/keyword legs — one call, one
  transaction, no hand-written recursion.
- Why a new type rather than the existing `extra_lists`: an `extra_lists` entry is
  a *single* spliced rule body, which cannot express the recursive shortest-path
  rule that bounded-hop proximity needs. `GraphLeg` generates that rule — a seed
  relation, a hop-1 base rule, and a `min(dist)` recursive rule gated at
  `max_hops` — for you. Supports `undirected` traversal (also follows edges in
  reverse) and multiple seeds (unioned).
- **Injection-safe.** Seed values are passed as query **params** (`$hg{i}_seed{j}`),
  never string-interpolated; the label, edge relation, and column names are
  validated as bare identifiers, and empty seeds / `max_hops == 0` are rejected.
  The generated script remains inspectable via `hybrid_search_script`.
- `runtime/hybrid.rs`; guarded by `tests/hybrid_graph_leg.rs` (recall a neighbour
  the fixed legs miss, closer-outranks-farther via `min(dist)`, hop bound,
  undirected reverse edges, multi-seed union, script-is-recursive-and-parametrised,
  input validation). Backward compatible: an empty `graph_legs` generates the exact
  prior script.

### Added — read-path latency baseline (groundwork for a future plan/stored-query cache)
- `benches/read_path.rs` (criterion) times `parse_only` (parse + compile-to-AST)
  vs `full_run` (end-to-end `run_script`) for a point read and a multi-rule
  retrieval query on SQLite, to size the parse/compile fraction a compiled-plan
  cache could eliminate before any cache is built (the fork's baseline-first
  rule). **Finding:** parse/compile is a roughly *fixed* ~20–85 µs cost — ≈39% of
  a 55.7 µs point read but only ≈1.1% of a 7.68 ms multi-rule retrieval query — so
  a plan cache helps cheap point reads but is noise for the retrieval workload,
  where execution (and, on RocksDB, the pessimistic txn) dominates. That makes
  **Bet 1a (one fused call instead of three `run_script`s) the read-path latency
  fix that matters**, not the plan cache. A real plan cache must also clear two
  structural blockers: parse-time param inlining, and the lack of a reusable-plan
  execute entry point.

### FTS — Okapi BM25 scoring + summed disjunction + O(1) `avgdl`

The recall lever the hybrid-retrieval benchmark localized the entire fused-recall
gap to (FTS recall-agreement 0.72 vs vector 0.99 / graph 1.00).

- **New default score kind `bm25`** for `::fts`/`~rel:idx{… | score_kind: 'bm25'}`.
  Implements `idf · tf·(k1+1) / (tf + k1·(1 − b + b·|D|/avgdl))`: term-frequency
  **saturation** (`k1`, default 1.2) and **document-length normalization** (`b`,
  default 0.75, range `[0,1]`), both tunable as query params. Replaces upstream's
  raw `tf · idf`, which had neither — long documents and high raw term counts
  dominated unfairly. Two upstream defects fixed:
  - *No length normalization.* The per-document token length was **already stored**
    on every posting (`vals[3]`) but **discarded** at search time; it is now read
    (`LiteralStats::doc_len`) and used. Average document length (`avgdl`) is an
    **O(1) read** of a durable per-index doc-stats counter (see below).
  - *Disjunction did not sum.* An `a OR b` query took the **max** of per-term scores,
    so a document matching both terms could tie one matching a single term — forcing
    callers into app-side per-term aggregation with wide over-fetch. Under `bm25`,
    `OR` now **sums** per-term contributions (a document matching more query terms
    ranks higher). `tf`/`tf_idf` keep upstream's max-combine.
- **Backward compatible:** `score_kind: 'tf_idf'` and `'tf'` are unchanged
  (byte-identical scoring and the original `OR`=max semantics). Only the *default*
  moved to `bm25`.
- Guarded by `tests/bm25.rs` (sqlite backend, stored path): OR-sum beats
  repeated-single-term, length normalization favors the shorter doc, and `b: 0.0`
  provably disables length normalization (proving `b` is wired through).
- **`avgdl` is now O(1) (durable doc-stats counter).** The bench validated BM25's
  recall (0.75 → 0.96) but exposed a **~10× FTS latency regression** (71 → 755 ms
  p50): the initial `avgdl` was a full deduplicated index scan (O(#docs),
  ~680 ms/query at 40k chunks) recomputed on *every* query, because the cache lived
  on the per-operator `FtsCache`. Fixed: each FTS index maintains a durable
  `(total_tokens, n_docs)` counter at a reserved `[Bot]` key (sorts above all
  `[term, …doc_key]` postings, so it is invisible to term scans). `put_fts_index_item`
  adds a document's tokens, `del_fts_index_item` subtracts them (guarded by a posting
  existence probe), and `create_fts_index` publishes the authoritative count via one
  final scan — so `avgdl` is a single keyed `get`. A legacy index that predates the
  counter migrates itself on its first write (seed-by-scan) and, until then, reads
  fall back to a `Db`-scoped cross-query cache (one scan per process, not per query —
  correct because an un-migrated index is immutable). Identical scores to the prior
  scan (the counter value equals the scan); guarded by `tests/fts_avgdl.rs`
  (delete-equals-fresh-build, survives reopen, `avgdl` feeds the BM25 denominator).
  Well-behaved workloads (insert, delete, del-then-put update) are exact; an FTS-only
  relation with no secondary index can drift on in-place update, mirroring upstream's
  existing posting leak there (an index rebuild resets it). **Bench-confirmed:** the
  FTS leg returned to ~71 ms and decomposed p99 fell 2,900 → 258 ms with recall held
  at 0.954.

### Python
- `cozo-lib-python`'s `hybrid_search` now accepts a `graph_legs` list (mapped to
  `Vec<GraphLeg>`), so the embedded `mnestic` wheel can drive the native 3-way
  fused recall from Python. `cozo-lib-python` stays workspace-excluded (built only
  when the wheel is built).

## 0.8.2 — 2026-05-30

Third fork release. Makes HNSW index builds **non-blocking for readers**: an
index build no longer freezes all reads on the base relation for the (often
multi-minute) duration of the build. All 169 inherited lib tests + the
integration/feature suites pass; `cargo clippy -p mnestic -- -D warnings` is clean.

### Performance — non-blocking HNSW index builds (readers no longer stall)
- Building/rebuilding an HNSW index (`::hnsw create`) used to hold the base
  relation's **exclusive write lock** for the *entire* build, so every concurrent
  read (which takes the same lock shared) blocked until the build finished — in
  production, **10–20+ minutes** (76 min for a 151K × 1536 index). The stall was
  cozo's per-relation `ShardedLock`, not RocksDB.
- The build is now done **off-lock** on RocksDB: the heavy graph construction runs
  under a read-only snapshot with **no relation lock held**, and the lock is taken
  only briefly to set up the empty index relation and to publish the result.
  Measured: building a **40,000**-vector index takes ~5.6 s, during which
  **90,507** concurrent reads of the same relation completed, the slowest in
  **0.8 ms** (release). Previously those reads would have queued behind the whole
  ~5.6 s build.
- **How it stays correct.** The finished, key-sorted graph is bulk-published into
  the live store via `SstFileWriter` / `IngestExternalFile` (bypassing the
  transaction write-batch), and the index *data* is always ingested before its
  *metadata* is committed — so a reader can never observe an index before its keys
  exist. Base-relation rows that change during the unlocked build are folded in by
  a short reconcile pass (re-scan + diff against the build snapshot, applying the
  same incremental `hnsw_put`/`hnsw_remove` maintenance) under a brief final lock.
  Concurrent builds of the same index are serialised; a lost race cleans up its
  ingested data. Index relation ids are always freshly allocated, so a crash
  mid-publish leaves at worst unreferenced dead keys, never a torn index.
- Non-RocksDB backends (sqlite/mem/…) keep the in-transaction build + per-key
  flush unchanged. New `Storage::ingest_sorted` (default-errors; real impl only on
  RocksDB) carries the SST bulk-load. Guarded by `tests/hnsw_nonblocking_build.rs`
  (build correctness, persistence across reopen, reads-during-build, reconcile of
  concurrent inserts, drop+recreate).

## 0.8.1 — 2026-05-30

Second fork release. Adds the one-call hybrid-retrieval API, a substantial HNSW
index-build speedup, the maintained `mnestic-rocks` bridge fork, and a blocking
clippy CI gate. All 169 inherited lib tests + integration/feature tests pass.

### New — one-call hybrid retrieval (`HybridSearch`)
- `DbInstance::hybrid_search` / `Db::hybrid_search` (and `*_script` to inspect the
  generated CozoScript) run an HNSW + FTS (+ optional graph-traversal) recall,
  fuse it with `ReciprocalRankFusion`, and optionally diversify with
  `MaximalMarginalRelevance` — in one typed call. Previously this was ~7
  hand-assembled Datalog rules. Read-only; the query vector/text are passed as
  script params (never string-interpolated) and every interpolated identifier is
  validated against injection. New module `runtime/hybrid.rs`; tested in
  `tests/hybrid_helper.rs`.

### Performance — HNSW index build ~3× faster
- `::hnsw create` (and backfill) over a large relation was **superlinear**: the
  whole graph was built inside the script's pessimistic transaction, so every
  neighbour read/write round-tripped through RocksDB's `WriteBatchWithIndex`
  overlay (whose cost grows with the index). Measured baseline: **135 s for
  20k × 128** vectors (release).
- The build now constructs the graph in the in-RAM temp store (`is_temp` routing
  via new `idx_put`/`idx_get`/`idx_del`/`idx_exists` helpers) and bulk-migrates
  the finished, key-sorted graph to the persistent store in one pass; and it
  shares one `VectorCache` across the whole build instead of rebuilding it per
  node. Combined **~3.1× faster** (20k × 128: 135 s → 43.6 s; 10k: 51.8 s →
  16.5 s). The built graph is byte-identical; guarded by `tests/hnsw_build.rs`.
- *Investigated and dropped:* batched secondary-index writes (drafted "#4",
  claimed 2–3× ingest). Measured: index writes are ~7 % of ingest even with 4
  indexes, and within a pessimistic txn each `put` only appends to an in-memory
  batch — batching saves <1 %. The real ingest floor is the per-script
  pessimistic transaction, which that change doesn't touch.

### Bridge — `mnestic-rocks`
- Forked the C++/RocksDB bridge crate `cozorocks` → **`mnestic-rocks`** (v0.1.8),
  keeping the importable crate name `cozorocks` (`[lib] name`) so `use cozorocks::`
  and `cozorocks?/feature` references are unchanged. Enables shipping future
  bridge-level work (e.g. out-of-transaction index build + `IngestExternalFile`
  atomic publish) on crates.io.

### Maintenance
- `document-features` 0.2.8 → 0.2.12 to clear a future-incompat warning.
- **Blocking clippy CI gate** (`cargo clippy -- -D warnings`, default features).
  Pervasive/intrinsic inherited lints are allow-listed with rationale in
  `lib.rs` so the gate catches new issues. `cargo fmt --check` is deliberately
  not gated yet (a ~178-file reformat is deferred to its own pass).

### Deferred (designed, not in this release)
- **Lock-free index build** (out-of-transaction build against a snapshot +
  `SstFileWriter`/`IngestExternalFile` atomic publish) — directly fixes "rebuild
  holds the write lock and blocks readers"; touches the transaction lifecycle.
- **Native in-RAM graph** (adjacency as integer arrays, no per-edge
  (de)serialization) for a further ~10–50× build speedup, like hnswlib.

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

The fork's pre-fork bug drafts were written against the
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
