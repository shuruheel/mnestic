"""Embedder-agnostic mnestic store: schema management, write path, and hybrid
retrieval over one relation (id => text, metadata:Json, emb:<vec>). The framework
adapters add only the embedding step on top of this.

Vendored per integration package so each is standalone-installable.
"""

from __future__ import annotations

from typing import Any, Dict, List, Optional, Sequence, Tuple


class MnesticStore:
    def __init__(
        self,
        db: Any,
        *,
        dim: int,
        relation: str = "mnestic_docs",
        id_col: str = "id",
        text_col: str = "text",
        emb_col: str = "emb",
        vector_index: str = "vec",
        fts_index: str = "fts",
        distance: str = "Cosine",
        dtype: str = "F32",
        m: int = 16,
        ef_construction: int = 50,
        ef_search: int = 50,
        create: bool = True,
    ) -> None:
        self.db = db
        self.dim = int(dim)
        self.relation = relation
        self.id_col = id_col
        self.text_col = text_col
        self.emb_col = emb_col
        self.vector_index = vector_index
        self.fts_index = fts_index
        self.distance = distance
        self.dtype = dtype
        self.m = m
        self.ef_construction = ef_construction
        self.ef_search = ef_search
        if create:
            self.ensure_schema()

    # --- schema (idempotent) ---
    def ensure_schema(self) -> None:
        rels = {r[0] for r in self.db.run_script("::relations", {}, True)["rows"]}
        if self.relation not in rels:
            self.db.run_script(
                f":create {self.relation} {{ {self.id_col}: String => "
                f"{self.text_col}: String, metadata: Json?, "
                f"{self.emb_col}: <{self.dtype}; {self.dim}> }}",
                {},
                False,
            )
        idx = {r[0] for r in self.db.run_script(f"::indices {self.relation}", {}, True)["rows"]}
        if self.vector_index not in idx:
            self.db.run_script(
                f"::hnsw create {self.relation}:{self.vector_index} {{ dim: {self.dim}, "
                f"m: {self.m}, dtype: {self.dtype}, fields: [{self.emb_col}], "
                f"distance: {self.distance}, ef_construction: {self.ef_construction} }}",
                {},
                False,
            )
        if self.fts_index not in idx:
            self.db.run_script(
                f"::fts create {self.relation}:{self.fts_index} {{ extractor: {self.text_col}, "
                f"tokenizer: Simple, filters: [Lowercase, Stemmer('English'), Stopwords('en')] }}",
                {},
                False,
            )

    # --- write ---
    def add(
        self,
        ids: Sequence[str],
        texts: Sequence[str],
        embeddings: Sequence[Sequence[float]],
        metadatas: Optional[Sequence[Optional[dict]]] = None,
    ) -> List[str]:
        metas = list(metadatas) if metadatas is not None else [None] * len(ids)
        rows = [
            [i, t, (m or {}), [float(x) for x in e]]
            for i, t, e, m in zip(ids, texts, embeddings, metas)
        ]
        self.db.run_script(
            f"?[{self.id_col}, {self.text_col}, metadata, {self.emb_col}] <- $rows "
            f":put {self.relation} {{ {self.id_col} => {self.text_col}, metadata, {self.emb_col} }}",
            {"rows": rows},
            False,
        )
        return list(ids)

    def delete(self, ids: Sequence[str]) -> None:
        self.db.run_script(
            f"?[{self.id_col}] <- $rows :rm {self.relation} {{ {self.id_col} }}",
            {"rows": [[i] for i in ids]},
            False,
        )

    # --- read ---
    def search(
        self,
        query_vector: Sequence[float],
        query_text: str,
        k: int,
        *,
        rrf_k: float = 60.0,
        mmr: Optional[Dict[str, Any]] = None,
        **extra: Any,
    ) -> List[Dict[str, Any]]:
        """Hybrid (HNSW + FTS -> RRF) search.

        Any extra keyword argument is passed straight through to the engine's
        ``hybrid_search`` query dict (e.g. ``graph_legs``, ``extra_lists``,
        ``vector_k``, ``fts_k``), so new engine keys need no adapter change.
        ``detailed`` is rejected: its long format emits one row per
        (result, leg) and would surface duplicate ids here.
        """
        hq: Dict[str, Any] = {
            "relation": self.relation,
            "id_col": self.id_col,
            "vector_index": self.vector_index,
            "query_vector": [float(x) for x in query_vector],
            "vector_f64": self.dtype == "F64",
            "vector_k": max(k, 10),
            "ef": max(self.ef_search, k),
            "fts_index": self.fts_index,
            "query_text": query_text,
            "fts_k": max(k, 10),
            "rrf_k": rrf_k,
            "limit": max(k, 10),
        }
        if mmr is not None:
            mmr = dict(mmr)
            mmr.setdefault("embedding_col", self.emb_col)
            hq["mmr"] = mmr
        if extra.pop("detailed", None):
            raise ValueError(
                "detailed is not supported through the adapter: the long format "
                "emits one row per (result, leg) and would surface duplicate ids"
            )
        hq.update(extra)
        res = self.db.hybrid_search(hq)
        ranked = [(row[0], float(row[1])) for row in res["rows"]][:k]
        return self._hydrate(ranked)

    def search_by_vector(
        self,
        query_vector: Sequence[float],
        k: int,
        *,
        ef: Optional[int] = None,
    ) -> List[Dict[str, Any]]:
        """Vector-only recall (no FTS leg, no fusion).

        ``score`` is the negated engine distance, so higher = more similar —
        the same orientation as the hybrid semantic leg. Works on every engine
        version the adapters support (plain HNSW index search).
        """
        vec_call = "vec($qv, 'F64')" if self.dtype == "F64" else "vec($qv)"
        script = (
            f"?[id, score] := ~{self.relation}:{self.vector_index}{{ "
            f"{self.id_col}: id | query: {vec_call}, k: {int(k)}, "
            f"ef: {int(ef) if ef is not None else max(self.ef_search, k)}, "
            f"bind_distance: __dist }}, score = -__dist\n"
            f":order -score\n"
            f":limit {int(k)}"
        )
        res = self.db.run_script(script, {"qv": [float(x) for x in query_vector]}, True)
        ranked = [(row[0], float(row[1])) for row in res["rows"]]
        return self._hydrate(ranked)

    def get(self, ids: Sequence[str]) -> List[Dict[str, Any]]:
        """Keyed lookup by id, preserving input order; missing ids are skipped."""
        by_id = self._fetch(list(ids))
        out: List[Dict[str, Any]] = []
        for rid in ids:
            row = by_id.get(rid)
            if row is None:
                continue
            out.append(
                {
                    "id": rid,
                    "text": row[1],
                    "metadata": (row[2] if row[2] is not None else {}),
                }
            )
        return out

    def _hydrate(self, ranked: List[Tuple[str, float]]) -> List[Dict[str, Any]]:
        if not ranked:
            return []
        by_id = self._fetch([rid for rid, _ in ranked])
        out: List[Dict[str, Any]] = []
        for rid, score in ranked:
            row = by_id.get(rid)
            out.append(
                {
                    "id": rid,
                    "score": score,
                    "text": row[1] if row else "",
                    "metadata": (row[2] if row and row[2] is not None else {}),
                }
            )
        return out

    def _fetch(self, ids: List[str]) -> Dict[str, list]:
        query = (
            f"idset[{self.id_col}] <- $id_rows\n"
            f"?[{self.id_col}, {self.text_col}, metadata] := "
            f"idset[{self.id_col}], *{self.relation}{{ {self.id_col}, {self.text_col}, metadata }}"
        )
        res = self.db.run_script(query, {"id_rows": [[i] for i in ids]}, True)
        return {row[0]: row for row in res["rows"]}
