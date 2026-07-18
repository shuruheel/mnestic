"""The challenger's test as a permanent gate: a real compiled LangGraph with a
parallel fan-out superstep doing store writes + hybrid searches against a
shared namespace. Every node of every invocation must recall the anchor —
parallel superstep execution is LangGraph's reason for existing, and it is
exactly the load that exposed the prototype's non-atomic batch()."""

import asyncio
import operator
import uuid
from typing import Annotated, TypedDict

import pytest

pytest.importorskip("langgraph")

from langgraph.config import get_store  # noqa: E402
from langgraph.graph import END, START, StateGraph  # noqa: E402

from tests.conftest import make_store  # noqa: E402


class State(TypedDict):
    results: Annotated[list, operator.add]


def _make_node(i: int):
    def node(state: State):
        store = get_store()
        store.put(("shared",), f"note-{i}-{uuid.uuid4().hex[:8]}", {"text": f"filler {i}"})
        hits = store.search(("shared",), query="omega anchor", limit=5)
        return {"results": [any(h.key == "anchor" for h in hits)]}

    return node


@pytest.mark.parametrize("engine", ["mem", "sqlite"])
def test_compiled_graph_parallel_fanout(engine, tmp_path):
    store = make_store(engine, tmp_path)
    store.put(("shared",), "anchor", {"text": "omega anchor fact"})

    builder = StateGraph(State)
    for i in range(8):
        builder.add_node(f"n{i}", _make_node(i))
        builder.add_edge(START, f"n{i}")
        builder.add_edge(f"n{i}", END)
    graph = builder.compile(store=store)

    for _ in range(10):
        out = graph.invoke({"results": []})
        assert len(out["results"]) == 8
        assert all(out["results"]), "a parallel node lost the always-present anchor"

    async def arun():
        for _ in range(10):
            out = await graph.ainvoke({"results": []})
            assert len(out["results"]) == 8
            assert all(out["results"])

    asyncio.run(arun())
    store.close()
