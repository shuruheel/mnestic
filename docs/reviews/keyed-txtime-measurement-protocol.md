# Keyed access and TxTime measurement protocol review

Date: November 14, 2026. Outcome: **READY as an experimental design**.
Qualifying execution and performance acceptance are not yet established.

An independent, read-only `experiment-reviewer` reviewed the
[protocol](../plans/keyed-txtime-measurement-protocol.md) against both feature
specifications, relevant runtime source and the reference workload generator.
The reviewer did not edit files, run builds or execute experiments.

The approved design's SHA-256 was
`7c92c4faf9298bd4f8e6e127381668c002a5dc07cd8f13de4669738fc8f563bb`.
The subsequent protocol edit only updates its status and links this record;
measurement design and gates are unchanged. The future run manifest must hash
the actual committed protocol, rather than reuse this pre-status-edit hash.

## Findings and resolution

| Finding | Resolution verified in the closure review |
|---|---|
| A single-atom K3 fixture did not expose repeated-left partial-key scan amplification | K3 now joins 1,000 left rows against a three-column primary key with only positions 0 and 3 bound; it asserts `stored_mat_join`, join order and 2,500 matches per probe |
| Independently resampling writers could understate uncertainty at their shared lock | C1 resamples global acquisition blocks retaining all writer events; T2 blocks are independently resampled within the matched dataset seed |
| Equal-seed aggregate was ambiguous | The estimator is the ratio of arithmetic means of per-seed quantiles, also used inside bootstrap replicates |
| Warm-up definitions exceeded the K3 input space and did not identify search inputs | K3 uses five full-query warm-ups; search uses incremental-query IDs 0–99 before insertion, separate from the measured query bank |
| Historical base-scan cap setting does not exist in this batch; identified by author and source-confirmed by reviewer | H1 reports the current uncapped scan under the operation timeout; future batch-3 cap behavior is not claimed |
| K1 baseline/off full scans could consume the host budget without satisfying an additional specification gate | Ten fixed diagnostic probes per control arm; the full candidate-on scaling schedule and threshold remain intact; no comparative speedup inference from those controls |

The closure review reported no remaining design blocker. It confirmed the
candidate/baseline identities, unchanged thresholds, execution-only keyed timing,
direct lock-wait measurement, fresh state, provenance requirements, retained
errors and terminal resource rules.

## Outstanding execution evidence

- Implement and review the timing harness, batch comparator and result oracles;
  measure instrumentation overhead on development inputs.
- Freeze complete source/patch/executable/input hashes and exact operation
  schedules before qualifying measurements.
- Demonstrate natural writer overlap with the short registration/publication
  phases in C2; missing overlap is inconclusive.
- Treat six hours as a hard budget, not an established runtime estimate. Retain
  partial outcomes if the host cannot complete the fixed workloads.
- Obtain an independent result review before accepting comparative claims, and
  requalify after newer-main integration before changing PR readiness.

No benchmark was run as part of this design review. Existing correctness and
hosted-CI results remain documented separately in the
[implementation qualification](keyed-txtime-implementation.md).
