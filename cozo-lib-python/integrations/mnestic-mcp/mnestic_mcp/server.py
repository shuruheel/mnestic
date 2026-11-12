"""FastMCP wiring: 13 thin tools delegating to `MemoryStore`. Tool names speak
mechanism (store/search/link/recall), never a cognitive ontology."""

from __future__ import annotations

from typing import Any, List, Optional

from mcp.server.fastmcp import FastMCP

from mnestic_mcp import budget
from mnestic_mcp.instructions import INSTRUCTIONS
from mnestic_mcp.memory import MemoryStore


def build_server(store: MemoryStore) -> FastMCP:
    mcp = FastMCP(name="mnestic-memory", instructions=INSTRUCTIONS)

    @mcp.tool()
    def store_memory(
        text: str,
        meta: Optional[dict] = None,
        id: Optional[str] = None,
        source: Optional[str] = None,
        valid_from: Optional[str] = None,
    ) -> dict:
        """Store one memory (a durable fact, preference, decision, or outcome).
        `source` names the evidence (a message, document, URL). `valid_from`
        (ISO-8601) says when the fact became true, if not now. Returns its id."""
        return store.store(text, meta=meta, id=id, source=source, valid_from=valid_from)

    @mcp.tool()
    def store_batch(items: List[dict]) -> dict:
        """Store many memories atomically. Each item:
        {text, meta?, id?, source?, valid_from?}."""
        return store.store_batch(items)

    @mcp.tool()
    def search(
        query: str,
        k: int = 8,
        mode: str = "auto",
        explain: bool = False,
        expand_graph: bool = True,
        max_bytes: int = budget.DEFAULT_MAX_BYTES,
    ) -> dict:
        """Search memories. mode: auto (keyword-first, hybrid fallback),
        keyword (BM25), semantic (vector), hybrid (BM25+vector+graph fused).
        explain=true returns per-leg attribution for every result. Output is
        byte-budgeted: an over-budget result is cut at item boundaries and
        carries a `continuation` token for continue_result."""
        out = store.search(query, k=k, mode=mode, explain=explain, expand_graph=expand_graph)
        return budget.budget_slice(out, "results", max_bytes)

    @mcp.tool()
    def find_related(
        id: str,
        max_nodes: int = 25,
        max_depth: int = 3,
        weighted: bool = False,
        max_bytes: int = budget.DEFAULT_MAX_BYTES,
    ) -> dict:
        """Walk the link graph outward from a memory (budget-bounded traversal);
        returns related memories with cost/depth/parent. Byte-budgeted output
        (see search)."""
        out = store.find_related(id, max_nodes=max_nodes, max_depth=max_depth, weighted=weighted)
        return budget.budget_slice(out, "related", max_bytes)

    @mcp.tool()
    def list_recent(n: int = 10, max_bytes: int = budget.DEFAULT_MAX_BYTES) -> dict:
        """The most recently stored or updated memories. Byte-budgeted output
        (see search)."""
        return budget.budget_slice(store.list_recent(n), "memories", max_bytes)

    @mcp.tool()
    def continue_result(token: str, max_bytes: int = budget.DEFAULT_MAX_BYTES) -> dict:
        """Next slice of a byte-budgeted result, by its `continuation` token.
        Tokens are single-use and do not survive a server restart."""
        return budget.continue_result(token, max_bytes)

    @mcp.tool()
    def update(
        id: str,
        text: Optional[str] = None,
        meta: Optional[dict] = None,
        source: Optional[str] = None,
        valid_from: Optional[str] = None,
    ) -> dict:
        """Correct or extend a memory (meta is merged). Pass `valid_from` when the
        change took effect earlier than now (a backdated correction) and `source`
        for the evidence. Earlier beliefs stay queryable."""
        return store.update(id, text=text, meta=meta, source=source, valid_from=valid_from)

    @mcp.tool()
    def delete(id: str, source: Optional[str] = None, valid_until: Optional[str] = None) -> dict:
        """Delete a memory: it stops being true from `valid_until` (default now).
        Its history stays queryable via recall_believed and memory_timeline."""
        return store.delete(id, source=source, valid_until=valid_until)

    @mcp.tool()
    def link(src: str, dst: str, rel: str = "relates_to", weight: float = 1.0) -> dict:
        """Create a typed, weighted edge between two memories (rel names a
        mechanism, e.g. relates_to / follows / contradicts)."""
        return store.link(src, dst, rel=rel, weight=weight)

    @mcp.tool()
    def recall_as_of(
        t: str,
        query: Optional[str] = None,
        k: int = 20,
        max_bytes: int = budget.DEFAULT_MAX_BYTES,
    ) -> dict:
        """What the memory store contained at ISO-8601 instant t, by write time
        (updates/deletes are never destructive). For "what was true then" versus
        "what was known then", use recall_believed. Byte-budgeted output (see
        search)."""
        return budget.budget_slice(store.recall_as_of(t, query=query, k=k), "memories", max_bytes)

    @mcp.tool()
    def recall_believed(
        valid_at: Optional[str] = None,
        known_at: Optional[str] = None,
        query: Optional[str] = None,
        k: int = 20,
        max_bytes: int = budget.DEFAULT_MAX_BYTES,
    ) -> dict:
        """Two-axis recall: what was true at `valid_at`, according to what had been
        recorded by `known_at` (both ISO-8601, default now). Each result carries
        valid_from, recorded_at (stamped by the database, not the caller) and
        source. Byte-budgeted output (see search)."""
        out = store.recall_believed(valid_at=valid_at, known_at=known_at, query=query, k=k)
        return budget.budget_slice(out, "memories", max_bytes)

    @mcp.tool()
    def memory_timeline(
        id: str, limit: int = 100, max_bytes: int = budget.DEFAULT_MAX_BYTES
    ) -> dict:
        """Every recorded belief about one memory: each assertion and retraction
        with when it was valid, when it was recorded, and its source."""
        return budget.budget_slice(store.memory_timeline(id, limit=limit), "events", max_bytes)

    @mcp.tool()
    def stats() -> dict:
        """Store diagnostics: counts, indices, db path, embedding model."""
        return store.stats()

    return mcp


def tool_names(mcp: FastMCP) -> List[str]:  # pragma: no cover - trivial
    return [t.name for t in mcp._tool_manager.list_tools()]


__all__: Any = ["build_server", "tool_names"]
