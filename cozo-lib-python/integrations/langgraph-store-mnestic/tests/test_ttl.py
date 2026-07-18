"""TTL: lazy expiry on every read path, refresh-on-read, defaults, and the
physical sweeper. Uses the injectable clock — no sleeps."""

from tests.conftest import make_store


def _clocked(engine, tmp_path, ttl=None):
    s = make_store(engine, tmp_path, ttl=ttl)
    clock = {"t": 1000.0}
    s._now = lambda: clock["t"]
    return s, clock


def test_expiry_across_read_paths(tmp_path):
    s, clock = _clocked("sqlite", tmp_path)
    s.put(("t",), "ephemeral", {"text": "alpha fleeting"}, ttl=1.0)  # 1 minute
    s.put(("t",), "durable", {"text": "alpha lasting"})

    assert s.get(("t",), "ephemeral") is not None
    clock["t"] = 1000.0 + 61.0
    assert s.get(("t",), "ephemeral") is None
    assert s.get(("t",), "durable") is not None
    assert {i.key for i in s.search(("t",))} == {"durable"}
    assert {i.key for i in s.search(("t",), query="alpha")} == {"durable"}
    assert s.list_namespaces() == [("t",)]  # ns survives via the durable item
    s.close()


def test_expired_namespace_disappears(tmp_path):
    s, clock = _clocked("mem", tmp_path)
    s.put(("gone",), "only", {"v": 1}, ttl=1.0)
    assert s.list_namespaces() == [("gone",)]
    clock["t"] += 120.0
    assert s.list_namespaces() == []
    s.close()


def test_refresh_on_read_extends(tmp_path):
    s, clock = _clocked("sqlite", tmp_path, ttl={"refresh_on_read": True})
    s.put(("t",), "k", {"v": 1}, ttl=1.0)  # expires at t=1060

    clock["t"] = 1050.0
    item = s.get(("t",), "k", refresh_ttl=True)  # re-arms to 1050+60=1110
    assert item is not None
    clock["t"] = 1100.0
    assert s.get(("t",), "k") is not None  # would be gone without the refresh
    clock["t"] = 1200.0
    assert s.get(("t",), "k") is None
    s.close()


def test_refresh_false_does_not_extend(tmp_path):
    s, clock = _clocked("mem", tmp_path, ttl={"refresh_on_read": True})
    s.put(("t",), "k", {"v": 1}, ttl=1.0)
    clock["t"] = 1050.0
    assert s.get(("t",), "k", refresh_ttl=False) is not None
    clock["t"] = 1065.0
    assert s.get(("t",), "k") is None
    s.close()


def test_refresh_config_off_wins(tmp_path):
    s, clock = _clocked("mem", tmp_path, ttl={"refresh_on_read": False})
    s.put(("t",), "k", {"v": 1}, ttl=1.0)
    clock["t"] = 1050.0
    # base wrapper resolves refresh_ttl from config -> False; even a direct
    # refresh request must be a no-op because _apply_refresh checks the config
    assert s.get(("t",), "k", refresh_ttl=True) is not None
    clock["t"] = 1065.0
    assert s.get(("t",), "k") is None
    s.close()


def test_default_ttl_applied(tmp_path):
    s, clock = _clocked("mem", tmp_path, ttl={"default_ttl": 1.0, "refresh_on_read": False})
    s.put(("t",), "defaulted", {"v": 1})          # picks up default_ttl=1min
    s.put(("t",), "explicit_none", {"v": 2}, ttl=None)  # explicit None = no TTL
    clock["t"] = 1075.0
    assert s.get(("t",), "defaulted") is None
    assert s.get(("t",), "explicit_none") is not None
    s.close()


def test_refresh_preserves_value_and_updated_at(tmp_path):
    s, clock = _clocked("sqlite", tmp_path, ttl={"refresh_on_read": True})
    s.put(("t",), "k", {"payload": "alpha original"}, ttl=10.0)
    before = s.get(("t",), "k", refresh_ttl=False)
    clock["t"] = 1030.0
    refreshed = s.get(("t",), "k", refresh_ttl=True)
    assert refreshed.value == before.value
    assert refreshed.updated_at == before.updated_at  # refresh is not an update
    s.close()


def test_sweeper_removes_items_and_index_rows(tmp_path):
    s, clock = _clocked("sqlite", tmp_path)
    s.put(("t",), "dead", {"text": "alpha doomed"}, ttl=1.0)
    s.put(("t",), "alive", {"text": "alpha fine"})
    clock["t"] = 2000.0

    removed = s.sweep_ttl()
    assert removed == 1

    items = s.db.run_script(f"?[ns, key] := *{s._items}{{ns, key}}", {}, True)["rows"]
    vec_keys = {r[1] for r in s.db.run_script(f"?[ns, key] := *{s._vecs}{{ns, key}}", {}, True)["rows"]}
    assert [r[1] for r in items] == ["alive"]
    assert vec_keys == {"alive"}
    s.close()


def test_sweeper_thread_lifecycle(tmp_path):
    s, _clock = _clocked("mem", tmp_path, ttl={"sweep_interval_minutes": 60})
    s.start_ttl_sweeper()
    assert s._sweeper_thread is not None and s._sweeper_thread.is_alive()
    s.close()  # stops the sweeper
    assert s._sweeper_thread is None
