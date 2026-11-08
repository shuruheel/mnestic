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

Upstream's last commit was 2024-12-04. mnestic continues the engine, with these
capabilities on top of it:

- **Cached graph projections** — `::graph create G { edges: knows }` names an
  in-memory adjacency that twelve graph algorithms reuse across queries instead
  of rebuilding on every call. Always fresh: a projection never serves data
  differing from what the consuming transaction's own scan would return, and a
  write to a source frees what was built from it.
  ([spec](https://github.com/shuruheel/mnestic/blob/main/docs/specs/graph-projection.md))
- **Budgeted weighted traversal** — `BudgetedTraversal` expands cheapest-first
  from a set of seeds, over non-negative weights, under a global distinct-node
  budget (plus optional cost ceiling and exact hop bound) and an in-expansion
  admission gate, emitting each admitted node's `(cost, parent, depth)`.
  Deterministic by construction, interruptible, and able to consume a cached
  graph projection — the primitive for filling a fixed context window with the
  cheapest graph neighborhood around a set of search hits.
  ([spec](https://github.com/shuruheel/mnestic/blob/main/docs/specs/budgeted-traversal.md))
- **Bitemporality** — a `TxTime` column type with a crash-safe monotone commit
  clock, `:as_of` reads, the two-level `(valid time, transaction time)`
  resolution, and `::history` / `::history_gc` / `::evict`.
  ([spec](https://github.com/shuruheel/mnestic/blob/main/docs/specs/bitemporality.md))
- **Calendar-aware datetime library** — component extractors (`dt_year` …
  `dt_dow`), `dt_trunc`, calendar-aware `dt_add` / `dt_diff`, timezone-aware
  `dt_format`, and `dt_to_validity` — the typed bridge from float Unix *seconds*
  to a `Validity`'s integer microseconds, so a timestamp reaches the valid-time
  axis *as* a validity rather than a bare number whose unit the engine must
  guess.
- **Provenance semirings** — user-defined absorptive combines inside recursion
  (`Db::register_custom_aggr`), the `min_cost_k` bounded-meet aggregate returning
  the *k* best derivations with their evidence chains, and `:reconcile`
  recompute-based belief revision.
  ([spec](https://github.com/shuruheel/mnestic/blob/main/docs/specs/provenance-semirings.md))
- **Skyline / Pareto-frontier aggregates** — `pareto_min` / `pareto_max` keep, per
  group, the non-dominated set over a numeric vector (native componentwise
  dominance), surfacing a *contested set* — answers none of which beats another —
  instead of collapsing to one winner. Reachable from every binding through plain
  `run_script`; arbitrary caller-defined dominance is available in Rust via
  `register_bounded_meet_aggr`.
  ([spec](https://github.com/shuruheel/mnestic/blob/main/docs/specs/antichain-bounded-meet.md))
- **In-engine hybrid retrieval** — reciprocal-rank fusion over vector, full-text
  and graph legs as one Datalog-composable fixed rule, with MMR diversification.
  Each leg is optional, and a graph leg can run as budgeted cheapest-first
  expansion — seeded from the vector/FTS hits — to fill a fixed context budget
  with the cheapest graph neighborhood around what recall found.
- **Read-only Cypher** — an openCypher subset translated to CozoScript (alpha;
  feature `cypher`, off by default).
  ([spec](https://github.com/shuruheel/mnestic/blob/main/docs/specs/cypher-read.md))
- **Stored / named queries** — `::query create/list/show/run/remove` persists
  reusable, optionally typed and defaulted read rules. Invoke one by name or as
  a normal rule atom; atom form is hygienically spliced before magic-set
  rewriting, so caller bindings specialize through the stored rule chain.
  ([spec](https://github.com/shuruheel/mnestic/blob/main/docs/specs/stored-queries.md))
- **Atomic Parquet / Arrow copy-in** — the feature-gated host API imports one
  local Parquet or Arrow IPC file into an existing relation in a single
  transaction, with explicit conversion/resource limits and a report naming
  search indexes that require rebuilding.
  ([spec](https://github.com/shuruheel/mnestic/blob/main/docs/specs/parquet-arrow.md))
- **Candidate-scoped full-text retrieval** — `candidates:` restricts an FTS
  atom to an allowlist of base-relation primary keys before top-k truncation,
  while retaining corpus-global BM25 scores for eligible documents.
  ([spec](https://github.com/shuruheel/mnestic/blob/main/docs/specs/fts-candidates.md))
- **Faster lookups and plans** — equality pushdown turns post-filter point
  lookups into keyed seeks (~28× at 5k rows), plus a deterministic greedy join
  reorder and a default-on factorized `count()` rewrite (covering inequalities,
  behind a type gate).
- **Non-blocking vector index builds** — HNSW builds in RAM in parallel and no
  longer blocks reads for minutes; search-path neighbour vectors batch-fetch
  through RocksDB `MultiGet`.
- **Operational recovery** — `::reindex` rebuilds a relation's HNSW / FTS / LSH
  indexes in place from the configuration the database already stores, so a bulk
  load or a backup restore (neither of which maintains those indexes) has a repair
  path that is not "drop it and reconstruct the creation script by hand"; and
  `::repair_corrupt` surgically deletes truncated tuples instead of forcing you to
  drop a database that fails an integrity check.
- **Interruptibility that works** — `::kill` and `:timeout` abort running
  queries, including long graph-adjacency builds.
- **Governed multi-transactions** — a native Rust host-owned
  worker retains admission through cancellation and cleanup, with one deadline,
  bounded idle waiting, inherited statement memory limits and warning flushes.
  Any query error aborts the transaction; the legacy API remains available.
  ([contract](https://github.com/shuruheel/mnestic/blob/main/docs/specs/governed-transactions.md))

Everything else — CozoScript, the storage engines, the data model — is upstream
CozoDB, unchanged unless noted in
[`CHANGELOG-FORK.md`](https://github.com/shuruheel/mnestic/blob/main/CHANGELOG-FORK.md).

## New in 0.19.0

This release adds governed native Rust transactions and repairs transactional
FTS statistics and HNSW mutation connectivity.

- **Governed transactions (native Rust):** `db.governed_transaction(write, options)`
  returns a serialized client and host-owned worker. One deadline, bounded idle
  waiting, inherited statement memory limits and warning flushes apply across the
  transaction; every query error rolls it back. Hosts retain admission until the
  worker exits. Cancellation is cooperative and can race a commit, so hosts must
  reconcile uncertain outcomes. Python and other bindings do not expose this API.
- **BREAKING (results):** FTS scores/top-k now use statistics from the querying
  transaction's posting snapshot and pending writes. Aborts and failed commits no
  longer leak speculative totals. Recheck score thresholds and ranking fixtures;
  there is no legacy-statistics switch. Overlapping snapshots may rescan postings.
- **Rebuild pre-0.19 HNSW indexes with `::reindex <relation>`** after a backup, then
  validate search against exact base-row/vector queries. New mutations preserve
  directed paths; existing disconnections and missing legacy reverse records need
  that explicit rebuild. Writes, including fresh insertions, can fail with
  `hnsw::repair_limit` when local degree/work caps cannot preserve paths. Abort and
  open a new transaction after failure; later statements cannot make it committable.
- `::warnings` stays responsive during active memory/SQLite transactions.
  `parser.string_decoding_changed` is retired as promised; decoding is unchanged.
  The permanent 0.18 migration guidance still applies to legacy scripts.

Storage encoding and `mnestic-rocks` 0.1.12 are unchanged. The local repair caps
are not total memory or latency guarantees; filtered-ANN eligibility and its
approximate-recall limitations remain. Schedule maintenance time for index rebuilds
and evaluate write costs on your workload. A focused comparison between two 0.19
builds measured write p95 increases of 3.1% on RocksDB and 11.8% on SQLite, with
seed-sensitive tails; a separate one-seed window also showed roughly 16% increases
in selected exact scans. These are not 0.18-to-0.19 measurements or a bound on
upgrade cost. No general speedup or exact ANN recall is promised.

Full upgrade guidance is in the [`CHANGELOG-FORK.md`](https://github.com/shuruheel/mnestic/blob/main/CHANGELOG-FORK.md).

## Importable name

The published crate is `mnestic`, but the importable Rust crate name is `cozo`,
so existing CozoDB code works unchanged:

```toml
[dependencies]
mnestic = "0.19.0"
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
