"""LlamaIndex vector store + retriever backed by mnestic hybrid retrieval.

`MnesticVectorStore` is a full `BasePydanticVectorStore` (add / delete / query),
so it works with `VectorStoreIndex.from_documents(..., storage_context=...)`. Its
`query` runs a hybrid (HNSW + FTS → RRF) search when the `VectorStoreQuery` carries
`query_str`, otherwise a vector-only search. `MnesticRetriever` is a thin
`BaseRetriever` over the same backend for direct use.
"""

from __future__ import annotations

from typing import Any, List, Optional, Sequence

from llama_index.core.bridge.pydantic import PrivateAttr
from llama_index.core.retrievers import BaseRetriever
from llama_index.core.schema import BaseNode, MetadataMode, NodeWithScore, QueryBundle, TextNode
from llama_index.core.vector_stores.types import (
    BasePydanticVectorStore,
    VectorStoreQuery,
    VectorStoreQueryResult,
)

from ._core import MnesticStore


class MnesticVectorStore(BasePydanticVectorStore):
    stores_text: bool = True
    flat_metadata: bool = True

    _store: MnesticStore = PrivateAttr()

    def __init__(
        self,
        *,
        dim: int,
        db: Optional[Any] = None,
        engine: str = "mem",
        path: str = "",
        options: str = "{}",
        relation: str = "mnestic_docs",
        distance: str = "Cosine",
        **store_kwargs: Any,
    ) -> None:
        super().__init__()
        if db is None:
            from mnestic import CozoDbPy

            db = CozoDbPy(engine, path, options)
        self._store = MnesticStore(
            db, dim=dim, relation=relation, distance=distance, **store_kwargs
        )

    @property
    def client(self) -> Any:
        return self._store.db

    def add(self, nodes: Sequence[BaseNode], **kwargs: Any) -> List[str]:
        if not nodes:
            return []
        ids = [n.node_id for n in nodes]
        texts = [n.get_content(metadata_mode=MetadataMode.NONE) for n in nodes]
        embeddings = [n.get_embedding() for n in nodes]
        metadatas = [dict(n.metadata or {}) for n in nodes]
        return self._store.add(ids, texts, embeddings, metadatas)

    def delete(self, ref_doc_id: str, **delete_kwargs: Any) -> None:
        self._store.delete([ref_doc_id])

    def query(self, query: VectorStoreQuery, **kwargs: Any) -> VectorStoreQueryResult:
        k = query.similarity_top_k or 4
        query_str = query.query_str or ""
        hits = self._store.search(query.query_embedding, query_str, k)
        nodes: List[TextNode] = []
        similarities: List[float] = []
        ids: List[str] = []
        for h in hits:
            nodes.append(TextNode(id_=str(h["id"]), text=h["text"], metadata=h["metadata"]))
            similarities.append(h["score"])
            ids.append(str(h["id"]))
        return VectorStoreQueryResult(nodes=nodes, similarities=similarities, ids=ids)


class MnesticRetriever(BaseRetriever):
    """Direct hybrid retriever over a `MnesticVectorStore` + an embed model."""

    def __init__(
        self,
        vector_store: MnesticVectorStore,
        embed_model: Any,
        similarity_top_k: int = 4,
        callback_manager: Optional[Any] = None,
    ) -> None:
        self._vector_store = vector_store
        self._embed_model = embed_model
        self._similarity_top_k = similarity_top_k
        super().__init__(callback_manager=callback_manager)

    def _retrieve(self, query_bundle: QueryBundle) -> List[NodeWithScore]:
        embedding = query_bundle.embedding
        if embedding is None:
            embedding = self._embed_model.get_query_embedding(query_bundle.query_str)
        result = self._vector_store.query(
            VectorStoreQuery(
                query_embedding=embedding,
                query_str=query_bundle.query_str,
                similarity_top_k=self._similarity_top_k,
            )
        )
        nodes: List[NodeWithScore] = []
        for i, node in enumerate(result.nodes or []):
            score = result.similarities[i] if result.similarities else None
            nodes.append(NodeWithScore(node=node, score=score))
        return nodes
