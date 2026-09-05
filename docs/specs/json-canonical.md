# Canonical DataValue-to-JSON conversion (#54)

Status: released in Mnestic 0.18.0 (commit `1d13aedc`). The owner authorized the implementation and output compatibility contract on 2026-09-05. Replaces the 2026-08-28 draft (audit retained in git history).

## Outcome and boundary

One outbound representation applies to result cells, nested objects/lists, JSON builtins, JSON-derived keys, `to_string`, and writes into `Json` columns. The released 0.17.0 wheel renders nested UUIDs as integer arrays.

Preserve existing top-level JSON forms; nested values adopt those forms. The conversion is lossy and one-way, not a typed serialization format. Native binding conversions, CozoScript Display, inbound JSON conversion and MindGraph's persisted snapshot converter retain their own contracts.

## Canonical table

| Value | JSON representation | Boundary |
|---|---|---|
| Null / Bool | null / boolean | Unchanged |
| Int | exact i64 JSON integer | Floating-point consumers may lose precision beyond 2^53 |
| Finite Float | f64 JSON number, including 1.0 and signed zero | Unchanged |
| NaN | null | Indistinguishable from Null |
| Scalar +infinity / -infinity | "INFINITY" / "NEGATIVE_INFINITY" | Indistinguishable from strings with those contents |
| Str | string | Unchanged |
| Bytes | padded standard base64 string | Nested form changes; inbound strings are not retyped |
| Uuid | lowercase hyphenated string | Nested form changes; inbound strings are not retyped |
| Regex | pattern string | Internal-only; type lost |
| List | array of canonical elements | Recursive |
| Set | array in DataValue ordering | Internal-only; set type lost |
| F32 / F64 vector | numeric array; F32 widened to f64; non-finite elements null | Width and non-finite distinctions lost; preserve logical iteration order |
| Json | existing payload unchanged | Opaque; never reinterpret arrays or strings |
| Validity | [i64 timestamp, assertion boolean] | Inherited form; inbound plain list can be coerced, opaque Json array cannot |
| Bot | null | Internal-only; replaces panic |

## Implementation

- Add one borrowing `From<&DataValue> for JsonValue` match in `data/json.rs`. Recurse through lists/sets using that implementation. Iterate vectors instead of requiring contiguous slices; widen F32 elements to f64 before constructing JSON numbers, and emit null for non-finite vector elements.
- The owning implementation moves `DataValue::Json` payloads and delegates other variants to the borrowing implementation. Existing `Json` payloads are opaque and unchanged; do not sniff integer arrays for UUIDs.
- Delete `functions.rs::to_json`; route its JSON-building callers through the canonical conversion.
- `val2str` matches the resulting JSON value: return string payloads unquoted; stringify all other values as compact JSON. Do not create another DataValue policy.
- Replace the `ColType::Json` conversion match with a delegate. Keep nullable-column early-return semantics unchanged.
- `Bot` becomes null instead of panicking; this is an internal sentinel, not a newly exposed query value.
- No new inbound type sniffing, compatibility flag, tagged JSON, dependencies, storage format, or public binding API.

## Compatibility and migration

Nested UUIDs change from integer arrays to UUID strings; bytes change from integer arrays to base64 strings; scalar infinities change from null to the existing top-level strings. `to_string`, value-derived JSON keys and paths, and newly coerced `Json` columns use those forms too. NaN remains null; non-finite vector elements remain null. Integers remain exact i64 values, although consumers using floating-point numbers can lose precision beyond 2^53.

**Stored JSON is not rewritten.** Old array/null forms can coexist with newly written strings. Audit known typed fields, read affected rows, convert UUID byte arrays/base64 arrays client-side, then write explicitly. Never guess the type of arbitrary arrays. Old nulls from infinity have lost their sign/type and cannot be reconstructed without another source of truth. Existing `String` columns populated with `to_string` also retain old text.

Keep MindGraph's snapshot converter unchanged. Its format is separately persisted and its ordinary exported schema does not need engine UUID/vector/Validity conversions. Pin actual relation column types and populated export/import round trips, rather than searching arbitrary user strings for Debug-looking text.

## Acceptance

Lib unit tests in `data/tests/json.rs`, under existing default-feature CI:

- UUID, bytes, finite floats, signed zero, NaN, infinities, exact large integers, F32/F64 vectors, Validity, lists and opaque JSON: literal expected values at the top level, map member, list nesting, `json`, `json_object`, `set_json_path`, `to_string`, and a `Json` column.
- UUID-derived object keys, key lookup and removal use canonical text.
- Non-contiguous F32/F64 arrays convert without panic; vector non-finite behavior is pinned.
- Every DataValue variant has equal owning/borrowing output; set ordering is deterministic, regex is pattern text, Bot is null.
- Existing JSON that resembles an old UUID array stays unchanged. Existing stored rows survive reopen and coexist with newly canonicalized rows (SQLite fixture). No automatic migration.
- Pin inbound lossiness, NaN/null aliasing, and plain-string `to_string` behavior. Use the public string/JSON result API as well as internal conversion tests.
- Run engine tests, strict default-feature Clippy, consumer tests, changed docs examples and a freshly built Python binding smoke. Hosted CI and published-wheel acceptance remain release gates.

## Delivery

Engine conversion, tests, changelog and migration documentation are one bounded change. MindGraph snapshot-format coverage is a separate consumer change. Version bump, registry readmes, tags and publication belong to the release procedure. No bridge change is required.

## Review decisions, 2026-09-05

Keep NaN as null and all top-level compatibility forms. Fix the draft's migration implication that lost infinity values can be recovered; they cannot. Strengthen the persisted-JSON fixture and snapshot test to prove schema/round-trip contracts rather than a heuristic string blacklist. Remove obsolete 0.17.0 placement and ungrounded line-count estimates. Executable tests are authoritative.
