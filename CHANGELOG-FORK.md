# mnestic fork changelog

Divergences from upstream CozoDB `481af05` (2024-12-04). See `FORK.md` for
provenance and licensing.

## Unreleased

### New — TxTime relations: schema opt-in + write path (bitemporality step 3)
- **`TxTime` column type** (`docs/specs/bitemporality.md` §4): a relation whose
  last key column is `tt: TxTime` is transaction-time-stamped — tt-only
  (`{k, tt: TxTime => v}`, system-versioned) or bitemporal
  (`{k, v: Validity, tt: TxTime => x}`). The engine stamps every write with the
  commit clock's tt at commit; user-supplied tt values are rejected on every
  path (put specs, `:create`-with-rows headers, imports). **Relations declaring
  `TxTime` are unreadable by pre-fork/upstream builds.**
- **Temporal-axis rule enforced at `:create`** — axes are the trailing key
  columns, vt-then-tt, at most one of each; seven malformed shapes are
  `:create`-time errors whose message contains the copy-pasteable corrected
  declaration (deliberately stricter than `Validity`'s shipped query-time-only
  check).
- **Buffered commit-time stamping**: writes to tt-stamped relations buffer on
  the transaction and are stamped + written inside the commit critical section,
  so the whole transaction shares one tt (one belief event) and tt order ==
  commit order. Consequences, all pinned by tests: same-transaction writes are
  **not visible to later reads in the same script**; a key cannot be both
  asserted and retracted in one transaction (unbreakable resolution tie —
  rejected across statements, including tt-only `:put`+`:rm` mixes); double-puts
  of one (key [, vt]) in one transaction collapse last-write-wins; `::remove` of
  a relation with pending tt writes in the same script is rejected.
- **`:rm` on tt-only relations appends a retraction** at commit-tt (never a
  physical delete); values snapshot the key's latest row at statement time;
  `:rm` of a missing key is a no-op; `:delete` of a missing or believed-deleted
  key fails its existence assertion. **Deviations recorded:** on *bitemporal*
  relations `:rm` is rejected in v1 (removal is a valid-time statement — use
  `:put` with vt `"RETRACT"`; the spec-§6 remap is deferred to step 4);
  `:insert`/`:update`/`:ensure`/`:ensure_not` are rejected pending the as-of
  read path (step 4); `:replace` of an existing TxTime relation is rejected
  (destroy-and-recreate would silently drop history); triggers, secondary and
  search indexes, callbacks, and `:returning` are rejected on TxTime relations
  (indexes: step 5). A put-trigger on a *plain* relation may write into a
  TxTime relation (buffers and stamps normally; pinned).
- **Imports**: `import_relations` stamps **one tt per batch** (one belief
  event; spec §13.3), rejects tt columns in payload headers and delete-imports;
  `restore_backup` preserves tt bytes verbatim and **re-seeds the commit
  clock** from the backup's persisted mark; `import_from_backup` is rejected
  for TxTime relations (its rows would carry a foreign clock's tts — restore
  the full store or re-ingest). Export→import therefore does not round-trip
  TxTime relations; use backup/restore.
- **Interim read behavior**: bare scans work (all versions, newest tt first —
  the tt column decodes as a Validity value; use `:order` for chronological
  output); any `@` as-of read on a TxTime relation fails with a distinct
  "lands in step 4" error, so vt semantics never silently apply to the tt axis.
- Closes step 2's owed items: the restore re-seed and the HWM+rows same-tx
  atomicity test (an aborted transaction leaves neither rows nor a persisted
  mark). 24 txtime/tt_clock tests; hardened against a three-lens adversarial
  review (cross-statement conflict detection, `::remove`-with-pending-writes
  orphan bytes, believed-deleted `:delete` consistency were review catches).

### New — transaction-time commit clock (bitemporality step 2; internal, no user surface)
- The engine-assigned transaction-time (tt) allocator for the bitemporality
  work (`docs/specs/bitemporality.md` §5, decisions §13.10): a wall-clock-floored
  strictly monotonic commit counter `tt = max(now_µs, last_tt + 1)` held as an
  `AtomicI64` high-water mark on `Db` (`runtime/tt_clock.rs`), seeded at open
  from `max(persisted mark, wall clock)` and persisted as a system key
  (`[Null, "TT_HWM"]` under `RelationId::SYSTEM`, the `STORAGE_VERSION` idiom)
  **inside the committing transaction** — no crash window between advancing and
  persisting. `SessionTx::commit_tx_with_tt` allocates under a per-`Db`
  critical section so tt order == commit order == visibility order; values
  advanced by transactions that abort are burned, preserving monotonicity.
  Hardened by adversarial review: the HWM put goes through a new
  `StoreTx::put_externally_serialized` (RocksDB override clears the
  pessimistic transaction's begin-snapshot for this one key — otherwise any
  two temporally-overlapping tt commits would make the later one abort with
  `Resource busy`, the 0.8.4 `avgdl` hot-key failure mode; default impl is a
  plain put for sqlite/mem, whose storage-level write locks preclude
  overlapping write transactions). Seeding is monotone (`fetch_max`, so
  re-`initialize` and the step-3 restore re-seed can never regress the
  authority); a malformed persisted mark refuses to open (loud, mirroring the
  version-mismatch bail) instead of silently degrading to wall-clock seeding;
  the commit section recovers from mutex poisoning (sound: a mid-section
  panic only burns a never-committed value); the wall-clock read is
  cfg-guarded for wasm32 like `current_validity`. Nothing in the write path
  calls the clock yet — step 3 (schema opt-in + buffered stamping) routes
  tt-stamped commits through it, and **owes**: the restore/import re-seed
  (`max(persisted, max restored tt, wall clock)` after `restore_backup`) and
  the HWM+data-rows same-tx atomicity test. Pinned by tests: same-µs strict
  monotonicity, backward-clock step, concurrent per-caller monotonicity +
  global uniqueness, restart re-seeding from the persisted mark (sqlite),
  corrupt-mark open refusal, abort-doesn't-persist, mem-backend operation,
  and overlapping-commit non-conflict on RocksDB.

## 0.9.0 — 2026-06-28

Adds the **read-only Cypher query surface** (the headline feature) and bundles
the corrupt-database tooling that was banked as 0.8.6 but never published
(0.8.5 → 0.9.0 ships both; there is no separate 0.8.6 crates.io artifact).

### New — read-only Cypher query surface (alpha, feature `cypher`, off by default)
- Translate a subset of **openCypher** to CozoScript so the engine can be
  evaluated and adopted without first learning Datalog. Datalog stays the native,
  full-power language; this is a **read-only** on-ramp (no write clauses). New API:
  `DbInstance::run_cypher` / `cypher_to_script` (+ Python `run_cypher`), driven by
  a caller-supplied `CypherGraphSchema` / `NodeMap` / `EdgeMap` mapping the
  property-graph model onto stored relations — supporting both the
  relation-per-label and the shared-relation-with-discriminator conventions
  (the latter matches MindGraph's reified `node`/`edge` model).
- v1 subset: `MATCH` (fixed-length, directed, labels, types, inline property
  maps), `WHERE` (comparisons, `AND`/`OR`/`NOT`, `IN`, `IS NULL`, `STARTS`/`ENDS
  WITH`, `CONTAINS`), `RETURN` (`DISTINCT`, aliases, aggregates
  `count`/`sum`/`avg`/`min`/`max`/`collect`), `ORDER BY` / `SKIP` / `LIMIT`.
  Literals pass as params and every interpolated identifier is validated. True
  bag semantics (`count(*)`/`LIMIT` match openCypher), **null-aware `WHERE`**
  (a null operand drops the row instead of aborting the query), and per-`MATCH`
  edge-isomorphism. Module `cozo-core/src/cypher/`; design + scope in
  `docs/specs/cypher-read.md`; hardened against a multi-agent adversarial review.
  Off by default — enable the
  `cypher` feature (the published PyPI wheel ships without it for now; build with
  `--features cypher` to get the Python `run_cypher`). Deferred with explicit
  errors: undirected relationships, the
  schema `filter` field, variable-length paths, `OPTIONAL MATCH`, `WITH`. Known
  divergence: `sum` over an integer column returns a float (engine accumulator is
  f64).

### Fixed — `cozo-bin` token-table bearer auth ignored on query-string URLs
- The server's `authorize` evaluated `Authorization: Bearer` against the token
  table only in the `uri().query() == None` branch, so any endpoint that takes
  query params (`/transact?write=true`, `/rules/name?arity=2`) rejected valid
  bearer tokens. Bearer auth is now evaluated independently of the query string
  (a `?auth=<secret>` credential still takes precedence, then bearer falls
  through regardless of other query params). CORS also now allows the
  `Authorization` header so browser bearer clients pass preflight.

### Fixed — `cozo-bin` had no default feature, failed to build bare
- `cargo build -p cozo-bin` (no features) failed to resolve `rayon` — cozo-core
  uses rayon unconditionally on non-wasm, but it is pulled in only via the
  `graph-algo` feature, which `cozo-bin` did not enable by default — and any
  binary it did produce had no storage backend. `cozo-bin` now defaults to the
  `compact` combo (sqlite + requests + graph-algo): a runnable server/REPL out
  of the box. Opt into rocksdb explicitly.

### Fixed — `::index create` no longer panics on corrupt tuples
- Index population extracted columns by position with no bounds check: one
  truncated stored tuple (e.g. from an interrupted write) panicked the whole
  build — and since applications may (re)create indexes while initializing a
  database, one bad row made the database unopenable (observed in production
  2026-06-12). Corrupt tuples are now skipped with a loud error naming the
  relation, the index, and the arity mismatch, plus a build-level summary
  ("N corrupt tuple(s) skipped — the base relation needs repair").

### New — `::repair_corrupt <relation>`: surgically delete truncated tuples
- Tuples whose stored arity is shorter than the schema (truncated value bytes
  from interrupted writes) are deleted by their intact store keys; returns the
  removed count. Gives applications a surgical alternative to dropping a
  database that fails integrity checks — the motivating incident (2026-06-12)
  saw an application-level "repair" delete a production database over 15 bad
  rows. Pinned by `tests/fork_regressions.rs`.

## 0.8.5 — 2026-06-12

### Changed — flat in-RAM parallel HNSW bulk build (`::hnsw create`)
- `::hnsw create` now constructs the graph in flat, integer-indexed memory
  (one contiguous vector slab + per-node adjacency arrays, the
  hnswlib/pgvector/Lucene layout) instead of the temp-store BTreeMap of
  encoded tuples, and inserts in parallel with per-node locks. A profile of
  the old build showed >50% of CPU in tuple encode/decode, `CompoundKey`
  hashing and allocator traffic — all gone. The finished graph is serialised
  into the index relation's existing tuple format in one pass: the on-disk
  layout, the search path, incremental maintenance (`hnsw_put`/`hnsw_remove`),
  and the non-blocking Phase A–D build/reconcile orchestration are unchanged.
- `MNESTIC_INDEX_BUILD_THREADS` controls worker count (unset/0 = all cores;
  1 = serial insertion in scan order, the old behaviour). Parallel insertion
  makes the built graph non-deterministic across runs (link sets vary with
  interleaving, as in hnswlib/pgvector); recall agreement is guarded by
  `tests/hnsw_build.rs::parallel_build_recall_agreement`.
- Two parallel-only divergences from the serial insertion algorithm, both
  required for lock-safety: neighbour-overflow shrinking never extends
  candidates (would need two node locks at once), and a node's own link-list
  write merges with concurrently-arrived backlinks instead of replacing them
  (replacement severed edges and, on chain-shaped data, broke connectivity).

### Changed — plain-snapshot read path for read-only scripts (RocksDB)
- Read-only scripts no longer open a pessimistic transaction. They read the
  base DB through a plain snapshot (`SnapshotReadBridge` in `mnestic-rocks`
  0.1.9): the same consistent view as before — the old read path also pinned
  one snapshot — but with no lock-manager bookkeeping and no transaction
  write-batch overlay consulted on every read. This is the standard MVCC
  read pattern (TiKV, CockroachDB). Writing scripts keep the pessimistic
  transaction unchanged. Isolation semantics are pinned by
  `tests/snapshot_reads.rs` (uncommitted writes invisible, read transactions
  keep their snapshot view across concurrent commits).
- Measured (RocksDB, immutable scripts, 50k rows): keyed point read p50
  **28.5 → 23.9 µs** (−16%), p99 −19%; 20-row prefix scan p50 **46.0 →
  41.5 µs**. Retrieval-scale queries (40–150 ms) on a block-cache-resident
  corpus showed no measurable change, as expected — per-script transaction
  overhead is µs-scale. Parse/compile (~20 µs) now dominates point reads,
  which is the stored-queries → plan-cache item's territory.
- A write attempted through a read-only transaction now errors explicitly
  instead of silently succeeding inside a transaction that had no business
  existing. (No in-tree path did this; the error guards against future ones.)

### Changed — batched HNSW neighbour reads on the search path
- `hnsw_search_level` fetches all unvisited neighbours' vectors per expansion
  step through one `StoreTx::multi_get` — a true RocksDB `MultiGet` (shared
  bloom-filter probes, batched block reads) on the snapshot read path —
  instead of one serial point get per neighbour (`VectorCache::ensure_key`).
  Other backends fall back to a serial loop via the trait default. Neutral on
  a block-cache-resident corpus (fused p50 unchanged); the win case is
  cold-cache / larger-than-RAM data, where serial point gets pay one block
  read each.

### Changed — FTS bulk build (`::fts create`)
- The populate loop no longer runs a del pass per row: the index relation is
  freshly created and empty, so the old code tokenised every document a
  second time to delete postings that could not exist.
- Tokenisation + posting-row encoding now fan out across worker threads
  (same `MNESTIC_INDEX_BUILD_THREADS` control); the row format is unchanged.
- Corpus doc-stats (`avgdl`) are counted exactly during the build and seeded
  directly, replacing the post-build full index scan.

### Fixed — `::describe` was unreachable upstream; now parses and is read-only-guarded
- Upstream defines `describe_relation_op` in the grammar and implements
  `SysOp::DescribeRelation`, but never wired the rule into the `sys_script`
  alternations — `::describe rel 'note'` always failed to parse. The op is
  now reachable (top-level and inside imperative blocks).
- `::describe` writes relation metadata; it was also the only mutating sys op
  without a read-only guard. It now rejects `ScriptMutability::Immutable`
  with a clear error, like its siblings, instead of falling through to the
  storage layer. Pinned by `tests/fork_regressions.rs`.

### Tests — bulk-build coverage from the post-ship bug-hunt audit
- The audit (three independent reviews of the flat HNSW build, the snapshot
  read path/FFI bridge, and the FTS bulk build) found no correctness bug; it
  found untested live paths, now covered: HNSW flat build over a
  **list-of-vectors column** (`[<F32; N>]`, the `sub_idx` branch), an
  **F64 + Cosine** flat-build recall guard, and bulk-vs-incremental FTS
  doc-stats score equality on a **multi-column-PK** relation.
- Documented in the `hnsw_build.rs` module header: the flat build omits
  pruned edges where the incremental path writes tombstoned rows —
  indistinguishable to search (`include_deleted=false`); the degree counter
  it feeds was already approximate upstream.

## 0.8.4 — 2026-06-10

Fifth fork release: a defect fix for 0.8.3's concurrent-write regression plus
the per-leg fusion detail that powers MindGraph's "why retrieved" surface.

### New — per-leg retrieval detail: `detailed` on RRF and `HybridSearch`
- `ReciprocalRankFusion(..., detailed: true)` switches the output from
  `[item, fused_score]` to the long-format
  `[item, fused_score, list_id, leg_rank, leg_score]` — one row per
  *(item, contributing list)*. `leg_rank` is the 1-based within-list rank the
  fusion actually used (after best-score dedup); `leg_score` is that
  deduplicated raw score; lists an item did not appear in contribute no row.
  `detailed` must be a constant boolean (output arity depends on it). This is
  the mechanism behind a consumer's "why was this retrieved" surface: the rows
  reconstruct the fused score exactly (`Σ 1/(k + leg_rank)`).
- `HybridSearch::detailed: bool` plumbs it through the one-call helper. Without
  MMR the head is `[id, score, list_id, leg_rank, leg_score]` and the row limit
  widens to `limit × leg-count` so the top `limit` items are always fully
  covered; with MMR the per-leg detail is joined onto MMR's selection and the
  head is `[id, rank, score, list_id, leg_rank, leg_score]`.
- Python binding: pass `detailed: True` in the `hybrid_search` dict.

### Fixed — concurrency regression in the 0.8.3 doc-stats counter
- 0.8.3's durable `avgdl` counter was one shared storage key per FTS index,
  read (without a lock) and rewritten inside **every** document transaction.
  Under RocksDB pessimistic transactions this made all concurrent writers to
  an FTS-indexed relation conflict on a single row lock (held until commit),
  and the unlocked read-modify-write also lost updates, silently drifting the
  counter. Concurrent ingest produced `Resource busy`-class storage errors.
- The counter is now **process-cached and scan-seeded**: one deduplicated
  full scan per index per process (the path 0.8.3 already used for legacy
  indices), maintained incrementally in memory on every put/delete, with no
  shared storage key in the hot path. Per-query `avgdl` stays O(1); the
  pinned behaviours (deletes net correctly, scores identical across reopen,
  BM25 denominator) are unchanged and covered by `tests/fts_avgdl.rs`.
  Index rebuilds reseed the cache and delete any legacy 0.8.3 counter key.
- Deltas from rolled-back transactions are not undone; the drift is
  negligible for a smoothing denominator and clears on restart or rebuild.

### Fixed
- `log::error` import in `jlines.rs` is now gated behind the `requests`
  feature (was an unused-import warning under minimal feature sets).

## 0.8.3 — 2026-05-31

Fourth fork release. Two agentic-memory wedge features land together and are
**validated end-to-end** on the `mnestic-benchmarks` hybrid suite (2026-05-31,
SQLite-backed wheel, vs SQLite/DuckDB/LanceDB/Kuzu): **native 3-way fused recall**
(Bet 1a) and **BM25-correct FTS with O(1) `avgdl`** (Bet 1b). All 169 inherited
lib tests + feature suites pass; `cargo clippy -p mnestic -- -D warnings` is clean.

> **Heads-up — the FTS default scorer changed.** The default `::fts` score kind
> moves from `tf_idf` to Okapi `bm25` (a behaviour change). `tf` and `tf_idf`
> stay selectable for byte-identical upstream scoring.

**Measured (2026-05-31):**
- **BM25 + O(1) `avgdl`:** fused recall **0.75 → 0.954** (parity with DuckDB
  0.957 / SQLite 1.0); decomposed-path p50 **927 → 175 ms** and the cold p99 tail
  **2,900 → 258 ms**. (The tail was the per-query `avgdl` scan, *not* cold HNSW as
  first assumed — the vector leg even got faster cold→warm, 117 → 23 ms, unchanged.)
- **Native 3-way:** the one fused call runs vector+FTS+graph at **41.55 ms p50**
  (recall 0.873) — **~4× faster** than the 175 ms hand-decomposed path, fusing a
  signal (graph) no other engine here has (LanceDB native is 2-way only: recall
  0.456). The one-call advantage reappeared exactly as predicted once the `avgdl`
  fix removed the FTS scan that had masked it.

### New — native 3-way fused recall: typed `GraphLeg` on `HybridSearch`
- `HybridSearch::graph_legs: Vec<GraphLeg>` (new `GraphLeg` type, re-exported from
  the crate root). Each leg expands from a set of `seeds` over a stored edge
  relation up to `max_hops`, scores every reached node by its **minimum hop
  distance** (closer ⇒ higher rank), and contributes that ranked list to the
  *same* Reciprocal Rank Fusion as the vector/keyword legs — one call, one
  transaction, no hand-written recursion.
- Why a new type rather than the existing `extra_lists`: an `extra_lists` entry is
  a *single* spliced rule body, which cannot express the recursive shortest-path
  rule that bounded-hop proximity needs. `GraphLeg` generates that rule — a seed
  relation, a hop-1 base rule, and a `min(dist)` recursive rule gated at
  `max_hops` — for you. Supports `undirected` traversal (also follows edges in
  reverse) and multiple seeds (unioned).
- **Injection-safe.** Seed values are passed as query **params** (`$hg{i}_seed{j}`),
  never string-interpolated; the label, edge relation, and column names are
  validated as bare identifiers, and empty seeds / `max_hops == 0` are rejected.
  The generated script remains inspectable via `hybrid_search_script`.
- `runtime/hybrid.rs`; guarded by `tests/hybrid_graph_leg.rs` (recall a neighbour
  the fixed legs miss, closer-outranks-farther via `min(dist)`, hop bound,
  undirected reverse edges, multi-seed union, script-is-recursive-and-parametrised,
  input validation). Backward compatible: an empty `graph_legs` generates the exact
  prior script.

### Added — read-path latency baseline (groundwork for a future plan/stored-query cache)
- `benches/read_path.rs` (criterion) times `parse_only` (parse + compile-to-AST)
  vs `full_run` (end-to-end `run_script`) for a point read and a multi-rule
  retrieval query on SQLite, to size the parse/compile fraction a compiled-plan
  cache could eliminate before any cache is built (the fork's baseline-first
  rule). **Finding:** parse/compile is a roughly *fixed* ~20–85 µs cost — ≈39% of
  a 55.7 µs point read but only ≈1.1% of a 7.68 ms multi-rule retrieval query — so
  a plan cache helps cheap point reads but is noise for the retrieval workload,
  where execution (and, on RocksDB, the pessimistic txn) dominates. That makes
  **Bet 1a (one fused call instead of three `run_script`s) the read-path latency
  fix that matters**, not the plan cache. A real plan cache must also clear two
  structural blockers: parse-time param inlining, and the lack of a reusable-plan
  execute entry point.

### FTS — Okapi BM25 scoring + summed disjunction + O(1) `avgdl`

The recall lever the hybrid-retrieval benchmark localized the entire fused-recall
gap to (FTS recall-agreement 0.72 vs vector 0.99 / graph 1.00).

- **New default score kind `bm25`** for `::fts`/`~rel:idx{… | score_kind: 'bm25'}`.
  Implements `idf · tf·(k1+1) / (tf + k1·(1 − b + b·|D|/avgdl))`: term-frequency
  **saturation** (`k1`, default 1.2) and **document-length normalization** (`b`,
  default 0.75, range `[0,1]`), both tunable as query params. Replaces upstream's
  raw `tf · idf`, which had neither — long documents and high raw term counts
  dominated unfairly. Two upstream defects fixed:
  - *No length normalization.* The per-document token length was **already stored**
    on every posting (`vals[3]`) but **discarded** at search time; it is now read
    (`LiteralStats::doc_len`) and used. Average document length (`avgdl`) is an
    **O(1) read** of a durable per-index doc-stats counter (see below).
  - *Disjunction did not sum.* An `a OR b` query took the **max** of per-term scores,
    so a document matching both terms could tie one matching a single term — forcing
    callers into app-side per-term aggregation with wide over-fetch. Under `bm25`,
    `OR` now **sums** per-term contributions (a document matching more query terms
    ranks higher). `tf`/`tf_idf` keep upstream's max-combine.
- **Backward compatible:** `score_kind: 'tf_idf'` and `'tf'` are unchanged
  (byte-identical scoring and the original `OR`=max semantics). Only the *default*
  moved to `bm25`.
- Guarded by `tests/bm25.rs` (sqlite backend, stored path): OR-sum beats
  repeated-single-term, length normalization favors the shorter doc, and `b: 0.0`
  provably disables length normalization (proving `b` is wired through).
- **`avgdl` is now O(1) (durable doc-stats counter).** The bench validated BM25's
  recall (0.75 → 0.96) but exposed a **~10× FTS latency regression** (71 → 755 ms
  p50): the initial `avgdl` was a full deduplicated index scan (O(#docs),
  ~680 ms/query at 40k chunks) recomputed on *every* query, because the cache lived
  on the per-operator `FtsCache`. Fixed: each FTS index maintains a durable
  `(total_tokens, n_docs)` counter at a reserved `[Bot]` key (sorts above all
  `[term, …doc_key]` postings, so it is invisible to term scans). `put_fts_index_item`
  adds a document's tokens, `del_fts_index_item` subtracts them (guarded by a posting
  existence probe), and `create_fts_index` publishes the authoritative count via one
  final scan — so `avgdl` is a single keyed `get`. A legacy index that predates the
  counter migrates itself on its first write (seed-by-scan) and, until then, reads
  fall back to a `Db`-scoped cross-query cache (one scan per process, not per query —
  correct because an un-migrated index is immutable). Identical scores to the prior
  scan (the counter value equals the scan); guarded by `tests/fts_avgdl.rs`
  (delete-equals-fresh-build, survives reopen, `avgdl` feeds the BM25 denominator).
  Well-behaved workloads (insert, delete, del-then-put update) are exact; an FTS-only
  relation with no secondary index can drift on in-place update, mirroring upstream's
  existing posting leak there (an index rebuild resets it). **Bench-confirmed:** the
  FTS leg returned to ~71 ms and decomposed p99 fell 2,900 → 258 ms with recall held
  at 0.954.

### Python
- `cozo-lib-python`'s `hybrid_search` now accepts a `graph_legs` list (mapped to
  `Vec<GraphLeg>`), so the embedded `mnestic` wheel can drive the native 3-way
  fused recall from Python. `cozo-lib-python` stays workspace-excluded (built only
  when the wheel is built).

## 0.8.2 — 2026-05-30

Third fork release. Makes HNSW index builds **non-blocking for readers**: an
index build no longer freezes all reads on the base relation for the (often
multi-minute) duration of the build. All 169 inherited lib tests + the
integration/feature suites pass; `cargo clippy -p mnestic -- -D warnings` is clean.

### Performance — non-blocking HNSW index builds (readers no longer stall)
- Building/rebuilding an HNSW index (`::hnsw create`) used to hold the base
  relation's **exclusive write lock** for the *entire* build, so every concurrent
  read (which takes the same lock shared) blocked until the build finished — in
  production, **10–20+ minutes** (76 min for a 151K × 1536 index). The stall was
  cozo's per-relation `ShardedLock`, not RocksDB.
- The build is now done **off-lock** on RocksDB: the heavy graph construction runs
  under a read-only snapshot with **no relation lock held**, and the lock is taken
  only briefly to set up the empty index relation and to publish the result.
  Measured: building a **40,000**-vector index takes ~5.6 s, during which
  **90,507** concurrent reads of the same relation completed, the slowest in
  **0.8 ms** (release). Previously those reads would have queued behind the whole
  ~5.6 s build.
- **How it stays correct.** The finished, key-sorted graph is bulk-published into
  the live store via `SstFileWriter` / `IngestExternalFile` (bypassing the
  transaction write-batch), and the index *data* is always ingested before its
  *metadata* is committed — so a reader can never observe an index before its keys
  exist. Base-relation rows that change during the unlocked build are folded in by
  a short reconcile pass (re-scan + diff against the build snapshot, applying the
  same incremental `hnsw_put`/`hnsw_remove` maintenance) under a brief final lock.
  Concurrent builds of the same index are serialised; a lost race cleans up its
  ingested data. Index relation ids are always freshly allocated, so a crash
  mid-publish leaves at worst unreferenced dead keys, never a torn index.
- Non-RocksDB backends (sqlite/mem/…) keep the in-transaction build + per-key
  flush unchanged. New `Storage::ingest_sorted` (default-errors; real impl only on
  RocksDB) carries the SST bulk-load. Guarded by `tests/hnsw_nonblocking_build.rs`
  (build correctness, persistence across reopen, reads-during-build, reconcile of
  concurrent inserts, drop+recreate).

## 0.8.1 — 2026-05-30

Second fork release. Adds the one-call hybrid-retrieval API, a substantial HNSW
index-build speedup, the maintained `mnestic-rocks` bridge fork, and a blocking
clippy CI gate. All 169 inherited lib tests + integration/feature tests pass.

### New — one-call hybrid retrieval (`HybridSearch`)
- `DbInstance::hybrid_search` / `Db::hybrid_search` (and `*_script` to inspect the
  generated CozoScript) run an HNSW + FTS (+ optional graph-traversal) recall,
  fuse it with `ReciprocalRankFusion`, and optionally diversify with
  `MaximalMarginalRelevance` — in one typed call. Previously this was ~7
  hand-assembled Datalog rules. Read-only; the query vector/text are passed as
  script params (never string-interpolated) and every interpolated identifier is
  validated against injection. New module `runtime/hybrid.rs`; tested in
  `tests/hybrid_helper.rs`.

### Performance — HNSW index build ~3× faster
- `::hnsw create` (and backfill) over a large relation was **superlinear**: the
  whole graph was built inside the script's pessimistic transaction, so every
  neighbour read/write round-tripped through RocksDB's `WriteBatchWithIndex`
  overlay (whose cost grows with the index). Measured baseline: **135 s for
  20k × 128** vectors (release).
- The build now constructs the graph in the in-RAM temp store (`is_temp` routing
  via new `idx_put`/`idx_get`/`idx_del`/`idx_exists` helpers) and bulk-migrates
  the finished, key-sorted graph to the persistent store in one pass; and it
  shares one `VectorCache` across the whole build instead of rebuilding it per
  node. Combined **~3.1× faster** (20k × 128: 135 s → 43.6 s; 10k: 51.8 s →
  16.5 s). The built graph is byte-identical; guarded by `tests/hnsw_build.rs`.
- *Investigated and dropped:* batched secondary-index writes (drafted "#4",
  claimed 2–3× ingest). Measured: index writes are ~7 % of ingest even with 4
  indexes, and within a pessimistic txn each `put` only appends to an in-memory
  batch — batching saves <1 %. The real ingest floor is the per-script
  pessimistic transaction, which that change doesn't touch.

### Bridge — `mnestic-rocks`
- Forked the C++/RocksDB bridge crate `cozorocks` → **`mnestic-rocks`** (v0.1.8),
  keeping the importable crate name `cozorocks` (`[lib] name`) so `use cozorocks::`
  and `cozorocks?/feature` references are unchanged. Enables shipping future
  bridge-level work (e.g. out-of-transaction index build + `IngestExternalFile`
  atomic publish) on crates.io.

### Maintenance
- `document-features` 0.2.8 → 0.2.12 to clear a future-incompat warning.
- **Blocking clippy CI gate** (`cargo clippy -- -D warnings`, default features).
  Pervasive/intrinsic inherited lints are allow-listed with rationale in
  `lib.rs` so the gate catches new issues. `cargo fmt --check` is deliberately
  not gated yet (a ~178-file reformat is deferred to its own pass).

### Deferred (designed, not in this release)
- **Lock-free index build** (out-of-transaction build against a snapshot +
  `SstFileWriter`/`IngestExternalFile` atomic publish) — directly fixes "rebuild
  holds the write lock and blocks readers"; touches the transaction lifecycle.
- **Native in-RAM graph** (adjacency as integer arrays, no per-edge
  (de)serialization) for a further ~10–50× build speedup, like hnswlib.

## 0.8.0 — 2026-05-30

First fork release. Lineage: cozo 0.7.6 + 30 unreleased upstream commits (our fork
point). Bumped to **0.8.0** (not 0.7.7) to mark the fork's identity as a maintained,
agentic-memory-focused engine: it ships real planner/parser fixes *and* the first
hybrid-retrieval primitives (RRF, MMR). All changes are pure `cozo-core` (Rust) —
no `cozorocks` bridge changes — so the rocksdb feature still resolves to upstream
`cozorocks 0.1.7`. 169 inherited lib tests + 68 air_routes + all fork/feature tests
pass.

### Fork bootstrap
- Forked from `cozodb/cozo` at `481af05`; full history preserved, upstream
  remote retained as `upstream`, fork point tagged `fork-base`.
- Renamed brand and core package from `cozo` to `mnestic`. Original MPL-2.0
  per-file copyright notices preserved.
- Added `FORK.md` (provenance/attribution) and this changelog.

### Audit of MindGraph's drafted upstream bugs against the fork point

The fork's pre-fork bug drafts were written against the
**crates.io 0.7.6 release**. Our fork point (`481af05`) is the upstream `main`
HEAD, which is **30 commits ahead of the `v0.7.6` tag** (Ziyang merged fixes
after the release but never cut another version). Each draft was therefore
re-verified against HEAD with a backend-correct reproducer:

| # | Bug | Status at fork HEAD | Evidence |
|---|-----|---------------------|----------|
| 3 | `mat_join` joins on wrong symbols (silent 0 rows) | **Already fixed** by the PR #286 "fix-stored-prefix-join" cluster | `tests/matjoin_regression.rs` passes; plan now emits `stored_prefix_join {"**2":"uid"}` instead of `stored_mat_join {"**2":"**0"}` |
| 1 | Equality post-filter → full scan, not prefix lookup | **Was present; now FIXED in the fork** (measured ~28–29× at 5k rows) | fix in `query/reorder.rs`; `tests/fork_regressions.rs::equality_post_filter_uses_prefix_lookup` now active — see "Shipped" below |
| 2 | top-level `:create _foo` silent no-op | **Present, but is a scoping nuance** — `_`-relations are legitimate transaction-scoped temporaries (work across imperative `{...}` blocks). Only the top-level form is a silent trap. Fix is a design call. | `tests/fork_regressions.rs::top_level_create_underscore_relation_is_a_silent_noop` (ignored) |
| 4 | Secondary-index puts: N separate `.put()` calls | **Present** (perf 2-3×) | `cozo-core/src/query/stored.rs` still loops `store_tx.put()` per index |
| 7 | `hnsw` serial neighbor fetches | **Present** (perf 10-20% HNSW) | `cozo-core/src/runtime/hnsw.rs::VectorCache::ensure_key` still single-key `handle.get()` |

Note: the fork being 30 commits ahead of the released 0.7.6 means simply
adopting mnestic already gives MindGraph bug #3's fix (and the other unreleased
fixes) for free.

### Shipped in the fork

#### Phase 1 — agentic-memory features
- **`ReciprocalRankFusion` fixed rule (hybrid retrieval, Bet 1)** —
  `cozo-core/src/fixed_rule/utilities/rrf.rs`, aliased `RRF`. Fuses several ranked
  result lists (vector/HNSW + full-text/FTS + graph traversal) into one ranking
  via `Σ_lists 1/(k + rank_in_list)`. Input is a single relation
  `[list_id, item, score]`; rows are grouped by `list_id`, ranked within each list
  by `score` (`descending` option, default true), and the reciprocal-rank
  contributions are summed per item. Options: `k` (default 60), `descending`.
  Output `[item, fused_score]`, composable in further Datalog. Not feature-gated.
  Rationale: Datalog can already *sum* reciprocal contributions but cannot assign
  a *rank position within a group* — that intra-list ranking is the missing
  primitive. Tests: `cozo-core/tests/rrf.rs` (fusion math, `k` smoothing,
  ascending direction, alias, default-k).
- **`MaximalMarginalRelevance` fixed rule (diversity rerank, Bet 1)** —
  `cozo-core/src/fixed_rule/utilities/mmr.rs`, aliased `MMR`. Re-ranks a candidate
  set to balance relevance against diversity (avoids recalling near-duplicate
  memories). Input `[item, relevance, vector]`; greedily selects
  `argmax(λ·relevance − (1−λ)·max cosine_sim to already-selected)`. Options:
  `lambda` (default 0.5, clamped to [0,1]), `k` (default 0 = all). Output
  `[item, rank]` (selection order). Tests: `cozo-core/tests/mmr.rs`.
- **End-to-end hybrid retrieval test** — `cozo-core/tests/hybrid_retrieval_e2e.rs`
  runs a real HNSW (vector) search + a real FTS (keyword) search over one
  relation, fuses with `ReciprocalRankFusion`, then reranks with
  `MaximalMarginalRelevance` — proving the full hybrid path composes, not just
  synthetic ranked lists.
- Next on Bet 1: a higher-level one-call convenience + a LangChain/LlamaIndex
  adapter once the surface stabilises.
- **Pre-release review hardening** (all guarded by tests): RRF/MMR reject
  non-finite (NaN/inf) scores; MMR rejects inconsistent vector dimensions instead
  of panicking and now uses the true max cosine (not a 0 floor) so anti-correlated
  candidates are rewarded; `ulid_timestamp` rejects malformed/non-canonical ULIDs
  (wrong length, invalid char, leading char > 7) instead of silently truncating.
- **ULID functions (`rand_ulid`, `ulid_timestamp`)** — `data/functions.rs`,
  upstream cozo #296. `rand_ulid()` returns a lexicographically-sortable 26-char
  Crockford-base32 ULID (48-bit ms timestamp + 80-bit randomness); sortable string
  IDs are ideal keys for time-ordered agentic-memory scans (unlike random UUIDv4).
  `ulid_timestamp(s)` extracts the embedded Unix-ms timestamp. Tests:
  `cozo-core/tests/ulid.rs` (format, two hand-derivable decode vectors, recency,
  sortability, distinctness).

#### Phase 0 — fixes
- **#1 equality-pushdown for stored relations** (`query/reorder.rs`). Equality
  post-filters on a stored relation — `*rel[k, ..], k == <ground>` and
  `*rel{k, ..}, k == <ground>` — now compile to a keyed `stored_prefix_join`,
  identical to the binding-first form `k = <ground>, *rel{..}`. Upstream left
  these as a full `load_stored` scan + `eq(..)` post-filter. Implemented as a
  pre-pass (`push_equality_filters_to_bindings`) that converts qualifying
  `eq(var, ground)` predicates into unifications and hoists only those converted
  unifications ahead of the relation that produces the variable; the existing
  well-ordering logic then emits the prefix lookup. Pure optimization — result
  sets are unchanged. (NB: the fix is in `reorder.rs`, not `relation.rs::choose_index`
  as originally guessed — `choose_index` only selects *secondary* indices.)
  - **Correctness boundary (numeric grounds are NOT pushed):** `op_eq` treats
    `Int(n) == Float(n)` as equal across types, but a keyed lookup uses the index's
    strict `Num` ordering where `Int(n) != Float(n)`. Converting a numeric equality
    would silently drop cross-type matches, so the conversion is gated to non-numeric
    ground values (str/uuid/bytes/bool/null); numeric `==` keeps full `op_eq`
    post-filter semantics. User-written unifications are never reordered. Guarded by
    `tests/fork_regressions.rs::numeric_equality_keeps_cross_type_semantics`.
  - **Measured** (criterion, SQLite backend, 5000-row relation, single-row PK
    lookup): positional post-filter **1.746 ms → 61.1 µs (~28.6×)**; brace
    post-filter **1.756 ms → 59.4 µs (~29.5×)**; binding-first unchanged
    (~48–51 µs). The speedup scales with row count (O(N) scan → O(log N) lookup).
  - Tests: `tests/fork_regressions.rs::equality_post_filter_uses_prefix_lookup`
    (now active, was `#[ignore]`); baseline bench `benches/point_lookup.rs`.
- **#281 keyword-prefixed identifiers now parse** (`cozo-core/src/cozoscript.pest`).
  An identifier starting with a keyword literal — `nullable_column`, `trueValue`,
  `falsey` — failed to parse in value positions (`*rel{col: nullable_column}`)
  because `term` tries `literal` before `var` and `null`/`boolean` had no word
  boundary, so `null` greedily matched the `null`-prefix and the parse aborted.
  Added an identifier-boundary lookahead (`~ !("_" | XID_CONTINUE)`) to the `null`
  and `boolean` rules. Closes upstream cozo #281. Tests:
  `tests/fork_regressions.rs::keyword_prefixed_identifiers_parse` (includes a guard
  that real `null`/`true`/`false` literals still parse).
- **#287 `env_logger` moved to a dev-dependency** (`cozo-core/Cargo.toml`). It was
  a hard dependency but is only used by `runtime/tests.rs` (cfg(test)). Closes
  upstream cozo #287; trims downstream build graphs.
- `tests/matjoin_regression.rs` — regression guard pinning the #3 fix.
- `benches/point_lookup.rs` — first stable/CI-runnable criterion bench (the
  upstream pokec/wiki/time_travel benches need nightly `#![feature(test)]` +
  external datasets, so they don't run in CI).

### Next (ordered by value/confidence)
- **#4** batch secondary-index writes into a single `WriteBatch` — perf, needs the cozorocks bridge to expose batch put.
- **#7** `multi_get` for HNSW neighbor fetches — perf, needs bridge support.
- **bit-rot (deferred, low value)**: #307 — the upstream pokec/wiki/time_travel
  benches need nightly `#![feature(test)]` + external datasets, so they can't run
  in CI and can't be compile-verified on a stable toolchain; superseded by the
  criterion harness (`benches/point_lookup.rs`). #298 — newer rayon raises MSRV;
  builds fine on our toolchain (rustc 1.93.1, rayon 1.10/core 1.12.1), only bites
  users below rayon's MSRV, not reproducible for us. Revisit if either becomes
  load-bearing.
- **#2** decide + implement the top-level temp-create behavior (warn vs error vs surface scope).
