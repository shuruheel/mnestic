# Spec — Parquet / Arrow boundary I/O: atomic, chunked copy-in first; record-batch copy-out second

_Created 2026-08-18. Status: **BATCH A MERGED — D1–D11 signed by the owner 2026-08-18; local Rust, SQLite, RocksDB, Python wheel/sdist, and interoperability gates passed; hosted engine CI passed on PR #51; the publication-disabled cross-platform Python wheel/sdist matrix passed in run 32287514329. The feature remains unreleased.** Tracks [issue #11](https://github.com/shuruheel/mnestic/issues/11). Batch B export retains its separate sign-off gate in §8 and §11 and is not merged._

## 1. Outcome and scope

The first release slice adds a feature-gated host API that copies a local Parquet or Arrow IPC file into one **already-created, non-`TxTime` stored relation**. It converts and writes the source in row-bounded chunks through one database transaction, and either commits the whole import or leaves the relation unchanged.

This is an interchange boundary, not a second storage engine:

- **Copy-in, not federation.** The file is not queryable in place and cannot become a view or foreign relation.
- **Existing relation, not schema inference.** The stored relation owns names, types, keys, defaults, temporal behavior, and access policy.
- **Host API, not CozoScript file access.** V1 accepts a host-supplied local path. It adds no `LOAD FROM` grammar, sysop, or `FixedRule`.
- **Chunked conversion, atomic write.** `batch_rows` bounds conversion-loop work, not bytes: one value, source record batch, compressed page expansion, and the database transaction can all exceed that row count's intuitive memory size. Explicit resource ceilings are available, and their limits are stated honestly in §6.
- **Import first.** Query/relation export as Arrow record batches and Python's Arrow PyCapsule handoff are a separate second review unit (§8), because the current result path is row-major and fully materialized.

## 2. Verified current state (2026-08-18)

| Current fact | Code / policy evidence | Design consequence |
|---|---|---|
| The roadmap and issue #11 name a feature-gated, chunked import as the standalone first slice; record-batch/Python export follows | [`ROADMAP.md:65-72`](../../ROADMAP.md), [issue #11](https://github.com/shuruheel/mnestic/issues/11) | The first review unit must not depend on export |
| `data-import` registers script-controlled CSV/JSON readers; `requests` separately permits outbound fetches; `rdf-io` composes the two trust decisions | [`cozo-core/Cargo.toml:25-64`](../../cozo-core/Cargo.toml) | A future script reader must use the existing trust gate; a host-only API need not register a reader |
| `FixedRule` arity is known before execution, and its complete output materializes in `RegularTempStore` before the consuming query continues | [`fixed_rule/mod.rs:791-820`](../../cozo-core/src/fixed_rule/mod.rs), [`query/eval.rs:246-260`](../../cozo-core/src/query/eval.rs), [`runtime/temp_store.rs:26-41`](../../cozo-core/src/runtime/temp_store.rs) | A `ParquetReader` FixedRule would not satisfy bounded-memory bulk ingestion and is excluded from V1 |
| `NamedRows` owns every row in a `Vec<Tuple>`; `import_relations` therefore receives a fully materialized input | [`runtime/db.rs:263-291`](../../cozo-core/src/runtime/db.rs), [`runtime/db.rs:1151-1173`](../../cozo-core/src/runtime/db.rs) | V1 needs an internal iterator/batch write seam rather than repeated public `import_relations` calls |
| `import_relations` uses one write transaction, coerces against the target relation, updates B-tree indexes, bypasses triggers/callbacks and search-index maintenance, then commits once | [`runtime/db.rs:1151-1160`](../../cozo-core/src/runtime/db.rs), [`runtime/db.rs:1205-1363`](../../cozo-core/src/runtime/db.rs) | Columnar import must preserve this bulk-write contract, including the `::reindex` warning |
| `TxTime` bulk import buffers every converted row in `pending_tt_writes` until commit | [`runtime/db.rs:1220-1269`](../../cozo-core/src/runtime/db.rs), [`runtime/transact.rs:379-457`](../../cozo-core/src/runtime/transact.rs) | `TxTime` targets are excluded from V1 rather than falsely called chunked |
| The stored type system is narrower than Arrow's and existing coercion enforces nullability, list/vector widths, UUID parsing, and the user-supplied-`TxTime` prohibition | [`data/relation.rs:85-112`](../../cozo-core/src/data/relation.rs), [`data/relation.rs:187-355`](../../cozo-core/src/data/relation.rs) | Conversion must be explicit and terminate in the existing coercion path |
| Rust query results are owned row-major values with header names but no fixed column schema; final evaluation collects a second row vector | [`runtime/db.rs:263-291`](../../cozo-core/src/runtime/db.rs), [`runtime/db.rs:3663-3685`](../../cozo-core/src/runtime/db.rs) | Export cannot be implemented as a zero-copy view over `NamedRows` |
| Python converts every cell into ordinary Python objects and nested lists | [`cozo-lib-python/src/lib.rs:376-433`](../../cozo-lib-python/src/lib.rs) | A later Arrow handoff can remove Python boxing, but only after constructing Arrow buffers |
| The public backend-erasing seam dispatches each import/export method from `DbInstance` to every compiled backend | [`cozo-core/src/lib.rs:680-763`](../../cozo-core/src/lib.rs) | V1 needs matching `Db` and `DbInstance` methods; storage bridges do not change |
| Repository policy treats public third-party types as semver-load-bearing | [`cozo-core/src/lib.rs:72-86`](../../cozo-core/src/lib.rs) | V1 exposes only mnestic-owned option/report types, not arrow-rs types |

## 3. V1 public contract

All items below exist only under a new off-by-default `columnar-io` Cargo feature. These are the implemented Batch A API names; they are not promises about Batch B.

```rust
#[non_exhaustive]
pub enum ColumnarFileFormat {
    Parquet,
    ArrowIpcFile,
    ArrowIpcStream,
}

#[non_exhaustive]
pub struct ColumnarImportOptions { /* private fields; constructor + builders */ }

#[non_exhaustive]
pub struct ColumnarImportReport {
    /// Source rows successfully converted and submitted to put semantics.
    pub rows_processed: u64,
    /// Source record batches decoded by the format reader. Conversion-loop
    /// slices of an oversized IPC batch are not counted separately.
    pub batches_processed: u64,
    /// Search-index names whose contents do not include the imported rows.
    pub search_indexes_requiring_rebuild: Vec<String>,
}

impl<'s, S: Storage<'s>> Db<S> {
    pub fn import_columnar_file(
        &'s self,
        relation: &str,
        path: impl AsRef<Path>,
        options: &ColumnarImportOptions,
    ) -> Result<ColumnarImportReport>;
}
```

`DbInstance` exposes the same method and dispatches to the selected backend. The Python binding exposes:

```python
db.import_columnar_file(
    relation: str,
    path: os.PathLike[str] | str,
    *,
    format: Literal["parquet", "arrow_ipc_file", "arrow_ipc_stream"],
    columns: Mapping[str, str] | None = None,
    batch_rows: int = 8192,
    timeout: float | None = None,
    max_source_bytes: int | None = None,
    max_rows: int | None = None,
    max_decoded_batch_bytes: int | None = None,
    max_value_bytes: int | None = None,
    max_nesting_depth: int = 16,
) -> dict[str, object]
```

`ColumnarImportOptions::new(format)` supplies the same defaults and builder methods as the Python keywords. Private fields plus `#[non_exhaustive]` public types leave room for additive formats and limits without another source break.

The exact Rust builders are `with_columns(BTreeMap<String, String>)`, `with_batch_rows(usize)`, `with_timeout(Option<Duration>)`, `with_max_source_bytes(Option<u64>)`, `with_max_rows(Option<u64>)`, `with_max_decoded_batch_bytes(Option<usize>)`, `with_max_value_bytes(Option<usize>)`, and `with_max_nesting_depth(usize)`. Read-only accessors mirror each setting. Non-positive Python limits — `timeout` included — and zero Rust row/byte/depth limits or a zero-`Duration` timeout fail at option validation; `None` is the sole unbounded spelling.

`batch_rows` defaults to 8,192, must be greater than zero, and is an operational conversion/write-loop bound rather than a memory promise. Parquet configures the reader with that batch size. An oversized Arrow IPC record batch is retained in full while zero-copy slices no larger than the row bound are converted; slicing does not free or shrink its shared backing buffers.

The optional resource ceilings are checked with overflow-safe accounting: source file bytes before decoding; total observed rows; each decoded batch's full Arrow buffer footprint; each variable-width value before copying it into a `DataValue`; and schema nesting depth during preflight. `None` means unbounded for that dimension. These guards limit admitted work and conversion copies, but `max_decoded_batch_bytes` is necessarily checked **after** the selected decoder has allocated a batch and therefore is not a hard decoder-peak-memory sandbox. `timeout` is explicit wall-clock seconds; it does not register the import under `::running`/`::kill`, and the database's query-default timeout does not silently apply to a non-query host operation. The same isolation holds for the per-query memory budget: `:mem_limit` and its engine accounting neither charge nor bound this host operation — the ceilings above are its only limits. None of those ceilings bounds the accumulated storage write batch of the single transaction; `max_source_bytes` is the deliberate proxy for that dimension (§13).

The format is explicit. Extension guessing is omitted because IPC file and stream encodings are different, a path suffix is not authoritative, and a wrong choice should fail before a write.

## 4. Source and column contract

V1 accepts exactly one process-readable **local regular file**. It opens the path once read-only, verifies `File::metadata().is_file()` on that open handle, and passes that same handle to the decoder—no check-then-reopen race. It does not accept HTTP(S), object-store URLs, globs, directories, partitioned datasets, file-like callbacks, memory maps supplied by a caller, or an Arrow C stream pointer. The host caller has ambient filesystem authority; mnestic neither canonicalizes nor authorizes the path, and symlink resolution is the operating system's normal open behavior.

The target relation must already exist, must not be an index relation, and must pass the same access gate as `import_relations`: relation access level at least `Protected`. The operation is put/upsert, not insert-only, delete, create, or replace.

Column resolution is deterministic:

1. `columns` maps **target stored name → source field name**.
2. Every target key and non-key column not listed uses an identical source name.
3. A relation containing an engine-assigned `TxTime` column is rejected in V1. The existing commit path buffers the entire pending write, which would defeat this slice's chunked-conversion goal.
4. Every stored column is required in V1, even if its catalog definition has a default expression. Defaults are query-expression machinery and are not evaluated by the existing bulk path.
5. Referenced source names must be unique in the input schema. A missing or ambiguous source field or unknown target mapping fails during preflight, before the first row write.
6. Unreferenced source fields are ignored and projected out where the reader supports projection. One source field may feed more than one target field; conversion is independently checked against each target.

No source schema is persisted. Re-running against a changed file is revalidated from scratch. Re-import is also not value-deterministic for `Validity` columns fed by `"ASSERT"`/`"RETRACT"` strings: each run stamps them with that run's single preflight-captured time (§5).

## 5. Type conversion contract

The decoder first validates extension metadata, then maps one Arrow scalar to a `DataValue`; the target's existing `NullableColType::coerce` remains the final authority after the additional losslessness and cross-kind prechecks below. Unknown or malformed extensions fail **before** dispatch on their underlying storage type. A null always becomes `DataValue::Null` and therefore fails for a non-null target. Conversion errors identify the relation, target column, source field, zero-based source row, Arrow source type, and stored target type.

| Arrow field | Intermediate `DataValue` | V1 rule |
|---|---|---|
| `Null` | `Null` | Accepted only by nullable targets |
| `Boolean` | `Bool` | Direct |
| signed integers through 64 bits | `Num::Int(i64)` | Direct |
| unsigned integers through 64 bits | `Num::Int(i64)` | Checked; values above `i64::MAX` fail, never wrap or become float |
| `Float16`, `Float32`, `Float64` | `Num::Float(f64)` | Widening only; NaN and infinities remain floats |
| `Utf8`, `LargeUtf8`, `Utf8View` | `Str` | UTF-8 is already validated by Arrow |
| `Binary`, `LargeBinary`, `BinaryView`, `FixedSizeBinary` | `Bytes` | Direct; no implicit UTF-8 guess |
| canonical `arrow.uuid` or Parquet `UUID` | `Uuid` | Requires the specified 16-byte big-endian storage form |
| canonical `arrow.json` | parsed `Json` | Invalid RFC 8259 text fails; an unannotated string stays `Str` and cannot feed a `Json` target (see the cross-kind matrix) |
| `List`, `LargeList`, `FixedSizeList` of supported values | recursive `List` | Child nullability and target list/vector length are enforced by existing coercion |
| dictionary or run-end encoding over a supported logical value | the decoded logical value above | Encoding is transparent; dictionary index values are never imported as data |

Numeric target conversion is stricter than the legacy general coercer: integer → `Float` and float → `Int` are accepted only when an exact round trip proves no range or precision loss; non-finite or fractional floats never become integers. The same precheck applies recursively inside lists, tuples, and vectors. This prevents an imported `i64` above exactly representable `f64` range from silently rounding, and prevents Rust's saturating float-to-integer cast from turning an out-of-range source into a boundary value.

The numeric prechecks alone do not close the legacy coercer's liberal **cross-kind** arms, so V1 additionally prechecks every intermediate value's kind against the target column type before calling `coerce`. The complete cross-kind contract:

| Intermediate → target | V1 behavior |
|---|---|
| `Str` → `Bytes` | **Fails.** The legacy implicit base64 decode ([`data/relation.rs:237-250`](../../cozo-core/src/data/relation.rs)) is unreachable from columnar import |
| `Str` → vector (`<F32; n>` / `<F64; n>`) | **Fails.** The legacy base64-decode-then-reinterpret-as-floats path ([`data/relation.rs:301-331`](../../cozo-core/src/data/relation.rs)) is unreachable from columnar import |
| anything except parsed `arrow.json` → `Json` | **Fails.** The legacy wrap-any-value arm would store unannotated JSON text as a JSON *string scalar* — a silent meaning change asymmetric with the annotated path; a `Json` target requires the `arrow.json` annotation in V1 |
| `Str` → `Uuid` | **Accepted, documented.** RFC 4122 text parses through existing coercion |
| `Str` / `List` → `Validity` | **Accepted, documented.** The RFC 3339, `"ASSERT"`/`"RETRACT"`, and `[microseconds, bool]` forms already accepted by `coerce` |
| `List` of numerics → vector | **Accepted.** Existing coercion enforces element type and exact width |
| any intermediate → `Any` | **Accepted.** `Any` targets pass values through unchanged |
| every other cross-kind pair | **Fails** through the existing coercion error, with the §5 error context |

Cross-kind acceptances are additive later. Each precheck-closed pair fails with an error naming the disabled legacy conversion, so a caller reads a policy decision rather than a bug.

Everything else fails closed in V1: decimal, date, time, timestamp, duration, interval, map, struct, union, tensor/opaque/unknown extension types, and Parquet variant/geospatial logical types; encrypted Parquet files fail at decode with a clear error. Their physical storage must not be silently imported as integers or bytes. `Validity` may be populated only through an otherwise supported string/list representation already accepted by `NullableColType::coerce`. One `current_validity()` is captured at preflight and shared by every batch, so all `"ASSERT"`/`"RETRACT"` rows in a run carry one stamp — matching `import_relations` — and re-importing the same file necessarily stamps those rows differently. Relations containing `TxTime` are rejected in V1.

This narrow table is deliberate. Parquet distinguishes physical storage from logical meaning—for example, strings are annotated byte arrays, UUID is a specified 16-byte form, decimals carry scale/precision, and timestamps carry units and UTC semantics. Importing only the physical primitive would erase meaning. New mappings are additive once mnestic has a target type or an explicit lossless boundary representation.

Logical-meaning detection is pinned to the authoritative descriptor per format. For Parquet, the reader consults the Parquet schema's logical-type descriptors (UUID, JSON) directly rather than trusting that the parquet→Arrow schema conversion attached `arrow.uuid`/`arrow.json` extension metadata — that attachment is a version- and feature-dependent behavior of the pinned crates. For IPC, Arrow field extension metadata is authoritative. Without this pinning, a UUID column written by an independent producer could arrive as bare `FixedSizeBinary(16)`, become `Bytes`, and hard-fail against a `Uuid` target (`get_uuid` accepts only UUID values and text, [`data/value.rs:738-744`](../../cozo-core/src/data/value.rs)) with an error that misnames the real problem.

## 6. Transaction, indexing, timeout, resources, and failure

Preflight opens and validates the file/schema, resolves columns, validates supported logical types, resolves the target relation, obtains the existing relation lock, and checks access **before** processing rows. After preflight:

- one write transaction spans every decoded batch;
- each row uses the existing bulk put/coercion/index helper extracted from `import_relations` rather than recursively calling the public API; the extracted helper defines its index-rewrite check on the **coerced, storage-ordered** tuple — not the raw input row that `import_relations` compares today ([`runtime/db.rs:1324`](../../cozo-core/src/runtime/db.rs)), a quirk that is merely wasteful there (spurious index del/re-put) but would be semantically wrong for columnar rows, and which the shared helper fixes for both callers;
- duplicate keys within the source or against existing data use the current bulk put/last-row-wins behavior;
- B-tree secondary indexes are maintained;
- HNSW, FTS, and LSH indexes are not maintained; one structured warning directs the caller to `::reindex`, and their names are returned in `search_indexes_requiring_rebuild` so success cannot hide stale search state;
- triggers and callbacks do not run;
- graph-projection invalidation follows the existing bulk-import dirty hook;
- any open, decode, validation, coercion, resource-limit, storage, timeout, or commit error aborts the transaction and returns no success report.

Every new failure class carries a stable miette diagnostic code under a reserved `columnar::` namespace (matching the existing `import::`/`eval::` style); acceptance tests assert codes, not message text. A source admitted by preflight that yields zero rows is a **success**: the transaction commits having written nothing and the report returns zero counts.

The conversion/write loop checks the explicit deadline and row/value limits at least every 4,096 converted rows, at every batch boundary, and before commit. A timeout during one blocking arrow-rs decode call is observed only after that call returns; V1 makes no `::kill` or mid-decoder-callback promise. Python releases the GIL for the full engine operation.

“Chunked” means the conversion/write loop processes no more than `batch_rows` logical rows at once. It does **not** promise O(`batch_rows`) memory: IPC slicing retains the complete source batch's buffers; one value can be arbitrarily wider than an ordinary row unless `max_value_bytes` is set; decompression can allocate before the post-decode byte check; and RocksDB/SQLite may retain locks, write batches, WAL state, index entries, or old versions until the one atomic commit. V1 therefore gives precise optional ceilings and observability, not a false hard-memory sandbox. A future partial-commit mode would be a different failure contract and is excluded.

The single transaction also has a **concurrency** contract, not just a memory one. On SQLite the storage layer admits one writer at a time, so a large import blocks every other write for its full duration. On RocksDB the pessimistic transaction takes per-key locks as it writes; a concurrent script writing the same keys conflicts, and the storage layer surfaces that as an error on whichever transaction requests the lock second, subject to the lock-timeout configuration — the Batch A concurrency lane pins which side fails on each backend. Preflight takes the relation lock's **read** guard (the same guard `import_relations` takes, [`runtime/db.rs:1167-1168`](../../cozo-core/src/runtime/db.rs)), which excludes concurrent destructive schema operations for the whole run but deliberately admits concurrent writes — including a second import — into the same relation; those are resolved by the storage transaction, not rejected up front. Operators importing large files into a live database should expect writer stalls and schedule accordingly.

## 7. Feature, dependency, security, and packaging ruling

`columnar-io` is off by default and gates all Arrow/Parquet dependencies and public methods. It uses the official Apache arrow-rs `arrow-array`, `arrow-data`, `arrow-schema`, `arrow-ipc`, and `parquet` crates at one exact compatible release line; their types are not re-exported from mnestic's public API. `default-features = false`; the selected features must include Arrow canonical extension types, Parquet Arrow conversion, Snappy, Zstandard, and Parquet CRC verification, without enabling unrelated async/cloud/object-store stacks. The implementation PR records the resolved arrow-rs MSRV against mnestic's supported Rust toolchain.

Batch A resolves that family exactly to **59.2.0**. Cargo metadata reports `rust-version = 1.85` for all five crates, matching mnestic's supported Rust 1.85 floor (verified 2026-08-18; the local build lane ran on Rust 1.96.0).

`columnar-io` does **not** imply `data-import` because V1 registers no CozoScript reader. `requests` does not change this API: URL-shaped strings are ordinary local paths and fail as such. If a future `ParquetReader`/`ArrowReader` is made script-invocable, that review unit must make `columnar-io` imply the existing `data-import` trust gate and separately decide whether `requests` may add remote reach.

All IPC structural validation remains enabled; the arrow-rs unsafe skip-validation controls are never used. In addition, every decoded column runs recursive `ArrayData::validate_full()` before any scalar access, including nested offsets, dictionary indices, run ends, and UTF-8. Parquet page CRCs are verified when present. Malformed-file fixtures must return errors rather than panic. Apache Arrow's security guidance specifically warns that IPC readers may validate framing without fully validating array data, and that C Data Interface pointers cannot be made safe when supplied by an untrusted producer.

The Python crate gains a matching `columnar-io = ["cozo/columnar-io"]` passthrough. The proposed release posture is to compile it into published wheels and sdists, while the core Rust crate's default remains lean. Because Zstandard may compile native support, the build matrix—not a “pure Rust” label—is authoritative. The implementation PR must prove clean sdist builds on every advertised platform and report wheel/sdist size and build-time deltas before the owner signs the release change; there is no invented percentage gate in this spec.

Batch A's local release-profile measurement on 2026-08-18 used the real macOS arm64 wheel feature set (`compact,storage-rocksdb,rdf-io`) and then added `columnar-io`. The abi3 wheel grew from 11,473,515 to 13,185,157 bytes: +1,711,642 bytes (+14.92%). The baseline was a cold release build (283.92 seconds wall time); the columnar build reused that native cache and took 114.92 seconds, so those timings prove successful builds but are not presented as a controlled performance ratio. The sdist grew from 5,738,111 to 5,738,177 bytes: +66 bytes; warm archive builds took 1.90 and 1.65 seconds respectively. A clean Python 3.12 environment built and installed the columnar-enabled sdist in 148.22 seconds and exposed the method. A wheel-installed Python 3.12 smoke imported 500,000 IPC rows and a competing Python thread advanced 14,586,942 iterations during the call, proving the binding released the GIL. PyArrow 25 independently produced canonical UUID/JSON fields and a delta-dictionary IPC stream that imported successfully; its encrypted Parquet and a damaged checksummed Parquet were rejected with `columnar::` diagnostics. Cross-platform CI remains the authority for the other advertised targets.

## 8. Phase 2 — Arrow export and Python handoff (separate review unit)

Phase 2 is architecturally reserved here but is **not build-authorized by signing the V1 import decisions**. Its required contract is:

1. A stored-relation export produces bounded Arrow `RecordBatch` chunks with one stable schema for the stream. A query-result export is blocked until it has a separately signed typed-output contract; V1 query results carry no schema.
2. It does not call `export_relations` and then convert a complete `NamedRows`; it adds an internal relation-scan sink so rows are drained batch by batch.
3. Constructing columnar Arrow buffers from mnestic's row-major `DataValue`s is a conversion and usually a copy. Documentation must not call that engine step zero-copy.
4. Once Rust owns Arrow buffers, Python exposes `__arrow_c_stream__(self, requested_schema=None)` so PyArrow, Polars, and other compatible consumers can take those buffers without another binding-layer copy or a hard PyArrow runtime dependency. A compatible requested representation may be honored; a different field shape raises.
5. The C stream's `private_data`/release state owns or reference-counts its schema, iterator, batches, and buffers independently of both the database and the Python wrapper. Closing the database or dropping the wrapper does not invalidate already-produced state.
6. Each returned `"arrow_array_stream"` capsule is one-consumer-only. An unconsumed capsule destructor invokes the release callback; after a consumer moves the stream and nulls the capsule-owned callback, that consumer owns release. Repeated calls on the exporter create fresh independent streams or raise a documented consumed-state error—the Phase 2 API must choose. No raw integer pointers and no private `_import_from_c` dependency form the primary API.
7. Stored relations with concrete catalog types can define an Arrow schema. `Any`, nested `Any`, and unsupported concrete types are rejected rather than inferred or stringified. Query export remains blocked until the query language/API can supply a stable typed-output schema without scanning a complete materialized result first.

This phase deliberately accepts that any Rust API returning arrow-rs `RecordBatch` makes that dependency semver-load-bearing under the feature. The implementation spec must choose between that ergonomic cost and a mnestic-owned C-stream wrapper; V1 import avoids forcing the choice early.

## 9. Layering ruling

This belongs in **mnestic only**. Parquet/Arrow conversion is stable, general database mechanism that passes the stranger test and contains no cognitive or tenancy vocabulary. It adds no MindGraph ontology, retrieval policy, cloud credential, or tenant behavior. The stack rule is mechanism in mnestic, meaning in MindGraph, operations in cloud ([`docs/strategy/LAYERING.md:18-37`](../../../docs/strategy/LAYERING.md)).

MindGraph and mindgraph-cloud need no change merely because V1 ships. They would opt in only if a product flow later needs bulk dataset intake or Arrow-native analytics output; that product API, authorization, object storage, tenant quotas, audit, and billing stay above the engine.

## 10. Prior art and decision audit (researched 2026-08-18)

Primary sources consulted:

- Apache Arrow's [columnar format](https://arrow.apache.org/docs/format/Columnar.html), [IPC file/stream readers in arrow-rs](https://arrow.apache.org/rust/arrow_ipc/reader/index.html), [Parquet record-batch reader](https://arrow.apache.org/rust/parquet/arrow/arrow_reader/type.ParquetRecordBatchReaderBuilder.html), [C stream interface](https://arrow.apache.org/docs/format/CStreamInterface.html), [PyCapsule interface](https://arrow.apache.org/docs/format/CDataInterface/PyCapsuleInterface.html), [canonical extensions](https://arrow.apache.org/docs/format/CanonicalExtensions.html), and [security guidance](https://arrow.apache.org/docs/format/Security.html).
- Apache Parquet's [logical-type specification](https://parquet.apache.org/docs/file-format/types/logicaltypes/).
- DuckDB's [`COPY` contract](https://duckdb.org/docs/lts/sql/statements/copy), [Parquet import](https://duckdb.org/docs/current/guides/file_formats/parquet_import), and [record-batch export](https://duckdb.org/docs/current/guides/python/export_arrow).

| Decision area | Prior-art result | Spec effect |
|---|---|---|
| Copy into an existing typed relation | **CONFIRMS.** DuckDB `COPY ... FROM` targets an existing table and rejects non-convertible fields; its richer schema/default conveniences are not automatically portable | V1 targets an existing relation and uses its schema, while omitting new query grammar |
| Batch-shaped import | **CONFIRMS, with a caveat.** arrow-rs reads Parquet and both IPC encodings as iterators of `RecordBatch`; the Parquet builder exposes batch size and projection, but slicing an IPC batch retains its backing buffers | One internal batch writer and explicit file formats; `batch_rows` is not mislabeled a byte-memory bound |
| Logical-type fidelity | **CONFIRMS.** Parquet logical annotations distinguish strings, UUID, decimals, and temporal units from their primitive storage; Arrow canonical extensions define interoperable UUID/JSON forms | Supported mappings are explicit and unsupported meaning fails closed |
| One-schema stream | **CONFIRMS.** Arrow C stream requires every chunk to share one schema | Preflight resolves one schema; schema drift is an error |
| Python protocol | **CONFIRMS.** Arrow standardizes `__arrow_c_stream__` capsules and their release/one-consumer lifetime; it recommends this over private PyArrow pointer APIs | Phase 2 uses the standard capsule and has no hard PyArrow runtime dependency |
| “Zero-copy” boundary | **CONTRADICTS an unqualified claim.** Arrow enables zero-copy sharing of already-columnar buffers, but mnestic currently owns row-major `DataValue`s | The spec promises no extra Python handoff copy, not zero-copy row-to-column conversion |
| Validation | **CONFIRMS.** Arrow recommends explicit full-data validation for untrusted IPC and warns that C pointers cannot be validated as an authority boundary | V1 recursively validates decoded arrays, verifies present Parquet CRCs, and excludes raw C streams from file import |
| Live file querying | **GAP for this product contract.** DuckDB supports direct Parquet scans and views, but mnestic's roadmap expressly rejects federation/foreign relations | Prior art demonstrates the option; the layering/product strategy deliberately rejects it |

**Essential omitted source:** DataFusion was surveyed but omitted from the decision table. It confirms the same arrow-rs `RecordBatch`/Parquet streaming primitives but contributes no distinct contract decision beyond the direct Apache Arrow and DuckDB sources above.

## 11. Acceptance tests and delivery batches

Stored-relation behavior is tested primarily on SQLite per the repository rule in [`CLAUDE.md:70-78`](../../CLAUDE.md). Because the public `DbInstance` dispatch promises atomicity across compiled transactional backends, late-failure rollback also runs on real RocksDB, and the secondary-backend contract lanes exercise it wherever CI provisions that backend. Feature wiring gets no-default-features compile coverage.

### Batch A — import core (one review unit)

1. **Formats:** Parquet (uncompressed, Snappy, Zstandard), Arrow IPC file, and Arrow IPC stream fixtures import the same rows. A zero-row file and a file smaller than one `batch_rows` chunk succeed with exact counts.
2. **Chunk/resource behavior:** files larger than three configured chunks succeed; the conversion loop never processes more than `batch_rows` logical rows at once; an oversized IPC source batch is sliced while a test proves its full shared buffers remain retained, asserted via the batch's reported Arrow buffer footprint (`get_array_memory_size`), not by intuition. Each optional source/row/batch/value/nesting limit is pinned at below/equal/above-boundary cases.
3. **Projection/mapping:** identity, rename, reordered source fields, ignored extras, one-source-to-two-targets, and missing/ambiguous/unknown mappings.
4. **Primitive fidelity:** null, bool, signed/unsigned integer boundaries, exact/lossy integer↔float boundaries, float including NaN/infinity, UTF-8, binary, UUID, and JSON extension. Extension metadata is examined before storage type. Fixtures include dictionary-encoded and run-end-encoded columns, IPC streams with delta dictionary batches, `Utf8View`/`BinaryView`/`Float16` arrays, and UUID/JSON columns written by an independent producer (e.g. pyarrow) — not only arrow-rs round-trips.
5. **Nested/vector fidelity:** variable and fixed lists, nullable children, exact and wrong vector widths.
6. **Fail-closed types:** decimal, temporal, map, struct, union, unknown extension, oversized `UInt64`, invalid JSON, invalid UUID, encrypted Parquet, malformed IPC/Parquet, and every precheck-closed cross-kind pair (`Str`→`Bytes`, `Str`→vector, unannotated text→`Json`) return contextual errors carrying `columnar::` diagnostic codes, without panic.
7. **Atomicity:** a conversion failure after at least one full batch leaves no imported changes; a commit failure likewise returns no report. The late-failure case runs on SQLite and real RocksDB plus provisioned secondary-backend lanes.
8. **Put semantics:** existing keys and duplicate source keys match `import_relations` last-row-wins behavior; importing identical rows through `import_relations` and through columnar import yields byte-identical stored state (keys, values, and B-tree index entries) — the cheapest proof that the extracted helper did not drift.
9. **Indexes:** B-tree indexes reflect imported rows; HNSW/FTS/LSH remain unchanged, emit exactly one actionable `::reindex` warning, and appear by name in `search_indexes_requiring_rebuild`.
10. **Temporal:** a target relation containing `TxTime` is rejected before writes; ordinary valid-time `Validity` input still follows the explicit supported representations.
11. **Lifecycle:** triggers/callbacks do not fire; graph projections are invalidated; insufficient access fails before writes.
12. **Timeout:** the explicit deadline trips during conversion or before commit and leaves no partial state; a timeout during a blocking decode is reported immediately after that call returns. Python's GIL is free while the call runs.
13. **Security/robustness:** every decoded array receives recursive full validation; corrupt offsets, UTF-8, dictionary indices, run ends, nesting, and Parquet checksums return errors; representative Apache Arrow fuzz-regression fixtures do not panic; the file handle is opened once and URL/glob/directory inputs are not reinterpreted.
14. **Feature/package matrix:** default core build contains no Arrow/Parquet deps; `columnar-io` builds on the supported target matrix with canonical extensions and CRCs; Python wheel/sdist smoke-test the method, native-code toolchains, resolved MSRV, and artifact/build-time deltas.
15. **Concurrency:** a columnar import racing a concurrent scripted write is pinned per backend — SQLite serializes (one side waits or errors, never corrupts); RocksDB surfaces a key conflict on the defined loser; two concurrent imports into one relation resolve without corruption. Runs on the same backend lanes as test 7.

### Batch B — export (new sign-off required)

Before implementation, turn §8's reserved contract into its own exact API/type matrix and acceptance tests. At minimum: stable per-stream schema, bounded chunks, concrete stored-type mapping, `Any`/nested-`Any` rejection, a query typed-output prerequisite, exact requested-schema behavior, capsule ownership/destructor behavior, database-close independence, PyArrow/Polars interoperability, and proof that the Python boundary adds no buffer copy.

## 12. Owner decisions — approved 2026-08-18

| # | Question | Approved ruling |
|---|---|---|
| D1 | Import first or import/export together? | **Import first.** Export is a separate sign-off and review unit |
| D2 | FixedRule / query syntax or host API? | **Host API only in V1.** A FixedRule would materialize and expose script file authority |
| D3 | Target creation/inference? | **Existing relation only; target schema is authoritative** |
| D4 | Chunk commits or whole-file atomicity? | **One transaction / one commit.** Row-bounded conversion, explicitly not a decoder- or transaction-memory bound |
| D5 | Source reach? | **One local file; explicit Parquet / Arrow IPC file / Arrow IPC stream format.** No URLs, globs, or datasets |
| D6 | Conversion posture? | **Narrow mappings + numeric losslessness prechecks + the §5 cross-kind acceptance matrix over the legacy coercer: liberal string decodings (implicit base64 bytes, raw-float vectors, wrap-as-JSON) are precheck-closed; `Str`→`Uuid`/`Validity` are the documented acceptances; unsupported logical meaning fails closed** |
| D7 | Feature and wheel posture? | **Core off by default; published Python wheel/sdist compile the passthrough after reporting size/build deltas** |
| D8 | Index and lifecycle semantics? | **Match `import_relations`: B-tree yes; search indexes/triggers/callbacks no; warn and return stale index names** |
| D9 | What may “zero-copy” mean later? | **Only Arrow-buffer handoff through the standard PyCapsule protocol; never the row-to-column conversion** |
| D10 | `TxTime` relations in V1? | **Reject them.** The current commit path retains all pending rows, contradicting the chunked-conversion goal; add only with a separately designed stamp/write seam |
| D11 | Cancellation/resource posture? | **Explicit timeout plus optional source/row/batch/value/nesting ceilings; no `::kill` claim, no hard decoder-memory promise, no pending-write-bytes ceiling (`max_source_bytes` is its proxy), and the per-query memory budget does not apply** |

## 13. Rejected alternatives

- **`ParquetReader` / `ArrowReader` as a FixedRule in V1:** rejected because output materializes in a deduplicating temp B-tree and arity must be known before run; it does not implement chunked stored-relation ingestion.
- **Repeated `import_relations` calls per batch:** rejected because each call commits separately, exposing partial imports and assigning multiple transaction times.
- **New `LOAD FROM` syntax or sysop:** rejected as unnecessary grammar and a misleading echo of federation; the existing API-dispatch seam is enough.
- **Infer/create a stored relation from Arrow schema:** rejected because key choice, defaults, temporal axes, and `Any` vs typed columns are database policy, not file metadata.
- **Treat unsupported logical types as physical integers/bytes or stringify them:** rejected because it silently erases scale, units, timezone, or extension meaning.
- **Remote/object-store support through the current `minreq` helper:** rejected because it buffers the whole response and has no explicit size/timeout/host policy at the reader callsite, contradicting the bounded first slice.
- **Expose arrow-rs types in V1:** rejected because import does not need them and they would become a public semver constraint.
- **Claim end-to-end zero-copy export:** rejected because the current engine result is owned row-major `DataValue`; Arrow buffers must first be built.
- **A `max_pending_write_bytes` ceiling on the accumulated transaction:** rejected for V1 because the storage layers expose no portable pending-write accounting; `max_source_bytes` is the honest proxy, and §6 says so rather than implying a bound that does not exist.
- **A public progress callback:** rejected for V1 to keep the API one call with one report; a per-batch debug-level log line is permitted as non-contractual observability, and the report stays the sole programmatic output.

## 14. Spec-authoring provenance and simplification record

1. **Internal grounding:** three bounded fact-only repository passes covered import/transaction/trust, result/Python/export, and roadmap/types/layering/test seams. Facts were rechecked against the 2026-08-18 working tree; no grounding agent edited files.
2. **Lean draft:** the draft separates one buildable import unit from the export architecture it must not accidentally freeze.
3. **Adversarial panel:** three lenses (code/transaction verifier, Arrow/type/Python interop, and security/operability/layering) returned 22 raw findings. Overlap collapsed to 18 unique contract changes; all 18 were accepted, including corrections to API lifetime, memory wording, timeout reachability, `TxTime`, numeric losslessness, validation/CRC, extension dispatch, public-type extensibility, file-open authority, backend rollback tests, stale-index reporting, packaging/MSRV, `Any` export, and PyCapsule ownership. No finding was rejected.
4. **Prior art:** official Apache Arrow, Apache Parquet, and DuckDB sources were checked per decision area; results are in §10.
5. **Simplification:** final pass complete. Cuts retained: no grammar, no FixedRule, no remote/dataset reach, no schema creation, no public Arrow type, no `TxTime` special-case buffering, no export implementation in V1, no partial-commit mode, and no unprovable end-to-end zero-copy or hard-memory claim.
6. **Post-draft grounded review (2026-08-18):** a second verification pass re-checked every §2 citation against the working tree (all held) and added: the §5 cross-kind acceptance matrix (amending D6) and Parquet logical-type detection pinning; the single-`current_validity()` determinism rule; the §6 concurrency contract, coerced-tuple index-rewrite definition, zero-row ruling, and `columnar::` diagnostic namespace; the memory-budget non-interaction statement (amending D11); and the Batch A additions — dictionary/run-end/delta-dictionary, view-type and `Float16` fixtures, independent-producer UUID/JSON files, encrypted Parquet, `import_relations` byte-equivalence, and the concurrency lane.
