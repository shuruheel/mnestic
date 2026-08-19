# Spec — Cross-Instance RocksDB Memory: one shared block cache + WriteBufferManager envelope for many embedded instances in one process

_Created 2026-08-16. Status: **SHIPPED in 0.15.0 — §11's six decisions signed by the owner 2026-08-16, all as proposed.** (Originally: PROPOSED — awaiting owner sign-off.) This is the engine-side spec for a mechanism pulled by a real multi-instance embedding whose host-side memory work is already in place — the engine mechanism is the remaining half. Grounded against the bridge and storage code at HEAD 2026-08-16 — citations are file:line in `cozorocks/` and `cozo-core/src/` — and against the **vendored RocksDB 8.8.1 headers themselves** (`cozorocks/rocksdb/include/`), so every RocksDB API claim below is read from the tree we compile, not from wiki memory. Stranger test, stated up front: **this spec speaks bytes and instances, never anything else.** Any process embedding several mnestic RocksDB instances — a test harness, a multi-database desktop app, a server hosting many stores — has this problem; nothing here is specific to any one consumer._

> **Anti-overbuild guardrails.** One new opaque bridge type (a refcounted pair of `shared_ptr<Cache>` + `shared_ptr<WriteBufferManager>`), one new open path that accepts it, one small property-read surface, and the CF-loop fix — **nothing else crosses the bridge.** No shared `RateLimiter`, no shared `Env`, no eviction policy, no instance registry, no background threads, and no whole-process memory claim: this is a **soft, RocksDB-managed envelope** — materialization, allocator retention, thread stacks, WAL bookkeeping, and kernel memory remain outside it (the query-side story is the separate [`memory-budget.md`](./memory-budget.md); the two do not overlap and neither implies the other). Single-instance behavior with no handle passed stays **bit-for-bit identical** for every engine-reachable open (the CF-loop fix alters only a null-table-factory crash case reachable solely by direct bridge consumers). Prior art is unusually convergent: Kafka Streams, Flink (FLINK-7289), and TiKV each independently landed on exactly this construction — every surviving many-instances-per-process system either shares one memory envelope or shares one engine._

---

## 1. Why / what this buys

Today every mnestic RocksDB instance owns its own memory, and none of it is observable:

- The programmatic cache branch is dead code from the engine: `DbOpts::default` has `block_cache_size: 0` (`cozorocks/src/bridge/db.rs:57`) and nothing in `cozo-core` ever sets it (verified by search), so `if (opts.block_cache_size > 0) { cache = NewLRUCache(...) }` (`bridge/db.cpp:60-67`) never fires from the engine.
- The options-file path builds a **fresh cache per open**: `LoadOptionsFromFile` (`db.cpp:69-91`) constructs whatever `block_cache={capacity=…}` the file declares, per instance.
- N instances ⇒ N private caches + N private memtable pools, with no shared ceiling and **no way to ask RocksDB what any instance is using** — the bridge exposes zero property reads (verified: no `GetProperty`/`GetIntProperty` anywhere in `cozorocks/` or `cozo-core/src`).

For one embedded instance this is fine and stays the default. For many instances in one process it makes total memory a function of instance count times per-instance tuning — the failure mode every multi-instance RocksDB deployment rediscovers, and the reason Kafka Streams, Flink, and TiKV all converged on one shared cache + one `WriteBufferManager`. RocksDB supports the construction natively: one `WriteBufferManager` is explicitly designed to be shared ("Users can create one write buffer manager object and pass it to all the options of column families or DBs whose memtable size they want to be controlled"), and charging memtable memory into the block cache via dummy entries gives **one byte-denominated envelope** over data blocks + index/filter blocks + memtables (`write_buffer_manager.h:39-48`: "if cache is provided, we'll put dummy entries in the cache and cost the memory allocated to the cache").

## 2. Shipped baseline this builds against (verified 2026-08-16)

| Piece | Where | What transfers |
|---|---|---|
| The cxx bridge surface: `DbOpts` is a plain-data shared struct (`cozorocks/src/bridge/mod.rs:23-44`); the only opaque-handle precedent is `open_db(...) -> SharedPtr<RocksDbBridge>` (`mod.rs:149`), held as `inner: SharedPtr<RocksDbBridge>` (`db.rs:139`) | `cozorocks` | The bridging pattern: the new handle is a second opaque type crossing as `SharedPtr<T>` **function argument** — it cannot be a `DbOpts` field (plain data only) |
| `open_db`'s two cache paths: programmatic (`db.cpp:60-67`, dead from engine) and options-file (`db.cpp:69-91`); the 0.13.0 table-options fix — copy the options **already in effect**, null-checked, then install the cache in both branches (`db.cpp:113-147`) | `bridge/db.cpp` | The install points: a shared cache must land in **both** branches exactly where the 0.13.0 fix installs the per-instance one |
| **The CF loop (S13)**: `for (size_t i = 0; …) { auto* o = loaded_cf_descs[0].options…; o->block_cache = cache; }` — indexes `[0]` inside an `i` loop, no null check (its sibling fix block at :130-135 has one), and the multi-CF descriptor list is discarded anyway (`options = Options(db_opt, loaded_cf_descs[0].options)` at :90; single-CF `TransactionDB::Open` at :160-163) | `db.cpp:81-88` | Fixed in this same bridge change — as *making cache-attach correct for the single-descriptor reality*, not as enabling multi-CF |
| Vendored RocksDB 8.8.1 (`version.h:14-16`): `WriteBufferManager(size_t _buffer_size, std::shared_ptr<Cache> cache = {}, bool allow_stall = false)` with `memory_usage()`, `mutable_memtable_memory_usage()`, `dummy_entries_in_cache_usage()`, `buffer_size()`, `SetBufferSize`, `SetAllowStall` (`write_buffer_manager.h:50-96`); attaches via `DBOptions::write_buffer_manager` (`options.h:946`); `NewLRUCache(capacity, num_shard_bits=-1, strict_capacity_limit=false, high_pri_pool_ratio=0.5, …)` (`cache.h:270-279`, semantics :140-145) | `cozorocks/rocksdb/include/` | The entire RocksDB API surface needed — no vendored-tree change required |
| Property names in the vendored tree: `rocksdb.block-cache-usage` (`db.h:1204-1206`), `rocksdb.cur-size-all-mem-tables` (:1068-1070), `rocksdb.estimate-table-readers-mem` (:1096-1099); C++ accessor precedent `DB *get_base_db()` (`db.h:190-192`); Rust-side method precedent `fn get_db_path(self: &RocksDbBridge)` + status-out-param convention (`mod.rs:148,153`) | headers + bridge | §5's property surface: new `&self` bridge methods over `GetIntProperty` on the base DB |
| Engine open path: `new_cozo_rocksdb(path)` — path is the **only** parameter; reads `<path>/options`, unconditional `use_bloom_filter(true, 9.9, true)` so the 0.13.0 branch fires on every open (`storage/rocks.rs:34,86-106`); `DbInstance::new`'s `options` JSON is documented "ignored for every engine except `tikv`" (`lib.rs:192-194,201`) — and string-based consumers (the Python binding, cozo-bin) can only reach that JSON | `cozo-core` | §4's two entry points, and why the conflict-checked process default must exist for bindings |
| Runtime-knob precedent in the storage struct: the durable-writes `AtomicBool` threaded to `WriteOptions.sync` (`rocks.rs:117-131,147-155`) | `storage/rocks.rs` | The pattern for holding a handle on `RocksDbStorage` |
| Publish order: `cozorocks` = crates.io `mnestic-rocks` 0.1.10 (`cozorocks/Cargo.toml:2-3`), pinned at `cozo-core/Cargo.toml:155`; the release workflow publishes `mnestic-rocks` first, then `mnestic` (`crates-publish.yml:2-4,106-125`) | release machinery | §7's version plan |
| The second RocksDB backend: `storage-new-rocksdb` uses the crates.io `rocksdb` crate directly, bypassing cozorocks (`newrocks.rs:8-11`; `Cargo.toml:46`) | `cozo-core` | Explicitly **out of scope** — the backend is non-default, and a bridge-level mechanism deliberately does not cover it (it would need the `rocksdb` crate's own Cache/WBM bindings; §6) |

## 3. Design — the handle and config

**`RocksMemoryConfig`** (Rust, plain data, serde-able so it can also arrive as JSON via `DbInstance::new`):

```rust
pub struct RocksMemoryConfig {
    pub total_bytes: usize,           // the envelope
    pub memtable_fraction: f64,       // WBM buffer_size = total_bytes * memtable_fraction,
                                      // charged INTO the cache as dummy entries — a carve-out
                                      // of the envelope, not an addition beside it (default 0.25)
    // pinned in v1 (not configurable): strict_capacity_limit=false,
    // high_pri_pool_ratio=RocksDB default (0.5),
    // cache_index_and_filter_blocks_with_high_priority=true, allow_stall=false
}
```

Validation is explicit and total: `total_bytes` nonzero; `memtable_fraction` in (0,1), **strictly below 1** so block/index capacity survives full memtable pressure — incoherent configs are a typed error at handle construction, never a silent clamp.

**`RocksMemoryResources`** (the handle): constructed **once** from a config — `RocksMemoryResources::new(config) -> Result<Self>` — internally an opaque C++ bridge object holding `std::shared_ptr<Cache>` (from `NewLRUCache(total_bytes, -1, /*strict*/ false, /*high_pri*/ default)` — **the cache IS the envelope**) and `std::shared_ptr<WriteBufferManager>` (from `WriteBufferManager(total_bytes * memtable_fraction, cache, /*allow_stall*/ false)` — the cost-to-cache construction: memtable bytes are charged **inside** the cache's capacity as dummy reservations that evict real blocks, so the memtable share is a carve-out of the envelope, never an addition beside it; under full memtable pressure, block/index capacity degrades toward `total_bytes × (1 − memtable_fraction)`). Crossing cxx as `SharedPtr<RocksMemoryResources>` passed to a **new** open function (§7 keeps `open_db` untouched for semver). Clones are refcounted handles to the same live objects; the handle is immutable after construction (`SetBufferSize`/`SetAllowStall` deliberately not exposed in v1 — immutability is what makes the conflict-error semantics of §4 coherent).

**Requirements this design carries**: `strict_capacity_limit=false` initially (pinned index/filter metadata and WBM dummy charges must not become hard read failures); the high-priority pool with `cache_index_and_filter_blocks_with_high_priority=true` on every participating instance's table options; coherence validation as above; and metrics that state which bytes are and are not charged (§5). `allow_stall=false` because the WBM's stall contract gates writers across **all** instances — `write_buffer_manager.h`'s `ShouldStall` reads "all writer threads (including one checking this condition) across all DBs will be stalled" — and the wedge-against-`max_write_buffer_number` failure class is the one rocksdb#4622 exemplifies for ordinary flush pressure; per-instance write-buffer limits remain each instance's backpressure.

## 4. Design — entry points

Two, mirroring how consumers actually reach the engine:

1. **Explicit (primary, Rust hosts)**: `new_cozo_rocksdb_with_memory(path, &RocksMemoryResources)` beside `new_cozo_rocksdb` (`rocks.rs:34`), re-exported like its sibling (`lib.rs:101`). The host builds one handle, opens every instance with a clone. This is the required shape: an explicit, reference-counted resource handle from a canonical config, clones passed to every DB builder.
2. **Process default (bindings and string-configured hosts)**: `DbInstance::new`'s currently-ignored `options` JSON gains a rocksdb key, e.g. `{"shared_memory": {"total_bytes": …, …}}`. First use constructs the process default handle **and stores its canonical config**; any later request with a *different* config gets a **typed conflict error** naming both configs — never first-writer-wins-silently, never a naked `once_flag`. An identical config joins the existing handle. This is the only route reachable from the Python binding and cozo-bin, which pass the options string through verbatim (`cozo-lib-python/src/lib.rs:474-475`; cozo-bin's `args.config`). The `lib.rs:192` doc line ("ignored for every engine except tikv") is updated in the same change.

Non-rocksdb engines ignore the key (sqlite/mem no-op by construction — the JSON already flows past them today); property reads on non-rocksdb backends return None/empty, never error.

**Precedence with the host options file, stated loudly**: when a handle (or process default) is present, the shared cache and WBM **override** any `block_cache={capacity=…}` the `<path>/options` file declares — and the open **logs the override** with both values. The 0.13.0 lesson is binding here: silent option-dropping is precisely the bug class this bridge just fixed; overriding is correct (a live shared object cannot be expressed in an INI file), silence about it is not.

## 5. Design — the property surface

Minimal, typed, per-instance pull plus free handle-level aggregates:

- **Per-instance** (new `&self` bridge methods over `GetIntProperty` on `get_base_db()`, status-out-param convention): `block-cache-usage`, `cur-size-all-mem-tables`, `estimate-table-readers-mem` — the three names verified in the vendored headers — surfaced on the Rust `RocksDb` and up through `RocksDbStorage` as a small struct of `u64`s.
- **Handle-level** (already on the WBM object, zero bridge cost beyond the accessors): `memory_usage()`, `mutable_memtable_memory_usage()`, `dummy_entries_in_cache_usage()`, `buffer_size()`, plus the cache's capacity.
- **The charged/uncharged statement is part of the API docs**: charged into the envelope — data blocks, index/filter blocks (high-pri pool), memtable bytes (as WBM dummy entries); *not* charged — table-reader memory outside the cache, WAL, allocator retention, iterators/pinned blocks beyond cache accounting, and everything non-RocksDB. The docs say "soft envelope" and mean it.
- **No sysop in v1.** A `::mem_stats` script surface is a natural follow-on once the Rust surface exists; it is additive and deliberately deferred (§11 Q3).

## 6. What is deliberately NOT in this spec

Shared `RateLimiter` or `Env` (optional follow-on scope; must not be implied by this handle); eviction/admission policy of any kind (the host decides which instances live — mechanism here, policy there); runtime re-budgeting (`SetBufferSize` exposure — immutable v1); `strict_capacity_limit=true` modes; the `storage-new-rocksdb` backend (non-default; would need the `rocksdb` crate's own Cache/WBM bindings — out of scope, stated in docs); jemalloc/global-allocator anything; any claim of whole-process memory limiting; any instance registry or enumeration surface.

## 7. Bridge versioning and delivery

All bridge changes are additive **at the FFI surface**: a new opaque type + constructor, a new `open_db_with_resources(...)` beside `open_db` — whose *signature* is unchanged (`mod.rs:149`) while its **body is refactored into a shared install helper** both entry points call, and gains the CF-loop fix — plus new property methods. Plan: **`mnestic-rocks` 0.1.10 → 0.1.11**, published first; `cozo-core` pin bump (`Cargo.toml:155`) rides the same engine release; the sibling consumer's path+version dual pin syncs per the standing release checklist. The engine half targets the **next engine minor**. MPL headers preserved on every touched file; new files carry the fork header; no bare `cargo fmt`.

## 8. Test matrix

1. **Bit-parity default**: no handle, no JSON key ⇒ byte-identical behavior for every **engine-reachable** open (options-file and default paths with `block_cache_size=0`: file cache still constructed per open, programmatic branch still dead); the existing storage test suite green unchanged. The CF-loop fix alters only the null-table-factory crash case, reachable solely by direct bridge consumers — pinned in test 6, not here.
2. **Sharing is real**: two instances opened with one handle report the same cache object's usage moving together (write to A, observe `block-cache-usage` from B's cache view / handle aggregate); memtable growth in A moves `dummy_entries_in_cache_usage()`.
3. **Conflict semantics**: process-default JSON — same config twice joins; different config errors with the typed conflict naming both; explicit-handle path never consults the process default.
4. **Validation**: incoherent fractions/zero budgets error at construction with the typed message.
5. **Override loudness**: options file declaring `block_cache` + handle present ⇒ shared cache wins and the override log line fires (asserted via the log capture used by existing bridge tests, or a returned open-report field if logging proves untestable — §11 Q2).
6. **CF-loop fix**: options file with a cache reaches the (single) descriptor's table options with the null-check in place; a descriptor whose table factory lacks `BlockBasedTableOptions` no longer dereferences null (regression test for the defect class the sibling fix block guards).
7. **Property reads**: the three per-instance properties return plausible nonzero values on a warmed instance; non-rocksdb backends return None; property methods on a closed/failed instance error cleanly via the status convention.
8. **Stall posture**: with `allow_stall=false` the WBM stall channel never engages on any instance (`ShouldStall` is false by construction); the unsaturated instance may still see WBM-forced flushes and, transitively, its own per-instance stall triggers — asserted as no-deadlock and continued write progress within a bounded-time write storm on two instances.

## 9. Prior art

Kafka Streams (`RocksDBConfigSetter` shared-cache/WBM recipe), Flink FLINK-7289 (managed-memory shared envelope per slot), TiKV Partitioned Raft KV (one cache across many CF/instances) — three independent convergences on this exact construction; RocksDB's own Write Buffer Manager contract (shared-across-DBs by design; cost-to-cache dummy-entry mechanism; the 7/8-of-buffer mutable-memtable trigger and the over-limit-plus-half-mutable trigger, per `ShouldFlush`; `allow_stall`/`ShouldStall` semantics — all verified against the vendored 8.8.1 headers). The requirements input additionally records: byte-denominated budgets, not instance counts, are the budget; count caps are the backstop.

## 10. Interaction with the query memory budget

None, by design — and the docs must say so. [`memory-budget.md`](./memory-budget.md) bounds **evaluation-time materialization** (temp stores, engine heap); this spec bounds **RocksDB's storage-side memory** (cache + memtables). A deployment wanting a total story arms both. Neither counts the other's bytes; the property surface (§5) and the budget's estimate are documented as disjoint measurements.

## 11. Decisions (signed by the owner 2026-08-16 — all six as proposed)

| # | Question | Proposed |
|---|---|---|
| Q1 | Config shape: total + memtable carve-out fraction vs explicit per-resource byte values | **Total + single carve-out fraction** — the cache is the envelope and the WBM share is charged *inside* it (cost-to-cache nests, not adds), so a two-budget spelling would misrepresent the construction; explicit bytes accepted as an alternative spelling later if pulled |
| Q2 | Override surfacing: log line vs structured open-report | **Log line in v1** (test via capture); structured report only if capture proves brittle |
| Q3 | `::mem_stats` sysop in v1 | **No** — Rust/property surface first; sysop is additive |
| Q4 | high_pri_pool_ratio configurable | **Pinned to RocksDB default (0.5)** in v1; the requirements input names the pool's existence, not a ratio |
| Q5 | Bridge bump 0.1.11 (additive) vs 0.2.0 | **0.1.11** — `open_db` untouched, all additions; revisit only if implementation forces a signature change |
| Q6 | Process-default JSON key name and schema | `shared_memory` as sketched — bikeshed at sign-off |
