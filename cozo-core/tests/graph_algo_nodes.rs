/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Phase-0 oracles for the cached graph projection (`docs/specs/graph-projection.md` §3.7, §5).
//!
//! These pin the behaviour changes that ride along ahead of the cache itself, each of which is
//! independently observable and independently reversible:
//!
//! * **PageRank takes an optional node relation.** Vertices that appear in it but in no edge
//!   become real degree-0 vertices, so they are ranked and they enter the `1/N` base score.
//!   Previously the vertex set was exactly the edge endpoints.
//! * **The weighted builder registers a node relation the same way** — no in-tree algorithm
//!   passes one yet (the projection cache will), so a probe `FixedRule` exercises it here.
//! * **PageRank's default `iterations` is 20**, matching the vendored `graph` crate's own
//!   `PageRankConfig::DEFAULT_MAX_ITERATIONS`; 10 was a below-upstream override that is
//!   measurably non-convergent. Hitting the cap short of `epsilon` now warns.
//! * **The CSR builds check the poison flag** (unweighted and weighted), so a `:timeout` (or
//!   `::kill`) aborts a large build instead of waiting for it to finish.
//! * **An empty edge relation no longer panics** seven of the graph algorithms, and Prim keeps
//!   raising its starting-node diagnostics on one.

use cozo::{
    DataValue, DbInstance, FixedRule, FixedRulePayload, NamedRows, Poison, RegularTempStore,
    ScriptMutability,
};
use graph::prelude::Graph;
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

fn query(db: &DbInstance, script: &str) -> NamedRows {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Immutable)
        .unwrap_or_else(|e| panic!("script failed: {script}\n{e:?}"))
}

/// `[(node, rank)]` sorted by node, as f64.
fn ranks(rows: NamedRows) -> Vec<(i64, f64)> {
    let mut out: Vec<(i64, f64)> = rows
        .rows
        .into_iter()
        .map(|r| (r[0].get_int().unwrap(), r[1].get_float().unwrap()))
        .collect();
    out.sort_by_key(|(n, _)| *n);
    out
}

// ---------------------------------------------------------------------------
// PageRank: optional node relation
// ---------------------------------------------------------------------------

/// `3` is isolated: it is in the node relation and in no edge. It must be ranked, and its
/// presence must move every other rank, because PageRank's base score is `(1 - theta) / N`.
#[test]
fn pagerank_node_relation_makes_isolated_vertices_real() {
    let db = DbInstance::default();
    let without = ranks(query(
        &db,
        "e[] <- [[1, 2]] ?[node, rank] <~ PageRank(e[a, b])",
    ));
    let with = ranks(query(
        &db,
        "e[] <- [[1, 2]] n[] <- [[1], [2], [3]] ?[node, rank] <~ PageRank(e[a, b], n[c])",
    ));

    assert_eq!(
        without.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        vec![1, 2],
        "without a node relation the vertex set is exactly the edge endpoints"
    );
    assert_eq!(
        with.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "the isolated vertex 3 must be ranked"
    );

    // N went from 2 to 3, so the base score (1 - 0.85) / N changed for everyone.
    let base = (1.0 - 0.85) / 3.0;
    assert!(
        (with[2].1 - base).abs() < 1e-6,
        "an isolated vertex has no in-neighbours, so its rank is exactly the base score \
         {base}, got {}",
        with[2].1
    );
    assert!(
        (with[0].1 - without[0].1).abs() > 1e-6,
        "vertex 1's rank must change when N changes: {} vs {}",
        without[0].1,
        with[0].1
    );
}

/// A vertex that appears in an edge but not in the node relation is still a vertex: the node
/// relation registers ids up front, it does not filter the edges. (It is also the case that
/// gets the `node_values` build route wrong — the vendored builder asserts that the node-value
/// count covers the largest edge endpoint, so the ids must all be interned before the build.)
#[test]
fn pagerank_edge_endpoints_outside_the_node_relation_are_kept() {
    let db = DbInstance::default();
    let got = ranks(query(
        &db,
        "e[] <- [[1, 2], [2, 9]] n[] <- [[1], [3]] ?[node, rank] <~ PageRank(e[a, b], n[c])",
    ));
    assert_eq!(
        got.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        vec![1, 2, 3, 9],
        "union of node relation and edge endpoints"
    );
}

#[test]
fn pagerank_empty_inputs_do_not_panic() {
    let db = DbInstance::default();
    // Both empty: the `node_values` build route would assert `0 >= 1` against the vendored
    // builder's node count for an empty edge list, so the builder must not take that route.
    let got = query(
        &db,
        "e[a, b] <- [] n[c] <- [] ?[node, rank] <~ PageRank(e[a, b], n[c])",
    );
    assert!(got.rows.is_empty());

    // Nodes, no edges: every vertex is isolated.
    let got = ranks(query(
        &db,
        "e[a, b] <- [] n[] <- [[1], [2]] ?[node, rank] <~ PageRank(e[a, b], n[c])",
    ));
    assert_eq!(got.iter().map(|(n, _)| *n).collect::<Vec<_>>(), vec![1, 2]);
}

// ---------------------------------------------------------------------------
// PageRank: default iteration count
// ---------------------------------------------------------------------------

/// A path whose edges run *against* the vertex numbering: `i -> i-1`.
///
/// The vendored kernel sweeps vertices in ascending id order and writes scores in place, so a
/// path numbered along the edge direction settles in a single sweep (each vertex reads its
/// predecessor's freshly written score). Numbering it backwards makes every vertex read its
/// in-neighbour's *previous* value, so rank flows one hop per iteration and the run is still
/// moving well past iteration 10 — which is what lets this fixture see the iteration count.
fn reverse_path(n: i64) -> String {
    let edges = (1..n)
        .map(|i| format!("[{i},{}]", i - 1))
        .collect::<Vec<_>>()
        .join(",");
    format!("e[] <- [{edges}]")
}

/// `epsilon: 0.0` opts out of the convergence test, so the run is exactly `iterations` long and
/// the comparison isolates the default. Pins the default at 20 in both directions: equal to an
/// explicit 20, different from the old 10.
#[test]
fn pagerank_default_iterations_is_twenty() {
    let db = DbInstance::default();
    let path = reverse_path(30);
    let run = |opts: &str| {
        ranks(query(
            &db,
            &format!("{path} ?[node, rank] <~ PageRank(e[a, b], epsilon: 0.0{opts})"),
        ))
    };

    let default = run("");
    assert_eq!(
        default,
        run(", iterations: 20"),
        "the default `iterations` must be 20"
    );
    assert_ne!(
        default,
        run(", iterations: 10"),
        "if 10 and 20 iterations agree on this fixture the test cannot see the default at all"
    );
}

// ---------------------------------------------------------------------------
// PageRank: non-convergence warning
// ---------------------------------------------------------------------------

static LOG_BUFFER: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

struct CapturingLogger;

impl log::Log for CapturingLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Warn
    }
    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            LOG_BUFFER
                .get_or_init(Default::default)
                .lock()
                .unwrap()
                .push(format!("{}", record.args()));
        }
    }
    fn flush(&self) {}
}

/// Serializes the callers of `warnings_from`. The logger itself stays installed process-wide,
/// so a concurrently running test that emits any warning would land in the shared buffer — the
/// assertions below are therefore scoped to PageRank's own message, never to buffer emptiness.
static LOG_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Runs `script` with the warning logger installed and returns the warnings it emitted.
fn warnings_from(script: &str) -> Vec<String> {
    let _guard = LOG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _ = log::set_boxed_logger(Box::new(CapturingLogger));
    log::set_max_level(log::LevelFilter::Warn);

    LOG_BUFFER
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .clear();
    query(&DbInstance::default(), script);
    LOG_BUFFER
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .clone()
}

/// True iff the buffer holds PageRank's non-convergence warning. Matching on the subject rather
/// than asserting buffer emptiness keeps the test immune to unrelated warnings from tests
/// running concurrently in this binary.
fn has_pagerank_warning(warnings: &[String]) -> bool {
    warnings
        .iter()
        .any(|w| w.contains("PageRank") && w.contains("have not"))
}

#[test]
fn pagerank_warns_only_when_the_iteration_cap_cuts_convergence_short() {
    let path = reverse_path(30);
    let capped = warnings_from(&format!(
        "{path} ?[node, rank] <~ PageRank(e[a, b], iterations: 1)"
    ));
    assert!(
        has_pagerank_warning(&capped),
        "one iteration cannot converge to the default epsilon; expected a warning, got {capped:?}"
    );

    let converged = warnings_from(&format!(
        "{path} ?[node, rank] <~ PageRank(e[a, b], iterations: 200)"
    ));
    assert!(
        !has_pagerank_warning(&converged),
        "a converged run must not warn, got {converged:?}"
    );

    // `epsilon: 0.0` means "run the full cap"; that is not a failure to converge.
    let opted_out = warnings_from(&format!(
        "{path} ?[node, rank] <~ PageRank(e[a, b], epsilon: 0.0, iterations: 1)"
    ));
    assert!(
        !has_pagerank_warning(&opted_out),
        "epsilon 0 opts out of the convergence test, got {opted_out:?}"
    );
}

// ---------------------------------------------------------------------------
// An empty edge relation
// ---------------------------------------------------------------------------

/// All 12 CSR-building fixed-rule call sites, plus `DegreeCentrality` (which never builds one),
/// against an empty edge relation. Seven of these used to panic. Dijkstra and Yen take their
/// required start/termination relations and silently skip unresolvable entries — a pre-existing
/// semantic this test merely records.
///
/// The cause is one line in the vendored builder: an empty edge list reports `max_node_id() == 0`
/// (the identity of a `max` reduce), so the graph is sized to **one** vertex while the id map is
/// empty, and the first `indices[0]` blows up. Upstream evidently expected `node_count() == 0`
/// here — four algorithms carry a dead guard testing exactly that. The id map, not the graph, is
/// the authority on emptiness.
#[test]
fn empty_edge_relation_yields_no_rows_and_does_not_panic() {
    let cases = [
        "?[idx, node] <~ TopSort(e[a, b])",
        "?[node, g] <~ ConnectedComponents(e[a, b])",
        "?[node, g] <~ StronglyConnectedComponents(e[a, b])",
        "?[n, c, t, d] <~ ClusteringCoefficients(e[a, b])",
        "?[node, r] <~ PageRank(e[a, b])",
        "?[node, c] <~ BetweennessCentrality(e[a, b])",
        "?[node, c] <~ ClosenessCentrality(e[a, b])",
        "?[node, l] <~ LabelPropagation(e[a, b])",
        "?[node, l] <~ CommunityDetectionLouvain(e[a, b])",
        "?[f, t, w] <~ MinimumSpanningForestKruskal(e[a, b])",
        "?[f, t, w] <~ MinimumSpanningTreePrim(e[a, b])",
        "s[] <- [[1]] ?[f, t, c, p] <~ ShortestPathDijkstra(e[a, b], s[x])",
        "s[] <- [[1]] t[] <- [[2]] ?[f, g, c, p] <~ KShortestPathYen(e[a, b], s[x], t[y], k: 2)",
        "?[node, d, i, o] <~ DegreeCentrality(e[a, b])",
    ];
    for tail in cases {
        let db = DbInstance::default();
        let got = query(&db, &format!("e[a, b] <- [] {tail}"));
        assert!(
            got.rows.is_empty(),
            "{tail} on an empty edge relation should yield no rows, got {:?}",
            got.rows
        );
    }
}

/// The empty-edge early-return must not swallow Prim's starting-node diagnostics: a user who
/// names a starting node over an empty edge relation gets `algo::starting_node_not_found`, and
/// one who supplies an empty starting relation gets `algo::empty_starting` — not a silent empty
/// result. (Both errors predate this change; the early-return sits in the no-input-1 arm only.)
#[test]
fn prim_on_empty_edges_keeps_starting_node_diagnostics() {
    let db = DbInstance::default();
    let err = db
        .run_script(
            "e[a, b] <- [] s[] <- [[1]] ?[f, t, w] <~ MinimumSpanningTreePrim(e[a, b], s[x])",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .expect_err("a starting node that is not in the (empty) graph must error");
    assert!(
        err.to_string().contains("not found"),
        "expected starting_node_not_found, got: {err}"
    );

    let err = db
        .run_script(
            "e[a, b] <- [] s[x] <- [] ?[f, t, w] <~ MinimumSpanningTreePrim(e[a, b], s[x])",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .expect_err("an empty starting relation must error");
    assert!(
        err.to_string().contains("empty"),
        "expected empty_starting, got: {err}"
    );
}

/// The empty-edge guard must not swallow the node relation: `ConnectedComponents` skips Tarjan
/// when there are no vertices, but its node-relation overlay still has work to do.
#[test]
fn empty_edge_relation_still_emits_the_node_relation() {
    let db = DbInstance::default();
    let got = query(
        &db,
        "e[a, b] <- [] n[] <- [[1], [2]] ?[node, g] <~ ConnectedComponents(e[a, b], n[c])",
    );
    let mut nodes: Vec<i64> = got.rows.iter().map(|r| r[0].get_int().unwrap()).collect();
    nodes.sort();
    assert_eq!(nodes, vec![1, 2]);
    let groups: std::collections::BTreeSet<i64> =
        got.rows.iter().map(|r| r[1].get_int().unwrap()).collect();
    assert_eq!(groups.len(), 2, "two isolated vertices are two components");
}

// ---------------------------------------------------------------------------
// The weighted builder: node registration and negative-weight policy
// ---------------------------------------------------------------------------

/// No in-tree algorithm passes a node relation to the weighted builder yet — the projection
/// cache (spec §3.2) will be its first consumer — so this probe exercises it through the same
/// `register_fixed_rule` extension point an out-of-tree consumer would use. Emits
/// `(node, graph_node_count)` per registered vertex: the second column is what proves an
/// isolated vertex became a *real graph vertex* (sized into the CSR), not merely an id-map entry.
struct WeightedNodesProbe;

impl FixedRule for WeightedNodesProbe {
    fn run(
        &self,
        payload: FixedRulePayload<'_, '_>,
        out: &mut RegularTempStore,
        poison: Poison,
    ) -> miette::Result<()> {
        let edges = payload.get_input(0)?;
        let nodes = payload.get_input(1).ok();
        let allow_negative = payload.bool_option("allow_negative", Some(true))?;
        let (graph, indices, _inv) = edges.as_directed_weighted_graph_checked(
            false,
            allow_negative,
            nodes.as_ref(),
            &poison,
        )?;
        for val in indices.iter() {
            out.put(vec![
                val.clone(),
                DataValue::from(graph.node_count() as i64),
            ]);
        }
        Ok(())
    }

    fn arity(
        &self,
        _options: &BTreeMap<smartstring::alias::String, cozo::Expr>,
        _rule_head: &[cozo::Symbol],
        _span: cozo::SourceSpan,
    ) -> miette::Result<usize> {
        Ok(2)
    }
}

fn probe_db() -> DbInstance {
    let db = DbInstance::default();
    db.register_fixed_rule("WeightedNodesProbe".to_string(), WeightedNodesProbe)
        .unwrap();
    db
}

/// Vertex 3 is in the node relation but in no edge; it must be registered AND sized into the
/// weighted CSR (`node_count` 3, not 2 — the `node_values` build route). The node relation
/// deliberately lists the edge endpoints BEFORE the isolate: node registration runs first, so
/// this ordering gives the isolate the highest id — the only id the max-edge-endpoint sizing of
/// the plain build route would fail to cover. (An isolate-only node relation would get id 0 and
/// be covered by accident, discriminating nothing.)
#[test]
fn weighted_builder_registers_isolated_vertices() {
    let db = probe_db();
    let got = query(
        &db,
        "e[] <- [[1, 2, 1.5]] n[] <- [[1], [2], [3]] \
         ?[node, nc] <~ WeightedNodesProbe(e[a, b, w], n[c])",
    );
    let mut rows: Vec<(i64, i64)> = got
        .rows
        .iter()
        .map(|r| (r[0].get_int().unwrap(), r[1].get_int().unwrap()))
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        vec![(1, 3), (2, 3), (3, 3)],
        "the isolated vertex must be a real degree-0 vertex in a 3-vertex weighted CSR"
    );

    // Without the node relation the vertex set is exactly the edge endpoints.
    let got = query(
        &db,
        "e[] <- [[1, 2, 1.5]] ?[node, nc] <~ WeightedNodesProbe(e[a, b, w])",
    );
    assert_eq!(got.rows.len(), 2);
}

/// The negative-weight policy is enforced during the scan, nodes or no nodes.
#[test]
fn weighted_builder_negative_weight_policy() {
    let db = probe_db();
    let err = db
        .run_script(
            "e[] <- [[1, 2, -1.0]] ?[node, nc] <~ WeightedNodesProbe(e[a, b, w], allow_negative: false)",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .expect_err("a negative weight must be rejected when allow_negative is false");
    assert!(
        err.to_string().contains("weight"),
        "expected invalid_edge_weight, got: {err}"
    );

    // Permissive build accepts it (has_negative is recorded internally for the projection
    // cache's strict consumers — not observable through this API; pinned by Phase 2's tests).
    let got = query(
        &db,
        "e[] <- [[1, 2, -1.0]] ?[node, nc] <~ WeightedNodesProbe(e[a, b, w])",
    );
    assert_eq!(got.rows.len(), 2);
}

// ---------------------------------------------------------------------------
// The CSR build is interruptible
// ---------------------------------------------------------------------------

/// Enough edges that the build (scan + intern + CSR) dominates, and enough distinct vertices
/// that the id map is the expensive part.
const BUILD_EDGES: i64 = 300_000;

fn seed_big_graph() -> DbInstance {
    let db = DbInstance::default();
    db.run_script(
        ":create edge { a: Int, b: Int }",
        BTreeMap::new(),
        ScriptMutability::Mutable,
    )
    .unwrap();
    let rows: Vec<Vec<DataValue>> = (0..BUILD_EDGES)
        .map(|i| {
            vec![
                DataValue::from(i),
                DataValue::from((i * 7 + 1) % BUILD_EDGES),
            ]
        })
        .collect();
    let mut data = BTreeMap::new();
    data.insert(
        "edge".to_string(),
        NamedRows::new(vec!["a".to_string(), "b".to_string()], rows),
    );
    db.import_relations(data).unwrap();
    db
}

/// Self-calibrating: time an uninterrupted run, then assert a tiny `:timeout` aborts in a small
/// fraction of it. Without a poison check inside the builder the timeout is not observed until
/// the whole CSR is built, so the two timings converge and this goes red.
fn assert_build_aborts_on_timeout(script: &str, which_builder: &str) {
    let db = seed_big_graph();

    let t0 = Instant::now();
    query(&db, script);
    let full = t0.elapsed();

    let t0 = Instant::now();
    let res = db.run_script(
        &format!("{script} :timeout 0.02"),
        BTreeMap::new(),
        ScriptMutability::Immutable,
    );
    let interrupted = t0.elapsed();

    assert!(res.is_err(), "`:timeout 0.02` should abort the build");
    assert!(
        interrupted * 2 < full,
        "the timeout aborted after {interrupted:?} but an uninterrupted build takes {full:?}: \
         the poison flag is not being checked inside the {which_builder} CSR build"
    );
}

#[test]
fn csr_build_aborts_on_timeout() {
    assert_build_aborts_on_timeout(
        "?[node, group] <~ ConnectedComponents(*edge[a, b])",
        "unweighted",
    );
}

/// Kruskal reads the same 2-column fixture through the WEIGHTED builder (weights default to
/// 1.0), so this pins the poison check in `build_weighted_graph` independently of the
/// unweighted one above.
#[test]
fn weighted_csr_build_aborts_on_timeout() {
    assert_build_aborts_on_timeout(
        "?[f, t, w] <~ MinimumSpanningForestKruskal(*edge[a, b])",
        "weighted",
    );
}
