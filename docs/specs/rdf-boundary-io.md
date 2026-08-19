# Spec — RDF at the Boundary: Turtle-family readers, IRI helper functions, and round-trip export — without becoming a triple store

_Created 2026-08-16. Status: **SHIPPED in 0.15.0 — §12's seven decisions signed by the owner 2026-08-16: Q1–Q5 and Q7 as proposed; Q6 overridden (the PyPI wheel ships the reader passthrough in v1 — see the Q6 row and §4).** (Originally: PROPOSED — awaiting owner sign-off.) This is the dedicated spec behind the public [`ROADMAP.md`](../../ROADMAP.md) "RDF at the boundary, not in the core" item. Grounded two ways: (a) against the shipped fixed-rule/reader/function/export machinery — citations are file:line in `cozo-core/src/`, gathered and spot-verified 2026-08-16; (b) against the same-day prior-art sweep (RDFox/Jena TDB/Virtuoso interning norms, the DuckDB `rdf` community extension, Kùzu's RDFGraphs removal post-mortem, W3C Direct Mapping, and the oxttl/oxiri crate evaluation — §10 is the citable summary). The posture, in one line: **use RDF/OWL/SHACL as specification and interchange languages; use Datalog as the implementation** — RDF converts to ordinary relations at the edge, and nothing inside the engine knows it came from RDF._

> **Anti-overbuild guardrails — the Kùzu line.** Kùzu shipped RDF as a first-class citizen — a catalog entry, an `RDF_VARIANT` type, grammar and binder awareness — and deleted the whole thing in v0.7.0; the DuckDB community `rdf` extension ships the boundary-only form and thrives. This spec stays structurally on the surviving side: **one reader FixedRule + a handful of pure scalar functions + one Rust export API. Zero grammar changes, zero new sysops, zero new catalog concepts, zero new column types, zero planner awareness of "RDF-ness."** The fixed-rule surface enforces this for free: `parse_fixed_rule` resolves an opaque name against the registry (`parse/query.rs:1165-1167`) and the planner sees an opaque `FixedRuleApply` — a reader cannot leak RDF semantics into the engine even by accident. The dictionary-interned IRI *datatype* stays an **evidence-gated later phase**: this spec only reserves its slot (§9) and records why the gate is the industry norm, not caution. Explicitly out: SPARQL (product-side exception-only; never engine), OWL/RDFS entailment (a rules-library concern at most), R2RML-style relational→RDF mapping export (a second product surface — the exact slope Kùzu slid down), JSON-LD (rides the tree-data onboarding item (upstream cozodb/cozo#283), a different parser family), and named-graph *semantics* beyond a passive graph column._

---

## 1. Why / what this buys

Stable, dereferenceable identifiers and standard interchange formats are what make semantic-web data portable; triple stores are what make it painful. The pattern with living precedent splits the difference: read Turtle into ordinary relations, treat IRIs as opaque stable keys, evaluate with Datalog, translate back at the edge. RDFox proved the engine half at scale (dictionary-encoded IRIs over tuple tables, Datalog materialization — though RDFox itself remains a full RDF/SPARQL store); Nemo carries the boundary rule-engine form in Rust; the DuckDB `rdf` extension proves the reader-into-relations form in a relational engine. mnestic already models knowledge as relations, and Datalog covers the standard RDF rule profiles (RDFS entailment and OWL 2 RL are Datalog-implementable). The missing piece is purely intake and outflow:

1. **Readers** for the Turtle family (Turtle, N-Triples, N-Quads, TriG) into a fixed relational shape.
2. **IRI helper functions** (validation, resolution, CURIE expand/compact) so boundary identity handling is one function call, not string surgery.
3. **Round-trip export** of already-triple-shaped relations back to Turtle/N-Triples.

The adopter story this opens: the practitioner posture of "IRIs as keys, read/write Turtle, the RDF people are happy, and the software never bows to the triple store" — an on-ramp for semantic-web-adjacent users holding RDF corpora, at a cost bounded to one reader and some string functions.

## 2. Shipped baseline this builds against (verified 2026-08-16)

| Piece | Where | What transfers |
|---|---|---|
| CsvReader/JsonReader as `FixedRule` impls, invoked via `<~` — `res[...] <~ CsvReader(types: [...], url: 'file://...', has_headers: true)` then an ordinary rule + `:put`/`:replace` | `fixed_rule/utilities/csv.rs:28`, `jlines.rs:33`; usage `tests/air_routes.rs:36-46` | The invocation shape verbatim. **There is no `LOAD FROM` syntax and this spec does not add one** — the roadmap's "LOAD FROM" phrase borrowed Kùzu's scan-clause vocabulary; the actual surface is `<~` |
| The 0.14.0 opt-in gate: `#[cfg(feature = "data-import")]` on registry entries (`fixed_rule/mod.rs:1117-1126`) + on the utilities modules (`utilities/mod.rs:10-22`), asserted by `tests/data_import_security.rs:4-16`; HTTP is the separate `requests` feature with the `file://`-prefix split (`csv.rs:150-172`) | `cozo-core` | Extended, not invented: the RDF reader gates the same three points and keeps the same file-vs-HTTP split |
| `FixedRule` trait: `init_options` (parse-time, may mutate options), `arity` (parse-time, from options alone — `parse/query.rs:1193`), `run(payload, out: &mut RegularTempStore, poison)` (`fixed_rule/mod.rs:792-831`); typed option helpers with miette spans (`mod.rs:608-766`) | `fixed_rule/mod.rs` | The whole implementation skeleton |
| Reader row-typing precedent: CsvReader converts cells by declared `ColType` with nullable fallback, bails on unconvertible (`csv.rs:95-142`) | `csv.rs` | The vocabulary for §5's literal-typing decision — but the RDF reader's default is *no* coercion (see the `json()` caution below) |
| The `json()` mangling caution: `Bytes` → base64 string, `Uuid` → plain string at the JSON boundary (`data/json.rs:77,88-90`) — type identity silently erased | `data/json.rs` | Why objects default to lexical form + explicit datatype column, never silent coercion |
| Scalar-function pattern: `define_op!` (`functions.rs:42-51`), one `get_op` match arm (`expr.rs:794+`), fork-family precedent `dt_*` (`functions.rs:2734-2759`, `expr.rs:935-945`); tests in `src/data/tests/functions.rs` + `tests/spec_doc_validation.rs`. **No builtin scalar function is feature-gated** (zero `#[cfg]` in the `get_op` registrations) | `data/functions.rs`, `data/expr.rs` | The helper-function skeleton; and the constraint that forces §12 Q3 (a dep-backed helper either takes the dep unconditionally or invents a gating precedent) |
| Export today: `Db::export_relations` — Rust API, full range scan, `BTreeMap<String, NamedRows>` (`runtime/db.rs:820-859`); **no `::export` sysop exists** (`parse/sys.rs:30-95`); cozo-bin exposes HTTP routes over the Rust APIs (`server.rs:535-538`) | `runtime/db.rs` | The export surface pattern: a sibling Rust API, optional cozo-bin route, no sysop |
| Bulk path: `Db::import_relations` — pre-created relation, one write tx, B-tree indexes only, HNSW/FTS/LSH stranded with a warning pointing at `::reindex` (`db.rs:869-970`, warn :905-911) | `runtime/db.rs` | Documented as the big-corpus escape hatch (§6); not rebuilt |
| Skolemization primitives: `rand_uuid_v1/v4`, `uuid_timestamp`, the ULID family; uuid `v5` feature already enabled (`functions.rs:2992-3108`; `Cargo.toml:145`) | `data/functions.rs` | Deterministic blank-node skolemization needs no new deps |
| Optional-dep feature pattern: `requests = ["dep:minreq"]` (`Cargo.toml:56,153`); by contrast `csv` is an unconditional dep (:146) — 0.14.0 gated only *registration* | `Cargo.toml` | §4's feature wiring — deliberately **stricter** than the data-import precedent: the RDF parser dep must be `optional = true` |
| MPL headers: fork-authored files carry `Copyright 2026, Shan Rizvi (mnestic fork).` + MPL block (`utilities/rrf.rs:1-7`) | repo convention | New files' headers |

## 3. Design — the reader

**One registry name: `RdfReader`.** Registry names are forever (duplicate registration bails, `db.rs:1265-1270`), and oxttl hands us four formats in one crate — one name with a `format` option beats four names frozen into the registry (§12 Q1 confirms).

```
triples[s, p, o, g, lang, dt] <~ RdfReader(url: 'file://./data.ttl')
?[s, o] := triples[s, 'http://xmlns.com/foaf/0.1/knows', o, _, _, _]
```

- **Options**: required `url` (`file://` prefix = local path; anything else requires the `requests` feature — the exact `csv.rs:150-172` split); `format` in `{'turtle','ntriples','nquads','trig'}`, defaulting from the URL extension (`.ttl`/`.nt`/`.nq`/`.trig`), error if neither determines it; `base` (base IRI for resolution, validated — oxttl `with_base_iri`); `prefixes` (map option; injected via `with_prefix`); `prepend_index` (default false, the `csv.rs:81-91` counter precedent).
- **Output shape — the DuckDB 6-column shape, verbatim**: `subject, predicate, object` always populated; `graph, language_tag, datatype` nullable. All six are `DataValue::Str`/`Null` today — zero engine type changes (`value.rs:146-174`). Triple formats emit `Null` graphs; quad formats fill them. `arity` returns 6 (+1 under `prepend_index`), computable from options alone at parse time as the trait requires.
- **Term encoding**: IRIs as plain strings (no wrapping); blank nodes as their `_:label` lexical form by default (§5); literals as lexical form in `object` with `language_tag`/`datatype` carrying what the syntax carried (plain literals: both `Null`; `xsd:string` normalized to `Null` datatype — §12 Q4). **No silent coercion of typed literals** — the `json()` Bytes/Uuid mangling is the in-repo record of where silent type-identity loss leads. A later `types:`-style opt-in coercion option can reconcile convenience when demand shows.
- **Errors**: abort on first syntax error with the parser's message and byte position (CsvReader's posture, `csv.rs:155`). oxttl's error-recovery mode (collect-and-continue) and `lenient()` are **excluded from v1** — both are silent-wrong-answer machinery until someone needs them, and the errors can name the option when it exists.
- **Memory posture, stated honestly**: FixedRule output materializes into a `RegularTempStore` BTreeMap (`temp_store.rs:27-29`) before the consuming rule runs — the parse streams (oxttl `for_reader` pull iterator), the rows do not. For bulk corpora the documented escape hatch is `import_relations` (§6). The reader's materialization is charged by the query memory budget when [`memory-budget.md`](./memory-budget.md) lands (its FixedRule output store is one of the charged structures) — the two specs compose rather than overlap.
- **Implementation**: `fixed_rule/utilities/rdf.rs`, new file, fork MPL header. oxttl parser instance per invocation (blank-node label scope is one parser instance — one document, one parser). Poison consulted every 4,096 parsed triples — a deliberate improvement on the shipped readers, which bind `_poison` and never check it (`csv.rs:35`, `jlines.rs:40`); §11 test 7 pins the cadence.

## 4. Design — feature wiring

New feature: **`rdf-io = ["data-import", "dep:oxttl", "dep:oxiri"]`**, with `oxttl`/`oxiri` as `optional = true` dependencies.

- **Implying `data-import` is deliberate**: data-import is the trust gate for script-controlled file readers (the 0.14.0 security posture — `CHANGELOG-FORK.md:38-44`); an RDF reader is exactly such a reader and must not create a second gate semantics. Enabling `rdf-io` therefore also registers CsvReader/JsonReader — acceptable, because the gate's meaning is "this deployment trusts scripts to read local files," not "which formats."
- **Stricter than precedent, on purpose**: PR #36 gated registration but left the `csv` crate an unconditional dependency (`Cargo.toml:146`). Here the parser deps compile **only** under `rdf-io` — the default build gains zero bytes and zero dependencies. (`oxiri` may additionally be pulled unconditionally by §12 Q3's helper decision; that is that question's explicit trade.)
- Registration cfg'd at the three shipped points (registry entry, utilities module, `data_import_security.rs` assertion extended to `RdfReader`). HTTP URLs additionally require `requests`, exactly as CSV/JSON do.
- **Python wheel**: `cozo-lib-python` has no data-import passthrough today (its feature list stops at compact/storage/graph-algo/requests/cypher — `cozo-lib-python/Cargo.toml:18-46`), so the wheel cannot expose the reader without a deliberate passthrough addition — a separate, owner-visible decision (§12 Q6), consistent with 0.14.0's "the wheels expose no script-controlled reader."
- **Never re-export oxttl/oxrdf/oxiri types** from the public API. The `graph` crate's public-dependency semver lesson (a re-export makes the dep's version semver-load-bearing forever) is standing policy: the reader surfaces only `DataValue` rows and message-carrying errors, keeping the RDF crates freely upgradable.

## 5. Design — blank nodes

Default: emit `_:label` lexical forms unchanged. This is honest (labels are file-scoped and the reader is per-file) and round-trip-faithful for the single-file case. The cross-file join unsoundness (two files' `_:b0` are different nodes) is documented, and an opt-in option closes it:

- `skolemize: <namespace-iri>` — rewrite each blank node to a deterministic IRI: `uuid_v5(namespace, source_salt + label)` rendered under the given namespace (the W3C genid convention; uuid v5 is already available, `Cargo.toml:145`). The salt derives from the **source** — the resolved `url`, or a content hash where a stronger identity is wanted (§12 Q2 picks) — so two loads of the same file agree and two different files disagree: idempotent re-loads, the correct behavior for the "turn RDF into a real graph" use.

## 6. Design — export and the bulk path

**Export**: a sibling Rust API, `Db::export_relation_as_rdf(relation, format, options) -> String` (or writer-taking variant), mirroring `export_relations`' scan semantics (`db.rs:820-859`). It accepts only relations whose columns match the 6-column shape (by position; a `columns:` mapping option may name which columns play s/p/o/g/lang/dt for relations that carry extras). Prefix mappings arrive as an **argument map** — reading them from a conventionally-named relation would be a catalog concept through the back door, exactly what the guard-rail forbids. Serialization via oxttl's serializers (same feature gate). **Documented round-trip caveat (build finding, 2026-08-16)**: Q4's normalization plus the 6-column shape make an object IRI and a plain string literal indistinguishable in storage, so export emits an object with Null lang+datatype as an IRI when its text parses as an absolute IRI (`_:` prefix ⇒ blank node) and as a plain literal otherwise — a plain literal whose lexical form is a valid absolute IRI round-trips as an IRI. Inherent to the signed shape; stated prominently on the export method's docs. An optional cozo-bin HTTP route may follow the `/export/:relations` precedent (`server.rs:535-538`); **no sysop** (no precedent, new surface, not needed).

**Scope guard**: this is *round-trip* export — relations already in triple shape go back out as Turtle/N-Triples. General relational→RDF mapping export (R2RML/Direct-Mapping machinery: IRI templates over keys, blank nodes for keyless rows) is **excluded**; every surveyed system that exports arbitrary relational data does it through a mapping layer, and that is a product surface, not an engine boundary function. The W3C Direct Mapping's row-IRI construction is instead served composably: `curie_expand`-style helpers + string concat in ordinary Datalog mint IRIs from keys when a user wants them.

**Bulk corpora**: v1's answer is the shipped path, documented in the reader's docs — parse with the reader into a temp relation *or* pre-shape rows host-side and use `import_relations` (`db.rs:869-970`: pre-created relation, one tx, B-tree indexes only, then `::reindex` for search indexes). No new streaming-ingest API in v1; if real corpora blow past the materialized path, that evidence goes to the same gate as the IRI datatype (§9).

## 7. Design — IRI helper functions

Four pure scalar functions, `define_op!`/`get_op` pattern, `dt_*`-family style registration comment:

- `iri_valid(s) -> Bool` — RFC 3987 validation.
- `iri_resolve(base, rel) -> Str` — relative-IRI resolution (errors on invalid base).
- `curie_expand(prefix_map_json, curie) -> Str` and `curie_compact(prefix_map_json, iri) -> Str` — prefix-map-driven CURIE↔IRI conversion; the map is a `Json` value so scripts can carry one binding.

The dependency question is §12 Q3: scalar functions are never feature-gated (zero `#[cfg]` in `get_op`), so oxiri-backed helpers mean **oxiri becomes an unconditional dependency** (small, pure-Rust, zero-transitive-deps — the proposed answer), or the helpers hand-roll validation (a correctness risk RFC 3987 does not deserve), or a feature-gated-function mechanism gets invented (new engine surface; rejected). Tests in `src/data/tests/functions.rs` + a `spec_doc_validation.rs` section tied to this spec.

## 8. What is deliberately NOT in v1

SPARQL (any form, ever, engine-side — the product layer owns the exception path); OWL/RDFS entailment (users write the Datalog; a documented recipe may follow); JSON-LD (the upstream cozodb/cozo#283 tree-data machinery, different parser family); RDF/XML (oxttl doesn't parse it; demand-gated); N3 reasoning syntax; named-graph *semantics* (the `graph` column is passive data); error-recovery/lenient parse modes; a streaming ingest API; any new sysop, grammar production, `ColType`, or catalog object; re-exports of RDF crate types.

## 9. The reserved slot — dictionary-interned IRI datatype (later phase, evidence-gated)

Recorded here so the eventual design starts warm; **nothing below is v1 work.**

- **The gate is the industry norm**: Jena TDB inlines small values into the NodeId and dictionaries the rest; Virtuoso dictionary-encodes only literals >12 chars; interning is a measured optimization behind a threshold everywhere it ships. mnestic's gate: a real corpus where plain-`Str` IRIs measurably hurt (key size, cache pressure, join cost) beyond what prefix-compact CURIEs recover.
- **The slot's mechanics, pre-surveyed**: a new `DataValue` variant must simultaneously choose (a) an **end-of-enum position** — stored non-key value blobs decode via `rmp_serde::from_slice` (`relation.rs:912`), which identifies enum variants positionally, so mid-enum insertion breaks stored data (`relation.rs:100-104` is the in-repo record of the same rmp-serde positionality for the struct case) — and (b) the free memcmp tag byte **0x0E** for stored-key order (`memcmp.rs:21-37`), with arms in `encode_datavalue`/`decode_from_key`, `data/json.rs`, `ensure_same_value_type` (`functions.rs:53-74`), and `ColType`/`parse_type` if column-typed. The two orderings (derived `Ord` for temp stores vs memcmp for storage) already disagree upstream (Vec: tag 0x04, enum position 10) — the variant's sort story must be written down, not assumed.
- **The interning catalog must confront eviction**: Oxigraph's `id2str` dictionary is insert-only with no refcounting or GC — a never-delete leak. mnestic ships audited hard deletion (`::evict`); an interning table that cannot forget contradicts it. Any design must pick refcounting, epoch GC, or documented-leak-with-compaction — this tension is half the reason the phase is gated.

## 10. Prior art (verified against public sources, fetched 2026-08-16)

RDFox (dictionary-encoded IRIs → integer tuple tables, Datalog materialization; itself a full RDF/SPARQL store — we take the engine half only) · Nemo (Rust Datalog reasoner; traditional syntax; reads/writes the Turtle family with IRI values) · DuckDB `rdf` community extension (the direct living precedent: readers into a 6-column relational shape, no triple-native storage; its write path needs R2RML — where our scope line comes from) · Kùzu RDFGraphs (deep integration shipped and deleted in v0.7.0 — the guard-rail) · rdf_fdw (lossy plain-column shape retrofitted later — why six columns, not three) · Jena TDB / Virtuoso (gated interning norms) · W3C Direct Mapping (IRIs-from-keys, standards-grade) · oxttl/oxiri (chosen crates: MIT OR Apache-2.0, streaming pull parser, sans-IO chunk mode, active; rio is officially dead — "Unmaintained crate, use oxttl"; sophia alive but a full toolkit with a second hand-rolled parser stack).

## 11. Test matrix (sqlite backend per the repo test-backend rule)

1. **Format coverage**: each of the four formats parses a conformance fixture into the 6-column shape; triple formats emit Null graphs; TriG/N-Quads fill them.
2. **Round-trip**: reader → relation → `export_relation_as_rdf` → reader again is term-identical (modulo blank-node labels without skolemization; identical with it).
3. **Literal fidelity**: language tags, datatyped literals, and plain literals survive with lexical form + tag/datatype columns intact; `xsd:string` normalization pinned per Q4's ruling.
4. **Blank nodes**: default labels file-scoped; `skolemize:` produces IRIs stable across re-loads of the same source and distinct across sources.
5. **Base/prefix resolution**: `base` + `prefixes` options resolve relative IRIs and CURIEs per RFC 3987 (oxiri oracle).
6. **Security gating**: `data_import_security.rs` extended — `RdfReader` registered iff `rdf-io`; non-`file://` URL without `requests` bails with the named-feature error; default build has no oxttl symbols (feature-matrix compile check).
7. **Error posture**: first syntax error aborts with position; no partial rows observable in the consuming rule.
8. **Helpers**: `iri_valid`/`iri_resolve`/`curie_*` unit tests + a `spec_doc_validation.rs` section; invalid-base and non-IRI inputs produce typed errors, not panics.
9. **Arity/options**: parse-time arity check, unknown-format error, extension-based default, `prepend_index` parity with CsvReader.

## 12. Decisions (signed by the owner 2026-08-16 — Q6 overridden, the rest as proposed)

| # | Question | Proposed |
|---|---|---|
| Q1 | One `RdfReader` with `format:` vs per-format names | **One name** — registry names are forever; oxttl makes formats an option, not an architecture |
| Q2 | Blank-node default | **Labels as-is; opt-in `skolemize:` (uuid v5, per-invocation salt)** |
| Q3 | oxiri for helpers: unconditional dep vs hand-rolled vs invent gating | **Unconditional oxiri** (small, pure-Rust; correctness of RFC 3987 is worth one dep) — the one place this spec adds default-build weight; veto here reverts helpers to hand-rolled validation only |
| Q4 | `xsd:string` datatype: normalize to Null or carry explicitly | **Normalize to Null** (RDF 1.1 semantics; plain and xsd:string literals are the same term) |
| Q5 | Export shape: strict 6-column only vs `columns:` mapping option in v1 | **Strict in v1**; the mapping option is additive later |
| Q6 | Python wheel passthrough for `rdf-io` | **OVERRIDDEN at sign-off: ship the passthrough in v1.** `cozo-lib-python` gains an `rdf-io` passthrough feature and the published wheel builds with it — a deliberate reversal of the 0.14.0 wheel-reader posture, trading the locked-down default for reach (Python users are the likeliest RDF-corpus holders). Stated plainly, because the gate is one trust decision: `rdf-io` implies `data-import`, so the wheel also regains `CsvReader`/`JsonReader`, and since the wheel already compiles `requests`, script-controlled HTTP fetch becomes reachable too. The wheel README and the release migration notes must say all of this loudly. Implementation latitude for the build phase: if per-instance opt-in registration (compiled-in readers registered via a one-line Python call on `register_fixed_rule`) proves cheap, it may deliver the same reach with a no-reader per-instance default — a refinement of this ruling, not a reversal of it |
| Q7 | Typed-literal opt-in coercion (`types:`-style) in v1 | **No** — lexical + datatype column only; coercion is additive when demand shows |

## 13. Delivery

Engine-only, additive, no storage-format change, no bridge change. Ships in a normal banked minor (it need not ride the operations-and-trust minor; it composes with [`memory-budget.md`](./memory-budget.md) but depends on nothing). Doc-sync at release: public `ROADMAP.md` item moves to shipped **and its "LOAD FROM" wording is corrected to the `<~` reader surface in the same sync** (the separate "`LOAD FROM` Parquet/Arrow" item reworded to match), the crate/PyPI readmes gain the reader one-liner, and a docs page (mnestic-docs pipeline, examples validated against the engine) introduces the boundary posture with the reader + helpers + export in one worked example.
