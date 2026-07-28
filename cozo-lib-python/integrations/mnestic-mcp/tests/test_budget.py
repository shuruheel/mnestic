"""Byte-budget guard (`budget.py`): the roadmap's token-budgeted MCP output.
Unit tests on the slicer plus an end-to-end pass through a real MemoryStore
search payload."""

import json

from mnestic_mcp import budget


def _payload(n, text_bytes=200):
    return {
        "results": [
            {"id": f"m{i:04}", "text": "x" * text_bytes, "score": 1.0 - i / n}
            for i in range(n)
        ],
        "mode_used": "keyword",
    }


def _walk(payload, list_key, max_bytes):
    """Collect every slice by following continuations to exhaustion."""
    slices = [budget.budget_slice(payload, list_key, max_bytes)]
    while "continuation" in slices[-1]:
        slices.append(budget.continue_result(slices[-1]["continuation"], max_bytes))
    return slices


def test_under_budget_payload_is_untouched():
    p = _payload(3)
    assert budget.budget_slice(p, "results", 100_000) is p


def test_over_budget_is_cut_at_item_boundaries():
    p = _payload(50)
    out = budget.budget_slice(p, "results", 2_000)
    assert out["truncated"] is True
    assert 0 < len(out["results"]) < 50
    assert out["remaining"] == 50 - len(out["results"])
    # every slice fits the budget
    assert len(json.dumps(out, ensure_ascii=False)) <= 2_000 + 200  # note text slack
    # items are byte-identical prefixes, never trimmed
    assert out["results"] == p["results"][: len(out["results"])]
    assert out["mode_used"] == "keyword"


def test_continuation_walk_reassembles_everything_in_order():
    p = _payload(60)
    slices = _walk(p, "results", 2_000)
    assert len(slices) > 1
    reassembled = [it for s in slices for it in s["results"]]
    assert reassembled == p["results"]
    assert "continuation" not in slices[-1]


def test_tokens_are_single_use():
    p = _payload(50)
    out = budget.budget_slice(p, "results", 2_000)
    tok = out["continuation"]
    assert "error" not in budget.continue_result(tok, 2_000)
    again = budget.continue_result(tok, 2_000)
    assert "error" in again and "re-run" in again["hint"]


def test_unknown_token_is_a_clear_error():
    out = budget.continue_result("no-such-token")
    assert "error" in out


def test_single_oversized_item_is_served_not_looped():
    p = {"results": [{"id": "big", "text": "y" * 10_000}], "mode_used": "keyword"}
    out = budget.budget_slice(p, "results", 1_000)
    assert out["results"][0]["id"] == "big"
    assert out.get("oversized_item") is True
    assert "continuation" not in out


def test_oversized_head_with_tail_still_continues():
    p = {
        "results": [{"id": "big", "text": "y" * 10_000}]
        + [{"id": f"s{i}", "text": "z" * 50} for i in range(5)],
        "mode_used": "keyword",
    }
    slices = _walk(p, "results", 1_000)
    reassembled = [it for s in slices for it in s["results"]]
    assert [r["id"] for r in reassembled] == [r["id"] for r in p["results"]]


def test_pending_cap_evicts_oldest():
    before = budget.pending_count()
    for _ in range(40):
        budget.budget_slice(_payload(50), "results", 2_000)
    assert budget.pending_count() <= 32
    assert budget.pending_count() >= min(32, before)


def test_end_to_end_through_memory_store(mem):
    for i in range(30):
        mem.store(f"budget fixture memory number {i} " + "pad " * 40, id=f"b{i:02}")
    full = mem.search("budget fixture", k=30)
    n_full = len(full["results"])
    assert n_full >= 10
    slices = _walk(full, "results", 1_500)
    assert len(slices) > 1
    reassembled = [it for s in slices for it in s["results"]]
    assert [r["id"] for r in reassembled] == [r["id"] for r in full["results"]]
