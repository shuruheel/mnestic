"""Per-call byte budget with resumable continuation (ROADMAP: "token-budgeted
MCP output"): a wide result set must not be able to blow the caller's context
with no way back.

Every wide-result tool passes its payload through `budget_slice`, which cuts
the payload's list at item boundaries once the serialized size would exceed
`max_bytes`, and parks the remainder under a one-use continuation token. The
`continue_result` tool re-serves the remainder under the same budget rules —
possibly yielding a further token — so a caller walks an arbitrarily wide
result in bounded slices.

Design notes, stated for the tests to hold us to:
- **Cut at item boundaries only** — item content is never trimmed, so slice
  semantics equal full-result semantics restricted to a prefix.
- **Never return an empty slice**: a single item larger than the budget is
  returned alone (oversized, flagged), because an empty page with a token is
  an infinite loop waiting for a caller that doesn't check.
- **Continuations are in-process state** (this is a stdio server, one process
  per client), single-use, LRU-capped. After a restart a token is invalid and
  says so — the fix is to re-run the query, and the error tells you that.
"""

from __future__ import annotations

import json
import secrets
from collections import OrderedDict
from typing import Any, Dict

DEFAULT_MAX_BYTES = 20_000
_MIN_BUDGET = 512  # below this the overhead dominates; clamp rather than thrash
_MAX_PENDING = 32

_pending: "OrderedDict[str, Dict[str, Any]]" = OrderedDict()


def _size(obj: Any) -> int:
    return len(json.dumps(obj, ensure_ascii=False, separators=(",", ":")))


def budget_slice(payload: dict, list_key: str, max_bytes: int) -> dict:
    """Return `payload` if it fits `max_bytes` serialized; otherwise a prefix
    of `payload[list_key]` that fits, plus a `continuation` token for the rest.
    Keys other than `list_key` (mode_used, legs, …) ride along on every slice.
    """
    max_bytes = max(int(max_bytes), _MIN_BUDGET)
    items = payload.get(list_key) or []
    if _size(payload) <= max_bytes:
        return payload

    def _park(rest: list) -> str:
        token = secrets.token_urlsafe(9)
        _pending[token] = {"key": list_key, "items": rest, "base": base}
        while len(_pending) > _MAX_PENDING:
            _pending.popitem(last=False)
        return token

    base = {k: v for k, v in payload.items() if k != list_key}
    used = _size(base) + _size({list_key: [], "truncated": True, "remaining": 0}) + 64
    out = []
    for i, item in enumerate(items):
        sz = _size(item) + 1  # +1 for the list comma
        if used + sz > max_bytes:
            if not out:
                # The first item alone exceeds the budget: serve it ALONE
                # (never an empty page with a token — an infinite loop waiting
                # for a caller that doesn't check) and park the tail.
                rest = list(items[i + 1 :])
                resp = {**base, list_key: [item], "oversized_item": True}
                if rest:
                    resp.update(
                        truncated=True,
                        remaining=len(rest),
                        continuation=_park(rest),
                        note="oversized item served alone; call "
                        "continue_result(token) for the rest",
                    )
                return resp
            return {
                **base,
                list_key: out,
                "truncated": True,
                "remaining": len(items) - i,
                "continuation": _park(list(items[i:])),
                "note": (
                    f"byte budget ({max_bytes}) reached after {i} of {len(items)} items; "
                    "call continue_result(token) for the next slice"
                ),
            }
        out.append(item)
        used += sz
    return payload


def continue_result(token: str, max_bytes: int = DEFAULT_MAX_BYTES) -> dict:
    """Serve the next slice for a `continuation` token (single-use)."""
    entry = _pending.pop(token, None)
    if entry is None:
        return {
            "error": "unknown or expired continuation token",
            "hint": "tokens are single-use and do not survive a server restart; "
            "re-run the original query",
        }
    return budget_slice(
        {**entry["base"], entry["key"]: entry["items"]}, entry["key"], max_bytes
    )


def pending_count() -> int:  # for tests/stats
    return len(_pending)
