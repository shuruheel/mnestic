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
    assert_eq!(rels.rows.len(), 0, "the `_foo` temp silently did not persist");
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
    q(r#"?[id, nullable_column] <- [[1, 10],[2, 20]] :put rel { id => nullable_column }"#)
        .unwrap();

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
    assert_eq!(lit("?[x] := x = [null, true, false]")[0][0].get_slice().unwrap().len(), 3);
}
