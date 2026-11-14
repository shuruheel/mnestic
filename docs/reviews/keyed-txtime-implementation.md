# Keyed access and TxTime search: implementation qualification

Status: implemented on `spec-planner-keyed-access`; draft PR qualification.
No release or performance acceptance is claimed.

## Scope

One branch combines [planner keyed access](../specs/planner-keyed-access.md)
and batch 1 of [TxTime search indexes](../specs/txtime-search-indexes.md).

The planner adds local trailing-equality bindings after join ordering and magic
rewriting, and full-primary-key point joins with residual value checks. Existing
producers, multiplicity, secondary seeks, temporal gates, and volatile/fallible
expression evaluation are preserved. Conservative eligibility exclusions are
recorded in the specification. Consumer lint remains in place.

TxTime indexes contain the current belief under each logical key. FTS, HNSW,
and LSH maintenance commits atomically with corrections, retractions, imports,
reconciliation, and exact-version eviction. RocksDB builds private generations
from a pinned snapshot, records bounded touched keys, catches up, and publishes
atomically. SQLite/mem retain their single-writer build model.

Historical search, B-tree indexes, bitemporal search, triggers, callbacks and
projections are outside this batch. Historical base reads retain their existing
semantics. TxTime index/catalog DDL, history GC and corrupt-base repair require
standalone commands. Repair requires attached search indexes to be dropped first.

## Source and review

- `e1eef8a6`: initial planner implementation.
- `01bc71f6`, `7ed173e2`, `43e47c08`: planner review fixes for expression
  evaluation, vector equality, membership and existing filtered seeks.
- `a385cde9`: initial current-belief index implementation.
- `63a3e59f`: storage review fixes and lifecycle regression coverage.
- `eed47efc`: LSH signature identity and JSON signed-zero regression.

Independent bounded read-only reviews completed for both subsystems. All
reported findings were addressed on source verification. The reviews did not
run builds or establish performance acceptance.

The storage review found and corrected catalog-lock ordering, attached-index
rename identity (including manifests and LSH inverse handles), corruption repair
racing with a first build, and LSH no-op identity. A commit notification guard
invalidates in-flight builds after an ambiguous storage commit error or unwind.

## Correctness evidence

Local environment: macOS arm64, Rust 1.98.0, Cargo builds limited to `-j 2`,
`CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_INCREMENTAL=0`. Tests use temporary databases.
RocksDB used the existing local native archive and headers for the repository's
pinned vendor revision `986b8b9f20893dec811d8ecdb97b6a47f20d322d`.
This is local source-test evidence, not a published package test.

| Suite | Passing tests | Coverage |
|---|---:|---|
| `keyed_access` | 16 | On/off row equivalence and plans on mem, SQLite, RocksDB; complete positive consumer lint shapes with declared schemas/string parameters |
| `txtime_search_indexes` | 11 | All three backends: current beliefs, multi-row graph changes, retract/reassert, imports, reconciliation, eviction, rollback, standalone guards, rename, historical-search errors; SQLite/RocksDB backup/restore and reopen; JSON signed-zero LSH |
| `txtime_index_concurrency` | 9 | RocksDB: writers during create/rebuild, old-generation availability, bounded-log overflow, drop/rename/repair cancellation, exact eviction race, legacy/governed writers, failure injection at backfill/publication, publication deadline and catalog locks |
| Library filter `txtime` | 23 | Existing temporal invariants plus commit-section timeout without allocating a transaction time |

Commands (from the worktree; local native-library environment described above):

```sh
cargo test -p mnestic --features storage-rocksdb,test-hooks -j 2 \
  --test keyed_access --test txtime_search_indexes \
  --test txtime_index_concurrency -- --test-threads=1
cargo test -p mnestic --features storage-rocksdb,test-hooks -j 2 \
  --lib txtime -- --test-threads=1
```

The existing `fork_regressions`, `matjoin_regression`, `planner_shape`,
`recursive_workload`, `reindex`, `fts_transaction_stats`, `fts_governed_stats`,
`fts_lsh_update_leak`, `hnsw_correctness`, and `hnsw_mutation_repair` suites also
passed during implementation: 55 tests, with four existing ignored tests.
The committed planner baseline was not changed. Final lint/regression results
and hosted CI state are recorded in the PR.

Failure injection returns before the actual storage commit through the existing
`test-hooks` seam. It proves transaction rollback and publication behavior for
that boundary; it does not reproduce an operating-system crash or ambiguous I/O
completion. Detached builds may leave unreachable reserved generations after a
crash or ambiguous commit; cleanup never deletes a possibly attached generation.

## Remaining qualification

The PR stays draft until the following acceptance work is complete:

- Final hosted CI, including the release-mode LSQB and deep-recursion tiers.
- Keyed-access warm execution benchmarks on SQLite/RocksDB: 10k versus 1M
  existence probes, the 10k-left/1M-right join versus baseline and `multi_get`,
  and the adversarial composite-key/greedy controls. Compilation must be excluded.
- TxTime 40k-row search and update latency comparisons, four-writer section
  contention, and commit waits during FTS/HNSW builds, using the specification's
  unchanged thresholds. No gate is relaxed by the passing correctness tests.
- Freeze an immutable candidate and benchmark protocol, resource budget, inputs,
  and raw-output location; obtain the required experiment review before the
  costly comparison. Run without competing engine/native workloads on this 8 GB
  host. Do not treat the deterministic concurrency fixtures as latency evidence.

The later historical-vector and B-tree batches retain their separate demand and
measurement gates. This combined PR does not authorize them.
