# llama-index-vector-stores-mnestic

A [LlamaIndex](https://github.com/run-llama/llama_index) vector store backed by
[mnestic](https://github.com/shuruheel/mnestic) — an embedded graph + vector +
full-text database (a maintained fork of CozoDB). Retrieval is **hybrid**: dense
(HNSW) + keyword (full-text) fused with Reciprocal Rank Fusion, in one call.

> mnestic is a maintained fork of [CozoDB](https://github.com/cozodb/cozo); it is not the official CozoDB. Original design credit belongs to Ziyang Hu and the Cozo Project Authors.

```bash
pip install llama-index-vector-stores-mnestic
```

```python
from llama_index.core import VectorStoreIndex, StorageContext, Document
from llama_index.vector_stores.mnestic import MnesticVectorStore

vector_store = MnesticVectorStore(dim=1536, engine="sqlite", path="mydocs.db")
storage_context = StorageContext.from_defaults(vector_store=vector_store)

index = VectorStoreIndex.from_documents(
    [Document(text="the cat sat on the mat")],
    storage_context=storage_context,
)
nodes = index.as_retriever(similarity_top_k=4).retrieve("feline")
```

`dim` must match your embedding model's dimension. `query` runs hybrid search when
the query carries text (the usual index path), otherwise vector-only.

## Engine pass-through: graph legs and more

Extra keyword arguments flow from the retriever to the engine's hybrid query via
`vector_store_kwargs`. That includes **`graph_legs`** — fuse graph proximity into
the ranking alongside the dense and keyword legs, in the same single call:

```python
retriever = index.as_retriever(
    similarity_top_k=4,
    vector_store_kwargs={"graph_legs": [{
        "edge_relation": "links", "from_col": "src", "to_col": "dst",
        "seeds": ["doc-1"], "max_hops": 2, "label": "graph",
    }]},
)
```

`MnesticRetriever(..., search_kwargs={...})` forwards the same way, and so do
`extra_lists`, `vector_k`, `fts_k`, `rrf_k`, and future engine keys — no adapter
release needed when the engine grows new knobs.

## License

Mozilla Public License 2.0.
