"""MemoryStore behavior over a tmp sqlite db with the deterministic embedder."""

import pytest

from mnestic_mcp.memory import MemoryStore, parse_when

from tests.conftest import KeywordEmbedder, make_memory


def test_schema_idempotent_across_reopen(tmp_path):
    m1 = make_memory(tmp_path)
    r = m1.store("alpha fact about zebras", meta={"topic": "animals"})
    del m1

    m2 = make_memory(tmp_path)  # same file, fresh handle: ensure_schema re-runs
    hits = m2.search("zebras", mode="keyword")["results"]
    assert [h["id"] for h in hits] == [r["id"]]


def test_model_pinning_mismatch_raises(tmp_path):
    make_memory(tmp_path)

    class OtherModel(KeywordEmbedder):
        model_name = "test-other-model"

    with pytest.raises(ValueError, match="embedding model"):
        make_memory(tmp_path, embedder=OtherModel())


def test_store_and_search_modes(mem):
    a = mem.store("alpha note about datalog", meta={"k": 1})
    mem.store("omega note about zebras", meta={"k": 2})

    kw = mem.search("datalog", mode="keyword")
    assert kw["mode_used"] == "keyword"
    assert [h["id"] for h in kw["results"]] == [a["id"]]
    assert kw["results"][0]["score"] > 0

    sem = mem.search("alpha", mode="semantic")
    assert sem["mode_used"] == "semantic"
    assert sem["results"][0]["id"] == a["id"]

    hy = mem.search("alpha datalog", mode="hybrid")
    assert hy["mode_used"] == "hybrid"
    assert hy["results"][0]["id"] == a["id"]

    # "alphaz" has no FTS token match (stemming doesn't strip the z) but its
    # embedding maps to the alpha vector -> auto must fall back to hybrid
    auto = mem.search("alphaz")
    assert auto["mode_used"] == "hybrid"
    assert auto["results"][0]["id"] == a["id"]


def test_store_batch_atomic(mem):
    out = mem.store_batch(
        [{"text": "alpha one"}, {"text": "omega two", "meta": {"t": 2}, "id": "fixed"}]
    )
    assert out["count"] == 2 and "fixed" in out["ids"]
    assert mem.stats()["memories"] == 2

    with pytest.raises(Exception):
        mem.store_batch([{"text": "ok"}, {"text": "bad", "id": 123}])  # non-string id
    assert mem.stats()["memories"] == 2  # nothing from the failed batch


def test_update_reembeds_and_grows_history(mem):
    r = mem.store("alpha original", meta={"a": 1})
    before = mem.stats()["history_rows"]

    mem.update(r["id"], text="omega revised", meta={"b": 2})
    assert mem.stats()["history_rows"] == before + 1

    sem = mem.search("omega", mode="semantic")["results"]
    assert sem[0]["id"] == r["id"]  # re-embedded to the new text's vector
    got = mem.search("revised", mode="keyword")["results"]
    assert got[0]["meta"] == {"a": 1, "b": 2}  # meta merged

    mem.update(r["id"], meta={"c": 3})  # meta-only update keeps the embedding
    assert mem.search("omega", mode="semantic")["results"][0]["meta"]["c"] == 3


def test_delete_prunes_links_and_keeps_history(mem):
    clock = {"t": 1_700_000_000_000_000}
    mem._time_us = lambda: clock["t"]

    a = mem.store("alpha anchor")["id"]
    clock["t"] += 1_000_000
    b = mem.store("omega satellite")["id"]
    clock["t"] += 1_000_000
    mem.link(a, b)
    before_delete_us = clock["t"] + 1_000_000
    clock["t"] = before_delete_us + 1_000_000

    mem.delete(b)
    assert mem.delete(b) == {"deleted": False}
    assert mem.stats()["links"] == 0
    assert [h["id"] for h in mem.search("omega", mode="keyword")["results"]] == []

    from datetime import datetime, timezone

    iso = datetime.fromtimestamp(before_delete_us / 1e6, tz=timezone.utc).isoformat()
    past = mem.recall_as_of(iso)
    assert {m["id"] for m in past["memories"]} == {a, b}
    now_view = mem.recall_as_of(
        datetime.fromtimestamp((clock["t"] + 500_000) / 1e6, tz=timezone.utc).isoformat()
    )
    assert {m["id"] for m in now_view["memories"]} == {a}


def test_recall_as_of_between_updates(mem):
    clock = {"t": 1_700_000_000_000_000}
    mem._time_us = lambda: clock["t"]

    r = mem.store("alpha version one")
    t_mid = clock["t"] + 5_000_000
    clock["t"] = t_mid + 5_000_000
    mem.update(r["id"], text="alpha version two")

    from datetime import datetime, timezone

    iso_mid = datetime.fromtimestamp(t_mid / 1e6, tz=timezone.utc).isoformat()
    past = mem.recall_as_of(iso_mid)
    assert [m["text"] for m in past["memories"]] == ["alpha version one"]

    filtered = mem.recall_as_of(iso_mid, query="version")
    assert [m["text"] for m in filtered["memories"]] == ["alpha version one"]
    assert mem.recall_as_of(iso_mid, query="nomatch")["memories"] == []


def test_parse_when_guards():
    assert parse_when("2023-11-14T22:14:10Z").startswith("2023-11-14")
    assert parse_when("2021-06-01 12:00:00+00:00")
    with pytest.raises(ValueError, match="1970"):
        parse_when("1969-12-31T23:59:59Z")  # 0.12.2 engine would PANIC on this
    with pytest.raises(ValueError, match="ISO-8601"):
        parse_when("not a date")


def test_link_and_find_related(mem):
    a = mem.store("alpha root")["id"]
    b = mem.store("beta middle")["id"]
    c = mem.store("gamma far")["id"]
    d = mem.store("delta unrelated")["id"]
    mem.link(a, b, rel="follows")
    mem.link(b, c, rel="follows", weight=2.0)

    rel = mem.find_related(a, max_depth=2)["related"]
    assert [e["id"] for e in rel] == [b, c]
    assert rel[0]["depth"] == 1 and rel[0]["parent"] == a
    assert rel[1]["depth"] == 2 and rel[1]["parent"] == b

    only_one_hop = mem.find_related(a, max_depth=1)["related"]
    assert [e["id"] for e in only_one_hop] == [b]

    budget_one = mem.find_related(a, max_nodes=2, max_depth=3)["related"]
    assert len(budget_one) == 1  # budget covers seed + 1 node

    weighted = mem.find_related(a, max_depth=2, weighted=True)["related"]
    assert [e["id"] for e in weighted] == [b, c]
    assert weighted[1]["cost"] == pytest.approx(3.0)  # 1.0 + 2.0

    assert d not in [e["id"] for e in rel]
    with pytest.raises(ValueError, match="unknown memory"):
        mem.link(a, "missing-id")


def test_hybrid_explain_shows_graph_leg(mem):
    a = mem.store("alpha zebra report")["id"]
    b = mem.store("omega linked note")["id"]
    mem.store("omega distractor")
    mem.link(a, b)

    out = mem.search("zebra", explain=True)
    assert out["mode_used"] == "hybrid"
    assert "graph" in out["explain"]["legs"]
    by_id = {p["id"]: p for p in out["explain"]["per_result"]}
    assert b in by_id, "graph leg should surface the linked memory"
    assert any(c["leg"] == "graph" for c in by_id[b]["contributions"])
    result_ids = [r["id"] for r in out["results"]]
    assert result_ids and result_ids[0] == a


def test_list_recent_and_stats(mem):
    clock = {"t": 1_700_000_000_000_000}
    mem._time_us = lambda: clock["t"]
    ids = []
    for i, tok in enumerate(["alpha", "beta", "gamma"]):
        clock["t"] += 1_000_000
        ids.append(mem.store(f"{tok} item {i}")["id"])

    recent = mem.list_recent(2)["memories"]
    assert [m["id"] for m in recent] == [ids[2], ids[1]]

    s = mem.stats()
    assert s["memories"] == 3
    assert s["model"] == "test-keyword-2d" and s["dim"] == 2
    assert set(s["indices"]) == {"vec", "fts"}
    assert s["engine"] == "sqlite" and s["db_path"]


def test_hostile_queries_do_not_raise(mem):
    mem.store("alpha content")
    for q in ["cat AND (", 'unbalanced "quote', "NEAR^ * ;", "AND OR NOT", "  "]:
        mem.search(q)  # must not raise
    assert mem.search("", mode="keyword")["results"] == []
