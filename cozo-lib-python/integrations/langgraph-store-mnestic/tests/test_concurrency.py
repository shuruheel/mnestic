"""The non-optional concurrency matrix. A prior prototype passed 30/30
sequential checks and then lost 36.4% of concurrent semantic reads because its
batch() was not atomic — a sequential suite is structurally blind here. These
tests are the permanent gate."""

import threading

import pytest
from langgraph.store.base import PutOp

from tests.conftest import make_store

ENGINES = ["mem", "sqlite"]


@pytest.mark.parametrize("engine", ENGINES)
@pytest.mark.parametrize("n_threads", [4, 12])
def test_no_lost_writes(engine, n_threads, tmp_path):
    s = make_store(engine, tmp_path)
    per_thread = 25
    errors = []

    def writer(tid: int):
        try:
            for i in range(per_thread):
                s.put(("w", str(tid)), f"k{i}", {"tid": tid, "i": i, "text": f"omega {tid} {i}"})
        except Exception as e:  # pragma: no cover
            errors.append(e)

    threads = [threading.Thread(target=writer, args=(t,)) for t in range(n_threads)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    assert not errors
    for tid in range(n_threads):
        for i in range(per_thread):
            item = s.get(("w", str(tid)), f"k{i}")
            assert item is not None and item.value["i"] == i
    s.close()


@pytest.mark.parametrize("engine", ENGINES)
def test_always_present_item_never_lost_under_write_load(engine, tmp_path):
    """The 36.4% regression test: readers hybrid-search an always-present item
    while writers hammer the same namespace. Zero zero-result reads allowed."""
    s = make_store(engine, tmp_path)
    ns = ("agent", "memories")
    s.put(ns, "anchor", {"text": "omega anchor fact"})

    n_writers, n_readers, writes_each, min_reads = 8, 8, 30, 600
    zero_results = []
    read_count = [0]
    errors = []
    writers_done = threading.Event()
    lock = threading.Lock()

    def writer(tid: int):
        try:
            for i in range(writes_each):
                s.batch(
                    [
                        PutOp(
                            namespace=ns,
                            key=f"w{tid}-{i}",
                            value={"text": f"filler note {tid} {i}"},
                        )
                    ]
                )
        except Exception as e:  # pragma: no cover
            errors.append(e)

    def reader():
        try:
            while True:
                with lock:
                    if writers_done.is_set() and read_count[0] >= min_reads:
                        return
                    read_count[0] += 1
                hits = s.search(ns, query="omega anchor")
                if not any(h.key == "anchor" for h in hits):
                    zero_results.append(1)
        except Exception as e:  # pragma: no cover
            errors.append(e)

    wthreads = [threading.Thread(target=writer, args=(t,)) for t in range(n_writers)]
    rthreads = [threading.Thread(target=reader) for _ in range(n_readers)]
    for t in wthreads + rthreads:
        t.start()
    for t in wthreads:
        t.join()
    writers_done.set()
    for t in rthreads:
        t.join()

    assert not errors, errors[:3]
    assert read_count[0] >= min_reads
    assert len(zero_results) == 0, (
        f"{len(zero_results)}/{read_count[0]} reads lost the always-present item"
    )
    s.close()


@pytest.mark.parametrize("engine", ENGINES)
def test_correlated_pair_writes_stay_atomic(engine, tmp_path):
    """A writer updates k1 and k2 with the same counter in one batch. Readers
    (reading k1 then k2) must never observe k2 behind k1 — which a non-atomic
    batch would produce constantly."""
    s = make_store(engine, tmp_path)
    ns = ("pair",)
    s.batch(
        [
            PutOp(namespace=ns, key="k1", value={"v": 0}),
            PutOp(namespace=ns, key="k2", value={"v": 0}),
        ]
    )
    violations = []
    errors = []
    done = threading.Event()

    def writer():
        try:
            for i in range(1, 120):
                s.batch(
                    [
                        PutOp(namespace=ns, key="k1", value={"v": i}),
                        PutOp(namespace=ns, key="k2", value={"v": i}),
                    ]
                )
        except Exception as e:  # pragma: no cover
            errors.append(e)
        finally:
            done.set()

    def reader():
        try:
            while not done.is_set():
                v1 = s.get(ns, "k1").value["v"]
                v2 = s.get(ns, "k2").value["v"]
                if v2 < v1:
                    violations.append((v1, v2))
        except Exception as e:  # pragma: no cover
            errors.append(e)

    threads = [threading.Thread(target=writer)] + [
        threading.Thread(target=reader) for _ in range(4)
    ]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    assert not errors, errors[:3]
    assert not violations, violations[:5]
    s.close()
