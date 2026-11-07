# 0.19 transactional FTS statistics — November 5, 2026

The rollback defect reported by [PR #69](https://github.com/shuruheel/mnestic/pull/69)
is independently reproduced on released 0.18.0 and current main. The contributor's
fix has been applied on top of current main with additional governed-transaction,
hybrid-result and Python regression coverage. This record is local implementation
evidence; the final 0.19 candidate and hosted package matrix remain unqualified.

## Source and attribution

- Starting engine SHA: `d60da15d5798b2be9fa4b8de515f68426f15daaa`.
- Reviewed PR head: `cf0dc189f4d7d75eec7d2d955933f690882c62a1`.
- Contributor commits `c502b25c` and `cf0dc189` were cherry-picked with authorship
  preserved onto `work/0.19-fts-stats` as `af77dc08` and `b185d38f`.
- Follow-up changes add `fts_governed_stats.rs`, a Python rollback regression,
  workflow coverage and release notes. The contributor's test fixture uses arrays
  instead of a temporary vector to pass strict Clippy; its semantics are unchanged.
- No dependency versions, private Cargo patches, ranking formula or persisted
  storage format are changed.

## Reproduction before the fix

The three-document corpus has two `alpha` matches and one long nonmatching
padding document. A script removes the padding document, then fails on a missing
relation. All three base rows survive rollback, but the old shared statistics
retain the deletion's delta:

| Result | Before failed script | After rollback, old implementation |
|---|---:|---:|
| score(a) | 0.8145200573753216 | 0.21772010539904393 |
| score(b) | 0.75382517568058 | 0.23382944055188795 |
| top result | a | b |
| stored primary keys | a, b, c | a, b, c |

The installed Python baseline reproduces this on mem, SQLite and RocksDB. Its
extension SHA256 is `9225c1d969d1562010082c794b3841520ba8386875b6b132b51374b7884610f4`.
A separately downloaded release wheel,
`mnestic-0.18.0-cp37-abi3-macosx_11_0_arm64.whl`, contains the identical extension;
wheel SHA256: `1c042b400ad3ad464e1c6a0923e4f933b36f59d367dddbb3bc2866d57cbe7abc`.

On current-main native code, the imported abort, failed-script and rejected-commit
regressions fail (2 controls pass, 3 regressions fail). The added governed tests
also fail independently on both mem and SQLite after explicit abort. These failures
occur before applying the fix, with actual rows restored and scores changed.

## Review and additional coverage

The source review followed cache acquisition before snapshot creation, posting
mutation invalidation, commit publication, failed-commit teardown, index rebuild/
removal and raw restore. Cache locks are released before storage scans. The
transaction-local guard outlives `store_tx` on rollback; commit publication follows
successful storage commit. Older or overlapping snapshots cannot publish their
absolute counts over a newer committed corpus. Partial posting errors prevent
commit rather than silently accepting an incoherent index.

The additional governed test exercises explicit abort, query error, cancellation,
client drop, successful commit and commit-failure hooks. It checks restored base
rows, FTS scores, and vector+FTS hybrid results including per-leg contributions.
Workers are joined before verifying post-transaction state. Each backend runs its
own fixture, and successful commit is a positive control for local-stat promotion.

The contributor's native tests cover old warm/cold RocksDB snapshots, open writers,
overlapping commits in both orders, index recreation/reindex, actual storage
failures, and warm-cache scan counts. Conservative rescans after overlap remain a
performance tradeoff to measure in E1; passing correctness tests is not a latency
claim. No separate reviewer has yet signed off the final release candidate.

Captured red/green test output and SHA256 hashes are retained in the
[validation log manifest](0.19-fts-stats/manifest.json). Only trailing whitespace
in text transcripts is normalized; captured and stored hashes are both recorded.

## Native validation

Rust/Cargo 1.98.0, macOS arm64; two build jobs, incremental compilation disabled,
dev/test debug symbols disabled. Tests used a dedicated temporary target directory.

| Configuration | Result |
|---|---|
| `--no-default-features --features minimal,test-hooks`, focused FTS/governed suites | 33 passed |
| Default features plus `storage-rocksdb,test-hooks`, selected integration suites | 105 passed |
| Default features plus `storage-rocksdb,test-hooks`, engine library suite | 391 passed |
| Strict Clippy on library and three FTS regression targets with RocksDB/test-hooks | Passed |

Selected integration targets: `fts_transaction_stats`, `fts_stats_failures`,
`fts_governed_stats`, `governed_transaction`, `fts_corpus_stats`, `fts_avgdl`,
`fts_candidates`, `fts_phrase`, `fts_prefix`, `reindex`, and `hybrid_helper`.

Representative commands (from the engine root):

```sh
cargo test --locked -j 2 -p mnestic --no-default-features --features minimal,test-hooks \
  --test fts_transaction_stats --test fts_stats_failures \
  --test fts_governed_stats --test governed_transaction
cargo test --locked -j 2 -p mnestic --features storage-rocksdb,test-hooks --lib
cargo clippy --locked -j 2 -p mnestic --features storage-rocksdb,test-hooks \
  --lib --test fts_transaction_stats --test fts_stats_failures \
  --test fts_governed_stats -- -D warnings
```

For local RocksDB validation, `ROCKSDB_LIB_DIR` and `ROCKSDB_STATIC=1` reused the
compatible native archive already built from this workspace's unchanged bridge/
RocksDB sources. The Rust engine and bridge compiled against the current headers;
these are real RocksDB runtime tests, but not a fresh from-source RocksDB package
build. The default graph feature also compiled without Kaeru's reported override;
that does not independently validate Kaeru's complete dependency graph.

CI now explicitly includes the FTS/governed tests in the SQLite and RocksDB
`test-hooks` steps. Python wheel jobs run the binding regression on all three
backends; the sdist job runs mem/SQLite because RocksDB is wheel-only.

## Python binding validation

A separate macOS arm64 ABI3 wheel was built from this tree with
`compact,storage-rocksdb,rdf-io,columnar-io`, the release workflow's feature set.
The wheel required a fresh local RocksDB build after the earlier shared archive
was removed. It was installed into `/tmp/mnestic-019-python`, leaving the released
baseline environment unchanged. The rollback/ranking regression passes on all
three backends, and all five existing string/JSON/FTS compatibility tests pass.
The validation log manifest records the wheel SHA256.

This is an unoptimized development wheel, with debug symbols disabled and
`PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1` for the local Python 3.13 interpreter. It
retains package version 0.18.0 and is not a publishable 0.19 candidate. Native
cross-platform packaging and optimized benchmark equivalence are not established
by this local wheel. Existing PyO3 deprecation and upstream C++ build warnings
remain outside this patch.

The committed PR #65 reproducer (`mnestic-benchmarks@0ce1843`) produces the same
144 fixture outcomes against this wheel as the released baseline. No latency
comparison is made between different build profiles.

## Packaged native host

`cargo package --locked --allow-dirty --no-verify -p mnestic` produced the local
crate artifact recorded in the log manifest. The artifact was extracted into a
separate directory and consumed by a standalone Cargo project with default
features, no `[patch]` section and no private dependency overrides. Fresh dependency
resolution, compilation and execution of `examples/governed_transaction.rs` all
passed. The host verifies committed data and retains admission until the worker
has exited. Its manifest, dependency lockfile and command output are retained
alongside the validation logs.

This establishes a local packaged native-host smoke check, including default graph
dependencies. It does not reproduce Kaeru's application-specific dependency graph.
The example is included in the crate for hosts to reuse.

## Remaining release gates

Hosted CI for the final combined SHA, the full wheel/sdist/adapter matrix, repetition of the packaged host check at
the final candidate SHA, 0.18-to-candidate upgrade qualification, independent
patch review and E1–E3 measurements remain pending. Warning removal is a separate
implementation slice. No package was published and no GitHub PR was merged by this
local work.
