/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Regression tests for mnestic fork divergences from upstream CozoDB.

use cozo::{DataValue, DbInstance, ScriptMutability};
use std::collections::BTreeMap;

/// Fork #2 (DESIGN CALL, NOT YET FIXED): a top-level `:create _foo` returns Ok
/// but does not persist, because `_`-prefixed relations are TRANSACTION-SCOPED
/// temporaries. This is a legitimate feature inside imperative scripts (a `_rel`
/// created in one `{...}` block is usable in a later block of the same script —
/// see `runtime::tests::filtering`), so we must NOT blanket-reject the name.
///
/// The real trap is only the *top-level* form, where the transaction ends
/// immediately and the temp vanishes silently. A proper fix is a design call
/// (warn on top-level temp create, or surface the scoping) that needs to thread
/// imperative-vs-top-level context into the creation path. This test documents
/// the current behavior; flip `#[ignore]` once that fix lands.
#[test]
#[ignore = "fork #2: top-level `:create _foo` silently no-ops; fix is a scoping design call"]
fn top_level_create_underscore_relation_is_a_silent_noop() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new(
        "sqlite",
        dir.path().join("b2.db").to_str().unwrap(),
        Default::default(),
    )
    .unwrap();

    let created = db.run_script(
        ":create _foo { uid: String => val: String }",
        BTreeMap::new(),
        ScriptMutability::Mutable,
    );
    // CURRENT upstream behavior: Ok, but nothing persists. We assert the trap so
    // the test starts failing (alerting us) once the behavior is improved.
    assert!(created.is_ok(), "documents current (trap) behavior");
    let rels = db
        .run_script("::relations", BTreeMap::new(), ScriptMutability::Immutable)
        .unwrap();
    assert_eq!(
        rels.rows.len(),
        0,
        "the `_foo` temp silently did not persist"
    );
}

/// Fork #1 (FIXED): `*rel[col, ...], col == <ground>` and `*rel{col, ...},
/// col == <ground>` now compile to a keyed `stored_prefix_join`, identical to the
/// binding-first form `col = <ground>, *rel{...}`. Upstream compiled the post-
/// filter shapes to a full `load_stored` scan + `eq(..)` filter (~20× slower at
/// 1k rows). Fix: `query/reorder.rs::push_equality_filters_to_bindings`.
#[test]
fn equality_post_filter_uses_prefix_lookup() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new(
        "sqlite",
        dir.path().join("b1.db").to_str().unwrap(),
        Default::default(),
    )
    .unwrap();
    db.run_script(
        ":create pk_test { uid: String => val: String }",
        BTreeMap::new(),
        ScriptMutability::Mutable,
    )
    .unwrap();
    db.run_script(
        r#"?[uid, val] <- [["a","1"],["b","2"],["c","3"]] :put pk_test { uid => val }"#,
        BTreeMap::new(),
        ScriptMutability::Mutable,
    )
    .unwrap();

    let uses_prefix_join = |query: &str| {
        let plan = db
            .run_script(
                &format!("::explain {{ {query} }}"),
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )
            .unwrap();
        let ops: Vec<String> = plan.rows.iter().map(|r| format!("{:?}", r[4])).collect();
        assert!(
            ops.iter().any(|o| o.contains("prefix_join")),
            "expected a keyed prefix_join for `{query}`, got plan ops: {ops:?}"
        );
    };

    // Both post-filter shapes must now use a keyed lookup, like the fast form.
    uses_prefix_join(r#"?[uid, val] := *pk_test[uid, val], uid == 'b'"#);
    uses_prefix_join(r#"?[uid, val] := *pk_test{uid, val}, uid == 'b'"#);
    uses_prefix_join(r#"?[uid, val] := uid = 'b', *pk_test{uid, val}"#);

    // ...and the rewrite must not change results.
    let rows = db
        .run_script(
            r#"?[uid, val] := *pk_test[uid, val], uid == 'b'"#,
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .unwrap();
    assert_eq!(rows.rows.len(), 1, "exactly one row matches uid == 'b'");
    assert_eq!(rows.rows[0][0].get_str(), Some("b"));
    assert_eq!(rows.rows[0][1].get_str(), Some("2"));
}

/// Upstream cozo #281 (FIXED): identifiers that start with a keyword literal
/// (`null`, `true`, `false`) mis-parsed in value positions because `term` tries
/// `literal` before `var` and the keyword literals had no word boundary. So
/// `*rel{col: nullable_column}` failed while `*rel{col: x}` worked. Fix: word-
/// boundary lookahead on `null`/`boolean` in `cozoscript.pest`.
#[test]
fn keyword_prefixed_identifiers_parse() {
    let db = DbInstance::default();
    let q = |s: &str| db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable);

    q(":create rel { id: Int => nullable_column: Int? }").unwrap();
    q(r#"?[id, nullable_column] <- [[1, 10],[2, 20]] :put rel { id => nullable_column }"#).unwrap();

    // The #281 case: `field: binding` where the binding name starts with `null`.
    let r = q("?[id, nullable_column] := *rel{id, nullable_column: nullable_column}")
        .expect("nullable_column binding must parse");
    assert_eq!(r.rows.len(), 2);

    // Other keyword-prefixed identifiers as bindings/vars.
    for ident in ["nullable2", "trueValue", "falsey", "null_x", "true_thing"] {
        let r = db
            .run_script(
                &format!("?[{ident}] := {ident} = 1"),
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )
            .unwrap_or_else(|e| panic!("`{ident}` should parse as a var: {e:?}"));
        assert_eq!(r.rows.len(), 1);
    }

    // Regression guard: the actual literals must still parse and evaluate.
    let lit = |s: &str| {
        db.run_script(s, BTreeMap::new(), ScriptMutability::Immutable)
            .unwrap()
            .rows
    };
    assert_eq!(lit("?[x] := x = true")[0][0], DataValue::Bool(true));
    assert_eq!(lit("?[x] := x = false")[0][0], DataValue::Bool(false));
    assert_eq!(lit("?[x] := x = null")[0][0], DataValue::Null);
    assert_eq!(
        lit("?[x] := x = [null, true, false]")[0][0]
            .get_slice()
            .unwrap()
            .len(),
        3
    );
}

/// Fork #1 boundary (review fix): numeric equality post-filters must NOT be pushed
/// to a keyed lookup. `op_eq` treats `Int(n) == Float(n)` as equal (cross-type),
/// but the key index uses strict `Num` ordering where `Int(n) != Float(n)`, so a
/// pushdown would silently drop cross-type matches. This guards that numeric `==`
/// keeps full `op_eq` semantics (the conversion is gated to non-numeric grounds).
#[test]
fn numeric_equality_keeps_cross_type_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new(
        "sqlite",
        dir.path().join("xnum.db").to_str().unwrap(),
        Default::default(),
    )
    .unwrap();
    let q = |s: &str| {
        db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable)
            .unwrap()
    };
    q(":create rel { id: Int => k }");
    q(r#"?[id, k] <- [[1, 3], [2, 3.0]] :put rel { id => k }"#);
    // op_eq matches both Int(3) and Float(3.0); the pushdown must not change this.
    assert_eq!(
        q("?[id, k] := *rel{id, k}, k == 3").rows.len(),
        2,
        "numeric `== 3` must keep cross-type op_eq semantics (match Int and Float)"
    );
    assert_eq!(
        q("?[id, k] := *rel{id, k}, k == 3.0").rows.len(),
        2,
        "numeric `== 3.0` must also match both"
    );
}

/// `::describe` writes relation metadata, but it was the only mutating sys op
/// without a read-only guard. Before the snapshot read path it silently wrote
/// under `ScriptMutability::Immutable`; after, it would surface the generic
/// "write in read-only transaction" storage error. Pin the explicit guard
/// (clear message, no storage-layer dependence), and that the mutable path
/// still works.
#[test]
fn describe_relation_is_rejected_in_read_only_mode() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new(
        "sqlite",
        dir.path().join("desc.db").to_str().unwrap(),
        Default::default(),
    )
    .unwrap();
    db.run_script(
        ":create rel { id: Int => v }",
        BTreeMap::new(),
        ScriptMutability::Mutable,
    )
    .unwrap();

    let err = db
        .run_script(
            "::describe rel 'a note'",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .expect_err("::describe must be rejected in read-only mode");
    assert!(
        err.to_string().contains("read-only"),
        "expected a read-only rejection, got: {err}"
    );

    db.run_script(
        "::describe rel 'a note'",
        BTreeMap::new(),
        ScriptMutability::Mutable,
    )
    .expect("::describe must still work in mutable mode");
}

/// `::repair_corrupt` parses, runs against a healthy relation (reporting 0
/// removed), respects read-only mode, and errors on a missing relation.
/// True-corruption repair and ordinary-error behavior are exercised by the
/// internal SQLite regression, where raw store access can safely craft a
/// truncated value blob.
#[test]
fn repair_corrupt_basics() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new(
        "sqlite",
        dir.path().join("repair.db").to_str().unwrap(),
        Default::default(),
    )
    .unwrap();
    db.run_script(
        ":create t { k: Int => a: Int, b: Int }",
        BTreeMap::new(),
        ScriptMutability::Mutable,
    )
    .unwrap();
    db.run_script(
        "?[k, a, b] <- [[1, 2, 3], [4, 5, 6]] :put t {k => a, b}",
        BTreeMap::new(),
        ScriptMutability::Mutable,
    )
    .unwrap();

    let imperative = db
        .run_script(
            "{?[k, a, b] <- [[1, 2, 3]] :put t {k => a, b}} {::repair_corrupt t}",
            BTreeMap::new(),
            ScriptMutability::Mutable,
        )
        .unwrap();
    assert_eq!(imperative.rows[0][0].get_int(), Some(0));

    let res = db
        .run_script(
            "::repair_corrupt t",
            BTreeMap::new(),
            ScriptMutability::Mutable,
        )
        .unwrap();
    assert_eq!(res.headers, vec!["removed".to_string()]);
    assert_eq!(res.rows[0][0].get_int(), Some(0));

    // healthy rows untouched
    let rows = db
        .run_script(
            "?[k] := *t[k, _, _]",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .unwrap();
    assert_eq!(rows.rows.len(), 2);

    let err = db
        .run_script(
            "::repair_corrupt t",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .unwrap_err();
    assert!(err.to_string().to_lowercase().contains("read-only"));

    assert!(db
        .run_script(
            "::repair_corrupt missing_rel",
            BTreeMap::new(),
            ScriptMutability::Mutable
        )
        .is_err());
}
