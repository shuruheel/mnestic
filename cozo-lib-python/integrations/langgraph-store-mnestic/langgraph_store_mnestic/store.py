"""`MnesticStore` — a LangGraph `BaseStore` backed by mnestic.

Semantics mirror the reference `InMemoryStore`: within one `batch()`, reads
(get/search/list_namespaces) observe the **pre-batch** state, and all puts are
deduplicated last-write-wins and applied at the end — here in **one engine
transaction** (`multi_transact`), so a batch's writes become visible atomically
or not at all. That atomicity is load-bearing: a non-atomic prototype lost 36%
of concurrent semantic reads under LangGraph's parallel fan-out.

`search(query=...)` runs an in-engine hybrid query: an HNSW vector leg (when
the store has an IndexConfig) and a BM25 text leg, both with the namespace
prefix pushed down into the index search, fused with Reciprocal Rank Fusion and
hydrated in the same script (single snapshot).
"""

from __future__ import annotations

import asyncio
import re
import threading
import time
from datetime import datetime, timezone
from typing import Any, Dict, Iterable, List, Optional, Sequence, Tuple

from langgraph.store.base import (
    BaseStore,
    GetOp,
    IndexConfig,
    Item,
    ListNamespacesOp,
    Op,
    PutOp,
    Result,
    SearchItem,
    SearchOp,
    TTLConfig,
    ensure_embeddings,
    get_text_at_path,
)

from ._filters import item_matches
from ._ns import encode_ns, encode_prefix, matches_condition, validate_namespace
from ._schema import ensure_schema

_WORD = re.compile(r"\w+", re.UNICODE)


def _dt(epoch: float) -> datetime:
    return datetime.fromtimestamp(epoch, tz=timezone.utc)


class MnesticStore(BaseStore):
    """LangGraph store over an embedded mnestic database.

    Args:
        db: an existing ``mnestic.CozoDbPy`` handle; if omitted, one is opened
            from ``engine``/``path``/``options``.
        index: LangGraph ``IndexConfig`` (``dims`` + ``embed`` + optional
            ``fields``) enabling the vector leg of ``search``. Without it the
            store is BM25-only (keyword search still works).
        ttl: LangGraph ``TTLConfig``. TTL is always supported; the config
            supplies ``default_ttl`` / ``refresh_on_read`` /
            ``sweep_interval_minutes`` defaults.
    """

    supports_ttl = True

    def __init__(
        self,
        db: Optional[Any] = None,
        *,
        engine: str = "mem",
        path: str = "",
        options: str = "{}",
        index: Optional[IndexConfig] = None,
        ttl: Optional[TTLConfig] = None,
        relation_prefix: str = "lg_store",
        distance: str = "Cosine",
        m: int = 16,
        ef_construction: int = 50,
        ef_search: int = 50,
        rrf_k: float = 60.0,
        overfetch: int = 4,
        create: bool = True,
    ) -> None:
        self._owns_db = db is None
        if db is None:
            from mnestic import CozoDbPy

            db = CozoDbPy(engine, path, options)
        self.db = db
        self.index_config = index
        self.ttl_config = ttl
        self.ef_search = int(ef_search)
        self.rrf_k = float(rrf_k)
        self.overfetch = max(1, int(overfetch))

        self._embeddings = None
        self._dims: Optional[int] = None
        self._fields: List[str] = ["$"]
        if index is not None:
            self._dims = int(index["dims"])
            if self._dims <= 0:
                raise ValueError(f"index['dims'] must be positive, got {self._dims}")
            self._embeddings = ensure_embeddings(index["embed"])
            self._fields = list(index.get("fields") or ["$"])

        self._items = f"{relation_prefix}_items"
        self._vecs = f"{relation_prefix}_vecs"
        if create:
            ensure_schema(db, relation_prefix, self._dims, distance, m, ef_construction)

        self._now = time.time  # injectable clock (tests)
        self._sweeper_stop: Optional[threading.Event] = None
        self._sweeper_thread: Optional[threading.Thread] = None

    # --- BaseStore contract ---

    def batch(self, ops: Iterable[Op]) -> List[Result]:
        ops = list(ops)
        results: List[Result] = []
        put_ops: Dict[Tuple[Tuple[str, ...], str], PutOp] = {}
        refresh: List[List[str]] = []

        for op in ops:
            if isinstance(op, GetOp):
                results.append(self._get_op(op, refresh))
            elif isinstance(op, SearchOp):
                results.append(self._search_op(op, refresh))
            elif isinstance(op, ListNamespacesOp):
                results.append(self._list_namespaces_op(op))
            elif isinstance(op, PutOp):
                ns = validate_namespace(op.namespace)
                if op.value is not None and not isinstance(op.value, dict):
                    raise TypeError(f"Item values must be dicts, got {type(op.value)}")
                if op.ttl is not None and self.ttl_config is None and not self.supports_ttl:
                    raise NotImplementedError("TTL is not supported by this store")
                put_ops[(ns, op.key)] = op
                results.append(None)
            else:
                raise ValueError(f"Unknown operation type: {type(op)}")

        if put_ops:
            self._apply_puts(put_ops)
        if refresh:
            self._apply_refresh(refresh)
        return results

    async def abatch(self, ops: Iterable[Op]) -> List[Result]:
        ops = list(ops)
        return await asyncio.get_running_loop().run_in_executor(None, self.batch, ops)

    # --- write path ---

    def _apply_puts(self, put_ops: Dict[Tuple[Tuple[str, ...], str], PutOp]) -> None:
        now = self._now()
        upserts: List[Tuple[str, str, PutOp]] = []  # (ns_enc, key, op)
        deletes: List[List[str]] = []
        pending_texts: List[Tuple[int, str, int, str]] = []  # (upsert_idx, field, seq, text)

        for (ns, key), op in put_ops.items():
            ns_enc = encode_ns(ns)
            if op.value is None:
                deletes.append([ns_enc, key])
                continue
            idx = len(upserts)
            upserts.append((ns_enc, key, op))
            fields = self._op_fields(op)
            if fields is not None:
                for field in fields:
                    for seq, text in enumerate(get_text_at_path(op.value, field)):
                        pending_texts.append((idx, field, seq, text))

        # Embed the whole batch in one call, BEFORE any transaction is open —
        # never hold the write locks across a (possibly remote) embedding call.
        embeddings: List[List[float]] = []
        if pending_texts and self._embeddings is not None:
            embeddings = self._embeddings.embed_documents([t[3] for t in pending_texts])
            for vec in embeddings:
                if len(vec) != self._dims:
                    raise ValueError(
                        f"embedding dimension {len(vec)} != configured dims {self._dims}"
                    )

        affected_keys = [[e, k] for e, k, _ in upserts] + deletes
        vec_cols = "text, emb" if self._dims else "text"
        vec_rows: List[list] = []
        for i, (pidx, field, seq, text) in enumerate(pending_texts):
            ns_enc, key, _op = upserts[pidx]
            row: list = [ns_enc, key, field, seq, text]
            if self._dims:
                row.append([float(x) for x in embeddings[i]])
            vec_rows.append(row)

        tx = self.db.multi_transact(True)
        try:
            if upserts:
                res = tx.run_script(
                    f"keyset[ns, key] <- $keys\n"
                    f"?[ns, key, created_at] := keyset[ns, key], "
                    f"*{self._items}{{ns, key, created_at}}",
                    {"keys": [[e, k] for e, k, _ in upserts]},
                )
                existing = {(r[0], r[1]): float(r[2]) for r in res["rows"]}
                item_rows = []
                for ns_enc, key, op in upserts:
                    ttl_minutes = float(op.ttl) if op.ttl is not None else None
                    expires_at = (now + ttl_minutes * 60.0) if ttl_minutes is not None else None
                    item_rows.append(
                        [
                            ns_enc,
                            key,
                            op.value,
                            list(op.namespace),
                            existing.get((ns_enc, key), now),
                            now,
                            expires_at,
                            ttl_minutes,
                        ]
                    )
                tx.run_script(
                    f"?[ns, key, value, ns_parts, created_at, updated_at, expires_at, ttl_minutes] <- $rows\n"
                    f":put {self._items} {{ns, key => value, ns_parts, created_at, "
                    f"updated_at, expires_at, ttl_minutes}}",
                    {"rows": item_rows},
                )
            if deletes:
                # Items before vecs everywhere, so concurrent transactions
                # acquire the two relation locks in one global order.
                tx.run_script(
                    f"?[ns, key] <- $keys :rm {self._items} {{ns, key}}",
                    {"keys": deletes},
                )
            if affected_keys:
                tx.run_script(
                    f"keyset[ns, key] <- $keys\n"
                    f"?[ns, key, field, seq] := keyset[ns, key], "
                    f"*{self._vecs}{{ns, key, field, seq}}\n"
                    f":rm {self._vecs} {{ns, key, field, seq}}",
                    {"keys": affected_keys},
                )
            if vec_rows:
                tx.run_script(
                    f"?[ns, key, field, seq, {vec_cols}] <- $rows\n"
                    f":put {self._vecs} {{ns, key, field, seq => {vec_cols}}}",
                    {"rows": vec_rows},
                )
            tx.commit()
        except BaseException:
            try:
                tx.abort()
            except Exception:
                pass
            raise

    def _op_fields(self, op: PutOp) -> Optional[List[str]]:
        """Which field paths to index for this op; None = don't index at all."""
        if op.index is False:
            return None
        if op.index is not None:
            return list(op.index)
        return self._fields

    # --- read path ---

    def _get_op(self, op: GetOp, refresh: List[List[str]]) -> Optional[Item]:
        ns = tuple(op.namespace)
        ns_enc = encode_ns(ns)
        rows = self.db.run_script(
            f"?[value, ns_parts, created_at, updated_at, expires_at, ttl_minutes] := "
            f"ns = $ns, key = $key, "
            f"*{self._items}{{ns, key, value, ns_parts, created_at, updated_at, "
            f"expires_at, ttl_minutes}}",
            {"ns": ns_enc, "key": op.key},
            True,
        )["rows"]
        if not rows:
            return None
        value, ns_parts, ca, ua, ea, tm = rows[0]
        if ea is not None and ea <= self._now():
            return None
        if op.refresh_ttl and tm is not None:
            refresh.append([ns_enc, op.key])
        return Item(
            value=value,
            key=op.key,
            namespace=tuple(ns_parts),
            created_at=_dt(ca),
            updated_at=_dt(ua),
        )

    def _search_op(self, op: SearchOp, refresh: List[List[str]]) -> List[SearchItem]:
        nsp = encode_prefix(tuple(op.namespace_prefix or ()))
        now = self._now()
        if op.query:
            candidates = self._query_candidates(op, nsp, now)
        else:
            candidates = self._scan_candidates(op, nsp, now)

        picked = candidates[op.offset : op.offset + op.limit]
        out: List[SearchItem] = []
        for ns_enc, key, score, value, ns_parts, ca, ua, tm in picked:
            if op.refresh_ttl and tm is not None:
                refresh.append([ns_enc, key])
            out.append(
                SearchItem(
                    namespace=tuple(ns_parts),
                    key=key,
                    value=value,
                    created_at=_dt(ca),
                    updated_at=_dt(ua),
                    score=score,
                )
            )
        return out

    def _query_candidates(self, op: SearchOp, nsp: str, now: float) -> List[tuple]:
        # Lowercased word tokens only: bare uppercase AND/OR/NOT/NEAR are FTS
        # operators, and raw punctuation can fail the FTS query grammar.
        # Comma-join = OR-of-terms (whitespace would be AND): docs matching
        # more query terms rank higher under BM25 summing, and a single rare
        # term still recalls.
        qt = ", ".join(t.lower() for t in _WORD.findall(op.query or ""))
        have_sem = self._embeddings is not None
        have_txt = bool(qt)
        if not have_sem and not have_txt:
            return []

        fetch = min(max((op.offset + op.limit) * self.overfetch, 10), 1000)
        params: Dict[str, Any] = {"nsp": nsp}
        if have_sem:
            qv = self._embeddings.embed_query(op.query)
            if len(qv) != self._dims:
                raise ValueError(
                    f"query embedding dimension {len(qv)} != configured dims {self._dims}"
                )
            params["qv"] = [float(x) for x in qv]
        if have_txt:
            params["qt"] = qt

        rows = self.db.run_script(self._fused_script(have_sem, have_txt, fetch), params, True)[
            "rows"
        ]
        out = []
        for ns_enc, key, score, value, ns_parts, ca, ua, ea, tm in rows:
            if ea is not None and ea <= now:
                continue
            if not item_matches(value, op.filter):
                continue
            out.append((ns_enc, key, float(score), value, ns_parts, ca, ua, tm))
        return out

    def _fused_script(self, have_sem: bool, have_txt: bool, fetch: int) -> str:
        parts: List[str] = []
        if have_sem:
            parts.append(
                f"sem[id, score] := ~{self._vecs}:sem{{ns: n, key: k | query: vec($qv), "
                f"k: {fetch}, ef: {max(self.ef_search, fetch)}, bind_distance: d, "
                f"filter: starts_with(n, $nsp)}}, id = [n, k], score = -d"
            )
            parts.append("combined[lid, id, score] := sem[id, score], lid = 'semantic'")
        if have_txt:
            parts.append(
                f"txt[id, score] := ~{self._vecs}:txt{{ns: n, key: k | query: $qt, "
                f"k: {fetch}, bind_score: s, filter: starts_with(n, $nsp)}}, "
                f"id = [n, k], score = s"
            )
            parts.append("combined[lid, id, score] := txt[id, score], lid = 'text'")
        parts.append(
            f"fused[id, score] <~ ReciprocalRankFusion(combined[lid, id, score], k: {self.rrf_k})"
        )
        # Hydrate in the same script: one snapshot for legs, fusion, and items.
        parts.append(
            f"?[ns, key, score, value, ns_parts, created_at, updated_at, expires_at, ttl_minutes] := "
            f"fused[id, score], ns = get(id, 0), key = get(id, 1), "
            f"*{self._items}{{ns, key, value, ns_parts, created_at, updated_at, "
            f"expires_at, ttl_minutes}}"
        )
        parts.append(":order -score")
        parts.append(f":limit {fetch}")
        return "\n".join(parts)

    def _scan_candidates(self, op: SearchOp, nsp: str, now: float) -> List[tuple]:
        script = (
            f"?[ns, key, value, ns_parts, created_at, updated_at, expires_at, ttl_minutes] := "
            f"*{self._items}{{ns, key, value, ns_parts, created_at, updated_at, "
            f"expires_at, ttl_minutes}}, "
            f"starts_with(ns, $nsp), "
            f"if(is_null(expires_at), true, expires_at > $now)\n"
            f":order ns, key"
        )
        if not op.filter:
            script += f"\n:limit {op.offset + op.limit}"
        rows = self.db.run_script(script, {"nsp": nsp, "now": now}, True)["rows"]
        out = []
        for ns_enc, key, value, ns_parts, ca, ua, _ea, tm in rows:
            if not item_matches(value, op.filter):
                continue
            out.append((ns_enc, key, None, value, ns_parts, ca, ua, tm))
        return out

    def _list_namespaces_op(self, op: ListNamespacesOp) -> List[Tuple[str, ...]]:
        rows = self.db.run_script(
            f"?[ns, ns_parts] := *{self._items}{{ns, ns_parts, expires_at}}, "
            f"if(is_null(expires_at), true, expires_at > $now)",
            {"now": self._now()},
            True,
        )["rows"]
        namespaces = {tuple(parts) for _, parts in rows}
        result: List[Tuple[str, ...]] = list(namespaces)
        if op.match_conditions:
            result = [
                ns
                for ns in result
                if all(matches_condition(cond, ns) for cond in op.match_conditions)
            ]
        if op.max_depth is not None:
            result = sorted({ns[: op.max_depth] for ns in result})
        else:
            result = sorted(result)
        return result[op.offset : op.offset + op.limit]

    # --- TTL ---

    def _apply_refresh(self, keys: List[List[str]]) -> None:
        """Extend expiry for read items — post-batch, in one atomic script that
        re-reads each row in-engine (never clobbers a concurrent value update)."""
        if self.ttl_config is not None and not self.ttl_config.get("refresh_on_read", True):
            return
        self.db.run_script(
            f"keyset[ns, key] <- $keys\n"
            f"?[ns, key, value, ns_parts, created_at, updated_at, expires_at, ttl_minutes] := "
            f"keyset[ns, key], "
            f"*{self._items}{{ns, key, value, ns_parts, created_at, updated_at, ttl_minutes}}, "
            f"!is_null(ttl_minutes), "
            f"expires_at = $now + coalesce(ttl_minutes, 0.0) * 60.0\n"
            f":put {self._items} {{ns, key => value, ns_parts, created_at, updated_at, "
            f"expires_at, ttl_minutes}}",
            {"keys": keys, "now": self._now()},
            False,
        )

    def sweep_ttl(self) -> int:
        """Physically remove expired items (and their index rows). Returns the
        number of removed items. Correctness never depends on this — every read
        path filters expired rows — but it reclaims space."""
        now = self._now()
        tx = self.db.multi_transact(True)
        try:
            rows = tx.run_script(
                f"?[ns, key] := *{self._items}{{ns, key, expires_at}}, "
                f"if(is_null(expires_at), false, expires_at <= $now)",
                {"now": now},
            )["rows"]
            if rows:
                keys = [[r[0], r[1]] for r in rows]
                tx.run_script(
                    f"?[ns, key] <- $keys :rm {self._items} {{ns, key}}", {"keys": keys}
                )
                tx.run_script(
                    f"keyset[ns, key] <- $keys\n"
                    f"?[ns, key, field, seq] := keyset[ns, key], "
                    f"*{self._vecs}{{ns, key, field, seq}}\n"
                    f":rm {self._vecs} {{ns, key, field, seq}}",
                    {"keys": keys},
                )
            tx.commit()
            return len(rows)
        except BaseException:
            try:
                tx.abort()
            except Exception:
                pass
            raise

    def start_ttl_sweeper(self) -> None:
        if self._sweeper_thread is not None:
            return
        interval_min = 5.0
        if self.ttl_config is not None:
            interval_min = float(self.ttl_config.get("sweep_interval_minutes") or 5.0)
        stop = threading.Event()

        def _loop() -> None:
            while not stop.wait(interval_min * 60.0):
                try:
                    self.sweep_ttl()
                except Exception:  # pragma: no cover - keep the sweeper alive
                    pass

        thread = threading.Thread(target=_loop, name="mnestic-store-ttl-sweeper", daemon=True)
        self._sweeper_stop = stop
        self._sweeper_thread = thread
        thread.start()

    def stop_ttl_sweeper(self) -> None:
        if self._sweeper_stop is not None:
            self._sweeper_stop.set()
        if self._sweeper_thread is not None:
            self._sweeper_thread.join(timeout=5.0)
        self._sweeper_stop = None
        self._sweeper_thread = None

    # --- lifecycle ---

    def close(self) -> None:
        self.stop_ttl_sweeper()

    def __enter__(self) -> "MnesticStore":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()
