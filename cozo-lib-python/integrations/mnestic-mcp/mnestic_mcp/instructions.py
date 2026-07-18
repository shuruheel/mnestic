"""Server instructions surfaced to the model at `initialize`. Structure adapted
from mindgraph-mcp's keyword-first retrieval ladder — prose only, no ontology."""

INSTRUCTIONS = """\
You have a persistent local memory (mnestic-mcp). Everything stays on this machine.

## #1 RULE: go straight to the user's topic
One `search` call with 1-3 discriminating keyword terms from their message. Nothing
else first. There is no session to open and no context to pre-load.
  YES: search({query: "Kissinger NATO"})
  NO:  search({query: "what did the user previously say about..."})

## Search ladder — keyword first, semantic as fallback
The default mode "auto" tries keywords (BM25) first and falls back to hybrid.
Escalate yourself when results are thin:
1. fewer, more discriminating terms (proper nouns, technical terms; drop filler)
2. synonyms
3. mode "semantic" (natural-language queries are fine there)
4. mode "hybrid" — BM25 + vector + graph proximity fused in one call.
Very common words are stopworded on the keyword leg; prefer distinctive terms.

## Orient, then expand
`search` returns compact results. To explore around a strong hit, call
`find_related(id)` (budget-bounded graph walk over links). For "what have I
told you lately", use `list_recent`.

## When to WRITE
Store durable facts, preferences, decisions, and outcomes as they occur — one
fact per memory, a sentence or two, with meta tags like {"topic": ..., "kind": ...}.
Use `link(src, dst, rel)` when two memories belong together (rel names a
mechanism: "relates_to", "follows", "contradicts"). Use `update` for
corrections — never store a duplicate. Don't narrate tool usage.

## The two tools no other memory server has
- search(explain=true): per-leg attribution — exactly how much the keyword,
  vector, and graph legs each contributed to every result. Use when the user
  asks "why did you recall that?" or when retrieval needs debugging.
- recall_as_of(t): the memory store as it existed at time t — updates and
  deletes are never destructive. Use for "what did you know before X?".
"""
