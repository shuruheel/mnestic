"""FastMCP wiring: 10 thin tools delegating to `MemoryStore`. Tool names speak
mechanism (store/search/link/recall), never a cognitive ontology."""

from __future__ import annotations

from typing import Any, List, Optional

from mcp.server.fastmcp import FastMCP

from mnestic_mcp.instructions import INSTRUCTIONS
from mnestic_mcp.memory import MemoryStore


def build_server(store: MemoryStore) -> FastMCP:
    mcp = FastMCP(name="mnestic-memory", instructions=INSTRUCTIONS)

    @mcp.tool()
    def store_memory(text: str, meta: Optional[dict] = None, id: Optional[str] = None) -> dict:
        """Store one memory (a durable fact, preference, decision, or outcome).
        Returns its id."""
        return store.store(text, meta=meta, id=id)

    @mcp.tool()
    def store_batch(items: List[dict]) -> dict:
        """Store many memories atomically. Each item: {text, meta?, id?}."""
        return store.store_batch(items)

    @mcp.tool()
    def search(
        query: str,
        k: int = 8,
        mode: str = "auto",
        explain: bool = False,
        expand_graph: bool = True,
    ) -> dict:
        """Search memories. mode: auto (keyword-first, hybrid fallback),
        keyword (BM25), semantic (vector), hybrid (BM25+vector+graph fused).
        explain=true returns per-leg attribution for every result."""
        return store.search(query, k=k, mode=mode, explain=explain, expand_graph=expand_graph)

    @mcp.tool()
    def find_related(
        id: str, max_nodes: int = 25, max_depth: int = 3, weighted: bool = False
    ) -> dict:
        """Walk the link graph outward from a memory (budget-bounded traversal);
        returns related memories with cost/depth/parent."""
        return store.find_related(id, max_nodes=max_nodes, max_depth=max_depth, weighted=weighted)

    @mcp.tool()
    def list_recent(n: int = 10) -> dict:
        """The most recently stored or updated memories."""
        return store.list_recent(n)

    @mcp.tool()
    def update(id: str, text: Optional[str] = None, meta: Optional[dict] = None) -> dict:
        """Correct or extend a memory (meta is merged). History is preserved
        for recall_as_of."""
        return store.update(id, text=text, meta=meta)

    @mcp.tool()
    def delete(id: str) -> dict:
        """Delete a memory (its history stays recallable via recall_as_of)."""
        return store.delete(id)

    @mcp.tool()
    def link(src: str, dst: str, rel: str = "relates_to", weight: float = 1.0) -> dict:
        """Create a typed, weighted edge between two memories (rel names a
        mechanism, e.g. relates_to / follows / contradicts)."""
        return store.link(src, dst, rel=rel, weight=weight)

    @mcp.tool()
    def recall_as_of(t: str, query: Optional[str] = None, k: int = 20) -> dict:
        """What the memory store contained at ISO-8601 instant t (time travel;
        updates/deletes are never destructive)."""
        return store.recall_as_of(t, query=query, k=k)

    @mcp.tool()
    def stats() -> dict:
        """Store diagnostics: counts, indices, db path, embedding model."""
        return store.stats()

    return mcp


def tool_names(mcp: FastMCP) -> List[str]:  # pragma: no cover - trivial
    return [t.name for t in mcp._tool_manager.list_tools()]


__all__: Any = ["build_server", "tool_names"]
