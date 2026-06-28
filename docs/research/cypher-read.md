# Research + Design Seed — Read-Only Cypher Surface

_Status: **research complete, design not started** (2026-06-27). Seeds a future `docs/specs/cypher-read.md`. Companion to `../../ROADMAP.md` (Tier 1, sequenced **first**) and `../../DEVELOPMENT.md` §3.5. Sources cited inline; evidence-quality flagged where thin._

> **Goal.** A **read-only** query path that translates a *subset* of openCypher into CozoScript (Datalog), so developers can evaluate and adopt mnestic without first learning Datalog — the single biggest evaluation-friction remover for the Cozo/Kùzu community. Datalog stays the native, full-power language; Cypher is an on-ramp, not a replacement. **Read interop only** — no CREATE/MERGE/SET/DELETE (that's `ENGINEERING-PRIORITIES.md` X4).

## Why this is first on the roadmap

- **It removes the top adoption objection** ("but it only speaks Datalog") at evaluation time — when people decide. XTDB abandoned Datalog *for SQL* over exactly this ergonomic wall; we de-risk by *adding* a familiar read dialect rather than replacing Datalog.
- **The position is open.** Kùzu (the embedded Cypher engine) is archived; its community fork pivoted to lakehouse analytics. "Embedded + Cypher-readable + agent-memory-native" is unoccupied.
- Cozo's maintainer deliberately chose Datalog over Cypher ([HN](https://news.ycombinator.com/item?id=33518320)), so there's **no upstream scaffolding** — this is net-new, which is why a research pass precedes code.

## 1. Prior art (the technical seeds)

| Source | What it gives us | Reusable? |
|---|---|---|
| **Marton, Szárnyas & Varró, "Formalising openCypher Graph Queries in Relational Algebra," ADBIS 2017** ([ar5iv:1705.02844](https://ar5iv.labs.arxiv.org/abs/1705.02844)) | Exhaustive **operator-by-operator Cypher→relational-algebra mapping** (get-vertices, expand, all-different, grouping, top…). Conjunctive RA → Datalog rule bodies directly. | **The single best design seed.** Map RA operators → CozoScript rules. |
| **Francis et al., "Cypher: An Evolving Query Language for Property Graphs," SIGMOD 2018** ([PDF](https://homepages.inf.ed.ac.uk/libkin/papers/sigmod18.pdf)) + long form **arXiv:1802.09984** | **Formal read-only semantics** over binding tables with **bag semantics**; pins edge-uniqueness + 3-valued logic. | Correctness ground-truth (see §3). |
| **Raqlet, CIDR 2026** ([arXiv:2508.03978](https://arxiv.org/abs/2508.03978)) | Cypher → PGIR → **Datalog IR (stratified least-fixpoint)** → recursive SQL — exactly our pipeline shape. Pragmatic choice: rewrite to `RETURN DISTINCT` to match set-semantics backends. | **Design reference only — no usable OSS release** (the only candidate repo is 0★/unlicensed/unconfirmed). |
| **openCypher** `Cypher.g4` (Apache-2.0, 1,010★) | Spec-canonical grammar — but **generated from XML**, with reported correctness bugs. | Spec reference. |
| **Kùzu `src/antlr4/Cypher.g4`** (MIT, repo archived Oct 2025) | **Cleanest production-grade ANTLR4 Cypher grammar.** MIT stays usable post-archive. | **Recommended grammar starting point.** |
| **libcypher-parser** (Apache-2.0, 160★, dormant) | Standalone C parser. | Alt grammar reference. |
| RedisGraph (tri-license), Memgraph (BSL) | — | **Avoid copying** — non-permissive. |

(The `lmondada/raqlet` repo named in the original brief does not exist — corrected. Two toy Cypher-on-Datalog repos — `fhackett/cylog`, `LampicJ15/cypher-datalog-playground` — are abandoned and unlicensed.)

## 2. Translation strategy (node relation + typed-edge relations → conjunctive Datalog)

Assume the property graph maps to stored relations — e.g. `node(uid, label, …)` and typed edges `edge_TYPE(from, to, eid)` (the exact mapping is a design decision; mnestic/MindGraph already store nodes + typed edges this way).

| Cypher | Datalog/CozoScript | Confidence |
|---|---|---|
| `MATCH (a:A)-[r:TYPE]->(b:B)` | conjunctive body: `node_A(a), edge_TYPE(a,b,r), node_B(b)` (fresh edge-id var per relationship) | **Strong** (Raqlet + Marton both print this) |
| `WHERE φ` | append φ as body goals (selection σ) | **Strong** |
| `RETURN x,y` / `DISTINCT` | rule head = projection; DISTINCT is automatic under set semantics (the *inverse* — preserving Cypher's default duplicates — is the hard part, §3) | **Strong** |
| `WITH` | named intermediate relation feeding downstream rules (pipelining) | Moderate |
| aggregation (`count`/`collect`/…) + implicit group-by | separate aggregation rule keyed on the non-aggregating expressions, then a projection rule (**note: CozoScript forbids aggregation in union rules** — nested aggregates *must* decompose this way) | **Strong** on shape |
| `ORDER BY` / `SKIP` / `LIMIT` | **post-processing outside Datalog** (sort + slice) — set-semantics rules can't order/limit; Raqlet strips them too | **Strong** |
| `OPTIONAL MATCH` | left-outer-join: positive rule + negation rule null-padding the optional columns | Semantics strong; **exact Datalog encoding inferred, not cited** |
| **variable-length `[*1..3]` / `[*]`** | recursive rules: base `reach(a,b):-edge(a,b,_)`, step `reach(a,b):-reach(a,c),edge(c,b,_)`, + hop counter for bounds + visited-edge accumulator for uniqueness | **Weakest-cited** — no source prints the literal Cypher→recursive-Datalog rule; leans on general transitive-closure technique. **Defer (§5).** |

## 3. Semantic traps (the correctness landmines — all from SIGMOD 2018 / Neo4j docs)

1. **Bag vs set semantics.** Cypher tables are *multisets*; duplicates accumulate and are removed only by `DISTINCT`/`UNION`. Datalog is set-semantics — a naïve translation silently drops duplicates and **corrupts `count(*)` and `LIMIT`.** *Cope:* either (a) **default to `RETURN DISTINCT` set semantics** (Raqlet's pragmatic choice — simplest, document it), or (b) carry a provenance/row-id tuple to preserve true bag semantics, projected away at the end. **Pick one up front — it silently changes results.**
2. **Relationship-uniqueness (edge-isomorphism).** Within a single MATCH, all relationships must be distinct (nodes may repeat); Datalog defaults to homomorphism (one edge fact can satisfy two atoms) → over-production. *Cope:* pairwise disequality `r_i ≠ r_j` across relationship vars + a visited-edge accumulator inside var-length expansion. (General Cypher matching is NP-complete — this is more than a join.) **Non-negotiable even in the minimal subset.**
3. **Null & three-valued logic.** Cypher `WHERE` keeps only *exactly-TRUE* rows (null and false both filtered); `null = null` → null; aggregates skip nulls. Datalog is 2-valued/closed-world with no nulls. *Cope:* sentinel/present-flag for absence, expand predicates to full 3VL, guard negation so a null operand can't flip TRUE.
4. **DISTINCT × aggregation.** Three different mechanisms (implicit group-by, `count(DISTINCT x)`, `RETURN DISTINCT`) must not be conflated; aggregates skip nulls; nested-aggregate-in-expression is genuinely ambiguous.

## 4. ISO GQL (2024) — relevant, but target openCypher

GQL (ISO/IEC 39075:2024, pub. ~Apr 2024) adopts Cypher's MATCH/RETURN syntax and is **largely read-compatible with Cypher for the MATCH/RETURN/WHERE subset.** But: no DB ships full GQL conformance (even Neo4j supports "most but not all"); **openCypher has an open Apache-2.0 grammar AND an open TCK, no open GQL TCK was located**; and openCypher is the de-facto cross-vendor read dialect (Neptune, Memgraph, FalkorDB, Apache AGE, ArcadeDB). **Recommendation: target an openCypher read subset** — which is also the official on-ramp to GQL, so it's a convergence hedge, not a dead end.

## 5. Scoping recommendation (the 80/20)

**Ship first** (corroborated by [Amazon Neptune's supported-openCypher list](https://docs.aws.amazon.com/neptune/latest/userguide/feature-opencypher-compliance.html), the best vendor "supported subset" evidence):

1. `MATCH` single node + single/fixed-hop `(a:A)-[:R]->(b:B)` with labels + inline property maps
2. `WHERE` — comparison, boolean, `IN`, `IS NULL`/`IS NOT NULL`, `STARTS WITH`/`CONTAINS`
3. `RETURN` with projections, aliases, `DISTINCT`
4. `ORDER BY` / `SKIP` / `LIMIT` (static values, applied post-Datalog)
5. Multi-hop **fixed-length** chains
6. Basic aggregation `count`/`collect`/`sum`/`avg`/`min`/`max` with implicit grouping

**Explicitly defer:** variable-length/recursive paths `[*m..n]` (the under-documented hard case — though note recursion is *exactly* Datalog's strength, so this becomes a showcase later), `shortestPath`, **OPTIONAL MATCH**, complex multi-stage `WITH`, `CALL`/procedures, and **all write clauses**.

**The two non-negotiables even in the minimal subset:** (a) enforce **edge-isomorphism** (disequality on relationship vars); (b) **decide bag-vs-set policy** and document it.

**Test artifacts (both Apache-2.0, vendorable with attribution):** the **openCypher TCK** (Cucumber `.feature` files; ~220 feature files; use `clauses/match`, `clauses/return`, `clauses/with` as the conformance gate — real engines test against it, e.g. ArcadeDB self-reports 97.8%) and the openCypher grammar (or Kùzu's MIT `.g4`).

## 6. Proposed next steps

**→ Spec drafted 2026-06-27: [`../specs/cypher-read.md`](../specs/cypher-read.md)** — resolves the design decisions below (with recommendations), grounded in the CozoScript target grammar + the `hybrid_search` translate-to-CozoScript precedent. Key calls it makes: relation-per-label/type property-graph schema (caller-supplied); **preserve true bag semantics** via a binding-key row identity (verified: `count` does *not* dedup, so `count(*)` is correct given a per-binding input); translate to a **CozoScript string** run via `run_script_read_only`; a hand-written **`cypher.pest`** subset (reject `antlr-rust`); openCypher **TCK** subset as the CI gate. Open decisions tracked in the spec §11.

The original next-step list, now folded into the spec:
1. **Design decisions** — property-graph↔relation mapping; bag-vs-set; grammar source; the `run_cypher` entry point. *(resolved in spec §3–§8, §11)*
2. **Skeleton:** parse → Cypher AST → CozoScript string. *(spec §2, §10)*
3. **Conformance harness:** vendor the openCypher TCK match/return/with features. *(spec §9)*
4. **Recursive `[*]` follow-up** — where Datalog *wins*; a later differentiator, not part of the on-ramp. *(spec §1, §12)*

**Evidence flags:** strongest = SIGMOD 2018 semantics + Marton 2017 RA mapping + verified licenses/TCK. Weakest = the literal Cypher→recursive-Datalog form for var-length paths (no source prints it) and the OPTIONAL MATCH Datalog encoding (semantics cited, encoding inferred) — both in the deferred set, so they don't block the first cut.
