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

### Upgrading to 0.13.0

**Pre-1970 timestamps.** mnestic now accepts a date before the Unix epoch
wherever it accepts a timestamp string. It used to panic — and on the `mem` and
`sqlite` backends the panic happened while the store's write guard was held,
poisoning the lock and killing the database. Pre-epoch writes that previously
panicked were never committed, so **no stored data is affected; simply re-run
them.** No action is required on upgrade.

**HNSW indexes may need one rebuild.** Three separate bugs left stale data in
HNSW indexes built by any release through 0.12.2:

- A `:put` or `update` that set a row's vector column to `null` (or shortened a
  list-of-vectors column) left the row's old graph nodes behind. This is not a
  stale-result bug — the search reads a node's vector back from the base row, so
  a single stranded node makes **every** vector query on that relation fail with
  `Cannot interpret null as vector`, or panic.
- `::hnsw create` over a relation that **already had rows** wrote one-directional
  edges. A later `:rm` — or a re-`:put` that changes a vector, i.e. re-embedding —
  can then strand an orphan edge, and a search may fail with `Cannot find
  compound key for HNSW`.
- An all-zero vector (what a failed or absent embedding produces) made cosine
  distance `NaN`, which wedged the search heap and silently degraded results.

Upgrading alone restores **correct query results** for the zero-vector case. For
the other two, the stale rows are on disk. Rebuild once, per affected relation:

```text
::reindex <relation>
```

Your rows are untouched and nothing is deleted; `::reindex` rebuilds index
relations only. It is safe to run on an index that is already failing. An index
created on an **empty** relation and populated only with non-null vectors by
`:put`, without later nulling or shortening a vector field, is unaffected by the
two on-disk bugs.

**`::repair_corrupt` does not fix any of the above** and will report `removed: 0`.

**Corrupt value blobs are now an error, not a panic.** A corrupt value in a
stored relation used to panic the process — through the Python wheel that was a
`PanicException`, a `BaseException` subclass that `except Exception:` does not
catch. It is now an ordinary query error (`eval::corrupt_value_blob`, naming the
key). If you hit it, run `::repair_corrupt <relation>` to drop the unreadable
rows, **then** `::reindex <relation>` if that relation carries an HNSW/FTS/LSH
index — repair cannot evict the dead row's index postings, because it cannot
decode the row to know what they were.

**`restore_backup` could mint colliding relation ids.** In any release through
0.12.2, a relation created **after** a `restore_backup` into a fresh store could
be given an id a restored relation already owned — silently sharing one
keyspace, so reads of either returned both. **On upgrade, opening the store stops
any further collisions with no action on your part**, and logs an error naming
any relations that are already entangled.

**If that error fires, the entangled rows cannot be separated.** The store never
recorded which relation wrote which row. **Do not run `::repair_corrupt` on them
— it deletes the narrower relation's rows.** Recover by restoring the
**original** backup into a **fresh** store with this build. (If your only backup
was taken *from* the already-damaged store, it carries the entanglement.) Stores
where `restore_backup` was never called, or where no relation was created
afterwards, are unaffected.

**`hybrid_search`: every fusion leg now needs a distinct label.** Two graph legs
sharing a label were never fused as two lists — reciprocal-rank fusion groups by
the label, so the second was silently merged into the first. In Python, `label`
is optional and defaults to `"graph"`, so two defaulted graph legs collided by
construction. This now **errors at build time** instead of returning a wrong
ranking. Give each leg its own label (`"semantic"` and `"text"` are reserved). A
single graph leg is unaffected.

**Graph legs no longer re-score their own seeds.** A seed reachable from itself
— guaranteed at hop 2 whenever `undirected: true`, and possible at hop 1 via a
self-loop or an edge from a second seed — was re-entering its own ranked list.
Seeds that legitimately match the vector or keyword query are still returned
and still rank where those legs put them; only the spurious graph-leg
contribution is gone. Rankings will shift. No migration.

**One query that used to return zero rows now raises.** A query reading a stored
relation by a **fully-bound key**, with a filter that errors at evaluation time,
silently returned `Ok([])`; it now raises that error. (The same query with an
unbound key already raised — the engine was giving two different answers to the
same logical query depending on the plan it chose.)

For idiomatic LangChain / LlamaIndex usage, install the integration packages
(`langchain-mnestic`, `llama-index-vector-stores-mnestic`).

The query language (CozoScript / Datalog) and engine semantics follow CozoDB; see
the [upstream documentation](https://docs.cozodb.org/) and the
[fork changelog](https://github.com/shuruheel/mnestic/blob/main/CHANGELOG-FORK.md).

## License

Mozilla Public License 2.0. Original work © 2022 The Cozo Project Authors; fork
modifications © 2026 Shan Rizvi.
