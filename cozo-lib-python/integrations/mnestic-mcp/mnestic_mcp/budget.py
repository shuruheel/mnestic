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
- **What is measured**: UTF-8 bytes of the text block FastMCP actually puts
  on the wire for a dict result (`pydantic_core.to_json(..., indent=2)`), for
  the *final* response — truncation fields, token and note included. Measuring
  compact JSON in Python characters (the 0.2.0 behaviour) under-counted
  non-ASCII text ~3x and the indentation ~1.3x, so "fitting" responses could
  be several times the budget.
- **Cut at item boundaries only** — item content is never trimmed, so slice
  semantics equal full-result semantics restricted to a prefix.
- **The one exception to the cap**: a single item larger than the budget is
  returned alone, flagged `oversized_item: true` with its `response_bytes`,
  because an empty page with a token is an infinite loop waiting for a caller
  that doesn't check, and trimming would break prefix semantics. Every other
  response is <= `max_bytes`.
- **Continuations are in-process state** (this is a stdio server, one process
  per client), single-use, LRU-capped. After a restart a token is invalid and
  says so — the fix is to re-run the query, and the error tells you that.
"""

from __future__ import annotations

import secrets
from collections import OrderedDict
from typing import Any, Dict

import pydantic_core

DEFAULT_MAX_BYTES = 20_000
_MIN_BUDGET = 512  # below this the overhead dominates; clamp rather than thrash
_MAX_PENDING = 32

_pending: "OrderedDict[str, Dict[str, Any]]" = OrderedDict()


def wire_bytes(obj: Any) -> int:
    """UTF-8 size of `obj` exactly as FastMCP serializes a tool result into
    its text content block (`func_metadata._convert_to_content`). The protocol
    test pins this against a real client session so a FastMCP change that
    alters the encoding fails loudly instead of silently loosening the cap."""
    return len(pydantic_core.to_json(obj, fallback=str, indent=2))


def budget_slice(payload: dict, list_key: str, max_bytes: int) -> dict:
    """Return `payload` if it fits `max_bytes` on the wire; otherwise the
    longest prefix of `payload[list_key]` whose complete response fits, plus a
    `continuation` token for the rest. Keys other than `list_key` (mode_used,
    legs, …) ride along on every slice.
    """
    max_bytes = max(int(max_bytes), _MIN_BUDGET)
    if wire_bytes(payload) <= max_bytes:
        return payload
    items = list(payload.get(list_key) or [])
    if not items:
        # Nothing to slice: the overage is in the non-list keys. Serve as-is
        # rather than drop fields; no wide-result tool produces this shape.
        return payload

    base = {k: v for k, v in payload.items() if k != list_key}
    # Mint the token up front so the measured response is byte-identical to
    # the one returned (token length is fixed, but don't rely on it).
    token = secrets.token_urlsafe(9)

    def _resp(n: int) -> dict:
        return {
            **base,
            list_key: items[:n],
            "truncated": True,
            "remaining": len(items) - n,
            "continuation": token,
            "note": (
                f"byte budget ({max_bytes}) reached after {n} of {len(items)} items; "
                "call continue_result(token) for the next slice"
            ),
        }

    # Response size is monotone in the prefix length: binary-search the
    # largest n in [1, len-1] that fits (n == len already failed above).
    lo, hi = 0, len(items) - 1
    while lo < hi:
        mid = (lo + hi + 1) // 2
        if wire_bytes(_resp(mid)) <= max_bytes:
            lo = mid
        else:
            hi = mid - 1

    if lo == 0:
        # Even one item does not fit: serve it ALONE (never an empty page with
        # a token — an infinite loop waiting for a caller that doesn't check),
        # flagged with its real size, and park the tail.
        resp = {**base, list_key: items[:1], "oversized_item": True}
        if len(items) > 1:
            resp.update(
                truncated=True,
                remaining=len(items) - 1,
                continuation=token,
                note="oversized item served alone; call "
                "continue_result(token) for the rest",
            )
            _park(token, list_key, items[1:], base)
        # Self-referential size: iterate to the fixed point (digit count
        # changes at most a couple of times).
        resp["response_bytes"] = 0
        while resp["response_bytes"] != (n := wire_bytes(resp)):
            resp["response_bytes"] = n
        return resp

    _park(token, list_key, items[lo:], base)
    return _resp(lo)


def _park(token: str, list_key: str, rest: list, base: dict) -> None:
    _pending[token] = {"key": list_key, "items": rest, "base": base}
    while len(_pending) > _MAX_PENDING:
        _pending.popitem(last=False)


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
