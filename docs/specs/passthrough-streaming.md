# Spec — Passthrough-Rule Streaming into FixedRule Inputs: eliminate the copy of a bare stored-relation rule (the direct-feed rewrite)

_Created 2026-07-10. Status: **PROPOSED — awaiting owner sign-off; do not build before §11's decisions are signed.** This is the spec for the passthrough-streaming item on the public [`ROADMAP.md`](../../ROADMAP.md) — the cheaper of the two benchmark-earned perf items, banked alongside the 0.12 budgeted-traversal headline. It resolves five questions the item raises: Q1 default state, Q2 what shapes qualify, Q3 temporal-selector handling, Q4 multi-consumer semantics, Q5 observability/guarding. Grounded two ways: (a) against the shipped compile/eval pipeline — citations are file:line in `cozo-core/src/`, gathered and spot-verified 2026-07-10; (b) against the measured D3 benchmark that earned it (the same-box canned-graph-algorithm comparison recorded against `CHANGELOG-FORK.md`'s 0.11.0 graph-projection numbers). Companion to [`join-reorder.md`](./join-reorder.md) and [`factorize.rs`](../../cozo-core/src/query/factorize.rs) (the two shipped whole-program planner passes this one mirrors) and [`antichain-bounded-meet.md`](./antichain-bounded-meet.md) (the spec-discipline template)._

> **Anti-overbuild guardrails.** One new whole-program compiler pass, mirroring `factorize::maybe_rewrite_and_advise` in slot and signature; **zero execution-side changes, zero new IR, zero grammar changes.** The FixedRule execution path *already* streams a stored relation directly — `FixedRuleInputRelation::iter` dispatches `Stored` → `scan_all`/`skip_scan_all`/`bitemporal_scan_all` (`fixed_rule/mod.rs:99-114`, verified 2026-07-10) — so the entire optimization is: recognize a rule whose body is a bare stored-relation atom feeding a FixedRule input, and rewrite that one `FixedRuleArg` from `InMem{rule}` to `Stored{relation}`. No new operator, no CSR-builder change, no stratifier change beyond what falls out of the rewritten dependency graph. The only *additive* surface is an optional `::explain` annotation so the optimization is testable (§9). Budget it as a **contained planner pass on entirely-shipped scaffolding** — smaller than the graph projection, smaller than the join reorder. Default **OFF** until the planner regression suite exists to guard it (§8, the 0.10.5→0.10.7 lesson applied pre-emptively).

---

## 1. Why / what this buys

A rule whose body is a single bare stored-relation atom — `edges[a, b] := *knows[a, b]` — is materialized into a temp store before the FixedRule that consumes it ever runs. The cost is three passes where one would do: (i) a full scan of `*knows` in epoch 0, (ii) a full copy into `edges`'s `EpochStore` (a `RegularTempStore` `.wrap()`ed in), (iii) a second scan of that store when the algo calls `.iter()` (`query/eval.rs:89-108,173-226`; `fixed_rule/mod.rs:91-116`). Feeding the stored relation straight into the FixedRule collapses all three to one direct backend scan.

Measured 2026-07-09 (LDBC SNB sf1: 10,620 persons / 438,900 directed `knows` rows, mem backend, three fresh processes × 9 reps):

| `ConnectedComponents` form | median | vs Ladybug ~251 ms |
|---|---|---|
| `ConnectedComponents(*knows[a,b], *person[x,e])` — direct feed | **~136 ms** | **wins 1.8×** |
| `edges[a,b] := *knows[a,b]` … `ConnectedComponents(edges, nodes)` — lifted | ~282 ms | **loses** |

The lifted form materializes **449,520 rows** into temp stores and runs **~2.1× slower** for byte-identical output. Two consequences the item exists to fix: it is a footgun no user should have to know about (the natural way to write the query is the slow way), and — load-bearing for positioning — **the WCC win against Ladybug is conditional on the direct-feed form.** A reader who writes the natural lifted form measures a loss and concludes we cherry-picked. The rewrite makes the fast form the *only* form, so the win becomes unconditional and quotable without an asterisk. It is ~2.1× on **every** canned graph algorithm, available before any CSR cache exists (and composing with the shipped 0.11.0 projection: a projection consumes a stored-relation source directly, so a passthrough-lifted projection input has the same footgun and the same fix).

Honesty note: mindgraph-rs does not itself call these FixedRules today — it hand-writes traversals (`traverse_reachable`) and `min_cost_k` recursion. This is an OSS-community differentiator and a benchmark-integrity fix, not a MindGraph pull; it fits the wedge (a graph engine whose canned algorithms are not quietly 2× slower when driven the obvious way) and passes the stranger test: *"a rule that is an identity projection of a stored relation should not be copied before it is scanned."*

## 2. Shipped baseline this builds against (verified 2026-07-10)

| Piece | Where | What transfers |
|---|---|---|
| `FixedRuleArg` = `InMem{rule}` \| `Stored{relation, valid_at, tx_valid_at}` \| `NamedStored{…}` — the InMem/Stored distinction is decided **purely syntactically at parse time** (bare name ⇒ `InMem`, `*name` ⇒ `Stored`) | `data/program.rs:402-425`; `parse/query.rs:1001,1039,1087` | the rewrite target — swap one variant for another |
| `FixedRuleApply` survives every compile stage carrying `Vec<FixedRuleArg>` unchanged through NormalForm (`NormalFormRulesOrFixed::Fixed`); the Fixed body is **not** compiled to relational algebra | `data/program.rs:284-293,793`; `query/compile.rs:190-192` | verbatim — at NormalForm the args are still mutable source-level `FixedRuleArg` |
| `FixedRuleInputRelation::iter` **already** dispatches `Stored` → `scan_all` / `skip_scan_all` (validity) / `bitemporal_scan_all` (tt), `InMem` → `store.all_iter()`; `prefix_iter` and `arity` branch identically | `fixed_rule/mod.rs:91-145,1208-1226` | verbatim — **the entire execution side is already done** |
| `factorize::maybe_rewrite_and_advise(prog: NormalFormProgram, enabled) -> (NormalFormProgram, Option<String>)` — a whole-program pass in the exact pipeline slot, behind an `AtomicBool` gate, returning an advisory | `query/factorize.rs:81-84`; pipeline `runtime/db.rs:2816-2828` | as the **pattern to copy** — slot, signature, gate, advisory-row all mirror |
| `enable_factorize: AtomicBool` default `false`, `set_query_factorization`/`query_factorization` toggles, `.load(Relaxed)` at the call site | `runtime/db.rs:187,387,671-679,2822-2824` | as the **pattern to copy** — the kill switch, default OFF |
| Stratification: a Fixed rule's `InMem` arg inserts a dependency edge valued `true` → a **forced stratum boundary** (like negation/aggregation); `Stored`/`NamedStored` insert **no edge** | `query/stratify.rs:142-153` | verbatim — removing the InMem dep is what deletes the boundary and the epoch store; the pass must run **before** `into_stratified_program` |
| `get_downstream_rules` — per-rule consumer discovery (collects `FixedRuleArg::InMem{name}` and `NormalFormAtom::Rule` targets) | `query/magic.rs:292-322` | verbatim — the multi-consumer analysis (§6) reuses this walk |
| CSR builders read edge endpoints **positionally** (`tuple.next()` for from/to, position-2 for weight); binding names are irrelevant to graph construction | `fixed_rule/mod.rs:323-326,381-397` | verbatim — the reason a **reorder is not a passthrough** (§4) |
| `differential_naive_equals_factorized` — the DQP oracle (run both forms, assert bit-identical on mem AND sqlite); `join_reorder.rs` plan-shape assertions (`load_refs`, `plan_sig` over `::explain`) | `tests/factorize.rs:13-15`; `tests/join_reorder.rs` | as the **pattern to copy** for §10 |

**What does NOT transfer: the recognition predicate.** No shipped pass matches "a single-rule ruleset whose sole body atom is a bare identity-projection stored relation, feeding a FixedRule input, with no other blocking consumer." That predicate (§3.1) and the per-arg rewrite (§3.2) are the genuinely-new work. Everything they stand on is shipped.

## 3. Design

### 3.1 The recognition predicate

A rule `R` in the `NormalFormProgram` is a **streamable passthrough** iff all hold (fields per `data/program.rs:961-981,1968-1975`):

1. `R`'s ruleset is exactly one `NormalFormInlineRule` (a disjunction is split into multiple rules by normalization — more than one ⇒ a union, not a passthrough).
2. `aggr` is all `None` (no aggregation).
3. `body` is exactly one atom, and it is `NormalFormAtom::Relation(NormalFormRelationApplyAtom)` — **not** `Rule`, `NegatedRelation`, `HnswSearch`, `FtsSearch`, `LshSearch`, `Predicate`, or `Unification`.
4. The atom's `args` are **distinct variables** — no repeated variable (a self-join filter: `*knows[a,a]`) and no constant (a selection filter: `*knows[a, 5]`).
5. **Identity projection**: `head` equals the body atom's `args`, elementwise in the same order (rename of the head *names* is fine — names do not reach the positional CSR; **reorder is not**, §4).
6. **Arity match**: the stored relation's arity equals the head arity — no truncation (`edges[a,b] := *knows[a,b,_]` drops a column that a *weighted* algo would read as the weight, §4).
7. **v1 conservative gate (Q3)**: `valid_at == None && tx_valid_at == None` — no temporal selector (the execution path supports temporal streaming; §5 defers it deliberately, not because it cannot work).

An `R` satisfying 1–7 is a **candidate source** for any FixedRule input that references it by `InMem{R}`.

### 3.2 The rewrite

The pass is `NormalFormProgram → NormalFormProgram`, run in the `factorize` slot (`runtime/db.rs:2822`, **before** `into_stratified_program`). For each FixedRule application in `prog`, for each `rule_args[i] == FixedRuleArg::InMem{R}` where `R` is a candidate source:

1. **Swap the arg**: `rule_args[i] := FixedRuleArg::Stored{ name: R's stored relation, bindings: the atom's args, valid_at: None, tx_valid_at: None }`. (`NamedStored` sources are lowered to positional `Stored` in adorn downstream, `magic.rs:392-447` — the pass may emit either; emitting `Stored` in storage order is simplest and 3.1(5)+(6) guarantee the order and arity are already right.)
2. **Recompute `R`'s consumers** (the `get_downstream_rules` walk, `magic.rs:292-322`) over the *rewritten* program. If `R` has zero remaining references, **dead-rule-eliminate** it (drop from `prog.prog`) — its store is never built. If `R` still has consumers (another rule, or another FixedRule input the pass declined to rewrite), **leave it in** `prog` — it keeps materializing for them, unaffected.

Both branches are safe because a stored relation is a stable set independent of rule evaluation within the transaction: reading `*knows` directly N times, or once into a store then N times from the store, yields the same tuples in the same storage order (the equivalence §10 pins). The rewrite is strictly per-arg — one algo input can stream while another input of the same algo, or another consumer of the same rule, keeps its materialized form.

### 3.3 Determinism & equivalence

There is nothing schedule-dependent here — the pass is a pure syntactic rewrite over a deterministic IR, and the *output* of the query is provably identical to the un-rewritten form because both read the same stored relation through the same `SessionTx` against the same pinned snapshot. The only observable differences are performance and the disappearance of `R`'s materialization (which §9 makes assertable). Storage-order note: `scan_all` returns columns in storage order (keys then non-keys), and 3.1(5)+(6) guarantee the materialized store `R` had that identical column order and arity — so the CSR built from the streamed relation is byte-identical to the CSR built from the store, not merely equivalent up to relabeling.

## 4. What qualifies (Q2 — resolved: identity projection over all columns, nothing else)

The recognition predicate is deliberately narrow, and each exclusion is a correctness requirement, not conservatism:

- **Reorder is excluded and must be rejected**, not materialized-around: `edges[b,a] := *knows[a,b]` materializes a store whose position-0 column is `knows`'s *second* column; feeding `*knows` directly gives position-0 = `knows`'s first column = **reversed edges = wrong graph**. The CSR builders read endpoints positionally (`fixed_rule/mod.rs:323-326`) and `scan_all` cannot permute, so a reorder cannot be pushed into how the algo reads columns. The pass simply does not fire (3.1(5) fails); the rule materializes as today. _Rejected:_ supporting reorder via a column-permuting wrapper iterator — real future work, but it is a new execution-side mechanism, exactly what this pass's anti-overbuild line excludes.
- **Arity truncation is excluded**: `edges[a,b] := *knows[a,b,_]` is harmless for unweighted CC (the extra column is never read) but a *weighted* algo reads position-2 as the edge weight, whereas the materialized arity-2 store defaults the weight to 1.0. Because the pass is algo-agnostic (it rewrites the arg before the algo's weight-reading behavior is known), it must require full-arity identity (3.1(6)).
- **Filters, self-joins, multi-atom bodies, aggregation, rule/search atoms** are all excluded by 3.1(2)–(4): any of them means the rule computes something the stored relation does not already contain, so the copy is doing real work and is not a passthrough.

**Decision: identity projection over all columns of a single stored-relation atom, distinct vars, no filter, no aggregation, no temporal selector (v1).** _Rejected:_ the broader "any projection expressible as a column subset/permutation" (needs the permuting iterator — deferred with trigger, §7); firing on truncation for unweighted algos only (algo-specific behavior in an algo-agnostic pass — a footgun worse than the one it fixes).

## 5. Temporal sources (Q3 — resolved: execution supports it, v1 defers it behind the same predicate)

Unlike graph *projections*, which **refuse** transaction-time sources (`runtime/graph_projection.rs:832-841`, `ProjectionTxTimeSourceError` — a projection outlives the transaction and would cache the raw history keyspace), the direct-feed path resolves the temporal read **inside** the transaction: `FixedRuleInputRelation::iter` already dispatches `tx_valid_at` → `bitemporal_scan_all`, `valid_at` → `skip_scan_all` (`fixed_rule/mod.rs:99-114`), and the selectors are encoded at adorn time (`encode_temporal_for_fixed_rule`, `magic.rs:32-47`). So `edges[a,b] := *knows[a,b] @ 'NOW'` *could* be streamed: the synthesized `Stored{valid_at, tx_valid_at}` carries the atom's selectors and runs the same scan whether the atom is read inside a rule body or fed directly. This matters for the wedge — **temporal edge relations cannot be projected (0.11.0), so the direct-feed stream is the *only* way a bitemporal graph reaches a canned algorithm fast.**

v1 nonetheless gates to non-temporal (3.1(7)) for one reason: the equivalence proof (materialized-then-scanned vs directly-scanned) must confirm the temporal resolution timing is identical — both resolve against the same pinned snapshot through the same `SessionTx`, which it is, but that is a claim the test matrix should establish before the pass fires on it. **Decision: v1 requires `valid_at == None && tx_valid_at == None`; the temporal extension is deferred with a named trigger** — *the first consumer that feeds a temporal edge relation to a canned algorithm and measures the lifted-form penalty* (the bitemporal-graph-analytics case). _Rejected:_ streaming temporal sources in v1 (correct but unproven — ship the non-temporal equivalence first, then extend with its own test rows); blocking temporal permanently (it is the one case where this pass is not merely an optimization but the only fast path).

## 6. Multiple consumers (Q4 — resolved: per-arg rewrite, keep-or-eliminate by remaining references)

Answered in §3.2 and grounded: consumers are discoverable structurally (`get_downstream_rules`), the rewrite is per-`FixedRuleArg`, and the keep-vs-eliminate decision is a post-rewrite reference count. A rule consumed by both a FixedRule and an ordinary rule streams to the algo and stays materialized for the rule; a rule that is the algo's sole consumer is deleted and never built. **Decision: as §3.2.** _Rejected:_ firing only when the rule has exactly one consumer (leaves the common "one algo input + one debug `?[...]` echo" case unoptimized for no reason — the per-arg rewrite handles it); tracking a materialization refcount as new state (`get_downstream_rules` already computes it on demand).

*Precision note:* the retained rule, if kept for other consumers, is magic-rewritten and join-reordered normally for *those* consumers (`magic_rewrite` passes Fixed rulesets through untouched, `magic.rs:98-102`; the retained ordinary rule optimizes independently) — the streamed algo arg and the retained rule do not interact.

## 7. What is deliberately NOT in v1

Column-subset/permutation passthroughs (needs a permuting wrapper iterator — new execution mechanism; trigger: a measured consumer whose natural query reorders columns into an algo, once the base pass proves out); temporal-source streaming (§5, execution-ready, deferred behind the equivalence proof; trigger stated there); passthrough of a *rule* input that is itself a passthrough chain `a := b; b := *c` (transitive collapse — possible but the two-hop case is rare and the single-hop case captures the measured D3 win; trigger: a real chain in a consumer); firing on non-FixedRule consumers of a passthrough rule (ordinary rule-to-rule passthroughs are a different and larger optimization the join machinery partly handles already — out of scope by the item's framing); default-ON (§8, gated on the regression suite).

## 8. Default state & guarding (Q1 — resolved: OFF until the planner regression suite exists)

The 0.10.5→0.10.7 lesson is the whole reason this is a decision and not an afterthought: a default-on planner heuristic (the greedy join reorder) shipped a 120 s pathological plan, and a *field-tester* found it, not our tests. This pass is lower-risk than the reorder — it is a syntactic rewrite whose output is provably identical, with no plan-cost search that could pick a bad order — but "provably identical output" is exactly what the factorized `!=` rewrite also believed before it miscounted on Int/Float. So it ships behind an `AtomicBool` kill switch (`enable_passthrough_stream`, default `false`, `set_/query_` toggles, mirroring `enable_factorize` — `runtime/db.rs:187,387,671-679`) and **stays off by default until the planner regression suite (on the [`ROADMAP.md`](../../ROADMAP.md) near-term list) is standing to guard it.** The suite is the same gate that unblocks factorization-default-on and the `!=` restore; this pass joins that queue. The DQP oracle (§10) is a necessary but not sufficient condition — it proves correctness on the fixtures it contains; the regression suite proves no *shape* regresses at scale.

**Decision: gate behind a default-OFF `AtomicBool`; flip to default-ON only after the planner regression suite lands and this pass is in it.** _Rejected:_ default-ON at ship (the exact 0.10.5 mistake, restated in the roadmap as a hard "not before the suite"); no gate at all (even a provably-correct rewrite needs a kill switch for the field-bug-we-did-not-foresee, per the factorized-count precedent).

## 9. Observability (Q5 — resolved: add a stable input-source annotation to the Fixed `::explain` row)

Today `::explain` emits a single bare `op = "algo"` row for a FixedRule with **no input-source detail** (`runtime/db.rs:1772-1778`) — it does not show whether an input is `Stored` or `InMem`, so a test cannot directly assert "this input is streamed, not materialized." The *indirect* signature is observable (the passthrough rule's own `CompiledRuleSet::Rules` rows vanish from an earlier stratum when eliminated, and a stratum collapses), but that is fragile and absent when the rule is *kept* for other consumers. This pass therefore adds a **typed, stable input-source annotation** to the Fixed explain row — mirroring how factorization added its `factorize_advisory` row (`db.rs:1786-1791`) — e.g. each algo input reported as `*knows (streamed)` vs `edges (materialized)`. This is deliberately aligned with the roadmap's `::explain`-as-advisor / typed-warnings-surface item: it is the first concrete column of that surface, and it is what makes the pass CI-assertable on a *stable* field rather than by substring-matching the unstable debug format (the trap `join_reorder.rs` fell into). **Decision: the annotation is part of this item, not a follow-up.** _Rejected:_ shipping the pass with only the indirect (rule-disappearance) signature (untestable when the rule is kept; fragile against `::explain` format churn — the exact anti-pattern the warnings-surface item exists to end).

## 10. Test matrix (all sqlite backend per the repo test-backend rule; failing-test-first per house workflow)

In-crate `cozo-core/tests/passthrough.rs`. **sqlite, not mem** — the mem backend uses `mem_mat_join` and does not exercise the `scan_all` stored-relation path this pass targets (`tests/matjoin_regression.rs:16-18`; CLAUDE.md test-backend rule). Template: `tests/factorize.rs`'s differential oracle. Rows:

- (a) **Differential equivalence (the core)**: for `ConnectedComponents`, `PageRank` (weighted), and one more algo — direct form `ConnectedComponents(*knows[a,b], *person[x,e])` and lifted form `edges[a,b] := *knows[a,b] … ConnectedComponents(edges, nodes)` produce **byte-identical** output rows, switch ON and OFF.
- (b) **Elimination**: with the switch ON and the passthrough rule the algo's sole consumer, `::explain` shows the algo input annotated `streamed` and the `edges` rule's rows are absent; with it OFF, `edges` materializes (its rows present).
- (c) **Multi-consumer retention**: `edges` consumed by both the algo and a `?[a,b] := edges[a,b], a < b` echo — both results correct, `edges` still materializes (annotation shows the algo input streamed, the rule present).
- (d) **Reorder must decline**: `edges[b,a] := *knows[a,b]` feeding the algo — the pass must NOT fire; result equals the materialized (reversed-edge) form, not the direct-`*knows` form. This is the row that proves the reorder guard (§4).
- (e) **Arity truncation must decline**: `edges[a,b] := *knows[a,b,_]` into a weighted algo — must not fire; weighted result matches the materialized (default-weight-1.0) form.
- (f) **Filter/self-join/multi-atom/aggregation must decline**: `edges[a,b] := *knows[a,b], a<b` (filter); `*knows[a,a]` (self-join); a two-atom body; an aggregating body — each declines, result unchanged from OFF.
- (g) **Temporal source declines (v1)**: `edges[a,b] := *knows[a,b] @ 'NOW'` — must not fire under the v1 gate; result equals the materialized temporal form (this row becomes an equivalence row when §5's extension lands).
- (h) **Named-field source**: `edges[a,b] := :knows{from: a, to: b}` where the field order matches storage order — fires; a field order that permutes — declines (same as reorder).
- (i) **Stratum/lifetime**: assert the eliminated case produces one fewer stratum / the `edges` store lifetime is gone (the mechanism §3.2 relies on), via `::explain` stratum numbering.
- (j) **Projection composition**: a `graph:` projection whose source would otherwise be lifted — confirm the pass and the projection compose (the projection already reads a stored relation directly; the pass ensures the *input to the projection-consuming algo* is also not lifted).

Discrimination plan per house rule: neutralize the identity-order check (accept reorders) → (d) goes red while (a) stays green; neutralize the arity check → (e) red; neutralize the aggregation exclusion → (f) red; disable the elimination branch → (b) red. Post-landing: mutation counts recorded here, drift confessed if found.

## 11. Decisions (awaiting sign-off)

1. **A new whole-program `NormalFormProgram → NormalFormProgram` pass (`query/passthrough.rs`) in the `factorize` slot, before `into_stratified_program`** (§3.2, §2). _Rejected:_ a per-rule `reorder.rs`-style pass (can't see the algo arg or delete a sibling rule — needs whole-program scope); an execution-side change (unnecessary — `iter` already streams `Stored`).
2. **Recognition predicate: single-rule, no-aggregation, single bare stored-relation atom, distinct-var identity projection over all columns, non-temporal (v1)** (§3.1, §4). _Rejected:_ reorder/truncation/subset support (need a permuting iterator — deferred with triggers).
3. **Per-arg rewrite; keep-or-dead-eliminate the source rule by post-rewrite reference count** (§3.2, §6). _Rejected:_ single-consumer-only firing; a new refcount state.
4. **Temporal sources deferred behind the same predicate, execution-ready, named trigger** (§5). _Rejected:_ streaming them in v1 (unproven); blocking permanently (it is the only fast path for bitemporal graph analytics).
5. **Default OFF behind an `AtomicBool`; flip to ON only after the planner regression suite lands and includes this pass** (§8). _Rejected:_ default-ON at ship (the 0.10.5 mistake); no gate.
6. **Add a stable input-source annotation to the Fixed `::explain` row as part of this item** (§9) — the first column of the roadmap's typed-warnings surface. _Rejected:_ indirect-signature-only (untestable when the rule is kept).
7. **Release vehicle: mnestic 0.12.0** (rides with the budgeted-traversal headline; both are the benchmark-earned engine items the roadmap scopes to 0.12), default-OFF so the behavior change is opt-in until the suite gates it; `CHANGELOG-FORK.md` in the same PR; crate + PyPI readme "New in 0.12.0" blocks. **Contributor-suitability: high** — the predicate + rewrite are well-bounded, the oracle template exists, and it is named in the roadmap as a good "first real feature" task; the `::explain` annotation is the one part that touches shared plumbing and may want maintainer review.

## 12. Prior art

- Datalog/relational **view inlining / rule unfolding** — the classical optimization of substituting a non-recursive view's definition into its consumer instead of materializing it (Ullman, *Principles of Database and Knowledge-Base Systems*). This pass is the degenerate case: the "view" is an identity over a base relation, so unfolding = feeding the base relation directly. The instructive difference: general unfolding rewrites a *relational-algebra* consumer; here the consumer is a FixedRule that never gets compiled to algebra (`compile.rs:190-192`), so the "unfold" is a metadata swap of an opaque input handle, not a body rewrite.
- **Projection/identity elimination** in query optimizers (e.g. removing a no-op `Project` node) — same idea at the operator level; here there is no operator to remove because the copy is a stratum-boundary materialization, so the "elimination" is dead-rule removal plus a stratum collapse.
- The engine's own **equality-pushdown** (`reorder.rs`, fork #1) and **factorized `count()`** (`factorize.rs`) — the two precedents for *"recognize a syntactic shape the query author didn't optimize, and rewrite it,"* both gated, both differential-tested. This pass is the third, and the smallest: no cost model, no plan search, output provably identical — which is exactly why its risk is a *foreseen-correctness* risk (guard with the oracle) rather than a *plan-quality* risk (guard with the scale suite), and both guards are named.
- Neo4j GDS **native projection** vs **Cypher projection**, and Ladybug/Kùzu `project_graph` — other engines make the user *choose* a graph representation before running an algorithm, so "don't copy the edge list" is a manual, explicit step. mnestic's positioning is that the canned algorithm reads the stored relation directly with no projection ceremony; this pass removes the one case where the obvious query silently reintroduced the copy. There is no equivalent "we detect and remove your accidental edge-list materialization" documented in those systems.

---

| Date | Change |
|---|---|
| 2026-07-10 | Authored (research fleet: compile/eval-pipeline internals + benchmark grounding; author spot-verification of the InMem/Stored dispatch and the `factorize` pass slot). Status PROPOSED; §11 awaiting owner sign-off. |
