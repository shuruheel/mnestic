# Spec — Cardinality Algebra: factorized counting by hand (the authoring pattern)

_Created 2026-07-07. Status: **AUTHORING PATTERN — documented as of 0.10.5.** This is a reference for query authors, not a build spec: nothing here requires engine changes, and every worked example below was executed against the engine (mem backend) — the counts shown are actual output, naive and factorized forms verified identical. An **automatic narrow rewrite** of the simplest shape (a single-clause rule whose head is exactly one `count()`, over an alpha-acyclic all-positive body) ships **in the engine in 0.10.5**; see §7 for the division of labor. This document remains the reference for every shape the automatic pass does not catch — the inclusion-exclusion forms (§3.3, §3.4), `count_unique` (§4.4), and anything the conservative trigger declines. Companion to the fork's other specs in this directory; the semantics it leans on (set-valued rule stores, bag-valued aggregate streams) are upstream CozoDB semantics, unchanged by the fork._

> **Anti-overbuild guardrails.** This spec adds zero engine surface. It documents rewrites any user can author today, their exactness conditions, and the failure modes of getting the conditions wrong. Where a condition is load-bearing, §4 says *why* it holds in the engine — because a rewrite that is "usually right" is the worst possible artifact for a counting query.

---

## 1. The problem: `count()` over a join enumerates every match

A count-over-join query pays for every homomorphic match of its body, even though
the caller only wants one number:

```
?[count(tup)] :=
    *knows[p1, p2],
    *knows[p2, p3],
    *member[group, p3],
    p1 != p3,
    tup = [p1, p2, p3, group]
```

(§3.3 factorizes this exact query.)

The engine evaluates an aggregate rule by driving the compiled join pipeline and
streaming each projected match into a per-group accumulator (`count` is `+= 1`
per row — `AggrCount` in `data/aggr.rs`). Memory stays flat at O(#groups), but
CPU is O(#matches) — plus, in the form above, a per-match list allocation for
`tup` (§5.1). At tens of millions of matches this is seconds; at billions it is
not runnable. There is no cost-based optimizer and no automatic factorization of
general shapes: **the human is the optimizer.**

An external benchmark measured this directly: on a standard social-network join
workload, naive count-over-join CozoScript ran **4–342× slower** than a columnar
engine whose optimizer factorizes count aggregation automatically. The same
computations rewritten in the patterns below closed the gap and won several
workloads outright — one pattern with 1.65 × 10⁹ matches counted in about half a
second, because the rewrite never enumerates the matches. The rewrites are
mechanical, and their answers are **exactly** equal to the naive forms (§4), not
approximations.

The algebra is classical: Yannakakis-style counting on acyclic joins / the
counting semiring of the FAQ literature (Abo Khamis–Ngo–Rudra PODS'16;
Bakibayev–Olteanu–Závodný VLDB'13). What this document adds is the CozoScript
authoring discipline: which engine semantics make it exact, and where it breaks.

## 2. The toy dataset (all examples run against this)

People, groups, and a directed `knows` graph, plus a small
city/country/tag/class periphery. Six scripts, one per relation (each ran as its
own script):

```
?[city, country] <- [[10, 1], [11, 1], [12, 1], [12, 2], [13, 2], [14, 2]]
:create city_in {city: Int, country: Int}

?[person, city] <- [[1, 10], [1, 13], [2, 10], [3, 11], [4, 12], [5, 13], [6, 14]]
:create lives_in {person: Int, city: Int}

?[group, person] <- [[100, 1], [100, 2], [100, 4], [101, 2], [101, 3], [101, 5], [102, 4], [102, 6]]
:create member {group: Int, person: Int}

?[group, tag] <- [[100, 200], [100, 201], [101, 201], [102, 202]]
:create group_tag {group: Int, tag: Int}

?[tag, class] <- [[200, 300], [201, 300], [201, 301], [202, 302]]
:create tag_class {tag: Int, class: Int}

?[a, b] <- [[1, 2], [2, 1], [2, 3], [3, 4], [4, 2], [1, 3], [3, 5], [5, 6], [4, 5], [2, 4]]
:create knows {a: Int, b: Int}
```

The data is deliberately non-uniform so that a wrong rewrite would show: city 12
sits in two countries and person 1 in two cities (per-key counts > 1), `knows`
contains symmetric pairs (1↔2, 2↔4) so §3.3's diagonal is non-empty, and
shortcut edges (1→3, 2→4) so §3.4's correction term is non-empty.

## 3. The four rewrite patterns

### 3.1 P1 — Join-tree count DP (sum-of-products over per-key counts)

The general pattern, of which the other three are special cases or escapes.
Applies when the body's atoms form a **tree** (alpha-acyclic hypergraph): pick a
central edge, root the two subtrees at its endpoints, and propagate per-key
counts bottom-up — a leaf contributes a `count`, an internal node a `sum` of its
child's counts joined through the connecting atom, and the root a sum of
products across the central edge.

Naive — a five-atom chain, counted by enumeration:

```
?[count(country)] :=
    *city_in[city, country],
    *lives_in[person, city],
    *member[group, person],
    *group_tag[group, tag],
    *tag_class[tag, class]
```

→ `[[24]]`

Factorized — the chain splits at the `member[group, person]` edge into a
person-side subtree and a group-side subtree; each propagates counts toward the
split:

```
cc[city, count(country)] := *city_in[city, country]
pc[person, sum(c)] := *lives_in[person, city], cc[city, c]
tc[tag, count(class)] := *tag_class[tag, class]
gt[group, sum(c)] := *group_tag[group, tag], tc[tag, c]
?[sum(prod)] := *member[group, person], pc[person, a], gt[group, b], prod = a * b
```

→ `[[24.0]]` — same count; `sum` returns a Float (§5.2). Wrapping the final
answer in `to_int(...)` restores the integer: → `[[24]]`.

Each sub-rule is at worst linear in one relation joined against an
already-aggregated per-key table; nothing ever enumerates the 24 (or 1.65 × 10⁹)
full matches.

### 3.2 P2 — Star product

The one-level degenerate case of P1: all atoms share exactly one variable (the
star center). The count is the sum over the center of the **product of branch
degrees**.

Naive:

```
?[count(person)] :=
    *member[group, person],
    *knows[friend, person],
    *lives_in[person, city]
```

→ `[[15]]`

Factorized — one per-center degree rule per branch, product at the join:

```
mg[person, count(group)] := *member[group, person]
kf[person, count(friend)] := *knows[friend, person]
lc[person, count(city)] := *lives_in[person, city]
?[sum(prod)] := mg[person, a], kf[person, b], lc[person, c], prod = a * b * c
```

→ `[[15.0]]`

A center value missing from any branch contributes zero matches in the naive
form and drops out of the factorized join identically — no special-casing
needed.

### 3.3 P3 — `!=` inclusion-exclusion

A `p1 != p3` linking two otherwise-disjoint components blocks the product
bijection (§4.3). The escape is mechanical:

> count(body ∧ A ≠ B)  =  count(body)  −  count(body[A := B])

The subtrahend — the "diagonal" — is literally the body with one variable
substituted for the other, and it usually factorizes or collapses on its own.

Naive:

```
?[count(p2)] :=
    *knows[p1, p2],
    *knows[p2, p3],
    *member[group, p3],
    p1 != p3
```

→ `[[18]]`

Factorized — `total` is the unconstrained chain count (a P1 shape); `diag` is
the body with `p1 := p3`, which collapses the two `knows` atoms into a
symmetric-pair scan:

```
indeg[p2, count(p1)] := *knows[p1, p2]
gc[p3, count(group)] := *member[group, p3]
total[sum(prod)] := *knows[p2, p3], indeg[p2, d], gc[p3, t], prod = d * t
diag[sum(t)] := *knows[p3, p2], *knows[p2, p3], gc[p3, t]
?[ans] := total[a], diag[b], ans = to_int(a - b)
```

→ `[[18]]`

This pattern carried the largest single win in the external benchmark (two
orders of magnitude): the diagonal is tiny compared to the body it corrects.
Two inequalities need four terms (include–exclude–exclude–include); beyond two,
the term count doubles per inequality and the bookkeeping risk usually
outweighs the win.

#### 3.3a Soundness: the `!=` type gate (0.14.0 — read before touching this again)

The identity above has ONE hazard in this engine, it was shipped once, cut once
(`a60a8013`), and restored behind a gate — state the argument here so the next
reviewer does not re-derive the miscount and revert a second time.

**The hazard.** The subtrahend `count(body[A := B])` implements "A equals B" as
a **join** — and joins use the engine's total order, under which `Int(1)` and
`Float(1.0)` are **distinct** (`Num::cmp` maps numerically-equal cross-variant
pairs to `Less`; the memcmp key encoding agrees). The predicate `A != B`,
however, is evaluated by `op_neq`, which special-cases exactly the
`(Int, Float)` pair and compares **numerically** (`1 != 1.0` is `false`). A
numerically-equal cross-variant pair therefore escapes both terms: the naive
form excludes it (op_neq says equal) while the factorized form counts it (the
diagonal join never matches it). Silent miscount.

**Why the automatic pass is sound anyway (the gate, `factorize.rs`
`neq_types_admissible`).** The rewrite fires only when every binding occurrence
of both operands of every inequality is a declared **non-nullable**,
**non-`Any`** column of a stored relation, and all occurrences — across both
operands — declare the **same** type. Then:

1. Query-path writes coerce to the declared variant
   (`NullableColType::coerce` — the Int/Float arms *convert*, not merely
   check), so a declared-`Int` column holds only `Int` at rest.
2. `import_from_backup` was the one user-reachable raw-put that bypassed
   coercion; since 0.14.0 it refuses mismatched schemas
   (`tx::import_schema_mismatch`), so it cannot smuggle a `Float` into a
   declared-`Int` column.
3. ⇒ both operands are variant-identical at rest, the divergent `(Int, Float)`
   `op_neq` arm is unreachable, and the two "equality" notions coincide.

**⚠️ Amended 2026-07-17 (review): the divergent class is NOT only cross-variant
numerics.** `Json` columns diverge too, in the current build: `JsonData`'s `Eq`
is **structural** (serde_json equality — IEEE `==`, so `json(-0.0)` equals
`json(0.0)`; key-order-insensitive if any downstream crate unifies
`serde_json/preserve_order`) while its `Ord` — what storage keys and the
correction join use — compares `to_string()` output. Reproduced:
`json(-0.0)` vs `json(0.0)` are op_neq-equal but join-distinct, and the fired
rewrite overcounts 2-for-0 on a two-row self-join. The gate therefore requires
**variant-stable** types (`factorize.rs::coltype_variant_stable`): `Any` and
`Json` are excluded **recursively through `List`/`Tuple` element types** (the
derived container impls inherit the element divergence). `Vec` is
`OrderedFloat` on both sides and `Num`'s `PartialEq` is defined as
`cmp == Equal` — consistent, admissible.

Load-bearing details, each with a test in `tests/factorize.rs`:

- **All occurrences, not the first** — an operand bound by two atoms must agree
  at every occurrence; first-occurrence-wins is unsound
  (`ie_neq_disagreeing_occurrences_decline`).
- **`Any` is excluded by name** — an `Any` column is not variant-stable; it
  holds `Int(1)` and `Float(1.0)` simultaneously
  (`ie_neq_any_typed_operand_declines`).
- **Same non-numeric types are admissible** — the divergent `op_neq` arm exists
  only for `(Int, Float)` (`ie_neq_same_string_type_fires`).
- The non-null requirement is sufficient but is NOT justified by `op_neq`
  (`Null` hits the generic fallthrough, not the numeric arm); it is kept
  because inclusion–exclusion has no independent story for `Null` operands and
  the pass's bias is to decline.
- **For hand-written factorizations** (the subject of this document) the gate
  does not protect you: apply §3.3 only when both operand columns declare the
  same non-`Any` type, or accept that a cross-variant numerically-equal pair
  diverges from `!=` semantics.

### 3.4 P4 — Anti-join (negation) inclusion-exclusion

A cross-component `not *rel[...]` atom is the same move with the negation
dropped instead of a variable substituted:

> count(body ∧ ¬R)  =  count(body)  −  count(body ∧ R)

Naive:

```
?[count(p2)] :=
    *knows[p1, p2],
    *knows[p2, p3],
    *member[group, p3],
    not *knows[p1, p3]
```

→ `[[20]]`

Factorized — `total` as in §3.3; the correction term counts the matches where
the shortcut edge **does** exist, using a materialized 2-path pair count:

```
indeg[p2, count(p1)] := *knows[p1, p2]
gc[p3, count(group)] := *member[group, p3]
total[sum(prod)] := *knows[p2, p3], indeg[p2, d], gc[p3, t], prod = d * t
cn[p1, p3, count(p2)] := *knows[p1, p2], *knows[p2, p3]
hit[sum(v)] := cn[p1, p3, c], *knows[p1, p3], gc[p3, t], v = c * t
?[ans] := total[a], hit[b], ans = to_int(a - b)
```

→ `[[20]]`

**Honesty about the correction term:** `cn` materializes per-pair 2-path counts
— it is itself a (small) cyclic-ish computation and can become the new
bottleneck. Inclusion-exclusion pays off exactly when the correction term is
much cheaper than the body it corrects; when the negated atom is what makes the
result small, the positive `total` term is the giant and this pattern only
restructures the pain. Measure.

Combining §3.3 and §3.4 (a body with both `!=` and a negated atom) subtracts
both correction terms, then adds back their intersection if the two conditions
can overlap — standard inclusion-exclusion discipline; the external benchmark's
hardest workload needed exactly three terms.

## 4. Why this is exact — the conditions, each load-bearing

The product-of-counts identity is a theorem about this engine's semantics, not
a heuristic. Two facts carry it, and every condition below exists to keep one
of them applicable.

### 4.1 Rule stores are sets; aggregate input streams are bags

**Sets:** stored relations are keyed sets, and non-aggregate rule outputs are
deduplicated at the rule-store boundary. So a single-clause conjunctive body
over set atoms enumerates each full variable binding **exactly once** — for a
fixed valuation of the separator variables, the matches of the whole body are
in bijection with the Cartesian product of the per-component match sets. That
bijection *is* the product formula.

**Bags:** the projection from body variables to head arguments before an
aggregate does **not** deduplicate — the match stream hits the accumulator
as-is. So `count(x)` counts *body matches* regardless of what `x` is, and
`pc[person, sum(c)] := *lives_in[person, city], cc[city, c]` sums `c` once per
`(person, city)` row even though `city` doesn't survive into the head. Every
`sum` in §3 relies on this.

Both properties verified directly:

```
?[count(person)] := *lives_in[person, city]        # bag: one per row      → [[7]]
?[count_unique(person)] := *lives_in[person, city] # distinct variant      → [[6]]

mid[person] := *lives_in[person, city]             # set store dedups on projection
?[count(person)] := mid[person]                    #                       → [[6]]
```

The third result is **the projection pitfall**: routing multiplicity through a
non-aggregate intermediate rule that projects variables away silently collapses
it. Corollaries: do the count/sum in the *same* rule as the atoms whose
multiplicity it measures; and any non-aggregate helper rule must keep **all**
variables that distinguish matches in its head (as `cn[p1, p3, …]` and the
per-key count rules do — they key on everything that matters).

### 4.2 Single clause only

Multi-clause aggregate rules are legal (clauses must declare identical
aggregations) and all clauses stream into **one shared accumulator** — a match
derivable via two clauses is counted twice:

```
r[count(x)] := *lives_in[x, 10]          # 2 matches
r[count(x)] := *lives_in[x, c], x <= 2   # 3 matches, overlapping the first
?[v] := r[v]                             # → [[5]], not 4
```

A factorization of one clause has no way to reproduce that cross-clause bag
union, so: only factorize single-clause rules. (If you *wanted* set-union
semantics across clauses, the multi-clause count was already not computing it.)

### 4.3 No predicates across components

Every filter, unification, or negation must sit entirely inside one component
of the decomposition. A crossing predicate breaks the bijection of §4.1 — the
per-component matches are no longer independently combinable. The only two
crossing forms with a mechanical escape are `!=` (§3.3) and a single negated
atom (§3.4), both of which *remove* the crossing element and correct for it
exactly. Anything else crossing components (a `<`, an arithmetic relation, a
function of variables from two sides): do not factorize. Never approximate a
correction term — it is either exactly characterizable or the rewrite is wrong.

### 4.4 `count`, not `count_unique`

`count_unique` deduplicates after projection; product-of-counts is a statement
about the bag of matches and does not apply. `count_unique` has its own exact
rewrite that needs no products — project to the set first through a
non-aggregate rule (§4.1's "pitfall" used deliberately), then count:

```
xs[x] := <body>            # set store dedups x
?[count(x)] := xs[x]       # = count_unique(x) over the body
```

### 4.5 Separator keying, and empty groups

Each per-component sub-rule must carry **all** separator variables as group
keys (`mg`/`kf`/`lc` key on the star center; `pc`/`gt` on the split edge's
endpoints). A key present in some components but not others drops out of the
final join — which matches the naive semantics (zero matches) with no
special-casing, per §3.2.

Empty inputs behave asymmetrically and it matters for inclusion-exclusion:
**keyless** aggregate heads emit the neutral element on an empty body, keyed
ones emit nothing:

```
?[count(a)] := *knows[a, b], a > 100    # → [[0]]
?[sum(a)]   := *knows[a, b], a > 100    # → [[0.0]]
per[a, count(b)] := *knows[a, b], a > 100
?[a, c] := per[a, c]                    # → []
```

So a §3.3/§3.4 final join `?[ans] := total[a], diag[b], …` stays correct when
the correction term happens to be empty — verified: with an impossible diagonal
the answer is the full total, not zero rows — **as long as both terms are
keyless**. Per-group inclusion-exclusion (keyed correction terms) needs
explicit handling of missing groups (a `not`-guarded default clause), and at
that point re-check §4.2.

## 5. Practical tips

### 5.1 `count(var)`, never `count(tup)`

`count` ignores its argument's value; any body-bound variable counts the same
bag of matches (verified: the `tup` form of §3.2 also returns `[[15]]`).
Translations of `RETURN count(*)` habitually invent
`tup = [every, single, variable]` — that unification builds a fresh list **per
match**, tens of millions of pointless allocations on exactly the queries where
it hurts. Count a variable you already have.

### 5.2 `sum` is f64: the Float return and the 2⁵³ cliff

`sum` accumulates in `f64` and returns a Float even over all-Int input
(`AggrSum` in `data/aggr.rs`; verified: `[[27.0]]` over Int rows, and §3.1/§3.2
return `24.0`/`15.0` where the naive forms return Int). Two consequences:

- **Type:** wrap the final answer in `to_int(...)` if downstream code expects
  the naive form's Int (all §3 examples do this where it matters).
- **Precision:** f64 is exact for integers up to 2⁵³ (≈ 9.0 × 10¹⁵). The
  factorized patterns keep *per-key* partial counts small (bounded by per-key
  degrees), but the point of factorizing is that the **final total** can be
  astronomically larger than anything you'd enumerate — and that total flows
  through f64 sums. Below 2⁵³ every intermediate and the answer are exact;
  above it, `sum` silently rounds. There is no engine-side guard in the manual
  pattern. If your true count can exceed 2⁵³, split the outermost sum into
  disjoint ranges and add the (exactly representable) parts yourself, or accept
  the rounding and say so. Int multiplication in `prod = a * b` is i64 and can
  wrap in release builds, but per-key products are bounded by the true per-key
  match count, so if the true answer fits in i64 the partials do too.

### 5.3 When NOT to factorize

- **Cyclic cores.** Triangles, 4-cycles, and anything else that is not
  alpha-acyclic have no separator that splits them: delete any one variable and
  the rest stays connected. No sum-of-products form exists (counting a cyclic
  core is genuinely harder). The effective manual treatment is **join
  ordering**, not cardinality algebra: materialize the small cyclic core first
  in its own rule (e.g. enumerate `knows`-triangles), then decorate it with the
  cheap functional atoms. A tree-shaped *residue* hanging off a cyclic core can
  still be counted with these patterns and joined to the core.
- **Selective results with bloated intermediates.** If the final count is small
  and the cost is a bad written atom order, reorder the atoms — factorization
  restructures the aggregation, it does not fix a Cartesian blowup on the way
  to it.
- **You need the rows.** These patterns compute the *number* of matches without
  enumerating them; if any downstream rule needs the matches themselves, there
  is nothing to factorize away.
- **Expensive correction terms** — see the §3.4 warning; measure before
  assuming inclusion-exclusion wins.

## 6. Method: how to apply this to a query

1. Confirm eligibility: single clause (§4.2), head aggregate is `count`
   (§4.4), body all positive after setting aside `!=`s and negated atoms.
2. Draw the body's atom hypergraph. Repeatedly peel off "ears" — atoms whose
   variables are private to them except for ones covered by a single neighbor
   (GYO reduction). If a cyclic core remains, stop — §5.3.
3. Set aside cross-component `!=`s / negated atoms; each becomes one
   inclusion-exclusion term (§3.3, §3.4). Predicates that cross components and
   are neither of those: stop — §4.3.
4. Pick the separator (star center or central edge), write per-key `count`
   rules for the leaves and `sum`-of-child-counts rules inward (§3.1/§3.2),
   keying every sub-rule on all its separator variables (§4.5).
5. Final rule: sum of products across the separator; subtract correction
   terms; `to_int` the answer (§5.2).
6. **Verify on a sample**: run naive and factorized on a small slice and
   compare counts. The patterns are exact, but the conditions are yours to
   check, and a counting query that is quietly wrong is worse than one that is
   slow.

## 7. Relationship to the automatic pass (0.10.5)

The engine ships a **narrow automatic rewrite** in 0.10.5, doing pattern
§3.1/§3.2 by itself: it fires only on single-clause, non-recursive rules whose
head is exactly one `count()` (with optional group keys) over an all-positive,
alpha-acyclic body, and is specified to accumulate in exact integer arithmetic
internally (no §5.2 Float caveat on that path). Everything it declines — the
inclusion-exclusion shapes (§3.3, §3.4), `count_unique`, unions, cyclic bodies,
**any body containing a `!=` predicate** (the engine-side `!=`
inclusion-exclusion auto-rewrite was cut before release for miscounting on mixed
Int/Float data, so §3.3 stays hand-authored), and other crossing predicates —
falls back to naive evaluation unchanged, and this document is the reference for
rewriting those by hand. Consult
`CHANGELOG-FORK.md` for the shipped trigger conditions; the hand patterns in
this document remain valid (and identical in result) whether or not the
automatic pass fires.

---

## Changelog

| Date | Change |
|------|--------|
| 2026-07-07 | First authoring (0.10.5 wave, T1 of the factorized-aggregation design). Four patterns documented with executed worked examples (mem engine; naive = factorized on all four: 24/15/18/20); exactness conditions grounded in engine semantics (set stores × bag streams, single clause, no crossing predicates, count vs count_unique, keyless-empty neutral rows); tips: count(var) allocation, f64 sum cliff, when not to factorize. |
