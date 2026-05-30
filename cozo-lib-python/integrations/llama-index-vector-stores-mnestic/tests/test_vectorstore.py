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
