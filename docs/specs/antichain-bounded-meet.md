# Spec — Antichain Bounded-Meet: custom dominance registration (the skyline aggregate)

_Created 2026-07-04. Status: **PROPOSED — awaiting owner sign-off; do not build before §8's decisions are signed.** This is the dedicated spec that `cozoscript-extensions.md` §3.1 requires before implementation: it resolves that section's four open questions (Q1 cap, Q2 law/probe, Q3 delivery surface, Q4 contracts) and replaces its honest-touch-list sketch with a buildable design. Grounded two ways: (a) against the shipped 0.10.0 `BoundedMeetStore` code (citations are `file:line` in `cozo-core/src/`), verified this session by a seven-dimension review panel plus an adversarial code review; (b) against the skyline literature for the three genuinely uncertain calls — the insert algorithm (BNL in-buffer maintenance is the canonical in-memory approach), the cap question (every principled bounded skyline in the literature is a *semantic* reduction — representative-k, k-dominant — never arrival-order truncation), and the tie-break composition rule (lexicographic composition is safe over strict **weak** orders; a genuinely partial clause is safe only in last position). Companion to [`provenance-semirings.md`](provenance-semirings.md) (R1 recorded exactly this as deferred: "Deferred: custom bounded-meet registration; configurable cap.") and [`cozoscript-extensions.md`](cozoscript-extensions.md) §3.1._

> **Anti-overbuild guardrails.** One new aggregate category slot, opened by registration — no DDL, no in-language dominance, no representative-skyline objectives, no persistence of registered algebras. The store core is new (the shipped one is total-order-shaped end to end and cannot be parameterized honestly); everything around it — stratifier permit, magic-set exemption, eval dispatch, saturation, trigger policy — is reused verbatim. Budget it the way the semirings spec §6 budgeted R1: **genuinely-new engine work**, not a parameter on an existing slot.

---

## 1. Why / what this buys

`min_cost_k` is `BoundedMeet` with one hardcoded prune: keep the k cheapest under a total order. The general operation is *keep the non-dominated set under a caller-supplied strict partial order* — the skyline / Pareto frontier (Börzsönyi 2001). The driving read: surfacing a **contested set** (survivors none of which dominates another) instead of silently picking one winner — plus multi-objective ranking, conflict detection, and maximal-element queries, all generic to any consumer (fraud graph, diagnostic engine, audit store).

```
# the shipped special case — dominance = "worse cost, beyond rank k":
best[claim, min_cost_k(pack, 3)] := candidate[claim, chain, cost], pack = [chain, cost]

# this spec — dominance registered by the host, called by name:
#   db.register_bounded_meet_aggr("antichain", dominates, max_survivors)
surviving[claim, antichain(binding)] := candidate[claim, binding]
```

Singleton per group → resolved; more than one survivor → contested. `min_cost_k(pack, 1)` always returns one and hides the tie.

## 2. Shipped baseline this builds against (verified)

| Piece | Where | What transfers |
|---|---|---|
| `AggrKind::BoundedMeet` gate + v1 head-shape rule (exactly one bounded aggregate, last head position) | `query/compile.rs:37-47`, `:88-102` | verbatim |
| Stratifier permit (`is_meet \|\| is_bounded_meet` treated alike; non-meet recursion rejected) | `query/stratify.rs:57-63` | verbatim |
| Semi-naive loop, changed-bit saturation, `has_delta` | `query/eval.rs:340-350`, `runtime/temp_store.rs:477` | verbatim |
| Divergence cap: hard error after 4096 consecutive changed-epochs, resets on quiet epochs, cannot cap co-stratified recursion | `query/eval.rs:36`, `:144-153`, `:351-364` | verbatim |
| Registration precedent: factory field + `Db`-scoped registry + parser fallback + trigger rejection + name interning | `register_custom_aggr`, `runtime/db.rs:945-983`; `meet_factory`, `data/aggr.rs:33`; parser `parse/query.rs:828-876` | as the **pattern to copy** (the existing typing cannot carry a dominance object — §3) |
| Non-recursive path: `AggrKind::BoundedMeet` is not recursion-conditional — bounded-meet rules run through the same store in plain non-recursive rules | pinned by the non-recursive `chain` rule in `cozo-core/tests/spec_doc_validation.rs` | verbatim (no normal-path adapter analogue needed) |
| **What does NOT transfer: the store core.** `BoundedMeetStore` is total-order-shaped end to end — binary-search insertion on a contractually-total `cmp_candidates`, dedup on `Ordering::Equal`, rank-k early reject, single-pop truncation, order-defined delta-twin, cost-ordered output | `runtime/temp_store.rs:236-378` | **replaced** by §3's `DominanceMeetStore` |

Custom aggregates take no call-site arguments in the shipped R0 registration (the bails at `data/aggr.rs:1396` and `:1420`); `min_cost_k`'s `k` is builtin-only plumbing. §3 sidesteps rather than extends this.

## 3. Design

### 3.1 Registration API (host-API only, mirroring R0b)

```rust
db.register_bounded_meet_aggr(
    name: String,                                            // lowercase ident; reserved names rejected
    dominates: Arc<dyn Fn(&DataValue, &DataValue) -> bool + Send + Sync>,
    max_survivors: usize,                                    // mandatory resource guard; overflow = loud error
) -> Result<()>
```

- **The cap lives in the registration, not the call site.** This deliberately sidesteps the registered-arg plumbing gap (§2 last row): the head form is `name(operand)` — one body-bound operand variable, zero trailing args, parsed by the existing `aggr_arg` grammar unchanged. Per-call-site cap overrides are deferred until pulled (they would need registered-arg-shape handling in `parse_rule_head_arg`).
- Registry: `Db.custom_bounded_meets: Arc<ShardedLock<BTreeMap<String, RegisteredBoundedMeet>>>` alongside `custom_aggrs`; `RegisteredBoundedMeet { dominates, max_survivors }`. Same name-interning, duplicate-rejection, builtin-reservation, and unregister semantics as `register_custom_aggr` — plus one extension: the reservation check also rejects builtin **function** names (the `coalesce`-collision lesson from `cozoscript-extensions.md` §3.4; apply it to `register_custom_aggr` in the same change).
- Parser: in `parse_rule_head_arg`, after the builtin `parse_aggr` miss and the `custom_aggrs` miss, consult `custom_bounded_meets` → construct an owned `Aggregation { is_bounded_meet: true, bounded_dominance: Some(entry.clone()), .. }` (a new factory-style field, `const None` on the builtin path exactly like `meet_factory`). Thread through the same six `parse_script` call sites R0b threads — the internal run loop and `Db::run_script` (`runtime/db.rs:395/469`), `DbInstance::run_script` (`lib.rs:244`), and the three trigger re-parses (`query/stored.rs:96/752/2064`).
- **Call-site args rejected loudly.** The `aggr_arg` grammar happily parses stray trailing args (`antichain(binding, 3)`) and const-evals them (`parse/query.rs:841-843`); the shipped "custom aggregates take no arguments" guard sits on the meet-init path this category never traverses. The registered-bounded init analogue must reject non-empty args with a clear error, or they would be silently ignored.

### 3.2 The store: `DominanceMeetStore` (new), BNL insert

A separate store struct selected at store-construction time where `BoundedMeetStore` is selected today; it reuses the epoch/delta plumbing and differs only in its set-maintenance core. The dispatch surface, named so nobody invents it silently: a new `TempStore` variant (arms in `EpochStore::merge_in` / `range_iter` / `is_empty` / `exists`), a `new_bounded_meet`-style constructor analogue, and builtin-vs-registered dispatch inside `initial_rule_bounded_meet_eval` / `incremental_rule_bounded_meet_eval`'s store construction — mechanical, but real lines. The insert is the canonical BNL in-buffer maintenance (scan the survivor buffer; reject if dominated; otherwise evict everything the newcomer dominates and insert):

```
insert(c):
  for s in survivors:
      if s == c (structural DataValue equality)  → reject, unchanged   # engine-side dedup, ○= is equality
      if dominates(s, c)                          → reject, unchanged
  removed = survivors.retain(!dominates(c, ·))                          # multi-removal
  survivors.push(c);  changed = true
  if survivors.len() > max_survivors → bail! (loud, names aggregate + cap + group)
```

- O(n) per insert / O(n²) per group worst case — the accepted in-memory baseline (BNL; indexed skyline methods like BBS exist for disk-resident bulk data and are overkill for a per-group aggregate buffer).
- **v1 needs only the insertion half.** The literature's hard half — incremental maintenance under *deletion* (BBS-Update's window queries) — cannot arise: within one fixpoint evaluation the store receives a monotone candidate stream; nothing retracts mid-evaluation. (`:reconcile` and persistence operate on materialized output rows, never on the store.)
- **Dedup before domination**: structural equality is checked first so an element never "dominates itself" via a buggy closure; the debug probe (§3.4) additionally asserts irreflexivity.
- Delta-twin (the second store used for semi-naive deltas): equality-dedup only, no dominance pruning — mirrors how the shipped delta store relaxes the main store's contract, and keeps deltas a superset of what may still change the main store.

### 3.3 Output order: canonical, not arrival

An antichain has no natural order and insertion order is arrival-dependent — emitting it would leak evaluation scheduling into results (non-confluent output for confluent *sets*). The survivor `Vec` is therefore **maintained in the engine's standard `DataValue` total order** (memcmp — the same order any materialized relation would impose on the rows anyway): insertion position by binary search, which also makes §3.2's structural-equality dedup a free lookup, while the dominance scan stays linear (dominance is unrelated to memcmp order). `range_iter` then streams the already-sorted set every epoch, not just at final emission. Pinned by a permuted-input confluence test (§6i).

### 3.4 Laws and probes

- **The caller's law:** `dominates` must be a strict partial order — irreflexive and transitive (hence asymmetric) — and **pure** (no interior mutability, no clock/RNG). Under that law the uncapped fold is order-insensitive over the surfaced candidate set: dedup-then-BNL maintains exactly the maximal elements of the candidates seen so far, independent of arrival order (transitivity is what licenses discarding a dominated element without re-comparing what it dominated).
- **The safe tie-break recipe (the "narrower agent wins ties" pattern, made precise):** compose clauses lexicographically only when every clause except possibly the **last** is a strict *weak* order (incomparability is transitive — i.e. the clause behaves like a score/key). Lexicographic composition of strict weak orders is itself a strict weak order; putting a genuinely partial clause (e.g. set-inclusion "narrower scope") anywhere but last can silently break transitivity of the whole. Document with a worked example in the registration rustdoc.
- **Debug-build probes** (the R0b idempotence probe re-applies `update` and cannot check a comparator, so this category gets its own): on every insert, assert `!dominates(c, c)` (irreflexivity); on every comparison that returns true, assert the swapped call returns false (asymmetry). Transitivity is not cheaply probeable and stays a documented obligation. A violation panics (debug only), naming the aggregate.
- **The closure sees only the aggregated operand.** No side-channel to other columns or relations — the packed candidate must carry every field the predicate inspects (`cozoscript-extensions.md` §3.1 Q2(v), A.1).
- **Recursion semantics:** identical approximate reading to `min_cost_k` — inside recursion the result is the antichain of *surfaced* candidates, with the 4096-epoch divergence cap as the backstop (a cyclic "dominance" that keeps displacing survivors surfaces as that loud error). The two guards divide the labor: the epoch cap backstops non-termination; `max_survivors` backstops unbounded store growth (see §4).

### 3.5 What is deliberately NOT in v1

In-language dominance (a 2-ary Datalog relation consulted mid-fixpoint — a genuinely new evaluator capability; trigger renamed honestly: *the first bindings/server consumer*, see §5); per-call-site cap args; representative-skyline / k-dominant objectives (different operators, not caps — see §4); `@using` algebra DDL (`cozoscript-extensions.md` §3.5, trigger unchanged); persistence or fingerprinting of registered algebras (caller's versioning obligation, §7); `catch_unwind` hardening (same status as R0b).

## 4. The cap decision (Q1 — resolved: loud error, never truncation)

An antichain has no canonical k-subset. A silently truncating cap would make results depend on candidate arrival order (non-confluent), dropping a non-dominated `x` un-prunes everything only `x` dominated (breaking the antichain invariant), and under re-feeding the dropped candidate re-arrives, flips the changed-bit, and can spuriously trip the 4096-epoch bail. The shipped engine's two caps are deterministic-by-total-order (`min_cost_k`) or a loud error (the epoch cap) — never a silent partial-order truncate.

The literature agrees from the other direction: skyline cardinality genuinely explodes with dimensionality (the curse-of-dimensionality results in the skyline survey literature), and every principled answer to "bound the skyline" is a **semantic reduction with its own objective** — representative-k skylines (maximize dominance coverage), k-dominant skylines (weaken the dominance relation), ranked skylines — i.e. *different operators*, none of them arrival-order truncation. If a consumer someday needs bounded output, that is a future `representative_*` registration with a declared objective, not a flag on this one.

**Decision:** `max_survivors` is a mandatory registration parameter; exceeding it mid-fold is a hard `bail!` naming the aggregate, the cap, and the group key — exactly the divergence-cap philosophy. No default (the registrant must think about it); no truncation mode.

## 5. Delivery surface (Q3 — stated, not solved)

Host-closure registration is **Rust-embedded only**: unreachable from PyPI `mnestic`, `langchain-mnestic`, `llama-index-vector-stores-mnestic`, and `cozo-bin` — the same limit the semirings spec records for R0b ("registration is host-API/Rust-only"). A Python-callable dominance closure would mean per-pair FFI callbacks inside the fixpoint loop (GIL, purity, performance) — significant, unscoped, and not v1. For bound/served consumers the only honest route is the deferred in-language dominance. v1 ships with this limitation stated in the registration rustdoc and the public docs; the in-language variant's trigger is *the first bindings/server consumer who needs it*, at which point it gets its own spec (it changes the evaluator, not just registration).

## 6. Test matrix (all sqlite backend per the repo test-backend rule; failing-test-first per house workflow)

- (a) Non-recursive 2-D Pareto frontier over a stored relation with a known answer set — the A.1 contested-set scenario from `cozoscript-extensions.md`.
- (b) Recursive use: graph walk with dominance pruning converges; result matches the non-recursive antichain of the reachable candidate set.
- (c) Full containment / equal payloads / incomparable chains — dedup and multi-removal correctness.
- (d) `max_survivors` overflow → loud error naming aggregate + cap.
- (e) Debug probes fire on a deliberately reflexive and a deliberately symmetric closure.
- (f) Non-meet recursion rejection unaffected; antichain in recursion permitted (stratifier parity with `min_cost_k`).
- (g) Trigger scripts referencing a registered bounded-meet rejected at `::set_triggers` (R0b parity).
- (h) Builtin-name and duplicate registrations rejected; **builtin function names now also rejected** (both here and in `register_custom_aggr`).
- (i) **Confluence:** permuted input orders (including adversarial orders that maximize mid-fold evictions) produce byte-identical output (canonical emission order, §3.3).
- (j) Persistence: `:put` of antichain output persists as ordinary rows; readable after reopen with no re-registration; recompute without registration errors loudly (R2 parity).
- (k) Non-recursive path exercised explicitly (no adapter — §2).

## 7. Contracts (Q4 — inherited + extended)

Inherit the R0b contract set wholesale: reserved/lowercase names, duplicate rejection, trigger rejection at validation time, factory/closure infallible-cheap-no-panic (no `catch_unwind` in cozo-core — a panicking closure unwinds into the host), registry in-memory and `Db`-scoped, outputs persist as plain rows. New and specific to this category: **nothing fingerprints a registered algebra** — re-registering a different `dominates` under the same name silently changes what a later `:reconcile` diff of persisted antichain output means; the documented obligation is to version algebra names like schemas (`antichain_authority_v2`).

## 8. Decisions for sign-off

1. **Cap = mandatory `max_survivors`, overflow = loud error** (§4). _Rejected:_ silent truncation (non-confluent); optional cap (unbounded store); total tie-break canonical truncation (collapses toward `min_cost_k`; a legitimate *future* registration, not this one).
2. **Cap in registration, not call site** (§3.1) — avoids registered-arg parser plumbing entirely. _Rejected for v1:_ `antichain(binding, k)` call-site form.
3. **New `DominanceMeetStore`, BNL insert, equality-dedup-first, delta-twin = dedup-only** (§3.2). _Rejected:_ parameterizing `BoundedMeetStore` (its `cmp_candidates` contract is total-order; a partial order cannot implement it honestly).
4. **Canonical memcmp emission order** (§3.3). _Rejected:_ insertion order (leaks scheduling).
5. **Laws: strict partial order + purity; probes: irreflexivity + asymmetry (debug); transitivity documented; tie-break recipe: strict-weak clauses, genuinely-partial clause only last** (§3.4).
6. **Rust-embedded-only v1, limitation stated; in-language dominance deferred with a named trigger** (§5).
7. **Name-reservation extended to builtin function names, retrofitted onto `register_custom_aggr` in the same change** (§3.1).
8. **Version/release: lands with the interval primitives in 0.11.0; CHANGELOG-FORK in the same PR; contributor-owned implementation is explicitly suitable** (well-bounded, mechanism-only; maintainer reviews against this spec).

## 9. Prior art

- Börzsönyi, Kossmann, Stocker — *The Skyline Operator* — ICDE 2001 — the operation itself; BNL (block-nested-loops) is the in-buffer maintenance §3.2 adopts.
- Papadias, Tao, Fu, Seeger — *BBS / BBS-Update* — the indexed and incremental-maintenance line; cited here mostly for what v1 **avoids** (deletion-side maintenance cannot arise in a monotone fixpoint stream).
- The skyline survey literature (e.g. Kalyvas & Tzouramanis, *A Survey of Skyline Query Processing*, arXiv:1704.01788) — cardinality explosion with dimensionality; the taxonomy of bounded variants (representative-k, k-dominant, ranked) that grounds §4's no-truncation decision.
- Green, Karvounarakis, Tannen — *Provenance Semirings* — PODS 2007; Li, Huang, Naik — *Scallop* — PLDI 2023 — the bounded/approximate provenance frame (`○=`, top-k) whose registration slot this opens, per `provenance-semirings.md`.
- Strict weak orderings (comparator-correctness literature) — lexicographic composition preserves strict-weak; incomparability-transitivity is the property a genuinely partial clause lacks — the formal basis of §3.4's tie-break recipe.

---

## Changelog

| Date | Change |
|------|--------|
| 2026-07-04 | First authoring, per `cozoscript-extensions.md` §3.1's disposition ("its own spec resolving Q1–Q4 before any build"). Q1–Q4 resolved as §8 decisions 1–7; store design, laws/probes, test matrix, and delivery-surface position stated. Literature grounding: BNL insert, no-truncation cap, strict-weak tie-break composition. Awaiting owner sign-off. |
