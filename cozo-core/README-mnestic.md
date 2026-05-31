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

**0.8.3**

- **Native 3-way fused recall** — `hybrid_search` now fuses a graph-proximity leg
  *in-engine* alongside vector (HNSW) and full-text (FTS). A typed `GraphLeg`
  generates a recursive bounded shortest-path rule (k-hop, `min(dist)` scoring) and
  folds it into the same RRF, so one call returns the vector+FTS+graph ranking — a
  capability no other embedded engine here offers. Measured **41.55 ms p50**, ~4×
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
mnestic = "0.8.3"
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
