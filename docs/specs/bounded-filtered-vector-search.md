# Bounded filtered-vector search: experimental contract

Status: draft for an isolated prototype, November 5, 2026. This specifies required
behavior, not an available API or an accepted 0.19 feature. Existing `filter:`,
`ef`, scripts and storage formats retain their semantics. No new public syntax,
Python transaction handle or automatic routing threshold has been implemented.

## Evidence and design decision

The [update investigation](../reviews/2026-11-05-hnsw-update-investigation.md)
separates two failures: global candidate truncation before filtering, and physical
disconnection after vector changes/deletions. A third gap is resource accounting:
the current ANN loop does not receive the query poison or account for its visited
set and vector cache. An ef limit does not settle any of these independently.

Start with exact evaluation over a bounded, complete eligible-key set. Add a
resumable graph search only after actual-work accounting and mutation correctness
are tested. Compare an ACORN-style variant separately. Graph maintenance is a
separate prerequisite for reliable ANN under mutation; query-time filtering alone
cannot reconnect disconnected components.

This order follows, without importing their performance claims:

- [Lucene's documented exact fallback](https://www.elastic.co/docs/solutions/search/vector/knn/filtered-knn-search)
  uses eligible cardinality and observed graph visits to choose direct scoring.
  Mnestic needs its own measured crossover by dimension/backend; the same numeric
  threshold is not assumed.
- [pgvector iterative scans](https://github.com/pgvector/pgvector#iterative-index-scans)
  illustrate continuation until enough results or a limit. Its scan-tuple limit
  is approximate and excludes the initial scan. Here the proposed work ledger
  includes initialization and every continuation.
- [ACORN](https://arxiv.org/html/2403.04871v1) describes predicate-subgraph traversal
  and ACORN-1's extended neighbor exploration. Evaluate one/two-hop expansion
  with explicit edge/visit limits and query/filter correlation strata. Do not
  assume simply rejecting ineligible neighbors preserves reachability.
- [FreshDiskANN](https://arxiv.org/abs/2105.09613) studies graph maintenance under
  updates. Its results motivate a separate mutation workload; they do not prove
  Mnestic's transaction, rollback or storage invariants.

## Inputs and eligibility

The first prototype supports one vector column with one vector per row, explicit
dimension and F32/F64 dtype, and L2/Cosine/IP using the existing engine distance
functions. L2 means squared distance; IP means `1 - dot`. Reject a mismatched
dtype/dimension, non-finite input/distance and zero-norm cosine input explicitly.
Do not silently normalize or change scoring. Multiple vector columns, list-vector
subindices and temporal validity need a later contract and fail validation here.
These restrictions belong to the experimental surface only.

Candidates are a sequence of complete typed primary-key tuples in schema order:
Rust logical shape `Vec<Vec<DataValue>>`, Python logical shape list-of-lists.
This is a data contract, not a declaration of a new callable. Single keys still
use one-element tuples; composite keys must have the exact arity/types. Do not
coerce invalid keys or truncate oversized input. Bound both input tuple count and
encoded bytes **before** deduplication. Duplicate keys score once; absent/deleted
keys are skipped. Bound key width, vector dimension, k and output bytes too.

Eligibility is the intersection of supplied keys, row existence and a compiled
row predicate evaluated in the search snapshot. Predicate failures are errors,
not nonmatches. The initial predicate surface is scalar comparisons and Boolean
composition with bounded-width values; arbitrary host callbacks, unaccounted
allocating expressions and regex execution are excluded. A graph filter controls
result membership, while traversal may visit nonmatching bridge nodes. Such
nodes never enter the returned results.

Caller-supplied keys define the search universe; incomplete input cannot support
a claim of exactness over the full predicate. For predicate-wide exactness,
enumerate every eligible key within the same snapshot and budgets. Overflow must
fail or select a separately budgeted graph path; it must never silently truncate
eligibility. An empty successfully enumerated set returns exact empty results.

## Ranking and result semantics

Exact mode scores every eligible vector and returns the first min(k,eligible)
rows ordered by `(distance, full primary key)` with the engine key ordering.
Maintain a bounded top-k heap in the prototype. The existing scripted exact
example uses a sort and is not this memory-bound implementation. All distance
operations retain the selected dtype's arithmetic; validate against an independent
oracle with declared floating-point tolerances and tie rules.

Graph mode ranks observed eligible results with the same comparator and reports
`exact=false`. Finding k results or exhausting the reachable frontier establishes
neither exact top-k nor absence of other eligible rows. A short/empty approximate
result must not be labeled "no matches". Only a completed exact scan can establish
that the eligible universe is empty or has fewer than k rows.

Return diagnostic metadata: selected mode, exactness, stop reason, returned count,
eligible count when completely known, and cumulative work/memory counters.
Successful graph stop reasons distinguish enough candidates from exhausted
frontier. Required-budget exhaustion, deadline expiry and cancellation return a
structured error and **no successful result rows** in the first prototype. Include
the exhausted resource/counters in the error. Do not overload an empty list.
Existing timeout/killed/memory diagnostics remain compatible; a new work-limit
diagnostic needs an explicit name and compatibility review before publication.
Partial-result opt-in is outside the first prototype.

## Snapshot and transaction boundary

Eligibility, base vectors, graph traversal and any exact fallback use one native
read transaction. Within a write transaction they must see the same transaction's
own changes. Multiple attempts never open fresh snapshots. Never publish
continuation state across requests; dropping the request releases its state and
transaction. A failed governed statement retains the existing terminal rollback
behavior; don't catch its error and continue on a closed transaction.

Use the existing [governed transaction ownership](governed-transactions.md):
RocksDB pins a snapshot while writers may commit; SQLite holds its read guard and
therefore can block writers. Measure that cost. The host retains admission until
the actual worker exits, even if the caller times out. Python parity requires a
separately reviewed binding or a single native operation owning the full search;
repeated current Python `run_script` calls do not satisfy this snapshot contract.

Hybrid evaluation must apply the same eligibility and snapshot to both legs.
FTS candidates restrict before top-k while preserving corpus-global BM25
statistics. Report vector recall separately so fusion cannot conceal ANN failure.

## Budgets and instrumentation

Every request supplies an absolute deadline and finite caps; no guessed production
defaults are selected. One cumulative ledger covers eligibility construction,
initial search, continuation, fallback and result construction. Switching modes
or widening ef never resets the ledger. The prototype must expose and enforce:

| Resource | Accounting boundary |
|---|---|
| Candidate count and encoded bytes | Before allocating/deduplicating the carrier; ingress allocation remains host-owned |
| Rows examined and key lookups | Eligibility scans, base reads and fallback; account before issuing work |
| Edge rows examined | Every adjacency entry, including ignored/duplicate entries and second-hop expansion |
| Distinct vertices visited | Before inserting a new visited key, including initialization and upper layers |
| Distance evaluations | Every calculation, including upper layers, retries, reranking and exact fallback |
| Estimated live bytes | Carrier, visited set, frontier/results heaps, vector cache, fetched tuples, scratch/output buffers and retained continuation capacity |
| Deadline/cancellation | Before/after storage batches and at bounded intervals inside scans, distance work and graph loops |

Check/increment counters before dispatch; size batches to the remaining allowance.
Charge reserved capacity and key/vector widths before growing containers; release
charges when storage is actually released, not merely when lengths decrease.
Account for the temporary distance-expression allocation as well as cached vectors.
Upper-bound the dimension so one distance calculation cannot be arbitrarily large.
Instrument counters independently from result counts. Test initial-search
exhaustion and mode switches, where budget resets are particularly easy to hide.

Estimated memory is not total process RSS. Cooperative deadlines cannot preempt
an in-flight storage call or make a hard wall-clock latency guarantee. The host
must also cap admitted work and input bytes. A request returning an error while
its worker continues must retain admission. The E2 250-ms/64-MiB settings remain
measurement targets, not guarantees established by this document.

## Routing and experiments

First expose explicit exact versus graph modes for comparison. An `auto` policy
is experimental until paired measurements establish a crossover for backend,
dimension, eligible count and predicate cost. Estimate/filter discovery work is
part of the cost. A switch from ANN to exact uses remaining budgets and the same
snapshot; if insufficient, return budget exhaustion. Never retry with looser
limits implicitly.

Continuation should retain the frontier, visited set and scored results within
the request. Re-running independent ef=100/200/400/800/1600 calls is a separate
baseline and must charge repeated work. An ACORN-style variant must charge
second-hop edges even when it avoids their distance calculations. Neither mode
is a substitute for repairing an already disconnected index.

The frozen [E1–E3 protocol](../plans/0.19-measurement-protocol.md) remains unchanged;
timings wait for the owner's quiet window. Any new engine prototype has a new
SHA and separately versioned experiment manifest. Preserve the baseline and
candidate campaign at engine `0148693b`, harness `8050e72`, instead of relabeling
new implementation results as the frozen candidate's evidence.

The subsequently approved 0.19 mutation repair supersedes `0148693b` as a release
candidate. Keep that old manifest as a pre-repair control and create a new pinned
candidate/artifact manifest before release timings; the old resume command must
not silently stand in for the repaired release campaign.

## Acceptance and staged implementation

1. **Completed investigation:** retain graph witnesses, released/candidate parity,
   and the executable exact eligible-key example. It is current-script guidance,
   not a new bounded API. See the linked investigation for 1,344 exact assertions.
2. **Graph-maintenance prerequisite:** create a deterministic topology fixture,
   reproduce bridge deletion/vector replacement, then design the smallest repair.
   Validate degree/pruning, entry-point and vector-identity invariants, repeated
   mutations, rollback/failed commit and persistent reopen on SQLite/RocksDB.
   Full-neighbor reconnection is not selected without bounded update-cost analysis.
3. **Bounded exact prototype:** instrument all ledgers and test exact ordering,
   empty/fewer-than-k, duplicate/missing/composite keys, predicate intersection,
   dtype/dimension/invalid values, snapshot overlap and read-your-writes.
4. **Bounded continuation:** add cancellation and budget checks inside ANN; test
   strict caps during initial and resumed search, memory release, fallback and
   host admission retention. Test both backends' distinct concurrency behavior.
5. **Algorithm comparison:** compare fixed ef, charged restarts, continuation,
   exact eligible search and ACORN-style expansion on the preregistered selectivity,
   correlation, mutation and dimension strata. Record quality and total cost.
6. **Public surface:** review Rust/Python behavior, error naming, documentation and
   compatibility before selecting syntax or defaults. No storage-format change
   is assumed; a topology/construction change needs explicit migration/rebuild
   analysis. Requalify any production candidate that changes.

The graph-maintenance defect is recorded in 0.19 readiness separately from optional
filtered-search features. This draft neither expands the frozen release scope
automatically nor declares that defect accepted for release.
