# Admission fix and recording-control results

November 14, 2026. **Merge qualification remains blocked.** The admission fix
passes correctness checks and both prescribed sustained-writer development
runs. The replacement recording-control campaign does not pass its frozen
acceptance rule. No qualifying performance run or release acceptance is claimed.

## Source and correctness

Candidate engine source: `0cedce380a40280e27899b316968c48eff9afca5`, integrating
main `272c7b7196de9acd8d4aa66ec3e6e2f10f0057ea` (0.19.3). The baseline exports
only the shared measurement code and runner. Both release executables use the
same compiler, flags, native archive and headers. The native archive is the
pinned RocksDB vendor compiled at `-O0`; these runs do not establish a numerical
speedup over earlier binaries.

FIFO relation admission prevents later mutations overtaking queued maintenance.
A brief global gate drains native-lock-owning transactions before maintenance
acquires its target relation guards. Maintenance releases global admission before
storage work. Native-lock-free buffered TT transactions may release admission
between statements and therefore still span index attachment or rebuild. Guard
ordering, cancellation and deadlines have deterministic regression coverage.
Independent storage review found no remaining blocker in this implementation.

Local checks passed: 421 release library tests (one existing debug-only panic
test excluded), 31 governed-transaction tests, 11 TxTime concurrency tests,
strict Clippy for library/tests/runner, and a production RocksDB plus columnar
build check without `test-hooks`. Twelve timing/diagnostic and five control
reporting tests passed. Prior failed attempts remain in the evidence directory.

## Sustained-writer development checks

The [reviewed plan](../plans/keyed-txtime-admission-validation.md) prescribed one
fresh RocksDB FTS run followed by one HNSW run, seed 99, 120 rows, 64 dimensions,
four writers, 10 ms pacing, a 20-second operation deadline and a 90-second
supervisor cap. Neither run uses a writer-drain intervention.

| Observation | FTS | HNSW |
|---|---:|---:|
| DDL completion | 339.273 ms | 244.384 ms |
| Successful writer operations | 27 | 19 |
| Writer errors | 0 | 0 |
| Final writer acknowledgments | 4 | 4 |
| Maximum per-operation union of admission and TT waits | 415.794 ms | 344.917 ms |

All planned operations, oracles and traces passed. Each report's one error is
the expected historical-search rejection. Independent results review accepted
completion with continuing arrivals in these fixtures. Registration/catch-up
still lack naturally overlapping writer calls, and publication has no confirmed
TT-wait intersection. These runs do not qualify C2 or establish a general wait
bound. One-second resource samples stayed within limits; the short runs cannot
establish actual memory peaks.

## Recording-cost campaign: failed acceptance

The [reviewed amendment](../plans/keyed-txtime-measurement-amendment.md) freezes
18 controls: baseline plain, candidate plain and candidate TT, each on SQLite
and RocksDB with read, single-writer and four-writer cases. All 18 were attempted
once in 74.072 seconds. Scripts, protocol, plans and executable hashes remained
unchanged. No resource stop or retry occurred. Sampled process-tree RSS was at
most 75.063 MiB and free disk at least 4,019.656 MiB; these are sampled values.
Background `fileproviderd` reached at least 100% CPU in one baseline RocksDB
single-writer sample and two candidate RocksDB TT read samples (up to 101.2%).
This was not a uniformly quiet host. No affected sample or cell is excluded.

Six read controls completed all 100 on/off pairs. Their one-sided 95% upper
bounds for p99 recording-on/off ratios all exceed the predeclared 1.05 maximum:

| Read control | p50 upper bound | p99 upper bound |
|---|---:|---:|
| Baseline SQLite plain | 1.015 | 2.080 |
| Candidate SQLite plain | 1.017 | 1.265 |
| Candidate SQLite TT | 1.004 | 1.294 |
| Baseline RocksDB plain | 1.008 | 1.817 |
| Candidate RocksDB plain | 1.005 | 2.581 |
| Candidate RocksDB TT | 1.005 | 1.526 |

Each bound is the larger paired block/IID bootstrap upper bound. The median
bounds pass; the tail bounds do not. This does not establish those values as
causal tracing overhead: unresolved tail uncertainty also fails the rule. No
threshold is relaxed and no overhead is subtracted.

All twelve mutation controls stopped at their first exact-current-row oracle.
A separate single-operation diagnostic saved the returned row: its scalar and
vector values match the expected values when decoded in Python, while the
runner's JSON-number equality rejects the oracle. The corrected oracle compares
vectors exactly within their declared F32 domain (`emb == vec($expected_vector)`)
and asserts the returned key, text, kind and equality result. It uses no numeric
tolerance. A separately labelled ten-operation check passes, including FTS and
HNSW checks. The original failed controls remain failed; neither diagnostic is
a replacement control or an accepted latency sample.

## Remaining gate

The recording-cost gate blocks freezing the full qualification campaign. A new,
explicitly reviewed control phase must address tail uncertainty and use the
corrected oracle before any repeat campaign. Full K1-K4/P1/T1/T2/C1/C2/H1 input
generation and inferential analysis remain outstanding. Final hosted CI is also
required before merge. The PR remains draft.

Artifact identities and compact results are in
[keyed-txtime-admission-validation.json](keyed-txtime-admission-validation.json).
Raw evidence is retained at
`/Users/shuruheel/Code/mindgraph/benchmark-artifacts/keyed-txtime-admission-20261114-v1`.
The regenerable Cargo cache was removed after both executables and manifests
were preserved; no raw result, source worktree or native archive was removed.
