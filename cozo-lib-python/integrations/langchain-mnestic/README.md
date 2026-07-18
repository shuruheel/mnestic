# langchain-mnestic

A [LangChain](https://github.com/langchain-ai/langchain) `VectorStore` backed by
[mnestic](https://github.com/shuruheel/mnestic) — an embedded graph + vector +
full-text database (a maintained fork of CozoDB). Retrieval is **hybrid** by
default: dense (HNSW) + keyword (full-text) fused with Reciprocal Rank Fusion, in
one call.

> mnestic is a maintained fork of [CozoDB](https://github.com/cozodb/cozo); it is not the official CozoDB. Original design credit belongs to Ziyang Hu and the Cozo Project Authors.

```bash
pip install langchain-mnestic
```

```python
from langchain_mnestic import MnesticVectorStore
from langchain_openai import OpenAIEmbeddings

store = MnesticVectorStore.from_texts(
    ["the cat sat on the mat", "a dog ran in the park"],
    embedding=OpenAIEmbeddings(),
    metadatas=[{"src": "a"}, {"src": "b"}],
    engine="sqlite", path="mydocs.db",   # or engine="mem" for ephemeral
)

docs = store.similarity_search("feline", k=2)
retriever = store.as_retriever(search_kwargs={"k": 4})
```

Scores returned by `similarity_search_with_score` are RRF **fused scores — higher
is better** (a relevance score, not a distance).

## Engine pass-through: graph legs and more

Any extra keyword argument on the search methods goes straight to the engine's
hybrid query. That includes **`graph_legs`** — fuse graph proximity into the
ranking alongside the dense and keyword legs, in the same single call:

```python
db = store._store.db  # or pass your own CozoDbPy via MnesticVectorStore(db=...)
db.run_script(":create links {src: String, dst: String}", {}, False)
db.run_script("?[src, dst] <- [['doc-1', 'doc-9']] :put links {src, dst}", {}, False)

docs = store.similarity_search(
    "feline", k=4,
    graph_legs=[{
        "edge_relation": "links", "from_col": "src", "to_col": "dst",
        "seeds": ["doc-1"], "max_hops": 2, "label": "graph",
    }],
)
```

`extra_lists`, `vector_k`, `fts_k`, `rrf_k`, and future engine keys pass through
the same way — no adapter release needed when the engine grows new knobs.

## Mem0

The store works as a [Mem0](https://github.com/mem0ai/mem0) vector-store backend
through Mem0's `provider="langchain"` (it implements the by-vector search, scored
search, `get_by_ids`, `add_embeddings`, and scalar metadata `filter` calls Mem0's
wrapper makes — exercised in `tests/test_mem0.py`):

```python
config = {"vector_store": {"provider": "langchain",
                           "config": {"client": store, "collection_name": "mem0"}}}
```

## License

Mozilla Public License 2.0.
