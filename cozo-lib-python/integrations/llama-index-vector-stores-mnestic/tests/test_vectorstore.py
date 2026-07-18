"""End-to-end tests for llama-index-vector-stores-mnestic. Deterministic keyword
embedder — no network / ML deps. Run: pytest (inside a venv with mnestic +
llama-index-core + the package installed)."""

from typing import List

from llama_index.core import Document, StorageContext, VectorStoreIndex
from llama_index.core.embeddings import BaseEmbedding

from llama_index.vector_stores.mnestic import MnesticRetriever, MnesticVectorStore


class KeywordEmbedding(BaseEmbedding):
    @staticmethod
    def _e(t: str) -> List[float]:
        t = t.lower()
        cat = 1.0 if ("cat" in t or "feline" in t) else 0.0
        dog = 1.0 if "dog" in t else 0.0
        return [cat, dog] if (cat or dog) else [0.05, 0.05]

    def _get_query_embedding(self, query: str) -> List[float]:
        return self._e(query)

    def _get_text_embedding(self, text: str) -> List[float]:
        return self._e(text)

    async def _aget_query_embedding(self, query: str) -> List[float]:
        return self._e(query)

    async def _aget_text_embedding(self, text: str) -> List[float]:
        return self._e(text)


TEXTS = [
    "the cat sat on the mat",
    "a dog ran in the park",
    "birds fly in the sky",
    "the weather is nice",
]


def _index():
    vs = MnesticVectorStore(dim=2, engine="mem")
    sc = StorageContext.from_defaults(vector_store=vs)
    docs = [Document(text=t, metadata={"i": i}) for i, t in enumerate(TEXTS)]
    return vs, VectorStoreIndex.from_documents(docs, storage_context=sc, embed_model=KeywordEmbedding())


def test_vector_store_index_retrieve():
    _vs, index = _index()
    nodes = index.as_retriever(similarity_top_k=2).retrieve("feline")
    assert nodes[0].node.get_content() == "the cat sat on the mat"
    assert all(n.score is not None for n in nodes)


def test_mnestic_retriever_hybrid():
    vs, _index_ = _index()
    nodes = MnesticRetriever(vs, KeywordEmbedding(), similarity_top_k=2).retrieve("dog")
    assert nodes[0].node.get_content() == "a dog ran in the park"


# --- graph_legs / pass-through (the previously-dropped **kwargs) ---
#
# Strict vector gradient for the query "alpha" (a > b > c > d), FTS matches
# only a, and an edge a -> d lets a graph leg boost d above b. The old
# `query()` dropped **kwargs on the floor and silently returned graph-blind
# results — these tests fail against that behavior.

from llama_index.core.schema import TextNode  # noqa: E402
from llama_index.core.vector_stores.types import VectorStoreQuery  # noqa: E402

GRADIENT = {
    "alpha": [1.0, 0.0],
    "beta": [0.95, 0.05],
    "gamma": [0.85, 0.15],
    "delta": [0.7, 0.3],
}

GRAPH_LEG = {
    "edge_relation": "links",
    "from_col": "src",
    "to_col": "dst",
    "seeds": ["a"],
    "max_hops": 1,
    "label": "graph",
}


def _gvec(t: str) -> List[float]:
    t = t.lower()
    for tok, v in GRADIENT.items():
        if tok in t:
            return list(v)
    return [0.5, 0.5]


class GradientEmbedding(BaseEmbedding):
    def _get_query_embedding(self, query: str) -> List[float]:
        return _gvec(query)

    def _get_text_embedding(self, text: str) -> List[float]:
        return _gvec(text)

    async def _aget_query_embedding(self, query: str) -> List[float]:
        return _gvec(query)

    async def _aget_text_embedding(self, text: str) -> List[float]:
        return _gvec(text)


def _graph_vs() -> MnesticVectorStore:
    vs = MnesticVectorStore(dim=2, engine="mem")
    nodes = [
        TextNode(id_=i, text=f"{tok} doc", embedding=_gvec(tok))
        for i, tok in zip("abcd", ["alpha", "beta", "gamma", "delta"])
    ]
    vs.add(nodes)
    db = vs.client
    db.run_script(":create links {src: String, dst: String}", {}, False)
    db.run_script("?[src, dst] <- [['a', 'd']] :put links {src, dst}", {}, False)
    return vs


def test_graph_legs_change_ranking_direct_query():
    vs = _graph_vs()
    q = VectorStoreQuery(query_embedding=GRADIENT["alpha"], query_str="alpha", similarity_top_k=4)
    base = list(vs.query(q).ids)
    assert base == ["a", "b", "c", "d"]

    boosted = list(vs.query(q, graph_legs=[GRAPH_LEG]).ids)
    assert boosted != base
    assert boosted.index("d") < boosted.index("b")


def test_graph_legs_through_index_retriever():
    vs = _graph_vs()
    index = VectorStoreIndex.from_vector_store(vs, embed_model=GradientEmbedding())
    base = [n.node.node_id for n in index.as_retriever(similarity_top_k=4).retrieve("alpha")]
    boosted = [
        n.node.node_id
        for n in index.as_retriever(
            similarity_top_k=4, vector_store_kwargs={"graph_legs": [GRAPH_LEG]}
        ).retrieve("alpha")
    ]
    assert boosted != base
    assert boosted.index("d") < boosted.index("b")


def test_mnestic_retriever_search_kwargs():
    vs = _graph_vs()
    r = MnesticRetriever(
        vs, GradientEmbedding(), similarity_top_k=4, search_kwargs={"graph_legs": [GRAPH_LEG]}
    )
    ids = [n.node.node_id for n in r.retrieve("alpha")]
    assert ids.index("d") < ids.index("b")


def test_vector_only_when_no_query_str():
    vs = _graph_vs()
    res = vs.query(VectorStoreQuery(query_embedding=GRADIENT["alpha"], similarity_top_k=2))
    assert list(res.ids) == ["a", "b"]
    assert all(s is not None for s in res.similarities)
