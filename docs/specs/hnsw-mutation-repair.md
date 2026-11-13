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
If the cap cannot be met within the work bounds, fall back (0.19.1, below): reconnect
each in-neighbour by one edge, unlink the vertex on that layer, and record a
`hnsw.repair_fallback` warning. All bypass distances use the existing metric and
current vector identities.

Write new active edges with reverse bookkeeping rows, then remove x's edges and
self row. Count only active outgoing edges in degrees. Identify self edges by
the complete (row key, field, subindex), so list/vector-column siblings remain
distinct. Vector replacement uses this same removal path before reinsertion.

Incremental insertion/replacement pruning also preserves paths: it only removes
an outgoing edge when a checked alternate route survives. It
uses the existing selection heuristic as a preference, retaining necessary edges.
All selected bidirectional insertion links are installed before pruning, so the
proof sees the complete new layer. While shrinking an existing neighbor, first
try removing older outgoing edges using local and then bounded external proofs.
Only if those cannot meet the cap, consider removing its newly installed link to
the inserted vertex, again requiring a path proof. This preserves a new route
that later neighbors may need: an individually valid early removal can otherwise
leave a later neighbor with too many indispensable branches despite a feasible
alternative pruning order. The new link is a preference, not a protected edge;
some fixed edge sets remain genuinely infeasible and must still fail explicitly.
Both phases share the same local-work and external witness budgets and cache.
Pruning first checks the immediate neighborhood.
If that induced subgraph is insufficient, a bounded bidirectional breadth-first
search may prove an existing route through external vertices. Forward search
starts from currently retained outgoing neighbours; backward search starts from
the candidate destination. Expand the smaller frontier and accept a proof when
the visited sets meet, before scheduling further unrelated expansion. Both sides
exclude the pruning target. Cached stored adjacency is therefore independent of
earlier outgoing-edge removals within the same pass.

Backward search uses physical reverse bookkeeping to enumerate possible
predecessors, then reads the opposite edge's active flag. An ignored row is never
itself evidence of a traversable edge. Missing reverse rows fail as index
corruption; legacy indexes still require the documented rebuild. During removal,
both forward rows and backward columns between incident vertices come from the
current repair plan, not cached stored edges. This preserves uncommitted bypasses
and excludes previously pruned bypasses in both search directions. Reaching an
empty frontier disproves that particular alternate route without exploring an
unrelated large component; other removable edges can still be considered.
The fallback only reads existing edges; it adds no connections and never traverses
ignored reverse records. Missing proof or exhausted work triggers the bounded fallback below.
Shrinking selects from existing outgoing neighbors, matching the bulk builder's
no-extension policy. An infeasible bounded degree constraint falls back for that
neighbour (decline the new link, else the heuristic selection; below), with the same
warning.

Repair is bounded by 256 incident/pruning vertices, 4,096 physical rows per
adjacency scan, 1,048,576 incident-vector components per layer, 16,777,216 distance components
per layer repair and 1,048,576 adjacency checks per pruning pass. Exceeding
a limit raises `hnsw::repair_limit` with a reason inside the repair; the write path
then applies the bounded fallback below. It never fails the user's write. These are local
repair work bounds, not a statement RSS limit or a hard wall-clock guarantee.
They must never silently truncate the neighborhood or skip a required connection.
The external-path fallback additionally caps cached vertices and the combined
distinct vertices visited by both search directions at 1,024,
physical adjacency rows read (including reverse-edge point reads) at 16,384, and traversed adjacency entries at
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
Recurring `hnsw.repair_fallback` warnings mean parts of the graph were maintained
without path preservation; schedule `::reindex` from the warning count. No
production database is rebuilt by the release itself.

## Bounded fallback (0.19.1)

0.19.0 rejected the user's write whenever a repair exceeded its work bounds. Under
sustained churn on clustered data that rejected a growing share of ordinary
updates, deletes and inserts (about 4% rising to 20% per round at 2,000 rows, m=16),
while 0.18, which never repaired, accepted every write but left rows unreachable
(11% after ten rounds). Neither is acceptable for a long-lived memory store.

From 0.19.1 a repair limit never fails the write. Every limit is checked before
the repair writes anything, so the fallback starts from an untouched graph:

- removal: skip the bypass repair for that layer. Before the unlink, give each
  in-neighbour u of the removed vertex x (an active u→x edge) one replacement
  edge u→v to the nearest live out-neighbour v of x that u does not already link
  to, then unlink x. u loses u→x and gains u→v, so its degree is unchanged and no
  cap is exceeded (a one-edge localized reconnection, after arXiv 2503.00402).
  An in-neighbour whose own adjacency scan exceeds a bound is skipped; the step
  never fails the write;
- insertion pruning: if the neighbour's existing edges still fit the degree cap
  without the new link, keep all of them and decline only the link to the inserted
  vertex, so no existing row loses a path; otherwise keep the heuristic neighbour
  selection, exactly as before 0.19.

Measured (clustered vectors, 2,000 rows, dim 64, m=16; ten rounds each
re-embedding 10%, deleting 5% and inserting 5%; rows not found by searching their
own vector at k=1, ef=64):

| build | writes rejected | unreachable after 10 rounds | recall@10 |
|---|---|---|---|
| 0.18.0 | 0 | 227 | 0.85 |
| 0.19.0 | 14–87 per round | 0 | 1.000 |
| 0.19.1, plain unlink | 0 | 9 | 1.000 |
| 0.19.1, reconnect | 0 | 1 | 1.000 |

At 10,000 rows (same churn, ten rounds): 0.18.0 789 unreachable, recall 0.912;
0.19.0 3,293 writes rejected, 5 unreachable, recall 0.997; plain unlink 55
unreachable, recall 0.998; reconnect 0 rejected, 32 unreachable, recall 0.997.

A diagnostic at 4,000 rows classified the reconnect build's 6 misses: 4 still had
live in-edges and were found at `ef: 400` (ordinary approximate-search misses,
which 0.19.0 also shows), and 2 had no active in-edge (orphans, unreachable by
any ANN search). Exact scans found all 6 and `::reindex` restored them.

### Orphan guards (0.19.2)

A vertex with no active in-edge on a layer cannot be reached by any search, at
any `ef`. The fallbacks above could create one three ways; each now checks
before it drops the last in-edge. "Has another in-edge" is decided from the
vertex's own bookkeeping rows (every active `u -> y` has a row `y -> u`), with
the usual adjacency-scan bound; over the bound the guard leaves the edge set
alone rather than failing the write.

- Insertion: if every neighbour declined or pruned its link back to the new
  vertex on a layer, the nearest neighbour that can takes it back: with spare
  degree it simply adds the link; at the cap it swaps out its farthest edge
  whose target keeps another in-edge.
- Heuristic fallback shrink: an edge the heuristic would drop that is its
  target's only in-edge is kept, in exchange for the farthest selected edge
  whose target keeps another. The selection never grows.
- Removal fallback: out-neighbours of the deleted vertex whose only in-edge
  came from it are covered first, each by its nearest still-unassigned
  in-neighbour; the other in-neighbours then take their nearest target as in
  0.19.1. Still one new edge per in-neighbour.

Each guard preserves degree caps and adds no new failure mode. The churn
regression asserts no bottom-layer orphans. Ordinary search-width misses remain
(found at a wider `ef`); `::reindex` remains the full rebuild.

Each fallback emits a structured `hnsw.repair_fallback` warning (`::warnings`,
`Db::recent_warnings`) naming the relation, operation, layer and limit. Writes
that repair within bounds keep the full path-preservation guarantee. Errors other
than repair limits (corruption, missing vectors, I/O) still fail the write and
still block commit through the incomplete-mutation guard.

## Acceptance

- Deterministic level-zero bridge fixture fails before repair and passes after it.
- Delete, duplicate/distinct vector replacement, list/field identities, ignored
  reverse edges, degree accounting, and upper-layer/entry deletion are covered.
- Repeated mutations, read-your-writes, script abort, explicit abort, failed commit
  and persistent reopen pass on SQLite and RocksDB.
- Pruning checks demonstrate an alternate path survives; infeasible degree/work
  cases fall back with a `hnsw.repair_fallback` warning and never fail the write
  (0.19.1). Sustained clustered churn commits every write and keeps survivors
  reachable (`sustained_clustered_churn_never_fails_a_write`).
- An existing alternate path outside the immediate neighbourhood is recognized;
  insertion pruning and removal repair both cover this case. Ignored edges are
  excluded and witness-budget exhaustion remains explicit.
- Wide-component fixtures recognize short proofs without exhausting the frontier,
  retain necessary leaf edges, and find a different removable edge. Vertex, row
  (including reverse probes) and step limits still fail explicitly. Reusing a
  stored-adjacency cache respects changing repair plans in both directions.
- A deterministic sequential-pruning fixture retains the new link until older
  local/external proofs are tried, permits its later removal when necessary, and
  checks final degree caps and all-pairs reachability. Duplicate-vector fixtures
  exercise the consumer's 100/100/1 insertion batches and verify full retrieval.
- Review insertion pruning interaction and partial legacy-transaction errors.
- Run HNSW/reindex/hybrid and transaction regressions and strict Clippy. Retain
  immutable fix SHA and independent storage-contract review before delivery.
- Qualify the new release candidate separately. Do not relabel artifacts from
  engine `0148693b` as repaired-candidate evidence. Long-run performance/quality
  measurements remain pending the owner's quiet-window signal.
