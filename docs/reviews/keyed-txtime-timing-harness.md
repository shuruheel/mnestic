# Keyed access and TxTime timing harness

The harness implements the measurement boundaries in the
[reviewed protocol](../plans/keyed-txtime-measurement-protocol.md). It is an
internal development tool behind `test-hooks`. Qualifying seeds and release
acceptance remain disabled; the campaign input generator, full frozen run
manifest and inferential analysis are separate prerequisites.

## Measurement boundaries

- `measurement::capture` preallocates 256 thread-local event slots before timing
  the caller closure. Event overflow is explicit and invalidates the sample.
  Each capture has a unique operation ID; a shared monotonic epoch permits
  comparisons across writer threads. Clock samples never use wall time.
- `query_execution` begins after compilation and ends after result construction,
  including sorting, residual checks and materialization. Early errors retain
  incomplete spans. Parse failures have no execution interval. The runner's
  `end_to_end_ns` includes the public script call; parameter conversion, result
  checking, JSON encoding, hashing and output I/O occur outside that interval.
- `commit` surrounds the existing transaction commit funnel. `tt_commit_section`
  captures first acquisition attempt, successful acquisition if any, and release.
  Failed acquisition records its elapsed wait and null acquisition/hold fields.
  The release clocks bracket unlocking: `release_begin_ns` precedes unlock and
  `end_ns` follows it. Hold duration remains an observed upper bound; positive
  overlap coverage uses only the conservative pre-unlock endpoint.
- `tt_ddl_section` and `build_registration`, `build_backfill`, `build_catchup`,
  `build_publication` distinguish DDL phases. SQLite/mem backfill remains inside
  their existing single-writer section; detached catch-up is RocksDB-specific.
- The fixed-schema T2 comparator reads the left input, encodes and fetches right
  keys in batches of 256, checks the residual and builds the same sorted,
  deduplicated result. Its execution interval excludes catalog/transaction setup,
  just as the normal query's execution interval does. The scan iterator is fused
  to prevent a backend cursor restarting after exhaustion between batches.

All measurement hooks disappear without `test-hooks`; instrumentation changes no
schema or storage format. The subsequent admission fix changes production lock
ordering and is covered separately by the TxTime specification. Recording introduces no shared trace mutex. The API is
thread-local: it does not propagate a capture into governed-transaction workers.
The harness uses synchronous, single-query scripts with no triggers or callbacks;
its scalar execution total is not an accounting model for nested query spans.

## Runner and evidence

`cozo-core/examples/keyed_txtime_timing.rs` accepts a JSON plan and a new output
directory. It supports serial operations, concurrent worker schedules, and a
RocksDB build with four paced writers. Build writers first acknowledge a commit,
continue until DDL returns, and each acknowledge one final commit. Missed pacing
slots are recorded. Every thread writes its own raw JSONL outside the engine
interval; errors, panics and missing timing channels remain visible.

The runner accepts only `purpose: development` and seed 99. These small fixtures
validate the harness; they are not the frozen 40k-row reference workload. A
successful `summary.json` means development conformance, not that C2 achieved
natural overlap or that any latency threshold passed.

`scripts/timing_harness.py` provides:

- `fixture`: deterministic seed-99 plans, at most 600 stored rows per relation
  and 64 vector dimensions. Search/update, error, four-writer, FTS/HNSW build,
  and 100-pair recording-on/off fixtures are included.
- `build`: release build with one or two Cargo jobs; source file hashes before/after
  compilation, compiler/native-library provenance and a copied executable hash.
  Source changes during compilation reject the artifact.
- `run`: verify the binary against its build manifest; preserve prelaunch
  provenance, sample process-tree RSS, disk, CPU and swap counters; stop the
  owned process group on limits; record completion separately. Default limits
  are 600 seconds, 4 GiB RSS and 2 GiB minimum free disk for development smoke.
- `report`: reconcile raw rows against planned operations and completion files,
  detect duplicates/missing rows/failed oracles, and suppress overhead estimates
  unless every planned pair is present and valid. Coverage is keyed by measured
  build cell, DDL operation and phase; setup cannot replace it.

The development supervisor records competing CPU and swap activity but does not
implement the qualifying campaign's AC-power/idle/contamination/retry policy.
A supervised process killed before writing its summary is incomplete, not a
successful zero-row run. Overhead reports are descriptive paired diagnostics,
without a release-gate confidence claim or retrospective subtraction from timings.

Example (use a new path for each artifact; native RocksDB environment may be set
before the build):

```sh
python3 scripts/timing_harness.py build "$PWD" /path/to/target /path/to/candidate-build
python3 scripts/timing_harness.py fixture rocksdb keyed /path/to/keyed-plan.json
RAYON_NUM_THREADS=2 OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1 \
  python3 scripts/timing_harness.py run \
  /path/to/candidate-build/keyed_txtime_timing /path/to/keyed-plan.json /path/to/run \
  --build-manifest /path/to/candidate-build/build.json
python3 scripts/timing_harness.py report /path/to/run
```

## Matched baseline

`scripts/prepare_timing_baseline.py` requires a clean detached worktree at
`272c7b7196de9acd8d4aa66ec3e6e2f10f0057ea` (0.19.3). It copies the shared measurement
module/runner and inserts the same query/commit boundaries. It does not copy
optimizer or TxTime-search implementation code. The runner's only baseline
adaptation rejects requests for the absent keyed-access toggle. Baseline plans
must omit `keyed_access`; candidate on/off plans set it explicitly.

The export includes a replayable patch containing new-file contents, file hashes
and the baseline identity. Build/run manifests distinguish the baseline source
and binary from the supervisor script. Candidate-only TxTime lock/build hooks
have no corresponding baseline search path; plain comparisons share the timing
code. Correctness tests that require candidate timeout semantics are not baseline
acceptance tests.

## Validation

On November 14, 2026:

- Five engine instrumentation tests passed, including memory/SQLite/RocksDB
  row equivalence across batch boundaries, duplicate right targets, misses and
  residuals; materialization/error intervals; failed lock acquisition; trace
  capacity, disabled recording, unwind recovery and thread isolation.
- All 37 existing keyed-access/TxTime integration tests passed with RocksDB
  and `test-hooks`. Strict Clippy passed for the library, runner and these
  suites with `-D warnings`. A production-feature build check with RocksDB
  and **without** `test-hooks` also passed.
- Six Python evidence regression tests passed: complete/missing/duplicate pairs,
  panic rows, missing completion, setup/build separation, and binary-manifest
  mismatch. A qualifying-seed plan was rejected before creating a database.
- Candidate and frozen-baseline release binaries built. The complete baseline
  patch passed `git apply --reverse --check` after preparation.
- Nineteen supervised development runs retained 3,490 operation records. Eighteen
  completed their planned conformance checks with zero missing operations or
  oracle/trace failures. Baseline plain/keyed and candidate on/off checks ran on
  SQLite and RocksDB. Four-writer and build checks ran on RocksDB.
- The FTS-create run with four writers at 10 ms cadence timed out at its 60-second
  development deadline before any recorded build phase. All failed writer/DDL
  observations remain in the incomplete report. A **separate diagnostic** at
  1,000 ms cadence completed and captured all build phases; it does not replace
  the failed run or change the qualifying cadence.
- HNSW-create completed with writer overlap during backfill and the publication
  phase envelope, but none during the acquired publication TT section or
  registration. The [diagnostic investigation](keyed-txtime-diagnostics.md)
  corrects the older report's publication-coverage interpretation. The lower-load FTS diagnostic has no writer overlap in any
  phase. Neither establishes the qualifying C2 coverage gate.

The 100-pair on/off diagnostics produced p50 ratios of 1.0010/1.0005 for
SQLite/RocksDB keyed reads, 1.0265/0.9856 for their TxTime updates, and 1.0182
pooled across four RocksDB writers. Individual writer ratios ranged from 0.9401
through 1.2200: contention noise remains substantial. These are descriptive
instrumentation diagnostics, not evidence that instrumentation bias is negligible
or that a specification threshold passes. No overhead is subtracted from latency.
The minimum observed positive clock step was 41 ns. One-second resource sampling
observed at most 79.6 MiB process-tree RSS and at least 3,853 MiB free disk; it
cannot exclude shorter unsampled peaks.

[Machine-readable run summaries](keyed-txtime-harness-validation.json) record
binary identities and the SHA-256 of the full artifact index. The local raw root
is `/Users/shuruheel/Code/mindgraph/benchmark-artifacts/keyed-txtime-harness-20261114-v1`.
It contains complete plans, prelaunch source/binary manifests, JSONL events,
resource CSVs, failures, baseline patch, build logs and per-run reports. Database
fixtures are disposable and excluded from the raw-evidence index.

Independent read-only storage review found no blocking issue in the engine
seams. Independent experiment review found four evidence defects (binary/source
provenance, incomplete baseline patches, setup overwriting build coverage, and
partial overhead reports); all were corrected and closure-reviewed with no
remaining development-harness blocker. The review does not approve qualifying
execution. The FTS progress timeout, natural C2 phase coverage and contention
measurement noise need resolution before the expensive campaign.

The smoke binaries precede one lint-only runner edit: `&e.phase == p` became
`e.phase == p` in the post-timing phase-presence check. Independent review verified
that this is the sole source drift from the candidate build manifest; engine
hooks, schedules and timing semantics are unchanged. The original executable
hashes and per-file build hashes are retained rather than attributing those runs
to an unbuilt final commit. The final results review independently verified all
1,068 indexed hashes and approved the bounded observations above.

Subsequent [diagnostic findings](keyed-txtime-diagnostics.md) identify relation
admission starvation and confounding in the contention overhead fixture. The
reporter now requires confirmed-held section overlap, and the original evidence
is preserved unchanged. Original validation counts above describe that earlier
binary; diagnostic validation is recorded separately.

## Admission and replacement recording controls

The [measurement amendment](../plans/keyed-txtime-measurement-amendment.md)
retains the specification latency gates and adds the union of transaction/
relation admission and TT waits, avoiding double-counting nested spans. Candidate
traces also expose maintenance admission. The old diagnostic evidence remains
preserved and does not qualify the integrated candidate.

`scripts/run_overhead_controls.py` freezes all 18 matched-snapshot controls before
launch. Each on/off pair runs identical operations after 100 read-only probes,
using persistent workers and barriers before and after all captures. Snapshot
copy, open, warm-up, output encoding and validation are excluded from timing and
recorded separately. `scripts/overhead_controls.py` checks exact rows, indexes,
history counts, complete pairs, hashes and resource limits before estimating
paired block-bootstrap and IID sensitivity bounds. Any failed cell blocks
freezing the qualification campaign; no overhead is subtracted from results.

Independent implementation review closed the snapshot, oracle, postprocessing,
provenance and budget findings. The complete K1-K4/P1/T1/T2/C1/C2/H1 campaign
implementation and qualification remain prerequisites to merge acceptance.
