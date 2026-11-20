# Measurement amendment after admission diagnostics

November 14, 2026. Proposed revision 2; not authorization to run qualifying
seeds. The revision-1 protocol and its results remain preserved in Git and the
raw artifact roots. No specification latency ratio is relaxed.

## New source comparison

Use main `272c7b7196de9acd8d4aa66ec3e6e2f10f0057ea` (0.19.3) as the
baseline. Candidate identity must be frozen after integration, admission fixes,
correctness checks and review. The old `595fdaa6` comparison cannot qualify the
merged implementation. Export shared execution/commit instrumentation onto the
new baseline without copying the candidate's admission policy. Record the
candidate-only admission events separately. Both arms must use identical native
archive/header identities, compiler flags, durability and backend configuration.

## C2: retain the four same-relation writers and expose admission delay

Keep revision 1's same-relation C2-FTS and C2-HNSW fixtures, four writers, 10 ms
arrival cadence, final acknowledgments, three seeds, operation/build/resource
caps and 3-times-T2-update-p99 maximum-wait threshold. Record the public-call
interval, complete admission interval (transaction gate plus relation admission),
and TT commit wait for each writer operation. The existing commit-wait maximum
must pass. Add a conservative gate: maximum **union duration of admission and TT commit-wait intervals per operation**
must also be <= 3 times the same T2 TxTime end-to-end update p99. Nested spans
must not be summed twice. All errors and incomplete attempts remain failures.

Add separately labelled companion FTS/HNSW cells with four writers to an
independent TxTime relation, leaving the build target unmodified. The independent
relation uses the same 40k-row fixture and both indexes, and T2's payload distribution and denominator. Use C2's four disjoint target
sets, repeating alternating payloads, 10 ms pacing, missed-slot recording and
continued arrivals through publication plus one final acknowledgment per writer.
Do not substitute T2's finite single-writer schedule. Apply the same maximum-wait gates. These
cells exercise the shared TT mutex after maintenance has acquired the target's
relation guards and released global admission. They do not replace the original
same-relation cells or establish catch-up cost on their own.

Revise the coverage criterion explicitly: every registration, backfill,
catch-up and publication phase must have a complete valid trace and at least one
in-flight writer public-call interval overlapping it. Record positive TT-wait
intersection with the confirmed-held section separately; do not manufacture or
assume it. Report zero actual TT overlap as such. Same-relation admission can
legitimately prevent writers from reaching the TT mutex during an exclusive
phase; demanding positive TT contention in every such phase contradicts that
ordering. The added admission gate prevents moving delay outside the TT metric
from producing a passing result. If no writer is in flight during a phase, the
cell remains inconclusive. No engine sleeps or stretched phases are permitted.

This replaces the revision-1 phase-contention requirement; it is a disclosed
measurement-definition change, not a retrospective pass for earlier traces.
Require independent design review and a new immutable campaign before using it.

## Recording overhead: matched state and operations

Retain read-only controls and add separate single-writer and four-writer
mutation controls for SQLite/RocksDB. Freeze 100 development pairs per control,
seed 99. Each pair begins from two copies of the same cleanly closed, hashed
database snapshot; initial setup/index construction is outside timing. Warm both
copies with the same read-only operations, then perform identical measured
scripts and parameters. Mutations change text on disjoint keys while preserving
vectors, avoiding random HNSW topology changes between the arms. Verify current
rows, index results and logical history cardinality after each arm. The next
pair resets both arms from the snapshot, so no evolving state crosses pairs.

Use persistent worker threads, barrier-synchronized complete rounds and
alternating on/off order across pairs. Exclude worker creation, snapshot copy,
open, warm-up, output encoding and verification from measured intervals. Save
physical snapshot hashes, all input/output hashes, complete on/off records,
errors and reset costs. Do not select a successful subset or match writers by
finish order.

Primary contention statistic: four-operation makespan, maximum end minus
minimum start. Report on/off p50 and p99 ratios with 10,000 paired bootstrap
draws (seed 20261114), retaining all four writers per pair; also show per-writer
observations and completion-order changes. Fresh snapshots remove database evolution but not serial host/cache/thermal
dependence. The primary interval resamples contiguous blocks of ten paired
rounds, retaining all four writers and on/off pairing. Also report an explicitly
conditional IID-pair sensitivity interval; use the more conservative upper bound
for the freeze decision.
Read-only/single-writer controls use their operation-level public-call duration.
No overhead is subtracted from any qualifying measurement. These development
controls quantify uncertainty; they cannot prove exact zero instrumentation bias.
Before inspecting results, define recording cost as adequately bounded only if
both p50 and p99 on/off ratios have a one-sided 95% upper bound <= 1.05 in every
control, using the larger block/IID upper bound. Otherwise freeze is blocked,
whether the excess reflects bias or unresolved uncertainty. This 5% recording
control rule does not relax any specification ratio, including no-regression
threshold 1.0, and does not authorize subtracting overhead. The comparison
measures recording on versus disabled hooks, not total cost versus a production
binary without instrumentation.

Use the existing seed-99 120-row, 64-dimensional normalized F32 development
fixture: Simple/Lowercase FTS, HNSW Cosine m=16/ef_construction=64/ef=100, persisted
FTS statistics disabled. Freeze exact snapshot and schedule hashes. Controls
cover baseline plain, candidate plain and candidate TxTime on both backends;
each has read-only, single-writer and four-writer cases. Use 100 identical
read-only warm-up point probes per arm; measure one probe or one update per
worker per round. Measured update keys are disjoint across workers, and only text
changes. No 40k-row or qualifying-seed inputs enter these controls.

Budget: 30 minutes total control execution, 3 minutes per control cell, 30 seconds
per operation, 4 GiB RSS and 2 GiB free disk. No concurrent builds or paid services.
One attempt per cell; any failure, missing pair, limit breach or oracle mismatch
blocks freeze. Save reset/open/warm-up costs outside timing and all raw failures.
Additional samples or design changes require a new labelled phase; no successful
subset or post-result tolerance change is permitted.

## Unchanged remaining gates

K1-K4, P1, T1, T2, C1, H1, result/recall checks, seed schedules, confidence rules,
ratio thresholds (including 1.0 for no regression), terminal rules and the six-hour
qualification budget remain as specified in revision 1. Add the two companion
C2 cells after their same-relation counterparts. The harness must implement and
validate this revision before qualifying execution is enabled.
