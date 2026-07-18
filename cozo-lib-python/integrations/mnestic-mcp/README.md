# mnestic-mcp

A local **memory MCP server** backed by
[mnestic](https://github.com/shuruheel/mnestic) — an embedded graph + vector +
full-text database (a maintained fork of CozoDB). One process, one file, fully
local: your agent's memory never leaves the machine.

> mnestic is a maintained fork of [CozoDB](https://github.com/cozodb/cozo); it is not the official CozoDB. Original design credit belongs to Ziyang Hu and the Cozo Project Authors.

## Install

Add to any MCP client (Claude Desktop, Claude Code, Cursor, ...):

```json
{ "command": "uvx", "args": ["mnestic-mcp"] }
```

That's it. The database lives in your platform data dir (override with
`--db PATH` or `MNESTIC_MCP_DB`); embeddings run locally via
[fastembed](https://github.com/qdrant/fastembed) (default
`BAAI/bge-small-en-v1.5`, a one-time ~67 MB download that happens in the
background — keyword search works immediately, before the model arrives).
Pre-provision with `uvx mnestic-mcp --download-model`.

## Tools

`store`, `store_batch`, `search`, `find_related`, `list_recent`, `update`,
`delete`, `link`, `recall_as_of`, `stats` — plus the two things no other local
memory server has:

- **`search(explain=true)`** — per-leg attribution: exactly how much the
  keyword (BM25), vector (HNSW), and graph-proximity legs each contributed to
  every result, straight from the engine's fused three-way retrieval. Ask
  your agent *"why did you recall that?"* and get a real answer.
- **`recall_as_of(t)`** — time travel. Updates and deletes are never
  destructive (valid-time history in the engine's bitemporal storage):
  *"what did you know about this before last Tuesday?"* just works.

`search` is keyword-first with an automatic hybrid fallback; `link` builds a
typed, weighted memory graph that both `find_related` (budget-bounded
traversal) and the hybrid graph leg exploit.

## Configuration

| Flag / env | Default | Meaning |
|---|---|---|
| `--db` / `MNESTIC_MCP_DB` | per-OS data dir | database file |
| `--engine` / `MNESTIC_MCP_ENGINE` | `sqlite` | `sqlite` \| `rocksdb` (single-instance) \| `mem` |
| `--model` / `MNESTIC_MCP_MODEL` | `BAAI/bge-small-en-v1.5` | any fastembed model |
| `MNESTIC_MCP_CACHE` | data dir `/models` | model cache location |

A database pins the embedding model it was built with — mixing embedding
spaces is refused with an actionable error, never a silent quality collapse.

## Notes

- Requires Python ≥ 3.10. Storage engines ship in the `mnestic` wheel
  (`sqlite`/`mem` also in the sdist; `rocksdb` wheel-only).
- Several MCP clients may share one sqlite database; writes serialize on the
  file. `rocksdb` is strictly single-instance.
- Deleting memories under heavy churn can slowly degrade vector recall
  (a known engine-side HNSW defect, tracked upstream); updates are safe.

## License

Mozilla Public License 2.0.
