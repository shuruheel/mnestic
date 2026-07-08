# Semirings, the tropical "overlay," and the `FixedRule` trait

_A plain-language explainer for two engine concepts that come up when evaluating
probabilistic ranking and external-data ideas (e.g. a contributor's tropical-semiring
+ "APITables" proposals, 2026-06-28). Grounded in mnestic's actual code. Companion to
`docs/strategy/MNESTIC-ROADMAP.md` § "Evaluated proposals" (private strategy repo)._

---

## Part 1 — The tropical semiring (and what an "overlay" would mean)

### What's a semiring?

A **semiring** is a set of values with two operations — "add" (⊕) and "multiply" (⊗) —
obeying the usual laws (⊕ commutative, ⊗ distributes over ⊕, each has an identity).
Ordinary numbers form one with `+` and `×`. The trick: **you can swap in *different*
operations and the same machinery still works.**

### One query, many readings

The 2007 "provenance semirings" result (Green–Karvounarakis–Tannen): when a database
evaluates a query, every join is a "multiply" and every union/OR/projection is an "add."
Keep the *structure* of how a result was derived but swap what ⊕ and ⊗ *mean*, and the
same derivation computes different things:

| Pick ⊕ / ⊗ as… | …and the query computes |
|---|---|
| OR / AND (booleans) | **Does this answer exist?** (ordinary query) |
| `+` / `×` (counting) | **How many distinct ways** can it be derived? |
| `+` / `×` on [0,1] | **Total probability** (sum over all explanations) |
| **`min` / `+`** ← *tropical* | **Cheapest derivation** (shortest path / lowest penalty) |
| `max` / `×` (Viterbi) | **Most probable single explanation** |

### The tropical semiring specifically

⊕ = `min`, ⊗ = `+`. ("Add" becomes "take the minimum"; "multiply" becomes "add them up.")
Named after the Brazilian mathematician Imre Simon; also called min-plus algebra. It is
exactly **shortest-path / least-cost reasoning**: building a path *adds* edge costs (`⊗ = +`),
choosing among alternatives *takes the cheapest* (`⊕ = min`).

The "cute" observation: probabilities multiply, and you can't easily add probabilities of
overlapping explanations. But store **`−log(probability)`** instead:

- multiplying probabilities → **adding** penalties: `−log(P_A · P_B) = (−log P_A) + (−log P_B)`
- picking the *most probable* → picking the *smallest* total penalty

So "log-space integer penalties + `min`" **is** the most-probable-explanation computation,
with cheap integer arithmetic and no floating-point underflow.

### What an "overlay" would be — and the critical limit

The "factor graph / Bayesian network as a Datalog overlay" pitch: put penalty weights on
graph edges as ordinary data, write recursive rules that accumulate them, aggregate with
`min` — and plain Datalog queries return probabilistically-ranked answers, no separate
inference engine.

**The limit:** the tropical reading gives you **MAP** — the single *most-likely* explanation.
It does **not** give you **marginals** — the *total* probability of something being true
summed over *all* explanations. Marginals need the `+`/`×` sum-of-products reading, which
double-counts shared sub-derivations and is **#P-hard** (Dalvi–Suciu dichotomy); real
probabilistic engines (ProbLog) use knowledge compilation, not a Datalog fixpoint. So
honestly: you get *penalty-ranked / most-likely-answer* queries, not full probabilistic
inference. "You get Bayesian networks" oversells it.

### Why mnestic already does the tractable part

Datalog recursion iterates a rule to a **fixpoint** (until nothing changes). For an
aggregation used *inside* recursion to converge, it must be **idempotent** — applying it to
the same value twice changes nothing. `min`/`max` are idempotent (`min(x, x) = x`); `sum`
is not. So:

- `min` / `max` / `min_cost` / `shortest` → **`is_meet = true`** → **allowed in recursion**.
- `sum` / `product` / `count` / `mean` → **`is_meet = false`** → **forbidden in recursion**.

That `is_meet` boundary is *exactly* the line between the tractable tropical reading and the
intractable probabilistic one — and the engine already enforces it (`data/aggr.rs`,
stratifier in `query/stratify.rs`). Look at `min_cost` (`aggr.rs:799`):

```rust
// min_cost takes [payload, cost] pairs and keeps the payload with the smallest cost
DataValue::List(l) => {                     // each candidate is [value, cost]
    let cost = l[1].get_float()...;
    if cost < self.cost {                   // ⊕ = min   (pick the cheaper alternative)
        self.cost = cost;                   //           (the "+" — accumulating cost —
        self.found = l[0].clone();          //            happens in the rule body)
    }
}
// returns [winning_payload, winning_cost]  ← the witness: WHICH derivation won
```

That is the **tropical (min, +) MAP computation with a provenance witness, already shipped.**
A useful "overlay" (penalty-ranked retrieval) = log-penalty columns + existing aggregates,
written at the *app* layer (mindgraph). A *generic* "thread any semiring through any query,
read it five ways" engine feature is the research-grade, mostly-not-useful part. In
CozoScript the existing machinery already expresses shortest-path = tropical:

```
shortest[to, min_cost([path, cost])] := edge[from, to, w], from == $start, path = [to], cost = w
shortest[to, min_cost([path, cost])] := shortest[mid, [p, c0]], edge[mid, to, w],
                                        path = append(p, to), cost = c0 + w
#                                                                    ↑ the "+"     ↑ min_cost = "min"
```

---

## Part 2 — The `FixedRule` trait

### What it is

`FixedRule` is Cozo's plugin point for **"a relation whose rows are produced by Rust code
instead of being stored on disk."** You register a Rust object; in a query it binds exactly
like a stored relation, but at run time your code generates the rows. Every built-in graph
algorithm (PageRank, connected components, Dijkstra) is a `FixedRule`; so are the CSV and
JSON-lines *readers*.

### The trait (`fixed_rule/mod.rs:538`)

```rust
pub trait FixedRule: Send + Sync {
    fn init_options(&self, ...) -> Result<()> { Ok(()) }     // optional: validate options once
    fn arity(&self, options, rule_head, span) -> Result<usize>; // how WIDE is your output?
    fn run(
        &self,
        payload: FixedRulePayload,   // inputs: bound relations + options
        out: &mut RegularTempStore,  // you WRITE result rows here
        poison: Poison,              // check periodically so the user can cancel
    ) -> Result<()>;
}
```

Tell the engine your output width (`arity`), then `run` reads inputs from `payload`, does
whatever it wants, and pushes rows into `out`. `SimpleFixedRule` (`:571`) is a closure-based
convenience version: `Fn(inputs, options) -> output_rows`.

### How you call one — the `<~` operator

```
stored[a, b]  <-  [[1, 2], [3, 4]]           # <-  constant rows
derived[a, c] :=  stored[a, b], other[b, c]  # :=  ordinary Datalog rule
?[node, rank] <~ PageRank(*edges[])          # <~  apply a FixedRule
```

### The mechanism mix-up worth getting right

"It already works for HNSW, so why not register a function?" conflates two mechanisms:

1. **HNSW / FTS search is *not* a `FixedRule`.** It's a hard-wired compiler path —
   `MagicAtom::HnswSearch` / `FtsSearch` (`compile.rs:500`), invoked with the *search*
   operator `~relation:index_name{ bindings | query: ..., k: 10 }`. Not user-extensible.
2. **The pluggable "function that looks like a relation" already exists** — it's `FixedRule`,
   and it already reaches *outside* the DB: the CSV utility (`csv.rs:150`) reads from a
   `file://` path **or an HTTP URL**.

So the precedent for external-data relations isn't HNSW — it's `FixedRule`, which is the
right mechanism *and already does external I/O.*

### Which built-in `FixedRule`s are just semiring recursion in disguise?

Part 1 said a *meet* aggregate (`min` / `max` / `min_cost` / `min_cost_k`, `is_meet = true`)
is exactly the tractable tropical reading, usable inside recursion. Several of the built-in
graph fixed-rules compute precisely what such a recursive rule computes — so you can express
them as CozoScript and then **swap the algebra on the same traversal** (cost → confidence →
top-k proofs) just by changing the aggregate. Two of these equivalences have been validated
on the SNB `knows` graph (a benchmarker's redundancy sweep, 2026-07-08); the rest are the
same family or partial:

| Built-in `FixedRule` | Semiring / meet form | Status |
|---|---|---|
| `ShortestPathDijkstra` | recursive `min_cost([path, cost])` | ✅ **validated identical** (bar the source self-distance convention: the fixed rule reports `d(s,s)=0`; the recursion reaches `s` only via a cycle) |
| `ShortestPathBFS` | `min_cost`, unit weight | ✅ same family |
| `KShortestPathYen` | `min_cost_k([path, cost], k)` | ◐ **approximate** — Yen returns *loopless* (simple) paths; the bounded meet keeps the k lowest-cost derivations and, without an explicit visited-guard in the rule body, can pad with cycles |
| `ConnectedComponents` (weakly-connected) | recursive **min-label** propagation | ✅ **validated identical** |
| `ShortestPathAStar` | `min_cost` | ◐ same `(min, +)` result, but the A\* heuristic is imperative pruning — not expressible declaratively (you keep the answer, lose the speed-up) |
| `Bfs` / `Dfs` reachability | boolean semiring (`reachable?`) | ◐ reachability yes; DFS *visit order* is imperative |
| `ClosenessCentrality` | all-pairs `min_cost` core + reciprocal sum | ◐ the distance core is a meet; the `1/Σd` tail is non-idempotent |
| `BetweennessCentrality`, `PageRank`, `LabelPropagation` (community), `Louvain`, `StronglyConnectedComponents` (directed/Tarjan), `MinimumSpanningTree` (Prim/Kruskal), `Triangles` / clustering, `TopSort`, `RandomWalk`, `DegreeCentrality` | — | ✗ **not semiring** — additive / greedy / ordering / stochastic (or, for degree, a non-recursive `count`) |

> **Two easy conflations, spelled out** (both worth getting right before quoting the table):
> 1. **`LabelPropagation` ≠ min-label propagation.** The built-in `LabelPropagation` is
>    *community detection* (Raghavan et al. 2007): each node adopts the **weighted-plurality**
>    label of its neighbours, with randomized tie-breaks and randomized visit order. That is
>    non-idempotent, order-dependent and nondeterministic — **not** a meet. The thing that *is*
>    a meet is **min-label** propagation (each node takes the *minimum* label reachable), which
>    is exactly how you express `ConnectedComponents`. Same word, different algebra.
> 2. **`KShortestPathYen` ≠ `min_cost_k` exactly** (see the ◐ above): simple-path enumeration
>    vs. k-cheapest-derivation truncation. They agree on acyclic inputs and diverge once a
>    cheaper *walk* revisits a node.

Swapping the algebra is the whole point — the *same* recursive traversal answers a different
question by changing one aggregate:

```
# cheapest single path (MAP):
sp[to, min_cost([path, cost])]        := edge[from, to, w], from == $start, path = [from, to], cost = w
sp[to, min_cost([path, cost])]        := sp[mid, [p, c0]], edge[mid, to, w], path = append(p, to), cost = c0 + w

# k cheapest paths, each WITH its evidence chain (what mindgraph's `top_k_paths` rides on) —
# identical body, `min_cost` → `min_cost_k([payload, cost], k)`:
kp[to, min_cost_k([[path, cost], 3])] := edge[from, to, w], from == $start, path = [from, to], cost = w
kp[to, min_cost_k([[path, cost], 3])] := kp[mid, packs], edge[mid, to, w], /* unpack + visited-guard */ ...
```

**Why keep both, then?** The fixed rules aren't dead weight: hand-optimized Rust (Dijkstra's
heap, A\*'s heuristic, Yen's deviation logic) keeps a measured ~1.6–2.4× edge over the general
semi-naive fixpoint, and each `<~ Name(...)` is a stable, already-tested API surface. So the
value here is **document, don't delete**: knowing the equivalence lets you (a) swap `cost →
confidence → top-k proofs` on the same graph without a new algorithm, and (b) use the pairs as
**differential tests** — a fixed-rule-vs-semiring disagreement is a bug (`ShortestPathDijkstra`
and `ConnectedComponents` are already pinned this way in `runtime/tests.rs` /
`tests/spec_doc_validation.rs`). Retiring a fixed rule in favour of its Datalog form would
trade a fast, stable surface for hand-written recursion — not worth it.

---

## Part 3 — How those connect to "APITables"

An **APITable** = a `FixedRule` whose `run()` opens a connection to Postgres / REST / Parquet
and emits the fetched rows, so an external dataset appears as a joinable relation.

**Why "reaching a fixpoint" is the worry:** Datalog iterates recursive rules until the result
stops changing, and correctness *assumes the base facts don't change mid-query*. A live
external source could return different rows per read → recursion might not settle.

**Why it's mostly free here:** `FixedRule`s force a **stratification boundary** — the engine
runs the fixed rule *once*, materializes its output to a temp relation, then continues. It is
not re-invoked per recursive iteration, so an APITable is effectively **snapshotted once per
query**. The dangerous case (external source *inside* a recursive cycle) is exactly what
stratification prevents.

**The hazard not usually named — async-in-sync:** `run()` is synchronous and every script runs
inside **one pessimistic RocksDB transaction**. A network call inside `run()` blocks that whole
transaction — a slow Postgres stalls the *entire database*. A real APITable needs timeouts,
connection pooling, and a circuit breaker (the server's LLM circuit-breaker pattern is the
template). That's the actual work, and why this is "weeks," not "register a function."

**The strategic line:** copy-on-load (one-shot bulk `IMPORT` from Postgres/Parquet/REST) is
copy-and-transform → on-wedge (already roadmap Tier-1 `LOAD FROM`). *Live* foreign relations
queried in place = map-without-copy federation → a different product than agent memory.

---

## Part 4 — Adjacent terms, quickly

- **TMS (Truth Maintenance System):** beliefs record *why* they're held (justifications); on
  retraction/contradiction the system *propagates* consequences (un-believes dependents,
  surfaces contradictions). MindGraph has the data *shape* (justification/contradiction/
  supersession edges) but not the propagation — it detects contradictions statistically and
  leaves them for a human. Hence **contradiction-aware, not truth-maintaining**.
- **MAP vs. marginals:** MAP = "single most-likely explanation" (`min`/`max`, tractable,
  tropical). Marginal = "total probability across *all* explanations" (`sum`, #P-hard).
- **OVM** (in the enterprise meaning-model): compile ontology constraints (transitive /
  symmetric / inverse / cardinality) into Datalog rules that derive edges and enforce schema.
  Structurally the same "compile down to CozoScript" move as the Cypher-read surface — which is
  why it's the plausible good-faith roadmap-capture path. Rail: "enforce the ontology MindGraph
  already stores" (a MindGraph feature), not "a LinkML/OWL governance compiler in the engine."
- **Provenance polynomial:** the most detailed reading — annotate each answer with a *formula*
  recording every tuple/combination that produced it. "Universal": every other reading
  (boolean, count, tropical, probability) is recovered from it by substitution.
