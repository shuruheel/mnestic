# HNSW update omission: graph disconnection

Status: cause established for the retained tiny fixtures; engine repair is not
implemented. Exact eligible-key retrieval is verified on both packages. This is
correctness investigation only. E1–E3 timings remain deferred to the owner's quiet
window. The [bounded-search contract](../specs/bounded-filtered-vector-search.md)
separates query filtering from graph maintenance.

## Evidence and interpretation

The [fixture](0.19-hnsw-update/investigate.py) runs eight mutations, ten fresh
random graph builds each, on SQLite and RocksDB. It captures base rows, directed
edges, reachability from every base node, unfiltered ANN and filtered ANN at
ef=50/1600, then rebuilds the index over identical rows. Each graph has 25 initial
two-dimensional F32 vectors `[i,i]`, `m=8`, `ef_construction=50`, L2 distance,
query `[0,0]` and k=30. Rare keys initially are 20, 22 and 24.

| Mutation | 0.18 SQLite | 0.18 RocksDB | Candidate SQLite | Candidate RocksDB |
|---|---:|---:|---:|---:|
| Metadata only; vector unchanged | 0/10 | 0/10 | 0/10 | 0/10 |
| Change key 23 to duplicate vector `[22,22]` | 7/10 | 4/10 | 6/10 | 5/10 |
| Change key 23 to distinct vector `[22.5,22.5]` | 4/10 | 4/10 | 4/10 | 5/10 |
| Delete bridge key 23 | 5/10 | 4/10 | 6/10 | 3/10 |
| Delete leaf key 24 | 0/10 | 0/10 | 0/10 | 0/10 |
| Insert duplicate vector at new key 25 | 0/10 | 0/10 | 0/10 | 0/10 |
| Original combined metadata/vector update | 9/10 | 3/10 | 3/10 | 4/10 |
| Abort vector update | 0/10 | 0/10 | 0/10 | 0/10 |

Cells count builds with unfiltered omissions after mutation. HNSW randomness is
uncontrolled: differences between packages/backends are **not** regression rates
or evidence of improvement. Released source is
`1d13aedc7a309105ae147f2ed1d79cd18a078b00`; candidate source is
`0148693b93f23956896999ba9a59f1fb4889fab6`.

Across all 320 fixtures:

- Before mutation and after rebuild, all four ANN queries return every expected
  key. All after/rebuilt base rows are identical.
- After mutation, the unfiltered missing set equals exactly the base keys outside
  node 0's directed layer-0 reachable component, in every capture. There are 40
  failing baseline fixtures and 36 candidate fixtures.
- ef=1600 has the same missing set as ef=50. Filtered omissions equal the eligible
  subset of unfiltered omissions. No returned key is ineligible or duplicated.
- Base row 24 remains present and unchanged except in the intentional leaf-delete
  case. Rollback preserves both base rows and graph rows.

This establishes physical graph disconnection for these omissions, independently
of filtering, insufficient ef, duplicate-vector ties or missing base data. It
does not establish a universal connectivity guarantee for fresh HNSW graphs.

## Source mechanism and a concrete witness

At the candidate, [hnsw_put_vector](../../cozo-core/src/runtime/hnsw.rs) returns
early for an unchanged vector hash (lines 285–289). A changed vector calls
`hnsw_remove_vec` before reinsertion (line 291). Removal deletes the vertex's
edges and their reciprocal entries and decrements neighbor degrees, without
reconnecting the remaining neighbors (lines 1076–1193). The inherited comment
at line 1107 explicitly acknowledges possible disconnection.

Candidate SQLite `duplicate_update`, repetition 0, gives a simple witness:

```text
Before:  ... 21 <-> 22 <-> 23 <-> 24
Update:  remove 23's links; reinsert 23 at vector [22,22]
After:   ... 21 <-> 22 <-> 23       24 (isolated)
Rebuild: ... 21 <-> 22 <-> 24       23 also connected
```

The retained graph rows show that 24's only active connection was through 23;
the update removes it. Reinsertion searches the surviving reachable component.
The base row for 24 is still `[24,'rare','alpha',[24,24]]`. Both ANN modes omit it
at both ef settings. Rebuild reconnects it. Other witnesses disconnect larger
sets; the diagram is one specific capture, not the topology of every run.

Adding two-hop filtered traversal cannot bridge a component with no surviving
path. A maintenance fix needs its own contract for neighbor repair, degree/pruning
invariants, entry points, repeated mutation and transaction rollback. Simply
increasing ef or connecting every pair of neighbors is not an established fix.

## Verified exact path

[exact_eligible.py](0.19-hnsw-update/exact_eligible.py) executes the existing
CozoScript path, independent of HNSW edges:

```cozoscript
eligible[id] <- $eligible_rows
?[id,d] := eligible[id], *docs{id,kind,v}, kind == $kind,
           d = l2_dist(v,vec($q))
:order d,id
:limit $k
```

For this single-Int-key, single-F32-vector schema, pass candidate keys as rows,
e.g. `[[22],[23],[24]]`, and a positive k. `l2_dist` is **squared** Euclidean
distance. The candidate relation deduplicates keys; missing keys disappear at the
join; the stored predicate is rechecked in the same query. Ties use primary key.

Each package passed 672 assertions across both backends, before/after all eight
mutations. Cases cover all keys, no keys, missing keys, duplicate keys mixed with
ineligible rows, one result, equal vectors, and k=1/3/30. Independent arithmetic
over base vectors provides the expected distances/order. Both captured plans use
`stored_prefix_join`. This fixture tests exactly representable L2 values; it does
not qualify arbitrary metrics, dimensionalities or floating-point tolerances.

Exactness is relative to the supplied candidate set and predicate. A stale or
incomplete external key set can omit eligible rows. Derive complete eligibility
in the same query or retain a governed native snapshot across eligibility and
scoring. The current Python API does not offer that multi-query governed handle.
The existing sort can materialize candidates; this example is not proof of a
hard work/memory/latency bound. No automatic routing threshold is selected.

## Reproduction and evidence audit

Use isolated environments containing the released wheel and optimized candidate
wheel documented in the [candidate qualification](2026-11-05-0.19-candidate.md).
The scripts need the wheel with both storage backends; no native build is needed.

```sh
python docs/reviews/0.19-hnsw-update/investigate.py --output /tmp/update.json
python docs/reviews/0.19-hnsw-update/exact_eligible.py --output /tmp/exact.json
python3 docs/reviews/0.19-hnsw-update/audit.py
```

The first two commands run new disposable fixtures. The audit reads the committed
captures, verifies hashes, recomputes reachability and checks result invariants;
it needs only the Python standard library. Investigation exits successfully on
completed collection, even when ANN misses rows. It is not a passing regression
test for repaired ANN. The exact example exits nonzero on any mismatch.

[manifest.json](0.19-hnsw-update/manifest.json) pins source SHAs, extension hashes,
fixture hashes and compressed evidence. `baseline.json.gz` / `candidate.json.gz`
contain all graph and result captures; `exact-*.json.gz` contain exact-query
results and plans. The logs summarize omission counts. Counts may differ on rerun;
use captured topology for auditing the demonstrated cause.

## Release disposition and next implementation boundary

The frozen candidate is unchanged. This inherited correctness defect is now
explicit in the release plan, rather than an unresolved filtered-recall symptom.
A repaired candidate must get a new immutable SHA, mutation regression coverage
and package validation. These observations do not establish its performance.

Next maintenance work should first make the bridge topology deterministic through
a test-only graph-level/seed seam, then validate a scoped repair against graph
invariants and rollback on both backends. Include duplicate and distinct updates,
delete/reinsert cycles, entry-point deletion, list-vector identities and degree
pruning. A rebuild restored these disposable fixtures; production rebuild cost,
atomic availability and long-run mutation behavior have not been qualified.
No automatic rebuild or new traversal algorithm is shipped by this investigation.
