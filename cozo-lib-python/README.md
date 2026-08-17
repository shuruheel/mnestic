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

## RDF at the boundary — and what it means for the wheel's trust posture

The wheel ships the `rdf-io` engine feature: `RdfReader` reads Turtle,
N-Triples, N-Quads and TriG into a fixed 6-column relational shape
(`subject, predicate, object, graph, language_tag, datatype`) straight from
CozoScript, and IRI helper functions (`iri_valid`, `iri_resolve`,
`curie_expand`, `curie_compact`) handle boundary identity:

```python
db.run_script("""
    triples[s, p, o, g, lang, dt] <~ RdfReader(url: 'file://./data.ttl')
    ?[s, o] := triples[s, 'http://xmlns.com/foaf/0.1/knows', o, _, _, _]
""", {}, True)
```

**Read this before running untrusted CozoScript.** `rdf-io` implies the
engine's `data-import` trust gate, so this wheel — deliberately reversing the
0.14.0 posture — again registers script-controlled readers: `RdfReader`,
`CsvReader` and `JsonReader` can read any file the process can read, and
because the wheel also compiles HTTP support (`requests`), a script can fetch
non-`file://` URLs. Only run CozoScript from callers you trust with those
capabilities, or build the binding from source without the `rdf-io` feature
for a locked-down deployment.

## New in 0.14.0

The Python-facing highlights of this security-first minor release:

- **Safer defaults.** The wheel no longer registers CozoScript's file/network
  `CsvReader` and `JsonReader` utilities. Stored-relation ACLs now cover
  indexes, negation, fixed rules, exports, history, and cached graph
  projections; malformed vector payloads fail before unsafe decoding.
- **Exact-phrase FTS and matched context.** Quoted multi-word queries require
  adjacency and order. FTS atoms can bind the matching byte offsets with
  `bind_spans`, and `snippet(text, spans, window)` formats a window without
  re-tokenizing the document.
- **Durability is explicit.** `db.set_durable_writes(True)` requests
  fsync-on-commit and returns whether the selected backend honors the knob.
  RocksDB does; SQLite is already durable under its default
  `synchronous=FULL` policy and reports `False`.
- **Warnings are queryable.** `::warnings` returns stable warning codes with a
  message and actionable hint; `::warnings clear` empties the per-database
  buffer.
- **Hardened framework adapters.** `langchain-mnestic` 0.2.1 and
  `llama-index-vector-stores-mnestic` 0.2.1 validate all identifiers and numeric
  HNSW settings before constructing CozoScript.

Migration highlights: indirect reads of `Hidden` relations now fail, and
quoted multi-word FTS queries now mean exact phrases. Drop the quotes to retain
the previous AND-of-terms behavior.

See the [fork changelog](https://github.com/shuruheel/mnestic/blob/main/CHANGELOG-FORK.md)
for the full accounting, and for 0.13.0's upgrade guidance if you are coming
from an earlier release (`::reindex` for HNSW/FTS indexes, pre-1970 timestamps,
and the hybrid-leg ranking changes).

For idiomatic LangChain / LlamaIndex usage, install the integration packages
(`langchain-mnestic`, `llama-index-vector-stores-mnestic`).

The query language (CozoScript / Datalog) and engine semantics follow CozoDB; see
the [upstream documentation](https://docs.cozodb.org/) and the
[fork changelog](https://github.com/shuruheel/mnestic/blob/main/CHANGELOG-FORK.md).

## License

Mozilla Public License 2.0. Original work © 2022 The Cozo Project Authors; fork
modifications © 2026 Shan Rizvi.
