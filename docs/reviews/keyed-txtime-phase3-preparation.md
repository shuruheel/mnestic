# Phase-3 recording controls: validation and host admission

Status, November 14: the runner, control orchestration, final binaries and runtime
smokes passed review and validation. The amended campaign stopped at host
preflight with **zero measured blocks** because disk and CPU conditions failed.
Performance qualification remains blocked; PR #81 remains draft.

The [new control phase](../plans/keyed-txtime-controls-phase3.md) preserves the
1.05 recording p50/p99 upper-bound threshold and every specification performance
gate. It replaces the failed phase-2 100-pair design prospectively with ten
fresh 100-pair blocks per control, paired hierarchical and IID sensitivity
bounds, and a stricter quiet-host criterion. Earlier results remain failed.

Independent experiment review accepted the design and orchestration after two
corrections: qualification now inherits the stricter host conditions, and the
campaign verifies frozen build manifests and executables through final
acceptance. A review manifest binds final source/build identities and the shared
baseline runner export. Different Cargo target paths are permitted; compiler,
profile and native-library settings must match.

The shared runner now supports hash-checked external parameters and row oracles,
disjoint inline parameters, setup-result bindings for historical snapshots, and
qualification authorization bound to passing phase-3 controls and the exact
executable. An explicit cardinality projection counts materialized history rows
without JSON-expanding all stored vectors; primary query timing is unchanged.
Qualification does not become accepted merely because a runner invocation
succeeds. The controller and statistical/quality gates remain authoritative.

Independent source review found and closed two lifecycle problems: fallible
input preparation now crosses both worker barriers before returning an error;
a failed output capture retains its raw trace and rows. All workers/DDL receive
setup bindings. Ordinary engine deadline arguments remain at most 30 seconds;
recognized setup index builds at most 900 seconds. Outer supervision remains
necessary for hard wall-time limits, including the existing plain-HNSW path.

Local validation: five Rust runner regressions pass, strict Clippy passes for
tests and the runner, and 24 Python evidence/orchestration regressions pass
(12 timing, 5 original controls, 7 phase-3 checks). CI now runs these runner and
Python checks. Production engine code is unchanged from the admission candidate.

No phase-3 measurements or full performance qualification have run at this stage.
The user selected this Mac after resources recover. An initial Cargo check was
stopped when another native build started and free disk fell below 200 MiB;
the resumed check and subsequent tests passed. No unrelated process was stopped
and no previous evidence was removed. The initial final binaries passed 22 ordinary runtime smokes (352 operations),
two rejection checks and two supported 100-pair/100-warmup snapshot smokes
(49,208 operations). Independent review accepted their provenance and oracles.
A shortened initial smoke fixture was correctly rejected by the fixed-size
control guard; its failure is retained separately from the corrected smokes.

Preparation logs are retained at
`/Users/shuruheel/Code/mindgraph/benchmark-artifacts/keyed-txtime-phase3-20261114-v1`.


The first control attempt was interrupted during its first host preflight after
hosted Rust 1.100 Clippy rejected an existing tail error macro. No benchmark
process or block started. Commit `be394ba4` uses an explicit equivalent error
return; independent source review and hosted strict Clippy pass. The retained
93 host samples span 100.087 seconds, with 89 CPU rejections, four swap-out
changes, 7,604 additional swapouts and no quiet window. This span is not total
controller elapsed; the separate operator record and clarification preserve
that distinction.

The [prospective amendment](../plans/keyed-txtime-controls-phase3.md#prospective-compiler-compatibility-amendment)
permits one source-driven replacement with fresh build/export/smoke evidence.
It reserves 120 seconds for the interrupted attempt, leaving 7,080 seconds of
the original two-hour budget. All sampling, thresholds and stop rules remain
unchanged; earlier observations are excluded from acceptance. Seven orchestration
tests pass after this accounting change. Updated artifacts use
`/Users/shuruheel/Code/mindgraph/benchmark-artifacts/keyed-txtime-phase3-20261114-v2`.

[Machine-readable evidence](keyed-txtime-phase3-validation.json) records artifact
hashes. The full qualification generator, controller and inferential/quality
analysis remain outstanding, followed by qualifying execution and independent
results review, conditional on passing recording controls. The PR remains draft.


Both replacement release builds now pass with matched Rust 1.98/native settings,
unchanged source-before/after snapshots and exact executable hashes. The accepted
baseline export differs from the initial export only at the compatibility error
return. Repeated final-binary smokes pass all 24 ordinary checks and both exact
100-pair/100-warmup snapshot controls; independent artifact review accepted the
new provenance and raw correctness evidence. The candidate executable is
`cb16af86ba89e114f1cf92d4628fef394a15f4239426a6294bc48ba45869603c`;
the baseline executable is
`7dddcd960c46464004a80fdec8be6d85bee8893d3782d0b199ce563f528552ec`.
Only this task's regenerable build caches and temporary
`cozorocks/rocksdb/AUTHORS` symlinks were removed after copying and rehashing both
final executables.


The amended campaign is **incomplete: host preflight failed, zero of 180 blocks
executed**. Its five-minute preflight retained 278 observations. Every sample
had insufficient free disk (1,518–1,665 MiB against the 2,048 MiB minimum) and
at least one other process above 25% CPU; 11 samples also contained a native
process. Power conditions passed, swapouts did not increase during this attempt,
and no quiet window was observed. The controller stopped automatically without
launching a benchmark or generating qualifying inputs. Elapsed time was 301.341
seconds, or 421.341 seconds including the reserved prior-attempt budget. Frozen
scripts, protocol and build identities remained unchanged.

This is an execution-admission failure, not a recording-overhead measurement or
an engine performance failure. There is no estimate to compare with the unchanged
1.05 ceiling. No repeat follows this failed admission under the reviewed phase-3
rules. A future run requires recovered host resources and a prospectively reviewed
new campaign; full qualification remains gated on passing recording controls.

Independent admission-results review verified all frozen identities, host counts,
budget accounting and zero-run evidence, and confirmed that the stop rule applies.
