# Spec — CozoScript Extensions for Provenance-Grounded Memory (the generic residue after 0.10.0)

_Created 2026-07-04. Source: an external design-partner note, following the "what's missing in mnestic to be a first-class TMS substrate" review. Status: **PARTIALLY SHIPPED — §3.1 (antichain bounded-meet) & §3.4 v1 (interval primitives) shipped in 0.10.1; §3.2/§3.3 documentation-only; §3.5 deferred (0.10.1 baseline)** after a full verification pass against the shipped 0.10.0 code (a seven-dimension review panel with adversarial refutation of every major finding, then this revision; every listing below marked ✓ is pinned by an actual engine run — `cozo-core/tests/spec_doc_validation.rs`, sqlite backend per the repo test-backend rule). Verification changed four load-bearing verdicts from the original note: **§3.3 is zero engine work** (the surface already shipped, including an infix projection operator the note didn't know about); **§3.1's cost was understated** (a parallel bounded-meet store core, not a swapped prune) and carries three design questions that must be resolved in a dedicated spec before build; **§3.2's minimal cut as first drafted was unrunnable** (underscore-temp trap, tt misuse, map destructuring the engine doesn't have) — the pattern below is the corrected, validated form; **§3.4 was blocked on an interval-representation decision** the original presupposed (resolved below: flat `[start, end]` columns for v1). §4's priority order is re-derived accordingly — it inverts. Companion to [`provenance-semirings.md`](provenance-semirings.md) and [`bitemporality.md`](bitemporality.md), both SHIPPED in 0.10.0. Scope unchanged: only **generically useful** engine/surface features — anything TMS-policy-specific (σ-visibility filtering, agent preorders, AAG status naming) belongs in a consuming library, not the engine._

> **Anti-overbuild guardrails (apply throughout).** Sections proposing work carry a **NOT in scope** line and a **minimal cut** that is the recommended first ship; sections resolved as already-shipped carry the documentation instead. Where a feature can live outside the engine, that is the preferred option; where it already ships, the deliverable is documentation, not code. The original note's through-line — "every item adds one parameter to an existing slot" — did **not** survive verification and is withdrawn: §3.2 and §3.3 are zero-engine-change documentation, §3.4's v1 is two small additions behind one representation decision, and §3.1 is genuinely-new engine work that must be budgeted the way the semirings spec §6 budgeted R1 ("genuinely-new engine work", not a flag flip). Ship the documentation cuts, scope §3.1 as its own spec, and stop.

---

## 1. Problem / why this exists

0.10.0 landed the two hard substrates — pluggable provenance semirings (R0–R3) and the bitemporal axis. Against that baseline, a review of what a demanding provenance-grounded memory still needs turned up **seven** candidate gaps. Re-sorted against the shipped state and a strict *generic-vs-policy* axis, they close as: **one partly-shipped with residue** (→ §3.2), **one that splits** (its generic half already shipped → §3.3; its policy half is out), **one missing and generic** (→ §3.1 — the sole genuine engine item), **one subsumed** (→ §3.1), **one deliberately deferred** (→ §3.5), **one out as sugar pending a one-line clarification** (row 6), and **one expressible by convention rather than shipped** (row 7). **§3.4 (interval primitives) is not one of the seven — it is a new candidate this note adds**, included because it passes the same inclusion test.

The inclusion test: *would a second, unrelated consumer of mnestic — a diagnostic engine, a fraud graph, an audit store — want this?* If a feature only makes sense under one consumer's perspective semantics, it is out.

| Candidate (from the review) | Disposition |
|---|---|
| `complete(rel, scope)` as first-class | **Partly shipped** — `:reconcile` is whole-relation complete belief (verified: the grammar takes no scope argument and the implementation scans the full relation; its own doc comment pins "whole-relation semantics", `query/stored.rs:1091-1093`). Scoped + readable residue → §3.2 |
| σ-parameterized query views | **Splits** — the structured-param half **already shipped** (→ §3.3, documentation only); auto-filter-by-visibility is policy, **out** |
| Non-dominated-set (antichain) aggregate | **Missing, generic** → §3.1 — verbatim the R1 deferred item ("Deferred: custom bounded-meet registration; configurable cap.", `provenance-semirings.md` §4 R1) — but **not** a small one; see §3.1's touch-list |
| Agent-specificity tie-break | **Subsumed** by §3.1 as one more dominance clause — with a transitivity caveat (§3.1 Q2): lexicographic composition of orders is not automatically a strict partial order |
| Surface DDL for algebra selection | **Deliberately deferred** in 0.10.0; §3.5 names the trigger |
| Rule-kind head shapes | **Out (sugar)** *if* the review meant penalty ranking — that is rule-produces-cost + `min_cost_k` today. If it meant a non-idempotent, derivation-*combining* ⊕ in recursion (sum over alternative proofs), that is a genuinely missing algebra category the engine correctly bars (`sum` is not `is_meet`), not sugar. One clarifying exchange closes this row |
| `supersedes`/`:live`, `usesInput` identity | **Expressible by convention, not shipped as a guarantee.** `:reconcile` + `::history` + as-of give the belief lifecycle; carrying derivation identity in a `min_cost_k` payload is a *user convention* the engine neither enforces nor understands. Stated this way so no consumer reads it as engine-level lineage (PROV-style `usesInput` tracking does not exist) |

## 2. What 0.10.0 already gives (baseline — verified 2026-07-04 against code, not just the companion specs)

| Capability | Surface | Verified |
|---|---|---|
| Custom absorptive semiring registration | `Db::register_custom_aggr(name, is_meet, factory)` — host-API only; rides recursion via `is_meet` + changed-bit saturation | the `register_custom_aggr` fn in `runtime/db.rs`; stratifier permit `query/stratify.rs:53-64`; saturation `query/eval.rs:340-350` |
| Top-k proofs | `AggrKind::BoundedMeet` + `min_cost_k(pack, k)`. **Call convention:** the aggregated operand must be a *body-bound variable* (`aggr_arg` requires a var, `cozoscript.pest:79`) — bind `pack = [payload, cost]` in the body; a literal list in head position does not parse. v1 shape: exactly one bounded aggregate, last head position (`query/compile.rs:88-102`). Divergence cap = **hard error** after 4096 changed epochs (total count, `query/eval.rs:36` — hardened post-release from a consecutive streak), never a silent truncation; not yet configurable (deferred) | `BoundedMinCostK` `data/aggr.rs:67-121`, registration `:245`, parse/dispatch wiring `:1437`/`:1465`; truncation `runtime/temp_store.rs:298-318`; ✓ test |
| Tag persistence | tags-as-columns; `:put` of annotated output persists; composes with tt → annotated belief history | pinned by the `semiring_tags_persist_in_rows` test in `runtime/tests.rs` |
| User-driven complete-belief / retraction | `:reconcile` — diffs a query output vs the resolved current belief; records assertions + retractions (tt-only) / vt-cessations (bitemporal) as one commit-tt belief event. **Whole-relation only** (no scope argument) | `query/stored.rs:1115-1306` |
| Bitemporal axis | tt = engine-stamped trailing key; **not a bare `Reverse<i64>`** but a Validity-shaped pair `(Reverse<i64> ts, Reverse<bool> flag)` — the flag byte is the retract bit on tt-only relations and must-be-0/reserved on bitemporal ones. Read surface: per-atom selector **inside the braces** — `*rel{cols @ (tt: T)}` (`relation_named_apply`, `cozoscript.pest:95`) — plus block-level `:as_of`; `::history` | `bitemporality.md` §4–§7; ✓ test |
| **Valid-time representation (load-bearing for §3.4)** | vt is upstream Cozo **point-event `Validity`** (timestamp + assert/retract flag, `data/value.rs:126-131`). **No interval value exists anywhere in the engine** — an interval exists only implicitly between consecutive events of one key | verified; 0.10.0 deliberately kept vt as-is |

Two consequences §3 builds on: (a) "declare a relation's current **complete** belief" exists at **whole-relation** granularity via `:reconcile`; (b) "what did we believe, **and why**, as of T" exists via annotated `::history` + as-of reads (R2 tag columns survive into both). Neither is re-invented below. One erratum found and fixed in the companion spec (2026-07-04): `bitemporality.md` §4's example block printed the selector after the closing brace; the shipped grammar attaches it **inside** the closing brace — an attachment inherited unchanged from upstream, which is also why it stays: the engine is right, the examples were wrong, and adding an outside-brace spelling would be exactly the two-spellings tax that spec's §12/§13 rejects.

---

## 3. The extensions

### 3.1 Custom bounded-meet with pluggable dominance — the antichain / skyline aggregate. Genuinely missing; genuinely not small.

**Why.** `min_cost_k` is `BoundedMeet` with one hardcoded prune: *keep the k cheapest*. The general operation is *keep the non-dominated set under a caller-supplied strict partial order* — the **skyline** / Pareto frontier (Börzsönyi 2001). Generic uses: multi-objective ranking, conflict detection, maximal elements, and — the review's driver — surfacing a **contested** set (survivors none of which dominates another) instead of silently picking one. This is verbatim R1's deferred line item, plus a dominance parameter.

**Surface — PROPOSED (not yet parseable; shown for shape only):**

```
# shipped special case: dominance = "worse cost, beyond rank k"
best[claim, min_cost_k(pack, 3)] := supports_r[claim, chain, cost], pack = [chain, cost]

# proposed: keep the non-dominated set under a registered dominance
#   register_bounded_meet("antichain", dominates = <host closure>, on_overflow = error(cap))
surviving[claim, antichain(binding)] := candidate[claim, binding]
```

Note the registered-arg gap even in this sketch: custom aggregates take **no arguments** in the shipped R0 registration (the `custom aggregate {} takes no arguments` bail in `data/aggr.rs`'s `meet_init`/`normal_init`), and `min_cost_k`'s `k` is special-cased builtin plumbing — a registered bounded-meet with its own arg shape needs new init/arg handling, one of the touch-list items below.

**The honest touch-list (replaces the withdrawn "the only new surface area is the prune").** The shipped `BoundedMeetStore` is total-order-shaped end to end: binary-search insertion on `cmp_candidates` (contractually a **total** order), dedup on `Ordering::Equal`, rank-k early reject, single-pop truncation, an order-defined delta-twin contract, and cost-ordered output (`runtime/temp_store.rs:236-378`). A strict-partial-order dominance violates every one of those. An implementer builds:

- a **new insert algorithm** — linear dominance scan with multi-removal (one insert can evict several dominated survivors), replacing binary-search + single-pop;
- **structural-equality dedup** (`DataValue` equality) replacing `Ordering::Equal`-as-dedup — under a partial order, "incomparable" and "equal" are different, and conflating them corrupts the set;
- a **new trait / registration object** — `MeetAggrObj`'s factory typing cannot carry a dominance closure, and `cmp_candidates` cannot be implemented by a partial order without lying about totality;
- a **new factory field + registry variant + registration method** (the R0b `meet_factory` path is the *pattern* to copy, not a slot to reuse);
- **arg-shape plumbing** in bounded-meet init (see the sketch note above; the no-args bails for registered aggregates are the two `custom aggregate {} takes no arguments` guards in `data/aggr.rs`'s `meet_init`/`normal_init`);
- a **redefined delta-twin mode** (equality dedup without dominance pruning) and a **canonical output-order decision** (an antichain has no natural order; pick and document one, e.g. insertion-stable or memcmp);
- a **new debug probe** (see Q2).

What genuinely transfers, verbatim: the stratifier permit and magic-set exemption, the `AggrKind::BoundedMeet` gate and eval dispatch, `has_delta` saturation, the trigger-rejection policy, and the no-serialization property (registered algebras never persist — see Q4). Real, worth having — but this is a **parallel store core behind a shipped gate**, not a parameter.

**Open design questions — resolve in a dedicated spec before any build:**

- **Q1 — cap semantics (the biggest unaddressed flaw in the original).** An antichain has no canonical k-subset, so a *silently truncating* cap makes results depend on candidate arrival order (epoch/delta scheduling) — non-confluent; dropping a non-dominated `x` also un-prunes everything only `x` dominated, breaking the antichain invariant; and under re-feeding, a dropped candidate re-arrives, flips the changed-bit, and can spuriously trip the 4096-epoch bail. The shipped caps are either deterministic-by-total-order (`min_cost_k`) or a loud error (the epoch cap) — never a silent partial-order truncate. Consistent options: **(a) cap = loud error on overflow** (recommended; mirrors the divergence-cap philosophy), or (b) require the registration to also supply a total tie-break so truncation is canonical — which collapses back toward `min_cost_k`. The cap cannot simply be dropped: an uncapped antichain store is unbounded (O(n) scan per insert, O(n²) total), so some resource guard is mandatory.
- **Q2 — the law and the probe.** Strict partial order (transitive + irreflexive, hence asymmetric) *is* sufficient for the uncapped per-store fold to be order-insensitive over the surfaced candidate set — write that argument down in the implementation spec. But: (i) structural-equality dedup is a hard engine-side requirement the bool closure cannot express; (ii) the R0b idempotence probe re-applies `MeetAggrObj::update` and **cannot check a comparator contract** — the new probe checks irreflexivity (`!dom(x,x)`) and asymmetry (`dom(a,b) ⇒ !dom(b,a)`) on encountered pairs; transitivity is not cheaply probeable and stays a documented caller's law; (iii) the "narrower agent wins ties" lexicographic tie-break is *one more clause of the closure* only if composed correctly — composing comparisons lexicographically can silently break transitivity (e.g. when the primary is not a total preorder); the spec must give the safe composition recipe; (iv) termination is "finite surfaced candidates within the epoch cap" — a recursive payload-growing program with many incomparable candidates grows the store without bound, same approximate-semantics caveat `min_cost_k` documents; (v) the registered dominance receives **only the aggregated operand** — the packed candidate must carry every field the predicate inspects; there is no side-channel to other columns or relations (A.1 illustrates the discipline).
- **Q3 — delivery surface (inverts the original's deferral logic).** Host-closure registration is **Rust-embedded only** — unreachable from PyPI `mnestic`, `langchain-mnestic`, `llama-index-vector-stores-mnestic`, and the `cozo-bin` server (the semirings spec records the same limit for R0b: "registration is host-API/Rust-only"). A Python-callable dominance closure means per-pair FFI callbacks inside the fixpoint loop (GIL, purity, perf) — significant unscoped work. For bound/served consumers, the *deferred* in-language dominance (a 2-ary Datalog relation the aggregate consults — a genuinely new capability: an aggregate reading a derived relation mid-fixpoint) is the only route. v1 position: **Rust host closure, with the delivery limitation stated in the registration docs**; in-language dominance stays deferred with its trigger renamed honestly — *the first bindings/server consumer*, not "dominance must depend on derived facts."
- **Q4 — contracts.** Inherit the R0b contract set wholesale and say so: builtin names reserved, lowercase-identifier names, duplicate registration errors, custom aggregates rejected in trigger scripts, factories infallible/cheap/no-panic (no `catch_unwind` in cozo-core), registry process-scoped — persisted antichain *outputs* are ordinary rows, readable after reopen without re-registration; recompute errors loudly. New to state: **nothing fingerprints a registered algebra** — re-registering a *different* dominance under the same name silently changes what a subsequent `:reconcile` diff of persisted antichain output means. Document as the caller's versioning obligation (name your algebras like schemas: `antichain_authority_v2`). Also state the non-recursive path up front: `AggrKind::BoundedMeet` is not recursion-conditional — `min_cost_k` already runs through the same store in plain non-recursive rules (the pinning test's `chain` rule is non-recursive), so the antichain needs no normal-path adapter analogue; the dedicated spec should say so rather than leave it to inference.

**Attribution is out of v1 (removed from the original's A.1).** "Each survivor returned with the clause that failed to separate it" requires a labeled-verdict channel and a pairwise output shape that neither the closure (a bool) nor the aggregate (a set) can produce. The honest composition available today: run the antichain for the survivor set, then author ordinary pairwise CozoScript over the survivors' packed fields to compute per-clause comparisons — user-authored, not engine-computed.

**NOT in scope:** any non-partial-order "dominance" (cycles surface as the 4096-epoch error — loud, but late; the probe catches asymmetry violations earlier); attribution (above); in-language dominance (Q3, deferred); making the epoch cap configurable (its own deferred spec item).

**Disposition.** Build-worthy and on-wedge, but it is the one genuine engine feature in this note: it gets its **own spec** resolving Q1–Q4 with the same review-and-sign-off cycle the companions went through, and it is a natural **contributor-owned** feature (well-bounded, mechanism-only, no cognitive vocabulary). Do not start from "swap the prune." **That spec now exists and is IMPLEMENTED: [`antichain-bounded-meet.md`](antichain-bounded-meet.md) (2026-07-04 — Q1–Q4 resolved as its §8 decisions, signed off, built the same day; `register_bounded_meet_aggr`, Rust-embedded-only v1).**

### 3.2 Scoped `complete` — "absent = unknown" vs "absent = known-complete". Zero engine change, now validated.

**Why.** Negation-as-failure and aggregation are sound only over a relation whose extension is *known complete* in the relevant scope. Every Datalog-with-negation consumer meets this closed/open-world boundary; it is not TMS-specific. `:reconcile` supplies it at whole-relation granularity (§2). The generic residue: (a) **partition-scoped** completeness, (b) a **query-time predicate** so a rule body can branch on it.

**Minimal cut — a documented pattern on shipped primitives (✓ validated end-to-end).** Model completeness as an ordinary **persistent, tt-stamped** witness relation with **flat scope columns**, and read it with `not`:

```
:create complete_witness {rel: String, topic: String, tt: TxTime => note: String}
```

```
?[rel, topic, note] <- [["supports", "outage_2026_07", "all sources ingested"]]
:put complete_witness {rel, topic => note}
```

(Two separate scripts — a schema op and a query op don't stack in one plain script; a later `:put` would silently displace the `:create`.)

```
explained[c] := *claim[c, _], *supports[_, c, _]
blocked[c]   := *claim[c, t], not explained[c],
                *complete_witness{rel: "supports", topic: t}
open[c]      := *claim[c, t], not explained[c],
                not *complete_witness{rel: "supports", topic: t}
?[c, status] := explained[c], status = "explained"
?[c, status] := blocked[c], status = "blocked"
?[c, status] := open[c], status = "open"
```

That is the entire three-valued distinction (`blocked` = known-absent, `open` = unknown), and because the witness is tt-stamped, **"believed complete as of T" is already a shipped as-of read** — no promotion needed:

```
?[rel, topic] := *complete_witness{rel, topic @ (tt: $t0)}   # per-atom …
# … or pin the whole three-valued program:  :as_of $t0       # ✓ validated: flips blocked → open
```

> **⚠ The two traps the first draft fell into — both engine-verified, both fatal:**
> 1. **Never name the witness with a leading underscore.** `_`-prefixed relations are *transaction-scoped temps* (`data/symb.rs:82-84`; fork known-issue #2): `:create _complete` + `:put` succeeds and silently vanishes at commit, and every later script either errors relation-not-found or — if it re-creates the temp — reads an always-empty witness, silently degrading every `blocked` to `open`. The fork additionally hard-rejects `TxTime` on `_`-relations at `:create` time (`runtime/relation.rs:427-429`).
> 2. **Key the scope with flat columns, not a Json map.** A map literal in atom position is an equality probe against one constructed value — `{topic}` shorthand does not parse, and there is no destructuring that binds a variable *from* a stored map. Per-field access on a Json column (`get(scope, 'topic') == t`) is a post-filter the fork's equality pushdown does not convert to a keyed lookup. Flat columns are prefix-seekable and read naturally as `{rel: "supports", topic: t}`.

**Coexistence with `:reconcile` — a rule, not a vibe.** These are two unlinked sources of known-absence with a destructive failure mode: a whole-relation `:reconcile` run against a partially-covered relation *retracts rows in scopes that are still OPEN* (its diff treats absence-from-output as cessation, everywhere), and witness rows are invisible to it. v1 rule: **a relation is either reconcile-managed (relation-granular completeness) or witness-scoped (partition completeness) — never both.** The pipeline picks per relation and records the choice.

**Medium cut — scoped `:reconcile` (deferred; the real one).** The original's medium cut (bless the witness as a system relation) is withdrawn — the validated minimal cut already delivers its as-of benefit, leaving only naming ergonomics. The medium cut that actually honors "scoped completeness is `:reconcile` made readable" is a **scope-filtered `:reconcile`**: retract only within the stated scope, auto-assert the witness row in the same belief event. Machinery to reuse exists (the internal prefix-scoped bitemporal scan, `runtime/relation.rs:759`); surface and semantics need their own design note. Trigger: a second consumer, or the first incident of the mixed-mode failure above.

**Maximal cut — three-valued NAF (deferred, unchanged).** Making `not p[x]` itself yield *unknown* absent a completeness witness touches stratification and every existing `not` — wide and risky for narrow extra value. Defer until a consumer needs the *engine*, not the library, to enforce it.

**NOT in scope:** deriving completeness (it is an external pipeline claim — coverage probes, exhausted-source witnesses — never recomputable from the graph); automatic invalidation when a scope's completeness flips (truth-maintenance pull, same class as R3's deferred incremental half).

### 3.3 Structured query parameters — RESOLVED: already shipped; the deliverable is this section.

The original asked for a reality check before building ("do not build a feature that is one built-in function away from already existing"). The check came back **stronger than its floor: there is nothing to build.**

**What ships today (✓ all validated):** query params arrive as `BTreeMap<String, DataValue>` (the params argument to `run_script` in `runtime/db.rs`); a JSON-object param becomes `DataValue::Json` and is inlined at parse time as a constant (`parse/expr.rs:186-200`). Field access has two shipped spellings, plus the as-of wiring:

```
# $ctx = {"viewer": "auditor", "asof": 1780500000000000}
?[v] := v = get($ctx, 'viewer')      # errors on a missing key (the `op_get` Json branch
                                     # in data/functions.rs; returns plain scalars)
?[v] := v = $ctx->'viewer'           # infix projection, tightest precedence (OP_MAYBE_GET,
                                     # cozoscript.pest:120); returns null on a missing key
?[rel, topic] := *complete_witness{rel, topic}
:as_of get($ctx, 'asof')             # the structured param drives the bitemporal read
```

(A menu of three separate scripts, not one program — the `?` entries would clash if pasted together.) Caveats to document, not fix: the `->` right-hand side must be a quoted string (`$ctx->viewer` parses `viewer` as a variable); use `get()` when a missing key should be an error.

**The trap — and why dot-projection is rejected, not deferred.** `$ctx.viewer` **parses today** as a single parameter literally named `ctx.viewer` (dots are legal inside the param token, `cozoscript.pest:63`) and fails with param-not-found under a `$ctx` map. So dot-projection would be a *breaking re-interpretation of currently-legal syntax* — any host already passing dotted param names silently changes meaning — not additive sugar. And the exact dotted spelling is available today with zero engine change by host-side flattening (`{"ctx.viewer": "auditor"}`). _Rejected:_ dot-projection grammar change (breaking, redundant with `->` and flattening).

**Explicitly out — the σ-view trap (unchanged).** "Auto-filter every relation by `$ctx.viewer` under a visibility preorder" is policy, not engine. The engine's job ends at passing a structured param and projecting fields; the `visible[...]` filter is authored by the consumer as an ordinary predicate (A.3).

### 3.4 Valid-time interval primitives — v1 SHIPPED 2026-07-04; MTL stays out.

**The blocker the original skipped.** Shipped valid time is point-event `Validity`, not intervals (§2). `coalesce(vt)` as originally sketched had no interval input to merge — each bound `vt` is one (timestamp, flag) point. Three candidate representations, now dispositioned:

1. **Flat `[start, end]` span columns — CHOSEN for v1 (✓ the pattern is validated today).** Spans are ordinary data; the primitives are honest *list/value utilities*, deliberately decoupled from the vt axis, `@`-reads, and the skip-scan. This matches the anti-overbuild rule: universally wanted, storage-adjacent, no new types.
2. **A new Interval `DataValue`** — _rejected:_ a storage-type change (memcmp encoding, coercions, ordering) far bigger than "two ordinary functions", and the same shape `bitemporality.md` §12 already rejected for the tt axis (interval-column rewrites / interval trees are the wrong shape for a memcmp-keyed store).
3. **Deriving spans from Validity assert/retract event pairs** — _deferred with its open questions recorded:_ which rows feed it (a raw no-`@` scan includes retract rows and, on bitemporal relations, superseded tt-correction rows — stale beliefs would pollute spans; an as-of read collapses to one winning row per key — nothing left to merge), and per-key event-pairing is an order-sensitive windowing step, not a value fold. Revisit when a consumer wants engine-derived validity spans rather than app-recorded ones.

**v1 scope — SHIPPED 2026-07-04, both primitives engine-pinned (✓):**

- `interval_overlaps(a, b)` over `[start, end)` lists — shipped as a plain builtin (`op_interval_overlaps`, `data/functions.rs`; half-open semantics — touching intervals do not overlap, and an empty span `[x, x)` overlaps nothing, a case the textbook `s1 < e2 ∧ s2 < e1` test gets wrong; malformed or NaN spans are loud errors, never silent falses; mixed int/float bounds compare **numerically**, not by `Num`'s storage order, under which `Int(5)` and `Float(5.0)` are never equal). The rest of Allen's relations stay unbuilt until pulled, as thin skins over one generalized interval-transform (Vadalog's own lesson: the operator surface is ergonomics, not the semantic core).
- **`interval_coalesce(span)`** — shipped as an ordinary list-returning aggregate (`AggrIntervalCoalesce`, `data/aggr.rs`; precedent: `collect` normal, `union` meet): merges the group's overlapping **and adjacent** spans into maximal intervals (`[0,5)` + `[5,10)` = `[0,10)`); equal-valued grouping rides the rule head's other columns; malformed spans error. **Renamed from the original's `coalesce`,** which collides with the shipped variadic null-coalescing builtin and its `~` operator (`data/functions.rs:287-295`). The aggregate namespace is technically separate from the function namespace, which makes the collision *silent* rather than impossible — one token, two unrelated semantics by position, and SQL-trained readers will assume null-coalescing. Follow-on hardening (SHIPPED 0.10.1): `register_custom_aggr`'s name-reservation guard now reserves builtin *function* names too (a `get_op(&name).is_some()` bail, mirrored in `register_bounded_meet_aggr`, both in `runtime/db.rs`), pinned by the registration-policy case in `tests/antichain_bounded_meet.rs`.

The pre-existing zero-build pattern (✓ A.4): spans as columns, overlap as a plain predicate. The primitives are packaging over it, which is exactly why they were small.

**NOT in scope (unchanged):** metric/MTL operators (`since`/`until`/`◆`/`◇`); anything that turns the store into a temporal reasoner. The prior-art split stands verified: `provenance-semirings.md`'s prior-art note flags Temporal Vadalog / DatalogMTL as "Unitemporal — no transaction time" and records the interval-transform + since/until-second-class lessons — that whole line stays the reasoning layer *above* mnestic.

### 3.5 Surface DDL for algebra selection — deferred, trigger unchanged, example fixed.

0.10.0 chose host-API registration and left query-language syntax as "a later, separate decision." In the shipped model the algebra is named by the aggregate and ⊗ is body arithmetic, so a block-level `@using(algebra){…}` selector buys nothing yet. **Trigger to build it:** the first program needing two engine-owned algebras at once — most plausibly an antichain *of* `min_cost_k` proofs once §3.1 lands. Note the composed program (A.5) needs the aggregate operands threaded explicitly (`pack` bound in the body, the antichain aggregating the pack) — the original's "they compose without rule rewrites" oversold it; they compose without *rewriting the recursion*, which is the claim that matters. Until then, the aggregate name is the algebra selector, and that is enough.

---

## 4. Priority & sequencing — re-derived after verification

The original ranked §3.1 first on a cost premise ("adds one parameter to an existing slot") that verification refuted. Re-derived under the same formula (leverage × generality ÷ cost):

1. **§3.3 — ship now, as documentation.** Zero build; the surface exists (including `->` and `:as_of get($ctx, …)`). Cost ≈ this section + the pinning test.
2. **§3.2 — ship now, as documentation.** Zero engine change; the validated witness pattern + the reconcile-coexistence rule. Most load-bearing per unit cost of anything here.
3. **§3.4 v1 — SHIPPED 2026-07-04.** `interval_overlaps` + `interval_coalesce` over `[start, end)` lists; rename settled; no new types.
4. **§3.1 — SHIPPED 2026-07-04** via its own spec ([`antichain-bounded-meet.md`](antichain-bounded-meet.md)): Q1–Q4 resolved, signed off, and built — `register_bounded_meet_aggr(name, dominates, max_survivors)`, Rust-embedded-only v1, §6 matrix pinned.
5. **§3.5 — deferred**; revisit on the named trigger.

The corrected through-line: three of the five items generalize shipped machinery with little or no engine code — and the discipline held *because* verification caught the two places the note claimed that without it being true. §3.1 re-opens no shipped subsystem, but it is a new store core behind a shipped gate and is budgeted as such.

## 5. Prior art

- **Börzsönyi, Kossmann, Stocker — *The Skyline Operator* — ICDE 2001** — the non-dominated-set (Pareto frontier) operation §3.1 generalizes `min_cost_k` into.
- **Green, Karvounarakis, Tannen — *Provenance Semirings* — PODS 2007** — the shipped substrate §3.1–§3.2 extend.
- **Li, Huang, Naik — *Scallop* — PLDI 2023** — bounded/top-k proofs and the `(T, 0̄, 1̄, ⊕, ⊗, ⊖, ○=)` interface whose bounded-meet slot §3.1 opens to registration (attribution verified against `provenance-semirings.md` §3).
- **Bellomarini, Sallinger et al. — *Temporal Vadalog / DatalogMTL*** — the valid-time interval-reasoning complement §3.4 leaves above the engine; source of the interval-transform and since/until-second-class notes (verified verbatim in `provenance-semirings.md`'s prior-art entry, including "Unitemporal — no transaction time").
- **Abiteboul, Hull, Vianu — *Foundations of Databases*** — stratified negation / the closed-world boundary §3.2 makes scope-explicit.

---

## Appendix A — worked examples (evidence graph)

A non-TMS running example in the register the semirings note already uses: sources assert claims, claims support claims, everything auditable. Blocks marked **✓** run today and are pinned by `cozo-core/tests/spec_doc_validation.rs`; blocks marked **✳** use the §3.1 antichain, which runs once the host registers it (`register_bounded_meet_aggr` — Rust-embedded only; pinned separately in `cozo-core/tests/antichain_bounded_meet.rs`). One `:create` per script (schema statements don't stack in a single plain script); comments are `#` (CozoScript; `%` is the mod operator).

Base schema (✓):

```
:create claim    {id: String => topic: String}
:create supports {src: String, dst: String => weight: Float}
:create complete_witness {rel: String, topic: String, tt: TxTime => note: String}
```

### A.1 — antichain (§3.1 ✳): surface contested claims, don't silently pick one

```
# ✓ shipped: cheapest supporting source per cause — pack bound in the body
answer[cause, min_cost(pack)] := *supports[src, cause, w], pack = [src, -ln(w)]

# ✳ PROPOSED: survivors under a registered dominance over the aggregated entry
#   (x dominates y ⇔ x.cost < y.cost − ε; costs within ε survive together as contested.
#    The closure sees ONLY the aggregated operand — entry must carry every field it
#    inspects, §3.1 Q2(v); here entry = [cause, [src, cost]], so cost is available.)
surviving[antichain(entry)] := answer[cause, pack], entry = [cause, pack]
```

Singleton → **resolved**; cardinality > 1 → **contested**. Contrast `min_cost_k(pack, 1)`, which always returns one and hides the tie. Per-pair "which clause failed to separate them" is **not** part of this feature (§3.1) — author it as ordinary pairwise rules over the survivors if needed.

### A.2 — scoped completeness (§3.2 ✓): OPEN vs BLOCKED

The full validated program is in §3.2. The reading: `explained` non-empty → resolved; `blocked` → every source for this topic was ingested and nothing was found — an honest dead-end; `open` → nobody ever claimed exhaustion — *unknown*, the distinction a closed-world reading erases. Flip a claim from OPEN to BLOCKED with one `:put` to `complete_witness`; ask "was it blocked *last Tuesday*?" with `@ (tt: …)` or `:as_of` — no extra machinery.

### A.3 — structured params (§3.3 ✓): one scoped read, policy stays in the query

```
:create source {src: String, tt: TxTime => trust: Float}
```

```
# $ctx = {"asof": 1780500000000000}
visible[s] := *source{src: s, trust}, trust >= 0.7     # the consumer's policy, authored here
?[c] := *supports[s, c, _], visible[s]
:as_of get($ctx, 'asof')     # pins the tt-stamped source; supports is plain, so the
                             # query is only partially reproducible (bitemporality.md §4)
```

`source` is deliberately tt-stamped: `:as_of` errors, by design, on a block that references no tt-stamped relation (the typo guard, `data/program.rs:660-672`). The engine's contribution ends at Json params + `get()`/`->` + `:as_of`; no access model is baked in.

### A.4 — intervals (§3.4 ✓): spans as data, primitives shipped

```
:create fact_spans {entity: String, span_start: Int, span_end: Int => value: String}
```

```
# ✓ today: [start, end) spans as ordinary columns; overlap as a plain predicate
?[a, b] := *fact_spans{entity: e, span_start: s1, span_end: e1, value: a},
           *fact_spans{entity: e, span_start: s2, span_end: e2, value: b},
           a != b, s1 < e2, s2 < e1

# ✓ shipped (§3.4 v1, 2026-07-04): the same, packaged
conflict[a, b] := *fact_spans{entity: e, span_start: s1, span_end: e1, value: a},
                  *fact_spans{entity: e, span_start: s2, span_end: e2, value: b},
                  a != b, interval_overlaps([s1, e1], [s2, e2])
held[e, v, interval_coalesce(span)] := *fact_spans{entity: e, span_start: s, span_end: t, value: v},
                                       span = [s, t]
```

`since`/`until` stay in the reasoning layer above mnestic — deliberately not here.

### A.5 — composition: "the contested answers, each with its evidence chain, as of T"

```
:create supports_b {src: String, cause: String, tt: TxTime => w: Float}
```

```
# ✓ shipped + validated: top-k evidence chains over a tt-stamped relation, reproducible as of T
chain[cause, min_cost_k(pack, 3)] := *supports_b{src, cause, w},
                                     pack = [[src, cause], -ln(w)]
?[cause, pack] := chain[cause, pack]
:as_of $t1                     # pins every tt-stamped atom in the block; explicit selectors win

# ✳ PROPOSED (§3.1): the survivors among those chains
surviving[antichain(entry)] := chain[cause, pack], entry = [cause, pack]
```

Two of the three readings ship today (top-k proofs; as-of reproducibility — validated both directions: as-of after commit returns the chains, as-of before returns nothing). The third (the antichain) is §3.1. Composition requires threading the pack operand — no recursion rewrite, which is the claim that matters. Reproducibility spans exactly the tt-stamped relations in the block; a query mixing in plain relations is only partially reproducible (`bitemporality.md` §4).

---

## Changelog

| Date | Change |
|------|--------|
| 2026-07-04 | First authoring (external design note): re-sorts the "missing from mnestic" review against the 0.10.0 baseline into four residue items + one deferred ergonomic. |
| 2026-07-04 | **Verification + revision pass** (panel review against shipped code, adversarial refutation, engine-validated listings — `cozo-core/tests/spec_doc_validation.rs`). §3.3 resolved as already-shipped (incl. `->` and `:as_of get($ctx,…)`); §3.2 listings corrected (persistent tt-stamped witness, flat scope columns; underscore + map-destructuring traps documented; reconcile-coexistence rule added; medium cut re-pointed at scoped `:reconcile`); §3.1 re-costed (honest touch-list; Q1 cap confluence, Q2 law/probe, Q3 delivery surface, Q4 contracts; attribution claim removed; needs own spec); §3.4 representation decided (flat `[start,end]` columns v1; `interval_coalesce` rename over the `coalesce` builtin collision; Validity-derived spans deferred with open questions); §1 accounting fixed (row 6 clarification pending, row 7 reworded to convention, §3.4 marked net-new); §4 priority re-derived (inverts); per-atom selector syntax corrected to the in-brace form throughout. A post-revision verification round (coverage / citations+listings / consistency agents) then caught and fixed: A.3 rewritten against a tt-stamped `source` and pinned (the draft would have tripped the `:as_of` typo guard, `data/program.rs:660-672`), A.3/A.4 schemas printed, fence-stacking traps split (§3.2, §3.3 noted), guardrail-box claim rescoped, `:reconcile` row dual-phrased, Q2(v) operand-only-visibility + Q4 non-recursive-path contracts added. |
| 2026-07-04 | **§3.4 v1 shipped**: `interval_overlaps` builtin (`op_interval_overlaps`) + `interval_coalesce` aggregate (`AggrIntervalCoalesce`) over half-open `[start, end)` list intervals; touching spans don't overlap but do coalesce; malformed spans are loud errors. A.4's proposed blocks flipped to ✓ and engine-pinned (incl. edge semantics + error cases). §4 item 3 closed; only §3.1 (own spec first) and §3.5 (deferred) remain open. |
| 2026-07-04 | **§3.1 dedicated spec authored**: [`antichain-bounded-meet.md`](antichain-bounded-meet.md) — Q1 resolved as mandatory `max_survivors` + loud error (no truncation; literature: bounded skylines are semantic reductions, never arrival-order truncation), Q2 as BNL insert + irreflexivity/asymmetry probes + strict-weak tie-break recipe, Q3 as Rust-embedded-only v1 with stated limitation, Q4 as R0b contracts + algebra-versioning obligation. PROPOSED — build gated on owner sign-off. |
| 2026-07-04 | **§3.1 signed off and SHIPPED**: `register_bounded_meet_aggr` + `DominanceMeetStore` landed per the dedicated spec (build deviations recorded there). Every §4 item is now closed except §3.5 (deferred, trigger unchanged). A.1/A.5's ✳ blocks run under a host registration. |
