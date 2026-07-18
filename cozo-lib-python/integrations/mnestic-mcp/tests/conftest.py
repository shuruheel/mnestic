"""Deterministic 2-D keyword embedder (house pattern — no network/ML deps) and
a tmp-sqlite MemoryStore fixture. The schema takes its dim from the embedder,
so tests build 2-d HNSW indexes."""

from typing import List

import pytest

from mnestic_mcp.memory import MemoryStore

GRADIENT = {
    "alpha": [1.0, 0.0],
    "beta": [0.95, 0.05],
    "gamma": [0.85, 0.15],
    "delta": [0.7, 0.3],
    "omega": [0.0, 1.0],
}


def gvec(text: str) -> List[float]:
    t = text.lower()
    for tok, vec in GRADIENT.items():
        if tok in t:
            return list(vec)
    return [0.5, 0.5]


class KeywordEmbedder:
    model_name = "test-keyword-2d"
    dim = 2

    def embed_documents(self, texts):
        return [gvec(t) for t in texts]

    def embed_query(self, text):
        return gvec(text)

    def ready(self):
        return True

    def wait_ready(self, timeout: float = 0.0):
        return None


class NeverReadyEmbedder(KeywordEmbedder):
    model_name = "test-never-ready"

    def ready(self):
        return False

    def wait_ready(self, timeout: float = 0.0):
        raise RuntimeError("the embedding model (test) is still downloading/loading")

    def embed_documents(self, texts):
        self.wait_ready()

    def embed_query(self, text):
        self.wait_ready()


def make_memory(tmp_path, engine: str = "sqlite", embedder=None) -> MemoryStore:
    from mnestic import CozoDbPy

    path = str(tmp_path / "memory.db") if engine == "sqlite" else ""
    db = CozoDbPy(engine, path, "{}")
    return MemoryStore(db, embedder or KeywordEmbedder(), db_path=path, engine=engine)


@pytest.fixture()
def mem(tmp_path):
    return make_memory(tmp_path)
