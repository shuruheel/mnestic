/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Tests for the automatic factorized-count rewrite (mnestic fork, 0.10.5 —
//! items I + J; `query/factorize.rs`).
//!
//! THE deliverable is the randomized **differential** property suite
//! ([`differential_naive_equals_factorized`]): it generates hundreds of small
//! random schemas + queries and asserts the naive `count()` and the forced
//! factorized rewrite return BIT-IDENTICAL results — on BOTH the `mem` backend
//! and a SQLite `tempdir()`, because the two use different join operators
//! (`mem_mat_join` vs `stored_*_join` — see `matjoin_regression.rs`). A silently
//! wrong count is the worst possible defect, so correctness is asserted
//! exhaustively; the targeted tests below merely pin the individual firing
//! patterns and every non-firing guard.

use cozo::{DataValue, DbInstance, NamedRows, ScriptMutability};
use std::collections::BTreeMap;

fn mem_db() -> DbInstance {
    DbInstance::new("mem", "", Default::default()).unwrap()
}

fn sqlite_db(dir: &tempfile::TempDir, name: &str) -> DbInstance {
    DbInstance::new(
        "sqlite",
        dir.path().join(name).to_str().unwrap(),
        Default::default(),
    )
    .unwrap()
}

fn run_mut(db: &DbInstance, s: &str) {
    db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap();
}

fn run_mut_p(db: &DbInstance, s: &str, params: BTreeMap<String, DataValue>) {
    db.run_script(s, params, ScriptMutability::Mutable).unwrap();
}

/// Run a read query and return its rows, sorted (so row order never matters).
fn sorted_rows(db: &DbInstance, query: &str) -> Vec<Vec<DataValue>> {
    let mut rows = db
        .run_script(query, BTreeMap::new(), ScriptMutability::Immutable)
        .unwrap()
        .rows;
    rows.sort();
    rows
}

/// The rule names appearing in the compiled `::explain` plan.
fn plan_rule_names(db: &DbInstance, query: &str) -> Vec<String> {
    let plan: NamedRows = db
        .run_script(
            &format!("::explain {{ {query} }}"),
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .unwrap();
    let idx = plan.headers.iter().position(|h| h == "rule").unwrap();
    plan.rows
        .iter()
        .filter_map(|r| match &r[idx] {
            DataValue::Str(s) => Some(s.to_string()),
            _ => None,
        })
        .collect()
}

/// Did the rewrite fire? (The synthesized helper rules are `*fac…`-named, and
/// only appear in the compiled plan when the rewrite actually replaced the
/// entry.) Requires the toggle to be on.
fn fired(db: &DbInstance, query: &str) -> bool {
    plan_rule_names(db, query)
        .iter()
        .any(|n| n.starts_with("*fac"))
}

/// Insert `rows` (an all-key set relation) under `name`.
fn put_rows(db: &DbInstance, name: &str, arity: usize, rows: &[Vec<i64>]) {
    let cols: Vec<String> = (0..arity).map(|i| format!("c{i}")).collect();
    let typed = cols
        .iter()
        .map(|c| format!("{c}: Int"))
        .collect::<Vec<_>>()
        .join(", ");
    run_mut(db, &format!(":create {name} {{ {typed} }}"));
    let head = cols.join(", ");
    let dv_rows: Vec<DataValue> = rows
        .iter()
        .map(|r| DataValue::List(r.iter().map(|&x| DataValue::from(x)).collect()))
        .collect();
    let mut p = BTreeMap::new();
    p.insert("rows".to_string(), DataValue::List(dv_rows));
    run_mut_p(
        db,
        &format!("?[{head}] <- $rows :put {name} {{ {head} }}"),
        p,
    );
}

/// Assert that on `db` (a) the rewrite fires and (b) the forced-on result equals
/// the naive (forced-off) result.
fn assert_fires_and_matches(db: &DbInstance, query: &str) {
    db.set_query_factorization(false);
    let naive = sorted_rows(db, query);
    db.set_query_factorization(true);
    assert!(fired(db, query), "expected rewrite to fire on: {query}");
    let fac = sorted_rows(db, query);
    assert_eq!(naive, fac, "naive != factorized for: {query}");
}

/// Assert the rewrite declines (does NOT fire) and the result is unchanged.
fn assert_declines(db: &DbInstance, query: &str) {
    db.set_query_factorization(false);
    let naive = sorted_rows(db, query);
    db.set_query_factorization(true);
    assert!(!fired(db, query), "expected rewrite to DECLINE on: {query}");
    let same = sorted_rows(db, query);
    assert_eq!(naive, same, "declined rewrite changed the result: {query}");
}

// ------------------------------------------------------------------------
// The `cardinality-algebra.md` toy dataset (§2), used by the targeted tests.
// ------------------------------------------------------------------------

fn populate_toy(db: &DbInstance) {
    put_rows(
        db,
        "city_in",
        2,
        &[
            vec![10, 1],
            vec![11, 1],
            vec![12, 1],
            vec![12, 2],
            vec![13, 2],
            vec![14, 2],
        ],
    );
    put_rows(
        db,
        "lives_in",
        2,
        &[
            vec![1, 10],
            vec![1, 13],
            vec![2, 10],
            vec![3, 11],
            vec![4, 12],
            vec![5, 13],
            vec![6, 14],
        ],
    );
    put_rows(
        db,
        "member",
        2,
        &[
            vec![100, 1],
            vec![100, 2],
            vec![100, 4],
            vec![101, 2],
            vec![101, 3],
            vec![101, 5],
            vec![102, 4],
            vec![102, 6],
        ],
    );
    put_rows(
        db,
        "group_tag",
        2,
        &[
            vec![100, 200],
            vec![100, 201],
            vec![101, 201],
            vec![102, 202],
        ],
    );
    put_rows(
        db,
        "tag_class",
        2,
        &[
            vec![200, 300],
            vec![201, 300],
            vec![201, 301],
            vec![202, 302],
        ],
    );
    put_rows(
        db,
        "knows",
        2,
        &[
            vec![1, 2],
            vec![2, 1],
            vec![2, 3],
            vec![3, 4],
            vec![4, 2],
            vec![1, 3],
            vec![3, 5],
            vec![5, 6],
            vec![4, 5],
            vec![2, 4],
        ],
    );
}

fn scalar(db: &DbInstance, query: &str) -> i64 {
    let rows = sorted_rows(db, query);
    assert_eq!(rows.len(), 1, "expected one row for {query}");
    assert_eq!(rows[0].len(), 1, "expected one column for {query}");
    rows[0][0].get_int().expect("integer count")
}

// ------------------------------------------------------------------------
// Targeted firing patterns (run on both backends).
// ------------------------------------------------------------------------

const CHAIN: &str = "?[count(country)] := \
     *city_in[city, country], *lives_in[person, city], *member[group, person], \
     *group_tag[group, tag], *tag_class[tag, class]";

const STAR: &str = "?[count(person)] := \
     *member[group, person], *knows[friend, person], *lives_in[person, city]";

const STAR_TUP: &str = "?[count(tup)] := \
     *member[group, person], *knows[friend, person], *lives_in[person, city], \
     tup = [group, person, friend, city]";

const IE_NEQ: &str = "?[count(p2)] := \
     *knows[p1, p2], *knows[p2, p3], *member[group, p3], p1 != p3";

const GROUPBY: &str =
    "?[country, count(person)] := *city_in[city, country], *lives_in[person, city]";

fn check_targeted(db: &DbInstance) {
    // Pattern P1 — join-tree count DP. The `cardinality-algebra.md` §3.1 value.
    db.set_query_factorization(false);
    assert_eq!(scalar(db, CHAIN), 24);
    assert_fires_and_matches(db, CHAIN);

    // Pattern P2 — star product. §3.2 value.
    db.set_query_factorization(false);
    assert_eq!(scalar(db, STAR), 15);
    assert_fires_and_matches(db, STAR);

    // `count(tup = [..])` — the pure-output list unification is dropped; counts
    // the same bag of matches as `count(person)`.
    db.set_query_factorization(false);
    assert_eq!(scalar(db, STAR_TUP), 15);
    assert_fires_and_matches(db, STAR_TUP);

    // Pattern P3 — `!=` inclusion–exclusion. §3.3 value (18), the headline case.
    db.set_query_factorization(false);
    assert_eq!(scalar(db, IE_NEQ), 18);
    assert_fires_and_matches(db, IE_NEQ);

    // Two inequalities → the four-term inclusion–exclusion (capped case). A
    // star on `x` with two cross `!=`s; neither pair co-occurs in an atom.
    assert_fires_and_matches(
        db,
        "?[count(x)] := *knows[a, x], *knows[b, x], *knows[c, x], a != b, a != c",
    );

    // Group-by keys with pure factorization.
    assert_fires_and_matches(db, GROUPBY);
}

#[test]
fn targeted_patterns_mem() {
    let db = mem_db();
    populate_toy(&db);
    check_targeted(&db);
}

#[test]
fn targeted_patterns_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_db(&dir, "targeted.db");
    populate_toy(&db);
    check_targeted(&db);
}

// ------------------------------------------------------------------------
// Non-firing guards (each must DECLINE and return the correct naive result).
// ------------------------------------------------------------------------

fn check_non_firing(db: &DbInstance) {
    // count_unique is distinct-after-projection; product-of-counts does not apply.
    assert_declines(
        db,
        "?[count_unique(person)] := *member[group, person], *knows[friend, person]",
    );
    // Negation present.
    assert_declines(
        db,
        "?[count(p2)] := *knows[p1, p2], *knows[p2, p3], not *knows[p1, p3]",
    );
    // Cyclic hypergraph (a triangle) has no separator — declines.
    assert_declines(
        db,
        "?[count(p1)] := *knows[p1, p2], *knows[p2, p3], *knows[p3, p1]",
    );
    // Mixed aggregates.
    assert_declines(
        db,
        "?[count(person), max(city)] := *lives_in[person, city], *member[group, person]",
    );
    // Recursion / rule-application atom in the body.
    assert_declines(
        db,
        "reach[a, b] := *knows[a, b]\n\
         reach[a, b] := reach[a, c], *knows[c, b]\n\
         ?[count(b)] := reach[1, b], *member[group, b]",
    );
    // More than two inequalities (inclusion–exclusion capped at 2).
    assert_declines(
        db,
        "?[count(p2)] := *knows[p1, p2], *knows[p2, p3], *knows[p3, p4], \
         p1 != p2, p2 != p3, p3 != p4",
    );
    // A crossing predicate that is neither `!=` nor droppable (a `<` filter).
    assert_declines(
        db,
        "?[count(p2)] := *knows[p1, p2], *knows[p2, p3], p1 < p3",
    );
    // Single atom — nothing to factorize.
    assert_declines(db, "?[count(city)] := *lives_in[person, city]");
    // A non-count query is untouched entirely.
    assert_declines(
        db,
        "?[person, city] := *lives_in[person, city], *member[group, person]",
    );
}

#[test]
fn non_firing_guards_mem() {
    let db = mem_db();
    populate_toy(&db);
    check_non_firing(&db);
}

#[test]
fn non_firing_guards_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_db(&dir, "nonfiring.db");
    populate_toy(&db);
    check_non_firing(&db);
}

// ------------------------------------------------------------------------
// The toggle default (OFF) leaves an eligible query dormant but correct.
// ------------------------------------------------------------------------

#[test]
fn toggle_off_is_dormant_but_correct() {
    let db = mem_db();
    populate_toy(&db);
    // Fresh DbInstance defaults to factorization OFF.
    assert!(!db.query_factorization(), "default must be OFF (opt-in)");
    assert!(
        !fired(&db, STAR),
        "with the toggle off the rewrite must be dormant"
    );
    assert_eq!(
        scalar(&db, STAR),
        15,
        "dormant path returns the naive count"
    );
}

// ------------------------------------------------------------------------
// Item I — the detector advisory is emitted in `::explain` regardless of the
// rewrite kill switch (it is the safe, standalone half).
// ------------------------------------------------------------------------

fn explain_ops(db: &DbInstance, query: &str) -> Vec<String> {
    let plan: NamedRows = db
        .run_script(
            &format!("::explain {{ {query} }}"),
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .unwrap();
    let op = plan.headers.iter().position(|h| h == "op").unwrap();
    plan.rows
        .iter()
        .filter_map(|r| match &r[op] {
            DataValue::Str(s) => Some(s.to_string()),
            _ => None,
        })
        .collect()
}

#[test]
fn detector_advisory_independent_of_toggle() {
    let db = mem_db();
    populate_toy(&db);

    // Toggle OFF: the plan is naive (no `*fac` rules) but the advisory row is
    // still present — the detector is independent of the rewrite.
    db.set_query_factorization(false);
    assert!(!fired(&db, STAR));
    assert!(
        explain_ops(&db, STAR)
            .iter()
            .any(|o| o == "factorize_advisory"),
        "detector advisory must appear in ::explain even with the rewrite off"
    );

    // A non-factorizing query (single atom) emits no advisory.
    assert!(
        !explain_ops(&db, "?[count(city)] := *lives_in[person, city]")
            .iter()
            .any(|o| o == "factorize_advisory")
    );
}

// ------------------------------------------------------------------------
// The randomized differential property suite — the primary deliverable.
// ------------------------------------------------------------------------

/// Deterministic LCG (reproducible: `Math.random` is unavailable, so the whole
/// suite is seeded and replayable).
struct Lcg {
    state: u64,
}
impl Lcg {
    fn new(seed: u64) -> Self {
        Lcg { state: seed }
    }
    fn next_u64(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.state
    }
    /// Uniform in `0..n` (n > 0).
    fn below(&mut self, n: usize) -> usize {
        ((self.next_u64() >> 33) as usize) % n
    }
    fn chance(&mut self, num: usize, den: usize) -> bool {
        self.below(den) < num
    }
}

/// One generated case: the relations to create and the count query over them.
struct Case {
    relations: Vec<(String, usize, Vec<Vec<i64>>)>, // (name, arity, rows)
    query: String,
}

/// Generate a random eligible-*shaped* count query. Shapes are biased toward
/// star / chain (which reliably factorize) plus fully-random wirings (which
/// exercise decline-safety); correctness must hold either way.
fn gen_case(rng: &mut Lcg, id: usize) -> Case {
    let dom = 3 + rng.below(4); // value domain 3..6
    let n_atoms = 2 + rng.below(3); // 2..4 atoms
    let shape = rng.below(3); // 0 = star, 1 = chain, 2 = random

    let mut relations: Vec<(String, usize, Vec<Vec<i64>>)> = vec![];
    let mut atom_strs: Vec<String> = vec![];
    let mut used_vars: Vec<String> = vec![];
    let note_var = |v: &str, used: &mut Vec<String>| {
        if !used.contains(&v.to_string()) {
            used.push(v.to_string());
        }
    };

    // random-shape variable pool
    let pool_n = 2 + rng.below(3);

    for a in 0..n_atoms {
        let arity = 2 + rng.below(2); // 2..3 columns
        let name = format!("f{id}_{a}");
        // random rows
        let n_rows = 2 + rng.below(dom * 2);
        let mut rows = vec![];
        for _ in 0..n_rows {
            rows.push((0..arity).map(|_| rng.below(dom) as i64).collect());
        }
        relations.push((name.clone(), arity, rows));

        // wire columns to variables per shape
        let cols: Vec<String> = match shape {
            0 => {
                // star: column 0 is the center `v0`; the rest are private.
                let mut cs = vec!["v0".to_string()];
                for j in 1..arity {
                    cs.push(format!("s{a}_{j}"));
                }
                cs
            }
            1 => {
                // chain: columns 0,1 are spine vars `w{a}`,`w{a+1}`; extra private.
                let mut cs = vec![format!("w{a}"), format!("w{}", a + 1)];
                for j in 2..arity {
                    cs.push(format!("s{a}_{j}"));
                }
                cs
            }
            _ => (0..arity)
                .map(|_| format!("v{}", rng.below(pool_n)))
                .collect(),
        };
        for c in &cols {
            note_var(c, &mut used_vars);
        }
        atom_strs.push(format!("*{name}[{}]", cols.join(", ")));
    }

    // Choose the head: optional group-by keys, then the single count.
    // Inequalities require the keyless case (matches the v1 firing predicate).
    let keyless_ok_for_ie = !used_vars.is_empty();
    let want_ie = keyless_ok_for_ie && rng.chance(1, 3);

    let mut group_keys: Vec<String> = vec![];
    if !want_ie && rng.chance(1, 2) && !used_vars.is_empty() {
        // group by 1-2 used vars (favor the natural separators of the shape).
        let candidates: Vec<String> = match shape {
            0 => vec!["v0".to_string()],
            1 => vec!["w0".to_string(), format!("w{}", n_atoms)],
            _ => used_vars.clone(),
        };
        let candidates: Vec<String> = candidates
            .into_iter()
            .filter(|v| used_vars.contains(v))
            .collect();
        if !candidates.is_empty() {
            let k = 1 + rng.below(candidates.len().min(2));
            let mut chosen = vec![];
            for _ in 0..k {
                let v = candidates[rng.below(candidates.len())].clone();
                if !chosen.contains(&v) {
                    chosen.push(v);
                }
            }
            group_keys = chosen;
        }
    }

    let count_arg = used_vars[rng.below(used_vars.len())].clone();
    let mut head = String::new();
    for gk in &group_keys {
        head.push_str(gk);
        head.push_str(", ");
    }
    head.push_str(&format!("count({count_arg})"));

    let mut body = atom_strs.join(", ");

    if want_ie && used_vars.len() >= 2 {
        let n_ineq = 1 + rng.below(2); // 1..2
        for _ in 0..n_ineq {
            let u = &used_vars[rng.below(used_vars.len())];
            let v = &used_vars[rng.below(used_vars.len())];
            if u != v {
                body.push_str(&format!(", {u} != {v}"));
            }
        }
    }

    let query = format!("?[{head}] := {body}");
    Case { relations, query }
}

#[test]
fn differential_naive_equals_factorized() {
    const N_CASES: usize = 400;

    let mem = mem_db();
    let dir = tempfile::tempdir().unwrap();
    let sql = sqlite_db(&dir, "differential.db");

    let mut rng = Lcg::new(0x_D1FF_ACE5_1234_5678);
    let mut fired_count = 0usize;
    let mut ie_fired = 0usize;
    let mut nonzero_count = 0usize;

    for id in 0..N_CASES {
        let case = gen_case(&mut rng, id);

        // materialize the schema+data identically on both backends
        for (name, arity, rows) in &case.relations {
            put_rows(&mem, name, *arity, rows);
            put_rows(&sql, name, *arity, rows);
        }

        // naive (off) and factorized (on) on each backend
        mem.set_query_factorization(false);
        let naive_mem = sorted_rows(&mem, &case.query);
        mem.set_query_factorization(true);
        let fac_mem = sorted_rows(&mem, &case.query);

        sql.set_query_factorization(false);
        let naive_sql = sorted_rows(&sql, &case.query);
        sql.set_query_factorization(true);
        let fac_sql = sorted_rows(&sql, &case.query);

        // The core obligation: every form agrees, on both operators.
        assert_eq!(
            naive_mem, fac_mem,
            "MEM naive != factorized (case {id}): {}",
            case.query
        );
        assert_eq!(
            naive_sql, fac_sql,
            "SQLITE naive != factorized (case {id}): {}",
            case.query
        );
        assert_eq!(
            naive_mem, naive_sql,
            "MEM naive != SQLITE naive (case {id}): {}",
            case.query
        );

        // bookkeeping to prove the suite actually exercises the rewrite
        if fired(&mem, &case.query) {
            fired_count += 1;
            if case.query.contains("!=") {
                ie_fired += 1;
            }
        }
        if naive_mem.iter().any(|r| {
            r.last()
                .and_then(|v| v.get_int())
                .map(|c| c > 1)
                .unwrap_or(false)
        }) {
            nonzero_count += 1;
        }
    }

    eprintln!(
        "DIFFERENTIAL STATS: {N_CASES} cases, fired={fired_count}, ie_fired={ie_fired}, count>1 in {nonzero_count}"
    );
    // The suite must genuinely fire the rewrite (and its `!=` branch) on a
    // substantial fraction of cases, and produce counts that exceed trivial
    // small values — otherwise it would not be testing what it claims.
    assert!(
        fired_count >= N_CASES / 5,
        "rewrite fired on only {fired_count}/{N_CASES} cases — suite not exercising it"
    );
    assert!(
        ie_fired >= 5,
        "inclusion–exclusion path fired on only {ie_fired} cases"
    );
    assert!(
        nonzero_count >= N_CASES / 5,
        "only {nonzero_count}/{N_CASES} cases produced a count > 1"
    );
}

// ------------------------------------------------------------------------
// The `!=` type gate (0.14.0). The inclusion–exclusion correction term JOINS
// the operands while `!=` compares them with `op_neq`, and the two disagree on
// numerically-equal cross-variant pairs (`Int(1)` vs `Float(1.0)`: distinct
// under the engine total order, equal under `op_neq`). The gate admits the
// rewrite only when every binding occurrence of both operands is a declared
// non-nullable, non-`Any` stored column and all occurrences agree on ONE type.
//
// Discrimination (run manually, recorded in the commit): with the gate call in
// `maybe_rewrite_and_advise` replaced by `true`, `ie_neq_mixed_types_declines`
// goes red with count 4 (the smuggled pair survives) instead of declining.
// ------------------------------------------------------------------------

fn populate_mixed(db: &DbInstance) {
    // knows_i: Int endpoints; knows_f: Float endpoints. The values coincide
    // numerically (1 vs 1.0) so op_neq and the join disagree about them.
    run_mut(db, ":create knows_i { c0: Int, c1: Int }");
    run_mut(
        db,
        "?[c0, c1] <- [[1, 100], [2, 100]] :put knows_i {c0, c1}",
    );
    run_mut(db, ":create knows_f { c0: Float, c1: Int }");
    run_mut(
        db,
        "?[c0, c1] <- [[1.0, 100], [3.0, 100]] :put knows_f {c0, c1}",
    );
}

/// Cross-variant operands (Int vs Float) — the exact miscount class the 2026-07
/// cut removed — must DECLINE, and the declined (naive) answer stands. Naive:
/// pairs (a, b) over the star join with a != b under op_neq — (1, 1.0) is
/// numerically equal, so 3 of the 4 pairs survive. A fired rewrite would count
/// 4 (the correction join never matches across variants).
#[test]
fn ie_neq_mixed_types_declines() {
    let db = mem_db();
    populate_mixed(&db);
    let q = "?[count(x)] := *knows_i[a, x], *knows_f[b, x], a != b";
    db.set_query_factorization(false);
    assert_eq!(scalar(&db, q), 3, "naive baseline");
    assert_declines(&db, q);
}

/// An `Any`-typed operand column is not variant-stable and must decline even
/// though both sides declare the same (`Any`) type.
#[test]
fn ie_neq_any_typed_operand_declines() {
    let db = mem_db();
    run_mut(&db, ":create ka { c0: Any, c1: Int }");
    run_mut(&db, "?[c0, c1] <- [[1, 100], [2.0, 100]] :put ka {c0, c1}");
    run_mut(&db, ":create kb { c0: Any, c1: Int }");
    run_mut(&db, "?[c0, c1] <- [[1.0, 100], [3, 100]] :put kb {c0, c1}");
    assert_declines(&db, "?[count(x)] := *ka[a, x], *kb[b, x], a != b");
}

/// A nullable operand column declines.
#[test]
fn ie_neq_nullable_operand_declines() {
    let db = mem_db();
    run_mut(&db, ":create kn { c0: Int?, c1: Int }");
    run_mut(&db, "?[c0, c1] <- [[1, 100], [2, 100]] :put kn {c0, c1}");
    run_mut(&db, ":create km { c0: Int, c1: Int }");
    run_mut(&db, "?[c0, c1] <- [[1, 100], [3, 100]] :put km {c0, c1}");
    assert_declines(&db, "?[count(x)] := *kn[a, x], *km[b, x], a != b");
}

/// Same non-numeric declared type (String) is admissible — the divergent
/// `op_neq` arm only exists for the (Int, Float) pair.
#[test]
fn ie_neq_same_string_type_fires() {
    let db = mem_db();
    run_mut(&db, ":create sa { c0: String, c1: Int }");
    run_mut(
        &db,
        "?[c0, c1] <- [['x', 100], ['y', 100]] :put sa {c0, c1}",
    );
    run_mut(&db, ":create sb { c0: String, c1: Int }");
    run_mut(
        &db,
        "?[c0, c1] <- [['x', 100], ['z', 100]] :put sb {c0, c1}",
    );
    let q = "?[count(x)] := *sa[a, x], *sb[b, x], a != b";
    db.set_query_factorization(false);
    assert_eq!(scalar(&db, q), 3, "naive baseline");
    assert_fires_and_matches(&db, q);
}

/// An operand bound by TWO atoms must agree at EVERY occurrence —
/// first-occurrence-wins would be unsound. Here `a` occurs as Int in one atom
/// and Float in another (zero rows can ever join, but the GATE must not even
/// consult the data).
#[test]
fn ie_neq_disagreeing_occurrences_decline() {
    let db = mem_db();
    run_mut(&db, ":create oa { c0: Int, c1: Int }");
    run_mut(&db, "?[c0, c1] <- [[1, 100]] :put oa {c0, c1}");
    run_mut(&db, ":create ob { c0: Float, c1: Int }");
    run_mut(&db, "?[c0, c1] <- [[1.0, 100]] :put ob {c0, c1}");
    run_mut(&db, ":create oc { c0: Int, c1: Int }");
    run_mut(&db, "?[c0, c1] <- [[2, 100]] :put oc {c0, c1}");
    // `a` is bound by both oa.c0 (Int) and ob.c0 (Float); `b` by oc.c0 (Int).
    assert_declines(
        &db,
        "?[count(x)] := *oa[a, x], *ob[a, x], *oc[b, x], a != b",
    );
}

/// A `Json`-typed operand column must decline: JsonData's Eq is STRUCTURAL
/// (serde_json equality — IEEE `==`, so json(-0.0) == json(0.0)) while its Ord
/// (what the correction join uses) compares to_string() output — op_neq-equal
/// but join-distinct pairs exist TODAY, and the fired rewrite overcounts
/// 2-for-0 on this exact fixture (reproduced during the 0.14.0 review).
#[test]
fn ie_neq_json_typed_operand_declines() {
    let dir = tempfile::tempdir().unwrap();
    for db in [mem_db(), sqlite_db(&dir, "json_gate.db")] {
        run_mut(&db, ":create jr { k: Int => v: Json }");
        run_mut(
            &db,
            "?[k, v] <- [[1, parse_json('-0.0')], [2, parse_json('0.0')]] :put jr {k, v}",
        );
        let q = "?[count(a)] := *jr{k: a, v: x}, *jr{k: b, v: y}, x != y";
        db.set_query_factorization(false);
        assert_eq!(scalar(&db, q), 0, "naive: -0.0 and 0.0 are op_neq-equal");
        assert_declines(&db, q);
    }
}

/// The variant-stability check recurses: a List embedding Json inherits the
/// Eq/Ord divergence element-wise and must decline...
#[test]
fn ie_neq_list_of_json_declines() {
    let db = mem_db();
    run_mut(&db, ":create lj { k: Int => v: [Json] }");
    run_mut(
        &db,
        "?[k, v] := k = 1, v = [parse_json('-0.0')] :put lj {k, v}",
    );
    run_mut(
        &db,
        "?[k, v] := k = 2, v = [parse_json('0.0')] :put lj {k, v}",
    );
    assert_declines(
        &db,
        "?[count(a)] := *lj{k: a, v: x}, *lj{k: b, v: y}, x != y",
    );
}

/// ...while a List of a stable element type (Int) still FIRES — the recursion
/// must not over-decline.
#[test]
fn ie_neq_list_of_int_fires() {
    let db = mem_db();
    run_mut(&db, ":create li { k: Int => v: [Int] }");
    run_mut(&db, "?[k, v] <- [[1, [1]], [2, [2]]] :put li {k, v}");
    let q = "?[count(a)] := *li{k: a, v: x}, *li{k: b, v: y}, x != y";
    db.set_query_factorization(false);
    assert_eq!(scalar(&db, q), 2, "naive baseline");
    assert_fires_and_matches(&db, q);
}
