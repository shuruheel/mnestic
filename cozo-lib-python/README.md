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

New here? The [quickstart](https://github.com/shuruheel/mnestic/blob/main/docs/guides/quickstart.md)
walks a persistent memory store through indexing, hybrid retrieval, updates,
backup, reopen and restore in one runnable script. Coming from Cypher, see
[Cypher to CozoScript](https://github.com/shuruheel/mnestic/blob/main/docs/guides/cypher-to-cozoscript.md).

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
The source distribution ships `compact`, RDF boundary I/O, and host-controlled
Parquet/Arrow import, but no RocksDB sources, so the persistent engine remains
wheel-only.

Published wheels and source builds expose atomic local-file copy-in through
`import_columnar_file`. The target relation must already exist; the format is
explicit and the call releases Python's GIL while decoding and committing:

```python
report = db.import_columnar_file(
    "events",
    "events.parquet",
    format="parquet",  # or arrow_ipc_file / arrow_ipc_stream
    columns={"event_id": "id"},
    batch_rows=8192,
    max_rows=1_000_000,
)
```

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

## New in 0.19.0

This release adds governed native Rust transactions and repairs transactional
FTS statistics and HNSW mutation connectivity.

- **Governed transactions (native Rust):** `db.governed_transaction(write, options)`
  returns a serialized client and host-owned worker. One deadline, bounded idle
  waiting, inherited statement memory limits and warning flushes apply across the
  transaction; every query error rolls it back. Hosts retain admission until the
  worker exits. Cancellation is cooperative and can race a commit, so hosts must
  reconcile uncertain outcomes. Python and other bindings do not expose this API.
- **BREAKING (results):** FTS scores/top-k now use statistics from the querying
  transaction's posting snapshot and pending writes. Aborts and failed commits no
  longer leak speculative totals. Recheck score thresholds and ranking fixtures;
  there is no legacy-statistics switch. Overlapping snapshots may rescan postings.
- **Rebuild pre-0.19 HNSW indexes with `::reindex <relation>`** after a backup, then
  validate search against exact base-row/vector queries. New mutations preserve
  directed paths; existing disconnections and missing legacy reverse records need
  that explicit rebuild. Writes, including fresh insertions, can fail with
  `hnsw::repair_limit` when local degree/work caps cannot preserve paths. Abort and
  open a new transaction after failure; later statements cannot make it committable.
- `::warnings` stays responsive during active memory/SQLite transactions.
  `parser.string_decoding_changed` is retired as promised; decoding is unchanged.
  The permanent 0.18 migration guidance still applies to legacy scripts.

Storage encoding and `mnestic-rocks` 0.1.12 are unchanged. The local repair caps
are not total memory or latency guarantees; filtered-ANN eligibility and its
approximate-recall limitations remain. Schedule maintenance time for index rebuilds
and evaluate write costs on your workload. A focused comparison between two 0.19
builds measured write p95 increases of 3.1% on RocksDB and 11.8% on SQLite, with
seed-sensitive tails; a separate one-seed window also showed roughly 16% increases
in selected exact scans. These are not 0.18-to-0.19 measurements or a bound on
upgrade cost. No general speedup or exact ANN recall is promised.

Full upgrade guidance is in the [fork changelog](https://github.com/shuruheel/mnestic/blob/main/CHANGELOG-FORK.md).

For idiomatic LangChain / LlamaIndex usage, install the integration packages
(`langchain-mnestic`, `llama-index-vector-stores-mnestic`).

The query language (CozoScript / Datalog) and engine semantics follow CozoDB; see
the [upstream documentation](https://docs.cozodb.org/) and the
[fork changelog](https://github.com/shuruheel/mnestic/blob/main/CHANGELOG-FORK.md).

## License

Mozilla Public License 2.0. Original work © 2022 The Cozo Project Authors; fork
modifications © 2026 Shan Rizvi.
