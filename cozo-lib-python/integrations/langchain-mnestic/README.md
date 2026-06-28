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

## License

Mozilla Public License 2.0.
