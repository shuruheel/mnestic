"""LangChain `VectorStore` backed by mnestic hybrid retrieval (HNSW + FTS → RRF).

`similarity_search`/`_with_score` run a hybrid (dense + keyword) query in one call;
`score` is the RRF fused score — **higher is better** (a relevance score, not a
distance). `add_texts`/`from_texts` embed and write into a mnestic relation,
creating the HNSW + FTS indices on first use.
"""

from __future__ import annotations

import uuid
from typing import Any, Iterable, List, Optional, Sequence, Tuple

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

    def add_embeddings(
        self,
        embeddings: List[List[float]],
        metadatas: Optional[List[dict]] = None,
        *,
        ids: Optional[List[str]] = None,
        texts: Optional[Sequence[str]] = None,
        **kwargs: Any,
    ) -> List[str]:
        """Insert precomputed embeddings (no embedding call).

        The indexed text comes from `texts` when given, else from each
        metadata's `"data"` key (the Mem0 payload convention), else `""`.
        """
        if not embeddings:
            return []
        metas = list(metadatas) if metadatas is not None else [{} for _ in embeddings]
        ids = ids or [uuid.uuid4().hex for _ in embeddings]
        if texts is None:
            texts = [str((m or {}).get("data", "")) for m in metas]
        return self._store.add(ids, list(texts), embeddings, metas)

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
        return [(self._to_document(h), h["score"]) for h in hits]

    def similarity_search(self, query: str, k: int = 4, **kwargs: Any) -> List[Document]:
        return [doc for doc, _ in self.similarity_search_with_score(query, k, **kwargs)]

    def similarity_search_with_score_by_vector(
        self, embedding: List[float], k: int = 4, **kwargs: Any
    ) -> List[Tuple[Document, float]]:
        """Vector-only search from a precomputed embedding.

        `score` is the negated engine distance (higher = more similar), unlike
        the hybrid methods whose score is the RRF fused score. An optional
        `filter` dict (scalar metadata equality, e.g. `{"user_id": "u1"}` —
        the shape Mem0 passes) is applied post-search over an over-fetched
        candidate set; operator filters raise rather than silently match
        nothing.
        """
        metadata_filter = kwargs.pop("filter", None)
        if metadata_filter:
            for key, val in metadata_filter.items():
                if isinstance(val, (dict, list)):
                    raise ValueError(
                        f"unsupported filter for {key!r}: only scalar equality "
                        "filters are supported (got an operator/list form)"
                    )
            hits = self._store.search_by_vector(embedding, max(k * 4, 20), **kwargs)
            hits = [
                h
                for h in hits
                if all(h["metadata"].get(key) == val for key, val in metadata_filter.items())
            ][:k]
        else:
            hits = self._store.search_by_vector(embedding, k, **kwargs)
        return [(self._to_document(h), h["score"]) for h in hits]

    def similarity_search_by_vector(
        self, embedding: List[float], k: int = 4, **kwargs: Any
    ) -> List[Document]:
        return [
            doc
            for doc, _ in self.similarity_search_with_score_by_vector(embedding, k, **kwargs)
        ]

    def get_by_ids(self, ids: Sequence[str], /) -> List[Document]:
        """Fetch documents by id, preserving input order; missing ids are skipped."""
        return [self._to_document(r) for r in self._store.get(list(ids))]

    @staticmethod
    def _to_document(hit: dict) -> Document:
        return Document(
            id=hit["id"],
            page_content=hit["text"],
            metadata={**hit["metadata"], "id": hit["id"]},
        )
