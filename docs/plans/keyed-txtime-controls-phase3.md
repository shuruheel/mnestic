# Recording controls, phase 3

Proposed November 14, 2026, following the user's explicit request to proceed
with a new control phase and full qualification. Phase 2 remains failed. This
phase changes sampling and host admission prospectively; it cannot rescue or
pool the previous observations. Specification performance thresholds and the
recording p50/p99 upper-bound ceiling of 1.05 are unchanged.

## Questions and design

Can recording cost be bounded under controlled host conditions with more than
one effective observation in the upper one percent? Can all mutation controls
pass exact current-state, search and history oracles?

Retain the 18 controls and their seed-99 fixtures, identical on/off operations,
100 read-only warm-ups per arm, clean snapshot resets, persistent workers,
alternating arm order, complete-round makespan and exact F32-domain oracle from
phase 2's correction. No batching/averaging of timed operations, changed engine
settings, numerical tolerances, time subtraction or discarded outliers.

Each control consists of **ten predeclared blocks of 100 paired rounds** (1,000
pairs total). Each block starts a fresh process and constructs a fresh seed
snapshot; each pair still resets both arms from that block's clean snapshot.
This reuses the validated runner without changing engine instrumentation. All
block plans are byte-identical within a control, while their output directories
and block identities differ. The fresh HNSW graphs are not deterministically
seeded: treat block as the shared setup/host cluster, never select a graph.

Run block 0 across the 18 controls in the existing backend/schema/kind order,
then blocks 1 through 9 in that same order. Freeze all plans, order, source and
binary hashes before the first launch. Keep both failed and completed blocks.
Never stop early for a favorable estimate or add blocks after inspecting results.

## Analysis

Within each block retain paired on/off round identities and all four workers
for contention. Pool the 1,000 round durations with equal block weights to form
nearest-rank p50 and p99; the unit remains one public call or a four-call
makespan. Report block-specific quantiles and all raw per-worker observations.

For each quantile calculate the on/off ratio and a one-sided 95% upper bound
from 10,000 paired hierarchical bootstrap draws (seed 20261114): sample ten
whole setup blocks with replacement; inside each selected block sample ten
aligned groups [0..9], [10..19], ..., [90..99] with replacement, never
sliding groups. Draw the ten outer block IDs first, then the ten group IDs for
each selected block. Retain on/off pairing and all writers. Also calculate a conditional paired-IID sensitivity bound over all
1,000 pairs in block-major, pair-major order. Initialize Python random.Random
with 20261114 separately for each method; use the same draws for p50 and p99.
The upper bound is the nearest-rank 95th percentile (sorted draw index 9,499).
Use the larger bound. Report the same estimator in both methods;
no average of per-pair ratios and no averaging latency into pseudo-samples.

All 18 controls require both upper bounds <=1.05, complete valid evidence and
all ten blocks. Any unobserved, invalid or failed block suppresses acceptance
for its control. The old per-block 100-pair acceptance result is descriptive
only; do not require each noisy block to pass or select passing blocks. A ratio
failure or inconclusive bound ends this phase without another repeat. No claim
of zero total instrumentation cost versus a production binary is made.

## Host, budgets and stop rules

Keep candidate engine semantics at `0cedce38` and baseline at `272c7b71`.
First implement and review qualification authorization and external-input
loading in the shared runner; export identical runner/hooks to the baseline.
Build and freeze both final qualification-capable executables with source/patch,
native/compiler and binary hashes. Use those exact binaries for both controls
and qualification, without a later binary-equivalence assumption. No new qualifying dataset or engine
execution is permitted until this phase passes independent results review.

Before every block require AC power, low-power mode disabled, at least 2 GiB
free disk, no native build/other engine benchmark, and 30 consecutive one-second
observations with no swap-out increase and no other process above 25% CPU.
Allow at most five minutes for a block's preflight; a failure stops the phase.
This is a deliberately stricter host criterion than phase 2, which lacked an
idle preflight. Record all observations; do not stop another user's process or
change system services to obtain eligibility.

During a block keep the same 4 GiB process-tree RSS and 2 GiB free-disk caps.
A native build/another benchmark, swap-out increase, loss of AC power, low-power
mode enabled, or another process above 25% CPU for five consecutive samples
marks contamination and stops the phase. Preserve all data. No contaminated
retry is allowed in this phase, so no selective replacement can affect results.
Shorter load spikes remain visible and are not excluded from quantiles.

Budget: two hours including preflights, execution, compression and analysis;
three minutes per 100-pair block; 30 seconds per operation. No concurrent builds
or experiments. Failure of any runtime/oracle/resource/host gate ends the phase;
all missing blocks remain missing, not passing. Calculate acceptance only after
the fixed schedule completes. Build profiles and thread counts remain unchanged.

After a completed block, losslessly gzip its JSONL files outside timing. Verify
the decompressed SHA-256 before deleting the uncompressed copy; preserve a
manifest mapping original names, sizes and hashes to archived files. Streaming
analysis must retain all rows/failures while bounding memory. Do not delete
prior raw evidence, native archives or unrelated Cargo caches.

## Execution gate

Require independent protocol and implementation review plus fail-closed tests
before launch. Full performance qualification follows only if all controls pass;
its original and revision-2 thresholds, workload sizes, seeds, ordering, quality
checks and six-hour budget remain unchanged. Carry the phase-3 25% CPU, power,
no-swap and quiet-window criteria into qualification prospectively, so controls
and qualifying operations admit the same host conditions. Qualification retains
its original allowance of one complete repeat of a contaminated cell within
the six-hour budget; phase 3 permits no repeat. If host admission fails, record
that outcome and retain the prepared work; do not start qualification on a
smaller fixture or a contaminated host.

## Prospective compiler-compatibility amendment

The first phase-3 attempt (`keyed-txtime-phase3-20261114-v1/controls`) was
interrupted during its first host preflight, before any benchmark process or
measured block. Hosted Rust 1.100 Clippy rejected a tail `miette::bail!` macro;
commit `be394ba4` replaces it with the equivalent explicit error return. Preserve
the original freeze, preflight observations and operator termination record.

Independent review permits one source-driven replacement, identified as
`keyed-txtime-phase3-20261114-v2/controls`, with freshly built/exported binaries,
runtime smokes, reviewed hashes and a new manifest. This remains phase 3 with
all sampling, estimator, threshold, host and budget rules unchanged. No earlier
observations count toward acceptance. This exception does not authorize a retry
after host admission fails or after any measured block fails or is inconclusive.
Reserve 120 seconds of the original two-hour campaign budget for the interrupted
attempt (113.329 seconds from output-directory creation through the termination
record). The replacement has at most 7,080 seconds for its preflights, execution,
compression and analysis; record both elapsed and combined accounted time.
