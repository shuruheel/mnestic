"""Collision-safe namespace tuple <-> storage key encoding.

Every label is escaped and terminated with a separator, so:

- the encoding is injective for *any* label content (labels containing the
  separator, the escape, or dots all stay distinct — ``('a.b',)`` can never
  collide with ``('a', 'b')``);
- ``enc(p)`` is a string prefix of ``enc(ns)`` iff ``p`` is a tuple prefix of
  ``ns`` (the trailing separator stops ``('a',)`` from matching ``('ab',)``),
  which lets ``starts_with(ns, $prefix)`` implement ``namespace_prefix`` as a
  pushed-down key range scan in the engine.
"""

from __future__ import annotations

from typing import Any, List, Sequence, Tuple

SEP = "\x1f"
ESC = "\x1e"


def _esc(label: str) -> str:
    return label.replace(ESC, ESC + ESC).replace(SEP, ESC + "S")


def encode_ns(namespace: Sequence[str]) -> str:
    """Encode a namespace tuple as a sortable, prefix-searchable string key."""
    return "".join(_esc(label) + SEP for label in namespace)


def encode_prefix(prefix: Sequence[str]) -> str:
    """Encode a namespace-prefix tuple; `()` encodes to `""` (matches all)."""
    return encode_ns(prefix)


def decode_ns(encoded: str) -> Tuple[str, ...]:
    """Inverse of `encode_ns` (used for debugging/tests; reads use ns_parts)."""
    labels: List[str] = []
    cur: List[str] = []
    i = 0
    while i < len(encoded):
        ch = encoded[i]
        if ch == ESC:
            nxt = encoded[i + 1]
            cur.append(ESC if nxt == ESC else SEP)
            i += 2
        elif ch == SEP:
            labels.append("".join(cur))
            cur = []
            i += 1
        else:
            cur.append(ch)
            i += 1
    if cur:
        raise ValueError(f"truncated namespace encoding: {encoded!r}")
    return tuple(labels)


def matches_condition(condition: Any, namespace: Tuple[str, ...]) -> bool:
    """`ListNamespacesOp` match-condition semantics — a faithful port of the
    reference `_does_match` in `langgraph.store.memory` (prefix/suffix with
    `"*"` wildcards)."""
    match_type = condition.match_type
    path = condition.path
    if len(namespace) < len(path):
        return False
    if match_type == "prefix":
        pairs = zip(namespace, path)
    elif match_type == "suffix":
        pairs = zip(reversed(namespace), reversed(path))
    else:
        raise ValueError(f"Unsupported match type: {match_type}")
    for label, p_elem in pairs:
        if p_elem == "*":
            continue
        if label != p_elem:
            return False
    return True


def validate_namespace(namespace: Sequence[str]) -> Tuple[str, ...]:
    """Structural validation for ops arriving via `batch()` directly.

    Deliberately weaker than upstream's `put()`-level `_validate_namespace`
    (which also rejects periods): the encoding above makes any label content
    collision-safe, so `batch()` only enforces shape. The user-facing `put()`
    wrapper still applies upstream's stricter rules before we ever see the op.
    """
    ns = tuple(namespace)
    if not ns:
        raise ValueError("Namespace cannot be empty")
    for label in ns:
        if not isinstance(label, str) or not label:
            raise ValueError(f"Namespace labels must be non-empty strings, got {ns!r}")
    return ns
