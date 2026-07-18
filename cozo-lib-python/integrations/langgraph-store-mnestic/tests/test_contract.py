"""BaseStore contract parity: CRUD, list_namespaces, filters, pagination, and
the batch semantics mirrored from the reference InMemoryStore (reads observe
the pre-batch state; puts dedupe last-write-wins and apply atomically)."""

import pytest
from langgraph.store.base import GetOp, ListNamespacesOp, PutOp, SearchOp

from tests.conftest import make_store


def test_put_get_roundtrip(store):
    value = {
        "text": "alpha memo",
        "nested": {"a": [1, 2, {"b": "c"}], "unicode": "héllo wörld ✓"},
        "n": 42,
        "f": 1.5,
        "flag": True,
        "none": None,
    }
    store.put(("users", "u1"), "k1", value)
    item = store.get(("users", "u1"), "k1")
    assert item is not None
    assert item.value == value
    assert item.key == "k1"
    assert item.namespace == ("users", "u1")
    assert item.created_at is not None and item.updated_at is not None


def test_update_preserves_created_at(store):
    clock = {"t": 1000.0}
    store._now = lambda: clock["t"]
    store.put(("ns",), "k", {"v": 1})
    first = store.get(("ns",), "k")
    clock["t"] = 2000.0
    store.put(("ns",), "k", {"v": 2})
    second = store.get(("ns",), "k")
    assert second.value == {"v": 2}
    assert second.created_at == first.created_at
    assert second.updated_at > first.updated_at


def test_delete_and_missing(store):
    store.put(("ns",), "k", {"v": 1})
    assert store.get(("ns",), "k") is not None
    store.delete(("ns",), "k")
    assert store.get(("ns",), "k") is None
    assert store.get(("nowhere",), "nope") is None


def test_batch_reads_see_pre_batch_state(store):
    # Mirrors InMemoryStore: a Get/Search in the same batch as a Put observes
    # the pre-batch state; the Put becomes visible only after the batch.
    results = store.batch(
        [
            PutOp(namespace=("b",), key="k", value={"text": "alpha item"}),
            GetOp(namespace=("b",), key="k"),
            SearchOp(namespace_prefix=("b",), query="alpha"),
        ]
    )
    assert results[0] is None
    assert results[1] is None  # pre-batch state: not there yet
    assert results[2] == []
    assert store.get(("b",), "k") is not None
    found = store.search(("b",), query="alpha")
    assert [i.key for i in found] == ["k"]


def test_batch_put_dedupe_last_write_wins(store):
    store.batch(
        [
            PutOp(namespace=("d",), key="k", value={"v": 1}),
            PutOp(namespace=("d",), key="k", value={"v": 2}),
        ]
    )
    assert store.get(("d",), "k").value == {"v": 2}


def test_batch_atomic_abort_on_bad_value(store):
    store.put(("a",), "pre", {"v": 0})
    with pytest.raises(Exception):
        store.batch(
            [
                PutOp(namespace=("a",), key="good", value={"v": 1}),
                # a set() cannot cross the binding — fails mid-transaction
                PutOp(namespace=("a",), key="bad", value={"v": {1, 2, 3}}),
            ]
        )
    # nothing from the failed batch may be visible
    assert store.get(("a",), "good") is None
    assert store.get(("a",), "pre") is not None


def test_batch_rejects_bad_ops(store):
    with pytest.raises(ValueError):
        store.batch([PutOp(namespace=(), key="k", value={})])
    with pytest.raises((TypeError, ValueError)):
        store.batch([PutOp(namespace=("a",), key="k", value=["not", "a", "dict"])])
    with pytest.raises(ValueError):
        store.batch(["not an op"])


def test_list_namespaces(store):
    for ns in [("a", "b", "c"), ("a", "b", "d"), ("a", "x"), ("z",)]:
        store.put(ns, "k", {"v": 1})

    all_ns = store.list_namespaces()
    assert all_ns == sorted([("a", "b", "c"), ("a", "b", "d"), ("a", "x"), ("z",)])

    assert store.list_namespaces(prefix=("a", "b")) == [("a", "b", "c"), ("a", "b", "d")]
    assert store.list_namespaces(suffix=("d",)) == [("a", "b", "d")]
    assert store.list_namespaces(prefix=("a", "*", "c")) == [("a", "b", "c")]
    assert store.list_namespaces(max_depth=2) == [("a", "b"), ("a", "x"), ("z",)]
    assert store.list_namespaces(limit=2) == [("a", "b", "c"), ("a", "b", "d")]
    assert store.list_namespaces(offset=3) == [("z",)]


def test_scan_search_filters_and_pagination(store):
    for i in range(6):
        store.put(
            ("f",),
            f"k{i}",
            {"i": i, "even": i % 2 == 0, "tag": {"deep": f"t{i % 3}"}, "text": "omega row"},
        )

    assert {i.key for i in store.search(("f",), filter={"even": True})} == {"k0", "k2", "k4"}
    assert {i.key for i in store.search(("f",), filter={"i": {"$gte": 2, "$lt": 5}})} == {
        "k2",
        "k3",
        "k4",
    }
    assert {i.key for i in store.search(("f",), filter={"i": {"$ne": 0}, "even": True})} == {
        "k2",
        "k4",
    }
    assert {i.key for i in store.search(("f",), filter={"tag": {"deep": "t1"}})} == {"k1", "k4"}
    assert {i.key for i in store.search(("f",), filter={"i": {"$eq": 3}})} == {"k3"}
    assert {i.key for i in store.search(("f",), filter={"i": {"$lte": 0}})} == {"k0"}
    assert {i.key for i in store.search(("f",), filter={"i": {"$gt": 4}})} == {"k5"}

    page1 = store.search(("f",), limit=2)
    page2 = store.search(("f",), limit=2, offset=2)
    assert [i.key for i in page1] == ["k0", "k1"]
    assert [i.key for i in page2] == ["k2", "k3"]
    assert all(i.score is None for i in page1)


def test_async_twins(store):
    import asyncio

    async def flow():
        await store.aput(("as",), "k", {"text": "alpha async"})
        item = await store.aget(("as",), "k")
        hits = await store.asearch(("as",), query="alpha")
        namespaces = await store.alist_namespaces()
        await store.adelete(("as",), "k")
        gone = await store.aget(("as",), "k")
        return item, hits, namespaces, gone

    item, hits, namespaces, gone = asyncio.run(flow())
    assert item is not None and item.value["text"] == "alpha async"
    assert [h.key for h in hits] == ["k"]
    assert ("as",) in namespaces
    assert gone is None


def test_bm25_only_store(tmp_path):
    s = make_store("mem", tmp_path, index=None)
    s.put(("b",), "k1", {"text": "zebra migration patterns"})
    s.put(("b",), "k2", {"text": "unrelated content entirely"})
    hits = s.search(("b",), query="zebra")
    assert [h.key for h in hits] == ["k1"]
    assert hits[0].score is not None
    s.close()
