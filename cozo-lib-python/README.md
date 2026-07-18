# mnestic (Python)

Embedded **graph + vector + full-text** database with **Datalog** queries — a
maintained fork of [CozoDB](https://github.com/cozodb/cozo), tuned as a substrate
for **agentic memory**. This package is the in-process Python binding (no server
required).

> mnestic is **not** the official CozoDB and is not affiliated with or endorsed by
> its original authors. All credit for the original design belongs to Ziyang Hu and
> the Cozo Project Authors. See the
> [fork repository](https://github.com/shuruheel/mnestic) for provenance and
> licensing.

```bash
pip install mnestic
```

```python
from mnestic import CozoDbPy

db = CozoDbPy("mem", "", "{}")  # engines: "mem", "sqlite" (file path), "rocksdb" (dir path)
db.run_script("?[x] <- [[1],[2],[3]]", {}, False)

# One-call hybrid retrieval (HNSW + full-text fused with Reciprocal Rank Fusion),
# over a relation that has an HNSW index and an FTS index:
hits = db.hybrid_search({
    "relation": "docs",
    "vector_index": "vec", "query_vector": [0.1, 0.9], "vector_k": 5,
    "fts_index": "fts", "query_text": "vector search", "fts_k": 5,
})
# -> {"headers": ["id", "score"], "rows": [["d3", 0.033], ...], "next": None}

# Pass "detailed": True for per-leg contributions — one row per (item, leg)
# with the within-leg rank the fusion used and the leg's raw score:
# headers ["id","score","list_id","leg_rank","leg_score"]
```

The `"rocksdb"` persistent backend now ships in the published wheel —
`CozoDbPy("rocksdb", "./my.db", "{}")` works straight from `pip install mnestic`.
The source distribution stays SQLite/`compact`-only, so the persistent engine is
wheel-only.

**Upgrade note (0.10.6):** a persistent database whose relation catalogs were
last written by a build older than 0.10.0 could fail to open with `Cannot
deserialize relation metadata from bytes` after upgrading to 0.10.0–0.10.5.
0.10.6 fixes this — legacy catalogs open again with no migration, so upgrade to
0.10.6 if you carry a pre-0.10.0 database.

`run_script` takes an optional `timeout=` — a per-query wall-clock budget in
seconds; on expiry the query raises an `eval::timeout` error.
`db.set_default_query_timeout(secs)` sets a Db-wide default and
`db.default_query_timeout()` reads it back; the effective budget for a query is
the minimum of that default and any per-call `timeout`.

## New in 0.13.0

0.13.0 is a combined correctness-and-capability release. The Python-facing
highlights:

- **The published wheel now honours RocksDB table options.** A block cache,
  block size, and index/filter caching configured through an `options` file
  were silently discarded on every open, so `CozoDbPy("rocksdb", ...)` ran with
  an 8 MB default cache and 4 KB blocks no matter what the file asked for. Any
  read-path benchmark taken against a RocksDB store before this release measured
  a slower engine than mnestic actually is. Fixed in the bundled `mnestic-rocks`
  0.1.10; the SQLite-only source distribution was never affected.

- **Datetime standard library (`dt_*`), reachable from `run_script`.** Component
  extractors (`dt_year` … `dt_dow`), `dt_trunc`, calendar-aware `dt_add` /
  `dt_diff`, strftime `dt_format`, and `dt_to_validity` — the typed bridge from
  float Unix *seconds* to a `Validity`'s integer microseconds. `@` and `:as_of`
  now accept a `Validity`-typed expression
  (`@ dt_to_validity(parse_timestamp('2024-01-01'))`), which together with
  0.12.2's float rejection closes the seconds-vs-microseconds trap that is
  especially easy to hit from Python. The new `dt_*` names are reserved against
  `register_custom_aggr`.

- **Better parse errors.** A failed `run_script` now points its caret at the
  deepest position the parser reached and adds a `help:` line naming the literal
  tokens that would have been accepted (`expected one of: :=, <-, <~`) — the
  improved text flows straight through the wheel. Index-search diagnostics now
  name the index kind that actually failed (`fts_query_required`, not the old
  `hnsw_query_required`).

- **`hybrid_search`: budgeted graph expansion and optional legs.** A `graph_legs`
  entry gains optional keys (`max_nodes`, `max_cost`, `weight_col`, `graph`,
  `seed_from_legs`, `gate_relation` / `gate_cols` / `admit`); setting `max_nodes`
  runs the leg as cheapest-first weighted expansion under a distinct-node budget.
  `vector_index` and `fts_index` are now optional, so you can fuse any non-empty
  subset of {vector, FTS, graph} legs. Existing dicts parse unchanged, but an
  unknown key in a `graph_legs` entry is now rejected loudly, so a typo like
  `max_hop` can no longer silently run the leg with defaults.

- **The `!=` factorized-count rewrite is restored, default OFF.** Enable the
  Db-wide switch with `db.set_query_factorization(True)` (read it back with
  `db.query_factorization()`); it rewrites an eligible `count()`-over-join
  carrying an inequality via inclusion–exclusion, sound behind a stored-column
  type gate. Measured ~140× on LSQB q6. It stays off by default until a nightly
  soak clears the default-on flip.

- **Corrupt data raises instead of crashing the interpreter.** A corrupt value
  blob in a stored relation used to raise `PanicException` — a `BaseException`
  subclass that `except Exception:` does **not** catch. It is now an ordinary
  `eval::corrupt_value_blob` query error naming the key: run `::repair_corrupt
  <relation>` to drop the unreadable rows, then `::reindex <relation>` if that
  relation carries an HNSW/FTS/LSH index. HNSW indexes built by any release
  through 0.12.2 can also carry stale nodes/edges from null-vector and
  pre-existing-row bugs — rebuild once per affected relation with `::reindex
  <relation>`. Your rows are untouched; `::reindex` rebuilds index relations only.

See the [fork changelog](https://github.com/shuruheel/mnestic/blob/main/CHANGELOG-FORK.md)
for the full per-case upgrade guidance — pre-1970 timestamps, `restore_backup`
relation-id reconciliation, and the hybrid-leg ranking changes (fusion legs now
require distinct labels, and a graph leg no longer re-scores its own seeds).

For idiomatic LangChain / LlamaIndex usage, install the integration packages
(`langchain-mnestic`, `llama-index-vector-stores-mnestic`).

The query language (CozoScript / Datalog) and engine semantics follow CozoDB; see
the [upstream documentation](https://docs.cozodb.org/) and the
[fork changelog](https://github.com/shuruheel/mnestic/blob/main/CHANGELOG-FORK.md).

## License

Mozilla Public License 2.0. Original work © 2022 The Cozo Project Authors; fork
modifications © 2026 Shan Rizvi.
