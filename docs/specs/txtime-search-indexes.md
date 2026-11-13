# Search and secondary indexes on transaction-time relations

Status: **DRAFT — not reviewed, not approved.** Selected as roadmap item 1 by the
owner on November 13, 2026. Passes run (workspace `spec-authoring` method): 1 (code
facts, verified against `main` at `fd0d3d0d` and the published 0.19.2 wheel on
November 13, 2026) and 2 (this draft). Pass 0 does not apply (no user interface).
Passes 3–5 (adversarial panel, prior-art grounding, simplification) have **not**
run; the open questions at the end are the ones they should attack first.

## Goal

Today a relation can keep transaction-time history (`TxTime`) or have search
indexes, never both. This spec lets a system-versioned (tt-only) relation carry
B-tree, full-text, vector and LSH indexes that answer for the **current belief**,
and defines what a search **as of** an earlier transaction time does.

## Verified current state

| Claim | Where | Consequence |
|---|---|---|
| `::index`, `::fts`, `::hnsw`, `::lsh create` and `::set_triggers` reject `TxTime` relations with `eval::txtime_unsupported_op` | `runtime/relation.rs:984-992`; call sites `:1006`, `:1226`, `:1357`, `:1571`, `:1891`; confirmed on the 0.19.2 wheel | The gate this spec lifts for four of the five |
| Writes to a `TxTime` relation are buffered on the transaction and written at commit with one engine-assigned tt | `runtime/transact.rs:60` (`pending_tt_writes`), `:322`, `:390`, `:406-479`; every write op funnels there (`query/stored.rs:1420`, `:1586`, `:1727`, `:1821`, `:1930`; `runtime/db.rs:1421`) | One place to maintain indexes; statement-time maintenance cannot know the tt |
| The tt commit runs under a per-`Db` mutex: allocate tt, stamp rows, persist the high-water mark, commit | `commit_tx_with_tt`, `runtime/transact.rs:380-398` | tt order equals commit order; the section's length is paid by every tt committer |
| On RocksDB a write transaction validates its **writes** against a begin snapshot, but its **reads** see the latest committed state | `storage/rocks.rs:283` (`set_snapshot(true)`); `cozorocks/bridge/tx.h:20-34`, `:87-103` (`ReadOptions` never receives a snapshot) | A read of "the current belief" at commit time is fresh; index-row writes can still hit a snapshot conflict |
| `put_externally_serialized` drops the begin snapshot so a mutex-serialized key cannot conflict | `storage/rocks.rs:435-446` | Precedent for index rows written inside the tt section |
| Script writers hold a **shared** guard on each relation lock; index DDL takes it exclusively. A vector index is built from a snapshot without the lock, then reconciled against the base under it | `runtime/db.rs:2083-2086`; `:2455-2507`, `:3208`, `:3239`; `reconcile_hnsw_index`, `runtime/relation.rs:1791-1805` | Writers to one relation run concurrently on RocksDB; commits landing during a vector build are picked up by the reconcile |
| SQLite and `mem` admit one writer at a time | `storage/sqlite.rs:204` (`begin immediate`); `storage/mem.rs:51-54` | The concurrency questions below are RocksDB-only |
| A current-belief point read for a tt-only key exists | `tt_current_row`, `query/stored.rs:1128-1142` (one skip-scan seek) | Reusable as the "old tuple" lookup and the base fetch |
| Index maintenance helpers take an old tuple and a new tuple and skip unchanged fields | `query/stored.rs:958` (`update_in_index`), `:516` (`replace_in_fts`), `:542` (`update_in_hnsw_changed`), `:482`/`:507` (LSH) | Reusable unchanged once the old tuple is supplied |
| Index backfill, the vector-build reconcile, rebuild and the filtered exact scan read `scan_all`, which on a `TxTime` relation would return every version | `runtime/relation.rs:1319`, `:1455`, `:1738`, `:1805`, `:2007`, `:2021`; `runtime/reindex.rs:117`; `runtime/hnsw.rs:1505` | Each must read current beliefs instead |
| `:as_of` sets a default tt on relation atoms and fixed-rule inputs but skips search atoms | `data/program.rs:1020-1061` | Harmless today (search cannot reach a tt relation); a silent-current-result hazard once it can |
| `choose_index` skips an index for a time-travel query unless the index's last column is the relation's last key | `runtime/relation.rs:247-274` | The seam for "no index on a historical read" |
| On a valid-time-only relation, search indexes hold **every version** and a secondary index answers from superseded versions | 0.19.2 probe: FTS returned the superseded text; `*d:byt{text: 'alpha old', k @ 'NOW'}` returned the key although the current text differs | Existing behaviour, not changed here; the reason valid time is out of scope |
| `:returning`, event callbacks and graph-projection sources reject `TxTime` relations | `query/stored.rs:1183-1186`; `runtime/graph_projection.rs:1462` | Unchanged |

## Decisions

**D1 — Scope: tt-only relations, four index kinds.** `{k…, tt: TxTime => v…}`
relations accept `::index`, `::fts`, `::hnsw` and `::lsh create`. Bitemporal
relations (`Validity` + `TxTime`) stay rejected with the existing diagnostic:
valid time has no write-time notion of "current" (a future-dated version becomes
current without any write), and the valid-time index behaviour verified above
would be inherited. Triggers, `:returning`, callbacks and graph projections stay
rejected. Seam: `reject_txtime_relation` gains the tt-only exception for the four
index call sites.

**D2 — An index holds the current belief only.** For each logical key whose
latest record is an assertion, the index holds exactly the entries a plain
relation would hold for that row. Superseded and retracted versions have no
entries. Index rows are keyed by the **logical key** (the base key without the
tt column), so an update that leaves an indexed field unchanged leaves its
entries untouched, as on plain relations since 0.19.2. Seam: the four index
creation functions build their `src_*` columns from the keys minus the trailing
`TxTime` column.

**D3 — Maintenance runs at commit, inside the tt section.** In
`commit_tx_with_tt`, after the tt is allocated and before base rows are stamped:
collapse each relation's buffered rows to the last row per logical key; read
that key's current belief with `tt_current_row` (latest committed, see the
table); call the existing helpers with (old, new). A retraction calls only the
delete side. On RocksDB the begin snapshot is cleared on entry to the section,
as `put_externally_serialized` already does, so index rows written by an
earlier committer do not raise a snapshot conflict. Nothing is written to an
index at statement time, so a script still cannot see its own writes to a
`TxTime` relation through the base relation or through any index.

**D4 — Reads resolve the logical key.** Every base fetch made from an index
(HNSW vector fetch and result rows, FTS and LSH result rows, the join back from
a secondary index) uses a `RelationHandle::get_current` helper generalised from
`tt_current_row`. The tt column in a result is the resolved row's tt.

**D5 — Backfill and rebuild read current beliefs.** The eight `scan_all` sites
in the table use `skip_scan_all` at end-of-time for tt-only relations. The
vector-build reconcile compares current beliefs by logical key.

**D6 — No silent history.** A historical tt never reaches an index.
- Secondary index: `choose_index` is not consulted when the atom carries a
  historical tt (explicit selector or `:as_of`); the base relation answers. An
  explicit index atom with a historical tt is an error.
- Search atom in a block with `:as_of`, or hybrid search: error
  `search::as_of_unsupported` in batch 1. A search atom on a tt relation counts
  as a tt reference for the `:as_of` typo guard.
- Batch 2 replaces the vector error with an exact scan of the beliefs held at
  T, bounded by `FILTERED_EXACT_MAX_ROWS` (200,000 scanned rows,
  `runtime/hnsw.rs:1633`); beyond the cap it is an error, never a partial list.
- Full-text and LSH as-of stay errors until batch 3 is activated.

**D7 — History operations.** `::history_gc` removes only superseded records, so
it never changes an index. `::evict` removes the key's index entries; that work
is deferred into the commit section like any other index write (evict already
commits through it, `runtime/db.rs:3085`).

**D8 — Failure and budgets.** An index error inside the section fails the
commit; nothing is committed and the allocated tt is burned, which the clock
already permits. The transaction's deadline is checked before entering the
section. Work inside it is not preempted.

## Invariants

1. At every committed state, each index on a tt-only relation equals the index
   a plain relation holding exactly the current asserted beliefs would have.
2. No index row of a `TxTime` relation is written outside the commit section,
   except by index DDL: under the exclusive relation lock, or into a
   not-yet-attached vector index during its unlocked build.
3. Plain and valid-time-only relations keep byte-identical storage and cost.
4. tt order equals commit order (unchanged).
5. A query naming a historical tt returns the belief at that tt or an error;
   it never returns current-belief rows from an index.

## Acceptance tests

SQLite with `tempfile::tempdir()` and RocksDB; `mem` only for parser-level cases.

- Create each index kind on a populated tt-only relation: superseded and
  retracted versions are absent; results equal an exact scan of current beliefs.
- Correct a row: search finds the new version only; `@ (tt: T)` on the base
  still returns the old one. Retract, then re-assert: entries follow.
- Same key written twice in one transaction: one entry, last write wins.
- Update only a non-indexed field: FTS postings and HNSW rows are untouched
  (row-identity check, as in `tests/write_path_0192.rs`).
- A script that writes then searches sees neither base nor index changes.
- Concurrent RocksDB writers on one key, and on two new keys: invariant 1
  holds after both commit; no snapshot-conflict error from index rows.
- `::evict`, `::history_gc`, `::reindex`, `import_relations`, `:reconcile`,
  backup and restore: invariant 1 after each.
- Index failure injected inside the section: commit fails, base and indexes
  unchanged, later commits proceed.
- Every D6 case: error codes, and (batch 2) as-of vector results equal the
  exact top-k of the beliefs at T; the cap produces the named error.
- Bitemporal relations, triggers, callbacks, projections: still rejected.

Performance, measured on the 40,000-row benchmark shape and recorded before
release (targets to be confirmed by the panel):

| Measure | Target |
|---|---|
| Plain-relation read and write latency | unchanged (dispatch only) |
| Vector and full-text search p50, tt-only against plain | ≤ 1.5× |
| Single-row update p50, tt-only against plain | ≤ 1.3× |
| Time inside the tt section per indexed row (RocksDB) | reported, with p99 |

## Residual risks

- **Writers serialize on RocksDB.** The tt section now includes index work
  (tens of milliseconds per row with a vector index), so concurrent commits to
  any `TxTime` relation in the same database wait for it. A 1,000-row batch
  can hold the section for seconds. SQLite and `mem` already admit one writer.
- **Search fetches cost a seek, not a point read**, and lose the batched
  `multi_get` on vector fetches. The 1.5× target exists to catch this.
- **Bulk `import_relations` maintains every index** on a tt relation (it flows
  through the same funnel), unlike plain relations where it skips search
  indexes. Guidance: drop, import, recreate.
- **As-of vector search is exact, not the approximate result the index gave at
  the time.** It reproduces the belief, not the historical ranking noise.

## Layering ruling

Engine mechanism only: all changes are in `mnestic`. MindGraph adopts nothing
here. This closes one of the four prerequisites the bitemporality spec lists
for a MindGraph pilot (indexed current reads); triggers and projections,
history-preserving recovery and reversible enablement remain open.

## Rejected alternatives

- **Statement-time maintenance.** The base row's tt does not exist yet, and an
  index would show writes the base relation hides.
- **Index rows keyed by the full base key including tt** (no creation-code
  change, exact point reads). Every new version would re-key the row's entries:
  a full vector remove and insert even when the vector did not change.
- **Versioned index entries** (assert and retract rows stamped with tt, as
  sketched in the bitemporality spec §8). Gives indexed as-of reads for
  secondary indexes, at the cost of history in every index, coupling to
  `::history_gc` and `::evict`, and no answer for vector graphs. Revisit for
  B-tree indexes alone if an as-of lookup by secondary key is shown to be too
  slow as a base scan.
- **A per-key "current head" row** (logical key → current tt). Would give
  point-read resolution and row-level conflict detection, allowing index work
  outside the tt section. New storage on every indexed relation; revisit if the
  serialization or the 1.5× search target fails in measurement.
- **Maintenance before the tt lock, relying on row conflicts.** Two concurrent
  inserts of one new key touch disjoint index rows and both commit, leaving
  entries for a superseded version.
- **A versioned vector graph.** Edge history multiplies with every neighbour
  rewrite; no consumer.
- **Per-atom tt selectors on search atoms.** `:as_of` covers the reproduction
  case; add later if pulled.

## Batches (review units)

1. **Current-belief indexes** (D1–D5, D7, D8, D6 errors): the unblock. Largest
   batch; ships alone.
2. **As-of vector search by bounded exact scan** (D6).
3. **As-of full-text and LSH** — not designed here. Gate: a consumer names an
   as-of text-search question. Candidate: materialise postings for the beliefs
   at T into the transaction's temporary store, bounded, and run the existing
   evaluator.

## Open questions for review

1. Is clearing the begin snapshot at section entry sound for every key the
   transaction wrote earlier, or only for the final high-water-mark write it
   was introduced for?
2. Can `::evict`'s statement-time base deletes and a concurrent committer's
   index work deadlock or interleave, given evict also runs under a shared
   relation guard?
3. Does any path write a `TxTime` relation without passing through
   `stamp_pending_tt_writes`?
4. Should batch 1 ship secondary indexes at all before the as-of base-scan
   fallback is measured, or only the three search kinds?
5. Are the 1.5× and 1.3× targets the right bar, and what is the measured cost
   of a seek against a point read on both backends?
