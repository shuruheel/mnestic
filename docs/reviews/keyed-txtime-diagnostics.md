# Keyed/TxTime diagnostic findings

Development investigation of the issues exposed by the timing harness. The
[diagnostic design](../plans/keyed-txtime-diagnostics.md) was independently
reviewed before new runs. No qualifying thresholds or inputs were changed.
Original evidence remains under `keyed-txtime-harness-20261114-v1`.

## FTS timeout: exclusive relation admission is starved

The original FTS-create call spent 59.8877 seconds before its deadline, without
reaching registration. Writers completed 578 operations during that run. The
union of their commit intervals covers **100% of the DDL call interval**, with
no uncovered gap. In this synchronous single-mutation path, `execute_single`
holds its shared relation guard through `commit_tx`; commit intervals are thus a
conservative lower bound on shared ownership, even in the older traces that lack
explicit admission events.

The build requests exclusive relation ownership before reserving detached index
IDs. `relation_write` polls `ShardedLock::try_write` every 2 ms. The pinned
Crossbeam implementation releases partially acquired shards on `WouldBlock`;
it does not reserve admission against future readers. A continuously overlapping
set of shared holders therefore prevents this build from entering. The observed
failure is starvation at admission, not backfill work or a global deadlock.

New `test-hooks` events distinguish `relation_write_admission`, synchronous
`relation_read_admission`, and `build_reservation`. Production locking policy is
unchanged by this investigation. A diagnostic writer-drain control waits for each
writer's first acknowledgement before permitting drain, so it cannot manufacture
a readiness/deadline cycle.

## Overlap: a phase envelope is not a held TT section

The older HNSW report counted one writer wait overlapping the publication phase.
The phase starts before exclusive relation admission. Reanalysis against the
recorded acquisition-to-post-unlock interval finds **zero** overlapping writer
waits. This older interval is an upper bound; positive intersections would
not alone prove overlap while the lock was held.
Backfill did have eight overlapping waits, but neither registration nor the
acquired publication section had one. Publication's envelope was 371.4 ms and
its TT-section event lasted 368.4 ms; those durations do not imply that a writer
was waiting on that section.

This is structurally relevant: the builder drains shared holders before taking
exclusive relation ownership for publication. New mutations of that same
relation then wait before reaching the TT commit section. A same-relation
TT-wait-only metric cannot represent all build-related writer delay. The low-load
FTS diagnostic also lacked writer overlap in every phase; a 14-microsecond
registration interval gives sparse opportunities under 10 ms pacing.

The reporter now distinguishes phase-envelope overlap from intersection with an
**confirmed-held** TT section. New events capture `release_begin_ns` immediately
before unlocking. `covered` for registration/catch-up/publication requires overlap
with `[acquired_ns, release_begin_ns)`. The old post-unlock end timestamp remains
useful only for potential overlap. Backfill retains its phase-envelope criterion.
Empty intervals and endpoint contact cannot count as overlap. Original reports are not
rewritten or retroactively qualified.

## Overhead: worker identity does not match contention position

All 400 writer pairs used different mutation parameters between recording-on
and recording-off trials, and each trial mutated the index/history state used by
the next one. Writers also changed finish rank in **316/400** pairs; 99/100 rounds
had at least one rank change. The per-writer ratios therefore confound recording
with queue position, mutation cost and evolving state. They cannot isolate
instrumentation cost, even though their schedules were complete.

The reviewed exploratory reanalysis retains all four writers and compares each
round's `max(end_ns) - min(start_ns)`. Nearest-rank quantiles and 10,000 paired
block-bootstrap draws (ten-round blocks, seed 20261114) give:

| Four-writer round statistic | On/off ratio | Exploratory 95% interval |
|---|---:|---:|
| p50 makespan | 1.0807 | [0.9613, 1.1384] |
| p99 makespan | 0.9687 | [0.9508, 1.0736] |

These conditional intervals do not remove the unmatched mutations or establish
negligible bias. Changing the aggregate reduces reliance on writer identity;
it does not turn the original experiment into a controlled overhead estimate.

## Required follow-up before qualification

1. Implement deadline/cancellation-aware relation admission that prevents a
   pending exclusive build from being bypassed by unlimited new mutations.
   Review lock ordering, multi-relation acquisition, nested query behavior,
   failure cleanup and both read/write deadlines before changing policy.
2. Amend and independently review C2's workload/coverage definition. Retain
   same-relation end-to-end and relation-admission measurements, and add writers
   on an independent TxTime relation to exercise the shared TT section directly.
   Explicitly decide how admission delay is gated. Do not silently reinterpret
   the existing threshold or use phase-envelope overlap to satisfy section
   coverage. Existing qualification stays disabled until this is resolved.
3. Use identical-operation controls with matched starting state for a recording
   cost estimate. Preserve read-only controls, separate mutation/contended cases,
   use persistent workers and analyze complete rounds. Freeze that revised
   diagnostic before collecting a new confirmation set; do not tune away the
   earlier noise or subtract the old estimate from qualifying timings.

`scripts/analyze_timing_diagnostics.py` reproduces the exploratory trace and
round analysis. Test coverage includes incomplete-round rejection, rank swaps
with unchanged makespan, interval unions, and false publication coverage while
waiting for admission. This investigation diagnoses the failures and corrects
reporting; it does not claim a production starvation fix or release acceptance.

## New diagnostic execution

The two prescribed fresh-database cells ran once each with the same release
executable (`a15ca0e79a021411a6aa2f637bcbb68d45623787770342bbfc3c419d3281c16d`).
Source identity is `dbc3a608` plus the retained diagnostic patch; per-file hashes
match before/after compilation. The native RocksDB archive is the pinned vendor
revision compiled at `-O0`, copied and hashed before building. These are bounded
mechanism diagnostics, not cross-version latency comparisons.

| Observation | Sustained arrivals | Stop new arrivals after 2 s |
|---|---:|---:|
| DDL outcome | Deadline error | Completed |
| DDL elapsed | 19.9316 s | 2.0911 s |
| Initial exclusive relation admission | 19.9314 s, incomplete | 2.0875 s, acquired |
| Shared-guard coverage of admission wait | 100% | 99.9679% |
| Backfill elapsed | Never entered | 3.224 ms |
| Successful writer operations | 276 | 49 |
| Writer errors at deadline | 4 | 0 |

The sustained case has no successful exclusive admission or registration. In the
drain case, the final drain notification precedes DDL completion by 3.598 ms;
all four writers subsequently acknowledge a final successful commit. All planned
verification checks pass, including an expected historical-search error. No
writer overlaps registration, backfill or publication after draining; this is
an admission control experiment, not a replacement C2 pass.

These new admission events directly confirm the earlier source/trace inference:
continuing shared holders starve exclusive admission; stopping arrivals lets the
pending build complete promptly. They establish this mechanism for the tested
fixture, not a general bound or starvation-free scheduling guarantee.

Both cells stayed within the supervisor limits. One-second sampling observed
at most 77.1 MiB process-tree RSS and at least 4,914.8 MiB free disk; it cannot
exclude shorter unsampled peaks. Background `fileproviderd` exceeded 100% CPU
in 12/21 sustained samples and 1/3 drained samples. This was not a quiet-host
latency comparison; the evidence supports the observed admission mechanism.
The failed sustained run, four writer errors,
and missing final verification remain in the incomplete report. No retries or
replacement measurements were taken.

Two earlier build attempts are preserved separately: source-identity validation
rejected the first, and shared-target files disappeared during the second. The
successful third attempt used an isolated Cargo target and the copied native
archive. Build failures are not database experiment results.

Validation and artifact identities are recorded in
[keyed-txtime-diagnostics-validation.json](keyed-txtime-diagnostics-validation.json).
The local raw root is
`/Users/shuruheel/Code/mindgraph/benchmark-artifacts/keyed-txtime-diagnostics-20261114-v1`.

Six Rust measurement tests and eleven Python regression tests passed. Strict
Clippy passed for the library and runner, and a RocksDB library check without
`test-hooks` passed. Independent storage review accepted the hook boundaries;
independent experiment review accepted the design, coverage correction and
bounded raw-result conclusions, including the background-load limitation.

Reproduce validation with the native-library environment and isolated target from
`build-3/build.json`:

```sh
cargo test --locked --release -p mnestic --features storage-rocksdb,test-hooks \
  --lib measurement -j 2 -- --test-threads=1
cargo clippy --locked --release -p mnestic --features storage-rocksdb,test-hooks \
  --lib --example keyed_txtime_timing -j 2 -- -D warnings
cargo check --locked --release -p mnestic --features storage-rocksdb --lib -j 2
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p test_timing_harness.py -v
```
