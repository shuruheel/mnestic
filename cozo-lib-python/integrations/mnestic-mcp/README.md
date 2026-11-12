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

`store_memory`, `store_batch`, `search`, `find_related`, `list_recent`,
`update`, `delete`, `link`, `recall_as_of`, `recall_believed`,
`memory_timeline`, `continue_result`, `stats`. Three of them are unusual for
a local memory server:

- **`search(explain=true)`** — per-leg attribution: exactly how much the
  keyword (BM25), vector (HNSW), and graph-proximity legs each contributed to
  every result, straight from the engine's fused three-way retrieval. Ask
  your agent *"why did you recall that?"* and get a real answer.
- **`recall_believed(valid_at, known_at)`** (0.3.0) — two-axis recall. *What
  was true* at `valid_at`, *according to what had been recorded* by
  `known_at`. Each answer carries `valid_from`, `recorded_at` and `source`.
  Writes take an optional `source` (the evidence) and `valid_from` (for
  backdated corrections). `recorded_at` is the engine's commit stamp
  (`TxTime`), which a caller cannot supply, so it is an audit answer rather
  than an application claim.
- **`memory_timeline(id)`** (0.3.0) — every assertion and retraction for one
  memory, with when it was valid, when it was recorded and on what evidence.

Example: the user said they live in NYC (onboarding form, valid since
January). Weeks later a moving email shows they moved to SF on February 15,
and the agent records a backdated correction. Then
`recall_believed(valid_at="2026-02-20")` answers SF (source: moving email);
the same call with `known_at` set before the email answers NYC (source:
onboarding form); `memory_timeline("home")` shows both beliefs and when each
was recorded. `tests/test_bitemporal.py` runs exactly this scenario.

`recall_as_of(t)` remains: the store's contents as of write time `t`.
Updates and deletes are never destructive. Stores created before 0.3.0 gain
the two-axis record when opened; their earlier history stays available
through `recall_as_of`.

`search` is keyword-first with an automatic hybrid fallback; `link` builds a
typed, weighted memory graph that both `find_related` (budget-bounded
traversal) and the hybrid graph leg exploit.

**Byte-budgeted output** (0.2.0): every wide-result tool (`search`,
`find_related`, `list_recent`, `recall_as_of`, `recall_believed`,
`memory_timeline`) takes `max_bytes` (default
20 000). An over-budget result is cut **at item boundaries** — item content is
never trimmed — and returns `truncated: true`, a `remaining` count, and a
single-use `continuation` token; **`continue_result(token)`** serves the next
slice under the same budget. A wide result can no longer blow your agent's
context with no way back. (Tokens don't survive a server restart; the error
says so and tells you to re-run the query.)

`max_bytes` counts UTF-8 bytes of the complete response exactly as it goes on
the wire (FastMCP's indented JSON text block), truncation fields included.
Every response fits, with one exception: a single item larger than the budget
is served alone with `oversized_item: true` and its real `response_bytes`,
since trimming would break prefix semantics and an empty page would loop.
(0.2.0 counted compact-JSON characters, so non-ASCII results could exceed the
budget several times over; fixed in 0.3.0.)

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
