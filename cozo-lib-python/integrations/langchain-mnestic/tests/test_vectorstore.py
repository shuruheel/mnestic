"""End-to-end tests for langchain-mnestic (mem + sqlite). Deterministic keyword
embedder — no network / ML deps. Run: pytest (inside a venv with mnestic +
langchain-core + langchain-mnestic installed)."""

import os
import tempfile
from typing import List

import pytest
from langchain_core.embeddings import Embeddings

from langchain_mnestic import MnesticVectorStore


class KeywordEmbeddings(Embeddings):
    @staticmethod
    def _e(t: str) -> List[float]:
        t = t.lower()
        cat = 1.0 if ("cat" in t or "feline" in t) else 0.0
        dog = 1.0 if "dog" in t else 0.0
        return [cat, dog] if (cat or dog) else [0.05, 0.05]

    def embed_documents(self, texts):
        return [self._e(t) for t in texts]

    def embed_query(self, text):
        return self._e(text)


# Unambiguous corpus (one cat doc, one dog doc) so RRF can't tie targets.
TEXTS = [
    "the cat sat on the mat",
    "a dog ran in the park",
    "birds fly in the sky",
    "the weather is nice",
]
METAS = [{"src": c} for c in "abcd"]


def test_add_and_hybrid_search_mem():
    s = MnesticVectorStore.from_texts(TEXTS, KeywordEmbeddings(), metadatas=METAS, engine="mem")
    hits = s.similarity_search_with_score("cat", k=3)
    assert hits[0][0].page_content == "the cat sat on the mat"
    assert hits[0][0].metadata["src"] == "a"
    assert all(isinstance(score, float) for _, score in hits)


def test_as_retriever_and_delete():
    s = MnesticVectorStore.from_texts(TEXTS, KeywordEmbeddings(), metadatas=METAS, engine="mem")
    docs = s.as_retriever(search_kwargs={"k": 2}).invoke("cat")
    assert docs[0].page_content == "the cat sat on the mat"

    cat_id = docs[0].id
    assert s.delete([cat_id]) is True
    after = s.similarity_search("cat", k=3)
    assert all(d.page_content != "the cat sat on the mat" for d in after)


def test_sqlite_persistence_across_reopen():
    from mnestic import CozoDbPy

    path = os.path.join(tempfile.mkdtemp(), "docs.db")
    MnesticVectorStore.from_texts(TEXTS, KeywordEmbeddings(), metadatas=METAS, engine="sqlite", path=path)

    reopened = MnesticVectorStore(KeywordEmbeddings(), db=CozoDbPy("sqlite", path, "{}"), dim=2)
    hits = reopened.similarity_search("dog", k=2)
    assert hits[0].page_content == "a dog ran in the park"


# --- graph_legs / pass-through (the un-stranded engine surface) ---
#
# Corpus with a strict vector gradient for the query "alpha" so the hybrid
# ranking is deterministic: vector order a > b > c > d, FTS matches only a.
# An edge a -> d then lets a graph leg boost d above b — the ranking MUST
# change when graph_legs is supplied, which is exactly what the old adapter
# (dropping the kwarg) could never do.

GRADIENT = {
    "alpha": [1.0, 0.0],
    "beta": [0.95, 0.05],
    "gamma": [0.85, 0.15],
    "delta": [0.7, 0.3],
}


class GradientEmbeddings(Embeddings):
    @staticmethod
    def _e(t: str) -> List[float]:
        t = t.lower()
        for tok, v in GRADIENT.items():
            if tok in t:
                return list(v)
        return [0.5, 0.5]

    def embed_documents(self, texts):
        return [self._e(t) for t in texts]

    def embed_query(self, text):
        return self._e(text)


GRAPH_IDS = ["a", "b", "c", "d"]
GRAPH_TEXTS = ["alpha doc", "beta doc", "gamma doc", "delta doc"]

# 0.12.2-safe leg shape: single leg, unique label, explicit cols, client seeds.
GRAPH_LEG = {
    "edge_relation": "links",
    "from_col": "src",
    "to_col": "dst",
    "seeds": ["a"],
    "max_hops": 1,
    "label": "graph",
}


def _graph_store() -> MnesticVectorStore:
    s = MnesticVectorStore.from_texts(
        GRAPH_TEXTS,
        GradientEmbeddings(),
        metadatas=[{"i": i} for i in GRAPH_IDS],
        ids=GRAPH_IDS,
        engine="mem",
    )
    db = s._store.db
    db.run_script(":create links {src: String, dst: String}", {}, False)
    db.run_script("?[src, dst] <- [['a', 'd']] :put links {src, dst}", {}, False)
    return s


def test_graph_legs_change_ranking():
    s = _graph_store()
    base = [d.id for d in s.similarity_search("alpha", k=4)]
    assert base == ["a", "b", "c", "d"]

    boosted = [d.id for d in s.similarity_search("alpha", k=4, graph_legs=[GRAPH_LEG])]
    assert boosted != base
    assert boosted.index("d") < boosted.index("b")


def test_detailed_rejected():
    s = _graph_store()
    with pytest.raises(ValueError, match="detailed"):
        s.similarity_search("alpha", k=2, detailed=True)


def test_by_vector_and_get_by_ids():
    s = _graph_store()
    hits = s.similarity_search_with_score_by_vector(GRADIENT["alpha"], k=2)
    assert [d.id for d, _ in hits] == ["a", "b"]
    assert hits[0][1] >= hits[1][1]

    docs = s.get_by_ids(["b", "missing", "a"])
    assert [d.id for d in docs] == ["b", "a"]
    assert docs[0].page_content == "beta doc"


def test_add_embeddings_mem0_convention():
    s = MnesticVectorStore(GradientEmbeddings(), engine="mem", dim=2)
    ids = s.add_embeddings(
        embeddings=[[1.0, 0.0], [0.0, 1.0]],
        metadatas=[{"data": "alpha memory"}, {"data": "omega memory"}],
        ids=["m1", "m2"],
    )
    assert ids == ["m1", "m2"]

    hits = s.similarity_search_with_score_by_vector([1.0, 0.0], k=1)
    assert hits[0][0].id == "m1"
    assert hits[0][0].page_content == "alpha memory"
