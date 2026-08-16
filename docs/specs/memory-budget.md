# Spec — Query Memory Budget: a per-script byte budget on materialization, tripping a clean typed error before the host OOMs

_Created 2026-08-16. Status: **BUILD-READY — §12's seven decisions signed by the owner 2026-08-16, all as proposed.** (Originally: PROPOSED — awaiting owner sign-off.) This is the dedicated spec behind the headline item of the next engine minor (the "operations & trust" scope; the pre-0.14.0-release planning docs called it "0.14" — 0.14.0 then shipped 2026-08-08 as a security-first minor without it, so every "0.14" in older docs reads "next minor" here). Grounded two ways: (a) against the shipped evaluation pipeline — citations are file:line in `cozo-core/src/`, gathered and spot-verified at HEAD 2026-08-16, including a compiled probe measuring `size_of::<DataValue>()`; (b) against a nine-system prior-art sweep run the same day (ClickHouse, Neo4j, DataFusion, SQLite, DuckDB, PostgreSQL, RDFox, Redis, Soufflé/Datomic/TypeDB — behaviors and sources summarized in §10). Companion to [`passthrough-streaming.md`](./passthrough-streaming.md) (documents one adjacent unaccounted materialization) and [`antichain-bounded-meet.md`](./antichain-bounded-meet.md) (the spec-discipline template, and the ratified in-house precedent for a resource guard's determinism posture)._

> **Anti-overbuild guardrails.** One counter handle threaded beside the existing `Poison`, charged at the shipped materialization sites, checked at the shipped cadences, tripping one new typed error. **Zero planner changes, zero new evaluation phases, zero storage-backend involvement, zero spill machinery.** The knob plumbing clones the timeout triple verbatim (per-call option, per-block `:mem_limit`, Db-wide default, min-composed). The byte estimator reuses the graph-projection cache's existing vocabulary (`size_of::<DataValue>()`, `value_heap_bytes`, `BTREE_ENTRY_OVERHEAD` — `graph_projection.rs:447,1297-1343`) rather than inventing a second one. Explicitly rejected up front, with the prior-art receipts in §10: spill-to-disk (an engine-architecture rewrite; DuckDB itself aborts rather than spills for recursive dedup state), allocator hooks (`#[global_allocator]` belongs to the host; SQLite/ClickHouse own every allocation site, a library crate does not), heap/RSS sampling (host-allocator-dependent, nondeterministic), overcommit/victim-selection arbitration (server-fleet machinery), and any per-user or cross-query pool (a later extension seam, not v1). Budget it as **one accounting concern threaded through entirely-shipped scaffolding.**

---

## 1. Why / what this buys

The temp stores that hold every intermediate result of semi-naive evaluation have **zero accounting**. Semi-naive intermediates funnel through **four** in-RAM stores in `runtime/temp_store.rs`, each a std `BTreeMap` keyed by `Tuple = Vec<DataValue>` (`data/tuple.rs:17`) — `RegularTempStore { inner: BTreeMap<Tuple, bool> }` (:26-29), `MeetAggrStore` (:91-96), `BoundedMeetStore` (:236-248), `DominanceMeetStore` (:399-416) — wrapped by `enum TempStore` (:593-599) inside `EpochStore { total, delta, … }` (:659-665); §4 lists the remaining in-evaluation materialization channels. Nothing counts a row or a byte on any write path.

The failure this produces is the worst kind for an embedded engine: a mis-authored join OOM-kills the **host process**. Measured arithmetic (cross-validated at HEAD this session): `size_of::<DataValue>()` = 56, a `Vec` header = 24, so a 2-column tuple owns 136 bytes, plus ~32–48 bytes of BTreeMap node overhead — reproducing the audit's empirical **~168.4 B/row**, under which a `tmp[a,b] := rel[a], rel[b]` self-join over ~3,450 rows materializes ~11.9M rows ≈ 2 GB and kills a 2 GB container. An output `:limit` does not help: the limiter is entry-rule-only (`eval.rs:399`) and disabled under `:order` (`db.rs:3079-3083`), so intermediates materialize in full regardless.

The engine now ships inside other people's processes — the MCP server, the LangGraph store, LangChain/LlamaIndex adapters — which converts this from a footgun into a positioning liability: the embedded-substrate positioning itself (the public roadmap's North star — *one embedded engine*) is attacked directly by an engine that can kill its host. The prior-art sweep sharpens the upside: **no embedded Datalog engine has this.** Soufflé ships no memory option at all (OOM = OS kill), Datomic queries OOM the host peer JVM, TypeDB documents nothing, and RDFox's `max-memory` is whole-server. A deterministic per-query budget with a clean error is a genuine first in this class, and it is exactly the "safe substrate inside your process" story the wedge needs.

## 2. The refuted containment, and why this spec's shape follows from it

A Cx-4 containment was **built and refuted** (2026-07-13, recorded in the workspace audit archive): a row-count check placed at the per-rule sites. The refutation, verbatim from the record: `RegularTempStore::inner` has four write sites, not two (`put` :59-61, `put_with_skip` :62-64, and `merge_in`'s `mem::swap` :73 + `ent.insert(v)` :80); the accumulating store is fed exclusively by the latter two, which bypass both setters; and the proposed check site (`poison.check()?` after the item loop) is per-rule-per-epoch — *"the rule's entire relation drains into the store before the check is reachable (reproduced with the fix applied and limit=1000: tuples_already_in_store=360000 limit=1000 — it errors after the OOM, not instead of it)."* Verified at HEAD: the geometry is unchanged — the item loop fills `out_store.put(item)` at `eval.rs:419` and `poison.check()?` sits at `:422`, outside it; same for the meet (:519-524), aggr (:579-608), and incremental variants.

Two structural conclusions the design must honor:

1. **A count maintained only in `merge_in` is insufficient.** `merge_in` runs at `eval.rs:360-362`, *after* every rule's per-epoch `out_store` has fully materialized. A merge-site-only count reproduces the refuted failure for single-epoch blowups — which is precisely the mis-authored-cartesian threat model. The internal planning record offered "a budget handle threaded like Poison" and "the count maintained in merge_in" as two equivalent alternatives; the code says only the threaded handle (or a hybrid) catches the reproduced failure. This spec builds the hybrid: charge at the accumulation sites, reconcile at the merge.
2. **"Threaded like Poison" under-specifies.** `Poison { flag: Arc<AtomicBool>, deadline: Option<Instant> }` (`db.rs:3487-3491`) carries its deadline as a per-clone `Copy` field. A budget counter is shared mutable state and must live **inside the Arc**, like `flag` — a real structural difference from how the deadline was added.

## 3. Shipped baseline this builds against (verified 2026-08-16)

| Piece | Where | What transfers |
|---|---|---|
| **`QueryLimiter`** — per-put, in-loop row counting with early termination: `AtomicUsize` + `incr_and_should_stop` (`eval.rs:41-70`), consulted inside the entry rule's item loop (:399-425) | `query/eval.rs` | **The hook template.** It proves the engine already tolerates an O(1) check on the accumulation path. The budget generalizes it: all rules (not just entry), bytes (not rows), error (not early-success) |
| `Poison` threading: every rule-eval fn takes `poison: Poison`; `RelAlgebra::iter` poison-wraps every operator-boundary stream at `POISON_CHECK_INTERVAL = 4096` pulls (`ra.rs:899,904-922,2338-2357`) | `query/eval.rs`, `query/ra.rs` | The threading surface — the budget handle rides the same parameters; no new plumbing reaches any new function |
| `BOUNDED_MEET_MAX_EPOCHS = 4096` mid-evaluation `bail!` (`eval.rs:36-39,374-384`) | `query/eval.rs` | Precedent for aborting a running fixpoint on a resource guard (currently a plain bail with no diagnostic code — §7 upgrades the pattern) |
| The timeout knob triple: per-call `ScriptRunOptions` (future-proofed for new options, `db.rs:99-112`), per-block `:timeout` (`program.rs:61-80`, `parse/query.rs:273-284`), Db default (`db.rs:174-180,764-776`), min-combined "can only tighten, never extend" (`db.rs:3043-3059`) | `runtime/db.rs`, `parse/query.rs` | Cloned verbatim as the knob shape (§6) |
| Byte-estimation vocabulary: `size_of::<DataValue>()`, `value_heap_bytes` for Str/Bytes/List heap, `BTREE_ENTRY_OVERHEAD = 48` (`graph_projection.rs:447,1297-1343`) | `runtime/graph_projection.rs` | Reused as the estimator (§5); unlike the projection cache (which warns and degrades, :1160-1177) the budget **aborts** |
| Timeout/kill error surface: `eval::killed` / `eval::timeout` miette codes; "a budget expiry aborts before any commit, so a killed mutable script leaves no partial writes" (`db.rs:3574-3595,599-600`) | `runtime/db.rs` | The error-contract pattern (§7); the same abort-before-commit guarantee transfers to the budget trip |
| Rayon evaluation: non-entry rules run `par_iter` within an epoch (`eval.rs:248-252,335-339`); entry rules sequential for limiter determinism (:322); the merge loop is sequential (:360-370) | `query/eval.rs` | Fixes the determinism analysis (§8): per-rule accounting is single-threaded; cross-rule trip order is schedule-dependent |
| EpochStore residency: `merge_in` clones each changed row's key into `prev` (`temp_store.rs:76-86`); BoundedMeet/Dominance merges re-materialize rows (:365-374,:578-586) | `runtime/temp_store.rs` | The accounting model must charge total + delta + per-epoch new co-residency (§5) — ignoring it undercounts peak 2–3× on churny epochs |

**What does NOT transfer:** any existing estimator of a *query's* memory (none exists), and any per-tuple check cadence (Poison's own doc forbids per-tuple inline checks, `db.rs:3568-3571` — §5 keeps the charge O(1) arithmetic and the compare amortized).

## 4. Design — surface

One budget, denominated in **bytes**, scoped **per script execution**:

- **Per-block option**: `:mem_limit <bytes>` in `QueryOutOptions`, parsed like `:timeout` (`parse/query.rs:273-284`). Applies to the block; multi-block scripts take the min of the block's and the script-level value for that block's evaluation.
- **Per-call option**: `ScriptRunOptions::with_mem_limit(bytes)` beside the timeout (`db.rs:99-112`).
- **Db default**: `Db::set_default_query_mem_limit(bytes)` / getter, beside `default_query_timeout_ms` (`db.rs:174-180,764-776`).
- **Combination**: minimum of all set values — "can only tighten, never extend" (`db.rs:3043-3059`). In particular, **script text can only lower the budget the host set, never raise it** — load-bearing because scripts are increasingly LLM-authored and arrive from untrusted contexts (the SQLite PRAGMA rule, adopted deliberately).
- **Unset/0 = unlimited** — bit-identical behavior to today (§12 Q5 carries the default question to the owner).

The budget covers **engine-held materialization during evaluation**: the four temp stores (charged at allocation events, §5 — including FixedRule output stores), plus the result-staging copies (`sort_and_collect`'s full Vec at `sort.rs:34`; the final collect at `db.rs:3165/:3221`). §12 Q3 rules on the remaining documented channels (`materialized_join`'s right-side cache `ra.rs:2796-2807`, the normal-aggr work map `eval.rs:553`, fixed-rule in-RAM builds, TempTx `_` relations `storage/temp.rs:47-50`) — proposed v1 posture: charge the first two cheaply if the seam is clean, otherwise document them as known-uncharged with the fixed-rule builds (which have their own 512 MiB projection ceiling).

## 5. Design — accounting

**The handle.** `MemBudget { counter: Arc<AtomicUsize>, limit: Option<NonZeroUsize> }`. For *checking*, it rides the already-threaded abort surface (every rule-eval fn, `RelAlgebra::iter`, and `FixedRule::run`'s poison parameter — `eval.rs:227`), clone-cheap like Poison, with the counter inside the Arc (§2 lesson 2). For *charging*, the threading surface is deliberately different: **the stores themselves hold a clone of the handle**, taken at construction — the four store constructors and `EpochStore::new_*` are all engine-internal call sites (`eval.rs:221,398,551,679`; `temp_store.rs:671-716`) — so `put`/`meet_put`/`merge_in` charge without any new parameters, `pub fn put`'s signature is unchanged for FixedRule implementations, and **FixedRule output stores are charged for free** (a fixed rule with a runaway output would otherwise reproduce the refuted after-the-fact geometry, since `run` fills its out-store internally and returns once).

**The charge — allocation events, not write paths.** Chargeable events, at `est_tuple_bytes(t) + BTREE_ENTRY_OVERHEAD` where `est_tuple_bytes = 24 + n·56 + Σ value_heap_bytes(v)`: (i) a `put`/`put_with_skip`/`meet_put` that actually inserts; (ii) the merge entry-arm's key-clone into `prev` (`temp_store.rs:79`); (iii) the BoundedMeet/Dominance merge re-materialization (`:365-374`,`:578-586`), which genuinely copies rows. **Accounting-neutral by design**: `RegularTempStore::merge_in`'s `mem::swap` fast path (`:72-75`) and its `ent.insert(v)` (`:80`) are *moves* of already-charged allocations — charging them would double-count the store in one step. **Debits**: each store keeps a private `charged: usize`; the debit sites are enumerated, not implied — a duplicate-key `put` (the incoming tuple drops, `:59-61`), the merge Occupied-arm drop (`:82-84`), BoundedMeet displacement pops (`:313-315`) and rejected candidates (`:306-310`), Dominance retain-eviction (`:519`), the epoch `prev` clear, and store drop (inter-stratum `stores.retain` at `eval.rs:86-89`, and scope exit) which debits the store's `charged` wholesale. The debit path is infallible (the ClickHouse "free should never throw" invariant). One refactor is part of v1 and budgeted here: `value_heap_bytes` and `BTREE_ENTRY_OVERHEAD` are today `#[cfg(feature = "graph-algo")]`-private to `graph_projection.rs` (`:446-447`,`:1326-1327`); they move to a shared un-gated module so the estimator exists on every build.

**The check.** Two placements, both shipped cadences:

1. **At the charge site**: O(1) — a relaxed `fetch_add` and a compare against the limit. This is the `QueryLimiter` pattern (`eval.rs:48-55`) generalized; it catches the single-epoch cartesian blowup the refuted containment missed, *before* the store fills.
2. **At the poisoned-iterator cadence** (`POISON_CHECK_INTERVAL = 4096`, `ra.rs:899`): the existing wrap point gains a budget compare beside the poison check, bounding overshoot from any charge path that batches.

If profiling shows the per-put atomic measurable, the fallback is the ClickHouse `max_untracked_memory` pattern — a thread-local delta flushed to the shared counter every ~64 KiB — but the target is that no batching is needed: one relaxed add + one compare per materialized row is the same order of work as the `BTreeMap` insert beside it. Performance gate in §11.

**Honesty contract.** The figure is a **logical estimate of engine-held bytes, not process RSS** — it undercounts allocator overhead by a roughly constant factor and counts nothing outside the charged structures. Neo4j documents exactly this posture ("only an estimate… slightly larger or slightly smaller") and it is the industry-standard license for approximation; the docs and the error message both say "estimated". RDFox's release-note history (five crash/corruption fixes for exhaustion mid-update; one over-reporting bug causing spurious trips) is the cautionary tale §11's trip-path tests exist for.

## 6. Design — knob semantics

Clone of the timeout triple (§3 row 4), with one deliberate divergence: the timeout ships unset and the 2026-07-13 audit found nobody arms it — "the defense exists but nobody arms it" is a known failure mode. Mitigations proposed (Q5 decides): engine default stays unset (never-silently-break-upstream; ClickHouse `max_memory_usage=0` and Neo4j `db.memory.transaction.max=0` are the precedents), but (a) the flagship consumers (mindgraph-server, `mnestic-mcp`, the LangGraph store) arm an explicit budget in the same release train, and (b) `cozo-bin` ships with a default budget **on** (it is an application, not a library — it may break upstream-parity where a library must not).

## 7. Design — the error

A dedicated, stable miette diagnostic following `eval::killed`/`eval::timeout` (`db.rs:3574-3595`):

- **Code**: `eval::mem_budget_exceeded`.
- **Message** (the union of ClickHouse's and Neo4j's shapes): the estimated live bytes at trip, the attempted charge, the effective limit, **which knob set it** (block/call/default), and the rule symbol + epoch being evaluated (attribution requires only the per-rule context already in scope at the charge sites; `max_survivors`' naming of aggregate+cap at `temp_store.rs:524-530` is the in-house precedent).
- **Help**: name the knob to raise and suggest the query-side fixes (`::explain`, join-order, factorized count).
- **Semantics**: the abort rides the existing script-error path — before any commit, so a mutable script leaves no partial writes (`db.rs:599-600`), the transaction rolls back (the RDFox abort-with-rollback precedent), temp stores drop, and no other query or the Db is affected. Documented as **retriable/transient** (the Neo4j classification).

`BOUNDED_MEET_MAX_EPOCHS`' code-less `bail!` stays as-is; this spec does not retrofit it (out of scope).

## 8. Design — determinism posture

Within one rule's evaluation the charge sequence is single-threaded, so the trip point is deterministic per-rule. Across rules in an epoch, rayon (`eval.rs:248-252`) makes *which rule observes the shared counter crossing the limit first* schedule-dependent — the total is a commutative sum and deterministic, the attribution is not. The codebase carries ratified precedent on **both** sides: sequential entry evaluation bought `:limit` its determinism (`eval.rs:322`), while the antichain spec deliberately accepted an arrival-order-dependent resource bail because determinism would have cost the very memory the guard protects (`antichain-bounded-meet.md:98`).

Proposed posture (Q4 decides): **adopt the antichain precedent** — guarantee "a query whose peak estimated usage exceeds the budget always errors; one comfortably under it always succeeds; at the margin, trip-vs-succeed and the attributed rule may vary with scheduling." A fully deterministic trip would require sequentializing rule evaluation or per-rule sub-budgets, both worse than the disease.

## 9. What is deliberately NOT in v1

- **No spill.** Not merely by fiat: DuckDB — the flagship spill engine — keeps its recursive-CTE dedup hash table memory-resident and answers exactly our workload shape with a clean canceled-query error; bounded spilling is abort with extra disk I/O (PG `temp_file_limit` cancels the transaction; DuckDB bounds spill at 90% of free disk); and the semi-naive total store is membership-probed by every derived tuple every epoch — the one access pattern no engine spills. Embedded-host reasons stand on their own: no background eviction threads in someone else's process, no surprise temp files beside the host's data (a data-governance regression for a memory engine), the `mem` backend has no disk, and BTreeMap stores have no page abstraction — spill is an Umbra-scale architecture, not a knob. A "future work" line may note sequential delta-spooling as the only coherent fragment; the total store never spills.
- **No allocator integration, no heap sampling** (§10 rows; structurally unavailable / nondeterministic).
- **No cross-query pool, no per-user hierarchy, no overcommit arbitration** — the handle holds an optional parent-pointer seam (the ClickHouse chain shape) so a process-global ceiling can be added later without redesign, but nothing arbitrates in v1.
- **No `::running` bytes column** — that display belongs to a planned `::running` observability enhancement and consumes this spec's counter when both land; the seam (expose peak + current estimated bytes on the running-query handle, `db.rs:3067-3071`) is noted here so the two don't collide.
- **No storage-side accounting.** RocksDB block-cache/memtable memory is bounded separately by [`cross-instance-memory.md`](./cross-instance-memory.md); the two measurements are disjoint by design and neither counts the other's bytes.
- **No change to `:limit` semantics** (`program.rs:66-69` contract untouched).

## 10. Prior art (each row verified against the named system's own documentation or source, fetched 2026-08-16)

| System | Knob / scope | Accounting | At limit | What we take |
|---|---|---|---|---|
| Neo4j | `db.memory.transaction.max`, per-txn, default ∞, dynamic | **logical estimate** at retain sites; scoped trackers; high-water mark | typed transient error naming the knob; "terminated without affecting the overall health of the database" | The architecture, the estimate disclaimer, the error shape |
| DataFusion | `MemoryPool` per context | **logical** `try_grow`/`shrink` per named consumer | `ResourcesExhausted` | Proof the pattern is idiomatic for an embedded Rust query library |
| ClickHouse | `max_memory_usage` per query, default ∞ | allocator-integrated hierarchical trackers; `max_untracked_memory` batching | `MEMORY_LIMIT_EXCEEDED` naming projected/attempted/limit | The message contract, the batching cadence, the parent-chain seam; **not** the allocator hook |
| SQLite | `hard_heap_limit64`, process-wide | own malloc wrapper | allocation fails, `SQLITE_NOMEM` | The embedded abort norm; the tighten-only PRAGMA rule; **not** the mechanism (we don't own allocation) |
| DuckDB | `memory_limit` per instance (80% RAM); no per-query cap | buffer manager | spill for 4 operator families; recursive dedup state aborts cleanly | The decisive abort-not-spill datapoint for recursive intermediates |
| PostgreSQL | `work_mem` per *operation*; no per-statement cap | logical bytes per executor node | spills; `temp_file_limit` cancels | Per-query scope is *stronger* than the canonical spill system offers |
| RDFox | `max-memory` whole-server (0.9× RAM) | — | abort-with-rollback | Rollback-on-trip semantics; the trip-path-testing lesson |
| Redis | `maxmemory` + `noeviction` | zmalloc wrapper | write commands rejected | Reject-with-clean-error as first-class policy; the check-cadence warning |
| Soufflé / Datomic / TypeDB | none | — | OS kill / host-JVM OOM / — | The gap this item closes; positioning |

## 11. Test matrix (sqlite backend per the repo test-backend rule; failing-test-first)

1. **The refutation repro, inverted** (inlined so the spec is self-verifying): over a relation `rel` of 2,000 int rows, `tmp[a, b] := *rel[a], *rel[b]` under a small `:mem_limit` must error with `eval::mem_budget_exceeded` while peak RSS stays bounded (the assertion the refuted containment failed: it must error *instead of* the OOM, not after — its repro filled the store with 360,000 tuples before a post-loop check could fire).
2. **Charge events, not write paths**: dedicated tests driving rows through `put`, `put_with_skip`, and both `merge_in` arms, asserting the *event* semantics — real insertions and `prev` key-clones move the counter; the `mem::swap` fast path moves it by **zero** (a move, not a copy); duplicate-key puts and the Occupied-arm drop debit or stay neutral; BoundedMeet/Dominance merges charge their re-materialized rows.
3. **Meet/BoundedMeet/Dominance parity**: each store variant trips under its own accumulation, including `merge_in` re-materialization residency.
4. **Trip-path integrity** (the RDFox lesson): a mutable script tripped mid-write leaves no partial writes (mirror of the timeout test at `db.rs:599-600`); a tripped query leaves the Db serving concurrent queries; repeated trip/retry cycles leak no counter residue (each store's private `charged` debits to zero at drop; the script counter returns to zero at scope exit).
5. **Knob combination**: block vs call vs default min-composition; script cannot raise a host budget; unset = unlimited bit-parity (a no-budget run's results byte-identical to pre-change engine).
6. **Determinism posture**: a comfortably-over query always errors across N runs; a comfortably-under query never errors; document (not assert) margin behavior.
7. **Overhead gate**: criterion bench on a materialization-heavy workload — unmeasurable delta with the budget unset (a `None` check), target <1% armed; run on mem *and* sqlite.
8. **Estimator sanity**: `est_tuple_bytes` on the narrow 2-col tuple lands within the audit's measured ~168 B/row bracket; a `Vec`-carrying tuple (1536-dim) charges ≥ its heap size.
9. **mem-backend parity**: the temp-store hooks are backend-independent; a smoke trip test runs on mem to pin it.

## 12. Decisions (signed by the owner 2026-08-16 — all seven as proposed)

| # | Question | Proposed |
|---|---|---|
| Q1 | Bytes (estimated) vs rows | **Bytes** — rows mis-scale ~10²B→multi-KB across tuple shapes; every surveyed system budgets bytes; estimator reused from graph-projection |
| Q2 | Charge-site hybrid (put-site + iterator-cadence + merge reconciliation) vs merge-only | **Hybrid** — merge-only is refuted by the single-epoch blowup (§2); this is the load-bearing design call |
| Q3 | v1 charge scope beyond the four stores | **Four stores — explicitly including FixedRule *output* stores (charged at the store's own write paths; distinct from the excluded fixed-rule internal in-RAM builds, which keep their projection ceiling) — plus sort/collect staging**; `materialized_join` cache + aggr map if the seam is clean, else documented-uncharged |
| Q4 | Determinism contract | **Antichain posture** — total deterministic, margin/attribution schedule-dependent; documented |
| Q5 | Default | **Engine unset; `cozo-bin` armed; flagship consumers armed same train** — revisit a %-of-RAM engine default only at a major |
| Q6 | Handle placement | **Split by surface**: check-side inside `Poison` (third field, counter in an Arc — the eval-fn/iter threading is churn-free); charge-side store-held (the four store constructors + `EpochStore::new_*` take the handle — engine-internal churn only, `pub fn put` unchanged) |
| Q7 | Estimator stability across releases | **Not stable** — documented as an estimate; tests pin brackets, not exact bytes |

## 13. Delivery

Lands in the next engine minor as its headline, banked per release discipline. No storage-format change, no bridge change, no publish-order implication. Public `ROADMAP.md` gains the item's public form when the spec is signed (it deliberately does not carry it today). The `::running` observability item consumes the counter when it lands (§9).
