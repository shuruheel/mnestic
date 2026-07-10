/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! End-to-end proof for the cached graph projection
//! (`docs/specs/graph-projection.md` §5), through the public script surface.
//!
//! Everything here is observed the way a user would observe it: `::graph list`
//! reports whether a variant is resident and when it was built and last used,
//! so a MISS is a *new* `built_at` and a HIT is the old one with a moved
//! `last_used`. The in-crate `mod cache_tests` drives the consume and produce
//! rules directly; this file proves the surface those rules sit behind.

#![cfg(feature = "graph-algo")]

use std::collections::BTreeSet;

use cozo::{DataValue, DbInstance, NamedRows, ScriptMutability};

// ---------------------------------------------------------------- fixtures

fn mem() -> DbInstance {
    DbInstance::new("mem", "", "").unwrap()
}

fn run(db: &DbInstance, script: &str) -> NamedRows {
    db.run_default(script)
        .unwrap_or_else(|e| panic!("script failed: {script}\n{e:?}"))
}

fn err(db: &DbInstance, script: &str) -> String {
    format!(
        "{:?}",
        db.run_default(script)
            .expect_err("expected this script to fail")
    )
}

fn rows(res: &NamedRows) -> Vec<Vec<DataValue>> {
    res.rows.clone()
}

/// A tiny directed graph: 1→2→3→1 is a cycle, 4→5 is a tail, 9 is isolated.
/// Weights are chosen so that the cheapest 1→3 route is the two-hop 1→2→3.
fn graph_db() -> DbInstance {
    let db = mem();
    run(
        &db,
        r#"
        :create knows {a: Int, b: Int => w: Float}
        "#,
    );
    run(
        &db,
        r#"
        ?[a, b, w] <- [[1,2,1.0],[2,3,1.0],[3,1,5.0],[4,5,2.0]] :put knows {a, b => w}
        "#,
    );
    run(&db, ":create person {id: Int}");
    run(&db, "?[id] <- [[1],[2],[3],[4],[5],[9]] :put person {id}");
    db
}

// ------------------------------------------------- `::graph list` as oracle

/// `(variant, est_bytes, built_at, last_used)` of every resident variant.
fn listed(db: &DbInstance) -> Vec<(String, i64, f64, f64)> {
    rows(&run(db, "::graph list"))
        .into_iter()
        .filter(|r| r[3] != DataValue::Null)
        .map(|r| {
            (
                r[3].get_str().unwrap().to_string(),
                r[4].get_int().unwrap(),
                r[5].get_float().unwrap(),
                r[6].get_float().unwrap(),
            )
        })
        .collect()
}

fn resident(db: &DbInstance) -> usize {
    listed(db).len()
}

/// The `built_at` of the single resident variant. Panics if there isn't exactly one.
fn built_at(db: &DbInstance) -> f64 {
    let l = listed(db);
    assert_eq!(l.len(), 1, "expected exactly one resident variant: {l:?}");
    l[0].2
}

/// `built_at` moves ⇒ the entry was rebuilt. The clock has microsecond
/// resolution here, so two builds in the same test are always distinguishable
/// — but only if something separates them, which every mutation does.
fn assert_rebuilt(db: &DbInstance, before: f64) {
    let after = built_at(db);
    assert!(
        after > before,
        "expected a rebuild, but built_at stayed at {before}"
    );
}

fn assert_reused(db: &DbInstance, before: f64) {
    let after = built_at(db);
    assert_eq!(
        after, before,
        "expected a cache hit, but the variant was rebuilt"
    );
}

// ------------------------------------------------------------ the surface

#[test]
fn create_list_and_drop_round_trip() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows, nodes: person}");

    let listing = run(&db, "::graph list");
    assert_eq!(
        listing.headers,
        vec![
            "name",
            "edges",
            "nodes",
            "variant",
            "est_bytes",
            "built_at",
            "last_used"
        ]
    );
    // Cold: one row, definition only.
    assert_eq!(rows(&listing).len(), 1);
    let row = &rows(&listing)[0];
    assert_eq!(row[0], DataValue::from("g"));
    assert_eq!(row[1], DataValue::from("knows"));
    assert_eq!(row[2], DataValue::from("person"));
    assert_eq!(row[3], DataValue::Null);
    assert_eq!(row[4], DataValue::Null);

    run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')");
    assert_eq!(resident(&db), 1);

    run(&db, "::graph drop g");
    assert_eq!(rows(&run(&db, "::graph list")).len(), 0);
}

#[test]
fn a_source_may_be_named_bare_or_quoted() {
    let db = graph_db();
    run(&db, "::graph create bare {edges: knows}");
    run(&db, "::graph create quoted {edges: 'knows'}");
    let names: BTreeSet<String> = rows(&run(&db, "::graph list"))
        .into_iter()
        .map(|r| r[0].get_str().unwrap().to_string())
        .collect();
    assert_eq!(names, BTreeSet::from(["bare".to_string(), "quoted".into()]));
}

#[test]
fn a_projection_may_be_named_bare_or_quoted_at_the_call_site() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    let quoted = run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')");
    let bare = run(&db, "?[n, c] <~ ConnectedComponents(graph: g)");
    assert_eq!(rows(&quoted), rows(&bare));
    assert_eq!(resident(&db), 1);
}

#[test]
fn creating_the_same_projection_twice_is_a_loud_error() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    assert!(err(&db, "::graph create g {edges: knows}").contains("already exists"));
}

#[test]
fn dropping_an_unknown_projection_is_a_loud_error() {
    let db = graph_db();
    assert!(err(&db, "::graph drop nope").contains("not found"));
}

#[test]
fn using_an_unknown_projection_is_a_loud_error() {
    let db = graph_db();
    let msg = err(&db, "?[n, c] <~ ConnectedComponents(graph: 'nope')");
    assert!(msg.contains("not found"), "{msg}");
    assert!(msg.contains("re-created after restart"), "{msg}");
}

#[test]
fn create_rejects_an_unknown_option() {
    let db = graph_db();
    let msg = err(&db, "::graph create g {edges: knows, weight: w}");
    assert!(msg.contains("unknown option 'weight'"), "{msg}");
}

#[test]
fn create_requires_an_edges_relation() {
    let db = graph_db();
    let msg = err(&db, "::graph create g {nodes: person}");
    assert!(msg.contains("requires an `edges` relation"), "{msg}");
}

#[test]
fn create_validates_its_sources_before_registering_anything() {
    let db = graph_db();
    assert!(err(&db, "::graph create g {edges: absent}").contains("Cannot find"));
    assert!(err(&db, "::graph create g {edges: person}").contains("arity 1"));
    // The failures above must not have half-registered `g`.
    assert_eq!(rows(&run(&db, "::graph list")).len(), 0);
    run(&db, "::graph create g {edges: knows}");
}

#[test]
fn a_projection_survives_inside_an_imperative_script() {
    let db = graph_db();
    run(&db, "{::graph create g {edges: knows}}");
    assert_eq!(rows(&run(&db, "::graph list")).len(), 1);
    // …and `::graph list` reads back through the same in-script sysop path.
    let listed = run(&db, "{::graph list}");
    assert_eq!(rows(&listed).len(), 1);
    run(&db, "{::graph drop g}");
    assert_eq!(rows(&run(&db, "::graph list")).len(), 0);
}

#[test]
fn a_projection_created_earlier_in_the_script_is_usable_later_in_it() {
    // This is the load-bearing reason `::graph create` registers immediately
    // rather than at commit: the natural setup script defines and uses the
    // projection in one transaction.
    let db = graph_db();
    let res = run(
        &db,
        r#"
        {::graph create g {edges: knows}}
        {?[n, c] <~ ConnectedComponents(graph: 'g')}
        "#,
    );
    assert_eq!(rows(&res).len(), 5);
}

#[test]
fn registry_changes_survive_their_scripts_abort_by_design() {
    // The registry is process-global in-memory state, deliberately outside
    // transaction semantics (like the FTS stats cache): deferring `::graph
    // create` to commit would break same-script use, above. The consequence,
    // pinned here so it stays a decision rather than becoming an accident: a
    // create or drop SURVIVES its script's abort. Definitions hold only
    // names and every use re-resolves them, so this can never produce a wrong
    // answer — an orphaned definition errors loudly at use, and the escape is
    // `::graph drop`.
    let db = graph_db();
    let msg = err(
        &db,
        r#"
        {::graph create g {edges: knows}}
        {?[x] <- [[1]] :put no_such_relation {x}}
        "#,
    );
    assert!(msg.contains("Cannot find"), "{msg}");
    assert_eq!(
        rows(&run(&db, "::graph list")).len(),
        1,
        "the create survived the abort"
    );
    run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')");

    let msg = err(
        &db,
        r#"
        {::graph drop g}
        {?[x] <- [[1]] :put no_such_relation {x}}
        "#,
    );
    assert!(msg.contains("Cannot find"), "{msg}");
    assert_eq!(
        rows(&run(&db, "::graph list")).len(),
        0,
        "the drop survived the abort too"
    );
}

#[test]
fn create_and_drop_are_refused_in_read_only_mode() {
    let db = graph_db();
    let ro = |s: &str| {
        db.run_script(s, Default::default(), ScriptMutability::Immutable)
            .map_err(|e| format!("{e:?}"))
    };
    assert!(ro("::graph create g {edges: knows}")
        .unwrap_err()
        .contains("read-only"));
    run(&db, "::graph create g {edges: knows}");
    assert!(ro("::graph drop g").unwrap_err().contains("read-only"));
    // Listing and *using* a projection are reads.
    assert_eq!(rows(&ro("::graph list").unwrap()).len(), 1);
    assert_eq!(
        rows(&ro("?[n, c] <~ ConnectedComponents(graph: 'g')").unwrap()).len(),
        5
    );
}

#[test]
fn the_graph_option_is_rejected_on_a_rule_that_cannot_use_it() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    // The lazy family evaluates per-tuple expressions against its edge relation,
    // which a CSR does not carry: excluded by design, not by omission. Unknown
    // options are ignored engine-wide, so without the guard this would silently
    // rebuild the graph the slow way.
    let msg = err(
        &db,
        "?[n, d, o, i] <~ DegreeCentrality(*knows[a, b, w], graph: 'g')",
    );
    assert!(
        msg.contains("cannot take its edges from a graph projection"),
        "{msg}"
    );
    // …and so is a utility that has no graph at all.
    let msg = err(&db, "?[a] <~ Constant(data: [[1]], graph: 'g')");
    assert!(
        msg.contains("cannot take its edges from a graph projection"),
        "{msg}"
    );
}

#[test]
fn a_simple_fixed_rule_still_receives_an_option_named_graph() {
    // SimpleFixedRule forwards every option to its closure, so an option
    // spelled `graph` was never silently dropped — the misfire the parse-time
    // guard exists to prevent. A pre-0.11 out-of-tree rule reading it must
    // keep working; the name gains no projection semantics for it.
    let db = graph_db();
    db.register_fixed_rule(
        "EchoGraphOption".to_string(),
        cozo::SimpleFixedRule::new(1, |_inputs, options| {
            let val = options
                .get("graph")
                .cloned()
                .unwrap_or(DataValue::from("MISSING"));
            Ok(NamedRows::new(vec!["x".to_string()], vec![vec![val]]))
        }),
    )
    .unwrap();
    let res = run(&db, "?[x] <~ EchoGraphOption(graph: 'social')");
    assert_eq!(rows(&res), vec![vec![DataValue::from("social")]]);
}

// ---------------------------------------------------- source-kind rejection

#[test]
fn an_index_relation_cannot_be_a_source() {
    let db = graph_db();
    run(&db, "::index create knows:by_b {b}");
    let msg = err(&db, "::graph create g {edges: 'knows:by_b'}");
    assert!(msg.contains("index relation"), "{msg}");
}

#[test]
fn a_temp_relation_cannot_be_a_source() {
    let db = graph_db();
    let msg = err(
        &db,
        r#"
        {:create _t {a: Int, b: Int}}
        {::graph create g {edges: _t}}
        "#,
    );
    assert!(msg.contains("temporary relation"), "{msg}");
}

#[test]
fn a_transaction_time_relation_cannot_be_a_source() {
    let db = mem();
    run(&db, ":create tt {a: Int, b: Int, at: TxTime}");
    let msg = err(&db, "::graph create g {edges: tt}");
    assert!(msg.contains("transaction-time relation"), "{msg}");
    // And a plain validity relation is *not* rejected: its selector-less scan
    // returns every version row on every engine path, projection included.
    run(&db, ":create vt {a: Int, b: Int, at: Validity}");
    run(&db, "::graph create ok {edges: vt}");
}

#[test]
fn renaming_a_tt_relation_into_a_source_name_errors_at_use() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')");
    run(&db, ":create tt {a: Int, b: Int, at: TxTime}");
    run(&db, "::rename knows -> gone, tt -> knows");
    // Source kinds are re-checked at every use, not only at create.
    let msg = err(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')");
    assert!(msg.contains("transaction-time relation"), "{msg}");
}

// --------------------------------------------------------- caching behaviour

#[test]
fn a_cold_call_builds_and_a_warm_one_reuses() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')");
    let first = built_at(&db);
    run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')");
    assert_reused(&db, first);
}

#[test]
fn a_hit_moves_last_used_but_not_built_at() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')");
    let (_, _, built, first_used) = listed(&db)[0].clone();
    run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')");
    let (_, _, built2, second_used) = listed(&db)[0].clone();
    assert_eq!(built, built2);
    // Strictly greater: `>=` is satisfied by a last_used that never moves at
    // all, which is precisely the defect this test exists to catch. The clock
    // is sub-microsecond f64 seconds and the two lookups are separate script
    // runs, so equality can only mean the hit did not touch the field.
    assert!(
        second_used > first_used,
        "the hit did not move last_used ({second_used} vs {first_used})"
    );
}

#[test]
fn each_variant_is_built_and_listed_separately() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')"); // undirected, unweighted
    run(&db, "?[i, n] <~ TopSort(graph: 'g')"); // directed, unweighted
    run(
        &db,
        "?[a, b, c] <~ MinimumSpanningForestKruskal(graph: 'g')",
    ); // undirected+weighted

    let variants: BTreeSet<String> = listed(&db).into_iter().map(|v| v.0).collect();
    assert_eq!(
        variants,
        BTreeSet::from([
            "undirected".to_string(),
            "directed".into(),
            "undirected+weighted".into(),
        ])
    );
}

#[test]
fn writing_a_source_forces_a_rebuild() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')");
    let first = built_at(&db);

    run(&db, "?[a, b, w] <- [[5, 6, 1.0]] :put knows {a, b => w}");
    // The entry is freed eagerly, not merely invalidated.
    assert_eq!(resident(&db), 0);

    run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')");
    assert_rebuilt(&db, first);
}

#[test]
fn writing_the_nodes_relation_forces_a_rebuild() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows, nodes: person}");
    run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')");
    let first = built_at(&db);
    run(&db, "?[id] <- [[42]] :put person {id}");
    assert_eq!(resident(&db), 0);
    run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')");
    assert_rebuilt(&db, first);
}

#[test]
fn writing_an_unrelated_relation_leaves_the_entry_alone() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')");
    let first = built_at(&db);
    run(&db, ":create other {x: Int}");
    run(&db, "?[x] <- [[1]] :put other {x}");
    run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')");
    assert_reused(&db, first);
}

#[test]
fn a_transaction_reads_its_own_uncommitted_writes_and_publishes_nothing() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    // One imperative transaction: write, then run the algorithm. The algorithm
    // must see the new edge (6→7), and must not leave it in the cache.
    let res = run(
        &db,
        r#"
        {?[a, b, w] <- [[6, 7, 1.0]] :put knows {a, b => w}}
        {?[n, c] <~ ConnectedComponents(graph: 'g')}
        "#,
    );
    let seen: BTreeSet<i64> = rows(&res).iter().map(|r| r[0].get_int().unwrap()).collect();
    assert!(seen.contains(&6) && seen.contains(&7));
    assert_eq!(resident(&db), 0, "a dirty transaction must publish nothing");
}

#[test]
fn removing_a_source_frees_the_entry_and_the_name_stops_resolving() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')");
    assert_eq!(resident(&db), 1);
    run(&db, "::remove knows");
    assert_eq!(resident(&db), 0);
    assert!(err(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')").contains("Cannot find"));
}

#[test]
fn replacing_a_source_mints_a_new_id_and_kills_the_entry() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')");
    let first = built_at(&db);
    run(
        &db,
        "?[a, b, w] <- [[7, 8, 1.0]] :replace knows {a, b => w}",
    );
    assert_eq!(resident(&db), 0);
    let res = run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')");
    assert_rebuilt(&db, first);
    assert_eq!(rows(&res).len(), 2);
}

#[test]
fn swapping_two_source_relations_forces_a_miss() {
    let db = graph_db();
    run(&db, ":create other {a: Int, b: Int}");
    run(&db, "?[a, b] <- [[100, 200]] :put other {a, b}");
    run(&db, "::graph create g {edges: knows}");
    run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')");

    // `::rename` moves no tuples and bumps no token; only the slot-id binding
    // in the consume rule can catch this. Three pairs, because `rename_relation`
    // refuses to rename onto a live name.
    run(&db, "::rename knows -> tmp, other -> knows, tmp -> other");

    let res = run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')");
    let seen: BTreeSet<i64> = rows(&res).iter().map(|r| r[0].get_int().unwrap()).collect();
    assert_eq!(seen, BTreeSet::from([100, 200]), "served the wrong graph");
}

#[test]
fn dropping_a_projection_frees_its_variants() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')");
    run(&db, "?[i, n] <~ TopSort(graph: 'g')");
    assert_eq!(resident(&db), 2);
    run(&db, "::graph drop g");
    run(&db, "::graph create g {edges: knows}");
    assert_eq!(resident(&db), 0);
}

// ------------------------------------------------ equivalence: the 12 ports

/// Row-set equality between the positional and the projection form.
fn assert_same_rows(db: &DbInstance, positional: &str, projected: &str) {
    let a = rows(&run(db, positional));
    let b = rows(&run(db, projected));
    assert!(!a.is_empty(), "fixture produced no rows: {positional}");
    assert_eq!(a, b, "\npositional: {positional}\nprojected:  {projected}");
}

/// CC and SCC group ids are arbitrary labels; only the partition is meaningful.
fn partition(res: &NamedRows) -> BTreeSet<BTreeSet<String>> {
    let mut groups: std::collections::BTreeMap<String, BTreeSet<String>> = Default::default();
    for row in &res.rows {
        groups
            .entry(format!("{:?}", row[1]))
            .or_default()
            .insert(format!("{:?}", row[0]));
    }
    groups.into_values().collect()
}

fn assert_same_partition(db: &DbInstance, positional: &str, projected: &str) {
    let a = partition(&run(db, positional));
    let b = partition(&run(db, projected));
    assert!(!a.is_empty(), "fixture produced no rows: {positional}");
    assert_eq!(a, b, "\npositional: {positional}\nprojected:  {projected}");
}

#[test]
fn connected_components_agree_at_partition_level() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    assert_same_partition(
        &db,
        "?[n, c] <~ ConnectedComponents(*knows[a, b, w])",
        "?[n, c] <~ ConnectedComponents(graph: 'g')",
    );
}

#[test]
fn connected_components_take_a_positional_nodes_overlay_after_the_shift() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    // Input 1 positionally, input 0 under `graph:`. The isolated 9 must appear
    // in both, as its own singleton component.
    let positional = run(
        &db,
        "?[n, c] <~ ConnectedComponents(*knows[a, b, w], *person[id])",
    );
    let projected = run(
        &db,
        "?[n, c] <~ ConnectedComponents(*person[id], graph: 'g')",
    );
    assert_eq!(partition(&positional), partition(&projected));
    assert_eq!(rows(&projected).len(), 6);
}

#[test]
fn a_vertex_repeated_in_the_positional_overlay_is_emitted_once() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    // The overlay's first column is `a`, which repeats: an unkeyed vertex list
    // is the normal shape when the nodes input is an edge-like relation or a
    // rule. The positional form dedupes by interning into `inv_indices`; the
    // projection form cannot touch that shared map and dedupes locally.
    run(&db, ":create pairs {a: Int, b: Int}");
    run(&db, "?[a, b] <- [[100, 1], [100, 2]] :put pairs {a, b}");

    let projected = run(
        &db,
        "?[n, c] <~ ConnectedComponents(*pairs[a, b], graph: 'g')",
    );
    let positional = run(
        &db,
        "?[n, c] <~ ConnectedComponents(*knows[a, b, w], *pairs[x, y])",
    );
    assert_eq!(partition(&projected), partition(&positional));
    assert_eq!(
        rows(&projected)
            .iter()
            .filter(|r| r[0] == DataValue::from(100))
            .count(),
        1,
        "vertex 100 got two components"
    );
}

#[test]
fn a_nodes_bearing_projection_makes_isolated_vertices_real() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows, nodes: person}");
    // The isolated 9 comes out of Tarjan, not out of the overlay.
    let res = run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')");
    assert_eq!(rows(&res).len(), 6);
    assert_eq!(
        partition(&res),
        partition(&run(
            &db,
            "?[n, c] <~ ConnectedComponents(*knows[a, b, w], *person[id])"
        ))
    );
}

#[test]
fn strongly_connected_components_agree_at_partition_level() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    assert_same_partition(
        &db,
        "?[n, c] <~ StronglyConnectedComponents(*knows[a, b, w])",
        "?[n, c] <~ StronglyConnectedComponents(graph: 'g')",
    );
    // …and the two really do differ: SCC splits 4→5, CC does not.
    let scc = partition(&run(
        &db,
        "?[n, c] <~ StronglyConnectedComponents(graph: 'g')",
    ));
    let cc = partition(&run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')"));
    assert_ne!(scc, cc);
}

#[test]
fn pagerank_agrees() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    assert_same_rows(
        &db,
        "?[n, r] <~ PageRank(*knows[a, b, w])",
        "?[n, r] <~ PageRank(graph: 'g')",
    );
}

#[test]
fn pagerank_isolated_vertices_come_from_the_projections_nodes() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows, nodes: person}");
    assert_same_rows(
        &db,
        "?[n, r] <~ PageRank(*knows[a, b, w], *person[id])",
        "?[n, r] <~ PageRank(graph: 'g')",
    );
    assert_eq!(rows(&run(&db, "?[n, r] <~ PageRank(graph: 'g')")).len(), 6);
}

#[test]
fn pagerank_refuses_a_positional_nodes_relation_alongside_a_projection() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    let msg = err(&db, "?[n, r] <~ PageRank(*person[id], graph: 'g')");
    assert!(msg.contains("positional nodes relation"), "{msg}");
    assert!(msg.contains("nodes:"), "{msg}");
}

#[test]
fn clustering_coefficients_agree() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    assert_same_rows(
        &db,
        "?[n, c, t, d] <~ ClusteringCoefficients(*knows[a, b, w])",
        "?[n, c, t, d] <~ ClusteringCoefficients(graph: 'g')",
    );
}

#[test]
fn top_sort_agrees() {
    let db = mem();
    run(&db, ":create dag {a: Int, b: Int}");
    run(&db, "?[a, b] <- [[1,2],[2,3],[1,3],[4,5]] :put dag {a, b}");
    run(&db, "::graph create g {edges: dag}");
    assert_same_rows(
        &db,
        "?[i, n] <~ TopSort(*dag[a, b])",
        "?[i, n] <~ TopSort(graph: 'g')",
    );
}

#[test]
fn betweenness_and_closeness_centrality_agree() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    assert_same_rows(
        &db,
        "?[n, c] <~ BetweennessCentrality(*knows[a, b, w])",
        "?[n, c] <~ BetweennessCentrality(graph: 'g')",
    );
    assert_same_rows(
        &db,
        "?[n, c] <~ ClosenessCentrality(*knows[a, b, w])",
        "?[n, c] <~ ClosenessCentrality(graph: 'g')",
    );
}

#[test]
fn dijkstra_agrees_and_respects_the_input_shift() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    run(&db, ":create start {id: Int}");
    run(&db, "?[id] <- [[1]] :put start {id}");
    run(&db, ":create goal {id: Int}");
    run(&db, "?[id] <- [[3]] :put goal {id}");

    // Unterminated: every reachable node.
    assert_same_rows(
        &db,
        "?[s, t, c, p] <~ ShortestPathDijkstra(*knows[a, b, w], *start[u])",
        "?[s, t, c, p] <~ ShortestPathDijkstra(*start[u], graph: 'g')",
    );
    // Terminated at 3: strictly fewer rows. An `input_base` off by one would
    // either lose the termination relation or mistake `start` for it.
    let unterminated = rows(&run(
        &db,
        "?[s, t, c, p] <~ ShortestPathDijkstra(*start[u], graph: 'g')",
    ));
    let terminated = rows(&run(
        &db,
        "?[s, t, c, p] <~ ShortestPathDijkstra(*start[u], *goal[v], graph: 'g')",
    ));
    assert!(
        terminated.len() < unterminated.len(),
        "the termination relation had no effect: {terminated:?}"
    );
    assert_same_rows(
        &db,
        "?[s, t, c, p] <~ ShortestPathDijkstra(*knows[a, b, w], *start[u], *goal[v])",
        "?[s, t, c, p] <~ ShortestPathDijkstra(*start[u], *goal[v], graph: 'g')",
    );
}

#[test]
fn yen_agrees() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    run(&db, ":create start {id: Int}");
    run(&db, "?[id] <- [[1]] :put start {id}");
    run(&db, ":create goal {id: Int}");
    run(&db, "?[id] <- [[3]] :put goal {id}");
    assert_same_rows(
        &db,
        "?[s, t, c, p] <~ KShortestPathYen(*knows[a,b,w], *start[u], *goal[v], k: 2)",
        "?[s, t, c, p] <~ KShortestPathYen(*start[u], *goal[v], k: 2, graph: 'g')",
    );
}

#[test]
fn prim_agrees_and_its_default_start_skips_an_isolated_vertex() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    assert_same_rows(
        &db,
        "?[a, b, c] <~ MinimumSpanningTreePrim(*knows[a, b, w])",
        "?[a, b, c] <~ MinimumSpanningTreePrim(graph: 'g')",
    );

    // A nodes-bearing projection can seat an isolated vertex at id 0, where the
    // old default start would have spanned nothing.
    let db2 = mem();
    run(&db2, ":create e {a: Int, b: Int => w: Float}");
    run(&db2, "?[a, b, w] <- [[1,2,1.0]] :put e {a, b => w}");
    run(&db2, ":create v {id: Int}");
    run(&db2, "?[id] <- [[0],[1],[2]] :put v {id}"); // 0 is isolated and first
    run(&db2, "::graph create g {edges: e, nodes: v}");
    let mst = rows(&run(
        &db2,
        "?[a, b, c] <~ MinimumSpanningTreePrim(graph: 'g')",
    ));
    assert_eq!(
        mst.len(),
        1,
        "the isolated start swallowed the tree: {mst:?}"
    );

    // …but a user-supplied starting relation still gets its diagnostic.
    run(&db2, ":create s {id: Int}");
    let msg = err(
        &db2,
        "?[a, b, c] <~ MinimumSpanningTreePrim(*s[id], graph: 'g')",
    );
    assert!(msg.contains("empty"), "{msg}");
}

#[test]
fn kruskal_agrees() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    assert_same_rows(
        &db,
        "?[a, b, c] <~ MinimumSpanningForestKruskal(*knows[a, b, w])",
        "?[a, b, c] <~ MinimumSpanningForestKruskal(graph: 'g')",
    );
}

#[test]
fn label_propagation_agrees_at_partition_level() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    // Labels are seeded randomly; the vertex set is what must agree.
    let a = run(&db, "?[c, n] <~ LabelPropagation(*knows[a, b, w])");
    let b = run(&db, "?[c, n] <~ LabelPropagation(graph: 'g')");
    let nodes = |r: &NamedRows| -> BTreeSet<String> {
        r.rows.iter().map(|row| format!("{:?}", row[1])).collect()
    };
    assert_eq!(nodes(&a), nodes(&b));
}

#[test]
fn louvain_agrees() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    assert_same_rows(
        &db,
        "?[l, n] <~ CommunityDetectionLouvain(*knows[a, b, w])",
        "?[l, n] <~ CommunityDetectionLouvain(graph: 'g')",
    );
}

#[test]
fn an_unweighted_source_gives_every_edge_unit_weight() {
    // `knows` has no third column here, so the weighted variants default to 1.0
    // and Kruskal's forest must weigh exactly one per edge.
    let db = mem();
    run(&db, ":create e {a: Int, b: Int}");
    run(&db, "?[a, b] <- [[1,2],[2,3]] :put e {a, b}");
    run(&db, "::graph create g {edges: e}");
    let mst = rows(&run(
        &db,
        "?[a, b, c] <~ MinimumSpanningForestKruskal(graph: 'g')",
    ));
    assert_eq!(mst.len(), 2);
    for row in mst {
        assert_eq!(row[2].get_float().unwrap(), 1.0);
    }
}

#[test]
fn a_json_keyed_projection_is_charged_its_real_bytes() {
    // Json is a storable key type and a blob vertex is interned as two owned
    // deep clones, so a flat per-key charge would let the real footprint
    // exceed the 512 MiB ceiling without bound while `::graph list` reported
    // a near-empty cache. The estimate must scale with the payload.
    let db = mem();
    run(&db, ":create e {a: Json, b: Json}");
    let blob = "x".repeat(10_000);
    db.run_script(
        "?[a, b] <- [[json_object('k', $blob), json_object('k', $k2)]] :put e {a, b}",
        std::collections::BTreeMap::from([
            ("blob".to_string(), DataValue::from(blob.as_str())),
            (
                "k2".to_string(),
                DataValue::from(format!("{blob}2").as_str()),
            ),
        ]),
        ScriptMutability::Mutable,
    )
    .unwrap();
    run(&db, "::graph create g {edges: e}");
    run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')");
    let (_, est_bytes, _, _) = listed(&db)[0].clone();
    assert!(
        est_bytes > 2 * 10_000,
        "two ~10 KB Json vertices estimated at only {est_bytes} bytes"
    );
}

// ----------------------------------------------------- negative-weight policy

/// A negative-weight fixture whose projection is `g`.
fn negative_db() -> DbInstance {
    let db = mem();
    run(&db, ":create e {a: Int, b: Int => w: Float}");
    run(
        &db,
        "?[a, b, w] <- [[1,2,-1.0],[2,3,1.0]] :put e {a, b => w}",
    );
    run(&db, ":create start {id: Int}");
    run(&db, "?[id] <- [[1]] :put start {id}");
    run(&db, "::graph create g {edges: e}");
    db
}

#[test]
fn one_weighted_variant_serves_permissive_and_strict_consumers() {
    let db = negative_db();

    // Permissive: builds the variant and consumes it, negative weight and all.
    let mst = run(
        &db,
        "?[a, b, c] <~ MinimumSpanningForestKruskal(graph: 'g')",
    );
    assert_eq!(rows(&mst).len(), 2);
    let built = built_at(&db);

    // Strict, on the same `undirected+weighted` variant: it takes the cache hit
    // and *then* refuses, loudly, naming itself and the projection.
    let msg = err(
        &db,
        "?[s, t, c, p] <~ ShortestPathDijkstra(*start[u], undirected: true, graph: 'g')",
    );
    assert!(msg.contains("negative edge weight"), "{msg}");
    assert!(msg.contains("ShortestPathDijkstra"), "{msg}");
    assert!(msg.contains("'g'"), "{msg}");

    // The refusal neither evicted nor rebuilt: one variant, one build.
    assert_eq!(resident(&db), 1);
    assert_reused(&db, built);
}

#[test]
fn a_strict_refusal_still_publishes_the_variant_it_refused() {
    let db = negative_db();
    // Dijkstra defaults to `undirected: false`, so this builds `directed+weighted`,
    // publishes it, and only then applies its own negative-weight policy. The
    // variant is fresh and correct — the *consumer* is what is strict — so the
    // next permissive consumer of that same variant must take a cache hit.
    assert!(err(
        &db,
        "?[s, t, c, p] <~ ShortestPathDijkstra(*start[u], graph: 'g')"
    )
    .contains("negative edge weight"));
    let built = built_at(&db);
    run(&db, "?[c, n] <~ LabelPropagation(graph: 'g')");
    assert_reused(&db, built);
    assert_eq!(listed(&db)[0].0, "directed+weighted");
}

#[test]
fn a_strict_positional_call_still_fails_at_the_weight_it_reads() {
    let db = mem();
    run(&db, ":create e {a: Int, b: Int => w: Float}");
    run(&db, "?[a, b, w] <- [[1,2,-1.0]] :put e {a, b => w}");
    // Unchanged from before projections existed: the scan rejects it.
    let msg = err(&db, "?[n, c] <~ BetweennessCentrality(*e[a, b, w])");
    assert!(msg.contains("edge weight"), "{msg}");
}

/// One row per strict port whose flag no other test pins. The positional and
/// projected arms share one `VariantSpec`, so the *_agrees equivalence tests
/// are structurally blind to a flipped flag — both arms flip together. Only
/// the policy's own observable (reject vs accept) discriminates.
#[test]
fn every_strict_port_refuses_a_negative_weight_variant() {
    let db = negative_db();
    run(&db, ":create goal {id: Int}");
    run(&db, "?[id] <- [[3]] :put goal {id}");
    for (name, script) in [
        (
            "CommunityDetectionLouvain",
            "?[l, n] <~ CommunityDetectionLouvain(graph: 'g')",
        ),
        (
            "KShortestPathYen",
            "?[s, t, c, p] <~ KShortestPathYen(*start[u], *goal[v], k: 2, graph: 'g')",
        ),
        (
            "ClosenessCentrality",
            "?[n, c] <~ ClosenessCentrality(graph: 'g')",
        ),
    ] {
        // The diagnostic code, not the prose: miette wraps long messages, so
        // "negative edge weight" can straddle a line break.
        let msg = err(&db, script);
        assert!(msg.contains("projection_negative_weight"), "{name}: {msg}");
        assert!(msg.contains(name), "{name}: {msg}");
    }
}

/// …and the permissive ports consume the same variant without complaint.
#[test]
fn every_permissive_port_accepts_a_negative_weight_variant() {
    let db = negative_db();
    let mst = run(&db, "?[a, b, c] <~ MinimumSpanningTreePrim(graph: 'g')");
    assert_eq!(rows(&mst).len(), 2, "Prim must span the negative edge too");
    run(&db, "?[c, n] <~ LabelPropagation(graph: 'g')");
    run(
        &db,
        "?[a, b, c] <~ MinimumSpanningForestKruskal(graph: 'g')",
    );
}

/// The `undirected` option must reach the build for the option-taking ports.
/// The two arms of `graph_input` share the spec, so equivalence rows cannot
/// see a hardcoded direction; what can is the *result changing with the
/// option* on an asymmetric graph — plus an equivalence row at the
/// non-default setting so both forms agree there too.
#[test]
fn the_undirected_option_changes_the_answer_on_an_asymmetric_graph() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    for (name, directed, undirected, positional_undirected) in [
        (
            "PageRank",
            "?[n, r] <~ PageRank(graph: 'g')",
            "?[n, r] <~ PageRank(undirected: true, graph: 'g')",
            "?[n, r] <~ PageRank(*knows[a, b], undirected: true)",
        ),
        (
            "BetweennessCentrality",
            "?[n, c] <~ BetweennessCentrality(graph: 'g')",
            "?[n, c] <~ BetweennessCentrality(undirected: true, graph: 'g')",
            "?[n, c] <~ BetweennessCentrality(*knows[a, b, w], undirected: true)",
        ),
        (
            "ClosenessCentrality",
            "?[n, c] <~ ClosenessCentrality(graph: 'g')",
            "?[n, c] <~ ClosenessCentrality(undirected: true, graph: 'g')",
            "?[n, c] <~ ClosenessCentrality(*knows[a, b, w], undirected: true)",
        ),
    ] {
        let d = rows(&run(&db, directed));
        let u = rows(&run(&db, undirected));
        assert_ne!(d, u, "{name}: the undirected option had no effect");
        assert_eq!(
            u,
            rows(&run(&db, positional_undirected)),
            "{name}: undirected forms disagree"
        );
    }
}

// -------------------------------------------------------------- empty graphs

#[test]
fn an_empty_edge_relation_is_safe_for_every_ported_algorithm() {
    let db = mem();
    run(&db, ":create e {a: Int, b: Int => w: Float}");
    run(&db, ":create start {id: Int}");
    run(&db, "::graph create g {edges: e}");
    for script in [
        "?[n, c] <~ ConnectedComponents(graph: 'g')",
        "?[n, c] <~ StronglyConnectedComponents(graph: 'g')",
        "?[n, r] <~ PageRank(graph: 'g')",
        "?[n, c, t, d] <~ ClusteringCoefficients(graph: 'g')",
        "?[i, n] <~ TopSort(graph: 'g')",
        "?[n, c] <~ BetweennessCentrality(graph: 'g')",
        "?[n, c] <~ ClosenessCentrality(graph: 'g')",
        "?[a, b, c] <~ MinimumSpanningTreePrim(graph: 'g')",
        "?[a, b, c] <~ MinimumSpanningForestKruskal(graph: 'g')",
        "?[c, n] <~ LabelPropagation(graph: 'g')",
        "?[l, n] <~ CommunityDetectionLouvain(graph: 'g')",
        "?[s, t, c, p] <~ ShortestPathDijkstra(*start[id], graph: 'g')",
    ] {
        assert_eq!(rows(&run(&db, script)).len(), 0, "{script}");
    }
}

// ------------------------------------------------------------ memory ceiling

#[test]
fn capacity_zero_disables_caching_but_not_the_registry() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')");
    assert_eq!(resident(&db), 1);

    // Through the DbInstance dispatcher — the path every language binding uses.
    db.set_graph_projection_capacity(0);
    assert_eq!(resident(&db), 0, "shrinking to zero must evict");

    // Still answers, still lists, still never caches.
    let res = run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')");
    assert_eq!(rows(&res).len(), 5);
    assert_eq!(resident(&db), 0);
    assert_eq!(rows(&run(&db, "::graph list")).len(), 1);
}

#[test]
fn a_variant_larger_than_the_ceiling_is_built_for_every_query() {
    let db = graph_db();
    run(&db, "::graph create g {edges: knows}");
    db.set_graph_projection_capacity(1);
    let res = run(&db, "?[n, c] <~ ConnectedComponents(graph: 'g')");
    assert_eq!(rows(&res).len(), 5);
    assert_eq!(resident(&db), 0);
}
