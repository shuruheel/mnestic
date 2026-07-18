"""Idempotent schema management for the two store relations + their indexes.

Layout (`{prefix}` defaults to `lg_store`):

- `{prefix}_items {ns, key => value, ns_parts, created_at, updated_at,
  expires_at, ttl_minutes}` — the item payloads. `ns` is the collision-safe
  encoded namespace (see `_ns.py`), `ns_parts` the original tuple so reads
  never decode. Timestamps are epoch seconds (Float); `expires_at` /
  `ttl_minutes` are nullable.
- `{prefix}_vecs {ns, key, field, seq => text[, emb]}` — one row per
  (item, indexed-field-path, text-instance). `text` feeds the BM25 index;
  `emb` exists only when the store is built with an IndexConfig. Both search
  legs run over this relation, so BM25 and vector search index identical text.

Indexes (created before any data — safe for continuous writes):
`{prefix}_vecs:sem` (HNSW, only with an IndexConfig) and `{prefix}_vecs:txt`
(FTS with Lowercase/Stemmer/Stopwords — note aggressive stopwording: queries
made only of common words like "hello" match nothing on the text leg).
"""

from __future__ import annotations

import re
from typing import Any, Optional

_IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*\Z")

ITEM_VALUE_COLS = "value, ns_parts, created_at, updated_at, expires_at, ttl_minutes"


def ensure_schema(
    db: Any,
    prefix: str,
    dims: Optional[int],
    distance: str,
    m: int,
    ef_construction: int,
) -> None:
    for name, val in (("relation_prefix", prefix), ("distance", distance)):
        if not _IDENT.match(val):
            raise ValueError(f"{name} must be a bare identifier, got {val!r}")

    items, vecs = f"{prefix}_items", f"{prefix}_vecs"
    rels = {r[0] for r in db.run_script("::relations", {}, True)["rows"]}

    if items not in rels:
        db.run_script(
            f":create {items} {{ns: String, key: String => value: Json, "
            f"ns_parts: [String], created_at: Float, updated_at: Float, "
            f"expires_at: Float?, ttl_minutes: Float?}}",
            {},
            False,
        )
    if vecs not in rels:
        emb = f", emb: <F32; {int(dims)}>" if dims else ""
        db.run_script(
            f":create {vecs} {{ns: String, key: String, field: String, seq: Int => "
            f"text: String{emb}}}",
            {},
            False,
        )
    else:
        _validate_vecs_columns(db, vecs, dims)

    idx = {r[0] for r in db.run_script(f"::indices {vecs}", {}, True)["rows"]}
    if dims and "sem" not in idx:
        db.run_script(
            f"::hnsw create {vecs}:sem {{dim: {int(dims)}, m: {int(m)}, dtype: F32, "
            f"fields: [emb], distance: {distance}, ef_construction: {int(ef_construction)}}}",
            {},
            False,
        )
    if "txt" not in idx:
        db.run_script(
            f"::fts create {vecs}:txt {{extractor: text, tokenizer: Simple, "
            f"filters: [Lowercase, Stemmer('English'), Stopwords('en')]}}",
            {},
            False,
        )


def _validate_vecs_columns(db: Any, vecs: str, dims: Optional[int]) -> None:
    """Loud error when a database is reopened with a mismatched IndexConfig —
    a dim mismatch must never surface as a runtime engine error mid-search."""
    rows = db.run_script(f"::columns {vecs}", {}, True)["rows"]
    emb_row = next((r for r in rows if r and r[0] == "emb"), None)
    if dims and emb_row is None:
        raise ValueError(
            f"relation {vecs} exists without an embedding column, but the store "
            "was configured with an IndexConfig. Use the same configuration the "
            "database was created with (or a fresh database/prefix)."
        )
    if not dims and emb_row is not None:
        raise ValueError(
            f"relation {vecs} has an embedding column, but the store was "
            "configured without an IndexConfig. Pass the original index "
            "configuration (dims + embed)."
        )
    if dims and emb_row is not None:
        type_str = " ".join(str(c) for c in emb_row)
        match = re.search(r"<F32\s*;\s*(\d+)\s*>", type_str)
        if match is None or int(match.group(1)) != int(dims):
            raise ValueError(
                f"relation {vecs} embedding column is {type_str!r}, which does "
                f"not match the configured IndexConfig dims={dims}. Embeddings "
                "from different models/dims cannot share one store."
            )
