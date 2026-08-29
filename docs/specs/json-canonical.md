# Spec — Canonical DataValue→JSON conversion: one representation at every nesting depth (mnestic #54)

_Created 2026-08-28. Status: **DRAFT — awaiting owner review**. Tracks [issue #54](https://github.com/shuruheel/mnestic/issues/54) (upstream [cozodb/cozo#269](https://github.com/cozodb/cozo/issues/269)). Roadmap gate, quoted verbatim from [`docs/strategy/MNESTIC-ROADMAP.md:33`](../../../docs/strategy/MNESTIC-ROADMAP.md): `| 5 | Canonical nested JSON conversion (#54) | Ready for design | 2 | 4 | Compatibility contract before output changes |`. A spec never self-approves._

**Provenance (all five passes run, 2026-08-28):** Pass 1 grounding (3 lenses) · Pass 2 lean draft · Pass 3 adversarial panel — 26 findings (3 blocker, 9 major, 14 minor; 5 are duplicates of an earlier finding), **26 folded, 0 rejected**, triage in §7 · Pass 4 prior art — 13 decision verdicts (9 CONFIRMS, 1 CONTRADICTS, 3 GAP) plus one essential omission, all folded and cited at the decision · Pass 5 simplification — 6 requirement-neutral cuts applied (§7), 2 owner rulings listed below. **Blockers still open: 0.** A second probe binary confirmed every panel-predicted value (§2 rows marked *(probe)*).

**Owner rulings requested (not applied — each amends a premise outside this spec):**

1. **Release placement.** Proposed 0.18.0 (§D5). Alternative: bank into 0.17.0, which is merged but unreleased and whose verify gate is still ahead of it (`MNESTIC-ROADMAP.md:29`, `versions.toml:28` = 0.16.0).
2. **NaN.** This spec keeps NaN → `null` at every depth so no top-level form changes except `Bot`. Prior art treats the non-finite family uniformly (Postgres `"NaN"`/`"Infinity"`/`"-Infinity"`); switching to `"NAN"` would be a second deliberate top-level change (§7).

## 1. Outcome and scope

Every `DataValue` that crosses the JSON boundary gets **one** representation, and that representation does not depend on whether the value is a top-level result cell, a member of a `{..}` map literal, an element of a nested list, the payload of `json()` / `json_object()` / `set_json_path()` / `to_string()`, or a value coerced into a `Json` column by `:put`.

This is a serialization contract, not a type system:

- **One outbound seam.** The recursive helper in `data/functions.rs` and the hand-rolled `ColType::Json` coercion arm in `data/relation.rs` are deleted; the `data/json.rs` conversion becomes the only DataValue→JSON policy in the engine (D2).
- **Top-level forms are already the contract.** Every host surface (HTTP, C, wasm, Java, Swift, embedded Rust) already emits the `json.rs` policy through `NamedRows::into_json`. The canonical table (D1) therefore *is* today's top-level table, with one deliberate change (`Bot`). The observable change is confined to nested values, `to_string()`, JSON keys derived from non-string values, and what `:put` stores in a `Json` column from now on.
- **One-way.** JSON→DataValue is declared lossy and left as it is (D3). No type sniffing is added on the way back.
- **Engine-only.** mindgraph-rs keeps its own snapshot converter, pinned by one test but not rewritten (D4, D8). Native bindings' non-JSON value conversions are out of scope (D1 scope note).
- **Compatibility contract first.** D5 is the deliverable the roadmap gate names; the code change is Batch A and cannot merge before D5 is owner-approved.

**Worked example — the issue script, before and after** (probe output, 2026-08-28; the `after` column is what D1 + D2 require):

| Cell | Before (today) | After |
|---|---|---|
| `x = to_uuid("a6bba931-e2a6-4040-8816-2b33603acf84")` | `"a6bba931-e2a6-4040-8816-2b33603acf84"` | unchanged |
| `m = {"x": x}` | `{"x":[166,187,169,49,226,166,64,64,136,22,43,51,96,58,207,132]}` | `{"x":"a6bba931-e2a6-4040-8816-2b33603acf84"}` |
| `s = to_string(x)` (57 chars today) | `"[166,187,169,49,226,166,64,64,136,22,43,51,96,58,207,132]"` | `"a6bba931-e2a6-4040-8816-2b33603acf84"` (36 chars, no quotes) |
| `{x: 1}` / `json_object(x, 1)` (key from a Uuid) | `{"[166,187,…,132]":1}` | `{"a6bba931-e2a6-4040-8816-2b33603acf84":1}` |
| `b = decode_base64("AQID")` / `{"b": b}` | `"AQID"` / `{"b":[1,2,3]}` | `"AQID"` / `{"b":"AQID"}` |
| `i = 1.0/0.0` / `{"inf": i, "ninf": -i, "nan": 0.0/0.0}` | `"INFINITY"` / `{"inf":null,"nan":null,"ninf":null}` | `"INFINITY"` / `{"inf":"INFINITY","nan":null,"ninf":"NEGATIVE_INFINITY"}` |
| `:put r {k => j}` with `j = x` into a `Json` column, read back | `[166,187,…,132]` | `"a6bba931-e2a6-4040-8816-2b33603acf84"` (new rows only — D5) |
| `vld` from a `Validity` column / `{"vld": vld}` | `[1787956506172712,true]` / same | unchanged |
| `vec([0.1])` / `{"v": ..}` | `[0.10000000149011612]` / same | unchanged (F32 widened to f64; D1) |

Effort/impact per the roadmap row: 2 / 4. The change is a few dozen lines of engine code plus tests; the value is in the contract and the migration note.

## 2. Verified current state (2026-08-28)

Every row was checked against the working tree today. Rows marked *(probe)* were additionally observed by running a throwaway binary against `cozo-core` (mem backend, `run_script` → `into_json`); the probe lives in the session scratchpad and left no footprint in the repo.

| Current fact | Where | Consequence |
|---|---|---|
| `DataValue` has exactly 13 variants: `Null, Bool, Num, Str, Bytes, Uuid, Regex, List, Set, Vec, Json, Validity, Bot`; `Regex`, `Set`, `Bot` are documented "used internally only" | [`cozo-core/src/data/value.rs:146-174`](../../cozo-core/src/data/value.rs) | Fixes the row set of D1; three rows are internal-only and need a form but no user-facing promise |
| `DataValue` and `NamedRows` derive `serde::Serialize` (the storage/wire encoding, externally-tagged enum form); cozo-bin's REPL prints `params` through it | [`value.rs:143-145`](../../cozo-core/src/data/value.rs), [`runtime/db.rs:263`](../../cozo-core/src/runtime/db.rs), [`cozo-bin/src/repl.rs:238`](../../cozo-bin/src/repl.rs) | A latent third JSON form exists in the type system; Invariant 2 declares it not a JSON boundary rather than pretending it is absent |
| `Set` is a `BTreeSet<DataValue>`, iterated in `Ord` order | [`value.rs:165`](../../cozo-core/src/data/value.rs) | JSON array order for sets is deterministic |
| Top-level policy: `impl From<DataValue> for JsonValue` — Uuid `json!(u.0)` (string), Bytes base64 `String`, Float NaN→`null` / ±∞→`"INFINITY"`/`"NEGATIVE_INFINITY"`, `Bot` → `panic!("found bottom")`, Vec `json!(a.as_slice().unwrap())`, Validity `json!([v.timestamp.0, v.is_assert])`, Json `j.0` pass-through, List/Set arrays with per-element `.clone()` | [`cozo-core/src/data/json.rs:55-101`](../../cozo-core/src/data/json.rs) (Float 61-75, Bytes 77, List 78-80, Bot 81, Set 82-84, Regex 85-87, Uuid 88-90, Vec 91-94, Validity 95-97, Json 98) | Survivor seam (D2); two panic paths (`Bot`, `as_slice().unwrap()`) to remove |
| `uuid` is built with the `serde` feature, so `json!(u.0)` is the hyphenated string; `serde_json` is pulled with no features (no `preserve_order`, so object keys serialize in `BTreeMap` order) | [`cozo-core/Cargo.toml:139, 163`](../../cozo-core/Cargo.toml) | D1 Uuid row; Invariant 3 is stated as value identity, not byte identity |
| Nested policy #1: `fn to_json(&DataValue)` — Uuid `json!(u.0.as_bytes())` (16-int array), Bytes `json!(b)` (int array), Float bare `json!(f)`, `Bot` → `null`, Validity `[vld.timestamp.0, vld.is_assert.0]`, Json `j.0.clone()`, List/Set/Vec element-wise | [`cozo-core/src/data/functions.rs:205-271`](../../cozo-core/src/data/functions.rs) (Float 217-219, Bytes 224-226, Uuid 227-229, Set 240-246, Vec 247-262, Json 263, Validity 264-266, Bot 267-269) | Root cause of #54; to be deleted (D2) |
| **Nested policy #2: the `ColType::Json` arm of `coerce`** — the `:put`/`:insert`/import write path for `Json` columns — hand-rolls the same nested forms (`Uuid => json!(u.0.as_bytes())`, `Bytes => json!(b)`, `Float => json!(f)`, Validity `[ts, is_assert]`, `Bot => null`); List/Set elements recurse through `self.coerce` | [`cozo-core/src/data/relation.rs:440-504`](../../cozo-core/src/data/relation.rs); external callers of `.coerce(`: `runtime/db.rs:1254,1313,1325,2663,2964` (the last two are the import/restore key-coercion paths), `query/stored.rs:2197,2200`, `runtime/columnar.rs:460`, `parse/sys.rs:593`, `runtime/stored_queries.rs:233`, `fixed_rule/utilities/csv.rs:62`, `data/program.rs:1542`; internal recursions at `relation.rs:259, 340, 470, 477` | The path that **persists** the old form; a third independent match body, to be deleted (D2). Reached by `:put`, `:create … default`, stored-query defaults, Parquet import, CSV typing |
| *(probe)* `:create r {k: Int => j: Json}` then `:put` of `to_uuid(..)`, `decode_base64("AQID")`, `1.0/0.0`, `vec([0.1])`, and `[uuid, bytes]` → stored `[166,187,…]`, `[1,2,3]`, `null`, `[0.10000000149011612]`, `[[166,…],[1,2,3]]` | probe, 2026-08-28 | Old forms are on disk in any `Json` column written this way; D5 items 4-5 and the migration note say so |
| `to_json` call sites: `op_json` (83), `op_set_json_path` (88, 93), `op_remove_json_path` (163), `op_json_object` (199), `val2str` (2091) | [`functions.rs:81-96, 161-188, 190-203, 2086-2094`](../../cozo-core/src/data/functions.rs) | Six call-site edits in one file, plus the `relation.rs` arm |
| JSON **keys** are derived by `val2str`: `json_object` keys (198), `{..}` literal keys (parse builds each key with `build_expr`, then `OP_JSON_OBJECT`), object path elements in `get_json_path` (105, 137) and `remove_json_path` (173) | [`functions.rs:105, 137, 173, 198`](../../cozo-core/src/data/functions.rs), [`cozo-core/src/parse/expr.rs:280-295`](../../cozo-core/src/parse/expr.rs) | A Uuid/Bytes key changes text with D2 — in the contract (D5 item 3) |
| `val2str` has three arms: `Str(s)` → raw `s`; `Json(JsonValue::String(s))` → raw `s`; every other variant → `to_json(v).to_string()` — the serialized JSON **literal**, so a `JsonValue::String` would come back with surrounding quotes | [`functions.rs:2086-2094`](../../cozo-core/src/data/functions.rs) | The `val2str` site is not a plain swap (D2 step 3); `to_string("a")` is `a`, not `"a"` (Invariant 1 carve-out) |
| *(probe)* `?[x,m,s] := x = to_uuid("a6bb…"), m = {"x": x}, s = to_string(x)` → `x = "a6bba931-…"`, `m = {"x":[166,187,169,…]}`, `s = "[166,187,…]"` (`length(s)` = 57); `json_object(x, 1)`, `{x: 1}` → `{"[166,…,132]":1}`; `remove_json_path({"a":1, x: 2}, [x])` → `{"a":1}` | probe | Issue #54 reproduced; keys confirmed depth-dependent |
| *(probe)* Bytes `decode_base64("AQID")`: top `"AQID"`, nested `{"b":[1,2,3]}`, `to_string` `"[1,2,3]"` | probe | Second depth-dependent variant, not named in the issue |
| *(probe)* Floats `1.0/0.0`: top `"INFINITY"`, nested `{"inf":null,"ninf":null,"nan":null}`, `to_string(inf)` `"null"`, `to_string(0.0/0.0)` `"null"`; `1.0` → `1.0`, `{"f": 1.0}` → `{"f":1.0}`, `to_string(1.0)` → `"1.0"`, `-0.0` → `-0.0` | probe; [`json.rs:61-75`](../../cozo-core/src/data/json.rs), [`functions.rs:217-219`](../../cozo-core/src/data/functions.rs) both `json!(f)` for finite values | Third depth-dependent variant; integral floats already keep `.0` at both depths (pinned by D6) |
| *(probe)* `vec([0.1])` → `[0.10000000149011612]` at both depths; `vec([0.1], "F64")` → `[0.1]`; `vec([1.0/0.0, -1.0/0.0, 0.0/0.0])` → `[null,null,null]` at both depths | probe; `json!(el)` on `f32`/`f64` → `serde_json::Value::from`, which widens `f32` to `f64` and yields `Null` for non-finite (`Number::from_f64` → `None`); `vec()` accepts any `get_float()` ([`functions.rs:2139-2147`](../../cozo-core/src/data/functions.rs)) | Two unnamed lossy edges in the draft; now D1 rows (Vec) |
| *(probe)* List/Json identical at both depths; a nested `9007199254740993` stays an exact integer; `json_to_scalar(parse_json('9007199254740993'))` exact | probe | Already agree; pinned by D6 |
| *(probe)* Validity `'ASSERT'` row: `[1787956506172712,true]` at top level, nested, via `to_string`, and via `get({"v": vld}, "v")`; but `:put` of that `get()` result into a `Validity` column **errors** (`when executing against relation 'v'`) | probe; [`relation.rs:411-437`](../../cozo-core/src/data/relation.rs) accepts only `List([Int, Bool])`, `v => bail!(InvalidValidity(v))`; `json2val` Array → `Json` ([`functions.rs:1869`](../../cozo-core/src/data/functions.rs)) | The two Validity expressions agree; the `get()`-extracted pair cannot be re-typed by coercion (D3 named gap) |
| `Regex` has no constructor op; `Set` has no constructor op | probe; [`value.rs:161,165`](../../cozo-core/src/data/value.rs) | Neither reachable from a script; pinned by Rust unit tests only |
| Inbound policy A, two impls with the same policy: `impl From<JsonValue> for DataValue` (17-34) and `impl From<&JsonValue> for DataValue` (36-53) — Number → `as_i64` else `as_f64` else `Str` of the digits; String→`Str`; Array→`List`; Object→`Json`; no UUID/base64 recognition | [`json.rs:17-53`](../../cozo-core/src/data/json.rs) | Canonical forms do not round-trip (D3); integers outside `i64` degrade to `Float` inbound |
| Inbound policy B: `json2val` (used by `json_to_scalar` and `get` on Json) — String→`Str`, **Array→`Json`**, Object→`Json` | [`functions.rs:321-325, 1848, 1855-1872`](../../cozo-core/src/data/functions.rs) | Two inbound policies disagree on arrays; out of scope, recorded (D3) |
| *(probe)* `is_uuid(json_to_scalar(parse_json('"a6bb…"')))` = `false`, `is_string(..)` = `true`; `is_json(json_to_scalar(parse_json('[1,2]')))` = `true`, `is_list(..)` = `false`; `is_uuid(to_uuid(get(parse_json('{"u":"…"}'), "u")))` = `true`; `to_string(json("abc"))` = `abc`, `to_string(json(1))` = `1` | probe | Inbound is lossy by construction; `to_uuid` is the explicit re-typing step; non-object `Json` payloads are indistinguishable from scalars at the boundary |
| `NamedRows::into_json` maps every cell through `JsonValue::from` (top-level policy); nested values inside a `Json` cell pass through verbatim | [`runtime/db.rs:315-330`](../../cozo-core/src/runtime/db.rs) | The only result-set boundary; fixing the nested policies fixes what `into_json` shows inside `Json` cells |
| `into_json` is called at three `lib.rs` sites — `run_script_fold_err` (613), `run_script_fold_err_with_options` (641, the fork's budgeted variant), `export_relations_str` (747, per relation); `run_script_str` (656) calls `run_script_fold_err`. Host surfaces on those: C (`cozo-lib-c/src/lib.rs:144, 214`), wasm (`cozo-lib-wasm/src/lib.rs:40, 42-44`), Java (`cozo-lib-java/src/lib.rs:91, 108`), Swift (`cozo-lib-swift/src/lib.rs:18-19`); HTTP server `res.into_json()` at `cozo-bin/src/server.rs:680` (also 811, 1082, 1152) | cited files | One outbound policy across all six surfaces and the relation-export path; the change is observed identically everywhere |
| Python binding converts cells natively (`Bytes`→`PyBytes` 387, `Uuid`→str 388, `Validity`→`[i64,bool]` 398, `Bot`→`None` 401, `Vec`→list 402-411) and hands `Json` cells to `json_to_py` verbatim (412); `json_to_py` never sees a `DataValue` | [`cozo-lib-python/src/lib.rs:378-414`](../../cozo-lib-python/src/lib.rs) (`json_to_py` at 350) | Top-level Python cells unaffected; nested values inside `Json` cells are fully determined by the engine — a Rust test is the guard |
| `cozo-lib-python/` has no `tests/` directory (only `integrations/*/tests`, separate packages); `build.yml` runs no maturin build or pytest; `python-publish.yml`'s gate is "the engine test suite" | `ls cozo-lib-python`; `find cozo-lib-python -name 'test*.py'`; [`.github/workflows/python-publish.yml:31-37`](../../.github/workflows/python-publish.yml) | No Python harness exists to add a binding test to (§7 cut) |
| Node binding: `Bytes`→`JsBuffer` (143), `Uuid`→string (147), `Bot`→`undefined` (173), `Json`→`json2js` (192) | [`cozo-lib-nodejs/src/lib.rs:143-192`](../../cozo-lib-nodejs/src/lib.rs) | Same shape as Python |
| CI: the `test` job runs `cargo test -p mnestic` on default (SQLite) features (lib unit tests use whatever `DbInstance` they build — `mem`); the `storage-rocksdb` job is `cargo build` plus two named `--test` targets; clippy is `cargo clippy -p mnestic --all-targets -- -D warnings` on default features | [`.github/workflows/build.yml:14-32, 120-141, 164-181`](../../.github/workflows/build.yml) | There is no per-backend matrix for lib tests; the D6 gate is stated in those terms (§5) |
| No `panic = "abort"` in any workspace profile | [`Cargo.toml:21-26`](../../Cargo.toml) (only `[profile.bench]` and a `mnestic-rocks` dev override) | A `Bot` panic in an axum handler kills the request, not the process (D7) |
| CozoScript `Display` is a separate literal form (`to_uuid("..")`, `decode_base64("..")`, `vec([..])`, Validity as a struct, `Bot` as `null`) | [`value.rs:632-672`](../../cozo-core/src/data/value.rs) | Not a JSON boundary; D1 does not touch it |
| The published docs already state the top-level forms as the contract: "`Bytes` render as a Base64 string, `Uuid` as its string form, `Vector` as a plain list of numbers, and `Validity` as a `[timestamp, is_assert]` pair. The stored value keeps its engine type — only the display coincides." | [`mnestic-site/src/app/docs/types/page.mdx:65-68`](../../../mnestic-site/src/app/docs/types/page.mdx) | D1 is that sentence extended to every depth; only the `functions` page examples for `json_object` (`:465`) and `set_json_path` (`:491`) need re-checking |
| Existing engine coverage: `bad_values` prints four lines (one plain `serde_json` literal, three `DataValue`→JSON conversions) and asserts nothing; `test_json_objects` only checks map literals parse | [`cozo-core/src/data/tests/json.rs:16-21`](../../cozo-core/src/data/tests/json.rs), [`cozo-core/src/runtime/tests.rs:611-619`](../../cozo-core/src/runtime/tests.rs) | D6 starts from zero pinned shapes |
| mindgraph-rs has its own converter `datavalue_to_json`: `Bytes`→`{"__bytes__": base64}` (14439), `Json` pass-through (14435), catch-all `_ => String(format!("{:?}", dv))` (14440); inverse `json_to_datavalue` recognises only the `__bytes__` tag (14463-14469); `named_rows_to_json` (14476-14493) is the sole caller, used only by `export_all` (7932-7975; 25 relations, `node_embedding` not among them); `import_snapshot` reads back via `json_to_named_rows` (7986) | [`mindgraph-rs/src/storage/cozo.rs:7932-7990, 14426-14493`](../../../mindgraph-rs/src/storage/cozo.rs) | A product-owned policy that never calls the engine seam; unaffected by D2 (D8) |
| No mindgraph-rs relation in `migrations.rs` declares a `Uuid`, `Bytes`, `Validity` or vector column (the only `Validity` hit is a comment at 308); props are `Json` (29). **Correction:** the product does declare one vector column outside `migrations.rs` — `node_embedding { uid => embedding: <F32; N> }` at `cozo.rs:8280` — but it is not among `export_all`'s 25 relations | [`mindgraph-rs/src/storage/migrations.rs:29, 308`](../../../mindgraph-rs/src/storage/migrations.rs), [`cozo.rs:8280`](../../../mindgraph-rs/src/storage/cozo.rs) | The `__bytes__` tag and the Debug catch-all are unreachable from exported relations today; the `node_embedding` vector column never reaches the export seam |
| `extract_json` accepts only `DataValue::Json` / `Null`; `row_to_node` reads props through it (13237); vector traffic is typed: the `node_embedding` column is declared `<F32; N>` (8280), reads unwrap `Vector::F32` (8490) and match `DataValue::Vec(v)` (8613, 9984) | [`cozo.rs:8280, 8490, 8613, 9984, 13237, 14208`](../../../mindgraph-rs/src/storage/cozo.rs) | `Json` pass-through is load-bearing for the product's hottest read path; Vec JSON form has no product consumer |
| mindgraph-rs runs no CozoScript that uses `json(`, `json_object(`, `set_json_path(`, `remove_json_path(` or `to_string(`, and no product `:put` writes a Uuid/Bytes/Float/Validity/Vec into a `Json` column (props arrive as `DataValue::Json` params) | grep of `src/storage/*.rs`, `mindgraph-server/src/*.rs`, 2026-08-28 | The nested path and the coercion path have zero product callers |
| Three product files read engine cells natively — `storage/cozo.rs`, `storage/budgeted_oracle.rs` (`NamedRows` at 34, 144, 165, 172, 567), `mindgraph-server/src/synthesis.rs` (`DataValue` at 19, 276, 376, 547-562); none calls `into_json`, `run_script_str`, or `JsonValue::from(DataValue)` (the `into_json` hits in `openai*.rs`/`biblio.rs` are ureq); HTTP `GET /export` returns a typed `TypedSnapshot` | greps of `mindgraph-rs/src`, `mindgraph-server/src`; [`mindgraph-server/src/lib.rs:3535`](../../../mindgraph-rs/mindgraph-server/src/lib.rs) | No product path crosses the JSON seam; the public export route is insulated |
| Existing mindgraph-rs snapshot tests assert counts only (`test_export_import_roundtrip`, `test_snapshot_serialization`); `graph.rs:6734-6737` compares two `export()` outputs by value (`assert_eq!` at 6737) | [`mindgraph-rs/tests/integration.rs:1894`](../../../mindgraph-rs/tests/integration.rs), [`mindgraph-rs/src/graph.rs:6734-6737`](../../../mindgraph-rs/src/graph.rs) | No per-variant shape is pinned on the product side (D8) |
| mindgraph-cloud depends only on `mindgraph` + `mindgraph-server` by path; no `DataValue`/`NamedRows`/`into_json` in `src/`; agent tooling emits `node.props.to_json()` (typed). TS SDK types props as `Record<string, unknown>`; dashboard renders props generically | [`mindgraph-cloud/Cargo.toml:7`](../../../mindgraph-cloud/Cargo.toml), [`mindgraph-cloud/src/agent_tools.rs:1330`](../../../mindgraph-cloud/src/agent_tools.rs), [`mindgraph-ts/src/types.ts:11`](../../../mindgraph-ts/src/types.ts), [`mindgraph-dashboard/src/app/dashboard/chat/[id]/page.tsx:2158`](../../../mindgraph-dashboard/src/app/dashboard/chat/[id]/page.tsx) | Cloud breaks iff mindgraph-rs breaks; it does not. No compile-time or parse-time consumer of the array form |
| Changelog precedent for a behaviour-is-the-fix change: `### Changed — …` with "The migration note first, because the break is the fix working"; precedent for naming the guarding fixture by test name | [`CHANGELOG-FORK.md:319, 1489-1491`](../../CHANGELOG-FORK.md) | The D5 note follows that shape |
| Published engine is `mnestic 0.16.0`; roadmap row 1 is `Release 0.17.0 \| Merged import, unreleased \| … \| Verify crates.io, PyPI, GitHub, and clean-install artifacts` | [`versions.toml:28`](../../../versions.toml), [`MNESTIC-ROADMAP.md:29`](../../../docs/strategy/MNESTIC-ROADMAP.md) | 0.17.0 is merged but not yet verified or cut; release placement is an owner ruling (status header) |

## 3. Decisions

### D1 — The canonical table (seam: `cozo-core/src/data/json.rs`, the single match body)

Scope: the **JSON boundary only** — `NamedRows::into_json`, the `Json`-producing builtins, `to_string()`, JSON keys derived from values, `Json`-column coercion, and every host surface that serialises through them. Native binding conversions (Python `value_to_py`, Node `value2js`) and CozoScript `Display` are **explicitly out of scope**; they keep their own top-level forms and inherit this table only for values nested inside a `Json` cell.

| Variant | Canonical JSON | Today: top / nested | Lossy boundary (named) |
|---|---|---|---|
| `Null` | `null` | same / same | — |
| `Bool(b)` | `true` / `false` | same / same | — |
| `Num(Int(i))` | JSON integer, exact `i64` | same / same | exact outbound; inbound integers outside `i64` degrade to `Float` (`json.rs:22-26`); consumers that parse to IEEE double lose precision above 2^53 (RFC 8259 §6 interoperable range) — a consumer-side hazard this spec documents but does not paper over with string-encoded ints |
| `Num(Float(f))`, finite | JSON number in `serde_json` f64 formatting — integral values keep a fractional marker (`1.0` → `1.0`, `-0.0` → `-0.0`), so `Int(1)` and `Float(1.0)` are textually distinct | same / same | — |
| `Num(Float(NaN))` | `null` | `null` / `null` | **lossy, and the only non-recoverable row**: NaN is indistinguishable from `Null` after conversion. Kept because it is today's top-level form; prior art would treat the non-finite family uniformly (owner ruling 2, §7) |
| `Num(Float(+∞))` | `"INFINITY"` | `"INFINITY"` / `null` | **lossy**: a `Str("INFINITY")` produces the same JSON; nested form changes. Spelling is Cozo's inherited top-level form (Postgres uses `"Infinity"`); kept for compatibility |
| `Num(Float(−∞))` | `"NEGATIVE_INFINITY"` | same / `null` | as above |
| `Str(s)` | JSON string | same / same | — |
| `Bytes(b)` | base64 (`STANDARD`, padded) JSON string | base64 / int array | **lossy**: indistinguishable from a `Str` holding the same text; nested form changes. Prior art is unanimous that bytes are one string (Postgres `\x` hex, Arrow hex, Neo4j base64) but splits hex/base64 — base64 is a **compatibility choice** (today's top-level form, already on the types page), not a prior-art norm |
| `Uuid(u)` | hyphenated lowercase string (`uuid::Uuid` `Serialize`) | string / 16-int array | **lossy**: indistinguishable from `Str`; `to_uuid()` re-types; nested form changes (the #54 headline). Confirmed by Postgres `to_json` (text output of the type) and DuckDB (lowercase on the JSON path) |
| `Regex(r)` | pattern string | same / same | internal-only; lossy (becomes `Str`) |
| `List(l)` | JSON array, elements by this table, recursively | same / same | — |
| `Set(s)` | JSON array in `BTreeSet` (`DataValue::Ord`) order, elements by this table | same / same | internal-only; **lossy**: becomes a `List` on the way back (RFC 8785: deterministic order is the canonical-form requirement; JSON has no set) |
| `Vec(F32(a))` / `Vec(F64(a))` | flat JSON array, iterated element-wise; each element is `Number::from_f64(el as f64)`, or `null` when that is `None` | same / same | **lossy, three ways**: (1) F32 vs F64 element type is not recorded (matches DuckDB/Arrow, where width lives in schema metadata); (2) F32 elements are widened to f64 before formatting — `vec([0.1])` → `[0.10000000149011612]`, `vec([0.1], "F64")` → `[0.1]`; (3) **non-finite elements → `null`**, sign and NaN/∞ distinction lost — differs from the scalar `Float` row **by design** so that `vec()` can still re-type the array via `as_f64()`; becomes a `List` on the way back |
| `Json(j)` | `j` **unchanged** — no re-wrapping, no re-encoding | same / same | load-bearing for mindgraph-rs `extract_json`; **lossy when the payload is not an object**: scalar/array payloads parse back as `Str`/`Num`/`Bool`/`Null`/`List` (policy A) or `Json` (policy B) |
| `Validity{ts, is_assert}` | `[ts_i64, is_assert_bool]` (two-element array) | same / same | **lossy**: becomes a `List` (policy A) or `Json` array (policy B) on the way back; only the `List` form re-enters a `Validity` column (D3). An in-house positional form with no prior-art precedent (Neo4j/Postgres emit ISO-8601 text); kept because it is Cozo's inherited top-level contract and already documented on the types page |
| `Bot` | `null` | **panic** / `null` | internal-only; see D7 |

Rules that apply to every row: nesting is recursive with no depth cap (a `List` of `Json` of `List` renders each layer by this table, and `Json` payloads are never re-walked); the table is a function of the variant alone, never of position; no variant is silently truncated (base64 is exact; `i64` is exact).

### D2 — The single conversion seam (survivor: `json.rs`; deleted: `functions.rs::to_json` and the `relation.rs` `ColType::Json` arm)

1. `cozo-core/src/data/json.rs` gains `impl From<&DataValue> for JsonValue` — a borrowing, recursive conversion whose match body **is** D1. This is the one and only hand-written policy (Postgres has exactly one `datum_to_json_internal` behind `to_json`, `row_to_json`, `json_build_object`, `json_agg`; the recursion re-entering the same function is precisely what #54 lost).
2. The existing owning `impl From<DataValue> for JsonValue` (`json.rs:55-101`) is reduced to a delegate with **one** arm of its own: it moves the `Json` payload (avoids deep-cloning `Json` result cells on the HTTP/string-API path) and delegates every other variant to the borrowing impl.
3. `fn to_json` at `functions.rs:205-271` is **deleted**. Five of its six call sites (`functions.rs:83, 88, 93, 163, 199`) become `JsonValue::from(&x)`. The sixth, `val2str` (`2086-2094`), is **not** a plain swap: `serde_json::Value::to_string()` on a `JsonValue::String` emits the quoted literal, so `val2str` unwraps a string payload before stringifying — a match on `JsonValue`, not on `DataValue`, so Invariant 2 holds. This generalises the existing `Json(JsonValue::String)` arm.
4. The `ColType::Json` arm of `coerce` (`relation.rs:440-504`) is **replaced** by one line delegating to the owning impl. Its List/Set arms already terminated in `From<DataValue>` via `arr.into()`, so nothing is lost; its scalar arms were the old nested forms.
5. `NamedRows::into_json` (`db.rs:315-330`) is unchanged. So are `run_script_str`, `export_relations_str`, `cozo-bin`, and every binding.

The resulting seam, in full:

```rust
// cozo-core/src/data/json.rs
impl From<&DataValue> for JsonValue {            // the policy: one match body, D1 row per arm
    fn from(v: &DataValue) -> Self {
        /* 13 arms; recursive through List/Set; Vec elements via
           Number::from_f64(el as f64).map_or(JsonValue::Null, JsonValue::Number) */
    }
}
impl From<DataValue> for JsonValue {             // delegate: moves the Json payload, nothing else
    fn from(v: DataValue) -> Self {
        match v {
            DataValue::Json(j) => j.0,
            other => JsonValue::from(&other),
        }
    }
}

// cozo-core/src/data/functions.rs — the only call site that is not a plain swap
fn val2str(arg: &DataValue) -> String {
    match JsonValue::from(arg) {
        JsonValue::String(s) => s,               // bare text: Str, Uuid, Bytes, ±∞, Regex, Json string payloads
        other => other.to_string(),              // compact JSON text for everything else (incl. NaN → "null")
    }
}

// cozo-core/src/data/relation.rs — ColType::Json coercion
ColType::Json => DataValue::Json(JsonData(JsonValue::from(data))),
```

Bounds that are explicit rather than implied: recursion depth is bounded only by the value's own depth (a `List` nests as deep as the script built it; `Json` payloads are not walked, so a stored JSON blob adds zero recursion); there is no size cap and no truncation at any depth — a 4096-dimension `Vec` emits 4096 numbers; F32 widening is deliberate (`el as f64`), not incidental.

Why `json.rs` survives and not `functions.rs`: `into_json` is the public result boundary for six host surfaces, its forms are what every published consumer already sees at top level, and mindgraph-rs has zero callers of the nested builtins. Unifying on the nested policy would change the top-level contract for everyone to fix a bug nobody outside `Json` cells can see.

### D3 — Round trip: the contract is one-way (seam: `json.rs:17-53` (both inbound impls), `functions.rs:1855-1872`, unchanged)

JSON→DataValue does **not** accept the canonical forms as typed values and this spec does not make it do so. A canonical UUID string parses back as `Str`; base64 parses back as `Str`; Validity, Vec and Set arrays parse back as `List` (via either `From<JsonValue>` impl) or `Json` (via `json_to_scalar` / `get`). The explicit re-typing steps remain `to_uuid()`, `decode_base64()`, `vec()`, and Validity column coercion on `:put` of a **`List`** — a Validity pair extracted from a `Json` cell with `get()` is a `Json` array and is rejected by coercion (probe, §2); it re-enters only via params or `parse_json` → `From<JsonValue>` (policy A). Named gap, not fixed here. The two inbound policies' disagreement on arrays (`List` vs `Json`) is recorded and left alone — it predates #54, has no depth-dependence, and changing it would alter `get()` chaining semantics.

Prior art confirms the shape: Postgres maps JSON string→text, number→numeric and nothing else (a UUID that went through `to_json` comes back as text; re-typing is an explicit cast); Neo4j states the same loss for plain JSON and solves it with a separate opt-in typed format rather than sniffing. No production system infers uuid/bytes from string shape.

### D4 — Blast radius across the boundary (verified negatives and the positives)

| Consumer | Path | Observes the change? | Breaks? |
|---|---|---|---|
| HTTP (`cozo-bin`), C, wasm, Java, Swift, embedded `run_script*` / `export_relations_str` | `into_json` | Only inside `Json` cells, `to_string()` results, and derived keys; top-level cells identical except `Bot` (panic→`null`) | No — nested arrays become strings; no consumer in this workspace parses the array form |
| Python binding (`mnestic` on PyPI) | native cells; `json_to_py` for `Json` | Nested only (same as above) | No |
| Node binding | native cells; `json2js` for `Json` | Nested only | No |
| CozoScript `to_string(uuid \| bytes \| ±inf)` | `val2str` → seam | **Yes, directly**: `"[166,…]"` → `"a6bb…"`, `"[1,2,3]"` → `"AQID"`, `"null"` → `"INFINITY"` | Behavioural change, D5 item 3 |
| CozoScript JSON keys from a Uuid/Bytes value (`{x: 1}`, `json_object(x, 1)`, `remove_json_path(j, [x])`) | `val2str` → seam | **Yes, directly**: key text follows `to_string()` | Behavioural change, D5 item 3 |
| CozoScript `:put` / `:insert` / import of a non-`Json` value into a `Json` column | `coerce` → seam | **Yes, directly; this is the write path that persisted the old form** | New rows store the canonical form; old rows are untouched — D5 items 4-5 |
| mindgraph-rs `export_all` / `import_snapshot` | own `datavalue_to_json` | **No** — never calls the engine seam | No; format unchanged (D8) |
| mindgraph-rs `row_to_node` / `extract_json`, HNSW / embeddings, stored data (`String` uids; no Uuid/Bytes/Validity columns), CozoScript (no JSON builtins, no `to_string`, no non-`Json` writes into `Json` columns) | typed cells / `Json` pass-through | No | No |
| `GET /export` (server), mindgraph-cloud, TS/Python SDKs, dashboard | typed structs / `Record<string, unknown>` | No | No |

Prior art has no analogue for this table; it warns only that nested-form changes surface as downstream driver bug reports (node-postgres #1356, DuckDB #14646) — residual risk 1 is the correct framing. Named silent-failure mode retained from grounding: `export_all_embeddings` (`cozo.rs:9984`) has `_ => continue`, so if a future change ever made vectors arrive as a non-`Vec`/`List` variant, embeddings would be dropped silently rather than erroring. Not touched here; recorded so the next reader knows the arm exists.

### D5 — Compatibility contract and migration note (deliverable named by the roadmap gate)

**Contract, stated for consumers:**

1. Top-level result cells are value-identical before and after, for every variant a script can produce. (`Bot` cannot be produced by a script.)
2. Values nested inside a `Json` cell — from `{..}` literals, `json()`, `json_object()`, `set_json_path()`, `remove_json_path()` — now use the top-level forms. Three variants change shape: `Uuid` (16-int array → string), `Bytes` (int array → base64 string), non-finite floats (`null` → `"INFINITY"` / `"NEGATIVE_INFINITY"`). NaN stays `null` and is the single value that cannot be told from `Null` after conversion (D1). Non-finite `Vec` elements stay `null` (D1).
3. `to_string()` of those three variants changes accordingly and returns the bare text (no surrounding quotes). Keys derived from non-string values — `json_object` keys, `{..}` literal keys, and object path elements in `get`/`set_json_path`/`remove_json_path` — follow `to_string()` and change identically, so `remove_json_path(j, [x])` with a Uuid `x` now addresses the string-form key.
4. `DataValue::Json` payloads are never re-encoded on read: mindgraph-rs props and every JSON value the user wrote as JSON are unchanged.
5. **No relation, index or snapshot is rewritten.** However, `Json` column values that were *built* from Uuid/Bytes/±∞/Vec before this release — by a map literal, `json_object`, `set_json_path`, or by `:put`/`:insert`/import coercion of a non-`Json` value into a `Json` column — keep their old nested forms on disk (16-int arrays, int arrays, `null`); rows written after it use the canonical forms. A column written across the upgrade holds both, and the engine cannot tell an old-form UUID array from a genuine 16-element list. Position independence is guaranteed per conversion, not across the history of a stored column. No CozoScript expression converts the array form (`to_uuid` accepts strings only); the rewrite is client-side.
6. mindgraph-rs snapshot files (`GraphSnapshot.relations`) keep their format; `mindgraph_version` stays the snapshot-compat key.
7. JSON→DataValue stays lossy (D3). Nothing that parsed as `Str` before parses differently after.

Prior art confirms the shape of the contract: serialization-shape fixes ship as behaviour changes with a note, not flags (DuckDB #17329 is a plain bug; Postgres documents json/jsonb representational differences in the reference; Neo4j's `Accept`-header versioning applies to an additive typed format, not to fixing a defect).

**Release placement (owner ruling 1):** the change is a `### Changed` entry, not a patch. Proposed: 0.18.0, so that 0.17.0 (merged, unreleased, its own verify gate still pending — `MNESTIC-ROADMAP.md:29`) ships with only the Parquet import it was scoped to. Alternative: bank into 0.17.0 if the owner prefers one minor with two `### Changed` entries. Either way it is banked, not cut alone ("bank releases, don't cut them").

**Migration note (draft, `CHANGELOG-FORK.md`, following the `:319` precedent):**

> ### Changed — nested values now serialize exactly like top-level values (#54)
>
> **The migration note first, because the break is the fix working:** a UUID, byte string, or infinite float placed inside a JSON object or list — `{"id": rand_uuid_v4()}`, `json_object("blob", decode_base64(..))`, `set_json_path(j, ["x"], 1.0/0.0)` — or written by `:put` into a `Json` column, previously rendered as a 16-element integer array, an integer array, and `null` respectively, while the same value at the top level of a result row rendered as a UUID string, a base64 string, and `"INFINITY"`. Nested values now use the top-level forms at every depth; `to_string()` of those values returns the same bare text; and a JSON key derived from a UUID or byte value (`{x: 1}`, `json_object(x, 1)`, a path element in `remove_json_path`) is now that text too. If you parsed the integer-array form of a nested UUID or key, switch to the string (`to_uuid()` in CozoScript, `uuid.UUID(s)` in Python).
>
> **Stored `Json` columns are not rewritten.** Values built from a UUID, bytes or ±∞ *before* this release keep the array/`null` form on disk; rows written *after* it carry strings, so one column can hold both, and no CozoScript expression converts the old form. To find old rows: `?[k] := *rel{k, j}, is_list(get(j, "u"))` (adjust the key). To rewrite: read them, convert client-side (`uuid.UUID(bytes=bytes(arr))` / `base64.b64encode(bytes(arr))` in Python), and `:put` the row back. Top-level cells are unchanged; NaN is still `null`; non-finite vector elements are still `null`. The internal `Bot` value now serializes as `null` instead of panicking. One policy now lives in `data/json.rs`; the copies in `data/functions.rs` and the `Json` column coercion in `data/relation.rs` are gone. Regression-guarded by `data/tests/json.rs::canonical_*` (one test per variant, each asserting top-level == map-literal == list-nested == `json_object` == `set_json_path` == `to_string` == `:put` into a `Json` column).

mindgraph-rs is unaffected by item 5: no product script builds `Json` from those variants and props arrive as `DataValue::Json` params (§2).

### D6 — Tests (seam: `cozo-core/src/data/tests/json.rs`, replacing `bad_values`)

All engine tests are **lib unit tests** (`cargo test -p mnestic --lib`) on the `mem` backend — the seam is storage-independent, and `mem` supports `:create`/`:put` of `Validity` and `Json` columns, so no stored backend is needed. Each asserts a literal expected value (no `println!`, no snapshot files). Arrow's per-type golden files and RFC 8785's per-number-class test vectors are the prior-art shape.

| Test | Pins |
|---|---|
| `canonical_uuid` | the #54 script verbatim: `x` string, `m.x` same string; `to_string(x)` same string with `length(to_string(x)) == 36` and `to_string(x) == to_string(json(x))`; `json_object("u", x)`, `set_json_path({}, ["u"], x)`, `json(x)`, `[x]` and `{"l": [x]}` all yield the string; key form: `json_object(x, 1)` and `{x: 1}` both have the string as their only key, and `remove_json_path({x: 2, "a": 1}, [x])` → `{"a":1}` |
| `canonical_bytes` | `decode_base64("AQID")` → `"AQID"` at every depth; `to_string` → `AQID` (4 chars) |
| `canonical_floats` | `1.0` → `1.0` and `to_string(1.0)` → `"1.0"`; `-0.0` → `-0.0`; NaN → `null` at every depth **and** `{"n": 0.0/0.0} == {"n": null}` (the aliasing is pinned, not incidental); `1.0/0.0` → `"INFINITY"`, `-1.0/0.0` → `"NEGATIVE_INFINITY"` at top level, nested, and via `to_string` (bare, 8 / 17 chars) |
| `canonical_int_exact` | `9007199254740993` nested stays an exact integer at every depth (documents the >2^53 consumer-side hazard both ways: exact here, IEEE-lossy in a double-based consumer) |
| `canonical_vec` | `vec([0.1])` → `[0.10000000149011612]` and `vec([0.1], "F64")` → `[0.1]` at every depth (F32 widening is contract); `vec([1.0/0.0, 0.0/0.0])` → `[null,null]` at every depth; a non-contiguous `Array1` view (Rust-level) converts without panic |
| `canonical_validity` | a `Validity` column row → `[ts, true]` at top level, nested, and via `to_string`; the retracted case `[ts, false]` |
| `canonical_put_json_column` | `:put` of Uuid, Bytes, `1.0/0.0`, `vec([0.1])`, a Validity value and `[uuid, bytes]` into a `Json` column; reading each back equals the corresponding map-literal form |
| `canonical_list_json_passthrough` | `[1,"a",null]` and `json({"k":[1,2]})` identical at every depth; a `Json` payload containing an int array that *looks like* a UUID is **not** rewritten; `to_string("a")` is `a` and `to_string(json("a"))` is `a` (the unquoted carve-out, Invariant 1) |
| `canonical_internal_variants` (Rust unit) | for one instance of each of the 13 variants, `JsonValue::from(v.clone()) == JsonValue::from(&v)`; `Set` of three unordered elements → ascending array, stable across two conversions; `Regex` → pattern string; `Bot` → `null` through both impls, no panic |
| `bad_values` | deleted (subsumed by `canonical_floats`) |

Product side (Batch B, no behaviour change): `mindgraph-rs/tests/integration.rs::snapshot_cell_shapes_are_pinned` — after `export()`, every cell of every exported relation is `null`/bool/number/string/array/object as produced by the enumerated arms, and no cell is a Rust `Debug` string (asserting the catch-all at `cozo.rs:14440` is unreachable). It runs in ordinary mindgraph-rs CI; `datavalue_to_json` never calls the engine seam, so its result is independent of Batch A by construction.

### D7 — `Bot` serializes as `null` (seam: `json.rs:81`)

`Bot` is internal-only, unconstructible from a script, already `null` in `Display` (`value.rs:652`), `None` in Python, `undefined` in Node, and `null` on both nested paths. A `panic!` inside `From<DataValue>` turns an engine invariant violation into an unwinding panic that kills the request on the HTTP path (`cozo-bin/src/server.rs:680`, inside a tokio task; no workspace profile sets `panic = "abort"`) instead of returning a value. Canonical: `null`. Prior art neither confirms nor contradicts (no external system has a bottom value; serde_json/ECMAScript degrade unrepresentable values to `null`, RFC 8785 errors). Rejected: panic everywhere (would make `json_object` on a `Bot` panic where it returned `null`).

### D8 — mindgraph-rs keeps its own converter; it is pinned, not replaced (seam: `mindgraph-rs/src/storage/cozo.rs:14426-14493`)

The engine will have one policy; the product's snapshot writer/reader is a *second*, product-owned encoding, and this spec says so rather than pretending otherwise. It stays because: (a) it is a matched writer/reader pair with a self-describing `__bytes__` tag, which the engine form deliberately is not (D3) — the same layering production systems use (Neo4j Typed JSON `{"$type","_value"}`, Arrow extension metadata: a tagged round-trippable form sits *above* the plain lossy form, never merged into it); (b) every exported relation is `String`/`Int`/`Float`/`Bool`/`Json` typed, so its `Uuid`/`Vec`/`Validity` Debug-string catch-all is unreachable today; (c) replacing it would change a persisted format for no consumer need. Batch B pins (b) with one test so that the day a relation with one of those column types is added to `export_all`, the test — not a customer — finds the Debug string.

## 4. Invariants

1. **Position independence.** For every `DataValue` `v` and every JSON-producing context `C` (`into_json` cell, `{..}` member, list element, `json()`, `json_object()`, `set_json_path()`, `remove_json_path()`, `Json`-column coercion), `C(v)` is D1(`v`). `to_string(v)` renders D1(`v`) as compact JSON text, except that a `JsonValue::String` result (`Str`, `Uuid`, `Bytes`, `±∞`, `Regex`, `Json` string payloads) is returned unquoted. Keys derived from `v` are `to_string(v)`.
2. **One body.** Exactly one hand-written DataValue→`JsonValue` policy exists in `cozo-core` (the borrowing impl); the owning impl's single `Json` move is the only other arm, and `val2str` matches on `JsonValue`, never on `DataValue`. The `serde::Serialize` derive on `DataValue`/`NamedRows` is the storage/wire encoding (and cozo-bin's `params` REPL display), not a JSON boundary: no host surface may serialize `NamedRows` through serde — `into_json` is the only route.
3. **`Json` is opaque.** A `DataValue::Json` payload is emitted value-identically (`JsonValue` equality) and never re-walked. Byte layout (key order, whitespace) belongs to the host's serializer, not the engine.
4. **No panic on conversion.** No variant, contiguous or not, panics in the seam.
5. **Determinism.** Equal `DataValue`s produce equal JSON across runs (`Set` ordering is `Ord`).
6. **Exactness bounds.** `i64` is exact; base64 is exact; every other lossy edge is one of the rows named in D1 (including F32 widening and non-finite `Vec` elements) — there is no unnamed lossy edge.
7. **One-way.** No inbound path (`json.rs:17-53`, both impls; `json2val`) grows type sniffing as part of this work.

## 5. Acceptance tests

- All D6 engine tests pass as lib unit tests on `mem` under the existing `test` job (`cargo test -p mnestic`, default features).
- The #54 script (`?[x, m] := x = rand_uuid_v4(), m = {"x": x}`) returns `m.x == x` through `run_script_str` (the string API), which covers C/wasm/Java/Swift — and, because `json_to_py`/`json2js` receive the already-converted `Json` payload, Python and Node — by construction.
- `cargo clippy -p mnestic --all-targets -- -D warnings` (default features, as `build.yml` scopes it). `cargo fmt --check` is **not** a CI gate (`build.yml` deliberately omits it: the inherited tree is not rustfmt-clean); run `cargo fmt` only on the files this work touches, never on the whole package.
- Batch B (`mindgraph-rs`): `snapshot_cell_shapes_are_pinned` passes in mindgraph-rs CI.
- `mnestic-docs` `doc_check` passes on `docs/functions` and `docs/types` against the Batch A engine (examples that print nested UUID/bytes forms regenerate).
- The migration note in D5 is present in `CHANGELOG-FORK.md` "Unreleased" in the same PR as Batch A, and links this spec.

## 6. Layering ruling

**mnestic only.** DataValue→JSON serialization is engine mechanism: it passes the stranger test (any CozoDB user hits #269), carries no cognitive or tenancy vocabulary, and is stable ([`docs/strategy/LAYERING.md:18-25, 31`](../../../docs/strategy/LAYERING.md)). Nothing new is added to the engine's surface — the work deletes two match bodies and adds tests; the only "new" public behaviour is `Bot → null` replacing a panic.

mindgraph-rs: **nothing new** — one pinning test, no behaviour change, no dependency on the engine's conversion. mindgraph-cloud: nothing.

## 7. Rejected alternatives

| Alternative | Why not |
|---|---|
| Patch `to_json`'s `Uuid` arm in place (the minimal fix for #269) | Leaves three policies; `Bytes`, non-finite floats and the `:put` coercion path stay depth-dependent; the next divergence is a matter of time |
| Unify on the nested (`functions.rs`) policy | Changes top-level output for every host surface (UUIDs and bytes become int arrays); breaks the one contract consumers already rely on |
| Self-describing tagged forms in the engine (`{"$uuid": "…"}`, `{"__bytes__": "…"}`) to make the round trip typed | Changes the top-level contract for everyone; type-tagging is a product-layer choice (mindgraph-rs already made it for its own snapshots; Neo4j ships it as a separate opt-in format); the engine's `Json` column would then contain tagged objects a user did not write |
| Type-sniff on the way back (UUID-shaped strings → `Uuid`) | Silently changes the type of ordinary strings; `to_uuid()` already exists as the explicit step; no production system does it (D3) |
| Fold `json2val` into `From<JsonValue>` | Changes `get()` on JSON arrays from `Json` to `List`, altering chaining semantics; unrelated to depth-dependence |
| Accept a `Json` array as a `Validity` on `:put` (close the D3 gap) | New coercion behaviour outside #54's scope; the `List` route exists; named, not fixed |
| Replace mindgraph-rs `datavalue_to_json` with an engine-exported canonical function | Changes a persisted snapshot format for no consumer; loses the `__bytes__` round trip; pulls product I/O policy down the stack |
| Make `Bot` panic at every depth (unify on `json.rs:81`) | Introduces a new panic in `json_object`/`set_json_path`/coercion where `null` was returned |
| Emit `null` for ±∞ at every depth (unify on the nested float policy) | Changes top-level output; loses the sign/infinite distinction that top-level consumers get today |
| NaN → `"NAN"` (uniform non-finite family, as Postgres does) | Would be a second deliberate top-level change beside `Bot`, breaking D5 item 1 for a value already `null` everywhere today; the aliasing is instead stated in D1/D5 and pinned by `canonical_floats`. **Owner ruling 2** if the uniform family is preferred |
| Apply the scalar `Float` row per `Vec` element (`"INFINITY"` strings inside a numeric array) | Breaks `vec()` re-typing of the emitted array (`as_f64()` on a string fails); `null` per element is kept and named lossy in D1 |
| Record the F32/F64 element type in the Vec JSON form | Changes a top-level form that already agrees at both depths, for no consumer; named as lossy instead (DuckDB/Arrow keep width in schema metadata, not in the array) |
| A data migration rewriting old-form values in `Json` columns | The engine cannot distinguish an old-form UUID array from a genuine 16-int list; any rewrite would be a guess. Detection query + client-side recipe in the migration note instead |
| A compatibility flag keeping the old nested forms | Keeps two policies alive, which is the defect; prior art ships shape fixes as plain behaviour changes (D5) |
| A grep-lint against `serde_json::to_*(&NamedRows)` in CI | The existing antipattern lint job lives in mindgraph-rs, not mnestic, and no such caller exists; Invariant 2 states the rule, a reviewer enforces it |
| Touch `Display` / CozoScript literal form | Different boundary; #54 is about JSON |

### Panel triage (Pass 3, 26 findings — all folded)

| Finding (severity) | Disposition |
|---|---|
| `val2str` returns the quoted JSON literal for a `JsonValue::String`, so every stated `to_string` value was wrong (blocker, reported twice) | Folded: D2 step 3 + code block (`val2str` matches on `JsonValue`), §1 row, §2 three-arm row, `canonical_uuid`/`canonical_bytes`/`canonical_floats` pin bare text and lengths |
| Third match body in `relation.rs` `ColType::Json` coercion — the write path persisting the old form (blocker) | Folded: §2 row + probe, D2 step 4, D4 row, D5 item 5, `canonical_put_json_column`, Invariant 2 |
| CI cannot deliver "green on all three backends"; clippy is default-features (major, reported twice) | Folded, option (a): lib unit tests on `mem`; §5 and Batch A gate reworded; clippy bullet quotes `build.yml` |
| No Python test harness exists for the binding (major, reported twice) | Folded: Python bullet deleted; covered by construction (`json_to_py` receives the converted payload); manual binding check stays in Batch C's PyPI verify step |
| JSON keys from Uuid/Bytes change too (major) | Folded: §2 key row + probe, D4 row, D5 item 3, migration note, `canonical_uuid` key assertions, Invariant 1 |
| "Stored data does not change" hides the mixed-encoding `Json` column (major, reported twice) | Folded: D5 items 4-5 rewritten, migration note gains detection + client-side rewrite recipe, residual risk 2 generalised, migration alternative rejected above |
| Non-finite `Vec` elements → `null` and F32 widening are unnamed lossy edges (major) | Folded: D1 Vec row (three named edges), D2 element conversion and bounds, `canonical_vec` pins `[null,null]` and `0.10000000149011612`, per-element `"INFINITY"` rejected above |
| Batch B two-engine gate proves nothing; `__bytes__` round-trip pins unreachable code (major) | Folded: one ordinary test, gate "passes in mindgraph-rs CI"; `__bytes__` round-trip test dropped |
| `to_string` is not D1 for `Str` and `Json` string payloads (minor) | Folded: Invariant 1 carve-out; §2 row; `canonical_list_json_passthrough` asserts the carve-out |
| "Only in `cozo.rs`" is false (`budgeted_oracle.rs`, `synthesis.rs`) (minor) | Folded: §2 row names three files; conclusion unchanged |
| Two inbound impls at `json.rs:17-53` (minor) | Folded: §2 row, D3 seam, Invariant 7 |
| `into_json` arrow backwards; three `lib.rs` sites + export paths; serde `Serialize` is a latent third form (minor, two overlapping findings) | Folded: §2 rows, D4 row, Invariant 2 scoped; grep-lint declined (rejected above) |
| `canonical_vec` cannot detect F32 widening with dyadic inputs (minor) | Folded into the Vec fold: `vec([0.1])` pinned |
| D7 "process abort" overstated (minor) | Folded: reworded; `panic = "abort"` absence verified (§2 row) |
| Validity coercion rejects the `get()`-extracted `Json` array (minor) | Folded: D3 named gap, D1 Validity row, probe row in §2, alternative rejected above |
| Non-object `Json` payloads and >`i64` inbound integers are lossy (minor) | Folded: D1 `Json` and `Int` rows |
| Integral floats keep `.0` — unpinned (minor) | Folded: D1 Float row, `canonical_floats` |
| Owning delegate's `Str` move cites a non-existent consumer (minor) | Folded: delegate reduced to the single `Json` move; rationale reworded |
| "§5" points at the wrong gate artifact (minor) | Folded: §1 bullet and Batch A gate say D5 |
| Release-placement premise false (0.17.0 not yet verified) (minor) | Folded: rewritten as owner ruling 1 with the roadmap row quoted |
| Invariant 3 "byte-identically" overstates (minor) | Folded: value identity; `preserve_order` absence recorded in §2 |

Prior-art folds (Pass 4): CONTRADICTS on non-finite floats → D1 NaN/±∞ rows, D5 item 2, `canonical_floats` aliasing pin, owner ruling 2; GAP on Validity → D1 row names it an in-house form; GAP on D4 → prior-art warning cited, residual risk 1 retained; GAP on D7 → rationale cites the two divergent precedents; essential omission (uniform non-finite family) → same as the CONTRADICTS fold; CONFIRMS verdicts cited at D1 (Uuid, Bytes, Vec, Set), D2, D3, D5, D6, D8; the D6 suggestion to document the >2^53 hazard both ways → `canonical_int_exact`.

### Simplification pass (Pass 5) — requirement-neutral cuts applied

1. Owning delegate reduced from two moves (`Json`, `Str`) to one (`Json`) — one residual arm, one test.
2. Python binding test and its implied maturin/pytest CI step dropped — covered by construction.
3. Three-backend engine test gate dropped — lib unit tests on `mem` under the existing job.
4. Batch B two-engine gate and the `__bytes__` round-trip unit test dropped — one ordinary test in mindgraph-rs CI.
5. Four Rust-level unit tests (`canonical_set_sorted`, `canonical_regex`, `canonical_bot`, `owning_and_borrowing_agree`) collapsed into `canonical_internal_variants`.
6. Grep-lint for serde serialization of `NamedRows` not added — Invariant 2 states the rule.

Owner-ruling cuts (not applied) are the two in the status header.

## 8. Batches = review units

| Batch | Repo | Content | Gate |
|---|---|---|---|
| **A** | mnestic | D2 seam (`json.rs` borrowing impl + owning delegate; delete `functions.rs::to_json`; five plain call-site swaps + the `val2str` unwrap; replace the `relation.rs` `ColType::Json` arm), D7, D6 engine tests, `CHANGELOG-FORK.md` Unreleased entry with the D5 note, `functions`/`types` doc pages through `doc_check` | D5 owner-approved first ("Compatibility contract before output changes"); existing CI green (`test` + `clippy` jobs, default features) |
| **B** | mindgraph-rs | `snapshot_cell_shapes_are_pinned` (D8). No behaviour change. Can land before A. | Passes in mindgraph-rs CI |
| **C** | mnestic release | Ship A in the release the owner rules (0.18.0 proposed); `versions.toml` + `bump.py`; roadmap row 5 → shipped; the existing PyPI verify step additionally runs the #54 script through the wheel and checks the nested value is a `str` | `/release` procedure; not cut alone |

## 9. Residual risks (stated honestly)

1. **Unknown external consumers of the nested array form.** The PyPI `mnestic` package and the HTTP server are public; someone may parse the 16-int UUID form. The migration note is the mitigation; there is no compatibility flag (§7).
2. **Mixed encodings in stored data.** Every `Json` column written before the upgrade from Uuid/Bytes/±∞ via any write-time JSON builtin or `:put` coercion keeps the old form; new rows differ, and the engine cannot tell the two apart (D5 item 5). The same applies to `String` columns filled from `to_string()`. No mindgraph-rs script does either (verified); external databases are unknown; the migration note carries the detection query and client-side recipe.
3. **Non-`.rs` query sources were not exhaustively searched** for JSON builtins (stored-query catalogs, `.md` docs examples). The `.rs` grep is the evidence; a stored query using `json_object` on a UUID would see the change — correctly.
4. **The `Json` payload move in the owning impl** is the one place two code paths remain; `canonical_internal_variants` is the guard, and a reviewer should reject any further arm added there.
5. **`Vec` non-contiguous case** is asserted at the Rust level only; no script can produce a non-contiguous `Array1` today, so the assertion proves the seam, not a user path.
6. **Validity re-entry gap** (D3): a Validity pair pulled out of a `Json` cell cannot be `:put` back into a `Validity` column. Pre-existing, unchanged, now documented.
7. **Doc pages.** `types/page.mdx:65-68` already states the top-level forms; the `functions` page entries for `json_object` (`:465`) and `set_json_path` (`:491`) were not run through `doc_check` in this pass — Batch A runs the `mnestic-docs` harness so any example that shows nested output is regenerated, not hand-edited.

## 10. Spec-authoring provenance

1. **Pass 1 — grounding (three lenses, 2026-08-28):** engine conversion paths and host surfaces; mindgraph-rs / mindgraph-cloud consumers and snapshot format; roadmap, changelog precedent, tests. Every citation re-checked against the working tree on 2026-08-28; a throwaway probe binary observed the nested/top-level/`to_string` output for Uuid, Bytes, floats, Vec, List, Json, Validity and the parse-back predicates.
2. **Pass 2 — lean draft.** Cuts: no compatibility flag, no tagged forms, no inbound type sniffing, no `json2val` fold, no product converter rewrite, no Display change.
3. **Pass 3 — adversarial panel (code-grounding verifier + semantics adversary + CI/process lens):** 26 findings, 3 blocker / 9 major / 14 minor; all folded (§7 triage). The two distinct blockers — the `val2str` quoting error and the third match body in `relation.rs` — each changed D2.
4. **Pass 4 — prior art (Postgres `json.c`/`to_json`, DuckDB `to_json` + #14646/#17329, SQLite JSON1, Arrow integration JSON + canonical extensions, Neo4j Typed JSON/Jolt/APOC, RFC 8259/8785, serde_json `Number`):** 9 CONFIRMS, 1 CONTRADICTS (non-finite float family), 3 GAP (Validity form, D4, D7), one essential omission (uniform NaN treatment); folded and cited at D1, D2, D3, D5, D6, D7, D8 and §7.
5. **Pass 5 — simplification (2026-08-28):** second probe confirmed every panel-predicted value (F32 widening, non-finite `Vec` elements, key derivation, `:put` coercion forms, Validity re-entry error, integral float text); six requirement-neutral cuts applied (§7); two owner rulings listed in the status header; leftover-sweep grep for pre-revision terminology (`all three backends`, `Python nested-UUID`, `both engines`, `one file`, `byte-identical`, `six call sites`, `§5`) run on the final text.
