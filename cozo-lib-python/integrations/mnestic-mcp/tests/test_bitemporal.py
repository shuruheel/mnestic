"""Two-axis recall: what was true at a valid time, according to what the store had
recorded by a transaction time, with the evidence for each answer.

Scenario: on day 1 the agent records that the user has lived in NYC since January
(from an onboarding form). Later a moving email reveals the user moved to SF on
February 15; the agent records a backdated correction. Questions:

  - Where did the user live on Feb 20?                     (now: SF)
  - Where did we THINK they lived on Feb 20, before the email? (NYC)
  - What changed, when was it recorded, and on what evidence?   (timeline)
"""

from __future__ import annotations

import time
from datetime import datetime, timezone

import pytest

from mnestic_mcp.memory import MemoryStore
from tests.conftest import KeywordEmbedder, make_memory

JAN1 = "2026-01-01T00:00:00Z"
JAN10 = "2026-01-10T00:00:00Z"
FEB15 = "2026-02-15T00:00:00Z"
FEB20 = "2026-02-20T00:00:00Z"


def _now_iso() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def _between_commits() -> str:
    """An instant strictly after the previous commit and before the next one."""
    time.sleep(0.005)
    t = _now_iso()
    time.sleep(0.005)
    return t


@pytest.fixture()
def moved(mem):
    mem.store("User lives in NYC", id="home", source="onboarding form", valid_from=JAN1)
    before_email = _between_commits()
    mem.update("home", text="User lives in SF", source="moving email", valid_from=FEB15)
    return mem, before_email


def test_valid_time_reflects_the_correction(moved):
    mem, _ = moved
    [m] = mem.recall_believed(valid_at=FEB20)["memories"]
    assert (m["text"], m["source"]) == ("User lives in SF", "moving email")
    assert m["valid_from"].startswith("2026-02-15")
    [m] = mem.recall_believed(valid_at=JAN10)["memories"]
    assert (m["text"], m["source"]) == ("User lives in NYC", "onboarding form")


def test_transaction_time_recovers_the_earlier_belief(moved):
    mem, before_email = moved
    [m] = mem.recall_believed(valid_at=FEB20, known_at=before_email)["memories"]
    assert (m["text"], m["source"]) == ("User lives in NYC", "onboarding form")
    # recorded_at is the engine's commit stamp, not the caller's valid_from
    assert m["recorded_at"] < before_email
    assert m["valid_from"].startswith("2026-01-01")


def test_nothing_was_known_before_the_first_write(moved):
    mem, _ = moved
    out = mem.recall_believed(valid_at=FEB20, known_at="2020-01-01T00:00:00Z")
    assert out["memories"] == []


def test_timeline_lists_every_belief_with_its_evidence(moved):
    mem, _ = moved
    ev = mem.memory_timeline("home")["events"]
    assert [(e["op"], e["text"], e["source"]) for e in ev] == [
        ("assert", "User lives in SF", "moving email"),
        ("assert", "User lives in NYC", "onboarding form"),
    ]
    assert ev[0]["recorded_at"] > ev[1]["recorded_at"]


def test_delete_retracts_going_forward_and_keeps_the_past(moved):
    mem, _ = moved
    before_delete = _between_commits()
    assert mem.delete("home", source="user asked to forget")["deleted"]
    assert mem.recall_believed()["memories"] == []
    # the past is still the past, and the pre-delete belief is still auditable
    [m] = mem.recall_believed(valid_at=FEB20)["memories"]
    assert m["text"] == "User lives in SF"
    [m] = mem.recall_believed(known_at=before_delete)["memories"]
    assert m["text"] == "User lives in SF"
    ops = [(e["op"], e["source"]) for e in mem.memory_timeline("home")["events"]]
    assert ops[0] == ("retract", "user asked to forget")


def test_query_filter(moved):
    mem, _ = moved
    assert mem.recall_believed(valid_at=FEB20, query="sf")["memories"]
    assert mem.recall_believed(valid_at=FEB20, query="nyc")["memories"] == []


def test_recorded_time_cannot_be_supplied(mem):
    # The audit guarantee lives in the engine: a caller cannot write a TxTime value.
    with pytest.raises(Exception):  # noqa: B017
        mem.db.run_script(
            "?[id, vt, tt, text, meta, source] <- "
            "[['x', 'ASSERT', [1000, true], 't', {}, '']] "
            ":put memory_belief {id, vt, tt => text, meta, source}",
            {},
            False,
        )


def test_existing_store_gains_the_belief_relation(tmp_path):
    mem = make_memory(tmp_path)
    mem.store("an old memory", id="old")
    mem.db.run_script("::remove memory_belief", {}, False)  # a store from before 0.3.0
    reopened = MemoryStore(mem.db, KeywordEmbedder(), db_path=mem.db_path, engine="sqlite")
    reopened.store("a new memory", id="new", source="chat")
    ids = {m["id"] for m in reopened.recall_believed()["memories"]}
    assert ids == {"new"}  # history before the upgrade stays in memories_hist
    assert reopened.stats()["belief_rows"] == 1


def test_bad_timestamp_is_rejected(mem):
    with pytest.raises(ValueError):
        mem.store("x", valid_from="next tuesday")
