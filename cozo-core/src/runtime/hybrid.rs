/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! One-call hybrid retrieval (mnestic fork addition).
//!
//! The agentic-memory read path is almost always *hybrid*: a semantic (HNSW)
//! recall, a keyword (FTS) recall, optionally one or more graph-traversal
//! signals, fused into a single ranking and (optionally) diversified. mnestic
//! ships the fusion primitives — [`ReciprocalRankFusion`] and
//! [`MaximalMarginalRelevance`] — but assembling the full path is ~7 hand-written
//! Datalog rules (see `tests/hybrid_retrieval_e2e.rs`).
//!
//! [`HybridSearch`] turns that into one typed call. It assembles the *exact*
//! proven CozoScript pattern, passing the query vector and query text as script
//! parameters (never string-interpolated) and validating every interpolated
//! identifier, then runs it read-only via the normal query path.
//!
//! Fixed rules cannot issue sub-queries, so this is deliberately a script
//! *builder* on top of the public query API rather than a single self-contained
//! fixed rule. The generated script is available via
//! [`crate::DbInstance::hybrid_search_script`] for inspection / hand-tuning.
//!
//! ```ignore
//! let q = HybridSearch {
//!     relation: "docs".into(),
//!     vector_index: "vec".into(),
//!     query_vector: vec![1.0, 0.0],
//!     fts_index: "fts".into(),
//!     query_text: "cat".into(),
//!     ..HybridSearch::default()
//! };
//! let rows = db.hybrid_search(&q)?;
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use miette::{ensure, Result};

use crate::data::value::DataValue;

/// Diversity-aware rerank applied after fusion (Maximal Marginal Relevance).
#[derive(Clone, Debug)]
pub struct MmrParams {
    /// Trade-off between relevance and diversity, clamped to `[0, 1]` by the
    /// operator. `1.0` = pure relevance, `0.0` = pure diversity.
    pub lambda: f64,
    /// How many items to select (`0` selects all candidates).
    pub k: usize,
    /// The column on the base relation holding the embedding used for the
    /// diversity (cosine) comparison.
    pub embedding_col: String,
}

impl Default for MmrParams {
    fn default() -> Self {
        MmrParams {
            lambda: 0.5,
            k: 10,
            embedding_col: "emb".into(),
        }
    }
}

/// An extra ranked list folded into the fusion alongside the vector and keyword
/// legs — e.g. an n-hop graph-traversal signal.
///
/// `rule_body` is a Datalog rule body (the right-hand side of a `:=`) that must
/// bind two variables: `id` (the item key, matching the fused output) and
/// `score` (its rank score, higher = better). Example traversal body:
/// `*edges{ from: $seed, to: id }, score = 1.0`.
///
/// The body is your own Datalog and is spliced verbatim — it is *not* sanitized.
/// Only `label` is validated (it becomes a fusion list tag).
///
/// For the common case of *bounded-hop graph proximity from a seed set*, prefer
/// the typed [`GraphLeg`] (`HybridSearch::graph_legs`): a single inline body
/// cannot express the recursive shortest-path rule that proximity needs, and
/// [`GraphLeg`] generates it for you (with min-distance semantics, a hop bound,
/// and params for the seeds) so it folds into the *same* fused call.
#[derive(Clone, Debug)]
pub struct HybridList {
    /// Fusion list tag (validated identifier).
    pub label: String,
    /// Rule body binding `id` and `score`.
    pub rule_body: String,
}

/// A typed **k-hop graph-proximity** leg folded into the fusion (the mnestic
/// fork's native 3-way fused recall — DEVELOPMENT.md Bet 1a).
///
/// Unlike [`HybridList`], whose body is a single spliced rule, this generates a
/// **recursive bounded shortest-path rule** over a stored edge relation: it
/// expands from `seeds` up to `max_hops` hops, scores each reached node by its
/// *minimum* hop distance (closer = higher rank), and contributes that ranked
/// list to the Reciprocal Rank Fusion alongside the vector and keyword legs —
/// all in the one optimized call (no second `run_script`, no hand-written
/// recursion). The reached node id binds to the fused output id, so seeds and
/// item ids must live in the same id space as the base relation's key.
///
/// The seed values are passed as query **params** (never string-interpolated);
/// every identifier (`label`, relation, columns) is validated.
#[derive(Clone, Debug)]
pub struct GraphLeg {
    /// Fusion list tag (validated identifier). Default `"graph"`.
    pub label: String,
    /// The stored edge relation to traverse, e.g. `"edges"`.
    pub edge_relation: String,
    /// Edge column holding the source node id. Default `"from"`.
    pub from_col: String,
    /// Edge column holding the destination node id. Default `"to"`.
    pub to_col: String,
    /// Seed node ids to expand from (the query anchors). The seeds themselves
    /// are *not* scored — proximity starts at hop 1.
    pub seeds: Vec<DataValue>,
    /// Maximum number of hops to expand (`k`). Must be `>= 1`.
    pub max_hops: usize,
    /// Treat edges as undirected — also traverse `to_col -> from_col`. Default
    /// `false` (follow edges in their stored direction only).
    pub undirected: bool,
}

impl Default for GraphLeg {
    fn default() -> Self {
        GraphLeg {
            label: "graph".into(),
            edge_relation: String::new(),
            from_col: "from".into(),
            to_col: "to".into(),
            seeds: Vec::new(),
            max_hops: 2,
            undirected: false,
        }
    }
}

/// Parameters for a one-call hybrid retrieval. See the module docs.
///
/// Note: the query vector is sent as an `F32` vector by default (the common
/// embedding case); set [`HybridSearch::vector_f64`] for `F64` indices.
#[derive(Clone, Debug)]
pub struct HybridSearch {
    /// The base stored relation, e.g. `"docs"`.
    pub relation: String,
    /// The key column of `relation` holding the item id. Default `"id"`.
    pub id_col: String,
    /// HNSW index name (the `<name>` in `relation:<name>`).
    pub vector_index: String,
    /// Query embedding for the semantic leg.
    pub query_vector: Vec<f32>,
    /// Send the query vector as `F64` (match an `F64` index). Default `false` (`F32`).
    pub vector_f64: bool,
    /// `k` for the HNSW search.
    pub vector_k: usize,
    /// `ef` (search breadth) for the HNSW search.
    pub ef: usize,
    /// FTS index name.
    pub fts_index: String,
    /// Query text for the keyword leg (a CozoScript FTS expression).
    pub query_text: String,
    /// `k` for the FTS search.
    pub fts_k: usize,
    /// Extra ranked lists folded into the fusion (e.g. graph traversal).
    pub extra_lists: Vec<HybridList>,
    /// Typed k-hop graph-proximity legs folded into the fusion (native 3-way
    /// fused recall). See [`GraphLeg`].
    pub graph_legs: Vec<GraphLeg>,
    /// RRF `k` constant (rank-bias damping). Default `60.0`.
    pub rrf_k: f64,
    /// Optional MMR diversity rerank. `None` returns the fused ranking directly.
    pub mmr: Option<MmrParams>,
    /// Max rows when no MMR rerank is applied (MMR uses its own `k`).
    pub limit: usize,
    /// Also return per-leg contribution columns (long format). Each output row
    /// is one *(item, contributing leg)* pair: without MMR the head is
    /// `[id, score, list_id, leg_rank, leg_score]`, with MMR it is
    /// `[id, rank, score, list_id, leg_rank, leg_score]`. `leg_rank` is the
    /// 1-based within-list rank the fusion actually used; `leg_score` is the
    /// leg's raw (deduplicated best) score; legs an item did not appear in
    /// contribute no row. Without MMR the row limit is widened to
    /// `limit × number-of-legs` so the top `limit` items are always fully
    /// covered (the tail may include partially-covered extra items). Default
    /// `false`.
    pub detailed: bool,
}

impl Default for HybridSearch {
    fn default() -> Self {
        HybridSearch {
            relation: String::new(),
            id_col: "id".into(),
            vector_index: String::new(),
            query_vector: Vec::new(),
            vector_f64: false,
            vector_k: 10,
            ef: 50,
            fts_index: String::new(),
            query_text: String::new(),
            fts_k: 10,
            extra_lists: Vec::new(),
            graph_legs: Vec::new(),
            rrf_k: 60.0,
            mmr: None,
            limit: 10,
            detailed: false,
        }
    }
}

/// Reject anything that isn't a bare CozoScript identifier, so interpolating it
/// into the generated script can't smuggle in extra clauses.
fn validate_ident(s: &str, what: &str) -> Result<()> {
    let mut chars = s.chars();
    let ok = match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {
            chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        _ => false,
    };
    ensure!(
        ok,
        "hybrid_search: {what} must be a bare identifier (got {s:?})"
    );
    Ok(())
}

/// Format an `f64` so it always carries a decimal point (CozoScript float).
fn fmt_f64(x: f64) -> String {
    let s = format!("{x:?}");
    s
}

/// Build the `(script, params)` for a hybrid retrieval without running it.
///
/// Exposed for [`crate::DbInstance::hybrid_search_script`]; the query vector and
/// text are returned as params (`$qv`, `$qt`), everything else is validated and
/// interpolated.
pub fn build_hybrid_query(q: &HybridSearch) -> Result<(String, BTreeMap<String, DataValue>)> {
    validate_ident(&q.relation, "relation")?;
    validate_ident(&q.id_col, "id_col")?;
    validate_ident(&q.vector_index, "vector_index")?;
    validate_ident(&q.fts_index, "fts_index")?;
    ensure!(q.vector_k > 0, "hybrid_search: vector_k must be > 0");
    ensure!(q.ef > 0, "hybrid_search: ef must be > 0");
    ensure!(q.fts_k > 0, "hybrid_search: fts_k must be > 0");
    ensure!(
        !q.query_vector.is_empty(),
        "hybrid_search: query_vector is empty"
    );
    ensure!(
        q.rrf_k.is_finite() && q.rrf_k >= 0.0,
        "hybrid_search: rrf_k must be finite and >= 0"
    );
    let mut labels = BTreeSet::from(["semantic", "text"]);
    for l in &q.extra_lists {
        validate_ident(&l.label, "extra_lists.label")?;
        ensure!(
            labels.insert(l.label.as_str()),
            "hybrid_search: every fusion leg needs a distinct label; duplicate '{}'",
            l.label
        );
    }
    for g in &q.graph_legs {
        validate_ident(&g.label, "graph_legs.label")?;
        ensure!(
            labels.insert(g.label.as_str()),
            "hybrid_search: every fusion leg needs a distinct label; duplicate '{}'",
            g.label
        );
        validate_ident(&g.edge_relation, "graph_legs.edge_relation")?;
        validate_ident(&g.from_col, "graph_legs.from_col")?;
        validate_ident(&g.to_col, "graph_legs.to_col")?;
        ensure!(
            g.max_hops >= 1,
            "hybrid_search: graph_legs.max_hops must be >= 1"
        );
        ensure!(
            !g.seeds.is_empty(),
            "hybrid_search: graph_legs.seeds is empty"
        );
    }
    if let Some(m) = &q.mmr {
        validate_ident(&m.embedding_col, "mmr.embedding_col")?;
        ensure!(
            m.lambda.is_finite(),
            "hybrid_search: mmr.lambda must be finite"
        );
    }

    let rel = &q.relation;
    let idc = &q.id_col;
    let vec_call = if q.vector_f64 {
        "vec($qv, 'F64')"
    } else {
        "vec($qv)"
    };

    let mut s = String::new();
    // Semantic (HNSW) leg — bind the key column to `id`, negate distance so
    // higher = better, matching the FTS score orientation.
    writeln!(
        s,
        "sem[id, score] := ~{rel}:{vidx}{{ {idc}: id | query: {vec_call}, k: {vk}, ef: {ef}, bind_distance: __dist }}, score = -__dist",
        vidx = q.vector_index,
        vk = q.vector_k,
        ef = q.ef,
    )
    .unwrap();
    // Keyword (FTS) leg.
    writeln!(
        s,
        "txt[id, score] := ~{rel}:{fidx}{{ {idc}: id | query: $qt, k: {fk}, bind_score: score }}",
        fidx = q.fts_index,
        fk = q.fts_k,
    )
    .unwrap();

    // Typed graph-proximity legs (Bet 1a). For each leg we emit a recursive
    // bounded shortest-path rule: a seed relation (seeds as params, unioned),
    // a base rule (hop 1) and a recursive rule (hop n+1, gated at `max_hops`)
    // that uses `min(dist)` so a node reached by several paths scores by its
    // *shortest* distance. Internal rule/var names are prefixed `hg{i}_` to
    // avoid colliding with the fixed legs or a user's `extra_lists` bodies.
    let mut params = BTreeMap::new();
    params.insert(
        "qv".to_string(),
        DataValue::List(
            q.query_vector
                .iter()
                .map(|f| DataValue::from(*f as f64))
                .collect(),
        ),
    );
    params.insert("qt".to_string(), DataValue::from(q.query_text.as_str()));
    for (i, g) in q.graph_legs.iter().enumerate() {
        let er = &g.edge_relation;
        let fc = &g.from_col;
        let tc = &g.to_col;
        // Seed relation: one union rule per seed, value carried as a param.
        for (j, seed) in g.seeds.iter().enumerate() {
            let pname = format!("hg{i}_seed{j}");
            writeln!(s, "hg{i}_seed[__s] := __s = ${pname}").unwrap();
            params.insert(pname, seed.clone());
        }
        // Hop 1: direct neighbours of the seeds.
        writeln!(
            s,
            "hg{i}_reach[__to, min(__d)] := hg{i}_seed[__s], *{er}{{ {fc}: __s, {tc}: __to }}, __d = 1.0"
        )
        .unwrap();
        if g.undirected {
            writeln!(
                s,
                "hg{i}_reach[__to, min(__d)] := hg{i}_seed[__s], *{er}{{ {fc}: __to, {tc}: __s }}, __d = 1.0"
            )
            .unwrap();
        }
        // Hop n+1: expand only from nodes whose shortest distance is below the
        // hop bound, so the recursion is capped at `max_hops`.
        let bound = fmt_f64(g.max_hops as f64);
        writeln!(
            s,
            "hg{i}_reach[__to, min(__d)] := hg{i}_reach[__mid, __pd], __pd < {bound}, *{er}{{ {fc}: __mid, {tc}: __to }}, __d = __pd + 1.0"
        )
        .unwrap();
        if g.undirected {
            writeln!(
                s,
                "hg{i}_reach[__to, min(__d)] := hg{i}_reach[__mid, __pd], __pd < {bound}, *{er}{{ {fc}: __to, {tc}: __mid }}, __d = __pd + 1.0"
            )
            .unwrap();
        }
    }

    // Union all legs into one [list_id, item, score] relation.
    writeln!(
        s,
        "combined[__lid, id, score] := sem[id, score], __lid = 'semantic'"
    )
    .unwrap();
    writeln!(
        s,
        "combined[__lid, id, score] := txt[id, score], __lid = 'text'"
    )
    .unwrap();
    for l in &q.extra_lists {
        writeln!(
            s,
            "combined[__lid, id, score] := {body}, __lid = '{label}'",
            body = l.rule_body,
            label = l.label,
        )
        .unwrap();
    }
    for (i, g) in q.graph_legs.iter().enumerate() {
        // Closer (smaller distance) ⇒ higher score, matching the other legs'
        // higher-is-better orientation; RRF only uses the within-list rank.
        writeln!(
            s,
            "combined[__lid, id, score] := hg{i}_reach[id, __gd], not hg{i}_seed[id], score = -__gd, __lid = '{label}'",
            label = g.label,
        )
        .unwrap();
    }

    let rrf_k = fmt_f64(q.rrf_k);
    let num_legs = 2 + q.extra_lists.len() + q.graph_legs.len();
    match (&q.mmr, q.detailed) {
        (None, false) => {
            writeln!(
                s,
                "?[id, score] <~ ReciprocalRankFusion(combined[__lid, id, score], k: {rrf_k})"
            )
            .unwrap();
            writeln!(s, ":order -score").unwrap();
            writeln!(s, ":limit {}", q.limit).unwrap();
        }
        (None, true) => {
            // Long format: one row per (item, contributing leg). Widen the row
            // limit by the leg count so the top `limit` items are always fully
            // covered; order keeps an item's rows adjacent.
            writeln!(
                s,
                "?[id, score, list_id, leg_rank, leg_score] <~ ReciprocalRankFusion(combined[__lid, id, score], k: {rrf_k}, detailed: true)"
            )
            .unwrap();
            writeln!(s, ":order -score, id, list_id").unwrap();
            writeln!(s, ":limit {}", q.limit.saturating_mul(num_legs)).unwrap();
        }
        (Some(m), detailed) => {
            writeln!(
                s,
                "fused[id, score] <~ ReciprocalRankFusion(combined[__lid, id, score], k: {rrf_k})"
            )
            .unwrap();
            writeln!(
                s,
                "cand[id, score, __emb] := fused[id, score], *{rel}{{ {idc}: id, {emb}: __emb }}",
                emb = m.embedding_col,
            )
            .unwrap();
            let lambda = fmt_f64(m.lambda.clamp(0.0, 1.0));
            if detailed {
                // MMR bounds the items; join the per-leg detail onto its
                // selection (rows only expand per contributing leg).
                writeln!(
                    s,
                    "mmrsel[id, rank] <~ MaximalMarginalRelevance(cand[id, score, __emb], lambda: {lambda}, k: {k})",
                    k = m.k,
                )
                .unwrap();
                writeln!(
                    s,
                    "detail[id, score, list_id, leg_rank, leg_score] <~ ReciprocalRankFusion(combined[__lid, id, score], k: {rrf_k}, detailed: true)"
                )
                .unwrap();
                writeln!(
                    s,
                    "?[id, rank, score, list_id, leg_rank, leg_score] := mmrsel[id, rank], detail[id, score, list_id, leg_rank, leg_score]"
                )
                .unwrap();
                writeln!(s, ":order rank, list_id").unwrap();
            } else {
                writeln!(
                    s,
                    "?[id, rank] <~ MaximalMarginalRelevance(cand[id, score, __emb], lambda: {lambda}, k: {k})",
                    k = m.k,
                )
                .unwrap();
                writeln!(s, ":order rank").unwrap();
            }
        }
    }

    Ok((s, params))
}
