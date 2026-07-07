# Spec — Deterministic Greedy Join Reorder (default-on, stat-free)

_Created 2026-07-07. Status: **SHIPPED in 0.10.5** — `query/reorder.rs::greedy_reorder_conjunction`, wired into `NormalFormInlineRule::convert_to_well_ordered_rule`; the per-query policy is `ReorderMode` (`data/program.rs`), the `:reorder` written option is parsed in `parse/query.rs`, and the `::explain` Cartesian annotation lives in `runtime/db.rs`. Pinned by `cozo-core/tests/join_reorder.rs`. This documents the shipped behavior; the code is the source of truth and the CHANGELOG-FORK 0.10.5 entry is the release-facing summary._

> **Anti-overbuild guardrails.** This is a **stat-free** pass: it consults no cardinality estimates, no histograms, no cost model — mnestic has no cost-based optimizer and this does not introduce one. It reorders only the positive stored-relation atoms of a conjunction by a deterministic structural heuristic (fewest new variables first), leaves results bit-identical, and is conservative by construction: it declines any rule whose shape it cannot prove safe rather than guessing. Where a written order is already good, the pass is the identity and touches nothing. The goal is narrow — remove the pathological N³ blow-up an LLM-authored conjunction falls into — not to become a planner.

---

## 1. The problem: no pass considered join order

Before 0.10.5 the only ordering pass was the upstream binding-before-use
well-ordering (`well_order_body`), which floats predicates/unifications/negations/
search atoms to their earliest fully-bound slot but **leaves the positive
`Rule`/`Relation` atoms in exactly their written order**. Join order was therefore
whatever the author wrote. That is fine for a hand-tuned query and pathological
for a naively-ordered one — exactly the conjunction an LLM agent authors when it
lists relations in schema order rather than join order.

The motivating repro (from an external Ladybug-vs-mnestic benchmark) is a
"members-first same-group" triangle: a conjunction whose written order forces a
large intermediate (all pairs of members) before the selective join that would
have shrunk it. Every individual step is a connected prefix join, so nothing is
"wrong" — but the intermediate is O(N³) where a better order keeps it O(N²).
Measured **54.5×** on the repro; the reorder converts the N³ intermediate to N².

## 2. Where the pass runs

Per inline rule, after disjunctive-normal-form and after the fork's **#1
equality-pushdown** (`push_equality_filters_to_bindings`), and **before** the
binding-before-use `well_order_body` (`convert_to_well_ordered_rule`). The #1
pass runs first so that any `k == <ground>` equality it hoists into a leading
unification is already present when the reorder seeds its bound set; the
well-ordering pass runs after so it remains the correctness arbiter for
binding-before-use over whatever positive-atom order the reorder produced.

Primary-key arities of the referenced relations are memoized in a
`key_arity_cache` shared across every rule of the program (a `SessionTx` has no
relation-handle cache, so this keeps the pass close to free even when a relation
is referenced many times).

## 3. The per-query policy (`ReorderMode`)

`ReorderMode` is `Greedy` by default. Two ways to change it:

- **`:reorder written`** — opt out for this query; the authored atom order is
  used verbatim (`well_order_body` still runs). `:reorder greedy` forces the
  default explicitly. Any other value is a parse error
  (`parser::bad_reorder_mode`).
- **A `:limit` without a `:sort`** forces `Written` at normalization time,
  regardless of the requested mode. Under a reordered plan an early-returning
  bare `:limit` could return a *different* (still valid) row subset than the
  written order would, so the pass steps aside to keep the returned subset
  stable. A `:limit` **with** a `:sort` is unaffected (the sort fixes the
  subset).

## 4. Eligibility (conservative v1 — all must hold)

The pass returns "no change" (leaving the written body untouched) unless every
one of these holds:

- **Every positive body atom is a stored `Relation` atom.** A derived-rule
  application (`Rule`) makes the magic-sets/recursion interaction non-trivial,
  and an `Hnsw`/`Fts`/`Lsh` search atom has fixed placement (it must sit where
  its query vector/text is bound) — either one disqualifies the whole rule.
- **At least 3 positive relation atoms.** Two or fewer cannot exhibit the
  blow-up and there is nothing to gain.
- **No multi-valued `in`-unification** in the body (see §6 — the load-bearing
  exclusion).
- **Not a bare `:limit` without `:sort`** (enforced query-wide by the caller,
  §3).

Predicates, single-valued unifications, and negated atoms in the body do **not**
disqualify the rule — they are re-floated to their earliest bound slot by
`well_order_body` afterward, and every one of them is set-preserving, so their
final position is result-immaterial. Only the positive relation atoms are
reordered; everything else keeps relative order and is re-floated.

## 5. The algorithm: min-new-vars greedy

Seed a bound-variable set from the unifications positioned **before** the first
relation atom (the #1-hoisted equalities and any leading user unification).
Wildcard columns (`~`, unique generated-ignored symbols that are never shared)
are excluded from connectivity and new-variable scoring throughout.

Then repeatedly, until every relation atom is placed:

1. **Connected candidates** = the remaining atoms that share at least one
   non-wildcard variable with the bound set (i.e. joinable without a Cartesian
   product).
2. **If any candidate is connected**, pick the argmin over the total,
   deterministic ordering:
   - **new variables introduced, ascending** — fewest-new-vars first. Pulling a
     0-new-var atom forward is semi-join filter pushdown.
   - then **bound-key-prefix length, descending** — prefer an atom whose leading
     primary-key columns are already bound, because it compiles to a keyed
     `stored_prefix_join` rather than a full scan.
   - then **written index, ascending** — a stable, unique final tie-break
     (written positions are unique, so the order is total and the result
     deterministic).
3. **If no candidate is connected**, the conjunction is (currently) disconnected:
   pick the earliest-written remaining atom. This yields the provably minimal
   number of Cartesian steps (= number of connected components of the
   variable-sharing graph − 1). The very first pick is the base scan, not a
   Cartesian join; a later disconnected pick is flagged as a Cartesian step
   (§7).

Add the picked atom's variables to the bound set and continue.

## 6. Why results are unchanged — and the multi-valued-`in` exclusion

A conjunction of positive generator atoms is **commutative under set
semantics**: reordering the atoms cannot change the set of variable bindings the
body produces, and `well_order_body` re-derives binding-before-use over the new
order (so a variable is never used before some atom binds it). Stored relations
are sets, so each full binding is enumerated exactly once regardless of order.
That is the whole correctness argument for the common case.

The **one** construct that breaks it is a **multi-valued `in`-unification**
(`x in <list>`, `one_many_unif`), which is why the pass disqualifies any rule
containing one. It is a *multiplicity injector*: it compiles to a **generator**
(one output row per list element, no dedup) when its variable is unbound at its
position, but to a **filter** (keep-if-member) when the variable is already
bound. Moving a relation atom that binds that variable across the `in`-unification
flips it between generator and filter, which changes the body's *multiset* of
matches. A set-valued (deduplicated) rule head hides that difference — but a
**non-idempotent aggregation** (`count`/`sum`/`collect`) reads the multiset
directly, so the reorder would silently change a `count`. Every other eligible
atom is set-preserving (stored relations are sets; rule and search atoms are
excluded by §4), so the multi-`in` unification is the sole unsafe construct, and
excluding the whole rule is the safe, simple response.

## 7. Cartesian steps: warned and annotated

If even the greedy order still contains a Cartesian step — a genuinely
disconnected conjunction, where a join shares no bound variable with its left
input — the pass emits a `log::warn!` naming the rule so agent frameworks can
surface it, and `::explain` annotates the operator as `<op> (cartesian)` (the
`joins_on` set for that operator is empty). This is a diagnostic, not an error:
a disconnected conjunction is legal, just usually a mistake, and the reorder
still minimizes the number of such steps.

## 8. Two safety valves: identity and compile-fallback

- **Identity.** If the greedy order equals the written order, the pass returns
  "no change" and the written body is used untouched. So any hand-tuned query
  whose written order is already stepwise-greedy-consistent compiles to a
  **byte-identical** plan — the pass never perturbs a good plan.
- **Compile-fallback.** If a reordered body fails the well-ordering fixpoint
  (e.g. an unbindable pending-unification chain the permutation exposed), the
  pass retries the **original written order** rather than surfacing a new error.
  The pass can therefore never introduce a compile failure that the written
  order would not have had, and error spans keep pointing at the user's written
  text.

## 9. Relationship to the #1 equality-pushdown

The two passes are complementary and ordered. #1
(`push_equality_filters_to_bindings`) rewrites `*rel[k, ..], k == <ground>`
equality post-filters into hoisted bindings so a keyed lookup compiles to a
prefix join; the greedy reorder then orders the positive atoms so that
already-bound key prefixes are exploited (the bound-key-prefix tie-break in §5).
#1 runs first precisely so its hoisted equalities seed the reorder's bound set.
Neither pass changes result sets; both are pure optimizations.

## 10. What this pass is not

- Not a cost-based optimizer, and not a step toward one — no statistics are
  consulted. When a written order is bad for a reason the heuristic cannot see
  (e.g. selectivity the structure does not reveal), the human is still the
  optimizer; `:reorder written` plus manual ordering remains available.
- Not a rewrite of aggregation cardinality — that is the separate, opt-in
  factorized-`count()` work (`docs/specs/cardinality-algebra.md`). This pass
  reorders a join; it does not count without enumerating.
- Not applied to rule applications, recursion, or search atoms — those are
  excluded wholesale (§4) to keep the interaction surface empty by construction.

---

## Changelog

| Date | Change |
|------|--------|
| 2026-07-07 | First authoring (0.10.5). Documents the shipped deterministic min-new-vars greedy join reorder: default-on `ReorderMode::Greedy`, `:reorder written` opt-out, `:limit`-without-`:sort` force-to-written; conservative eligibility (≥3 stored relation atoms, no rule/search atoms, no multi-valued `in`-unification); the min-new-vars → bound-key-prefix → written-index tie-break; set-semantics result-invariance and the multi-`in` multiplicity-injector exclusion; identity + compile-fallback valves; Cartesian `log::warn!` + `::explain` annotation; ordering after the #1 equality-pushdown. Measured 54.5× (N³→N²) on the repro. Pinned by `cozo-core/tests/join_reorder.rs`. |
