/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! **The recursive-workload planner pin** — the outstanding tier of the
//! planner regression suite (ROADMAP: "it should also pin the *recursive*
//! workload — fixpoint reachability at increasing depth — which is the thing
//! this engine is fastest at and therefore the thing a regression would hurt
//! most"). Until this landed, the stated position was that no further planner
//! pass could be enabled by default.
//!
//! Two tiers, mirroring `planner_shape.rs` / `lsqb.rs`:
//!
//! - **Per-PR (this file, un-ignored):** plan-shape pins for the three
//!   canonical recursive shapes — full transitive closure, seeded reachability
//!   (the magic-sets shape), same-generation (the magic-sets stress shape) —
//!   over EMPTY relations (the planner is stat-free), plus exact-count
//!   execution oracles on constructed graphs (closed-form answers) and on a
//!   seeded random digraph (Rust BFS oracle). Milliseconds to a few seconds.
//! - **Nightly (`--ignored`, release, wired into `planner-guard.yml`):** a
//!   deep-fixpoint pathology cap — seeded reachability down a 10k-diameter
//!   chain, i.e. 10k semi-naive iterations — with a generous wall-clock bound.
//!   No dataset, no env var: the graph is generated in-test.
//!
//! Baselines are inline. When a deliberate planner change fires the shape pin,
//! regenerate with:
//! `cargo test -p mnestic --test recursive_workload -- --ignored regenerate --nocapture`
//! and paste — in the same PR, per the T0 contract.

mod common;

use common::*;
use cozo::DbInstance;
use std::collections::{HashSet, VecDeque};
use std::time::Instant;

// ---------------------------------------------------------------------------
// The pinned recursive shapes
// ---------------------------------------------------------------------------

const TC_FULL: &str = "
tc[a, b] := *edge[a, b]
tc[a, b] := tc[a, c], *edge[c, b]
?[a, b] := tc[a, b]
";

const REACH_SEEDED: &str = "
reach[b] := *edge[1, b]
reach[b] := reach[a], *edge[a, b]
?[b] := reach[b]
";

const SAME_GEN: &str = "
sg[x, y] := *edge[p, x], *edge[p, y], x != y
sg[x, y] := *edge[a, x], sg[a, b], *edge[b, y]
?[x, y] := sg[x, y]
";

const SHAPES: &[(&str, &str)] = &[
    ("tc_full", TC_FULL),
    ("reach_seeded", REACH_SEEDED),
    ("same_gen", SAME_GEN),
];

fn edge_db(path: &std::path::Path) -> DbInstance {
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), Default::default()).unwrap();
    run_mut(&db, ":create edge {a: Int, b: Int}");
    db
}

// ---------------------------------------------------------------------------
// Tier A1 — plan-shape pins (empty relations, per PR)
// ---------------------------------------------------------------------------

/// Committed stored-load signatures per shape, `(greedy, written)`.
/// Regenerate via the ignored `regenerate` test below when a change is meant.
///
/// Note the two arms are IDENTICAL today, and that is a finding, not an
/// oversight: the greedy reorder declines every rule containing a derived
/// atom, and the recursive shapes' base cases sit below its 3-stored-atom
/// horizon — so no shipped pass touches recursive rules at all. The moment a
/// future pass does, this baseline fires and forces the T1 nightly tier to
/// prove the change safe. (`same_gen`'s 8 loads are the magic-sets rewrite
/// splitting the seeded rule set.)
const BASELINE: &[(&str, &[&str], &[&str])] = &[
    ("tc_full", &[":edge", ":edge"], &[":edge", ":edge"]),
    ("reach_seeded", &[":edge", ":edge"], &[":edge", ":edge"]),
    (
        "same_gen",
        &[
            ":edge", ":edge", ":edge", ":edge", ":edge", ":edge", ":edge", ":edge",
        ],
        &[
            ":edge", ":edge", ":edge", ":edge", ":edge", ":edge", ":edge", ":edge",
        ],
    ),
];

#[test]
fn recursive_plan_shapes_match_committed_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let db = edge_db(&dir.path().join("shape.db"));
    let mut drifted = vec![];
    for &(name, greedy_want, written_want) in BASELINE {
        let query = SHAPES
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, q)| *q)
            .unwrap();
        for (arm, got, want) in [
            ("greedy", greedy_refs(&db, query), greedy_want),
            ("written", written_refs(&db, query), written_want),
        ] {
            if got != want {
                drifted.push(format!(
                    "\n  {name} [{arm}]\n    baseline: {want:?}\n    current:  {got:?}"
                ));
            }
        }
    }
    assert!(
        drifted.is_empty(),
        "RECURSIVE PLAN-SHAPE DRIFT:{}\n\nTripwire, not verdict — if the change is \
         deliberate, run the nightly recursive tier and regenerate this baseline in \
         the same PR:\n`cargo test -p mnestic --test recursive_workload -- --ignored \
         regenerate --nocapture`\n",
        drifted.join("")
    );
}

/// Prints the current signatures for pasting into `BASELINE`.
#[test]
#[ignore = "regeneration helper, not a gate"]
fn regenerate() {
    let dir = tempfile::tempdir().unwrap();
    let db = edge_db(&dir.path().join("regen.db"));
    for (name, query) in SHAPES {
        println!(
            "(\n    \"{name}\",\n    &{:?},\n    &{:?},\n),",
            greedy_refs(&db, query),
            written_refs(&db, query)
        );
    }
}

// ---------------------------------------------------------------------------
// Tier A2 — execution oracles (exact counts, per PR)
// ---------------------------------------------------------------------------

fn put_edges(db: &DbInstance, edges: &[(i64, i64)]) {
    // Chunked inserts to keep script size sane.
    for chunk in edges.chunks(8000) {
        let rows: String = chunk
            .iter()
            .map(|(a, b)| format!("[{a},{b}]"))
            .collect::<Vec<_>>()
            .join(",");
        run_mut(db, &format!("?[a, b] <- [{rows}] :put edge {{a, b}}"));
    }
}

/// Counting twins of the pinned shapes (aggregation lives in the head in
/// CozoScript; the recursive rules are identical to the `::explain`ed forms).
const TC_COUNT: &str = "
tc[a, b] := *edge[a, b]
tc[a, b] := tc[a, c], *edge[c, b]
?[count(a)] := tc[a, b]
";

const REACH_COUNT: &str = "
reach[b] := *edge[1, b]
reach[b] := reach[a], *edge[a, b]
?[count(b)] := reach[b]
";

const SG_COUNT: &str = "
sg[x, y] := *edge[p, x], *edge[p, y], x != y
sg[x, y] := *edge[a, x], sg[a, b], *edge[b, y]
?[count(x)] := sg[x, y]
";

fn count(db: &DbInstance, script: &str) -> i64 {
    run(db, script).rows[0][0].get_int().unwrap()
}

/// Chain 1→2→…→n: closure = n(n−1)/2 pairs; fixpoint depth = the diameter.
/// Run at increasing depth — the roadmap's literal ask.
#[test]
fn chain_closure_exact_at_increasing_depth() {
    for n in [64i64, 256, 1024] {
        let dir = tempfile::tempdir().unwrap();
        let db = edge_db(&dir.path().join("chain.db"));
        let edges: Vec<_> = (1..n).map(|i| (i, i + 1)).collect();
        put_edges(&db, &edges);
        assert_eq!(
            count(&db, TC_COUNT),
            n * (n - 1) / 2,
            "chain closure wrong at depth {n}"
        );
    }
}

/// Chain + closing back-edge = a cycle: every node reaches every node (n²),
/// and the fixpoint must terminate despite the cycle.
#[test]
fn cycle_closure_terminates_and_is_exact() {
    let n = 200i64;
    let dir = tempfile::tempdir().unwrap();
    let db = edge_db(&dir.path().join("cycle.db"));
    let mut edges: Vec<_> = (1..n).map(|i| (i, i + 1)).collect();
    edges.push((n, 1));
    put_edges(&db, &edges);
    assert_eq!(count(&db, TC_COUNT), n * n);
    assert_eq!(
        count(&db, REACH_COUNT),
        n,
        "seeded reach on a cycle = all n"
    );
}

/// Seeded reachability on a deterministic pseudo-random sparse digraph,
/// checked against a Rust-side BFS — the defined-equivalence oracle pattern.
/// No closed form could hide a joint bug; the two implementations share
/// nothing but the edge list.
#[test]
fn random_digraph_reach_matches_bfs_oracle() {
    // Deterministic LCG (no external RNG, no ambient entropy).
    let mut state = 0x2545F4914F6CDD1Du64;
    let mut next = move |bound: u64| {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) % bound
    };
    let n = 2000u64;
    let m = 6000;
    let mut edges = Vec::with_capacity(m);
    for _ in 0..m {
        let a = next(n) + 1;
        let b = next(n) + 1;
        edges.push((a as i64, b as i64));
    }
    // BFS from node 1.
    let mut adj: Vec<Vec<u64>> = vec![vec![]; (n + 1) as usize];
    for &(a, b) in &edges {
        adj[a as usize].push(b as u64);
    }
    let mut seen = HashSet::new();
    let mut q = VecDeque::from([1u64]);
    while let Some(x) = q.pop_front() {
        for &y in &adj[x as usize] {
            if seen.insert(y) {
                q.push_back(y);
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let db = edge_db(&dir.path().join("rand.db"));
    put_edges(&db, &edges);
    assert_eq!(
        count(&db, REACH_COUNT),
        seen.len() as i64,
        "Datalog seeded reach disagrees with the BFS oracle"
    );
}

/// Same-generation on a full binary tree of depth d: two distinct nodes are in
/// `sg` iff they share an ancestor at equal distance — on a full binary tree
/// every pair of distinct same-depth nodes qualifies, so
/// `sg = Σ_depth 2^depth · (2^depth − 1)`.
#[test]
fn same_generation_exact_on_binary_tree() {
    let depth = 7u32;
    let dir = tempfile::tempdir().unwrap();
    let db = edge_db(&dir.path().join("tree.db"));
    let mut edges = vec![];
    for parent in 1..(1i64 << depth) {
        edges.push((parent, parent * 2));
        edges.push((parent, parent * 2 + 1));
    }
    put_edges(&db, &edges);
    let expected: i64 = (1..=depth).map(|d| (1i64 << d) * ((1i64 << d) - 1)).sum();
    assert_eq!(count(&db, SG_COUNT), expected);
}

// ---------------------------------------------------------------------------
// Tier B — nightly deep-fixpoint pathology cap (release, planner-guard.yml)
// ---------------------------------------------------------------------------

/// 10k semi-naive iterations (a chain's diameter IS the iteration count) with
/// an exact answer and a generous pathology cap. This is the workload the
/// engine is supposed to be fastest at; a planner or fixpoint regression that
/// makes per-iteration cost superlinear shows up here as minutes, not ms.
#[test]
#[ignore = "nightly tier — run in release via planner-guard.yml"]
fn deep_chain_reach_nightly() {
    let n = 10_000i64;
    let dir = tempfile::tempdir().unwrap();
    let db = edge_db(&dir.path().join("deep.db"));
    let edges: Vec<_> = (1..n).map(|i| (i, i + 1)).collect();
    put_edges(&db, &edges);
    let started = Instant::now();
    assert_eq!(count(&db, REACH_COUNT), n - 1);
    let elapsed = started.elapsed();
    println!("deep-chain reach, {n} iterations: {elapsed:?}");
    assert!(
        elapsed.as_secs() < 60,
        "deep-fixpoint pathology: 10k-iteration chain reach took {elapsed:?} \
         (cap 60s, release); a healthy engine does this in seconds"
    );
}
