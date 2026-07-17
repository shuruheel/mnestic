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
//! [`HybridSearch`] and [`GraphLeg`] are `#[non_exhaustive]`, so construction
//! is `Default` + field mutation:
//!
//! ```ignore
//! let mut q = HybridSearch::default();
//! q.relation = "docs".into();
//! q.vector_index = Some("vec".into());
//! q.query_vector = vec![1.0, 0.0];
//! q.fts_index = Some("fts".into());
//! q.query_text = "cat".into();
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

/// A typed **graph-proximity** leg folded into the fusion (the mnestic
/// fork's native 3-way fused recall — DEVELOPMENT.md Bet 1a).
///
/// One leg concept, two modes, switched by [`GraphLeg::max_nodes`]:
///
/// - **Recursive mode** (`max_nodes: None`, the default): generates a
///   **recursive bounded min-hop rule** over a stored edge relation — expands
///   from `seeds` up to `max_hops` hops, scores each reached node by its
///   *minimum* hop distance (closer = higher rank). This is the only graph leg
///   available in a `minimal` build, and the generated script is byte-identical
///   to previous releases (snapshot-guarded).
/// - **Budgeted mode** (`max_nodes: Some(n)`, requires the `graph-algo`
///   feature): generates a `BudgetedTraversal` call —
///   cheapest-first weighted expansion under a global distinct-node budget,
///   with an optional cost ceiling, an *exact* layered-label depth bound
///   (`max_hops` maps to `max_depth`), an optional liveness gate with an
///   in-expansion admission predicate, and optional edge weights. Seeds
///   default to the *other configured legs' own top-k* ([`seed_from_legs`]),
///   so the graph leg expands from what the vector/FTS legs found. Ranked
///   into the fusion by ascending path cost; seed roots (depth 0) are
///   excluded from the fusion contribution — they are the other legs' own
///   candidates, and re-emitting them would double-count identity rather
///   than measure proximity.
///
/// ⚠️ **Budgeted-mode cost model**: with an `edge_relation` input, the engine
/// pays **O(|edge relation|)** — a full scan plus an in-RAM CSR build —
/// *before any budget applies* (`max_nodes` bounds the expansion, not the
/// input). For production use over non-trivial graphs, point [`graph`] at a
/// pre-created cached projection (`::graph create`): the CSR is then built
/// once and reused across queries. Caveats: projections are process-local
/// RAM (a restart drops them), and a write to a source relation invalidates
/// the projection, so the next query after a write pays one full rebuild.
///
/// ⚠️ **Weight-column landmine**: a single `Null` or non-numeric cell in the
/// weight column aborts the whole query with a weight error — the CSR builder
/// never skips an edge. A *missing* weight column (`weight_col: None`) is
/// fine: every edge then costs `1.0`.
///
/// The seed values are passed as query **params** (never string-interpolated);
/// every identifier (`label`, relations, columns) is validated. [`admit`] is
/// the one exception, spliced verbatim like [`HybridList::rule_body`] — it is
/// *not* sanitized.
///
/// [`seed_from_legs`]: GraphLeg::seed_from_legs
/// [`graph`]: GraphLeg::graph
/// [`admit`]: GraphLeg::admit
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct GraphLeg {
    /// Fusion list tag (validated identifier). Default `"graph"`.
    pub label: String,
    /// The stored edge relation to traverse, e.g. `"edges"`. In budgeted mode,
    /// leave empty when [`GraphLeg::graph`] names a cached projection instead.
    pub edge_relation: String,
    /// Edge column holding the source node id. Default `"from"`.
    pub from_col: String,
    /// Edge column holding the destination node id. Default `"to"`.
    pub to_col: String,
    /// Seed node ids to expand from (the query anchors). The seeds themselves
    /// are *not* scored — proximity starts at hop 1. In budgeted mode these
    /// are honored *in addition to* the leg-derived seeds
    /// ([`GraphLeg::seed_from_legs`]).
    pub seeds: Vec<DataValue>,
    /// Maximum number of hops to expand (`k`). Must be `>= 1`. In budgeted
    /// mode this maps to `BudgetedTraversal`'s `max_depth` — an *exact*
    /// layered-label bound (a deliberate semantic upgrade over the
    /// recursion's min-hop bound).
    pub max_hops: usize,
    /// Treat edges as undirected — also traverse `to_col -> from_col`. Default
    /// `false` (follow edges in their stored direction only).
    pub undirected: bool,
    /// `Some(n)` switches this leg to **budgeted mode**: cheapest-first
    /// expansion admitting at most `n` distinct nodes (seed roots included).
    /// `None` (default) keeps the recursive min-hop mode.
    pub max_nodes: Option<usize>,
    /// Budgeted only: path-cost ceiling — paths costlier than this are pruned
    /// even while budget remains. `None` = no ceiling.
    pub max_cost: Option<f64>,
    /// Budgeted only: edge column holding a finite non-negative weight (the
    /// per-edge cost). `None` = every edge costs `1.0`. Costs are used as-is
    /// (min-plus); store `-ln(confidence)` yourself for multiplicative
    /// confidences.
    pub weight_col: Option<String>,
    /// Budgeted only: name of a **pre-created cached projection**
    /// (`::graph create`) to traverse instead of scanning `edge_relation`.
    /// This is the production path — see the cost-model note on [`GraphLeg`].
    /// Mutually exclusive with `edge_relation`/`weight_col` (the projection
    /// carries its own edges and weights).
    pub graph: Option<String>,
    /// Budgeted only: also seed from the configured vector/FTS legs' own
    /// top-k results (default `true`). With `false`, only explicit `seeds`
    /// are used (and must be non-empty).
    pub seed_from_legs: bool,
    /// Budgeted only: liveness-gate relation. Its **first key column must be
    /// the node id** (the gate is probed by key prefix). A node without a
    /// gate row — or failing [`GraphLeg::admit`] — spends no budget and never
    /// relays expansion.
    pub gate_relation: Option<String>,
    /// Budgeted only: the gate columns `admit` references, **by name**
    /// (order-independent; validated against the gate relation's schema at
    /// query time). Required iff `admit` is set.
    pub gate_cols: Vec<String>,
    /// Budgeted only: admission predicate over `gate_cols`, e.g.
    /// `"ok == 1"`. **Spliced verbatim — not sanitized.** `None` with a
    /// `gate_relation` = bare row-presence admits.
    pub admit: Option<String>,
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
            max_nodes: None,
            max_cost: None,
            weight_col: None,
            graph: None,
            seed_from_legs: true,
            gate_relation: None,
            gate_cols: Vec::new(),
            admit: None,
        }
    }
}

/// Parameters for a one-call hybrid retrieval. See the module docs.
///
/// Legs are **optional**: configure any non-empty subset of
/// {vector, FTS, graph legs, extra lists} and only those are generated and
/// fused. Note: the query vector is sent as an `F32` vector by default (the
/// common embedding case); set [`HybridSearch::vector_f64`] for `F64` indices.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct HybridSearch {
    /// The base stored relation, e.g. `"docs"`.
    pub relation: String,
    /// The key column of `relation` holding the item id. Default `"id"`.
    pub id_col: String,
    /// HNSW index name (the `<name>` in `relation:<name>`). `None` = no
    /// vector leg.
    pub vector_index: Option<String>,
    /// Query embedding for the semantic leg. Required (non-empty) iff
    /// `vector_index` is set.
    pub query_vector: Vec<f32>,
    /// Send the query vector as `F64` (match an `F64` index). Default `false` (`F32`).
    pub vector_f64: bool,
    /// `k` for the HNSW search.
    pub vector_k: usize,
    /// `ef` (search breadth) for the HNSW search.
    pub ef: usize,
    /// FTS index name. `None` = no keyword leg.
    pub fts_index: Option<String>,
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
            vector_index: None,
            query_vector: Vec::new(),
            vector_f64: false,
            vector_k: 10,
            ef: 50,
            fts_index: None,
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
    ensure!(q.vector_k > 0, "hybrid_search: vector_k must be > 0");
    ensure!(q.ef > 0, "hybrid_search: ef must be > 0");
    ensure!(q.fts_k > 0, "hybrid_search: fts_k must be > 0");
    // Optional legs: each index marker configures its leg; its payload is then
    // required — and a payload without its leg is a loud error, never a
    // silently dropped signal.
    match &q.vector_index {
        Some(vidx) => {
            validate_ident(vidx, "vector_index")?;
            ensure!(
                !q.query_vector.is_empty(),
                "hybrid_search: query_vector is empty but vector_index is configured"
            );
        }
        None => ensure!(
            q.query_vector.is_empty(),
            "hybrid_search: query_vector is set but vector_index is None — \
             configure the vector leg or drop the vector"
        ),
    }
    match &q.fts_index {
        Some(fidx) => validate_ident(fidx, "fts_index")?,
        None => ensure!(
            q.query_text.is_empty(),
            "hybrid_search: query_text is set but fts_index is None — \
             configure the FTS leg or drop the text"
        ),
    }
    ensure!(
        q.vector_index.is_some()
            || q.fts_index.is_some()
            || !q.graph_legs.is_empty()
            || !q.extra_lists.is_empty(),
        "hybrid_search: at least one leg (vector, fts, graph_legs, extra_lists) must be configured"
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
        validate_ident(&g.from_col, "graph_legs.from_col")?;
        validate_ident(&g.to_col, "graph_legs.to_col")?;
        ensure!(
            g.max_hops >= 1,
            "hybrid_search: graph_legs.max_hops must be >= 1"
        );
        if g.max_nodes.is_none() {
            // Recursive mode: budgeted-only fields set here would be silently
            // meaningless — reject them loudly instead.
            for (set, what) in [
                (g.max_cost.is_some(), "max_cost"),
                (g.weight_col.is_some(), "weight_col"),
                (g.graph.is_some(), "graph"),
                (!g.seed_from_legs, "seed_from_legs"),
                (g.gate_relation.is_some(), "gate_relation"),
                (!g.gate_cols.is_empty(), "gate_cols"),
                (g.admit.is_some(), "admit"),
            ] {
                ensure!(
                    !set,
                    "hybrid_search: graph_legs.{what} is a budgeted-mode field; \
                     set max_nodes to enable budgeted mode"
                );
            }
            validate_ident(&g.edge_relation, "graph_legs.edge_relation")?;
            ensure!(
                !g.seeds.is_empty(),
                "hybrid_search: graph_legs.seeds is empty"
            );
            continue;
        }
        // Budgeted mode (spec §9). The rule itself re-validates its options;
        // everything checked here is either builder-level (identifiers, the
        // feature seam, seed sources) or produces a better message early.
        #[cfg(not(feature = "graph-algo"))]
        ensure!(
            false,
            "hybrid_search: budgeted graph legs (max_nodes) require the `graph-algo` feature"
        );
        ensure!(
            g.max_nodes != Some(0),
            "hybrid_search: graph_legs.max_nodes must be >= 1"
        );
        if let Some(mc) = g.max_cost {
            ensure!(
                mc.is_finite() && mc >= 0.0,
                "hybrid_search: graph_legs.max_cost must be finite and >= 0"
            );
        }
        match &g.graph {
            Some(gname) => {
                validate_ident(gname, "graph_legs.graph")?;
                // The projection carries its own edges and weights; a stray
                // edge config would be silently ignored — reject it.
                ensure!(
                    g.edge_relation.is_empty(),
                    "hybrid_search: graph_legs.graph supersedes edge_relation; set only one"
                );
                ensure!(
                    g.weight_col.is_none(),
                    "hybrid_search: graph_legs.graph carries its own weights; drop weight_col"
                );
            }
            None => {
                validate_ident(&g.edge_relation, "graph_legs.edge_relation")?;
                if let Some(wc) = &g.weight_col {
                    validate_ident(wc, "graph_legs.weight_col")?;
                }
            }
        }
        match &g.gate_relation {
            Some(gr) => {
                validate_ident(gr, "graph_legs.gate_relation")?;
                for c in &g.gate_cols {
                    validate_ident(c, "graph_legs.gate_cols")?;
                }
                if g.admit.is_some() {
                    ensure!(
                        !g.gate_cols.is_empty(),
                        "hybrid_search: graph_legs.admit needs gate_cols naming the gate \
                         columns it references"
                    );
                }
            }
            None => ensure!(
                g.gate_cols.is_empty() && g.admit.is_none(),
                "hybrid_search: gate_cols/admit need a gate_relation"
            ),
        }
        let has_leg_seeds = g.seed_from_legs && (q.vector_index.is_some() || q.fts_index.is_some());
        ensure!(
            has_leg_seeds || !g.seeds.is_empty(),
            "hybrid_search: a budgeted graph leg needs a seed source — explicit seeds, or \
             seed_from_legs with a configured vector/FTS leg"
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
    if let Some(vidx) = &q.vector_index {
        writeln!(
            s,
            "sem[id, score] := ~{rel}:{vidx}{{ {idc}: id | query: {vec_call}, k: {vk}, ef: {ef}, bind_distance: __dist }}, score = -__dist",
            vk = q.vector_k,
            ef = q.ef,
        )
        .unwrap();
    }
    // Keyword (FTS) leg.
    if let Some(fidx) = &q.fts_index {
        writeln!(
            s,
            "txt[id, score] := ~{rel}:{fidx}{{ {idc}: id | query: $qt, k: {fk}, bind_score: score }}",
            fk = q.fts_k,
        )
        .unwrap();
    }

    // Typed graph-proximity legs (Bet 1a). Recursive mode emits a bounded
    // min-hop rule: a seed relation (seeds as params, unioned), a base rule
    // (hop 1) and a recursive rule (hop n+1, gated at `max_hops`) using
    // `min(dist)` so a node reached by several paths scores by its *shortest*
    // distance. Budgeted mode (spec §9) emits one BudgetedTraversal call whose
    // seeds union the other legs' own top-k with the explicit seed params.
    // Internal rule/var names are prefixed `hg{i}_` to avoid colliding with
    // the fixed legs or a user's `extra_lists` bodies.
    let mut params = BTreeMap::new();
    if q.vector_index.is_some() {
        params.insert(
            "qv".to_string(),
            DataValue::List(
                q.query_vector
                    .iter()
                    .map(|f| DataValue::from(*f as f64))
                    .collect(),
            ),
        );
    }
    if q.fts_index.is_some() {
        params.insert("qt".to_string(), DataValue::from(q.query_text.as_str()));
    }
    for (i, g) in q.graph_legs.iter().enumerate() {
        let er = &g.edge_relation;
        let fc = &g.from_col;
        let tc = &g.to_col;
        if let Some(max_nodes) = g.max_nodes {
            // Budgeted mode: seeds from the configured retrieval legs' own
            // candidates (their scores are NOT folded into path cost — leg
            // scores live on incomparable scales; cost starts at 0)…
            if g.seed_from_legs {
                if q.vector_index.is_some() {
                    writeln!(s, "hg{i}_seed[__n] := sem[__n, _]").unwrap();
                }
                if q.fts_index.is_some() {
                    writeln!(s, "hg{i}_seed[__n] := txt[__n, _]").unwrap();
                }
            }
            // …plus the explicit seed params, exactly as in recursive mode.
            for (j, seed) in g.seeds.iter().enumerate() {
                let pname = format!("hg{i}_seed{j}");
                writeln!(s, "hg{i}_seed[__n] := __n = ${pname}").unwrap();
                params.insert(pname, seed.clone());
            }
            // The edge input drops out entirely under a cached projection.
            let edge_input = match &g.graph {
                Some(_) => String::new(),
                None => {
                    match &g.weight_col {
                        Some(wc) => writeln!(
                            s,
                            "hg{i}_edge[__f, __t, __w] := *{er}{{ {fc}: __f, {tc}: __t, {wc}: __w }}"
                        )
                        .unwrap(),
                        // 2-column edge input: the rule defaults every weight
                        // to 1.0 (and a missing column can never hit the
                        // null-weight abort).
                        None => writeln!(
                            s,
                            "hg{i}_edge[__f, __t] := *{er}{{ {fc}: __f, {tc}: __t }}"
                        )
                        .unwrap(),
                    }
                    match &g.weight_col {
                        Some(_) => format!("hg{i}_edge[__f, __t, __w], "),
                        None => format!("hg{i}_edge[__f, __t], "),
                    }
                }
            };
            // Gate input: named bindings (order-independent, schema-checked)
            // when admit needs columns; bare `[]` for membership-only.
            let gate_input = match &g.gate_relation {
                None => String::new(),
                Some(gate) => {
                    if g.gate_cols.is_empty() {
                        format!("*{gate}[], ")
                    } else {
                        format!("*{gate}{{{cols}}}, ", cols = g.gate_cols.join(", "))
                    }
                }
            };
            let mut opts = format!("max_nodes: {max_nodes}, max_depth: {}", g.max_hops);
            if let Some(gname) = &g.graph {
                write!(opts, ", graph: '{gname}'").unwrap();
            }
            if let Some(mc) = g.max_cost {
                write!(opts, ", max_cost: {}", fmt_f64(mc)).unwrap();
            }
            if g.undirected {
                write!(opts, ", undirected: true").unwrap();
            }
            if let Some(admit) = &g.admit {
                write!(opts, ", admit: {admit}").unwrap();
            }
            writeln!(
                s,
                "hg{i}_bt[__n, __c, __p, __d] <~ BudgetedTraversal({edge_input}hg{i}_seed[__n], {gate_input}{opts})"
            )
            .unwrap();
        } else {
            // Recursive mode — byte-identical to previous releases
            // (snapshot-guarded).
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
    }

    // Union all legs into one [list_id, item, score] relation.
    if q.vector_index.is_some() {
        writeln!(
            s,
            "combined[__lid, id, score] := sem[id, score], __lid = 'semantic'"
        )
        .unwrap();
    }
    if q.fts_index.is_some() {
        writeln!(
            s,
            "combined[__lid, id, score] := txt[id, score], __lid = 'text'"
        )
        .unwrap();
    }
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
        // Closer (smaller distance / cheaper path) ⇒ higher score, matching
        // the other legs' higher-is-better orientation; RRF only uses the
        // within-list rank.
        if g.max_nodes.is_some() {
            // `__d > 0` excludes the seed roots from the fusion contribution:
            // the seeds are the other legs' own candidates, and re-emitting
            // them would double-count identity rather than measure proximity
            // (the recursive mode's `not hg{i}_seed[id]`, expressed on depth).
            writeln!(
                s,
                "combined[__lid, id, score] := hg{i}_bt[id, __c, _, __d], __d > 0, score = -__c, __lid = '{label}'",
                label = g.label,
            )
            .unwrap();
        } else {
            writeln!(
                s,
                "combined[__lid, id, score] := hg{i}_reach[id, __gd], not hg{i}_seed[id], score = -__gd, __lid = '{label}'",
                label = g.label,
            )
            .unwrap();
        }
    }

    let rrf_k = fmt_f64(q.rrf_k);
    let num_legs = usize::from(q.vector_index.is_some())
        + usize::from(q.fts_index.is_some())
        + q.extra_lists.len()
        + q.graph_legs.len();
    // With a budgeted leg and `detailed`, the head gains two trailing columns
    // (`parent`, `depth`) — the cheapest-path witness from BudgetedTraversal.
    // Opt-in only: callers without a budgeted leg get the old shapes
    // byte-for-byte (snapshot-guarded). Cost is already recoverable as
    // `-leg_score` on the budgeted leg's row.
    let budgeted_labels: Vec<(usize, &str)> = q
        .graph_legs
        .iter()
        .enumerate()
        .filter(|(_, g)| g.max_nodes.is_some())
        .map(|(i, g)| (i, g.label.as_str()))
        .collect();
    let witness_head = !budgeted_labels.is_empty() && q.detailed;
    // Guard for the "every other leg" rule: budgeted list_ids are excluded.
    let non_budgeted_guard: String = budgeted_labels
        .iter()
        .map(|(_, label)| format!(", list_id != '{label}'"))
        .collect();
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
        (None, true) if witness_head => {
            writeln!(
                s,
                "detail[id, score, list_id, leg_rank, leg_score] <~ ReciprocalRankFusion(combined[__lid, id, score], k: {rrf_k}, detailed: true)"
            )
            .unwrap();
            writeln!(
                s,
                "?[id, score, list_id, leg_rank, leg_score, parent, depth] := detail[id, score, list_id, leg_rank, leg_score]{non_budgeted_guard}, parent = null, depth = null"
            )
            .unwrap();
            for (i, label) in &budgeted_labels {
                writeln!(
                    s,
                    "?[id, score, list_id, leg_rank, leg_score, parent, depth] := detail[id, score, list_id, leg_rank, leg_score], list_id == '{label}', hg{i}_bt[id, _, parent, depth]"
                )
                .unwrap();
            }
            writeln!(s, ":order -score, id, list_id").unwrap();
            writeln!(s, ":limit {}", q.limit.saturating_mul(num_legs)).unwrap();
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
                if witness_head {
                    writeln!(
                        s,
                        "?[id, rank, score, list_id, leg_rank, leg_score, parent, depth] := mmrsel[id, rank], detail[id, score, list_id, leg_rank, leg_score]{non_budgeted_guard}, parent = null, depth = null"
                    )
                    .unwrap();
                    for (i, label) in &budgeted_labels {
                        writeln!(
                            s,
                            "?[id, rank, score, list_id, leg_rank, leg_score, parent, depth] := mmrsel[id, rank], detail[id, score, list_id, leg_rank, leg_score], list_id == '{label}', hg{i}_bt[id, _, parent, depth]"
                        )
                        .unwrap();
                    }
                } else {
                    writeln!(
                        s,
                        "?[id, rank, score, list_id, leg_rank, leg_score] := mmrsel[id, rank], detail[id, score, list_id, leg_rank, leg_score]"
                    )
                    .unwrap();
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn script(q: &HybridSearch) -> String {
        build_hybrid_query(q).unwrap().0
    }

    fn err(q: &HybridSearch) -> String {
        format!("{:?}", build_hybrid_query(q).unwrap_err())
    }

    fn both_legs() -> HybridSearch {
        HybridSearch {
            relation: "docs".into(),
            vector_index: Some("vec".into()),
            query_vector: vec![1.0, 0.0],
            fts_index: Some("fts".into()),
            query_text: "cat".into(),
            ..Default::default()
        }
    }

    // The no-regression proof for optional legs: a 0.12-shaped both-legs
    // query generates a byte-identical script. Golden, not `contains` — a
    // gained or lost line must fail this test.
    #[test]
    fn unbudgeted_both_legs_script_is_byte_stable() {
        assert_eq!(
            script(&both_legs()),
            "sem[id, score] := ~docs:vec{ id: id | query: vec($qv), k: 10, ef: 50, bind_distance: __dist }, score = -__dist\n\
             txt[id, score] := ~docs:fts{ id: id | query: $qt, k: 10, bind_score: score }\n\
             combined[__lid, id, score] := sem[id, score], __lid = 'semantic'\n\
             combined[__lid, id, score] := txt[id, score], __lid = 'text'\n\
             ?[id, score] <~ ReciprocalRankFusion(combined[__lid, id, score], k: 60.0)\n\
             :order -score\n\
             :limit 10\n"
        );
        let (_, params) = build_hybrid_query(&both_legs()).unwrap();
        assert!(params.contains_key("qv") && params.contains_key("qt"));
    }

    #[test]
    fn graph_only_recursive_script_is_byte_stable() {
        let q = HybridSearch {
            relation: "docs".into(),
            graph_legs: vec![GraphLeg {
                edge_relation: "edges".into(),
                seeds: vec![DataValue::from("n1")],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            script(&q),
            "hg0_seed[__s] := __s = $hg0_seed0\n\
             hg0_reach[__to, min(__d)] := hg0_seed[__s], *edges{ from: __s, to: __to }, __d = 1.0\n\
             hg0_reach[__to, min(__d)] := hg0_reach[__mid, __pd], __pd < 2.0, *edges{ from: __mid, to: __to }, __d = __pd + 1.0\n\
             combined[__lid, id, score] := hg0_reach[id, __gd], not hg0_seed[id], score = -__gd, __lid = 'graph'\n\
             ?[id, score] <~ ReciprocalRankFusion(combined[__lid, id, score], k: 60.0)\n\
             :order -score\n\
             :limit 10\n"
        );
        // no retrieval legs => neither $qv nor $qt is sent
        let (_, params) = build_hybrid_query(&q).unwrap();
        assert_eq!(params.len(), 1);
        assert!(params.contains_key("hg0_seed0"));
    }

    #[test]
    fn fts_only_script_is_byte_stable() {
        let q = HybridSearch {
            relation: "docs".into(),
            fts_index: Some("fts".into()),
            query_text: "cat".into(),
            ..Default::default()
        };
        assert_eq!(
            script(&q),
            "txt[id, score] := ~docs:fts{ id: id | query: $qt, k: 10, bind_score: score }\n\
             combined[__lid, id, score] := txt[id, score], __lid = 'text'\n\
             ?[id, score] <~ ReciprocalRankFusion(combined[__lid, id, score], k: 60.0)\n\
             :order -score\n\
             :limit 10\n"
        );
    }

    #[test]
    fn vector_only_script_is_byte_stable() {
        let q = HybridSearch {
            relation: "docs".into(),
            vector_index: Some("vec".into()),
            query_vector: vec![1.0, 0.0],
            ..Default::default()
        };
        assert_eq!(
            script(&q),
            "sem[id, score] := ~docs:vec{ id: id | query: vec($qv), k: 10, ef: 50, bind_distance: __dist }, score = -__dist\n\
             combined[__lid, id, score] := sem[id, score], __lid = 'semantic'\n\
             ?[id, score] <~ ReciprocalRankFusion(combined[__lid, id, score], k: 60.0)\n\
             :order -score\n\
             :limit 10\n"
        );
    }

    #[cfg(feature = "graph-algo")]
    #[test]
    fn budgeted_full_script_is_byte_stable() {
        let q = HybridSearch {
            graph_legs: vec![GraphLeg {
                edge_relation: "edges".into(),
                max_nodes: Some(64),
                max_cost: Some(10.0),
                weight_col: Some("w".into()),
                gate_relation: Some("live".into()),
                gate_cols: vec!["uid".into(), "ok".into()],
                admit: Some("ok == 1".into()),
                seeds: vec![DataValue::from("n1")],
                ..Default::default()
            }],
            ..both_legs()
        };
        assert_eq!(
            script(&q),
            "sem[id, score] := ~docs:vec{ id: id | query: vec($qv), k: 10, ef: 50, bind_distance: __dist }, score = -__dist\n\
             txt[id, score] := ~docs:fts{ id: id | query: $qt, k: 10, bind_score: score }\n\
             hg0_seed[__n] := sem[__n, _]\n\
             hg0_seed[__n] := txt[__n, _]\n\
             hg0_seed[__n] := __n = $hg0_seed0\n\
             hg0_edge[__f, __t, __w] := *edges{ from: __f, to: __t, w: __w }\n\
             hg0_bt[__n, __c, __p, __d] <~ BudgetedTraversal(hg0_edge[__f, __t, __w], hg0_seed[__n], *live{uid, ok}, max_nodes: 64, max_depth: 2, max_cost: 10.0, admit: ok == 1)\n\
             combined[__lid, id, score] := sem[id, score], __lid = 'semantic'\n\
             combined[__lid, id, score] := txt[id, score], __lid = 'text'\n\
             combined[__lid, id, score] := hg0_bt[id, __c, _, __d], __d > 0, score = -__c, __lid = 'graph'\n\
             ?[id, score] <~ ReciprocalRankFusion(combined[__lid, id, score], k: 60.0)\n\
             :order -score\n\
             :limit 10\n"
        );
    }

    #[cfg(feature = "graph-algo")]
    #[test]
    fn budgeted_projection_detailed_script_is_byte_stable() {
        let q = HybridSearch {
            relation: "docs".into(),
            vector_index: Some("vec".into()),
            query_vector: vec![1.0, 0.0],
            detailed: true,
            graph_legs: vec![GraphLeg {
                graph: Some("g1".into()),
                max_nodes: Some(8),
                undirected: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            script(&q),
            "sem[id, score] := ~docs:vec{ id: id | query: vec($qv), k: 10, ef: 50, bind_distance: __dist }, score = -__dist\n\
             hg0_seed[__n] := sem[__n, _]\n\
             hg0_bt[__n, __c, __p, __d] <~ BudgetedTraversal(hg0_seed[__n], max_nodes: 8, max_depth: 2, graph: 'g1', undirected: true)\n\
             combined[__lid, id, score] := sem[id, score], __lid = 'semantic'\n\
             combined[__lid, id, score] := hg0_bt[id, __c, _, __d], __d > 0, score = -__c, __lid = 'graph'\n\
             detail[id, score, list_id, leg_rank, leg_score] <~ ReciprocalRankFusion(combined[__lid, id, score], k: 60.0, detailed: true)\n\
             ?[id, score, list_id, leg_rank, leg_score, parent, depth] := detail[id, score, list_id, leg_rank, leg_score], list_id != 'graph', parent = null, depth = null\n\
             ?[id, score, list_id, leg_rank, leg_score, parent, depth] := detail[id, score, list_id, leg_rank, leg_score], list_id == 'graph', hg0_bt[id, _, parent, depth]\n\
             :order -score, id, list_id\n\
             :limit 20\n"
        );
    }

    // Without a budgeted leg, `detailed` keeps its old 5-column head
    // byte-for-byte — the witness columns are opt-in.
    #[test]
    fn detailed_without_budgeted_leg_keeps_old_head() {
        let q = HybridSearch {
            detailed: true,
            ..both_legs()
        };
        assert_eq!(
            script(&q),
            "sem[id, score] := ~docs:vec{ id: id | query: vec($qv), k: 10, ef: 50, bind_distance: __dist }, score = -__dist\n\
             txt[id, score] := ~docs:fts{ id: id | query: $qt, k: 10, bind_score: score }\n\
             combined[__lid, id, score] := sem[id, score], __lid = 'semantic'\n\
             combined[__lid, id, score] := txt[id, score], __lid = 'text'\n\
             ?[id, score, list_id, leg_rank, leg_score] <~ ReciprocalRankFusion(combined[__lid, id, score], k: 60.0, detailed: true)\n\
             :order -score, id, list_id\n\
             :limit 20\n"
        );
    }

    #[test]
    fn validation_errors_are_loud_and_specific() {
        // zero legs
        let q = HybridSearch {
            relation: "docs".into(),
            ..Default::default()
        };
        assert!(err(&q).contains("at least one leg"));
        // vector payload without its leg
        let q = HybridSearch {
            relation: "docs".into(),
            query_vector: vec![1.0],
            fts_index: Some("fts".into()),
            query_text: "cat".into(),
            ..Default::default()
        };
        assert!(err(&q).contains("vector_index is None"));
        // text payload without its leg
        let q = HybridSearch {
            relation: "docs".into(),
            vector_index: Some("vec".into()),
            query_vector: vec![1.0],
            query_text: "cat".into(),
            ..Default::default()
        };
        assert!(err(&q).contains("fts_index is None"));
        // vector leg without its payload
        let q = HybridSearch {
            relation: "docs".into(),
            vector_index: Some("vec".into()),
            ..Default::default()
        };
        assert!(err(&q).contains("query_vector is empty"));

        let leg = |g: GraphLeg| HybridSearch {
            graph_legs: vec![g],
            ..both_legs()
        };
        // budgeted-only fields without max_nodes
        for g in [
            GraphLeg {
                edge_relation: "edges".into(),
                seeds: vec![DataValue::from("n1")],
                max_cost: Some(1.0),
                ..Default::default()
            },
            GraphLeg {
                edge_relation: "edges".into(),
                seeds: vec![DataValue::from("n1")],
                gate_relation: Some("live".into()),
                ..Default::default()
            },
            GraphLeg {
                edge_relation: "edges".into(),
                seeds: vec![DataValue::from("n1")],
                seed_from_legs: false,
                ..Default::default()
            },
        ] {
            assert!(err(&leg(g)).contains("budgeted-mode field"));
        }
        // recursive mode still requires explicit seeds
        assert!(err(&leg(GraphLeg {
            edge_relation: "edges".into(),
            ..Default::default()
        }))
        .contains("seeds is empty"));

        #[cfg(feature = "graph-algo")]
        {
            // projection supersedes the edge config
            assert!(err(&leg(GraphLeg {
                edge_relation: "edges".into(),
                graph: Some("g1".into()),
                max_nodes: Some(4),
                ..Default::default()
            }))
            .contains("supersedes edge_relation"));
            assert!(err(&leg(GraphLeg {
                graph: Some("g1".into()),
                weight_col: Some("w".into()),
                max_nodes: Some(4),
                ..Default::default()
            }))
            .contains("drop weight_col"));
            // admit needs gate_cols; gate fields need a gate_relation
            assert!(err(&leg(GraphLeg {
                edge_relation: "edges".into(),
                max_nodes: Some(4),
                gate_relation: Some("live".into()),
                admit: Some("ok == 1".into()),
                ..Default::default()
            }))
            .contains("gate_cols"));
            assert!(err(&leg(GraphLeg {
                edge_relation: "edges".into(),
                max_nodes: Some(4),
                admit: Some("ok == 1".into()),
                ..Default::default()
            }))
            .contains("need a gate_relation"));
            // a budgeted leg needs a seed source
            assert!(err(&leg(GraphLeg {
                edge_relation: "edges".into(),
                max_nodes: Some(4),
                seed_from_legs: false,
                ..Default::default()
            }))
            .contains("seed source"));
            // ...and a graph-only budgeted query with seed_from_legs but no
            // retrieval legs has no seed source either
            let q = HybridSearch {
                relation: "docs".into(),
                graph_legs: vec![GraphLeg {
                    edge_relation: "edges".into(),
                    max_nodes: Some(4),
                    ..Default::default()
                }],
                ..Default::default()
            };
            assert!(err(&q).contains("seed source"));
            assert!(err(&leg(GraphLeg {
                edge_relation: "edges".into(),
                max_nodes: Some(0),
                ..Default::default()
            }))
            .contains("max_nodes must be >= 1"));
        }

        #[cfg(not(feature = "graph-algo"))]
        {
            // The feature-seam guard: a budgeted leg in a minimal build is a
            // loud builder error, not an opaque runtime "fixed rule not found".
            assert!(err(&leg(GraphLeg {
                edge_relation: "edges".into(),
                max_nodes: Some(4),
                ..Default::default()
            }))
            .contains("graph-algo"));
        }
    }
}
