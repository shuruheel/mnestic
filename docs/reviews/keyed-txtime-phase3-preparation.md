# Phase-3 recording controls: preparation

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

No phase-3 controls or full performance qualification have run at this stage.
The user selected this Mac after resources recover. An initial Cargo check was
stopped when another native build started and free disk fell below 200 MiB;
the resumed check and subsequent tests passed. No unrelated process was stopped
and no previous evidence was removed. Final candidate/baseline builds, runtime
smokes, reviewed build manifest and host preflight remain launch prerequisites.

Preparation logs are retained at
`/Users/shuruheel/Code/mindgraph/benchmark-artifacts/keyed-txtime-phase3-20261114-v1`.
