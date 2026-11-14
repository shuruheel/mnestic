# Keyed access and TxTime search: measurement protocol

Prepared November 14, 2026 for draft PR #81. Status: independently reviewed design;
**not yet runnable or performance-qualified**. See the
[review record](../reviews/keyed-txtime-measurement-protocol.md). The harness,
input artifacts and executable hashes must be frozen before qualifying runs.

This protocol operationalizes the unchanged gates in
[planner keyed access](../specs/planner-keyed-access.md#acceptance-tests) and
[TxTime search indexes](../specs/txtime-search-indexes.md). Passing correctness
tests does not satisfy these gates. Historical indexed search and B-tree indexes
are outside this batch. The historical lookup below uses the existing base scan.

## 1. Comparison identities and freeze

| Item | Identity |
|---|---|
| Candidate implementation | `851404aec49bebf58f67a4a854326b1fca01c3e2` |
| Baseline, common ancestor before these features | `595fdaa6f3327fdd737b0fa0151cc922d624edbb` |
| Reference workload repository | `mnestic-benchmarks`, `984d1ae9bf9e8856e0ac99f3083de77c9688173e` |
| Workload definition | `configs/scales.yaml:small`, `hybrid_recall/workload/generate.py` |
| RocksDB vendor | `986b8b9f20893dec811d8ecdb97b6a47f20d322d` |
| Development-only seed | 99, at most 2,000 rows, 64 dimensions |
| Qualifying dataset seeds | 1234, 1235, 1236; one independently built database per seed/arm/cell |

The baseline includes the previously merged FTS persisted-statistics feature.
Main observed during preparation is `cd039989686472a4546af203906450536eae738b`,
which additionally includes FTS fold/repair. That revision is **not** covered by
the comparison above. Integrating it requires new candidate/baseline identities,
an interaction review and affected correctness/performance requalification before
PR readiness. Do not silently substitute a newer branch tip in either arm.

Candidate-on/off isolates the keyed optimizer; baseline/candidate-on detects
whole-patch plain-relation regressions. Plain/TxTime compares matched schemas in
the candidate. Baseline has no TxTime search arm. Persisted FTS statistics are
disabled in every primary cell; opt-in measurements would be a separate protocol.

Before any qualifying operation, save an immutable `manifest.json` containing:

- This protocol's commit and SHA-256, specification hashes, review artifact,
  candidate/baseline source trees and Cargo.lock hashes.
- Harness commit; identical shared measurement-patch SHA-256 for both builds;
  any candidate-only instrumentation patch with a source diff explaining why it
  cannot affect baseline operations; executable SHA-256 for each arm.
- Generator revision, Python/NumPy versions, complete generator arguments,
  input file SHA-256s, operation schedules, exact schemas/scripts/parameters,
  expected row counts and oracle digests. A seed alone is not input provenance.
- Rust/compiler/linker versions, target/profile/features, native archive/header
  identity, OS/CPU/RAM, storage path/filesystem/free space, backend settings,
  thread counts, environment overrides, durability and cache settings.
- Run ID, UTC start, output directory, cell order, sample sizes and budgets below.

Missing fields, an unreviewed harness, a dirty unrecorded source tree, or a
missing timing channel block the run. Save hashes before inspecting qualifying
latency. Append deviations and failure records; never rewrite the frozen manifest.

## 2. Harness and timing contract

Use a Rust harness inside Mnestic, with measurement seams behind `test-hooks`.
The existing `benches/point_lookup.rs` times `run_script`, including compilation,
and is not sufficient for the keyed gate. Review the actual timing boundaries
before accepting the harness. All clocks are monotonic, with raw nanoseconds.

For keyed reads, time evaluation of a compiled plan through complete result
materialization, excluding parse, normalization, join planning and compilation.
Either reuse a compiled plan safely or instrument the execution interval inside
`run_script`; do not subtract an independently measured compile duration. Record
end-to-end latency as a secondary metric. Validate rows outside the primary timer.
Include residual checks, deduplication and output construction in all join arms.

For TxTime search, plain reads and single-row updates, the primary interval is
the in-process public `run_script` call through returned rows or completed commit,
including compilation. JSON/IPC serialization is excluded equally in all arms.
Use the same scripts and parameter representation except for the TxTime column
and required mutation syntax. Save execution and commit phase times separately.

Instrument `tt_commit_lock` acquisition directly: wait is first acquisition
attempt to successful acquisition, including retry/backoff; section hold is
acquisition to release. Emit one trace per attempt, including timeout/cancelled
attempts, with transaction ID and monotonic timestamps. A transaction acquiring
the lock multiple times records every interval; its wait metric is their maximum.
Do not infer lock wait from update latency. Trace DDL registration, backfill,
catch-up and publication separately. Thread-local fixed-size recording avoids
introducing a shared measurement lock; overflow invalidates the affected cell.

Use identical timing/output machinery in baseline and candidate plain operations.
On the development dataset, compare instrumentation enabled/disabled with 100
paired repetitions; report overhead and clock resolution. Do not numerically
subtract overhead from qualification results. A change in plan, output, locking,
or material measurement bias must be resolved before freeze.

The `multi_get` comparator is a benchmark-only T2 executor in the same process:
read all left rows, batch complete right keys in chunks of 256, fetch values,
apply the same residual and construct the same output. Count key encoding,
batch construction, reads and result materialization in its execution interval.
Preserve duplicate/missing-key semantics and assert identical output digests.
Record backend implementations: SQLite's batch API may loop over individual
gets. A storage-only timing is not an equivalent T2 comparator. No production
batched-join feature is added by this experiment.

## 3. Keyed-access cells

Run every cell on SQLite and RocksDB. Save `::explain` output and result digests
for baseline, candidate-off and candidate-on; compare atom occurrences, access
operators and residuals. The batch comparator is used only for K2. All IDs are
zero-padded strings. Seed controls permutations, not schema or selectivity.

| Cell | Fixed fixture and sample | Primary comparison / gate |
|---|---|---|
| K1 | `node {uid: String}` at 10,000 and 1,000,000 rows; `?[uid] := *node{uid}, uid = $u`; 1,000 seeded probes, 500 present and 500 absent, separately reported | Candidate-on execution p50 at 1M / 10k **< 2**, separately for hits and misses |
| K2 | 10,000 `edge {from: String, to: String}` rows; 1M `node {uid: String => label: String, tombstone_reason: String?}` rows; `?[f,t] := *edge{from:f,to:t}, *node{uid:t,tombstone_reason:'deleted'}`; 90% right hits, 10% misses, exactly half the hits deleted; 50 complete executions per arm | Report candidate-on / baseline, on / off and point / multi_get; no speed threshold is invented |
| K3 | 1,000 `probe {l:Int => a:Int,v:Int}` rows joining 100,000 `r {a:Int,b:Int,c:Int => v:Int}` rows; `a` has 4 values and `v` has 10 equiprobable values; `?[l,count(b)] := *probe{l,a,v}, *r{a,b,c,v}`; 50 complete executions | Candidate-on execution p50 / baseline **<= 1**; on/off and p99 also reported |
| K4 | Spec R4: `a{x=>y}`, `b{y=>z}`, `c{k=>z}`; 10k/100k/100k rows; `?[x] := *a{x,y}, *b{y,z}, *c{k,z}, k = 'pick'`; one picked row, 100 equally populated z values; 50 executions per arm | Candidate-on execution p50 / baseline **<= 1**; retain `a -> b -> c` and `b` point access |

K3 binds right-side positions 0 and 3 from many left rows while leaving key
positions 1 and 2 unbound. Assert the baseline and both candidate modes retain
`stored_mat_join` and probe-before-r order. Its aggregation produces 1,000 rows
while exercising 2.5M joined matches per execution; expected count per probe is
2,500. K4 uses default greedy order. Schema, generated row arrays,
query strings and expected results must be saved before freeze. K1 scales use
matched logical present/absent probes. K2 includes 100 repeated right targets
among the hit rows to validate output semantics, while left `from` stays unique.

Warm up with 100 distinct non-measured probes for K1 and 5 complete executions
for K2/K3/K4. Repeat each fixed schedule three times in the same warm database;
collapse repeated observations to the per-operation median before inference.
No cold-read claim is made. Keep durability/cache settings identical. Only one
database arm is open at a time; record reopen/warm-up costs outside the gate.

K1's qualifying scaling comparison uses candidate-on only. Its baseline/off
full-scan arms are diagnostic controls: exactly 10 fixed probes (five hits and
five misses), once each after two warm-up probes, at both scales and every seed.
They check plan/output behavior and expose baseline cost, but cannot support a
statistical speedup claim. This bounded control avoids spending the host budget
repeatedly scanning 1M rows for a comparison the specification does not require.
K2-K4 retain their full stated baseline/off/on schedules.

## 4. TxTime inputs, indexes and quality checks

Generate the entire reference `small` workload for each seed before extracting
engine fixtures: 10k entities, 50k edges, 40k chunks, 384-dimensional F32 vectors,
1,000 queries, 64 clusters, graph_hops=2, k=10, incremental=200, pool_n=100,
rrf_k=60, topic_size=200, chunk_words=10, query_words=4, summary_words=6,
real_embeddings=false. Generating chunks alone changes RNG consumption and is
not this reference dataset. No embedding service, model or judge calls are used.

Project chunk IDs, text and vectors into `chunk {cid:String => text:String,
emb:<F32;384>, kind:String}`; the paired TxTime schema adds only its required
transaction-time key. Load identical ascending-ID rows in batches of 2,000 before
creating indexes. Use fresh directories for every arm/cell; no history, query
caches or mutated state carry across cells. Retain manifests/hashes before
deleting regenerable database directories to conserve disk.

FTS: extractor `text`, tokenizer `Simple`, filters `[Lowercase]`, default BM25.
Query terms are deduplicated and joined with ` OR `, as in the reference adapter.
HNSW: F32, Cosine, m=16, ef_construction=64, query ef=100, k=100. Both search
queries return and materialize 100 candidate rows; recall@10 is a secondary
quality check. Freeze scripts matching the reference adapter. Each ordinary
search/update fixture carries **both** FTS and HNSW indexes.

The 100 search warm-up inputs are the first 100 incremental queries already
generated by the reference workload, evaluated before inserting incremental
chunks. They are distinct from the 1,000 measured query IDs and consume no new
RNG draws. Apply this rule to both P1 and T1. Point-read schedules use 500 present
and 500 absent IDs, balanced as in K1, with 100 other IDs for warm-up.

HNSW level assignment is not deterministically seeded by the workload seed.
Record this limitation and build a fresh graph for each arm and seed; do not
reuse the best graph. Retain graph/storage hashes where possible. Compute exact
cosine neighbors from the immutable input arrays outside timed operations.
Report recall@10 for plain and TxTime, tie-aware, with paired confidence bounds.
FTS result IDs and scores must match the paired plain fixture (allow only the
frozen numerical comparison tolerance of absolute 1e-6 plus relative 1e-6).
HNSW must return valid current IDs and correct distances within that tolerance;
the lower confidence bound of TxTime minus plain recall@10 must be >= -0.01.
This additional quality guard prevents an easier search from masquerading as
speed acceptance; it does not replace any specification gate. If inconclusive,
performance acceptance remains inconclusive.

For updates, precompute 2,000 operations on distinct existing chunk IDs. Replace
both text and vector using the 200 incremental chunks in a fixed cyclic mapping;
the target CID remains unchanged. Every operation changes both fields. Save the
schedule and expected final current rows. Retain the resulting TxTime history.
Use a separate database for updates, not the search fixture. Warm up 200 updates
on disjoint IDs; both arms receive the same changes. Check final current rows,
FTS terms and vector distances after timing, including removal of stale content.

## 5. TxTime cells and unchanged release thresholds

Run P1, T1, T2 and H1 on SQLite and RocksDB; C1 and C2 are RocksDB-only.
Use the three dataset seeds in every cell. Each seed is a fresh database/build.

| Cell | Sample and comparison | Gate |
|---|---|---|
| P1 plain regression | Baseline vs candidate plain; 1,000 point reads, 1,000 FTS searches, 1,000 vector searches and 2,000 updates; both indexes present, plus a separate no-index point-read/update control | Each read p50 and update p50/p99 candidate / baseline **<= 1** |
| T1 current search | Candidate TxTime / candidate plain; each of the 1,000 queries once per FTS/vector cell after 100 non-measured queries | Search p50 ratio **<= 1.5**, separately for FTS/vector/backend |
| T2 single writer | Candidate TxTime / candidate plain; 2,000 single-row updates from the fixed schedule | Update p50 and p99 ratios each **<= 1.3** |
| C1 four writers | Four simultaneous single-row writers, 1,000 operations each, disjoint existing IDs, both indexes present | p99 of direct per-transaction section waits **<= 3 times** paired T2 TxTime update p99 |
| C2 build with writers | Separate FTS-create and HNSW-create cells on 40k-row TxTime relation, with four writers to that same relation; index being created initially absent, the other index present | Maximum observed commit-section wait during each build **<= 3 times** paired T2 TxTime update p99 |
| H1 historical fallback | Five versions per logical key, 40k keys; 100 secondary-value predicates at the third version's snapshot; forced existing base-scan path, matched exact oracle | Latency p50/p99, physical/logical row counts, errors and rows **report-only** |

C1 threads start behind a barrier; each issues the next operation only after its
previous commit. The arrival model is closed-loop, without a network queue.
Report per-writer counts, throughput, total operation latency and lock hold/wait.
No thread's result may be omitted. Each operation changes text and vector using
the same payload distribution as T2. Four disjoint schedules prevent lost-update
semantics from changing the workload. Hot-key contention is not claimed.

C2 starts writers before registration and keeps them active until publication
and one final acknowledged commit per writer. Each writer attempts one new
operation every 10 ms, never with more than one outstanding call; missed slots
are recorded rather than queued or dropped silently. Targets cycle over 4,000
disjoint IDs per writer, payloads alternate so every update changes content.
Trace registration/publication boundaries and require at least one overlapping
writer acquisition attempt at each boundary and during backfill. If scheduling
misses a phase, the cell is inconclusive, not a zero-wait pass. Do not add sleeps
inside the engine or stretch publication to manufacture overlap. Report maximum
over all observed waits, also separately by phase and writer, including failures.
Catch-up-log overflow, build timeout and publication failure are failures, not
valid low-latency samples. A build is capped at 15 minutes and 400,000 total
writer attempts; reaching either cap stops the cell as incomplete.

For H1, use a common committed snapshot after all third-version writes and before
any fourth-version write. No `:as_of` search atom or future B-tree feature is used.
The current batch has no historical base-scan row cap: that feature is deferred
to batch 3. Record the 200,000 physical versions, 40,000 logical keys and query
timeout outcomes; the 30-second operation deadline still applies. Do not add a
scan-cap implementation or fabricate `query::as_of_scan_limit` measurements here.

## 6. Order, uncertainty and decision rules

Freeze cell order K1-K4, P1, T1, T2, C1, C2-FTS, C2-HNSW, H1; SQLite then
RocksDB where applicable. Rotate arm order by seed: baseline/off/on/batch at
1234, on/batch/baseline/off at 1235, batch/off/on/baseline at 1236, omitting
inapplicable arms. Paired plain/TxTime uses plain-first, TxTime-first, plain-first.
Run engines sequentially. Compilation, loading, index construction, or another
benchmark must not overlap a timed cell. C1/C2 writer concurrency is intentional.

Report every seed separately and an equal-seed aggregate. Quantiles use the
nearest-rank definition; never average per-request latency ratios. For each arm,
the aggregate p50/p99 is the arithmetic mean of the three per-seed p50/p99
values, explicitly labelled a mean of seed quantiles, not a pooled percentile.
The aggregate ratio divides these arm aggregates; each seed's ratio divides its
own arm quantiles. Use the same estimator inside the bootstrap. For inferential
gates, use 10,000 paired
hierarchical bootstrap replicates with analysis seed 20261114: resample dataset
seeds, then paired operation IDs within seed. Preserve repeated-query groups;
for sequential updates resample contiguous blocks of 100 operations within seed.
For C1, order the shared lock's acquisitions globally and resample contiguous
blocks of 100 acquisitions containing all writers' events; do not resample each
writer independently because their waits are coupled by the same lock. Retain
writer identity inside each block. C1 uses the maximum per-writer p99 as an
additional conservative gate. Independently resample the T2 denominator's
operation blocks within the same resampled dataset seed.

Pass a ratio gate only when the one-sided 95% upper confidence bound is at or
below its threshold (strictly below for K1) **and** every observed seed meets the
threshold. Fail when the observed estimate exceeds the threshold; otherwise a
bound crossing the threshold is inconclusive. Three independent builds give
limited power; 1,000/2,000 observations do not mean thousands of independent
builds. For C2's maximum gate, each observed maximum must pass; report counts and
the measured duration, with no claim about an unobserved population maximum.

“Unchanged” and “no slower” retain threshold **1.0**, with no implicit 5% margin.
Exact equality is difficult to establish under noise; an inconclusive finding
keeps the PR draft. Changing a threshold needs an explicit specification change
and a new experiment phase, not a retrospective adjustment.

All required cells must pass; no best-backend or best-seed selection. This is an
intersection of preregistered gates, not a family of optional wins. Report all
intervals; no claim of a speedup is made from an isolated favorable interval.
K2 and H1 remain report-only. All observed failures/timeouts stay in denominators;
do not compute a passing latency quantile using successful operations alone.
Any unexpected error, row mismatch, missing trace or exceeded resource cap makes
the cell failed/incomplete and prevents overall qualification.

## 7. Resource budget and terminal rules

Local-only, no paid API use. Host: 8 GiB arm64 Mac, Rust 1.98.0. Build sequentially
with `-j 2`, `CARGO_INCREMENTAL=0`, `CARGO_PROFILE_RELEASE_DEBUG=0`, release profile;
record all remaining profile settings. Use two Rayon threads, one BLAS/OpenMP
thread, and the same engine/background-thread settings in both arms. Reuse the
compatible pinned RocksDB archive if its provenance is verified. No unsafe
durability shortcuts, undocumented cache changes or implementation tuning.

Budget: at most 2 hours total compilation and 6 hours total qualification wall
time, including generation/loading/building/warm-up/analysis; 30 seconds per
ordinary operation, 15 minutes per index build, 4 GiB peak benchmark process-tree
RSS, and at least 2 GiB free disk throughout. Capture process RSS, free disk,
swap counters and competing CPU activity every second. Stop gracefully on a cap;
keep raw partial results. Do not delete unrelated targets or stop other users'
processes. If the host cannot support the 1M-row/40k-vector cells, report that
limitation without substituting a smaller qualifying workload.

Before each cell require AC power, low-power mode disabled, no competing native
build/engine benchmark, and a 30-second idle observation. Start with a five-minute
maximum wait for those conditions. Label a cell contaminated if another engine
benchmark/native build appears, swap-out increases, or another process consumes
more than one full CPU core for five consecutive samples. Retain the original
cell; at most **one** fresh repeat of the entire contaminated cell is allowed,
within the same global budget. Do not retry merely because a ratio failed or was
inconclusive. Two contaminated attempts or any resource cap end qualification.

After a correctness/performance failure, stop qualifying runs and diagnose on
the development fixture. A source/harness/protocol change creates a new phase;
repeat affected gates and preserve the failed phase. Never pool phases. Do not
expand sample sizes, select new seeds or relax gates after inspecting results.

## 8. Artifacts and completion

Use `artifacts/keyed-txtime/<run-id>/` outside tracked source for large raw data;
record its absolute path in the manifest. Preserve `manifest.json`, input hash
list, exact schemas/scripts, plans, builds/logs, result/oracle digests, raw timing
JSONL, resource CSV, errors, exclusion ledger, analysis command/version and
machine-readable gate decisions. Archive/hash raw artifacts before cleaning
database fixtures. Commit a concise result report and artifact manifest to
`docs/reviews/`; do not commit gigabytes of regenerated databases.

Each timing row includes run/cell/seed/backend/arm/op/writer identity, input ID,
warm-up flag, start/end, execution/end-to-end/wait/hold nanoseconds where defined,
success/error/timeout, row count and output digest. Use null for inapplicable
channels, never zero for an unmeasured channel. Capture outliers without clipping.

Required next deliverables, in order:

1. Independent read-only review of this protocol; resolve blocking findings.
2. Implement and smoke-test the harness/trace/comparator/oracles on seed 99;
   independently review measurement boundaries and freeze the complete manifest.
3. Execute fixed cells within budget, preserving unsuccessful outcomes.
4. Independent experiment review of raw artifacts and proposed conclusions.
5. Requalify after any newer-main integration, update PR #81 with measured limits,
   and remove draft status only if all required gates and final CI pass.

This document supplies the experimental design. It does not claim that the
harness exists, that measurements have run, or that PR #81 is ready to merge.
