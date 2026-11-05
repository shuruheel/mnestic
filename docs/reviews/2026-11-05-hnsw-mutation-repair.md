# 0.19 HNSW mutation repair validation

Implemented and locally validated at immutable source
`e606c661b3a863f6372789aee466ffb68c7d35ba`. Explicitly selected for 0.19 by the owner.
This is correctness/package evidence, not release acceptance or performance
qualification. The [contract](../specs/hnsw-mutation-repair.md) owns semantics,
limits, errors and rebuild guidance.

## Change and evidence

The [prior investigation](2026-11-05-hnsw-update-investigation.md) established
disconnection on released 0.18 and the pre-repair candidate. The deterministic
three-node regression failed before implementation: deleting bridge 1 returned
only key 0, omitting surviving key 2. Preserve [before.log](0.19-hnsw-repair/before.log).
The fixture subsequently gained nullable-schema corruption controls and additional
cases; the original bridge geometry and expected survivor set are unchanged.

The implementation substitutes directed bypass paths before removing a vertex.
Both repair pruning and incremental insertion pruning retain necessary paths or
return an explicit limit error. Reverse ignored-edge records and live degree counts
are maintained; sibling vector fields/list positions have distinct identities.
Online-build reconciliation caches disappearing snapshot vectors and removes stale
nodes before inserting current rows. An interrupted mutation prevents commit,
including when a legacy client catches its error and executes another statement.

Final local validation:

| Check | Result |
|---|---|
| Engine library, default + RocksDB + test hooks | 398 passed |
| Focused integration suites | 85 passed; timing-only `measure_reader_unblock` ignored |
| Strict Clippy, all targets, same features | Passed with `-D warnings` |
| Minimal-feature compilation | Passed with default features disabled |
| Rust crate packaging | Immutable source SHA and new module content verified |
| Independently extracted crate host | Fresh dependency resolution without private overrides; SQLite bridge-deletion smoke passed |

Integration totals: FTS transaction statistics 9, governed transactions 31,
HNSW build 5, HNSW correctness 4, mutation repair 9, nonblocking HNSW build 5,
hybrid helper 13, reindex 9. The nine mutation tests include a 210-fixture matrix:
seven mutations × ten fresh random builds × memory/SQLite/RocksDB. Each compares
unfiltered and rare-type query results against base-row eligibility at ef=50 and
1600, with k above the tiny corpus size. It also checks stored degrees. These
assertions establish the exercised regressions, not an estimated production
failure rate, exact ANN guarantee or broader filtered-recall acceptance.

Other fixtures cover deterministic bridge deletion, duplicate/distinct replacement,
entry-point/last-node deletion, repeated mutation and reopen, composite keys,
index predicates, vector lists/fields, script/explicit abort, injected failed
commit, snapshot reconciliation and incomplete legacy-transaction commit rejection.
The upper-layer failure fixture proves lower-layer repair writes occurred before
the error, a later successful statement cannot clear the guard, and no graph/base
change becomes visible after the rejected commit.

## Independent storage-contract review

A bounded read-only reviewer inspected the implementation and contract, without
editing or running builds. Four initial findings were corrected before acceptance:

1. Replacement insertion could prune a repaired bridge: added path-preserving
   incremental pruning and an explicit degree-pressure fixture.
2. Reconciliation could read disappeared snapshot vectors: cached the old vectors
   and ordered removal before current-row insertion; added snapshot-neighbor coverage
   and checked vector field/list access.
3. Extended-candidate shrinking could create edges without reverse records:
   shrinking now selects only from existing outgoing neighbors; upgrade guidance
   requires rebuilding legacy indexes to restore the prerequisite invariant.
4. Partial repair errors could remain committable through legacy transactions:
   added an incomplete-mutation counter in the common commit funnel and tested
   failure after lower-layer writes, including a later successful statement.

The final re-review reported no outstanding findings. It also corrected the limit
description: incident-vector components are capped per layer, not across retained
or reconciliation caches. This static review does not establish performance or
constitute independent execution of the reported tests.

## Reproduction and retained artifacts

The [manifest](0.19-hnsw-repair/manifest.json) records source/toolchain identity,
code/log hashes, fixture counts and the packaged crate hash. Full final test,
Clippy, minimal-build and package/host logs are alongside it. The qualification
crate is retained under ignored `release/0.19-qualification/e606c661/crate/`.
Its package version remains 0.18.0 until final release version synchronization.
Packaging warnings about omitted integration-test files are expected from the
existing package allowlist; the new production `src/runtime/hnsw/repair.rs` is
included and byte-verified. The extracted host builds that packaged module.

Native validation used Rust 1.98.0, `-j 2`, debug information disabled and a
compatible cached debug RocksDB archive. No optimized wheel or timing result is
claimed from those settings. The old optimized candidate wheel was preserved;
only its regenerable release build cache was removed when disk space was low.

```sh
cargo test -p mnestic --locked --features storage-rocksdb,test-hooks --lib \
  --test hnsw_mutation_repair --test hnsw_correctness --test hnsw_build \
  --test hnsw_nonblocking_build --test reindex --test hybrid_helper \
  --test governed_transaction --test fts_transaction_stats -j 2
cargo clippy -p mnestic --locked --all-targets \
  --features storage-rocksdb,test-hooks -j 2 -- -D warnings
cargo check -p mnestic --locked --no-default-features --features minimal -j 2
```

## Release boundary

Rebuild pre-0.19 HNSW indexes using `::reindex <relation>` and verify results
against exact base/vector queries. The fix prevents new disconnections under the
documented graph invariants; it cannot discover hidden legacy incoming edges or
repair existing disconnected components without rebuilding. Local degree/work
limits may reject a mutation, including an incremental insertion. The failed
transaction must be aborted; repeated retries do not repair its configuration.

The former engine `0148693b` package/campaign evidence remains a pre-repair control.
Before release measurements, qualify new optimized Rust/Python artifacts and freeze
their hashes with the repaired candidate SHA. E1–E3 remain deferred to the owner's
quiet-window signal. Hosted CI, downstream compatibility, final version sync and
publication are separate remaining release gates. No production rebuild, remote
push, release tag or publication was performed here.
