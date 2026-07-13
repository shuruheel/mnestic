# Spec — FTS Corpus Statistics: the per-query relation count that "O(1) avgdl" left behind

_Created 2026-07-13. Status: **IMPLEMENTED on `main`, banked for 0.13.0** (decision 18: "bank, do not cut; there is no 0.12.3"). Grounding: every `file:line` verified against HEAD (0.12.2) on 2026-07-13; the measurement is a first-ever `medium`-scale run of `hybrid-recall-bench` (mnestic 0.12.1, RocksDB, real embeddings, 40k vs 400k chunks, same box) — which required fixing the benchmark's workload generator first (`mnestic-benchmarks@dedd0f0`: `medium` and `large` could never be generated). Sibling: `fts_avgdl.rs` / the 0.8.4 avgdl redesign, **whose job this finishes**._

> **Anti-overbuild guardrails.** No new cache: `N` is sourced from the doc-stats cache that already exists on `Db` and is already maintained on every write. No persisted-catalog change (the cache is an in-memory `BTreeMap` behind an `Arc<Mutex<…>>` on `Db`, `runtime/db.rs:172` → cloned into `SessionTx`, `runtime/transact.rs:40-41`), so the rmp_serde append-only rule is not in play. No planner change, no grammar change, no new sysop. Two functions and one probe.

---

## 1. Why / what this buys

BM25's IDF term is `ln(1 + (N − df + 0.5) / (df + 0.5))`. Three statistics feed the score: `df` (documents containing the term), `avgdl` (mean document length), and `N` (the size of the collection).

In 0.8.3/0.8.4 the fork made `avgdl` an **O(1) read** off a process-level doc-stats cache — a shipped headline ("Okapi BM25 FTS (k1/b, OR-sums, **O(1) `avgdl`**)"). `df` was already index-derived. **`N` was not touched.** It kept calling `FtsCache::get_n_for_relation`, which `range_count`s the **entire base relation** — on **every FTS query**, because the `FtsCache` is constructed fresh per query execution (`query/ra.rs:1290`).

That is two defects in one, and they are independent:

**(a) It is wrong.** `df` is counted from the FTS *index* and `avgdl` is averaged over the FTS *index*, but `N` was counted over the base *relation*. A single IDF formula was mixing populations. A row whose extracted text yields no tokens is a row in the relation but **not a document in the collection being searched** — it carries no postings, it can never be a hit, and it must not enlarge `N`. Rows bulk-loaded by `import_relations` (which maintains no FTS index at all) inflate `N` by an arbitrary amount. Measured: **200 empty-body rows beside 3 real documents moved a BM25 score from 0.1335 to 4.065 — a 30× error.** For calibration, the FTS posting leak fixed in 0.12.1 was a *55%* error.

**(b) It is ruinously slow.** O(corpus) per query, and worse than that in practice. First-ever `medium` run of `hybrid-recall-bench` (mnestic 0.12.1, RocksDB, real embeddings, dim 384, same box):

| | small (40k chunks) | medium (400k chunks) | ratio |
|---|---|---|---|
| recall@10 | 0.884 | 0.863 | ~1.0× |
| hybrid p50 | 150 ms | **3,396 ms** | **22.6×** |
| hybrid p95 | 163 ms | 6,475 ms | 39.7× |
| hybrid p99 | 201 ms | **10,358 ms** | **51.5×** |
| ingest rows/s | 62,289 | 54,725 | ~flat |
| disk | 158 MB | 1,548 MB | 9.8× (linear) |

Quality holds and ingest/disk scale linearly; **latency scales super-linearly**. Isolating the legs on the `medium` store: **vector (HNSW, ef=64) = 350 ms, FTS/BM25 = 2,621 ms**, and a full count of the 400k-row base relation = 3,751 ms against a 0.2 ms point lookup. The FTS leg *is* the scan.

The super-linearity (10× data → 22× p50) is worse than the O(corpus) model predicts, because the count is **O(corpus bytes), not O(corpus rows)**: the benchmark's `chunk` relation is `{cid => text, emb: <F32;384>}`, so counting rows drags ~614 MB of float payload through the block cache to compute one integer. This spec does not depend on that mechanism being fully characterised — see §2 row 9 and the residue in §4.

**What it buys.** The scan disappears entirely (the function is deleted, not memoized), `N` becomes correct, and BM25 stops being the reason hybrid retrieval falls over at scale.

---

## 2. Verified baseline (load-bearing facts, re-checked 2026-07-13 against HEAD 0.12.2)

| # | Fact | Where |
|---|---|---|
| 1 | The `FtsCache` is constructed **fresh per query execution**, so any memoization inside it never survives a query. | `query/ra.rs:1290` — `let mut idf_cache = Default::default();` |
| 2 | `get_n_for_relation` full-scans the base relation via `range_count`. | `fts/indexing.rs:35-46` (pre-fix) |
| 3 | `N` was taken over `config.base_handle` (the relation) while `avgdl` was taken over `config.idx_handle` (the index) — different populations, same formula. | `fts/indexing.rs:404-411` (pre-fix) |
| 4 | Both `Bm25` **and** `TfIdf` pay it; `Tf` does not. | `fts/indexing.rs:404-409` (pre-fix) |
| 5 | `range_count` is a real key-by-key iteration on every backend — there is no estimate API. | `storage/mod.rs` (trait) + `storage/rocks.rs:435-450` |
| 6 | The doc-stats pair `(total_tokens, n_docs)` counts only documents with **≥1 posting**, deduplicated on the document key. | `fts/indexing.rs:125-144` (`scan_fts_doc_stats`) |
| 7 | Writes already maintain that pair incrementally (`bump_fts_doc_stats`), and rebuilds republish it (`seed_fts_doc_stats` / `rebuild_fts_doc_stats`). **The maintenance surface this fix needs already exists.** | `fts/indexing.rs:107-123`, `:146-173` |
| 8 | The Db-level cache idiom: `Arc<Mutex<BTreeMap<…>>>` on `Db`, cloned into each `SessionTx`. In-memory only; nothing serializes it. | `runtime/db.rs:172`, `runtime/transact.rs:40-41` |
| 9 | The engine's only FTS entry point is `SessionTx::fts_search`, reached from `query/ra.rs:1320`. The native `HybridSearch` compiles to the same operator, so **one fix covers both** the Datalog `~rel:fts{…}` surface and `hybrid_search()`. | `query/compile.rs:616`, `query/ra.rs:670`, `:1320` |
| 10 | **The del-then-put invariant that `put_fts_index_item` relies on is false.** `query/stored.rs:370-376` skips the posting delete when `extracted == tup` (an identical tuple derives identical postings), but the put still runs — so a value-unchanged `:put` bumped the doc count `+1` with no matching `−1`. | `query/stored.rs:370-376`, `fts/indexing.rs:474-476` (the stale comment asserting the invariant) |

---

## 3. Design

### 3.1 What `N` *is* (the decision)

**`N` is the number of documents in the FTS index carrying at least one posting** — i.e. `n_docs` from the doc-stats pair. Not the base relation's row count.

This is the definition Okapi BM25 requires: `N` is the size of *the collection being searched*, and it must be drawn from the same population as `df`, which is counted from the index. It is also the only definition under which `df ≤ N` holds by construction.

**This changes BM25 scores** on any corpus where the two numbers differ (unindexed rows: empty/whitespace/`Null` extractions, or `import_relations` bulk loads). That is a behaviour change and is flagged as such in the changelog under both **Fixed** and **Changed**. Where the two numbers agree — every corpus in which every row is a document — scores are byte-identical, which is why the existing `bm25.rs` and `fts_avgdl.rs` suites pass unchanged.

*Rejected: keeping `N` = base-relation row count and merely caching it.* It preserves a wrong number, requires a **second** cache with its own invalidation surface (put / rm / replace / drop / `import_relations` / `::reindex` / tombstones), and — decisively — cannot be guarded by a correctness oracle, so it would need a flaky timing test. See §6.

### 3.2 Sourcing

`FtsCache` becomes **stateless** (the struct is retained only so the `fts_search` signature and the `ra.rs` plumbing stay stable — an empty cache is the strongest possible proof that a per-query scan cannot come back). `get_n_for_relation` and its `total_n_cache` are **deleted**.

One accessor, `get_doc_stats_for_index`, returns `(total_tokens, n_docs)` from the Db-level cache, seeding it on first touch. `fts_search` takes **one** cache lock and derives both `N` and `avgdl` from it. `Tf` scoring reads neither and does not seed.

### 3.3 The idempotence probe (fact 10)

`put_fts_index_item` counts the document only if it is **not already in the index**, probed by an `exists` on its first posting key — the mirror image of the probe `del_fts_index_item` already performs.

This closes a **pre-existing** drift that `avgdl` had been hiding: a value-unchanged `:put` bumped `(+count, +1)` with no matching `(−count, −1)`, so the doc count climbed on every no-op write. It was invisible while `avgdl = total / n` was the counter's only consumer — both terms inflate together and the ratio barely moves — but BM25's `N` reads `n` directly, so the drift becomes a score error. Measured: **ten identical re-puts moved a score from 0.1335 to 2.27 (17×).**

Cost: one point-lookup `exists` per indexed document per `:put`. `del_fts_index_item` already pays exactly this.

---

## 4. Non-goals / residue

- **No durable counter.** The 0.8.3 design wrote a shared storage key from every document transaction; every concurrent writer then conflicted on one RocksDB lock. It was reverted, and it stays reverted (`fts/indexing.rs:85-95`).
- **No change to `avgdl`'s definition**, only to its accuracy (§3.3 removes drift that was there before).
- **The super-linearity is explained but not fully characterised.** The O(corpus-bytes) reading (§1) is arithmetic on the schema plus the measured per-byte cost gap, not an SST-level measurement. It does not gate this fix — deleting the scan removes the term regardless of what its constant was. It *does* gate any future claim about *other* full-relation scans in the engine.
- **The block cache is a separate, larger bug and is deliberately NOT in this change.** `cozorocks/bridge/db.cpp:110-114`: when `use_bloom_filter` is set, the branch builds a **fresh default** `BlockBasedTableOptions` and does `options.table_factory.reset(...)`, discarding the table factory loaded from the options file at `:88` — including the block cache attached at `:83`. `storage/rocks.rs:103` sets `use_bloom_filter` **unconditionally**, so **mindgraph-rs's versioned 128 MB block cache has never taken effect**; every mnestic store runs on RocksDB's default. (`:63` compounds it: `NewLRUCache` hardcodes 1 GB and ignores `opts.block_cache_size`.) Fixing it perturbs **every** read path and invalidates every published mnestic latency number, including the D3 canned-vs-canned wins. It gets its own spec, its own change, and its own re-baseline — landing it here would make this fix's effect unattributable.

---

## 5. Test matrix (`cozo-core/tests/fts_corpus_stats.rs`, sqlite backend — the real stored path)

| Test | Pins | Discriminates against |
|---|---|---|
| `unindexed_rows_do_not_change_bm25_scores` | 3 indexed docs score identically with and without 200 empty-body rows beside them. `df` and doc lengths are pinned by construction, so **`N` is the only free variable**. | `N` from the base relation. Pre-fix: 0.1335 vs 4.065 → **RED**. |
| `whitespace_only_rows_do_not_change_bm25_scores` | Same, for whitespace-only bodies (tokenize to zero tokens). | as above → **RED** pre-fix. |
| `repeated_identical_puts_do_not_drift_n` | Ten no-op `:put`s of the same rows move no score. | The missing idempotence probe (§3.3). Pre-fix: 0.1335 vs 2.27 → **RED**. |
| `indexed_writes_do_move_n` | Adding genuinely-indexed documents **does** move the IDF (`df` held constant). | A frozen/never-invalidated `N`, which the three tests above would not catch. |

**Discrimination was proven, not asserted**: each fix was reverted in the working tree and the corresponding tests observed to fail (the project's standing rule — *a correctness oracle cannot guard a performance fix*). Redefining `N` per §3.1 is what makes a correctness oracle sufficient here; had we merely cached the old number, no score would have moved and only a timing test could have guarded it.

Unchanged and passing: `bm25.rs` (3), `fts_avgdl.rs` (4), `fts_lsh_update_leak.rs` (5), `reindex.rs` (6).

---

## 6. Rejected alternatives (recorded so they are not re-litigated)

| Alternative | Why not |
|---|---|
| Cache the **base-relation row count** at Db level, keeping `N`'s old meaning | Faithfully caches a wrong number; needs a second cache with its own invalidation surface (put/rm/replace/drop/`import_relations`/`::reindex`); and no score moves, so it can only be guarded by a timing test. Strictly worse on correctness, maintenance, and testability. |
| Persist `N` in the index manifest / `RelationHandle` | Those *are* rmp_serde-persisted catalog types — the Jul-4→Jul-8 outage class. Unnecessary: the value is derivable and already maintained in memory. |
| A durable shared-key counter | The 0.8.3 design. One hot key ⇒ every concurrent writer conflicts on a single RocksDB lock. Already tried, already reverted. |
| A storage-level row estimate | No such API exists: `range_count` is the only count on the `Storage` trait, and it iterates. |
| Hoist the `FtsCache` to live longer than a query | Fixes the repetition but not the wrongness, and introduces an invalidation problem the doc-stats cache has already solved. |

---

## 7. Decision record

| # | Decision | Status |
|---|---|---|
| 1 | `N` = documents with ≥1 posting (index population), **not** base-relation rows. Scores change on corpora with unindexed rows. | **Proposed** — 2026-07-13 |
| 2 | Source it from the existing Db-level doc-stats cache; delete `get_n_for_relation` and `total_n_cache`; `FtsCache` becomes stateless. | **Proposed** — 2026-07-13 |
| 3 | Add the idempotence probe to `put_fts_index_item` (§3.3), fixing the pre-existing no-op-write drift `avgdl` was masking. | **Proposed** — 2026-07-13 |
| 4 | The block-cache clobber (§4) is a **separate** change with its own re-baseline. Not in this one. | **Proposed** — 2026-07-13 |
| 5 | Vehicle: banked on `main` for **0.13.0**. Not a 0.12.3 (decision 18). | **Proposed** — 2026-07-13 |

## 8. Build plan

1. ✅ Fix + tests on `main`; `CHANGELOG-FORK.md` **Unreleased**.
2. ⏳ **Re-run `hybrid-recall-bench` at `small` and `medium`** on the same box — the release's proof artifact. Expect the FTS leg to collapse to the posting scan alone and `medium` p50 to fall toward `vector + graph + fusion`. **If p50 does not collapse, the model is wrong and the FTS leg has another O(corpus) term** — investigate before claiming anything.
3. ⏳ Only then state a number. No N× claim ships without both scales re-measured on one box (standing rule).
4. ⏳ Separately: spec the block-cache clobber and re-baseline every published latency figure.
