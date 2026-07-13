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

**New in 0.12.2: a float in a validity position was silently read — and written —
one million times too small, landing in 1970.**

Validity timestamps are integer *microseconds* since the epoch. `now()` and
`parse_timestamp()` return float *seconds*. The engine coerced one into the other
without a word, so a `:put` of `[parse_timestamp(…), true]` into a `Validity`
column **succeeded** and stamped the row at 1970 — a row that reads back correctly
on an ordinary query, with the damage visible only under time travel. On the read
side, `@ parse_timestamp(…)` returned **zero rows and no error**.

This is especially easy to hit from Python, because a Python `float` reaches the
engine as a float: passing `time.time() * 1_000_000` as a bound parameter worked
only by luck (whenever the product happened to land on a whole number) and is now
a clear error. Pass an `int`.

**Upgrade action — and note carefully *where* it bites.** The schema still compiles; it is
the next **write** that now fails. The idiom `Validity default [floor(now()), true]` — and any
spelling that yields a *whole-numbered* float, so `floor(now())`, `round(now())`, or
`parse_timestamp(...)` on a whole second — has been silently writing 1970 into your valid-time
axis. It now errors on write. (Bare `[now(), true]` already errored before this release, but
only by luck: `now()` returns a *fractional* float, and the coercion only ever accepted
whole-numbered ones.) Write instead:
```
last_seen: Validity default [to_int(now() * 1000000), true]
```

An integer in *seconds* (`@ 1704067200`) is still accepted and still silently
returns nothing — valid time is an abstract logical clock (the tutorial queries
`@ 2019`), so no magnitude check can tell a wrong unit from a legitimate small
value. Use integer microseconds, or the string forms (`@ '2024-06-01'`).

For idiomatic LangChain / LlamaIndex usage, install the integration packages
(`langchain-mnestic`, `llama-index-vector-stores-mnestic`).

The query language (CozoScript / Datalog) and engine semantics follow CozoDB; see
the [upstream documentation](https://docs.cozodb.org/) and the
[fork changelog](https://github.com/shuruheel/mnestic/blob/main/CHANGELOG-FORK.md).

## License

Mozilla Public License 2.0. Original work © 2022 The Cozo Project Authors; fork
modifications © 2026 Shan Rizvi.
