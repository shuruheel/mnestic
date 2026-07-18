"""Protocol-level smoke: a real MCP client session against the server,
in-process (no subprocess/stdio flakiness)."""

import json

import anyio
import pytest
from mcp.shared.memory import create_connected_server_and_client_session

from mnestic_mcp.server import build_server

from tests.conftest import make_memory


def test_initialize_list_tools_store_search(tmp_path):
    store = make_memory(tmp_path)
    mcp = build_server(store)

    async def flow():
        async with create_connected_server_and_client_session(
            mcp._mcp_server
        ) as client:
            init = await client.initialize()
            assert "no session" in (init.instructions or "")

            tools = await client.list_tools()
            assert len(tools.tools) == 10

            stored = await client.call_tool(
                "store_memory", {"text": "alpha protocol fact", "meta": {"k": 1}}
            )
            assert not stored.isError
            payload = json.loads(stored.content[0].text)
            assert payload["id"]

            found = await client.call_tool("search", {"query": "protocol"})
            assert not found.isError
            results = json.loads(found.content[0].text)["results"]
            assert [r["id"] for r in results] == [payload["id"]]

            st = await client.call_tool("stats", {})
            assert json.loads(st.content[0].text)["memories"] == 1

    anyio.run(flow)


def test_tool_error_surfaces_cleanly(tmp_path):
    store = make_memory(tmp_path)
    mcp = build_server(store)

    async def flow():
        async with create_connected_server_and_client_session(
            mcp._mcp_server
        ) as client:
            await client.initialize()
            res = await client.call_tool(
                "recall_as_of", {"t": "1969-01-01T00:00:00Z"}
            )
            assert res.isError  # a clean tool error — not a crashed server
            assert "1970" in res.content[0].text

    anyio.run(flow)
