"""THE discriminating namespace tests: a prior prototype validated namespaces
only in put() (never in batch()), so `('a.b',)` silently overwrote the row for
`('a', 'b')`. The collision-safe encoding must keep every distinct tuple
distinct — for any label content."""

import random

from langgraph.store.base import PutOp

from langgraph_store_mnestic._ns import decode_ns, encode_ns, encode_prefix


def test_dot_label_does_not_collide(store):
    # direct batch() bypasses put()'s upstream validation — exactly the path
    # LangGraph itself uses. These MUST be two distinct rows.
    store.batch(
        [
            PutOp(namespace=("a.b",), key="k", value={"who": "dotted"}),
            PutOp(namespace=("a", "b"), key="k", value={"who": "nested"}),
        ]
    )
    assert store.get(("a.b",), "k").value == {"who": "dotted"}
    assert store.get(("a", "b"), "k").value == {"who": "nested"}

    under_a = store.search(("a",))
    assert [i.value["who"] for i in under_a] == ["nested"]
    assert {tuple(ns) for ns in store.list_namespaces()} == {("a.b",), ("a", "b")}


def test_hostile_labels(store):
    hostile = [
        ("a\x1fb",),          # raw separator inside a label
        ("a", "\x1e"),        # raw escape char
        ("a\x1eS", "b"),      # escape sequence lookalike
        ("a\x1f", "b\x1e\x1e"),
    ]
    for i, ns in enumerate(hostile):
        store.batch([PutOp(namespace=ns, key="k", value={"i": i})])
    for i, ns in enumerate(hostile):
        assert store.get(ns, "k").value == {"i": i}, ns
    encoded = [encode_ns(ns) for ns in hostile]
    assert len(set(encoded)) == len(encoded)


def test_prefix_boundary():
    # ('a',) must never prefix-match ('ab', ...)
    assert not encode_ns(("ab",)).startswith(encode_prefix(("a",)))
    assert encode_ns(("a", "b")).startswith(encode_prefix(("a",)))


def test_prefix_search_boundary(store):
    store.put(("a",), "k", {"v": "short"})
    store.put(("ab",), "k", {"v": "long"})
    hits = store.search(("a",))
    assert [i.value["v"] for i in hits] == ["short"]


def test_encode_decode_fuzz():
    rng = random.Random(42)
    alphabet = "ab.\x1f\x1eS*é"
    seen = {}
    for _ in range(200):
        ns = tuple(
            "".join(rng.choice(alphabet) for _ in range(rng.randint(1, 6)))
            for _ in range(rng.randint(1, 4))
        )
        enc = encode_ns(ns)
        assert decode_ns(enc) == ns
        if enc in seen:
            assert seen[enc] == ns
        seen[enc] = ns

        # tuple-prefix <=> string-prefix equivalence against a random other
        other = tuple(
            "".join(rng.choice(alphabet) for _ in range(rng.randint(1, 6)))
            for _ in range(rng.randint(1, 4))
        )
        is_tuple_prefix = other == ns[: len(other)]
        is_str_prefix = enc.startswith(encode_prefix(other))
        assert is_tuple_prefix == is_str_prefix, (ns, other)
