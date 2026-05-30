/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Baseline for fork #1 (equality-pushdown). Times a single-row primary-key
//! lookup written three ways against an N-row stored relation on the SQLite
//! backend:
//!   - `eq_postfilter_positional`: `*rel[uid, val], uid == $u`   (the slow shape)
//!   - `eq_postfilter_brace`:      `*rel{uid, val}, uid == $u`   (the slow shape)
//!   - `binding_first`:            `uid = $u, *rel{uid, val}`    (the fast shape)
//!
//! Before the fix, the post-filter shapes do an O(N) `load_stored` scan; after,
//! all three compile to an O(log N) `stored_prefix_join`. Run:
//!   cargo bench -p mnestic --bench point_lookup

use std::collections::BTreeMap;

use cozo::{DataValue, DbInstance, ScriptMutability};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

const N: usize = 5000;

fn make_db() -> (tempfile::TempDir, DbInstance) {
    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new(
        "sqlite",
        dir.path().join("point_lookup.db").to_str().unwrap(),
        Default::default(),
    )
    .unwrap();
    db.run_script(
        ":create pk_test { uid: String => val: String }",
        BTreeMap::new(),
        ScriptMutability::Mutable,
    )
    .unwrap();

    // Insert N rows in chunks via a constant relation.
    let mut i = 0;
    while i < N {
        let end = (i + 500).min(N);
        let rows: Vec<String> = (i..end)
            .map(|k| format!(r#"["k{k}","v{k}"]"#))
            .collect();
        let script = format!(
            "?[uid, val] <- [{}] :put pk_test {{ uid => val }}",
            rows.join(",")
        );
        db.run_script(&script, BTreeMap::new(), ScriptMutability::Mutable)
            .unwrap();
        i = end;
    }
    (dir, db)
}

fn bench_point_lookup(c: &mut Criterion) {
    let (_dir, db) = make_db();
    // Look up a key in the middle of the keyspace.
    let mut params = BTreeMap::new();
    params.insert("u".to_string(), DataValue::Str(format!("k{}", N / 2).into()));

    let mut group = c.benchmark_group("point_lookup");
    for (name, query) in [
        (
            "eq_postfilter_positional",
            "?[uid, val] := *pk_test[uid, val], uid == $u",
        ),
        (
            "eq_postfilter_brace",
            "?[uid, val] := *pk_test{uid, val}, uid == $u",
        ),
        ("binding_first", "?[uid, val] := uid = $u, *pk_test{uid, val}"),
    ] {
        group.bench_with_input(BenchmarkId::from_parameter(name), &query, |b, q| {
            b.iter(|| {
                let res = db
                    .run_script(q, params.clone(), ScriptMutability::Immutable)
                    .unwrap();
                assert_eq!(res.rows.len(), 1);
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_point_lookup);
criterion_main!(benches);
