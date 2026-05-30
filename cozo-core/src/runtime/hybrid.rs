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

use std::collections::BTreeMap;
use std::fmt::Write;

use miette::{bail, ensure, Result};

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
#[derive(Clone, Debug)]
pub struct HybridList {
    /// Fusion list tag (validated identifier).
    pub label: String,
    /// Rule body binding `id` and `score`.
    pub rule_body: String,
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
    /// RRF `k` constant (rank-bias damping). Default `60.0`.
    pub rrf_k: f64,
    /// Optional MMR diversity rerank. `None` returns the fused ranking directly.
    pub mmr: Option<MmrParams>,
    /// Max rows when no MMR rerank is applied (MMR uses its own `k`).
    pub limit: usize,
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
            rrf_k: 60.0,
            mmr: None,
            limit: 10,
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
    ensure!(q.rrf_k.is_finite() && q.rrf_k >= 0.0, "hybrid_search: rrf_k must be finite and >= 0");
    for l in &q.extra_lists {
        validate_ident(&l.label, "extra_lists.label")?;
    }
    if let Some(m) = &q.mmr {
        validate_ident(&m.embedding_col, "mmr.embedding_col")?;
        ensure!(m.lambda.is_finite(), "hybrid_search: mmr.lambda must be finite");
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
    // Union all legs into one [list_id, item, score] relation.
    writeln!(s, "combined[__lid, id, score] := sem[id, score], __lid = 'semantic'").unwrap();
    writeln!(s, "combined[__lid, id, score] := txt[id, score], __lid = 'text'").unwrap();
    for l in &q.extra_lists {
        writeln!(
            s,
            "combined[__lid, id, score] := {body}, __lid = '{label}'",
            body = l.rule_body,
            label = l.label,
        )
        .unwrap();
    }

    let rrf_k = fmt_f64(q.rrf_k);
    match &q.mmr {
        None => {
            writeln!(
                s,
                "?[id, score] <~ ReciprocalRankFusion(combined[__lid, id, score], k: {rrf_k})"
            )
            .unwrap();
            writeln!(s, ":order -score").unwrap();
            writeln!(s, ":limit {}", q.limit).unwrap();
        }
        Some(m) => {
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
            writeln!(
                s,
                "?[id, rank] <~ MaximalMarginalRelevance(cand[id, score, __emb], lambda: {lambda}, k: {k})",
                lambda = fmt_f64(m.lambda.clamp(0.0, 1.0)),
                k = m.k,
            )
            .unwrap();
            writeln!(s, ":order rank").unwrap();
        }
    }

    let mut params = BTreeMap::new();
    params.insert(
        "qv".to_string(),
        DataValue::List(q.query_vector.iter().map(|f| DataValue::from(*f as f64)).collect()),
    );
    params.insert("qt".to_string(), DataValue::from(q.query_text.as_str()));

    Ok((s, params))
}
