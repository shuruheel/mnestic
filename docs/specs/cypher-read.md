# Spec — Read-Only Cypher Surface

_Status: **Implementation in progress — steps 2 (parser) + 3 (translator) landed** (2026-06-27); steps 4–6 remain. Owner: TBD. Companion to the research briefing [`../research/cypher-read.md`](../research/cypher-read.md) (prior art, semantics, scoping), `../../ROADMAP.md` (Tier 1, sequenced **first**), and `../../DEVELOPMENT.md` §3.5. Grounded in the actual CozoScript target grammar (`cozo-core/src/cozoscript.pest`) and the `hybrid_search` translate-to-CozoScript precedent (`cozo-core/src/runtime/hybrid.rs`); citations are `file:line` in `cozo-core/src/`._

> **One-line goal.** Let a developer query mnestic with a **read-only subset of openCypher** that the engine translates to CozoScript and runs — so the engine is easy to *evaluate and adopt* without first learning Datalog. Datalog stays the native, full-power language; Cypher is an on-ramp. **No write clauses** (CREATE/MERGE/SET/DELETE) — that's `ENGINEERING-PRIORITIES.md` X4.

## 1. Goals & non-goals

**Goals:** remove the "it only speaks Datalog" evaluation objection; occupy the "embedded + Cypher-readable + agent-memory-native" position vacated by Kùzu's archival; ship the high-value 80/20 read subset with correct `count(*)`/`LIMIT` and injection safety.

**Non-goals (v1):** write clauses; variable-length/recursive paths `[*m..n]` and `shortestPath`; `OPTIONAL MATCH`; multi-stage `WITH` pipelines; `CALL`/procedures; full GQL. Datalog remains the only way to express what the subset can't (recursion in particular is *Datalog's* strength — a later showcase, not an on-ramp gap).

## 2. Architecture — translate to CozoScript (mirror `hybrid_search`)

The proven pattern in this codebase (`runtime/hybrid.rs`, the `HybridSearch` builder) is: **a typed request → assemble a CozoScript *string* → pass literal values as query params (never string-interpolated) → validate every interpolated identifier → run via the normal query path, and expose the generated script for inspection.** The Cypher surface is the same shape:

- **Translate Cypher AST → a CozoScript source string**, not to the engine's internal AST. Rationale: the string path is fully inspectable/testable, and it decouples the Cypher layer from internal compiler types that are deliberately not `Clone`/stable (the plan-cache finding in `DEVELOPMENT.md` item 9). The translated script then runs through the existing, optimized query pipeline for free.
- **Run via `run_script_read_only`** (`runtime/db.rs:470`). Because the surface is read-only, routing through the read-only entry means *the engine itself rejects any mutation* even if a translation bug emitted one — defense in depth, and it picks up the snapshot read path (0.8.5) automatically.
- **Injection safety (non-negotiable, copy `hybrid.rs`'s discipline):** every Cypher *literal* becomes a CozoScript **param** (`$p0`, `$p1`, … in the `params` map `run_script` already accepts); every *identifier* that gets interpolated (relation/column names from the property-graph schema, fusion-style tags) is **validated as a bare identifier** via `miette::ensure` before it touches the string. No user value is ever concatenated into the script.
- **Module:** new `cozo-core/src/cypher/` — `mod.rs` (schema types + public `run_cypher` / `cypher_to_script`), `parse.rs` (pest → Cypher AST), `translate.rs` (AST → CozoScript string). Behind a **`cypher` cargo feature** (see §11, decision 5).

```text
run_cypher(query, schema, params)
  └─ parse.rs:  Cypher text ──pest──▶ Cypher AST
  └─ translate.rs: AST + schema ──▶ CozoScript string  (+ params map, validated idents)
  └─ db.rs:    run_script_read_only(script, params) ──▶ NamedRows
  └─ mod.rs:   column-projection adapter ──▶ user-visible NamedRows
```

## 3. The property-graph schema (decision 1 — the crux)

openCypher assumes a **property graph** (labeled nodes with properties; typed, directed relationships with properties). mnestic stores **arbitrary relations** and — per `LAYERING.md` — must not bake in any fixed "node/edge" notion (that's cognitive policy; it lives in MindGraph). So the caller supplies a **typed property-graph schema** mapping Cypher's model onto stored relations — general, describable without cognitive vocabulary, and reusable by any property-graph-over-Cozo user (MindGraph just passes *its* node/edge relations).

```rust
/// How Cypher's property-graph model maps onto stored relations. Caller-supplied.
pub struct CypherGraphSchema {
    pub nodes: Vec<NodeMap>,
    pub edges: Vec<EdgeMap>,
}
pub struct NodeMap {
    pub label: String,            // Cypher label, e.g. "Person"
    pub relation: String,         // stored relation, e.g. "person" or "node"
    pub id_col: String,           // identity column, e.g. "id" / "uid"
    pub label_col: Option<String>,    // discriminator column when the relation is SHARED
                                      // across labels (e.g. "node_type"); None = relation IS the label
    pub label_value: Option<DataValue>, // value to match in label_col (default: the label string)
    pub filter: Option<String>,   // optional always-ANDed CozoScript predicate over this relation's
                                  // columns (e.g. a soft-delete guard) — a general mechanism, no
                                  // cognitive vocabulary; lets shared relations exclude dead rows
    // remaining columns are addressable as properties (a.name → the `name` column)
}
pub struct EdgeMap {
    pub rel_type: String,         // Cypher relationship type, e.g. "KNOWS"
    pub relation: String,         // stored relation, e.g. "knows" or "edge"
    pub from_col: String,         // source node id column
    pub to_col: String,           // destination node id column
    pub type_col: Option<String>,     // discriminator when the edge relation is SHARED (e.g. "edge_type")
    pub type_value: Option<DataValue>,// value to match in type_col (default: the rel_type string)
    pub eid_col: Option<String>,  // explicit edge identity (e.g. a reified edge "uid"); see §6
    pub filter: Option<String>,   // optional always-ANDed predicate (soft-delete guard, etc.)
}
```

**v1 supports BOTH modeling conventions** (decision 1 — revised 2026-06-27 after verifying the primary consumer):

- **Relation-per-label / relation-per-type** — each label/type is its own relation (`label_col = None`). Label/type filters become *relation selection*; constant property-equality compiles to keyed lookups via the fork's equality-pushdown (#1). Natural for purpose-built schemas.
- **Shared relation + discriminator column** — one node relation with a type column, one edge relation with a type column (`label_col = Some("node_type")`, `type_col = Some("edge_type")`). **This is how MindGraph stores data** (`node{uid => node_type,…}`, reified `edge{uid => from_uid, to_uid, edge_type,…}` — verified in `mindgraph-rs` migrations), so it is **not deferrable** — the surface would be useless for its biggest consumer without it. The discriminator filter is a param (`node_type: $p`), hitting MindGraph's `node:type_idx`/`edge:type_idx` secondary indexes + #1.

Both are expressed by the *same* `CypherGraphSchema` — only `label_col`/`type_col` differ. MindGraph passes one schema mapping its labels (node_type values, and/or layers) onto the shared `node`/`edge` relations; a hand-rolled graph passes relation-per-label maps. (A *generic* triple-store with arbitrary labels-as-data is the same shared-relation case with `label_value` per NodeMap.)

**Labels/types on patterns:** with a shared relation, an *unlabeled* `(n)` is trivial — drop the discriminator (`*node{uid: n}`). With relation-per-label, unlabeled needs a union over node relations unless one relation is designated the default — so v1 supports unlabeled **iff** a shared/default node relation exists, else returns a clear "label required" error (union-over-relations is v2).

## 4. Translation rules

For node label `L` → `NodeMap{relation, id_col}` and relationship type `T` → `EdgeMap{relation, from_col, to_col}`:

| Cypher | CozoScript | Notes |
|---|---|---|
| `(a:Person)` — relation-per-label | `*person{id: a}` | label → relation; binds the id var. Named-relation access (`relation_named_apply`, grammar :88). |
| `(a:Person)` — shared relation | `*node{uid: a, node_type: $pL}` | label → discriminator param (`$pL = "Person"`); hits `node:type_idx` + #1. |
| `(a:Person)-[r:KNOWS]->(b:Person)` | `*person{id: a}, *knows{fr: a, to: b}, *person{id: b}` | each label/type = one atom; relationship binds endpoints. `r` binds to the edge identity (see §6). |
| `a.age` (property) | bind the column: `*person{id: a, age: a_age}` → use `a_age` | property access = column binding, **not** the `->` op (that's Json access). |
| `WHERE a.age > 30` | `a_age > 30` (filter atom) | comparison/boolean/`IN`/`IS NULL`/`STARTS WITH`/`CONTAINS` → expr atoms. |
| `WHERE a.name = 'Bob'` | `*person{id: a, name: $p0}` (param) | constant equality pushed into the relation access → **indexed lookup via #1**. Literal `'Bob'` → param `$p0`. |
| `RETURN e1, e2` | head of the final `?[…]` rule | projection (see §5 for bag vs DISTINCT). |
| `RETURN DISTINCT …` | set-semantics head (no binding key) | Datalog set dedup = DISTINCT. |
| `count(*)`, `collect(x)`, `sum`/`avg`/`min`/`max` | head aggregate `?[k, count(a)]` (grammar `aggr_arg` :75) | implicit group-by = the non-aggregating RETURN columns. **`count` counts rows with multiplicity** (`aggr.rs:423-434`, *not* `count_unique`) → bag-correct given a per-binding input. |
| `ORDER BY x [DESC]` / `SKIP n` / `LIMIT m` | `:order -x` / `:offset n` / `:limit m` | **native CozoScript epilogue** (grammar :140-142, :157-160) — no external post-processing. |

### Worked example (end-to-end)

```cypher
MATCH (a:Person)-[:KNOWS]->(b:Person)
WHERE a.age > 30
RETURN b.name AS name, count(*) AS c
ORDER BY c DESC
LIMIT 10
```

with schema `Person→person{id => name, age}`, `KNOWS→knows{fr, to}` translates to:

```
_match[a, b, b_name] := *person{id: a, age: a_age}, a_age > 30,
                        *knows{fr: a, to: b},
                        *person{id: b, name: b_name}
counted[b_name, count(a)] := _match[a, b, b_name]
?[name, c] := counted[name, c]
:order -c
:limit 10
```

The aggregation rule (`counted`) feeds `count` a per-binding relation (`_match` is a *set* of distinct `(a, b, b_name)` bindings, so `count(a)` per `b_name`-group = the true Cypher row count); a final projection rule binds plain vars so `:order`/`:limit` can name them.

### Same query against a shared-relation (MindGraph) schema

With `Person → node{uid => node_type, name, age}` (`label_col="node_type"`) and `KNOWS → edge{uid => from_uid, to_uid, edge_type}` (`type_col="edge_type"`, `eid_col="uid"`), the *same* Cypher translates to:

```
_match[a, r, b, b_name] := *node{uid: a, node_type: $p0, age: a_age}, a_age > 30,
                           *edge{uid: r, from_uid: a, to_uid: b, edge_type: $p1},
                           *node{uid: b, node_type: $p0, name: b_name}
counted[b_name, count(a)] := _match[a, r, b, b_name]
?[name, c] := counted[name, c]
:order -c
:limit 10
```

with params `$p0 = "Person"`, `$p1 = "KNOWS"`. Discriminators are params (indexed via `node:type_idx`/`edge:type_idx` + #1); the reified edge's identity is `r` (its `uid`), so edge-isomorphism disequalities are over `r`. Only `label_col`/`type_col`/`eid_col` differ from the relation-per-label example — same translator, same bag/aggregation handling. (A real MindGraph schema would also set `filter: "tombstone_at == 0.0"` to exclude soft-deleted rows — a general predicate the engine ANDs in without knowing what a "tombstone" is.)

## 5. Bag-vs-set semantics (decision 3)

Cypher tables are **multisets** (duplicates accumulate; removed only by `DISTINCT`/`UNION`); Datalog is set-semantics. A naïve translation silently drops duplicates and corrupts `count(*)`/`LIMIT`. **Recommendation: preserve true bag semantics** via the binding tuple as row identity:

- The intermediate `_match` rule carries **all bound variables** (every node id, every relationship identity, every returned property var). Because `_match` is a *set* of distinct full bindings, its cardinality equals the Cypher bag size.
- **Non-DISTINCT `RETURN`** (no aggregation): the final `?[…]` head includes the RETURN expressions **plus the binding key** (hidden columns), so per-binding multiplicity survives; `:order`/`:limit` operate over the correctly-multiplied rows; the **NamedRows adapter projects to just the user-visible columns**. (Ordering ties broken by the hidden key — fine; Cypher leaves ties unspecified.)
- **`RETURN DISTINCT`**: head = RETURN columns only → Datalog set dedup = DISTINCT.
- **Aggregation**: feed the aggregate the per-binding `_match` (so `count` — which doesn't dedup — is correct); group-by = non-aggregating RETURN columns.

Simpler documented fallback (decision 3): **default everything to `RETURN DISTINCT` set semantics** (Raqlet's pragmatic choice). Cheaper, but `count(*)`/`LIMIT` then differ from real Cypher — which is exactly what evaluators check, so the recommendation is to do the binding-key approach.

## 6. Edge-isomorphism, null / 3-valued logic

- **Edge-isomorphism (per-MATCH relationship uniqueness, non-negotiable even in v1):** within one MATCH, all relationships must be distinct. Datalog defaults to homomorphism (one edge fact satisfies two atoms) → over-production. Emit pairwise disequality across relationship identities: for relationships `r_i, r_j` in the same MATCH, add `r_i != r_j`. **Relationship identity** = the `eid_col` if the `EdgeMap` provides one (e.g. MindGraph's reified edge `uid` — the common case for shared edge relations), else the edge's key tuple `(from_col, to_col)` plus `type_col` when discriminating on a shared relation (decision 6). A relation that permits parallel edges (same from/to/type, distinct rows) **must** set `eid_col`, or the two parallel edges are indistinguishable — documented limitation.
- **Null / three-valued logic:** Cypher `WHERE` keeps only *exactly-TRUE* rows (null and false both excluded); `null = null` → null; aggregates skip nulls. Datalog is 2-valued/closed-world. v1 cope: map `IS NULL`/`IS NOT NULL` explicitly; for missing optional properties use a sentinel/`is_null` guard; ensure negated predicates can't flip TRUE on a null operand. (Most of v1's subset matches on *present* columns, so the null surface is small; document the boundary.)

## 7. Surface API

Mirror `hybrid_search` / `hybrid_search_script` exactly:

```rust
impl DbInstance {
    /// Translate + run a read-only Cypher query. Read-only path; literals → params; idents validated.
    pub fn run_cypher(&self, query: &str, schema: &CypherGraphSchema,
                      params: BTreeMap<String, DataValue>) -> Result<NamedRows>;
    /// The generated CozoScript, for inspection / hand-tuning / golden tests.
    pub fn cypher_to_script(&self, query: &str, schema: &CypherGraphSchema)
                      -> Result<(String, BTreeMap<String, DataValue>)>;
}
```

- **Python binding** (`cozo-lib-python`): expose `run_cypher(query, schema_dict, params)` so the wheel + `langchain-mnestic` can use it (same way `hybrid_search` gained `graph_legs`).
- **Schema delivery (decision 2):** per-call `CypherGraphSchema` in v1 (simplest). A *registered/persisted* graph view (define once, query by name) is a v2 convenience.

## 8. Grammar choice (decision 4)

**Recommendation: a hand-written `cypher.pest`** (a new pest grammar for the v1 subset), parsed with the engine's existing pest infrastructure. Rationale: the whole engine is `pest`-based (`cozoscript.pest`); the v1 subset grammar is small; no new heavy dependency or build step. Use **Kùzu's MIT `Cypher.g4`** and the **openCypher Apache-2.0 grammar** as *reference* for getting productions right, and the **openCypher TCK** as the conformance gate.

**Rejected:** vendoring an ANTLR grammar + `antlr-rust` runtime — pulls a Java codegen build step and a less-mature runtime into a lean embedded engine; the generated openCypher `.g4` also has known correctness bugs. (See research doc §1.)

## 9. Testing

- **Backend: SQLite + `tempfile::tempdir()`** for execution tests (per `CLAUDE.md`; the stored/scan path differs from `mem`).
- **Golden translation tests** on `cypher_to_script` — assert exact CozoScript output for each subset feature (the core correctness surface; fast, no execution).
- **openCypher TCK subset** — vendor the Apache-2.0 `.feature` files for `clauses/match`, `clauses/return` (and `clauses/with` when WITH lands), with attribution; gate CI on the shippable subset. (Real engines do this — ArcadeDB self-reports 97.8%.)
- **Correctness traps**: bag fidelity (`count(*)` and `LIMIT` match Cypher on a graph with duplicate paths); edge-isomorphism (a triangle doesn't match the same edge twice); null/3VL (`WHERE` excludes null); injection (literals are params, hostile relation/column names rejected).

## 10. Phased implementation plan

1. **Spec + decisions** — ✅ done (this doc; §11 settled 2026-06-27).
2. **Schema types + `cypher.pest` + parser → Cypher AST.** — ✅ done (2026-06-27). Module `cozo-core/src/cypher/` (`schema.rs`, `ast.rs`, `cypher.pest`, `parse.rs`) behind the off-by-default `cypher` feature; 8 parser unit tests green; clippy clean on default *and* `--features cypher`. No execution path yet.
3. **Translator `cypher_to_script`** (AST + schema → CozoScript string + params). — ✅ done (2026-06-27). `translate.rs`: both schema conventions, WHERE/RETURN/DISTINCT/aggregates, inline-prop filters → keyed lookups, edge-isomorphism (eid or best-effort), bag fidelity via hidden binding-key, ORDER/SKIP/LIMIT → native epilogue. 8 tests incl. **end-to-end execution** against an in-memory DB (relation-per-label, MindGraph shared-relation, count, bag-vs-distinct) — the generated CozoScript runs and returns correct rows. Returns `CypherScript { script, params, out_columns }`. Deferred with clear errors: undirected rels, the schema `filter` field. *(Total cypher tests: 15 green; clippy clean.)*
4. **`run_cypher`** → `run_script_read_only` + the NamedRows column-projection adapter (keep first `out_columns.len()` columns for bag mode). *(next)*
5. **TCK conformance harness** (vendored subset) + correctness-trap tests.
6. **Python binding + docs**; flip the `cypher` feature on by default once the subset passes the TCK gate.

Each step: failing test first, CHANGELOG-FORK entry, `cargo test -p mnestic --lib` green (per `CLAUDE.md`). Steps 3–4 are the substance.

## 11. Settled decisions (2026-06-27)

1. **Property-graph mapping — support BOTH conventions in v1** (relation-per-label/type *and* shared-relation+discriminator). *Revised from the draft's "defer shared-relation to v2"* after verifying MindGraph (primary consumer) stores a single `node` relation (`node_type` column) + reified `edge` relation (`edge_type`, own `uid`) — deferring it would make the surface useless for its biggest user. One `CypherGraphSchema` expresses both (`label_col`/`type_col` optional). Unlabeled patterns: supported with a shared/default relation; relation-per-label union is v2.
2. **Schema delivery — per-call `CypherGraphSchema` in v1.** A registered/named persisted graph view is an additive v2 convenience (more motivated now that a MindGraph schema is large); auto-derivation from relation metadata is a later ergonomic. API designed so a named view is additive (no breaking change).
3. **Bag vs set — preserve true bag semantics** (binding-key row identity; `count` verified non-deduping). `RETURN DISTINCT` → set dedup. Correct `count(*)`/`LIMIT` is what evaluators check, so the cost is justified.
4. **Grammar — hand-written `cypher.pest` subset**, reusing cozo's existing pest + operator-precedence (Pratt) machinery for WHERE expressions (lowers divergence risk). Kùzu MIT `.g4` + openCypher grammar as reference; openCypher TCK as the CI gate. `antlr-rust` rejected (heavy/immature dep + Java codegen step in a lean engine).
5. **Feature gating — `cypher` cargo feature, off by default during alpha**, flipped default-on once the v1 subset passes the TCK gate; revisit removing the gate entirely (always-on, no new deps) at 1.0.
6. **Edge identity — `eid_col` when provided** (e.g. MindGraph's reified edge `uid`), else `(from_col, to_col)` + `type_col` for shared relations (= the edge relation's key tuple). Parallel-edge relations must set `eid_col` (documented limitation). Auto-deriving identity from relation key metadata is a v2 robustness improvement (kept out of v1 to keep `cypher_to_script` pure/tx-free).

## 12. v1 scope (ship / defer) — from the research 80/20

**Ship:** `MATCH` single node + fixed-length pattern `(a:A)-[:R]->(b:B)` with labels + inline property maps; `WHERE` (comparison, boolean, `IN`, `IS NULL`/`IS NOT NULL`, `STARTS WITH`/`CONTAINS`); `RETURN` with projection, aliases, `DISTINCT`; `ORDER BY`/`SKIP`/`LIMIT`; multi-hop fixed-length chains; basic aggregation (`count`/`collect`/`sum`/`avg`/`min`/`max`) with implicit grouping.

**Defer:** variable-length/recursive paths `[*m..n]`, `shortestPath` (where Datalog *wins* — a later differentiator showcase, not an on-ramp gap); `OPTIONAL MATCH`; complex multi-stage `WITH`; `CALL`/procedures; all write clauses.

**Two non-negotiables even in v1:** enforce edge-isomorphism (§6); decide and document the bag-vs-set policy (§5).

## 13. Rejected alternatives

- **Full Cypher / write semantics** — doubles surface, undercuts Datalog (X4). Read-only on-ramp only.
- **Translating to the internal AST/`Program`** instead of a CozoScript string — couples to non-`Clone` compiler internals; the string is inspectable and rides the existing pipeline.
- **`antlr-rust` + vendored ANTLR grammar** — heavy/immature dep + Java build step in a lean engine (§8).
- **Targeting ISO GQL instead of openCypher** — no open GQL TCK, no full implementations yet; openCypher has the open grammar + TCK *and* is the on-ramp to GQL (research §4).
