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

**New in 0.12.1: a correctness release — with one action to take if you use
full-text search.** Six bugs inherited from upstream Cozo are fixed; none is a
regression the fork introduced. The one that needs something from you: **full-text
postings leaked whenever a row was updated in place** on a relation carrying
*only* an FTS index. Deletion of the old postings was gated on the relation also
having a plain secondary index, so a `:put` over an existing key never removed
them — terms the document no longer contained kept matching it, the index grew
without bound, and BM25 scores drifted (a measured **55% score error** on a
two-document corpus). This affects every release through 0.12.0.

**The fix stops new leakage but cannot evict postings already written.** If you
have an FTS-only relation that has ever been updated in place, its index is
affected today and upgrading alone will not repair it — rebuild it once with the
new `::reindex`:

```python
db.run_script("::reindex docs", {}, False)
```

`::reindex` rebuilds a relation's HNSW / FTS / LSH indexes in place, in one write
transaction, from the index configuration the database already stores. It is also
the repair path after `import_relations` or a backup restore — neither maintains
these indexes (that is what makes bulk loading fast), and both now warn and point
at `::reindex` rather than leaving restored rows silently invisible to search.

Also fixed, and visible from this binding: a **failed commit raised no exception**.
`CozoDbMulTx.commit()` (from `db.multi_transact(True)`) returned normally even when
the underlying commit had errored, so a caller believed its data was durable when
it was not; change callbacks fired for those failed commits too. Both now behave.

For idiomatic LangChain / LlamaIndex usage, install the integration packages
(`langchain-mnestic`, `llama-index-vector-stores-mnestic`).

The query language (CozoScript / Datalog) and engine semantics follow CozoDB; see
the [upstream documentation](https://docs.cozodb.org/) and the
[fork changelog](https://github.com/shuruheel/mnestic/blob/main/CHANGELOG-FORK.md).

## License

Mozilla Public License 2.0. Original work © 2022 The Cozo Project Authors; fork
modifications © 2026 Shan Rizvi.
