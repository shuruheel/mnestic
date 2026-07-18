"""Mem0 `provider="langchain"` compatibility, executed — not inferred.

Instantiates Mem0's own Langchain vector-store wrapper around a
`MnesticVectorStore` and exercises the exact methods the wrapper calls
(insert -> add_embeddings/add_texts, search -> *_by_vector, get -> get_by_ids,
delete -> delete). No LLM, no network: skipped unless `mem0ai` (and its
`langchain-community` import dependency) is installed.

The full `mem0.Memory` pipeline needs an LLM key; an env-gated smoke for that
lives at the bottom (`MEM0_E2E=1` + `OPENAI_API_KEY`).
"""

import os
from typing import List

import pytest

mem0_langchain = pytest.importorskip(
    "mem0.vector_stores.langchain", reason="mem0ai (+ langchain-community) not installed"
)

from langchain_core.embeddings import Embeddings

from langchain_mnestic import MnesticVectorStore


class TwoDimEmbeddings(Embeddings):
    """Deterministic 2-D embedder; mem0 supplies vectors itself for search."""

    @staticmethod
    def _e(t: str) -> List[float]:
        t = t.lower()
        return [1.0, 0.0] if "alpha" in t else [0.0, 1.0]

    def embed_documents(self, texts):
        return [self._e(t) for t in texts]

    def embed_query(self, text):
        return self._e(text)


def _wrapper():
    store = MnesticVectorStore(TwoDimEmbeddings(), engine="mem", dim=2)
    return mem0_langchain.Langchain(client=store, collection_name="mem0"), store


def test_mem0_wrapper_insert_search_get_delete():
    wrapper, _store = _wrapper()

    # insert: prefers add_embeddings(embeddings=, metadatas=, ids=) when present.
    wrapper.insert(
        vectors=[[1.0, 0.0], [0.0, 1.0]],
        payloads=[
            {"data": "alpha memory", "user_id": "u1"},
            {"data": "omega memory", "user_id": "u2"},
        ],
        ids=["m1", "m2"],
    )

    # search: by-vector with real scores (similarity_search_with_score_by_vector).
    results = wrapper.search(query="alpha", vectors=[1.0, 0.0], top_k=1)
    assert results, "mem0 search returned nothing"
    top = results[0]
    assert top.id == "m1"
    assert top.payload.get("data") == "alpha memory"
    assert top.score is not None

    # search with filters — the shape every real mem0 Memory call uses
    # (user_id scoping). Must return only u2's memory despite the query
    # vector pointing at u1's.
    scoped = wrapper.search(query="alpha", vectors=[1.0, 0.0], top_k=5, filters={"user_id": "u2"})
    assert [r.id for r in scoped] == ["m2"]

    # get: get_by_ids under the hood.
    got = wrapper.get("m2")
    assert got is not None
    assert got.id == "m2"

    # update: delete + insert under the hood.
    wrapper.update("m2", vector=[0.0, 1.0], payload={"data": "omega revised", "user_id": "u2"})
    assert wrapper.get("m2").payload.get("data") == "omega revised"

    # delete
    wrapper.delete("m1")
    after = wrapper.search(query="alpha", vectors=[1.0, 0.0], top_k=2)
    assert all(r.id != "m1" for r in after)


@pytest.mark.skipif(
    not (os.environ.get("MEM0_E2E") and os.environ.get("OPENAI_API_KEY")),
    reason="full-pipeline e2e is env-gated (MEM0_E2E=1 + OPENAI_API_KEY)",
)
def test_mem0_full_memory_pipeline():
    from mem0 import Memory

    store = MnesticVectorStore(TwoDimEmbeddings(), engine="mem", dim=1536)
    m = Memory.from_config(
        {
            "vector_store": {
                "provider": "langchain",
                "config": {"client": store, "collection_name": "mem0"},
            }
        }
    )
    m.add("I prefer window seats on long flights.", user_id="u1")
    found = m.search("what seats does the user like?", user_id="u1")
    assert found.get("results")
