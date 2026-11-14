# Search and secondary indexes on transaction-time relations

Status: **APPROVED for implementation, November 13, 2026.** The owner accepted
the recommendations on the three open questions ("I agree with your
recommendations"); they are recorded under *Owner decisions*. Selected as roadmap item 1
by the owner on November 13, 2026. Passes (workspace `spec-authoring` method):

| Pass | Date | Result |
|---|---|---|
| 1 Code facts | Nov 13 | Verified against `main` `fd0d3d0d` and the 0.19.2 wheel |
| 2 Draft | Nov 13 | Merged as `23cae8a4` (PR #73) |
| 3 Panel (code grounding, concurrency and crash, semantics) | Nov 13 | 21 findings, 6 blockers; all folded, none rejected. Changes: three-phase commit section (D3), index DDL under the commit lock (D5), `::evict` deletes moved into the section (D7), planner seam moved to `compile.rs` (D6), tt column barred from index specs (D2) |
| 4 Prior art (SQL Server, MariaDB, XTDB, Datomic, Lucene/Elasticsearch, Neo4j, Lance, Milvus) | Nov 13 | D1, D2 confirmed; D3, D5, D6, D7 gaps, all folded: index builds no longer pause commits (D5), as-of secondary lookups bounded (D6), evict capped and its storage caveat stated (D7), commit-section time made a release gate. Essential omission (deferred, watermark-based indexing) recorded as an owner decision |
| 5 Simplification | Nov 13 | See *Simplification* below |

Pass 0 does not apply (no user interface).

Creation history: first committed November 13, 2026 at 09:58:27 Eastern in
`23cae8a4` (PR #73); review revisions merged at 11:16:14 in `3dcf8e58`
(PR #74); owner approval merged at 11:28:19 in `d2934708` (PR #75).

Implementation preflight, November 13, against engine `99a87cd8`: the approved
scope and synchronous-maintenance decision stand. D2–D5, D7/D8 and their tests
below clarify gaps found while preparing the combined TxTime/keyed-access PR:
physical snapshots, detached rebuilds, old-vector availability, lock ordering,
rollback and relation identity. These refinements have not yet been validated
by an implementation. The owner subsequently requested one branch and PR with
[planner keyed access](planner-keyed-access.md); this changes packaging, not the
later-batch demand and measurement gates.

## Goal

Today a relation can keep transaction-time history (`TxTime`) or have search
indexes, never both. This spec lets a system-versioned (tt-only) relation carry
B-tree, full-text, vector and LSH indexes that answer for the **current
belief**, and defines what a search **as of** an earlier transaction time does.

## Verified current state

| Claim | Where | Consequence |
|---|---|---|
| `::index`, `::fts`, `::hnsw`, `::lsh create` and `::set_triggers` reject `TxTime` relations with `eval::txtime_unsupported_op` | `runtime/relation.rs:984-992`; call sites `:1006`, `:1226`, `:1357`, `:1571`, `:1891`; confirmed on the 0.19.2 wheel | The gate this spec lifts for four of the five |
| Buffered writes to a `TxTime` relation are stamped and written at commit with one engine-assigned tt | `runtime/transact.rs:60` (`pending_tt_writes`), `:322`, `:406-479`; write ops `query/stored.rs:1420`, `:1586`, `:1727`, `:1821`, `:1930`; `runtime/db.rs:1421` | One place to maintain indexes |
| `::evict` is the exception: it deletes every base version and allocates its tt at **statement** time, then commits through the section via `tt_hwm_dirty` | `runtime/db.rs:3076-3085`, `:3112-3126` | The section cannot see what evict removed (D7) |
| `PendingTtWrite.handle` is captured at statement time | `runtime/transact.rs:110-120`, used at `:458-476` | Its index set can be stale by commit (D3) |
| The tt commit runs under a per-`Db` mutex: allocate tt, stamp rows, persist the high-water mark, commit. The mutex wait has no timeout | `commit_tx_with_tt`, `runtime/transact.rs:380-398` | Index work inside it serializes tt committers (residual risks); deadline handling needed (D8) |
| On RocksDB a write transaction's plain reads (`get`, `multi_get`, iterators) see the latest committed state plus its own writes; `GetForUpdate` and `Put` validate against the begin snapshot when they lock the key | `storage/rocks.rs:283`, `:411-416`; `cozorocks/bridge/tx.h:20-34`, `:87-103`; HNSW neighbour reads use `for_update` (`runtime/hnsw.rs:1253`) | Index-row writes **and** locking reads can raise a snapshot conflict; clearing the snapshot covers both, and only affects later operations |
| `put_externally_serialized` clears the begin snapshot for a mutex-serialized key | `storage/rocks.rs:435-446` | Precedent, sound only when the mutex is the sole writer of the key |
| The shared relation guard spans commit only in single-script execution. Multi-transactions never take it; governed transactions hold it per statement; top-level system ops (index DDL, `::reindex`) drop it before their commit | `runtime/db.rs:2083-2121`; `:559-570`; `:899-910` vs `:868-872`; `:3203-3207`, `:3272-3283` vs `:3507-3508` | Relation locks do not order index DDL against tt commits (D5) |
| System ops run only as top-level scripts, never inside a multi-statement or governed transaction | `parse/mod.rs:231-241` (`get_single_program` rejects `Sys`) | DDL, `::reindex`, `::evict` each own their transaction |
| SQLite and `mem` admit one writer at a time | `storage/sqlite.rs:204` (`begin immediate`); `storage/mem.rs:51-54` | Concurrency cases below are RocksDB-only |
| `tt_current_row` resolves a tt-only key's current belief with one skip-scan seek, through the transaction's own writes | `query/stored.rs:1128-1142` | Basis for old/new resolution and base fetches |
| HNSW insertion and neighbour shrinking fetch the entry point's and neighbours' vectors from the **base relation** | `VectorCache::ensure_key`/`ensure_keys`, `runtime/hnsw.rs:162-205`; call sites `:322`, `:548`, `:711`, `:724-725`, `:779` | Base rows of this commit must be written before vector maintenance (D3) |
| Index code derives the base key width from `metadata.keys.len()` in about twenty places | `runtime/hnsw.rs:285`, `:318-320`, `:352`, `:402`, `:421`, `:449`, `:620`, `:641`, `:864`, `:948`, `:1160-1165`, `:1322`, `:1385-1393`, `:1504`, `:1595-1599`; `fts/indexing.rs:791-793`, `:869-871`; `runtime/minhash_lsh.rs:39`, `:83`, `:118`; `runtime/relation.rs:212` | One accessor needed (D2) |
| Nine index build, rebuild and reconcile paths read `scan_all`, which on a `TxTime` relation returns every version | `runtime/relation.rs:1319`, `:1455`, `:1738`, `:1805`, `:2007`, `:2021`; `runtime/db.rs:2485` (unlocked vector build); `runtime/reindex.rs:117`; `runtime/hnsw.rs:1505` (filtered exact scan) | D5. (`relation.rs:227` and `db.rs:2942` are full-history by intent) |
| Both planner call sites pass `validity_query = valid_at.is_some() \|\| has_txtime()`, so every read of a tt relation is treated as time travel, and `choose_index` then admits only an index whose last column is the relation's last key | `query/compile.rs:298-305`, `:493-500`; `runtime/relation.rs:263-274` | The D6 seam is in `compile.rs` |
| The secondary-index join back to the base is a relation join (`StoredWithValidity` prefix skip-scan on tt relations); the index-only plan does no base fetch | `query/compile.rs:319-335`, `:378-388`; `query/ra.rs` | No new point-read helper for B-tree indexes |
| `:as_of` pins atoms whose named relation `has_txtime()`; index atoms (`*rel:idx`) and search atoms are not pinned | `data/program.rs:1020-1061` | Silent current results once indexes exist (D6) |
| Hybrid search is a CozoScript generator emitting ordinary search atoms | `runtime/hybrid.rs:331`, `:886-887`, `:935`, `:953` | Covered by the search-atom rule |
| On a valid-time-only relation, search indexes hold **every version** and a secondary index answers from superseded versions | 0.19.2 probe: FTS returned the superseded text; `*d:byt{text: 'alpha old', k @ 'NOW'}` returned the key although the current text differs | Existing behaviour, unchanged here; why bitemporal relations stay out |
| `:returning`, event callbacks and graph-projection sources reject `TxTime` relations | `query/stored.rs:1183-1186`; `runtime/graph_projection.rs:1462` | Unchanged |

## Decisions

**D1 — Scope: tt-only relations; search indexes first.** Batch 1 admits the
three search kinds (`::fts`, `::hnsw`, `::lsh`); B-tree secondary indexes
(`::index`) follow in batch 3 (owner decision 2). Across the batches: `{k…, tt: TxTime => v…}`
relations accept `::index`, `::fts`, `::hnsw` and `::lsh create`. Bitemporal
relations stay rejected with the existing diagnostic: valid time has no
write-time notion of "current" (a future-dated version becomes current without
a write), and the valid-time behaviour verified above would be inherited.
Triggers, `:returning`, callbacks and graph projections stay rejected. Seam:
`reject_txtime_relation` admits `is_tt_only()` relations at the four index call
sites.

**D2 — An index holds the current belief, keyed by the logical key.** For each
logical key whose latest record is an assertion, the index holds exactly the
entries a plain relation would hold for that row; superseded and retracted
versions have none. Index rows carry the logical key (base key without the tt
column), so an update that leaves indexed fields unchanged leaves the entries
untouched, as on plain relations since 0.19.2.
- Seam: `RelationHandle::index_key_len()` returns `keys.len() - 1` for tt-only
  relations and `keys.len()` otherwise. Index creation (`src_*` columns) and
  every index identity site listed in the table use it. Do **not** replace
  physical tuple-layout offsets: base rows still contain tt, and non-key field
  positions, extractors, vector field indices and stored-row encoding still use
  the full base schema. Audit each use as logical identity or physical layout,
  including `runtime/hnsw/repair.rs` and bulk-build code, not just `hnsw.rs`.
- The tt column may not appear in an index spec (indexed column, extractor
  input, vector field or filter). Error `eval::txtime_index_column`. An index
  keyed on tt would re-key on every write and is the one shape the existing
  time-travel rule in `choose_index` would admit for historical reads.

**D3 — Maintenance runs in the commit section, in three phases.** Assigning
the tt at commit under the lock avoids the conflict SQL Server's begin-time
stamping produces (error 13535); serial maintenance matches Datomic's single
transactor. Inside
`commit_tx_with_tt`, under `tt_commit_lock`:
1. **Resolve.** Resolve each touched relation's current handle and verify its
   immutable id against the captured id; `PendingTtWrite.handle`'s index set is
   not trusted. A dropped/recreated relation with the same name must not receive
   the old transaction's writes. Rename/drop/replace and index DDL participate
   in D5's identity protocol. Clear the RocksDB begin snapshot only after
   acquiring the section (sound for index keys by D5); preserve the validation
   already performed for earlier user-key locks. Do not waive ordinary
   catalog/user-key conflict checks outside this serialized domain.
   Collect every touched logical key: buffered rows plus evicted keys (D7).
   Read `old = tt_current_row(key)` for each, before any write.
2. **Write the base.** Apply evict deletes, then stamp and write buffered rows
   (unchanged code), then the high-water mark.
3. **Maintain.** For each touched key read `new = tt_current_row(key)`, which
   now sees this commit's writes; where `old ≠ new`, call the existing helpers
   with (old, new): `update_in_index`, `replace_in_fts`,
   `update_in_hnsw_changed`, LSH delete and put. `new = None` calls only the
   delete side. Vector fetches during maintenance see this commit's rows.
   HNSW maintenance is batched per index: seed a removal cache with **all**
   changed/deleted old vectors before removing any, remove stale graph entries,
   then insert changed/new vectors against the new base. Reuse the pattern in
   `hnsw_remove_snapshot_rows` (`runtime/hnsw/repair.rs:410-451`), with logical
   keys. Calling the single-row helper repeatedly after all base writes is
   insufficient: a deleted neighbour is no longer fetchable, and an updated
   neighbour's current vector is not the vector represented by the old graph.
   Unchanged vectors/filter membership retain their graph entries.
   Full-text statistics go through `finish_fts_change`, so they reach the
   committed cache only after the storage commit succeeds.
4. **Commit and notify.** Commit storage while holding the section, then append
   the touched keys to registered builds before releasing it. Failed/aborted
   commits do not publish index statistics or successful-write notifications.
   If a build cannot retain its notification, invalidate that build and prevent
   its publication; never silently lose a key or report a committed write as
   rolled back. The three data phases remain resolve, base write, maintenance.

Reading old and new from storage, rather than from the buffer, makes repeated
writes to one key, retraction and later reassertion, `:reconcile` and evict
uniform. Preserve the existing `eval::txtime_assert_retract_conflict`: asserting
and retracting one key within the same transaction remains an error; retraction
and reassertion here mean separate committed transactions.
Nothing reaches an index at statement time, so a script still cannot see its
own writes to a `TxTime` relation, through the base or any index.

**D4 — Reads resolve the logical key.** HNSW, FTS and LSH fetch base rows
through `RelationHandle::get_current` (generalised from `tt_current_row`),
both at search time (result rows, vector fetches) and inside `VectorCache`
during maintenance. A tt column in a result is the resolved row's tt. The
B-tree join back is the existing `StoredWithValidity` prefix join and needs no
change once D6 admits the index.

**D5 — Detached builds with a pinned snapshot and atomic publication.**
On RocksDB, every tt-only create/rebuild uses fresh, durably reserved index ids;
its rows are not reachable through a public index name before attachment.
`::reindex` does **not** clear/repopulate the attached index: readers and
committers continue using it while its replacement builds. Preserve the stored
manifests, including LSH permutations, without reconstructing configuration.

1. **Register and pin.** Register a unique build token with the immutable base
   relation id, requested index name/kind and expected old index ids (for a
   rebuild). Under `tt_commit_lock`, pin a read-only storage snapshot and record
   the clock watermark W consistently with that snapshot. Register before
   allowing a subsequent tt commit. A scan at `MAX_VALIDITY_TS` is valid only
   **inside that pinned snapshot**; do not record W and then scan a newer write
   transaction at end-of-time. All base reads, including HNSW vector fetches,
   use this same snapshot throughout backfill. Its lifetime also protects the
   required old versions from concurrent history GC/eviction.
2. **Build detached.** Release the section and bulk-build private index rows
   from the snapshot's resolved current beliefs, retaining the build's old rows
   needed to remove stale entries during catch-up. Committed writes append
   touched logical keys to its registration. No corpus-duration relation write
   lock or attached-index/catalog key lock may block ordinary tt commits.
3. **Catch up and attach.** Under the section, verify the build token, immutable
   base id, current ACL/schema, index name and expected old ids. Resolve the
   current beliefs of its touched keys. Remove their snapshot entries (using
   the saved old rows/vectors), then derive their current entries; handle keys
   inserted, corrected, retracted, reasserted or evicted during backfill.
   Atomically attach the new ids/manifests and commit. Old readers keep a
   coherent old index/base snapshot; new readers see the attached generation.
   Deregister only after successful publication or a terminal failure. No
   complete-base diff or scan is allowed inside this section.

A drop invalidates the corresponding build token under the section, detaches
any attached generation and commits. Dropping/renaming/replacing the base also
invalidates its builds, so a builder cannot resurrect an index or attach it to
a newly created relation of the same name. Publication merges into the current
catalog handle; it must not overwrite unrelated index changes from backfill.
Use one RAII-owned registration through every error/cancellation path. Cleanup
of detached/retired data is explicit, scoped to the reserved ids and safe for
existing reader snapshots. A crash before attachment may leave unreachable
private rows, never an attached partial index; reopening must remain usable.
Do not run non-transactional destructive range cleanup before catalog commit.

**Lock order and backend boundary.** Existing transactions may hold storage or
relation locks before reaching `tt_commit_lock`. Establish one audited order for
registration, DDL, eviction and commit; never wait for a lock held by a waiting
committer while owning `tt_commit_lock`. In particular, do not acquire SQLite's
single-writer admission (or mem's write guard) under the section. Its holder
might be waiting to enter the section. RocksDB publication must likewise avoid
catalog locks held by a transaction waiting for the section. Keep catalog DDL
serialization distinct from the corpus-duration build and use cancellation-aware
waits. The RocksDB unlocked-build and concurrent-writer acceptance gates below
apply to RocksDB. SQLite and mem retain their existing transaction admission
limits; this feature does not promise concurrent writers on those backends.

The registration tracks keys, not a sequence of whole-row events; repeated keys
may be deduplicated because catch-up resolves final belief. Bound registration
memory and fail the **build** explicitly on overflow, leaving ordinary commits
and the old attached index usable. Measure catch-up and snapshot-retention costs;
publication can still be long if many distinct keys changed during backfill.
This protocol makes the section the sole authority for attached-index mutations;
an unlocked builder writes only its own detached ids.

**D6 — No silent history.** A historical tt never reaches an index.
- Planner (`query/compile.rs:298`, `:493`): for a tt-only relation, call
  `choose_index` with `validity_query = false` unless the atom carries a tt
  selector (explicit, or from `:as_of`); with one, skip index selection and
  read the base. The index-only plan is not used when the atom binds the tt
  column. No tt selector is passed to the index relation.
- That historical base read is bounded like the vector fallback: past
  `FILTERED_EXACT_MAX_ROWS` scanned records it raises
  `query::as_of_scan_limit`, never a partial result, and `::explain` shows it
  as a history scan. Datomic answers such questions from covering history
  indexes and SQL Server recommends indexing its history table; mnestic does
  neither in batch 1 (rejected alternatives).
- `:as_of` (`data/program.rs:1020-1061`): an index atom or fixed-rule input
  naming an index of a tt relation, and any search atom on a tt relation,
  raises `search::as_of_unsupported` and counts as a tt reference for the typo
  guard. An explicit tt selector on an index atom keeps its existing error.
  Hybrid queries are covered because they compile to search atoms.
- Batch 2 replaces the vector error with an exact scan of the beliefs held at
  T, bounded by `FILTERED_EXACT_MAX_ROWS` (200,000 scanned rows,
  `runtime/hnsw.rs:1633`); past the cap it is an error, never a partial list.
  Versioned approximate search is not offered: Milvus removed its time-travel
  search in 2.3, and Lance supports it only because its index files are
  immutable per version, which an in-place graph is not.
- Full-text and LSH as-of stay errors unless batch 4 is separately designed,
  approved and implemented. Batch 3 only adds B-tree secondary indexes.

**D7 — History operations.** `::history_gc` removes only superseded records and
never changes an index. `::evict` on a relation with indexes records the keys
at statement time (validation and the audit row stay there) and performs its
base deletes in D3 phase 2, so phase 1 sees the evicted belief and phase 3 the
outcome. Capture the exact physical version keys selected by the statement,
not a logical prefix to rescan at commit; otherwise a newer concurrent version
would be erased. Apply the same deferred-delete protocol to tt-only eviction
even if no index is attached at statement time: an index can attach before the
eviction commits. Check the current handle under the section, and apply the
1,000-key limit there too if an index attached after statement validation.
A version committed by another writer between evict's statement and its commit
is outside evict's view; it survives in the base, as today, and the index follows
the base.
- Because its index work runs inside the commit section, one `::evict` call on
  an indexed relation accepts at most 1,000 keys and raises
  `eval::evict_batch_limit` past that (Datomic advises excising a few thousand
  datoms at a time).
- Eviction is logical until storage compaction: removed rows and index entries
  remain in RocksDB files, the write-ahead log and earlier backups until
  compacted or rotated, and neighbour lists in the vector graph were chosen
  using the evicted vector's distances. Documented, not changed: physical
  erasure is the operator's compaction and backup policy. Current-only indexes
  avoid Datomic's documented failure of excision leaving history-index entries.

**D8 — Failure, deadline and cancellation.** The section waits for
`tt_commit_lock` with a `try_lock` loop that checks the transaction's deadline
and cancellation flag (the pattern of the governed relation-lock wait,
`runtime/db.rs:900-907`), and checks both once more after acquiring the lock
and before allocating the tt. An error in phases 1–3 fails the commit; nothing
is committed and any allocated tt is burned, which the clock permits. Latch
that transaction as failed: retrying `commit` on a legacy handle must not commit
partial base/index writes after a maintenance error or after the pending buffer
was drained. Further statements/commit fail until abort. Work inside the section
is not preempted; the deadline bounds waiting/cooperative checks, not a hard
wall-clock commit duration. Successful storage commit is the durability boundary;
post-commit bookkeeping cannot turn that success into a rollback claim.

## Invariants

1. At every committed state, each index on a tt-only relation represents exactly
   the current asserted beliefs, with the same membership, indexed values and
   search semantics as the equivalent plain relation. HNSW topology and
   approximate rankings need not equal an independently randomized rebuild;
   graph integrity and the existing recall gates still apply.
2. A tt relation's index rows are read for maintenance, written and committed
   only under `tt_commit_lock`, except by an unlocked build (D5) into an
   index that is not yet attached.
3. Plain and valid-time-only relations keep byte-identical storage and cost.
4. tt order equals commit order (unchanged).
5. A query naming a historical tt, explicitly or by `:as_of`, returns the
   belief at that tt or an error; it never returns current rows from an index.

## Acceptance tests

SQLite with `tempfile::tempdir()` and RocksDB; `mem` only for parser cases.
The tests below cover the whole specification. Batch 1 runs the applicable
FTS/HNSW/LSH cases and verifies that B-tree creation and all historical search
remain rejected. B-tree plans and historical base-scan caps belong to batch 3;
historical vector results belong to batch 2. Later-batch tests do not imply
authorization to implement those batches in the combined PR.

- Create each index kind on a populated tt-only relation: superseded and
  retracted versions are absent; results equal an exact scan of current beliefs.
- Correct a row: search finds the new version only; `@ (tt: T)` on the base
  still returns the old one. Retract, then re-assert in a later transaction:
  entries follow. An assert and retract in one transaction retain the existing
  conflict error and commit neither base nor index changes.
- One transaction inserts many new keys into an **empty** HNSW-indexed relation,
  and one updates several keys that are each other's neighbours: no error;
  membership equals a rebuilt index and graph integrity/reachability invariants
  hold. Do not require byte-identical HNSW topology or identical approximate
  top-k from independently randomized builds. Include multi-key deletion and
  filter exclusion after the base rows have changed.
- Same key written twice in one transaction: one entry, last write wins.
- Update only a non-indexed field: FTS postings and HNSW rows untouched.
- A script that writes then searches sees neither base nor index changes.
- RocksDB concurrency: two writers on one key, and on two new keys; a
  multi-transaction and a governed transaction each buffer a write, then
  `::fts create` (and separately index drop, `::reindex`) runs, then they
  commit; `::reindex` concurrent with a commit; `::evict` concurrent with a
  commit to the same key. Invariant 1 after each; no snapshot-conflict errors.
- `::evict`, `::history_gc`, `import_relations`, `:reconcile`, backup and
  restore: invariant 1 after each. When evict removes all versions of a key,
  no B-tree entry, FTS posting, LSH bucket or HNSW row contains it, and BM25
  document count and token total drop by exactly that document. When a newer
  concurrent version survives the captured deletions, the index and statistics
  retain that version instead. 1,001 keys raise the batch limit.
- After N corrections to one key, BM25 document count and token total equal a
  plain relation holding the current beliefs, and ranked results match it.
- Index builds (RocksDB): commits during an unlocked build of each kind are
  reflected after attach (invariant 1); commits are not blocked for the build's
  duration (measured below). Fence registration/snapshot creation, mid-backfill
  and pre-attach. Exercise eviction/history GC during the pinned snapshot,
  drop/rename/recreate of the base, index drop and competing publication,
  failed attach, cancellation and registration overflow. The old index remains
  usable during a detached rebuild and after every failed rebuild. Cover the
  no-index-at-eviction-statement / index-attached-before-commit race.
- Lock-order fixtures: a transaction admitted by SQLite/mem before a build
  attempts the section can still commit; RocksDB metadata-lock holders cannot
  deadlock with a section holder. Use deterministic barriers and bounded waits.
- Index failure injected in each phase: commit fails, base and indexes
  unchanged, later commits on fresh transactions proceed. Reusing the failed
  legacy handle cannot commit or run another statement. Failed builds leave no
  usable partial index name, registration or stale FTS statistics.
- A commit waiting on the section past its deadline returns `eval::timeout`
  without allocating a tt.
- D2: tt column in each index spec kind is rejected.
- D6: current reads use a secondary index (`::explain`); historical reads do
  not; `:as_of` with a search atom, an index atom and an index passed to a
  fixed rule each raise `search::as_of_unsupported`; an as-of base read past
  the cap raises `query::as_of_scan_limit`; batch 2 as-of vector results equal
  the exact top-k of the beliefs at T; the cap raises its error.
- Bitemporal relations, triggers, callbacks, projections: still rejected.

Performance, on the 40,000-row benchmark shape. These are release gates:

| Measure | Gate |
|---|---|
| Plain-relation read and write latency | unchanged |
| Vector and full-text search p50, tt-only against plain | ≤ 1.5× |
| Single-row update p50 and p99, tt-only against plain with the same indexes | ≤ 1.3× |
| Longest time a tt commit waits on another commit's section, 4 concurrent single-row writers, p99 (RocksDB) | ≤ 3× the single-row update p99 |
| Longest commit wait while `::fts create` or `::hnsw create` runs on 40,000 rows (RocksDB) | ≤ 3× the single-row update p99 |
| As-of lookup by secondary key, 5 versions per key, base-scan fallback | reported; input to open question 1 |

## Residual risks

- **Writers serialize on RocksDB.** The section now includes index work (tens
  of milliseconds per row with a vector index), so concurrent commits to any
  `TxTime` relation in the database wait for it. A 1,000-row batch can hold it
  for seconds.
- **Search fetches cost a seek, not a point read**, and lose the batched
  `multi_get` on vector fetches. The 1.5× target exists to catch this.
- **Bulk `import_relations` maintains every index** on a tt relation (it flows
  through the buffer), unlike plain relations. Guidance: drop, import, recreate.
- **As-of vector search is exact, not the approximate result the index gave at
  the time.** It reproduces the belief, not the historical ranking.

## Layering ruling

Engine mechanism only: all changes are in `mnestic`. MindGraph adopts nothing
here. This closes one of the four prerequisites the bitemporality spec lists
for a MindGraph pilot (indexed current reads); triggers and projections,
history-preserving recovery and reversible enablement remain open.

## Rejected alternatives

- **Statement-time maintenance.** The base row's tt does not exist yet, and an
  index would show writes the base relation hides.
- **Maintenance before writing the base rows** (the first draft). Vector
  insertion fetches neighbours from the base, so a commit adding two new keys
  fails and updated neighbours are measured at stale positions (panel, three
  lenses independently).
- **Relying on relation locks to order DDL and commits.** They span commit only
  in single-script execution (table). Rejected for `tt_commit_lock` (D5).
- **Index builds holding `tt_commit_lock` for the whole backfill** (the
  post-panel draft). Correct, but pauses every tt commit for the build; every
  system surveyed builds without blocking writers.
- **Synchronous full-text maintenance only, as Neo4j's default; an
  eventually-consistent mode** (Neo4j `eventually_consistent`) was the model
  for the deferred option in the open questions.
- **Keeping the begin snapshot and failing on conflict.** Safe without D5, but
  turns every overlapping commit into an error; unnecessary once D5 holds.
- **Index rows keyed by the full base key including tt.** Every new version
  would re-key its entries: a full vector remove and insert even when the
  vector did not change.
- **Bitemporal indexes now.** Possible later without a write-time notion of
  "current": index every valid-time version of the current tt belief and filter
  valid time at read, as XTDB's recency partitions do. Out of scope; the
  exclusion is a scope choice, not an impossibility.
- **Versioned index entries** (bitemporality spec §8 sketch). Gives indexed
  as-of reads for secondary indexes, at the cost of history in every index,
  coupling to `::history_gc` and `::evict`, and no answer for vector graphs.
  Revisit for B-tree indexes alone if an as-of lookup by secondary key is shown
  to be too slow as a base scan.
- **A per-key "current head" row.** Point-read resolution and row-level
  conflicts, allowing index work outside the section. New storage on every
  indexed relation; revisit if the serialization or the 1.5× target fails.
- **A versioned vector graph.** Edge history multiplies with every neighbour
  rewrite; no consumer.
- **Per-atom tt selectors on search atoms.** `:as_of` covers reproduction.

## Batches (review units)

1. **Current-belief search indexes**: full-text, vector and LSH (D1–D5, D7,
   D8; D6 search-atom and `:as_of` errors). The first implementation unit;
   packaged with keyed access in the combined PR requested November 13.
2. **As-of vector search by bounded exact scan** (D6).
3. **B-tree secondary indexes** (D1, D2 for `::index`; D6 planner change,
   bounded as-of base scan and `query::as_of_scan_limit`). Gate: measure the
   as-of lookup by secondary key first (performance table).
4. **As-of full-text and LSH** — not designed here. Gate: a consumer names an
   as-of text-search question. Candidate: materialise postings for the beliefs
   at T into the transaction's temporary store, bounded, and run the evaluator.

## Resolved review questions

1. *Is clearing the begin snapshot sound?* Yes for keys the transaction locked
   earlier (pessimistic validation happens when a key is locked; the clear
   affects only later operations), and for index rows once D5 makes the
   section their only writer. Without D5 it would let a commit overwrite a
   concurrent `::reindex` silently.
2. *Can `::evict` and a concurrent committer corrupt an index?* They could
   under the draft; D7 moves evict's deletes into the section.
3. *Does any path write a `TxTime` relation outside `stamp_pending_tt_writes`?*
   `::evict` (now in the section) and `::history_gc` (superseded rows only).
   System ops cannot run inside multi-statement transactions.

## Owner decisions (November 13, 2026)

1. **Synchronous maintenance.** Index maintenance runs in the commit section
   (D3) behind the release gates. The deferred mode — commits record touched
   keys against an indexed-up-to tt, catch-up applies them, reads scan the
   unindexed tail exactly, as Datomic, Neo4j's eventually-consistent full-text
   indexes and Lance do — is built only if the section-wait gates fail. It can
   reuse D5's touched-key registration. mnestic has no engine-owned background
   threads, so its catch-up would ride on writers or be explicit.
2. **B-tree secondary indexes move to batch 3.** Batch 1 is the three search
   kinds, which is what combining history and search asked for, and does not
   need the planner change or the as-of base-scan cap.
3. **Release gates as proposed** in the performance table.

## Simplification

Requirement-neutral cuts, applied:
- A runtime warning and metric for slow commit sections (suggested by the
  prior-art pass) were dropped in favour of the release gates; a warning adds
  a threshold to tune and a diagnostic to support, with no consumer yet.
- Physical erasure after `::evict` (compaction of evicted ranges) was not
  added; documented as operator policy (D7).
- Moving tokenisation and vector normalisation out of the section was not
  added to batch 1; it is the first lever if the section gates fail.

B-tree deferral (owner decision 2) applied as a batch change; D6's planner
bullets now land with batch 3.
