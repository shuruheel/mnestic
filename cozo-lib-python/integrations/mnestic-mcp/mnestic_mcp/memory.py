"""`MemoryStore` — all engine access for the MCP server. Embedder- and
MCP-agnostic (takes an injected embedder; tests use a deterministic one).

Storage layout (targets the published mnestic 0.12.2 wheel):

- `memories {id => text, meta, created_at, updated_at, emb}` — current state,
  with HNSW (`:vec`) + FTS (`:fts`) indexes created at init, before any data.
- `memories_hist {id, at: Validity => text, meta, created_at}` — valid-time
  history, app-stamped integer MICROseconds (a per-process monotonic clock).
  Updates assert a new interval; deletes assert a retraction — `recall_as_of`
  reads `@ t`. Timestamps are validated `>= 0` in Python: the 0.12.2 engine
  panics on pre-epoch validity stamps.
- `links {src, dst, rel => weight, created_at}` — typed weighted edges,
  mechanism-named rels. Feeds the hybrid graph leg and `find_related`.
- `db_meta {k => v}` — schema version + embedding-model pinning (a db built
  with one model must never silently mix embedding spaces with another).

Every multi-statement write is one `{...} {...}` script = one atomic engine
transaction (verified: a failing later block rolls back the earlier ones).
"""

from __future__ import annotations

import re
import threading
import time
import uuid
from datetime import datetime, timezone
from typing import Any, Dict, List, Optional

SCHEMA_VERSION = "1"

_WORD = re.compile(r"\w+", re.UNICODE)


def _iso(us: int) -> str:
    return datetime.fromtimestamp(us / 1e6, tz=timezone.utc).isoformat()


def _fts_query(text: str) -> str:
    # Lowercased word tokens, comma-joined = OR-of-terms (whitespace would be
    # AND; bare uppercase AND/OR/NOT/NEAR are operators; punctuation can fail
    # the FTS grammar). Note the index stopwords very common English words.
    return ", ".join(t.lower() for t in _WORD.findall(text or ""))


def parse_when(t: str) -> str:
    """Validate an ISO-8601 timestamp and return it in the form the engine's
    validity parser accepts. Pre-epoch instants are rejected in Python — the
    0.12.2 engine panics on them."""
    raw = (t or "").strip()
    if raw.endswith("Z"):
        raw = raw[:-1] + "+00:00"
    try:
        dt = datetime.fromisoformat(raw)
    except ValueError as e:
        raise ValueError(f"not an ISO-8601 timestamp: {t!r} ({e})") from None
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    if dt.timestamp() < 0:
        raise ValueError(f"timestamps before 1970-01-01 are not supported (got {t!r})")
    return dt.isoformat().replace("+00:00", "Z")


class MemoryStore:
    def __init__(
        self,
        db: Any,
        embedder: Any,
        *,
        db_path: str = "",
        engine: str = "",
        rrf_k: float = 60.0,
    ) -> None:
        self.db = db
        self.embedder = embedder
        self.db_path = db_path
        self.engine = engine
        self.rrf_k = float(rrf_k)
        self._clock_lock = threading.Lock()
        self._last_us = 0
        self._time_us = lambda: time.time_ns() // 1000  # injectable (tests)
        self.ensure_schema()

    # --- clock ---

    def _now_us(self) -> int:
        with self._clock_lock:
            t = int(self._time_us())
            if t < 0:
                raise ValueError("system clock reports a pre-1970 time; refusing to write")
            if t <= self._last_us:
                t = self._last_us + 1
            self._last_us = t
            return t

    # --- schema ---

    def ensure_schema(self) -> None:
        dim = int(self.embedder.dim)
        rels = {r[0] for r in self.db.run_script("::relations", {}, True)["rows"]}
        if "memories" not in rels:
            self.db.run_script(
                f":create memories {{id: String => text: String, meta: Json, "
                f"created_at: Int, updated_at: Int, emb: <F32; {dim}>}}",
                {},
                False,
            )
        if "memories_hist" not in rels:
            self.db.run_script(
                ":create memories_hist {id: String, at: Validity => text: String, "
                "meta: Json, created_at: Int}",
                {},
                False,
            )
        if "links" not in rels:
            self.db.run_script(
                ":create links {src: String, dst: String, rel: String => "
                "weight: Float, created_at: Int}",
                {},
                False,
            )
        if "db_meta" not in rels:
            self.db.run_script(":create db_meta {k: String => v: String}", {}, False)

        idx = {r[0] for r in self.db.run_script("::indices memories", {}, True)["rows"]}
        if "vec" not in idx:
            self.db.run_script(
                f"::hnsw create memories:vec {{dim: {dim}, m: 16, dtype: F32, "
                f"fields: [emb], distance: Cosine, ef_construction: 50}}",
                {},
                False,
            )
        if "fts" not in idx:
            self.db.run_script(
                "::fts create memories:fts {extractor: text, tokenizer: Simple, "
                "filters: [Lowercase, Stemmer('English'), Stopwords('en')]}",
                {},
                False,
            )
        self._pin_model(dim)

    def _pin_model(self, dim: int) -> None:
        rows = self.db.run_script("?[k, v] := *db_meta{k, v}", {}, True)["rows"]
        meta = {k: v for k, v in rows}
        model = getattr(self.embedder, "model_name", "unknown")
        if not meta:
            self.db.run_script(
                "?[k, v] <- $rows :put db_meta {k => v}",
                {
                    "rows": [
                        ["schema_version", SCHEMA_VERSION],
                        ["model_name", model],
                        ["model_dim", str(dim)],
                    ]
                },
                False,
            )
            return
        pinned_model, pinned_dim = meta.get("model_name"), meta.get("model_dim")
        if pinned_model != model or pinned_dim != str(dim):
            raise ValueError(
                f"this database was built with embedding model {pinned_model!r} "
                f"({pinned_dim}-d); you asked for {model!r} ({dim}-d). Embedding "
                "spaces cannot be mixed — use a different --db, or re-store your "
                "memories with the new model."
            )

    # --- writes ---

    def store(self, text: str, meta: Optional[dict] = None, id: Optional[str] = None) -> dict:
        out = self.store_batch([{"text": text, "meta": meta or {}, "id": id}])
        return {"id": out["ids"][0], "created_at": out["created_at"]}

    def store_batch(self, items: List[dict]) -> dict:
        if not items:
            return {"ids": [], "count": 0, "created_at": None}
        texts = [str(it["text"]) for it in items]
        ids = [it.get("id") or uuid.uuid4().hex for it in items]
        metas = [dict(it.get("meta") or {}) for it in items]
        embs = self.embedder.embed_documents(texts)
        t = self._now_us()
        rows = [[i, tx, m, t, t, e] for i, tx, m, e in zip(ids, texts, metas, embs)]
        hrows = [[i, [t, True], tx, m, t] for i, tx, m in zip(ids, texts, metas)]
        self.db.run_script(
            "{?[id, text, meta, created_at, updated_at, emb] <- $rows "
            ":put memories {id => text, meta, created_at, updated_at, emb}} "
            "{?[id, at, text, meta, created_at] <- $hrows "
            ":put memories_hist {id, at => text, meta, created_at}}",
            {"rows": rows, "hrows": hrows},
            False,
        )
        return {"ids": ids, "count": len(ids), "created_at": _iso(t)}

    def update(
        self, id: str, text: Optional[str] = None, meta: Optional[dict] = None
    ) -> dict:
        cur = self._fetch([id])
        if id not in cur:
            raise ValueError(f"no memory with id {id!r}")
        cur_text, cur_meta, created_at = cur[id]
        new_text = cur_text if text is None else str(text)
        new_meta = {**cur_meta, **(meta or {})}
        if text is not None and new_text != cur_text:
            emb = self.embedder.embed_documents([new_text])[0]
        else:
            emb = None
        t = self._now_us()
        blocks = [
            "{?[id, at, text, meta, created_at] <- $hrows "
            ":put memories_hist {id, at => text, meta, created_at}}"
        ]
        params: Dict[str, Any] = {
            "hrows": [[id, [t, True], new_text, new_meta, created_at]],
        }
        if emb is not None:
            blocks.insert(
                0,
                "{?[id, text, meta, created_at, updated_at, emb] <- $rows "
                ":put memories {id => text, meta, created_at, updated_at, emb}}",
            )
            params["rows"] = [[id, new_text, new_meta, created_at, t, emb]]
        else:
            blocks.insert(
                0,
                "{keep[id, emb] := id = $id, *memories{id, emb} "
                "?[id, text, meta, created_at, updated_at, emb] := keep[id, emb], "
                "text = $text, meta = $meta, created_at = $ca, updated_at = $ua "
                ":put memories {id => text, meta, created_at, updated_at, emb}}",
            )
            params.update(
                {"id": id, "text": new_text, "meta": new_meta, "ca": created_at, "ua": t}
            )
        self.db.run_script(" ".join(blocks), params, False)
        return {"id": id, "updated_at": _iso(t)}

    def delete(self, id: str) -> dict:
        existed = id in self._fetch([id])
        if not existed:
            return {"deleted": False}
        t = self._now_us()
        self.db.run_script(
            "{?[id] <- [[$id]] :rm memories {id}} "
            "{?[id, at, text, meta, created_at] <- [[$id, [$t, false], '', {}, $t]] "
            ":put memories_hist {id, at => text, meta, created_at}} "
            "{?[src, dst, rel] := *links{src, dst, rel}, (src == $id or dst == $id) "
            ":rm links {src, dst, rel}}",
            {"id": id, "t": t},
            False,
        )
        return {"deleted": True}

    def link(self, src: str, dst: str, rel: str = "relates_to", weight: float = 1.0) -> dict:
        present = self._fetch([src, dst])
        missing = [x for x in (src, dst) if x not in present]
        if missing:
            raise ValueError(f"unknown memory id(s): {missing}")
        self.db.run_script(
            "?[src, dst, rel, weight, created_at] <- [[$src, $dst, $rel, $w, $t]] "
            ":put links {src, dst, rel => weight, created_at}",
            {"src": src, "dst": dst, "rel": str(rel), "w": float(weight), "t": self._now_us()},
            False,
        )
        return {"linked": True}

    # --- reads ---

    def _fetch(self, ids: List[str]) -> Dict[str, tuple]:
        rows = self.db.run_script(
            "idset[id] <- $ids "
            "?[id, text, meta, created_at] := idset[id], *memories{id, text, meta, created_at}",
            {"ids": [[i] for i in ids]},
            True,
        )["rows"]
        return {r[0]: (r[1], r[2], r[3]) for r in rows}

    def _row_out(self, id: str, text: str, meta: dict, created_at: int, score=None) -> dict:
        out = {"id": id, "text": text, "meta": meta, "created_at": _iso(created_at)}
        if score is not None:
            out["score"] = float(score)
        return out

    def search(
        self,
        query: str,
        k: int = 8,
        mode: str = "auto",
        explain: bool = False,
        expand_graph: bool = True,
    ) -> dict:
        if mode not in ("auto", "keyword", "semantic", "hybrid"):
            raise ValueError(f"unknown mode {mode!r}")
        k = max(1, min(int(k), 100))
        if explain:
            mode = "hybrid"

        if mode in ("auto", "keyword"):
            results = self._search_keyword(query, k)
            if mode == "keyword" or results:
                return {"results": results, "mode_used": "keyword"}
            mode = "hybrid"  # auto fallback

        if mode == "semantic":
            return {"results": self._search_semantic(query, k), "mode_used": "semantic"}

        return self._search_hybrid(query, k, explain=explain, expand_graph=expand_graph)

    def _search_keyword(self, query: str, k: int) -> List[dict]:
        qt = _fts_query(query)
        if not qt:
            return []
        rows = self.db.run_script(
            f"?[id, text, meta, created_at, score] := "
            f"~memories:fts{{id, text, meta, created_at | query: $q, k: {k}, bind_score: score}}\n"
            f":order -score\n:limit {k}",
            {"q": qt},
            True,
        )["rows"]
        return [self._row_out(r[0], r[1], r[2], r[3], r[4]) for r in rows]

    def _search_semantic(self, query: str, k: int) -> List[dict]:
        qv = self.embedder.embed_query(query)
        rows = self.db.run_script(
            f"?[id, text, meta, created_at, dist] := "
            f"~memories:vec{{id, text, meta, created_at | query: vec($qv), k: {k}, "
            f"ef: {max(50, 2 * k)}, bind_distance: dist}}\n"
            f":order dist\n:limit {k}",
            {"qv": [float(x) for x in qv]},
            True,
        )["rows"]
        return [self._row_out(r[0], r[1], r[2], r[3], -r[4]) for r in rows]

    def _has_links(self) -> bool:
        return bool(self.db.run_script("?[src] := *links{src} :limit 1", {}, True)["rows"])

    def _search_hybrid(self, query: str, k: int, *, explain: bool, expand_graph: bool) -> dict:
        qv = self.embedder.embed_query(query)
        qt = _fts_query(query)
        # All hybrid_search construction lives here — single upgrade site when
        # an engine with optional legs / seed_from_legs ships (0.14.0+).
        hq: Dict[str, Any] = {
            "relation": "memories",
            "id_col": "id",
            "vector_index": "vec",
            "query_vector": [float(x) for x in qv],
            "vector_k": max(k, 10),
            "ef": max(50, 2 * k),
            "fts_index": "fts",
            # 0.12.2 requires both legs. When sanitization leaves no tokens
            # (punctuation/whitespace-only queries) use a harmless placeholder
            # term — never the raw text, which can fail the FTS grammar.
            "query_text": qt or "xnomatchx",
            "fts_k": max(k, 10),
            "rrf_k": self.rrf_k,
            "limit": max(k, 10),
            "detailed": bool(explain),
        }
        seeds: List[str] = []
        if expand_graph and self._has_links():
            seeds = [r["id"] for r in self._search_keyword(query, 3)]
            if seeds:
                hq["graph_legs"] = [
                    {
                        "edge_relation": "links",
                        "from_col": "src",
                        "to_col": "dst",
                        "seeds": seeds,
                        "max_hops": 2,
                        "undirected": True,
                        "label": "graph",
                    }
                ]
        res = self.db.hybrid_search(hq)
        out: Dict[str, Any] = {"mode_used": "hybrid"}
        if explain:
            ranked, per_result = self._parse_detailed(res["rows"], k)
            legs = {
                "semantic": "vector (HNSW cosine)",
                "text": "keyword (BM25)",
            }
            if seeds:
                legs["graph"] = f"graph proximity within 2 hops of seeds {seeds}"
            out["explain"] = {"legs": legs, "per_result": per_result}
        else:
            ranked = [(r[0], float(r[1])) for r in res["rows"]][:k]
        by_id = self._fetch([rid for rid, _ in ranked])
        out["results"] = [
            self._row_out(rid, *by_id[rid], score=score)
            for rid, score in ranked
            if rid in by_id
        ]
        return out

    @staticmethod
    def _parse_detailed(rows: List[list], k: int):
        """Long-format rows [id, score, list_id, leg_rank, leg_score] ->
        (fused ranking, per-result leg contributions)."""
        fused: Dict[str, float] = {}
        contribs: Dict[str, List[dict]] = {}
        for rid, score, list_id, leg_rank, leg_score in rows:
            fused.setdefault(rid, float(score))
            contribs.setdefault(rid, []).append(
                {"leg": list_id, "rank": int(leg_rank), "raw_score": float(leg_score)}
            )
        ranked = sorted(fused.items(), key=lambda kv: -kv[1])[:k]
        per_result = [
            {"id": rid, "fused_score": score, "contributions": contribs[rid]}
            for rid, score in ranked
        ]
        return ranked, per_result

    def find_related(
        self, id: str, max_nodes: int = 25, max_depth: int = 3, weighted: bool = False
    ) -> dict:
        if id not in self._fetch([id]):
            raise ValueError(f"no memory with id {id!r}")
        edge_rule = (
            "e[fr, to, w] := *links{src: fr, dst: to, weight: w}"
            if weighted
            else "e[fr, to] := *links{src: fr, dst: to}"
        )
        edge_head = "e[fr, to, w]" if weighted else "e[fr, to]"
        rows = self.db.run_script(
            f"seeds[n] <- [[$id]]\n{edge_rule}\n"
            f"?[node, cost, parent, depth] <~ BudgetedTraversal({edge_head}, seeds[n], "
            f"max_nodes: {max(1, min(int(max_nodes), 500))}, "
            f"max_depth: {max(1, min(int(max_depth), 16))}, undirected: true)",
            {"id": id},
            True,
        )["rows"]
        related_rows = [r for r in rows if r[0] != id]
        by_id = self._fetch([r[0] for r in related_rows])
        related = []
        for node, cost, parent, depth in sorted(related_rows, key=lambda r: (r[1], r[0])):
            if node not in by_id:
                continue
            text, meta, created_at = by_id[node]
            entry = self._row_out(node, text, meta, created_at)
            entry.update({"cost": float(cost), "depth": int(depth), "parent": parent})
            related.append(entry)
        return {"related": related}

    def list_recent(self, n: int = 10) -> dict:
        rows = self.db.run_script(
            f"?[id, text, meta, created_at, updated_at] := "
            f"*memories{{id, text, meta, created_at, updated_at}}\n"
            f":order -updated_at\n:limit {max(1, min(int(n), 200))}",
            {},
            True,
        )["rows"]
        return {"memories": [self._row_out(r[0], r[1], r[2], r[3]) for r in rows]}

    def recall_as_of(self, t: str, query: Optional[str] = None, k: int = 20) -> dict:
        when = parse_when(t)
        k = max(1, min(int(k), 200))
        if query:
            script = (
                f"?[id, text, meta, created_at] := "
                f"*memories_hist{{id, text, meta, created_at @ $t}}, "
                f"str_includes(lowercase(text), lowercase($q))\n"
                f":order -created_at\n:limit {k}"
            )
            params = {"t": when, "q": str(query)}
        else:
            script = (
                f"?[id, text, meta, created_at] := "
                f"*memories_hist{{id, text, meta, created_at @ $t}}\n"
                f":order -created_at\n:limit {k}"
            )
            params = {"t": when}
        rows = self.db.run_script(script, params, True)["rows"]
        return {
            "as_of": when,
            "memories": [self._row_out(r[0], r[1], r[2], r[3]) for r in rows],
        }

    def stats(self) -> dict:
        def _count(script: str) -> int:
            rows = self.db.run_script(script, {}, True)["rows"]
            return int(rows[0][0]) if rows else 0

        indices = [r[0] for r in self.db.run_script("::indices memories", {}, True)["rows"]]
        return {
            "db_path": self.db_path,
            "engine": self.engine,
            "memories": _count("?[count(id)] := *memories{id}"),
            "links": _count("?[count(rel)] := *links{src, dst, rel}"),
            "history_rows": _count("?[count(at)] := *memories_hist{id, at}"),
            "indices": indices,
            "model": getattr(self.embedder, "model_name", "unknown"),
            "dim": int(self.embedder.dim),
            "embedder_ready": bool(self.embedder.ready()),
            "schema_version": SCHEMA_VERSION,
        }
