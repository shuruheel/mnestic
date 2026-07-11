/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Shared fixtures for the planner regression suite (`planner_shape.rs` = T0,
//! the every-PR plan-shape gate; `lsqb.rs` = T1, the nightly execution tier).
//!
//! The schemas and query text live here because **the two tiers must agree
//! exactly**. Plan shape is decided by key arity, so if T0 pinned a plan for
//! `knows{src, dst}` while T1 loaded `knows{src => dst}`, the gate would be
//! guarding a plan the execution tier never runs.
//!
//! Query provenance: LSQB (`ldbc/lsqb`, Apache-2.0) publishes these nine queries
//! in Cypher and SQL, not Datalog. The CozoScript ports below descend from
//! Matthias Autrata's ports in `github.com/matthiasautrata/Mnestic-Benchmarks`
//! (`bench/workloads.py`), used with permission, and are checked against LSQB's
//! own published expected-output counts (see `LSQB_ORACLE`).

#![allow(dead_code)] // each test binary uses a different subset

use cozo::{DbInstance, NamedRows};

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// The 11 LSQB edge relations reachable from the five ported queries.
///
/// Every one is a pure edge relation: **both columns are keys** (key arity 2).
/// That is load-bearing — the greedy reorder's tie-break scores an atom by how
/// much of its *key* is bound, so a `{src: Int => dst: Int}` (key arity 1) form
/// would produce a different plan and silently invalidate the committed baseline.
pub const LSQB_RELATIONS: &[&str] = &[
    ":create knows { src: Int, dst: Int }",
    ":create p_loc_city { person: Int, city: Int }",
    ":create city_in_ctry { city: Int, country: Int }",
    ":create f_has_member { forum: Int, person: Int }",
    ":create f_cont_post { forum: Int, post: Int }",
    ":create c_reply_post { comment: Int, post: Int }",
    ":create c_has_tag { comment: Int, tag: Int }",
    ":create tag_type_tc { tag: Int, tagclass: Int }",
    ":create p_int_tag { person: Int, tag: Int }",
    ":create c_creator_p { comment: Int, person: Int }",
    ":create post_creator_p { post: Int, person: Int }",
];

/// `(relation, projected-fk CSV basename)`. Used only by T1 (`lsqb.rs`).
///
/// `knows` is the trap: LSQB ships `Person_knows_Person` **one-directionally**
/// (18,135 rows, every row `src < dst`) while every query matches it undirected.
/// The loader must symmetrize it to 36,270 rows. Skip that and q2/q3/q6/q9 —
/// four of the five queries — return plausible, wrong numbers.
pub const LSQB_CSV_MAP: &[(&str, &str)] = &[
    ("knows", "Person_knows_Person"),
    ("p_loc_city", "Person_isLocatedIn_City"),
    ("city_in_ctry", "City_isPartOf_Country"),
    ("f_has_member", "Forum_hasMember_Person"),
    ("f_cont_post", "Forum_containerOf_Post"),
    ("c_reply_post", "Comment_replyOf_Post"),
    ("c_has_tag", "Comment_hasTag_Tag"),
    ("tag_type_tc", "Tag_hasType_TagClass"),
    ("p_int_tag", "Person_hasInterest_Tag"),
    ("c_creator_p", "Comment_hasCreator_Person"),
    ("post_creator_p", "Post_hasCreator_Person"),
];

/// `knows` is symmetrized on load; the rest are loaded as-shipped.
pub const KNOWS_RAW_ROWS: usize = 18_135;
pub const KNOWS_SYMMETRIC_ROWS: usize = KNOWS_RAW_ROWS * 2;

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

/// LSQB q1 — the **factorizable** shape. Acceptance test for flipping the
/// factorized-`count()` rewrite default-ON (measured 0.11.1: 14,264 ms →
/// 551 ms with factorization forced on, 25.8×).
pub const Q1: &str = "?[count(country)] := \
     *city_in_ctry[city, country], \
     *p_loc_city[person, city], \
     *f_has_member[forum, person], \
     *f_cont_post[forum, post], \
     *c_reply_post[comment, post], \
     *c_has_tag[comment, tag], \
     *tag_type_tc[tag, tagclass]";

/// LSQB q2 — **cyclic** (WCOJ-class, declined). Guards that it stays
/// declined-not-worse.
pub const Q2: &str = "?[count(comment)] := \
     *knows[person1, person2], \
     *c_creator_p[comment, person1], \
     *c_reply_post[comment, post], \
     *post_creator_p[post, person2]";

/// LSQB q3 — **the 0.10.5 catcher**. The same-country person triangle. Its
/// greedy `load_refs` is the one signature that discriminates the broken engine
/// from the fixed one; see `planner_shape.rs`.
pub const Q3: &str = "?[count(country)] := \
     *p_loc_city[p1, c1], *city_in_ctry[c1, country], \
     *p_loc_city[p2, c2], *city_in_ctry[c2, country], \
     *p_loc_city[p3, c3], *city_in_ctry[c3, country], \
     *knows[p1, p2], *knows[p2, p3], *knows[p3, p1]";

/// LSQB q6 — the **`!=` inclusion-exclusion** restore target (921× vs hand at
/// sf1). Measured 0.11.1: factorization does **not** help it (9,874 → 9,967 ms;
/// the rewrite declines on `!=` bodies) — which is the measured proof that the
/// factorization decision and the `!=`-restore decision are genuinely separate
/// and need separate acceptance tests.
pub const Q6: &str = "?[count(tag)] := \
     *knows[p1, p2], *knows[p2, p3], *p_int_tag[p3, tag], p1 != p3";

/// LSQB q9 — **negation**. Carried as a baseline-ratio guard only.
pub const Q9: &str = "?[count(tag)] := \
     *knows[p1, p2], *knows[p2, p3], *p_int_tag[p3, tag], p1 != p3, \
     not *knows[p1, p3]";

/// The five ported queries, keyed by the name used in `planner_baseline.json`.
///
/// **Stated coverage hole:** LSQB q4/q5/q7/q8 are *not* ported. They need the
/// virtual `:Message = Comment ∪ Post` label (and q7 an `OPTIONAL MATCH`, which
/// has no clean Cozo form). Every acceptance test for every gated decision does
/// live in {q1, q3, q6} — but the next pathology may hide in a shape these five
/// do not cover. This is a known gap, not implied completeness.
pub const LSQB_QUERIES: &[(&str, &str)] = &[
    ("lsqb_q1", Q1),
    ("lsqb_q2", Q2),
    ("lsqb_q3", Q3),
    ("lsqb_q6", Q6),
    ("lsqb_q9", Q9),
];

/// LSQB's own published expected-output counts for sf0.1 (`ldbc/lsqb`,
/// Apache-2.0) — an oracle authored by neither engine. A mismatch is a hard
/// failure, never a warning.
pub const LSQB_ORACLE: &[(&str, i64)] = &[
    ("lsqb_q1", 8_773_828),
    ("lsqb_q2", 82_990),
    ("lsqb_q3", 30_456),
    ("lsqb_q6", 55_607_896),
    ("lsqb_q9", 51_009_398),
];

// ---------------------------------------------------------------------------
// Plan-shape helpers
// ---------------------------------------------------------------------------

/// The ordered sequence of stored relations the plan loads, e.g.
/// `[":p_loc_city", ":city_in_ctry", …]`. This *is* the join order.
///
/// Only `::explain` columns 4 (`op`) and 5 (`ref`) are read, and that is
/// deliberate. Both are stable: `op` is a closed, code-defined vocabulary and
/// `ref` is *our own* relation name. The other columns are not safe to assert
/// across versions — cols 6/8 (`joins_on`/`out_relation`) carry
/// compiler-generated `**0`/`**1` binding names, and col 7 (`filters/expr`)
/// renders through `Expr`'s `debug_tuple` Display, so `p1 != p3` prints as
/// `neq(p1, p3)`. A committed baseline over those would rot on a refactor that
/// changed nothing about the plan.
pub fn load_refs(plan: &NamedRows) -> Vec<String> {
    plan.rows
        .iter()
        .filter(|r| r[4].get_str() == Some("load_stored"))
        .map(|r| r[5].get_str().unwrap_or("").to_string())
        .collect()
}

/// `::explain` the query as the planner would actually run it.
pub fn explain(db: &DbInstance, query: &str) -> NamedRows {
    run(db, &format!("::explain {{ {query} }}"))
}

/// The `load_stored` sequence under the default (greedy) planner.
pub fn greedy_refs(db: &DbInstance, query: &str) -> Vec<String> {
    load_refs(&explain(db, query))
}

/// The `load_stored` sequence with the reorder disabled — the in-run control.
/// Both arms come from the same binary in the same process, so this comparison
/// cannot rot across versions the way a committed baseline can.
pub fn written_refs(db: &DbInstance, query: &str) -> Vec<String> {
    load_refs(&explain(db, &format!("{query}\n:reorder written")))
}

pub fn run(db: &DbInstance, s: &str) -> NamedRows {
    db.run_script(s, Default::default(), cozo::ScriptMutability::Immutable)
        .unwrap_or_else(|e| panic!("query failed: {e}\n--- script ---\n{s}"))
}

pub fn run_mut(db: &DbInstance, s: &str) {
    db.run_script(s, Default::default(), cozo::ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed: {e}\n--- script ---\n{s}"));
}

/// A database with all 11 LSQB relations created but **empty**.
///
/// The greedy reorder is *stat-free* — it plans from the schema (key arity and
/// variable bindings), never from cardinality — so `::explain` over these empty
/// relations yields the byte-identical plan to the fully-loaded 2M-row dataset.
/// That is what lets the T0 gate run on every PR in ~25 ms with no download.
pub fn empty_lsqb_db(path: &std::path::Path) -> DbInstance {
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), Default::default()).unwrap();
    for script in LSQB_RELATIONS {
        run_mut(&db, script);
    }
    db
}
