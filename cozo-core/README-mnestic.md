# mnestic

**mnestic** is an independently maintained fork of
[CozoDB](https://github.com/cozodb/cozo) — a transactional,
relational-graph-vector database that uses Datalog for queries ("the
hippocampus for AI"). The fork continues the project as a substrate for
**agentic memory**, with performance, correctness, and operational work on top
of upstream `481af05` (the last upstream commit, 2024-12-04).

> mnestic is **not** the official CozoDB and is not affiliated with or endorsed
> by its original authors. All credit for the original design belongs to Ziyang
> Hu and the Cozo Project Authors. See
> [`FORK.md`](https://github.com/shuruheel/mnestic/blob/main/FORK.md) for
> provenance and licensing, and
> [`CHANGELOG-FORK.md`](https://github.com/shuruheel/mnestic/blob/main/CHANGELOG-FORK.md)
> for what diverges from upstream.

## What mnestic adds over CozoDB

Highlights (full detail in
[`CHANGELOG-FORK.md`](https://github.com/shuruheel/mnestic/blob/main/CHANGELOG-FORK.md)):

**0.10.6**

- **Fix: relation catalogs written before 0.10.0 now open again** ("Cannot
  deserialize relation metadata from bytes"). The 0.10.0 bitemporality work
  inserted a field *mid-struct* in `RelationHandle`; on the catalog-write paths
  that encode positionally, `#[serde(default)]` only rescues a *missing trailing*
  field, so any relation whose catalog was last written before 0.10.0 (or by an
  index/rename/destroy path) failed to deserialize on open — taking the whole
  database down. The field moved to the end of the struct so the trailing default
  applies to legacy arrays, and the seven catalog-rewrite paths now serialize
  self-describing maps (`.with_struct_map()`) like the create path, so future
  field additions can't reintroduce the bug. No migration: legacy catalogs stay
  readable and re-canonicalize to maps on their next write. **If you upgraded a
  pre-0.10.0 database to any of 0.10.0–0.10.5, upgrade to 0.10.6.** Regression-
  guarded by a real pre-0.10.0 catalog fixture.
- **Greedy join reorder is now a pure function** over a resolved schema view —
  an internal refactor of the 0.10.5 join-reorder pass that makes it independently
  unit-testable. No query-plan or behavior change.
- **Python-wheel CI hardened for `storage-rocksdb`** — the x86_64 manylinux leg
  now builds on `manylinux_2_28` with `libclang` so zstd-sys's bindgen resolves.
  Wheel-build only; no engine change.

**0.10.5**

- **Interruptible `::kill` / `:timeout`, plus a per-query wall-clock budget.**
  `::running`/`::kill` now dispatch before opening any storage transaction (so on
  the mem/sqlite backends a `::kill` no longer queues behind the very read query
  it targets), and the per-query poison flag is threaded through the
  relational-algebra enumeration and checked every 4096 pulls — so a long-running
  single-rule join that yields no output is now actually interruptible. A query
  can also carry a deadline three ways: the in-script `:timeout <secs>` option, a
  per-call
  `run_script_with_options(payload, params, mutability, ScriptRunOptions { timeout })`,
  and a Db-wide `set_default_query_timeout`. The effective deadline is the
  **minimum** of whichever are set, expiry raises a distinct `eval::timeout`
  (versus `eval::killed` for a `::kill`), and the old per-timed-query timer thread
  is gone (no leak). Exposed on `DbInstance`, in Python (a `timeout=` kwarg), and
  in cozo-bin (a `--default-query-timeout` flag + a `timeout` payload field). Wasm
  carries no wall-clock budget (no monotonic clock).
- **Deterministic greedy join reorder** (default **on**; opt out per-query with
  `:reorder written`). No pass previously considered join order, so a
  naively-ordered conjunction — exactly what an LLM agent authors — could spin on
  an N³ intermediate. A stat-free min-new-vars pre-pass reorders the positive
  relation atoms of an eligible conjunction (measured **54.5×** on the repro;
  N³→N²). Results are unchanged: conjunction is commutative under set semantics,
  and the pass is the identity on any already stepwise-greedy-consistent written
  order, so hand-tuned plans stay byte-identical. It excludes multi-valued
  `in`-unifications feeding an aggregation (which would otherwise change a
  `count`). A residual Cartesian step (a genuinely disconnected conjunction) is
  warned and annotated in `::explain`. Spec:
  [`docs/specs/join-reorder.md`](https://github.com/shuruheel/mnestic/blob/main/docs/specs/join-reorder.md).
- **Automatic factorized `count()` rewrite** (opt-in, **default off**, behind
  `set_query_factorization`). `count()` over a join streams every match; this
  rewrites an eligible single-clause `count()`-over-positive-join into per-key
  counting sub-rules — a bit-identical (exact-i64, `Int`-typed) answer computed
  without materializing the join (the benchmark measured **4–342×** versus a
  factorizing optimizer). It fires only on shapes it can prove exact and declines
  the rest; a body with any `!=` predicate falls back to exact naive evaluation.
  An always-on companion detector surfaces a factorization advisory in
  `::explain`. Spec:
  [`docs/specs/cardinality-algebra.md`](https://github.com/shuruheel/mnestic/blob/main/docs/specs/cardinality-algebra.md).
- **RocksDB in the PyPI `mnestic` wheel** — `CozoDbPy("rocksdb", path)` now works
  straight from `pip install mnestic` (the wheel previously shipped
  compact/SQLite-only). The sdist stays compact, so the persistent engine is
  wheel-only.
- **Python binding: interior-mutable `CozoDbPy`.** `close()` now takes a shared
  `&self`, fixing an "Already borrowed" error when it raced a live `run_script`;
  `run_script` gains an optional `timeout=None` kwarg, plus
  `set_default_query_timeout` / `default_query_timeout`.
- **Bulk `import_relations` into an index-bearing relation now warns** — the bulk
  path maintains B-tree secondary indexes but not HNSW/FTS/LSH, so imported rows
  stay invisible to vector/text search until the index is rebuilt. A warning now
  flags it (still not a hard error: importing a snapshot then reindexing is
  legitimate).

**0.10.1**

- **Dominance bounded-meet — the antichain / skyline aggregate.**
  `register_bounded_meet_aggr(name, dominates, max_survivors)` opens the
  bounded-meet category to a host-registered strict partial order: per group, the
  head form `name(operand)` keeps the non-dominated (Pareto-frontier) set of
  operands, each survivor its own output row, riding the same stratifier permit
  and divergence cap as `min_cost_k`. `max_survivors` is a mandatory resource
  guard — overflow is a loud error, never a silent truncation. Rust-embedded-only
  v1. Spec:
  [`docs/specs/antichain-bounded-meet.md`](https://github.com/shuruheel/mnestic/blob/main/docs/specs/antichain-bounded-meet.md).
- **Interval primitives** — `interval_overlaps(a, b)` (builtin) and
  `interval_coalesce(span)` (aggregate) over half-open `[start, end)` list
  intervals. Touching spans coalesce but do not overlap (`[0,5)` + `[5,10)` =
  `[0,10)`); empty spans `[x, x)` overlap nothing; mixed int/float bounds compare
  numerically; malformed spans are loud errors, never silent falses. Spec:
  [`docs/specs/cozoscript-extensions.md`](https://github.com/shuruheel/mnestic/blob/main/docs/specs/cozoscript-extensions.md) §3.4.
- **Correctness fix** — the `bit_and`/`bit_or` meet aggregates now report whether
  the value actually changed (the byte loop returned changed unconditionally, so a
  non-changing fold re-entered the semi-naive delta every epoch), and the
  bounded-meet divergence cap now counts total changed epochs rather than a
  consecutive streak that reset on quiet epochs.

**0.10.0**

- **Bitemporality — system-versioned (`TxTime`) relations.** An engine-assigned
  transaction-time axis alongside Cozo's valid-time: declare
  `{k, tt: TxTime => v}` (system-versioned) or
  `{k, vld: Validity, tt: TxTime => v}` (fully bitemporal) and every committed
  write is stamped by a crash-safe monotone commit clock. Reads default to
  current state; time-travel with `@ (vt: ..., tt: ...)` or a query-wide
  `:as_of`; existence-checking writes (`:insert`/`:update`/`:ensure`/bitemporal
  `:rm`) target the resolved current belief. Ask "what did we believe at time
  T about period Y" in-engine. Spec:
  [`docs/specs/bitemporality.md`](https://github.com/shuruheel/mnestic/blob/main/docs/specs/bitemporality.md).
- **History lifecycle sys ops** — `::history` (the raw belief timeline per
  key), `::history_gc` (drop superseded records below a cutoff; a persisted
  floor keeps as-of reads at/above the cutoff exact and makes reads below it
  error loudly), `::evict` (hard deletion for data-erasure obligations, with a
  salted audit trail; `unredacted` opt-out).
- **`:reconcile` — declarative belief revision.** Declare a query output to BE
  a `TxTime` relation's new complete current belief; the engine diffs against
  the resolved current belief and records assertions + retractions as one
  belief event (idempotent re-runs record nothing). Retract base facts,
  re-derive, `:reconcile` — then `::history` + as-of reads answer "what did we
  believe, and why, as of T."
- **Custom aggregates** — `register_custom_aggr`: register a domain-specific
  absorptive (semilattice) combine and use it in recursive rules exactly like
  `min`/`shortest`, including in the recursion guard.
- **Top-k proofs — `min_cost_k([payload, cost], k)`.** A new bounded-meet
  aggregate category that keeps the k best derivations per group as ordinary
  rows: k-shortest-paths with the evidence chain that justifies each answer
  (Scallop-style approximate top-k), guarded against divergent recursion.
- **Temporal-read performance, measured:** current-belief point reads within
  ~4–12% of the vt-only baseline on sqlite/rocksdb (pinned-cursor scans);
  single-version full scans ~2× faster than the baseline.
- **Four upstream CozoDB bugs fixed** — inverted changed-bit in `and`/`or`
  meet aggregates (changes never propagated in recursion), a panic on negated
  validity atoms, wrong answers from prefix-truncated joins on
  temporal-column relations, and a parse panic on braced `%return` clauses in
  imperative scripts.

**0.9.0**

- **Read-only Cypher query surface** (alpha, feature `cypher`, off by default) —
  translate a subset of openCypher to CozoScript so you can evaluate and adopt
  the engine without first learning Datalog (Datalog stays the native,
  full-power language; read-only, no write clauses). `run_cypher` /
  `cypher_to_script` (+ Python `run_cypher`) over a caller-supplied property-graph
  schema mapping labels/types onto stored relations; covers MATCH / WHERE /
  RETURN (DISTINCT, aggregates) / ORDER BY / SKIP / LIMIT with true bag semantics,
  null-aware WHERE, and edge-isomorphism. Design:
  [`docs/specs/cypher-read.md`](https://github.com/shuruheel/mnestic/blob/main/docs/specs/cypher-read.md).
  0.9.0 also bundles the corrupt-database tooling first banked internally as 0.8.6:
- **`::repair_corrupt`** — surgical corruption repair: scan a relation and drop
  only the unreadable tuples, leaving the rest of the data intact (no
  delete-and-rebuild).

**0.8.3**

- **Native 3-way fused recall** — `hybrid_search` now fuses a graph-proximity leg
  *in-engine* alongside vector (HNSW) and full-text (FTS). A typed `GraphLeg`
  generates a recursive bounded shortest-path rule (k-hop, `min(dist)` scoring) and
  folds it into the same RRF, so one call returns the vector+FTS+graph ranking — a
  capability no other embedded engine offers. Measured **41.55 ms p50**, ~4×
  faster than the hand-decomposed three-query path.
- **BM25-correct FTS, with O(1) `avgdl`** — the default `::fts` scorer is now Okapi
  **`bm25`** (term-frequency saturation `k1` + document-length normalization `b`,
  both tunable; `OR` sums per-term contributions). Average document length is an O(1)
  read of a durable per-index counter rather than a per-query index scan. Measured:
  fused recall **0.75 → 0.954** (parity with DuckDB / SQLite) at no net latency cost
  (decomposed p50 927 → 175 ms, cold p99 2,900 → 258 ms). **Heads-up:** this changes
  the default FTS score kind (a behaviour change); `tf`/`tf_idf` stay selectable for
  byte-identical upstream scoring.

**0.8.2**

- **Non-blocking HNSW index builds** — `::hnsw create` no longer holds the base
  relation's write lock during graph construction, so concurrent reads are no
  longer stalled for the whole build (previously 10–20+ min in production). Built
  off-lock under a snapshot and bulk-published via `SstFileWriter`/
  `IngestExternalFile`; concurrent mutations reconciled under a brief final lock.
  Measured: 90,507 reads completed (slowest 0.8 ms) during a ~5.6 s 40k-vector
  build. RocksDB only.

**0.8.1**

- **One-call hybrid retrieval** — `DbInstance::hybrid_search` runs HNSW + FTS
  (+ optional graph traversal), fuses with RRF, and optionally diversifies with
  MMR in a single typed call (was ~7 hand-written Datalog rules).
- **HNSW index build ~3× faster** — the build no longer round-trips the whole
  graph through the transaction's write-batch overlay (20k × 128: 135s → 43.6s,
  measured release). Built graph is byte-identical.
- **`mnestic-rocks`** — the C++/RocksDB bridge is now a maintained fork
  (importable name stays `cozorocks`), unblocking future bridge-level work.

**0.8.0 — fixes**

- **Equality pushdown** — `*rel[k, ..], k == <value>` now compiles to a keyed
  `stored_prefix_join` instead of a full scan (**~28–29× faster** single-row
  primary-key lookups, measured at 5k rows). Numeric equalities keep cross-type
  `op_eq` semantics.
- **Parser fix** — identifiers that start with a keyword literal
  (`nullable_column`, `trueValue`, `falsey`) now parse correctly (upstream #281).
- **Unreleased upstream fixes for free** — the fork point is 30 commits ahead of
  the published 0.7.6, including the `stored_prefix_join` correctness fix.
- `env_logger` moved to a dev-dependency for a slimmer dependency graph
  (upstream #287).

**0.8.0 — new: hybrid retrieval for agentic memory** (Datalog-composable fixed rules)

- `ReciprocalRankFusion` (alias `RRF`) — fuse vector (HNSW) + full-text (FTS) +
  graph-traversal result lists into one ranking.
- `MaximalMarginalRelevance` (alias `MMR`) — diversity-aware reranking that avoids
  near-duplicate recalls.
- `rand_ulid()` / `ulid_timestamp()` — lexicographically-sortable identifiers for
  time-ordered scans (upstream #296).

## Importable name

The published crate is `mnestic`, but the importable Rust crate name is `cozo`,
so existing CozoDB code works unchanged:

```toml
[dependencies]
mnestic = "0.10.6"
```

```rust
use cozo::{DbInstance, ScriptMutability};
```

The query language (CozoScript / Datalog) and engine semantics are unchanged
unless noted in the fork changelog.

## Features

Default is `compact` (SQLite backend). RocksDB, vector (HNSW), full-text search,
and graph-algorithm features match upstream Cozo 0.7.x. See the crate docs and
the [upstream CozoDB documentation](https://docs.cozodb.org/) for the query
language and feature flags.

## License

Mozilla Public License 2.0. Original work © 2022 The Cozo Project Authors;
fork modifications © 2026 Shan Rizvi.
