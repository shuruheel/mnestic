"""LangChain `VectorStore` backed by mnestic hybrid retrieval (HNSW + FTS → RRF).

`similarity_search`/`_with_score` run a hybrid (dense + keyword) query in one call;
`score` is the RRF fused score — **higher is better** (a relevance score, not a
distance). `add_texts`/`from_texts` embed and write into a mnestic relation,
creating the HNSW + FTS indices on first use.
"""

from __future__ import annotations

import uuid
from typing import Any, Iterable, List, Optional, Tuple

from langchain_core.documents import Document
from langchain_core.embeddings import Embeddings
from langchain_core.vectorstores import VectorStore

from ._core import MnesticStore


class MnesticVectorStore(VectorStore):
    def __init__(
        self,
        embedding: Embeddings,
        *,
        db: Optional[Any] = None,
        engine: str = "mem",
        path: str = "",
        options: str = "{}",
        relation: str = "mnestic_docs",
        dim: Optional[int] = None,
        distance: str = "Cosine",
        **store_kwargs: Any,
    ) -> None:
        if db is None:
            from mnestic import CozoDbPy

            db = CozoDbPy(engine, path, options)
        if dim is None:
            dim = len(embedding.embed_query("dimension probe"))
        self._embedding = embedding
        self._store = MnesticStore(
            db, dim=dim, relation=relation, distance=distance, **store_kwargs
        )

    @property
    def embeddings(self) -> Optional[Embeddings]:
        return self._embedding

    # --- write ---
    def add_texts(
        self,
        texts: Iterable[str],
        metadatas: Optional[List[dict]] = None,
        *,
        ids: Optional[List[str]] = None,
        **kwargs: Any,
    ) -> List[str]:
        texts = list(texts)
        if not texts:
            return []
        ids = ids or [uuid.uuid4().hex for _ in texts]
        embeddings = self._embedding.embed_documents(texts)
        return self._store.add(ids, texts, embeddings, metadatas)

    @classmethod
    def from_texts(
        cls,
        texts: List[str],
        embedding: Embeddings,
        metadatas: Optional[List[dict]] = None,
        *,
        ids: Optional[List[str]] = None,
        **kwargs: Any,
    ) -> "MnesticVectorStore":
        store = cls(embedding, **kwargs)
        store.add_texts(texts, metadatas, ids=ids)
        return store

    def delete(self, ids: Optional[List[str]] = None, **kwargs: Any) -> Optional[bool]:
        if not ids:
            return False
        self._store.delete(ids)
        return True

    # --- read ---
    def similarity_search_with_score(
        self, query: str, k: int = 4, **kwargs: Any
    ) -> List[Tuple[Document, float]]:
        query_vector = self._embedding.embed_query(query)
        hits = self._store.search(query_vector, query, k, **kwargs)
        return [
            (
                Document(id=h["id"], page_content=h["text"], metadata={**h["metadata"], "id": h["id"]}),
                h["score"],
            )
            for h in hits
        ]

    def similarity_search(self, query: str, k: int = 4, **kwargs: Any) -> List[Document]:
        return [doc for doc, _ in self.similarity_search_with_score(query, k, **kwargs)]
