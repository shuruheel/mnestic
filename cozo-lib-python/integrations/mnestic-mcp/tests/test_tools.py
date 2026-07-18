"""Tool-level behavior: registered surface + the embedder-not-ready path."""

import pytest

from mnestic_mcp.server import build_server, tool_names

from tests.conftest import NeverReadyEmbedder, make_memory

EXPECTED_TOOLS = {
    "store_memory",
    "store_batch",
    "search",
    "find_related",
    "list_recent",
    "update",
    "delete",
    "link",
    "recall_as_of",
    "stats",
}


def test_server_registers_ten_tools(mem):
    mcp = build_server(mem)
    assert set(tool_names(mcp)) == EXPECTED_TOOLS
    assert len(tool_names(mcp)) == 10
    assert "no session" in (mcp.instructions or "")


def test_embedder_not_ready_paths(tmp_path):
    m = make_memory(tmp_path, embedder=NeverReadyEmbedder())

    # embedding-dependent tools fail with the actionable message...
    with pytest.raises(RuntimeError, match="downloading"):
        m.store("alpha fact")
    with pytest.raises(RuntimeError, match="downloading"):
        m.search("anything", mode="semantic")

    # ...while keyword search, recall, and stats keep working
    assert m.search("anything", mode="keyword")["results"] == []
    assert m.recall_as_of("2024-01-01T00:00:00Z")["memories"] == []
    s = m.stats()
    assert s["embedder_ready"] is False
