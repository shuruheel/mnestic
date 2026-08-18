# Spec — Stored / named queries (`::query`, issue #9 / upstream #276)

_Created 2026-08-18. Status: **IMPLEMENTED in 0.16.0 — decisions 1–7 signed by owner 2026-08-18; implementation and SQLite regression matrix landed together**. This is the design the issue asked to see before any patch. Grounding: every `file:line` in §2 was verified against the working tree on 2026-08-18; §2.1's external claims were verified the same day by a five-lane official-docs survey (one URL per claim; empirical and excerpt-grade items flagged; raw citation trail: `docs/plans/stored-queries-prior-art.md` in the workspace repo). Companion reading: upstream [cozodb/cozo#276](https://github.com/cozodb/cozo/issues/276) (the demand signal — what users asked for is **views**, not procedures), `graph-projection.md` (sysop-family and spec-discipline template), `bitemporality.md` §4 (`:as_of` semantics the views must not silently interact with)._

> **Anti-overbuild guardrails.** These are **not materialized views** — a stored query is persisted *text*, re-parsed and spliced at each use, always evaluated fresh against the transaction's own snapshot; nothing is cached, nothing is invalidated. **No planner/evaluator/magic/stratify changes**: the entire feature is (a) a catalog, (b) sysop grammar + dispatch arms, and (c) one parse-level splice pass over `InputProgram` that runs before normalization. No plan cache in v1 (§6 records the path and the blocker). No mutation in stored bodies (v1 read-only, per the issue). No in-place update (`::query remove` + `create` only — this is what makes reference cycles impossible by construction, §3.6). No per-atom parameter binding (§3.2's scoping rule). No new storage keyspace (§3.1 stores definitions in an ordinary reserved-name relation, the `mnestic_evict_audit` precedent).

---

## 1. Why / what this buys

Three pulls, in order of weight:

1. **Upstream demand is for views.** Every participant in cozodb/cozo#276 wants the same thing: a web of reusable named rules (`allowed_users[principal, email] := …` → `allowed_channels[…] :- allowed_users[…], …`) that today must be textually duplicated into every query that filters by them. The commenter who said "I think what you're referring to is more like **views**, not procedures" was right, and this design follows him: the primary invocation form is a **rule atom in a caller's query**, not a procedure call.
2. **Agent tooling wants invoke-by-name.** mindgraph-style consumers save retrieval rules (security trims, standing filters, top-k recipes) and invoke them by name over the wire with parameters. `::query run <name>` plus `::query list` (which exposes each query's full typed parameter signature, §3.2) is that surface.
3. **Substrate for a future plan cache** — deliberately *not built* in v1. §6 records exactly what v1 pins so the cache stays reachable, and the one engine fact that blocks it today (parse-time `$param` inlining, §2 row 1).

The performance story of the view form: a spliced stored query participates in **magic-set rewriting like a hand-written rule** — in `?[ch] := allowed_channels['alice', ch]` the binding pushes down through the whole spliced rule chain, so upstream's access-control use case gets specialization for free. Stated honestly against the SQL lineage (§2.1 corrected an earlier draft's claim here): modern SQL engines *do* inline plain views — Postgres expands them through the rewrite system and pulls the subquery up into one plan tree; SQLite's flattener merges view subqueries precisely so outer predicates reach real indexes — so "views evaluate then filter" is **not** the differentiator. The differentiators are (a) **recursion**: SQLite documents that its flattener cannot flatten recursive CTEs and Postgres delegates recursive views entirely to `WITH RECURSIVE`, while magic sets specialize through recursive rules; and (b) **the catalog itself is open ground in the Datalog lineage** — Soufflé and DDlog fix rules at compile time, Datomic passes rule sets with every query, and the graph-DB analogues carry caveats ours doesn't (§2.1's table).

## 2. Verified baseline (load-bearing facts, checked 2026-08-18)

| # | Fact | Where |
|---|---|---|
| 1 | `$param` is inlined **at parse time**: `Rule::param` looks the name up in `param_pool` and emits `Expr::Const`; a missing param is `ParamNotFoundError` at parse. There is no symbolic parameter in the AST — this is what makes a parsed-AST cache impossible today and what shapes create-time validation (§3.5) | `parse/expr.rs:186-200` |
| 2 | Unresolved rule-apply names survive parse (`InputRuleApplyAtom` carries just a `Symbol`) and error only at **compile** (`RuleNotFound`) — so a post-parse, pre-normalize pass is a clean interception point with no grammar change | `parse/query.rs:711-726`; `query/compile.rs:131,218,416` |
| 3 | `rule_apply = {underscore_ident ~ "[" ~ apply_args ~ "]"}` with `apply_args` = expr list — rule-atom args already accept constants/params, so positional binding into a view needs zero new syntax. Program-local rule names may start with `_`; plain `ident` (`XID_START`-first) cannot — reserving plain-`ident` stored-query names never collides with a `_`-temp convention | `cozoscript.pest:102,116,72` |
| 4 | `InputProgram = { prog: BTreeMap<Symbol, InputInlineRulesOrFixed>, out_opts, disable_magic_rewrite }`, entry rule named `?` (`PROG_ENTRY`); the AST module is documented **unstable** ("structure and method signatures may change in any release") — adding a field is sanctioned | `data/program.rs:512-518`, `data/symb.rs:107`, `parse/mod.rs:8-10` |
| 5 | **Triggers are the persisted-script-text precedent**: `::set_triggers` validates each block by parsing it (empty param pool, empty custom-aggr registries) and stores the raw `as_str()` text; the text is re-parsed at every fire | `parse/sys.rs:278-310`; `query/stored.rs:97,777,2099` |
| 6 | `run_query` is the **single evaluation funnel** for single-query, imperative-embedded, and trigger programs; `::explain` replicates the compile pipeline separately in its own dispatch arm (and has the `tx` in hand) | `runtime/db.rs:3281`, `db.rs:1893-1915`, `db.rs:2438-2453` |
| 7 | "Is a write" = `needs_write_lock()` = the program has a `store_relation` output op (`:put`/`:create`/…); read-only mode rejects exactly that | `data/program.rs:609-619`, `db.rs:1926-1929` |
| 8 | Relation catalog rows live under `RelationId::SYSTEM` keyed by `DataValue::Str(name)`, value = rmp-serde `RelationHandle`; **`::relations` decodes every value in the SYSTEM range as `RelationHandle`** — any foreign row shape in that keyspace breaks the scan, which rules out a bare-keyspace catalog for stored queries | `runtime/relation.rs:1034,1093-1104`; `db.rs:3707-3718` |
| 9 | Sysop-family precedent: `graph_op` appears in **both** `sys_script` and `sys_script_inner` (imperative-reachable); sysops can run inside an imperative script's tx | `cozoscript.pest:14-19,26-29`; `imperative.rs:123-124` |
| 10 | Engine-feature-persists-rows precedent: `::evict` writes audit rows into the reserved-name ordinary relation `mnestic_evict_audit` from its dispatch arm | `db.rs:2389` (per `graph-projection.md` §3.4 row 12) |
| 11 | `QueryOutOptions` carries `limit, offset, timeout, mem_limit, reorder, sleep, as_of, sorters, store_relation, assertion` — the full set §3.3 must gate in atom form | `data/program.rs:61-85` |
| 12 | `CozoScript::get_single_program()` rejects imperative and sysop scripts with a loud diagnostic — the create-time body gate | `parse/mod.rs:228-240` |
| 13 | Column-type machinery is reusable as-is for parameter contracts: `col_type` grammar rule; `NullableColType::coerce`; `Expr::eval_to_const` for constant defaults; `table_col` already spells the idiom `ident ~ (":" ~ col_type)? ~ ("default" ~ expr)` | `cozoscript.pest:241-242`; `data/relation.rs:188`; `data/expr.rs:400` |
| 14 | The `mem` backend joins through a different operator than persistent backends — and the splice pass **reads a stored relation**, so its regression tests must run on **sqlite + tempdir**, per the repo's standing test-backend rule | `CLAUDE.md` (test-backend gotcha); `matjoin_regression.rs` |

### 2.1 Prior art (external survey, verified 2026-08-18)

_Method: five parallel doc-verification lanes over official documentation, one URL per claim; the complete citation trail is `docs/plans/stored-queries-prior-art.md` (workspace repo). Facts marked **(emp)** were additionally reproduced live (DuckDB 1.5.5, SQLite 3.51.0, Kùzu 0.11.3 wheel); LogicBlox is **excerpt-grade** (doc site unreachable — quotes from search excerpts of the official manual)._

| System | Stored named queries? | Parameter contract | Update story | Dependency story | Execution model |
|---|---|---|---|---|---|
| PostgreSQL (views + SQL functions, docs v18) | yes — catalog | functions: types **required**, defaults supported, named-arg calls; views: none | `OR REPLACE` restricted to column-compatible (append-only columns) | `DROP … RESTRICT` is the **default**; refuses while dependents exist | views = rewrite-rule expansion + subquery pull-up; SQL-function inlining conditional and documented only wiki-grade |
| DuckDB (macros) | yes — catalog, persisted, schema-qualified **(emp)** | name required, **type optional, default optional** (`:=`), defaults evaluated at **definition time**; per-call args, positional or named | `OR REPLACE` (whole definition; overloads only declarable all-at-once) | **none tracked** — drop succeeds, error at use **(emp)** | expansion then bind; predicate pushdown through table macros **(emp)**; recursion structurally impossible |
| SQLite (views) | yes | none | **no** `OR REPLACE` **(emp)** | none — drop succeeds, error at use **(emp)** | flattener merges views into the outer query; documented as unable to flatten recursive CTEs |
| Datomic (rules) | **no** — rules are per-query data (the `%` input) | positional unification; **required-bindings annotation** in the rule head (`(track-info [?artist] ?name ?duration)`) | n/a | n/a | duplicate names = the disjunction mechanism; query cache keyed on structural equality, docs advise "use parameterized inputs instead of embedding constants" |
| Soufflé | no runtime catalog — compile-time only | components parameterized by type names, bound per `.init` | n/a | n/a | `inline` relations = rule-body splice with a loud restriction list (no fully-inlined cycles, no `$` counter, no relations in aggregators, no I/O relations); components flatten into per-instance dot-qualified namespaces |
| DDlog | no — "changes to the relational schema or rules require re-compilation" | n/a | n/a | n/a | incremental evaluation over a fixed rule set |
| LogicBlox (excerpt-grade) | yes — inactive blocks, `execblock` on demand | — | — | — | persisted stored-rule blocks; product defunct |
| Neo4j APOC custom (Extended, v2025) | yes — system database | signature-**typed** params with JSON defaults; per-statement **READ/WRITE mode** declared at install | **overwrite on reinstall**, with a documented `db.clearQueryCaches()` footgun | drop by name | **eventually consistent** (refresh interval); a fresh install is not callable in the installing tx |
| Kùzu (macros, v0.11.3) | yes — catalog; rides `EXPORT DATABASE` (`macro.cypher`) **(emp)** | untyped names, defaults (`:=`), **scalar-only** | **create-once** in every released version — no drop, no replace **(emp)**; drop merged post-release | none | expression expansion |

What the survey establishes, decision by decision (§7 carries the same column):

- **D1 (catalog as an ordinary relation)** — reinforced: both stored-catalog graph systems ship definitions through their export tooling (Kùzu's `macro.cypher` in `EXPORT DATABASE`; APOC's system-DB metadata export exists explicitly for backup/restore migration). Our backing relation inherits the same for free.
- **D2 (declared params, type+default optional)** — now precedented exactly: DuckDB's macro contract is our shape verbatim, down to defaults evaluated at definition time (their definition-time = our create-time `eval_to_const`). Postgres functions are stricter (types mandatory); APOC types its signatures; Kùzu and Datomic declare bare names. **No surveyed system infers a signature from body text.** DuckDB's defaulted-params-must-trail rule is a positional-call artifact we don't inherit (our invocation binds by name, from the pool).
- **D3 (shadowing → warning)** — the survey confirms nobody has an answer to steal: the SQL lineage puts all relations in one namespace and errors at create (unavailable to us — our collision is with rule names in *future* caller scripts, unknowable at create), and Datomic dissolves the problem by never storing rules (duplicate rule names there are the *disjunction* mechanism, not a conflict). Warning-not-error remains our call, made on the no-retroactive-breakage argument.
- **D5 (no in-place update)** — sharpened in both directions. Counterpoint on record: three of the four stored-catalog systems offer replace (Postgres `OR REPLACE`, column-compatible only; DuckDB, whole-definition; APOC, overwrite-on-reinstall). Support on record: Postgres's RESTRICT-by-default drop is exactly our remove-refuses; released Kùzu is *stricter* than us (create-once — not even drop); and APOC's overwrite ships with a documented stale-plan footgun ("you might need to call `db.clearQueryCaches()` as lookups to internal id's are kept in compiled query plans") plus an eventual-consistency window — the two hazards our remove+create-with-snapshot-reads design structurally avoids. Recommendation unchanged for v1; §4 records the v2 path.
- **D7 (run-wide params)** — confirmed as a lineage split, chosen deliberately: SQL-descendant systems (DuckDB, Postgres functions, APOC, Kùzu) bind arguments per call site; Datalog-lineage systems bind positionally through unification (Datomic rule atoms) or at compile-time instantiation (Soufflé `.init`). Our head-column doctrine is the Datomic answer — and the one magic sets can specialize.
- **Read-only v1** — precedented: APOC declares a per-statement READ/WRITE mode at install (its two doc pages contradict on the default; ours is explicit with no default to dispute).
- **Splice + hygiene** — both halves have direct Datalog-lineage precedent: Soufflé `inline` is rule-body splice guarded by a loud restriction list (their no-fully-inlined-cycles rule is the *checked* version of our impossible-by-construction §3.6; their relations-in-aggregators restriction motivates §5 row 13), and Soufflé component instantiation flattens into per-instance dot-qualified namespaces — the same shape as our `q::?`/`q::helper` mangling.
- **Open ground** — a persisted, transactional, immediately-consistent named-rule catalog does not exist in the surveyed Datalog lineage (Soufflé/DDlog compile-time, Datomic per-query, LogicBlox defunct), and the graph-DB equivalents carry caveats ours doesn't (APOC: eventual consistency, plugin namespace; Kùzu: scalar-only, create-once). Same positioning class as the graph-projection spec's always-fresh claim.

## 3. Design

### 3.1 Surface and catalog

```
::query create <name> ($p: <col_type> default <const-expr>, …) { <single read-only query> }
::query remove <name>
::query list
::query show <name>
::query run <name>
```

- **Grammar**: new `query_op` alternative in both `sys_script` and `sys_script_inner` (baseline row 9; the `graph_op` template):
  - `query_op = {"query" ~ (query_create | query_remove | query_list | query_show | query_run)}`
  - `query_create = {"create" ~ ident ~ query_params_decl? ~ query_script_inner}` — the body is captured as raw text via `as_str()` exactly like trigger blocks (baseline row 5); `query_params_decl = {"(" ~ (query_param ~ ",")* ~ query_param? ~ ")"}`; `query_param = {param ~ (":" ~ col_type)? ~ ("default" ~ expr)?}` — deliberately mirroring `table_col` (baseline row 13) so users learn one declaration syntax.
  - `query_remove/show/run = {"remove"/"show"/"run" ~ ident}`; `query_list = {"list"}`.
  - Names are plain `ident` — no leading `_` (baseline row 3), no `:` (grammar-impossible), so a stored-query name can never be confused with a temp relation or an index relation.
- **SysOp variants** `CreateStoredQuery { name, params, body_text }`, `RemoveStoredQuery(name)`, `ListStoredQueries`, `ShowStoredQuery(name)`, `RunStoredQuery { name, param_pool }` (`run` captures the invocation's param pool at parse, the same way every parse consumes it — baseline row 1). Dispatch arms in `run_sys_op_with_tx`, each opening with the per-arm `if read_only { bail!(…) }` convention for `create`/`remove`; `list`/`show`/`run` are read-safe. No `AccessLevel` check on the arms themselves (index-create precedent).
- **Persistence: an ordinary reserved-name relation**, created lazily on first `::query create` (the `mnestic_evict_audit` precedent, baseline row 10 — and baseline row 8 is why *not* the SYSTEM keyspace: `::relations` decodes every row there as a `RelationHandle`):

  ```
  mnestic_stored_queries {
      name: String
      =>
      body: String,        # raw script text, exactly as written between the braces
      params: Json,        # ordered [{name, type: String?, default: <serialized DataValue>?}, …]
      head: Json,          # entry head column names, recorded at create for ::query list / tooling
      deps: Json,          # names of stored queries this body references (for the remove check, §3.6)
      description: String?,
      created_at: Float
  }
  ```

  This buys backup/`export_relations`/`import_relations`/restore — and the cross-instance memory envelope — **for free**, plus transparency: the definitions are queryable like any data (§2.1: Kùzu and APOC both had to build dedicated export paths for the same property). Direct user writes to this relation bypass create-time validation; a hand-mangled body fails loudly at next use, with the stored-query name in the diagnostic (§3.8 records this as accepted, documented behavior — same class as hand-editing any engine-maintained relation). Setting an `::access_level` on it is a deliberate, supported way to freeze the query catalog.
- Definitions are read **through the consuming transaction's own snapshot** — a stored query, like a trigger, is data. No in-memory mirror, no cache, no cross-process staleness class, and — unlike APOC's refresh-interval model (§2.1) — a definition is usable in the very transaction that can see it. The cost is one point-get per *unresolved* name per query (zero when the program resolves entirely locally — §3.3's pass touches storage only for names that would otherwise die as `RuleNotFound`).

### 3.2 Parameters: declaration, typing, defaults, scoping

**One rule, no tiers: every `$param` the body uses must be declared in the create head.** Create fails with "body references `$since` — declare it in the parameter list" (complete list obtained by a token-level walk over the body's `param` pairs, so the error names all of them at once). This is not ceremony — it is what makes `::query list` a complete, typed, machine-readable signature per query, which is precisely what the agent-tooling consumer needs to introspect before invoking. (§2.1: no surveyed system infers a signature from body text.)

- **Type is optional** (default: any value accepted). When declared, the supplied value is passed through `NullableColType::coerce` (baseline row 13) at each invocation — a loud error on mismatch, and **numeric coercion on the Int/Float boundary**. This kills a real footgun: joins compare structurally, so an un-coerced `1.0` against stored `1` silently matches nothing; a declared `Int` parameter makes that a coercion, not an empty result.
- **Default is optional**; when present it must be a constant expression, evaluated once at create via `Expr::eval_to_const` (baseline row 13; DuckDB documents the same definition-time evaluation for macro defaults — §2.1) and stored as a value. A required (default-less) parameter missing from the invocation's pool is a loud error naming both the stored query and the parameter — replacing the bare `ParamNotFoundError` whose span would otherwise point into text the caller never wrote.
- **Scoping: parameters are run-wide, not per-atom.** All references to stored queries in one script — and the stored bodies themselves, transitively — draw from the *invoking call's* param pool (plus per-query defaults, coerced per-query against each declaration; each body is parsed with its own adjusted copy, so two stored queries declaring the same name with different types coexist). There is **no per-call-site parameter binding**. The doctrine, stated in docs verbatim: *if a value should vary per call site, make it a head column and bind it positionally — that is what unification and magic sets are for; parameters are for run-wide knobs (thresholds, limits, as-of dates).* Upstream's own example already follows this (`principal` is a column, not a param), and it is the Datalog lineage's standing answer (§2.1, D7).

### 3.3 Invocation as a rule atom: the splice pass

The primary form. A caller references a stored query exactly like a local rule:

```
?[ch] := allowed_channels['alice', ch]
```

**Hook site.** One pass, `Db::resolve_stored_queries(&self, tx, &mut InputProgram, cur_vld)`, called from (a) the head of `run_query` and (b) the `::explain` arm before `into_normalized_program` (baseline row 6 — these two cover single queries, imperative-embedded programs, triggers, and explain; no other pipeline entry exists). The pass:

1. Walks every rule body's `InputAtom` tree (through negation and conjunction/disjunction nesting) **and every `FixedRuleApply`'s rule-typed input args**, collecting applied rule names not defined in `prog` — fixed rules were already resolved at parse (baseline row 2) and are not candidates.
2. For each candidate, point-reads `mnestic_stored_queries` through `tx`. Absent → fall through untouched; compile's existing `RuleNotFound` stays the error for genuine typos.
3. For each hit, parses the stored body with the adjusted param pool (§3.2) and the Db's **live** fixed-rule and custom-aggregate registries (unlike triggers' deliberately-empty registries, baseline row 5 — stored queries may use custom aggregates; a fixed rule present at create but unregistered at use fails loudly at this parse), then splices its rules into `prog` under mangled names, and recurses on the body's own stored references (§3.6 bounds this).

**Param-pool plumbing.** Parse consumes the pool (baseline row 1) and `run_query` never sees it — so `InputProgram` gains a field, `param_pool: Arc<BTreeMap<String, DataValue>>`, populated by `parse_query` and defaulted empty for programmatic AST builders (sanctioned by the module's instability note, baseline row 4). This is the design's only touch on a core type, and it is also the field a future plan cache keys late-binding on (§6).

**Hygiene (the macro property, pinned by tests).** Splicing is namespace-closed per definition:

- The entry (`?`) rule of stored query `q` is inserted as symbol **`q::?`**; its internal helper rules as **`q::<helper>`**. Rule idents cannot contain `:` (baseline row 3), so these symbols are grammar-unwritable — capture-proof, and still readable in `::explain` output. (Soufflé instantiates components into per-instance dot-qualified namespaces the same way — §2.1.)
- Caller atoms naming `q` are rewritten to `q::?` — **only when the caller does not define a local rule `q`**. A program-local rule always shadows a stored query (lexical scoping, innermost wins), and the shadowed case emits a `QueryWarning` into the diagnostics ring ("local rule 'q' shadows stored query 'q'") rather than an error — existing scripts must keep meaning what they meant before a colleague's `::query create` landed. The same rule applies *inside* a stored body: a body-local rule shadows a same-named catalog entry.
- References **inside** a stored body to other stored queries were resolved against the catalog at create time (§3.5) and are rewritten to their targets' mangled entries at splice — the caller's local names can never capture them. (This is the property that distinguishes splice from textual inclusion; the §5 matrix has a dedicated capture test.)
- A stored query referenced twice — directly, or via a diamond through two other views — is spliced **once**: mangled names are pure functions of (query name, internal name), so the operation is idempotent by construction.

**Arity** of the caller's atom must equal the stored entry's head arity — loud error naming both, with both spans.

**Out-options gate.** A stored body may carry query options — they are meaningful under `::query run` (§3.4). But options are program-level, and Datalog has no per-rule `:limit`: silently dropping them in atom form would change results invisibly. So atom-form use of a stored query whose body carries any non-default `QueryOutOptions` field (baseline row 11: sorters, limit, offset, as_of, assertion, timeout, mem_limit, sleep — `store_relation` is already unreachable, §3.5) **errors loudly**: "stored query 'q' carries `:limit` — invocable only via `::query run`". Checked per definition at splice, when that definition is actually referenced. (Soufflé's `inline` guards its splice with the same loud-restriction-list pattern — §2.1.)

**Everything downstream is untouched.** After splice the program is indistinguishable from one that was written by hand: stratification, magic sets, reorder, factorization, budgets, and access-level enforcement on the relations the body reads all apply unchanged. The semantics pin — and the primary test oracle — is: **atom-form results ≡ the hand-inlined (mangled) program's results**.

### 3.4 Standalone invocation: `::query run`

`::query run q` parses the stored body with the invocation's pool (params validated/coerced/defaulted per §3.2), runs the splice pass on the result (a stored query may reference others), and evaluates via `run_query` — the body runs **as itself**, so its own `:limit`/`:sort`/`:as_of` apply naturally, and the caller needs no arity knowledge; results come back with the entry's named columns. The arm re-checks `needs_write_lock().is_none()` after parse (defense against a hand-edited catalog row, §3.1) and evaluates read-only regardless of the session's mutability.

### 3.5 Create-time validation and the read-only restriction

`::query create` validates before persisting, in order, all loud:

1. Name is free (and is not the backing relation's own name).
2. Every body-used `$param` is declared (§3.2); declared-but-unused params are an error too (a stale signature is a lie to `::query list` consumers).
3. The body parses via `parse_script` → `get_single_program()` (baseline row 12: rejects imperative scripts and sysops), using a **synthetic pool**: each declared param's default if present, else the zero value of its declared type (`0`, `0.0`, `""`, `false`, `[]`, `null` for untyped). Zero-synthesis exists because parse is not evaluation-free — fixed-rule options and `:limit`-class positions evaluate at parse (baseline row 1's inlining feeds them) — and a `Null` dummy would false-fail them. Documented consequence: a param used in a parse-evaluated position whose zero value is rejected there needs an explicit `default` to pass validation.
4. **Read-only**: `needs_write_lock()` must be `None` (baseline row 7) — no `:put/:create/:rm/:update/:replace/:ensure` targets. This is the issue's v1 restriction, adopted: mutation-in-a-view raises trigger and permission questions that must not block the read case. (APOC's per-statement READ/WRITE mode is the precedent for making this a declared property later — §2.1.)
5. Every rule name applied in the body resolves: locally within the body, as a fixed rule (parse already did), or **in the catalog through the create's own tx**. Unresolved → create fails. Stored-relation (`*rel`) references are deliberately *not* checked — views over not-yet-created relations are legitimate and fail at use like any query.
6. The entry head's column names (`head`), the referenced stored-query names (`deps`), and the declared params are recorded on the row (§3.1).

### 3.6 Recursion, cycles, dependencies

- **Caller↔stored recursion is impossible by construction**, not rejected by a check: §3.5.5 means a stored body can never reference a rule it doesn't itself define (bodies are self-contained modulo the catalog), so no spliced rule can name a caller rule, and no SCC can span the boundary. Mutual recursion *among a stored body's own rules* is ordinary stratified Datalog and is allowed. (Soufflé's `inline` enforces the corresponding property as a checked rejection — "no cycle where every node is inlined"; ours is the stronger unexpressible form. §2.1.)
- **Stored↔stored cycles are impossible by construction**: create validates references against the already-committed catalog (§3.5.5), and there is no in-place update — a cycle would require referencing a query that doesn't exist yet. `::query remove` refuses while any other stored query lists the target in `deps` (one range scan of the small catalog at remove time; no reverse index — the Postgres `DROP … RESTRICT` default, §2.1, and the deliberate opposite of the DuckDB/SQLite drop-succeeds-error-at-use model). "Update" is remove + create, which preserves the invariant.
- **Defense in depth anyway**: the splice recursion carries a visited-set and a depth cap (32), bailing with "stored-query reference chain exceeds depth 32" — guarding against a hand-edited catalog row (§3.1) ever producing an infinite splice.

### 3.7 `::explain`

Works by placement, not by feature: the splice pass runs in the Explain arm before normalization (§3.3), so `::explain { ?[ch] := allowed_channels['alice', ch] }` shows the real spliced-and-magic-rewritten plan, with the view's rules visible under their `q::?` / `q::helper` names — the mangling scheme is part of the documented surface precisely so explain output reads well. `::explain { ::query run q }` is grammatically impossible (explain wraps a query program); the documented equivalent is explaining the atom form. A dedicated `::query explain` is future work, not v1.

### 3.8 Interactions (each verified against the mechanism it names)

- **Triggers**: trigger bodies go through `run_query`, so they resolve stored queries too — free and consistent. Trigger firing parses with an empty pool (baseline row 5), so a stored query used inside a trigger needs defaults for all its params; documented.
- **Imperative scripts**: embedded programs flow through `run_query` (splice applies); `::query create/remove` inside `{…}` blocks work via `sys_script_inner` + `run_sys_op_with_tx` (baseline row 9).
- **Bitemporality**: a caller's `:as_of` applies to the *spliced* atoms exactly as it would to hand-written ones (it is a program-level default over tt-atoms, baseline row 11) — no special interaction. A stored body carrying its own `:as_of` is atom-form-rejected by the out-options gate (§3.3) and honored under `run`. The backing relation itself is plain (not tt-stamped): definitions are current-only in v1.
- **Budgets/timeouts/memory**: the spliced program is one program on one tx — existing whole-script budgets apply with zero changes.
- **Access levels**: enforcement on the relations a view reads happens at evaluation, post-splice, unchanged — a view grants no bypass. The backing relation's own access level governs the catalog (§3.1).
- **Read-only sessions**: views and `run` are read-safe end-to-end; `create`/`remove` bail per-arm.
- **Cypher**: non-goal; the translator emits CozoScript and could target views later, nothing here blocks it.
- **Multi-process writers**: no new caveat — definitions are snapshot-read data (§3.1), not an in-process cache.

## 4. What v1 deliberately excludes

| Excluded | Why | Where the door stays open |
|---|---|---|
| Materialized / cached view results | Whole invalidation discipline (see the graph-projection watermark protocol) for unproven demand | The splice pass is the single place a materialization check would slot in |
| Plan cache | Blocked by parse-time `$param` inlining (baseline row 1) | §6 |
| Mutating bodies | The issue's own v1 restriction; triggers/permissions unresolved | §3.5.4 is one gate to relax; APOC's declared READ/WRITE mode (§2.1) is the shape it would take |
| In-place `::query update` | Would reintroduce the cycle problem §3.6 defines away | **v2 path, recorded**: `::query replace` guarded by a transitive `deps`-walk cycle check (cheap — `deps` is already recorded per row), landing **together with** whatever invalidation story the §6 plan cache needs by then — APOC's overwrite-plus-`db.clearQueryCaches()` footgun (§2.1) is the documented failure mode of shipping replace and a plan cache uncoupled. Until then: remove + create |
| Per-atom parameter binding | Head columns + magic sets already do it, better (§2.1: the Datalog lineage's answer) | §3.2 doctrine |
| Binding-mode annotations on view heads (Datomic's required-bindings `[?x]`, §2.1) | No demand yet; pure addition later | The `head` column can grow bound/free markers without a schema break |
| `::query explain` | Atom form under `::explain` covers the need | §3.7 |

## 5. Test matrix (sqlite + tempdir throughout — baseline row 14; restart tests reopen the store)

1. **Equivalence oracle**: for a 3-deep view chain (upstream's `allowed_users→channels→threads` shape), atom-form results ≡ hand-inlined mangled program, with and without a bound first column (magic-set path).
2. **Hygiene/capture**: caller defines local `allowed_users`; a referenced view that *internally* references stored `allowed_users` must bind the catalog's, not the caller's. Shadow-warning emitted for the caller's own atom.
3. **Idempotent diamond**: A refs B and C, both ref D — D spliced once; results correct.
4. **Params**: missing required (loud, names query+param); default applied; declared-type coercion incl. the Int/Float join case (un-coerced would return empty — the test asserts non-empty); undeclared-in-body and declared-unused both fail create.
5. **Out-options gate**: body with `:limit` — atom form errors naming the option; `run` honors it.
6. **Read-only gate**: body with `:put` fails create; hand-edited catalog row with `:put` fails at `run` (defense re-check).
7. **Recursion/deps**: create referencing a missing stored query fails; remove-while-depended-upon refuses; hand-edited self-referencing row hits the depth bail.
8. **Arity mismatch** loud error; **fixed rule unregistered at use** loud error.
9. **Persistence**: create → reopen store → atom form and `run` still work; definition survives `export_relations`/`import_relations` round-trip.
10. **`::explain`** on the atom form succeeds and its output contains `q::?`.
11. **Trigger body** referencing a defaulted stored query fires correctly.
12. **Grammar**: create in both `sys_script` and `sys_script_inner`; name rejected with leading `_`.
13. **Aggregation/negation over spliced atoms** (survey-driven — §2.1, Soufflé's `inline` restricts exactly these): caller aggregating over a view atom (`?[count(x)] := q[x]`), a view whose own entry rule aggregates, and a negated view atom (`not q[…]`) — each ≡ its hand-inlined equivalent.

## 6. The plan-cache path (recorded, not built)

The blocker is baseline row 1: params inline as `Expr::Const` at parse, so a parsed AST is specific to one param valuation and text→AST memoization buys nothing reusable. The enabling change is a symbolic `Expr::Param` variant bound at evaluation (or a substitution pass over a param-normalized AST) — an evaluator-touching change that must not ride in this feature. What v1 pins so the cache stays reachable: a **stable identity** per query (name + body text + typed param signature in one catalog row), a **single parse funnel** (every invocation-time parse of stored text goes through one function the cache would wrap), and the `param_pool` field on `InputProgram` (the late-binding seam). When the cache comes, its key is (definition row, backend schema epoch) and its invalidation is `::query remove` — no watermark protocol needed for parse artifacts, unlike data caches.

Prior-art anchors (§2.1): Datomic already runs the target shape — its query cache is keyed on structural equality of the query with the documented advice "use parameterized queries instead of building dynamic queries," i.e. stable identity + late-bound inputs, exactly what v1 pins. Postgres's generic-vs-custom plan machinery is the canonical warning about parameter-sensitive planning; mnestic's deterministic planner escapes most of it, **with one real instance**: the fork's equality-pushdown pre-pass (`query/reorder.rs::push_equality_filters_to_bindings`) fires on `eq(var, ground-const)`, so a future symbolic `Expr::Param` must still register as *ground* to that pass — and as bound to magic sets — or cached plans silently lose the fork's own point-read win. And APOC's `db.clearQueryCaches()` footgun is the standing reminder that a replace primitive (§4) and the plan cache must ship with one invalidation story, together.

## 7. Decisions (all signed by owner 2026-08-18)

All seven recommendations were adopted as signed decisions on 2026-08-18, after the §2.1 prior-art survey.

| # | Decision | Signed decision | Prior art (§2.1) |
|---|---|---|---|
| 1 | Catalog = ordinary reserved-name relation `mnestic_stored_queries` (vs SYSTEM keyspace / new keyspace) | **Relation** — baseline rows 8 & 10; backup/export/cross-instance free | Kùzu and APOC both built dedicated export paths for their catalogs; ours rides existing tooling free |
| 2 | Params must all be declared; type + default optional | **Yes** — typed introspectable signatures are the agent feature | DuckDB macros = the exact contract, incl. definition-time defaults; Postgres stricter (types mandatory); no surveyed system infers from body text |
| 3 | Local rule shadows stored query: warning (vs hard error) | **Warning** — pre-existing scripts must not break when a name lands in the catalog | No surveyed system faces this collision: SQL forbids it in one namespace at create (unavailable to us), Datomic dissolves it (rules are per-query data) |
| 4 | Atom-form rejects bodies with non-default out-options (vs silent drop / partial apply) | **Reject loudly** | Soufflé's `inline` guards its splice with the same loud restriction-list pattern |
| 5 | No in-place update in v1 (remove+create only) | **Yes** — it is the cycle-freedom proof; v2 replace path recorded in §4 | For: Postgres `DROP … RESTRICT` default = our remove-refuses; released Kùzu is create-once. Against: Postgres/DuckDB/APOC all offer replace — and APOC's documents the stale-plan-cache footgun ours avoids |
| 6 | `::query run` ships in v1 (not just the atom form) | **Yes** — it is the invoke-by-name deliverable and ~free once splice exists | Invoke-by-name is the norm for every stored-statement system surveyed (APOC `CALL custom.*`, Kùzu macros, Postgres functions) |
| 7 | Param scoping is run-wide, per §3.2 doctrine | **Yes** | The Datalog lineage's answer (Datomic positional unification; Soufflé instantiation-time binding); the SQL lineage's per-call args are served by head columns |

## 8. Phasing sketch

- **Phase 1 — catalog + sysops**: grammar, `SysOp` variants, backing relation, create-time validation (§3.5), `list/show/remove` incl. the deps check. No splice yet; `run` lands here for bodies with no stored references.
- **Phase 2 — the splice pass**: `resolve_stored_queries`, the `InputProgram::param_pool` field, hygiene/mangling, out-options gate, explain-arm call, shadow warning, full `run`. The §5 matrix lands with it.
- **Docs**: `mnestic-docs` skill flow (examples validated by `doc_check`); CHANGELOG-FORK entry; README "What mnestic adds" line — at release, per the standing checklist.
