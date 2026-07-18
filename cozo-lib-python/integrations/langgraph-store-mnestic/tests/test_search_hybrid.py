"""Hybrid search: BM25 must demonstrably contribute (not just ride along), the
namespace filter must be pushed into the index legs, and per-op index controls
must hold across both legs."""

import pytest
from langgraph.store.base import PutOp

from langgraph_store_mnestic import MnesticStore

from tests.conftest import INDEX, make_store


def _seed_gradient(store):
    # vector ranking for query "alpha ...": a > b > c > d (strict gradient);
    # only d's text contains the rare keyword "zebra".
    store.put(("g",), "a", {"text": "alpha doc"})
    store.put(("g",), "b", {"text": "beta doc"})
    store.put(("g",), "c", {"text": "gamma doc"})
    store.put(("g",), "d", {"text": "delta doc zebra"})


def test_bm25_leg_changes_ranking(store):
    _seed_gradient(store)
    # Pure vector order would be [a, b, c, d]. The query keyword "zebra"
    # matches only d's text, so hybrid RRF must fuse d above b and c —
    # something the vector leg alone can never produce.
    hits = store.search(("g",), query="alpha zebra")
    keys = [h.key for h in hits]
    assert keys[0] == "a"
    assert keys.index("d") < keys.index("b")
    assert keys.index("d") < keys.index("c")
    assert all(isinstance(h.score, float) for h in hits)


def test_namespace_filter_pushdown_with_distractors(store):
    # 500 distractors that are all vector-identical to the target: a global
    # top-k that ignored the namespace filter would drown the target out.
    target_ns = ("team", "x")
    store.put(target_ns, "needle", {"text": "omega target row"})
    ops = [
        PutOp(namespace=("other",), key=f"d{i}", value={"text": f"omega distractor {i}"})
        for i in range(500)
    ]
    store.batch(ops)

    hits = store.search(target_ns, query="omega target", limit=5)
    assert [h.key for h in hits] == ["needle"]


def test_index_false_excluded_from_query_but_scannable(store):
    store.put(("ix",), "indexed", {"text": "omega indexed"})
    store.put(("ix",), "hidden", {"text": "omega hidden"}, index=False)

    hits = store.search(("ix",), query="omega")
    assert {h.key for h in hits} == {"indexed"}

    scan = store.search(("ix",))
    assert {h.key for h in scan} == {"indexed", "hidden"}
    assert store.get(("ix",), "hidden") is not None


def test_per_op_index_fields_and_multi_instance(store):
    store.put(
        ("mv",),
        "k",
        {"title": "omega title", "tags": ["alpha special", "zebra unusual"], "skip": "gamma"},
        index=["tags[*]"],
    )
    # findable via either tag instance...
    assert [h.key for h in store.search(("mv",), query="zebra unusual")] == ["k"]
    assert [h.key for h in store.search(("mv",), query="alpha special")] == ["k"]
    # ...but no duplicate results despite two matching vector rows
    hits = store.search(("mv",), query="alpha zebra")
    assert [h.key for h in hits] == ["k"]
    # only the tags[*] instances were indexed — not title/skip (asserted on
    # the stored rows: the vector leg would match ANY query text, so a
    # public-API "no result" assertion cannot discriminate here)
    rows = store.db.run_script(
        f"?[field, seq, text] := *{store._vecs}{{field, seq, text}}", {}, True
    )["rows"]
    assert sorted(rows) == [["tags[*]", 0, "alpha special"], ["tags[*]", 1, "zebra unusual"]]


def test_hostile_fts_queries_do_not_raise(store):
    store.put(("h",), "k", {"text": "alpha content"})
    for q in ["cat AND (", 'quote " unbalanced', "NEAR^ * ;", "  ", "AND OR NOT"]:
        store.search(("h",), query=q)  # must not raise


def test_dims_mismatch_reopen_raises(tmp_path):
    s = make_store("sqlite", tmp_path)
    s.put(("r",), "k", {"text": "alpha"})
    s.close()

    with pytest.raises(ValueError, match="dim"):
        MnesticStore(
            engine="sqlite",
            path=str(tmp_path / "store.db"),
            index={"dims": 3, "embed": lambda texts: [[0.0, 0.0, 0.0] for _ in texts]},
        )
    with pytest.raises(ValueError, match="IndexConfig"):
        MnesticStore(engine="sqlite", path=str(tmp_path / "store.db"), index=None)


def test_query_on_index_config_fields(tmp_path):
    s = make_store(
        "mem", tmp_path, index={**INDEX, "fields": ["summary"]}
    )
    s.put(("cf",), "k", {"summary": "zebra findings", "body": "omega body text"})
    assert [h.key for h in s.search(("cf",), query="zebra")] == ["k"]
    # only the configured field was extracted/indexed
    rows = s.db.run_script(f"?[field, text] := *{s._vecs}{{field, text}}", {}, True)["rows"]
    assert rows == [["summary", "zebra findings"]]
    s.close()
