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
- **Faster lookups and plans** — equality pushdown turns post-filter point
  lookups into keyed seeks (~28× at 5k rows), plus a deterministic greedy join
  reorder and an opt-in factorized `count()` rewrite (now covering inequalities,
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

Everything else — CozoScript, the storage engines, the data model — is upstream
CozoDB, unchanged unless noted in
[`CHANGELOG-FORK.md`](https://github.com/shuruheel/mnestic/blob/main/CHANGELOG-FORK.md).

## New in 0.13.0

A combined correctness-and-capability release: a nine-bug correctness union
(FTS scoring, HNSW/FTS index maintenance, corrupt-blob handling, restore/open
relation-id reconciliation) alongside a feature tranche — a datetime standard
library, a budgeted-expansion mode for `HybridSearch`, and the restored `!=`
factorized-count rewrite. It is a minor, not a patch: several fixes change
results (BM25 scores, hybrid-leg rankings) and the RocksDB table-options fix
changes the on-disk block format on *future* writes, but the public Rust API
stays source-compatible except where flagged below.

**RocksDB table options are now honoured (ships with `mnestic-rocks` 0.1.10).**
Every `BlockBasedTableOptions` you configured — block cache, block size,
index/filter caching — was silently discarded on every open, so the engine ran
with an 8 MB default cache and 4 KB blocks no matter what your options file
asked for. **Any read-path benchmark taken against a RocksDB store before this
release measured a slower engine than mnestic actually is.** Fixed; newly
written SSTs pick up the configured `block_size`, existing SSTs stay readable,
no migration.

**Datetime standard library (`dt_*`).** Component extractors (`dt_year` …
`dt_dow`), `dt_trunc`, calendar-aware `dt_add` / `dt_diff`, strftime
`dt_format`, and — the piece a bitemporal database needed — `dt_to_validity`,
the typed bridge from float Unix *seconds* to a `Validity`'s integer
microseconds. `@` and `:as_of` now accept a `Validity`-typed expression
(`@ dt_to_validity(parse_timestamp('2024-01-01'))`), which together with
0.12.2's float rejection closes the seconds-vs-microseconds trap. The new
`dt_*` names are reserved against custom-function registration.

**`HybridSearch`: budgeted graph expansion and optional legs.** Setting
`GraphLeg::max_nodes` switches a graph leg to cheapest-first weighted expansion
under a distinct-node budget — 0.12.0's `BudgetedTraversal`, now reachable from
the one-call `hybrid_search` surface — and `vector_index` / `fts_index` are now
`Option`, so you can fuse any non-empty subset of {vector, FTS, graph} legs.
**BREAKING (API):** `HybridSearch` and `GraphLeg` are now `#[non_exhaustive]` —
construct with `Default` and set fields — paid once, in the same release that
adds eight `GraphLeg` fields, so future field additions never break again.
Recursive-mode graph legs are unaffected; the Python dict surface gains optional
keys only.

**The `!=` factorized-count rewrite is restored, behind a type gate, default
OFF.** Its inclusion–exclusion extension (cut in 0.10.5 for an Int/Float
miscount) is sound this time: the rewrite fires only when both operands of
every inequality are declared, non-nullable, variant-stable stored columns.
Measured on LSQB q6 (sf0.1, SQLite): **41.7 s → 0.30 s (~140×)**, count
identical to the published oracle either way. Enable with
`Db::set_query_factorization(true)`; the default-on flip waits for a nightly
soak.

**Better errors for query authors.** A failed parse points its caret at the
deepest position the parser reached and adds a `help:` line naming the literal
tokens that would have been accepted (e.g. `expected one of: :=, <-, <~`);
index-search diagnostics now carry the code of the index kind that actually
failed (`fts_query_required` rather than `hnsw_query_required`, and the like).

**Correctness union — you may need one `::reindex`.** BM25's document count `N`
is now read free from the FTS index instead of by re-scanning the whole base
relation on every query (previously a 30× score error and O(corpus) latency at
scale, both deleted); no-op re-puts no longer inflate the FTS doc count; corrupt
value blobs and corrupt HNSW/FTS index rows are ordinary query errors instead of
process panics; and restore/open reconciles the relation-id counter instead of
silently reusing a live id. HNSW indexes built by any release through 0.12.2 can
carry stale nodes or edges from null-vector and pre-existing-row bugs — rebuild
once per affected relation with `::reindex <relation>` (your rows are untouched;
it rebuilds index relations only).

Full detail, including the complete HNSW / corrupt-blob / restore upgrade steps,
is in
[`CHANGELOG-FORK.md`](https://github.com/shuruheel/mnestic/blob/main/CHANGELOG-FORK.md).

## Importable name

The published crate is `mnestic`, but the importable Rust crate name is `cozo`,
so existing CozoDB code works unchanged:

```toml
[dependencies]
mnestic = "0.13.0"
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
