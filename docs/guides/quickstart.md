# Quickstart: a persistent agent memory in Python

```bash
pip install mnestic
python cozo-lib-python/examples/quickstart.py
```

[`quickstart.py`](../../cozo-lib-python/examples/quickstart.py) builds a small
memory store and takes it through the lifecycle a long-lived agent memory
needs. Each step asserts its result, and `scripts/check-quickstart.sh` runs
the script in a fresh virtualenv for every release.

| Step | What it shows |
|---|---|
| 1. Open | `CozoDbPy("rocksdb", dir, "{}")`; `set_durable_writes(True)` adds an fsync per commit (SQLite is always fully synchronous) |
| 2. Schema | a keyed relation, then HNSW and full-text indexes created **before** data, so every write maintains them transactionally |
| 3. Write | a parameterized batch (`<- $rows :put`) in one transaction |
| 4. Retrieve | a keyed lookup; `hybrid_search` with `detailed: True` for per-leg attribution; exact filtered vector search |
| 5. Update and delete | catching a rejected write by its error `code`; the write rolls back and the old row stays readable |
| 6. Back up | `db.backup(path)` to one file while the store is open |
| 7. Reopen | a new handle on the same directory returns identical rows and search results |
| 8. Restore | `restore(path)` into an empty store brings back rows and indexes |

Expected output:

```
hybrid top-3: ['m1', 'm4', 'm3'] | legs for m1: ['semantic', 'text']
structured error code: query::relation_not_found
ok: 4 memories survived update, delete, reopen and restore
```

The embedder is a hashed bag of words, so the script needs no model download.
Replace `embed()` with a real model and change `DIM` to match it.

Coming from Cypher? See [Cypher to CozoScript](cypher-to-cozoscript.md).

## Notes for coding agents

Rules that prevent the common failures when generating CozoScript:

- **Parameters only.** Pass values as `$name` with a params dict. Never format
  values into the script.
- **Bind keys in braces.** `*rel{id: $id, text}` is a key seek;
  `*rel{id, text}, id == $id` scans the relation. The planner has no cost model.
- **Aggregates in the head.** `?[k, count(x)] := ...`, never `c = count(x)` in
  a body. Rules that share a head (unions) cannot aggregate; aggregate in a
  helper rule.
- **`:put` replaces the whole row.** Omitted columns are reset to defaults. Use
  `:update` to change some columns.
- **Results are sets.** Duplicate rows collapse; include a key if you need
  every row.
- **Indexes before data.** `::hnsw create` / `::fts create` on an empty
  relation, then load. `import_relations` and `batch_put` skip search indexes.
- **Errors are structured.** `exc.args[0]` is a dict; branch on `["code"]`.
  `hnsw::repair_limit` means the vector index could not repair its
  neighbourhood within its work bounds. The write was rolled back, and retrying
  it unchanged will not succeed. See
  [the repair contract](../specs/hnsw-mutation-repair.md).
- **Selective filters: score exactly.** For top-k among a rare subset, compute
  `cos_dist` over the eligible rows rather than filtering an HNSW result.
- **One script, one transaction.** Chain `{...} {...}` blocks for multi-step
  writes that must commit together.
