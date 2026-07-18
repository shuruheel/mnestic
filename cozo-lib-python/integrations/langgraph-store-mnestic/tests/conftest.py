"""Shared fixtures: deterministic keyword embedder (no network/ML deps) and
store factories over mem + sqlite. Persistent-path tests use sqlite — the mem
backend uses a different join operator than the stored backends, so sqlite is
the one that exercises the real path."""

from typing import List

import pytest

from langgraph_store_mnestic import MnesticStore

# Token -> vector gradient. For a query embedding [1, 0] ("alpha...") the
# vector ranking over docs a/b/c/d is strict: alpha > beta > gamma > delta.
# "omega" is orthogonal. Chosen tokens are not English stopwords (the FTS
# index stopwords common words — "hello" matches nothing on the text leg).
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


def kw_embed(texts) -> List[List[float]]:
    return [gvec(t) for t in texts]


INDEX = {"dims": 2, "embed": kw_embed}


def make_store(engine: str, tmp_path, *, index=INDEX, ttl=None, **kwargs) -> MnesticStore:
    if engine == "sqlite":
        kwargs = {"engine": "sqlite", "path": str(tmp_path / "store.db"), **kwargs}
    else:
        kwargs = {"engine": "mem", **kwargs}
    return MnesticStore(index=index, ttl=ttl, **kwargs)


@pytest.fixture(params=["mem", "sqlite"])
def store(request, tmp_path):
    s = make_store(request.param, tmp_path)
    yield s
    s.close()


@pytest.fixture()
def sqlite_store(tmp_path):
    s = make_store("sqlite", tmp_path)
    yield s
    s.close()
