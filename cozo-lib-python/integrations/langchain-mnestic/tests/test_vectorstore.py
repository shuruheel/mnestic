"""End-to-end tests for langchain-mnestic (mem + sqlite). Deterministic keyword
embedder — no network / ML deps. Run: pytest (inside a venv with mnestic +
langchain-core + langchain-mnestic installed)."""

import os
import tempfile
from typing import List

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
