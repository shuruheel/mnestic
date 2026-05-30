# llama-index-vector-stores-mnestic

A [LlamaIndex](https://github.com/run-llama/llama_index) vector store backed by
[mnestic](https://github.com/shuruheel/mnestic) — an embedded graph + vector +
full-text database (a maintained fork of CozoDB). Retrieval is **hybrid**: dense
(HNSW) + keyword (full-text) fused with Reciprocal Rank Fusion, in one call.

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

## License

Mozilla Public License 2.0.
