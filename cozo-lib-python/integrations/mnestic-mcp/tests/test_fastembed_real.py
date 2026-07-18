"""Real fastembed integration — downloads the model (~67 MB, once). Opt-in:
MNESTIC_MCP_TEST_FASTEMBED=1 pytest tests/test_fastembed_real.py"""

import os

import pytest

pytestmark = pytest.mark.skipif(
    not os.environ.get("MNESTIC_MCP_TEST_FASTEMBED"),
    reason="set MNESTIC_MCP_TEST_FASTEMBED=1 to run the real-model test",
)


def test_real_model_roundtrip(tmp_path):
    from mnestic import CozoDbPy

    from mnestic_mcp.embeddings import DEFAULT_MODEL, FastEmbedEmbedder
    from mnestic_mcp.memory import MemoryStore

    emb = FastEmbedEmbedder(model_name=DEFAULT_MODEL, cache_dir=str(tmp_path / "models"))
    assert emb.dim == 384  # registry-resolved before any download

    emb.wait_ready(timeout=600.0)
    db = CozoDbPy("sqlite", str(tmp_path / "m.db"), "{}")
    store = MemoryStore(db, emb, db_path=str(tmp_path / "m.db"), engine="sqlite")

    store.store("the mitochondria is the powerhouse of the cell")
    store.store("paris is the capital of france")
    hits = store.search("cellular energy production", mode="semantic")["results"]
    assert hits and "mitochondria" in hits[0]["text"]
