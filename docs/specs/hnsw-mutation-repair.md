# HNSW mutation repair for 0.19

Status: implemented locally, explicitly selected for 0.19 by the owner; release
qualification and timing gates remain pending.
This supersedes the earlier investigation-only scope for the demonstrated mutation
defect. Filtered-search redesign and E1–E3 timings remain separate.
The [validation record](../reviews/2026-11-05-hnsw-mutation-repair.md) pins source
`e606c661`, tests, independent review and the packaged-crate smoke.

## Invariant and algorithm

Deleting vertex x must preserve every pre-existing directed path between surviving
vertices that passed through x. This is a maintenance invariant, not a guarantee
of exact ANN recall or repair of previously disconnected graphs.

At each layer, collect the incident vertices before removing x. Read both active
edge directions: ignored reverse records are bookkeeping, not traversable edges.
For every active incoming u->x and outgoing x->v, consider u->v as a bypass.
Keep existing edges untouched. Prune an added bypass only if the affected local
graph or a bounded external search supplies an alternate directed path from its
source to its target. External searches exclude the removed vertex and use the
current repair plan for edges between incident vertices, including uncommitted
bypasses and excluding previously pruned bypasses. Stored edges outside that
plan remain available as witnesses. Each removal therefore preserves paths in
the graph that will actually be written, rather than in a stale adjacency cache.
Check remaining outgoing degrees against the manifest's layer-specific cap.
If the cap cannot be met, fail with `hnsw::repair_limit`; do not commit a degraded
graph. All bypass distances use the existing metric and current vector identities.

Write new active edges with reverse bookkeeping rows, then remove x's edges and
self row. Count only active outgoing edges in degrees. Identify self edges by
the complete (row key, field, subindex), so list/vector-column siblings remain
distinct. Vector replacement uses this same removal path before reinsertion.

Incremental insertion/replacement pruning also preserves paths: it only removes
an outgoing edge when a checked alternate route survives. It
uses the existing selection heuristic as a preference, retaining necessary edges.
All selected bidirectional insertion links are installed before pruning, so the
proof sees the complete new layer. Pruning first checks the immediate neighborhood.
If that induced subgraph is insufficient, a bounded breadth-first search may prove
an existing route through external vertices. It starts from currently retained
outgoing neighbours and never visits the pruning target. Cached witness adjacency
is therefore independent of earlier outgoing-edge removals within the same pass.
The fallback only reads existing edges; it adds no connections and never traverses
ignored reverse records. Missing proof or exhausted work still rejects the mutation.
Shrinking selects from existing outgoing neighbors, matching the bulk builder's
no-extension policy. An infeasible bounded degree constraint fails explicitly; this
can also reject a fresh incremental insertion rather than disconnect a leaf.

Repair is bounded by 256 incident/pruning vertices, 4,096 physical rows per
adjacency scan, 1,048,576 incident-vector components per layer, 16,777,216 distance components
per layer repair and 1,048,576 adjacency checks per pruning pass. Exceeding
a limit returns the same structured diagnostic with a reason. These are local
repair work bounds, not a statement RSS limit or a hard wall-clock guarantee.
They must never silently truncate the neighborhood or skip a required connection.
The external-path fallback additionally caps cached/visited vertices at 1,024,
physical adjacency rows read at 16,384, and traversed adjacency entries at
1,048,576. Row and traversal budgets are shared across all witness attempts in
one insertion-pruning or removal-repair call; each individual scan retains the
4,096-row cap. These bounds
are additional to the immediate-neighborhood proof, not whole-transaction limits.
The component check is per layer, not a cache-wide ledger: vectors retained across
layers and the reconciliation snapshot cache are outside that cap. This repair
does not complete the separate end-to-end search memory-accounting contract.

## Transactions and recovery

Repair writes participate in the same storage transaction as the row mutation.
Failed ordinary scripts and governed transactions roll back both. An incomplete
HNSW mutation also prevents commit through the shared commit funnel, including
legacy explicit multi-transactions. Catching the query error and running another
successful statement cannot clear the guard: abort and open a new transaction.

Online-build reconciliation removes all changed/deleted snapshot nodes before
inserting current rows. Its removal cache retains the old vectors for those
nodes, including list elements no longer present in the current base rows.

Rebuild pre-0.19 HNSW indexes from base rows with the
existing `::reindex relation` operation. Rebuild is explicit, not an automatic
fallback on the write path. Back up the database and schedule an appropriate
maintenance window; validate search after rebuild. Existing key/edge encoding
does not change. This recovers already-disconnected components and establishes
the reverse-edge bookkeeping invariant for legacy `extend_candidates:true`
indexes: old incremental shrinking could create edges without reverse records.
The local repair cannot discover those hidden incoming edges without scanning the
whole index. Increasing ef cannot reconnect a disconnected component.

Example, using the relation from the investigation:

```cozoscript
::reindex docs
```

This rebuilds the relation's search indexes using their existing manifests.
Verify expected keys against exact base-row/vector queries afterward. A rebuild
does not guarantee that a future mutation fits a restrictive degree/work cap.
If a repair-limit error recurs, abort and investigate the index configuration or
use exact eligible-key retrieval; repeated retries with the same inputs are not
a recovery strategy. No production database is rebuilt by the release itself.

## Acceptance

- Deterministic level-zero bridge fixture fails before repair and passes after it.
- Delete, duplicate/distinct vector replacement, list/field identities, ignored
  reverse edges, degree accounting, and upper-layer/entry deletion are covered.
- Repeated mutations, read-your-writes, script abort, explicit abort, failed commit
  and persistent reopen pass on SQLite and RocksDB.
- Pruning checks demonstrate an alternate path survives; infeasible degree/work
  cases fail without committed base or graph changes.
- An existing alternate path outside the immediate neighbourhood is recognized;
  insertion pruning and removal repair both cover this case. Ignored edges are
  excluded and witness-budget exhaustion remains explicit.
- Review insertion pruning interaction and partial legacy-transaction errors.
- Run HNSW/reindex/hybrid and transaction regressions and strict Clippy. Retain
  immutable fix SHA and independent storage-contract review before delivery.
- Qualify the new release candidate separately. Do not relabel artifacts from
  engine `0148693b` as repaired-candidate evidence. Long-run performance/quality
  measurements remain pending the owner's quiet-window signal.
