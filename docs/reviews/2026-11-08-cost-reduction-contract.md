# 0.19 cost-reduction slice — November 8, 2026

The owner authorized implementation after the source audit of candidate3a8815a2.
This slice preserves the engine's correctness repairs and reduces avoidable work;
it does not claim a measured speedup or qualify generic filtered-ANN widening.

## Engine invariants

- Local HNSW reachability proofs reuse visited/stack buffers only within one
  pruning operation. Every proof clears state, including after early success or
  budget exhaustion. Traversal order, physical reads, edge choices, degree caps
  and shared work charges remain unchanged. No cache crosses graph mutations.
- FTS transactions still capture committed generation before their storage
  snapshot. An atomic marker is set before posting-mutation bookkeeping. A
  transaction that never started a mutation can skip completion/cleanup locks;
  a potentially partial writer must retain all uncertainty and publication fences.
- Warm serialized FTS statistics remain cached. No late generation capture,
  speculative shared totals, or weakened commit protection is allowed.

## Coordinated consumer behavior

The separate MindGraph type/layer semantic path will enumerate live embedded
eligible nodes through cap+1, using the existing20000-node materialization cap.
Selection and scoring share one governed read transaction, deadline and memory
budget. At/below cap, score the complete set exactly; above cap use existing ANN
behavior without claiming exactness over a truncated set. Preserve include/exclude
filters, tombstones, layer filtering and rounded-distance/UID ordering. Close the
read transaction on success and failure. Bind vectors as parameters. This is a
consumer query strategy using existing engine operations, not an engine auto mode.

Tests cover distant rare-type neighbors, ties, fewer-than-k/empty sets, dead and
unembedded rows, overlapping include/exclude filters, layer mismatch, cap equality,
overflow and the existing single-ANN-query shape. Engine tests cover reuse after
success/failure/exhaustion, shared work charges, old snapshot generations and
partial-writer commit rejection, alongside existing repair/FTS/governed suites.

A separate contract review precedes delivery. The earlier measured artifacts remain
attached to3a8815a2; changes require candidate CI/package qualification. Runtime
validation must not be described as a new timing comparison.
