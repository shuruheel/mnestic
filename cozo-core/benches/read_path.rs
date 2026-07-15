/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Read-path latency baseline for DEVELOPMENT.md **Item 9** (compiled-plan cache
//! and read-only snapshot path). Grounds *where* a read query's per-call cost goes
//! before any caching work — per the fork's "baseline-first" rule.
//!
//! For each query shape it times two things on the SQLite backend:
//!
//! - `parse_only`: `parse::parse_script(...)` alone — parse + compile-to-AST.
//!   This is the work a compiled-plan cache would eliminate on a cache hit.
//! - `full_run`: `run_script(...)` end-to-end — parse + compile + open the
//!   (pessimistic) transaction + execute.
//!
//! `full_run − parse_only` ≈ transaction-open + execution. The split tells us
//! how much a plan cache could save (the `parse_only` fraction) versus what only
//! a read-snapshot execution path can touch (the remainder). Run:
//! `cargo bench -p mnestic --bench read_path`
//!
//! Caveat (the reason this is a *baseline*, not a fix): the public API offers no
//! way to execute a pre-compiled plan twice — `CozoScript` is not `Clone` and
//! `run_script_ast` consumes it — and parameters are inlined as constants at
//! parse time (`parse/expr.rs`), so a real plan cache additionally needs
//! late-bound params. This bench measures the *ceiling* such work could reach.

use std::collections::BTreeMap;

use cozo::data::functions::current_validity;
use cozo::{parse::parse_script, CustomAggrRegistries, DataValue, DbInstance, ScriptMutability};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

const N: usize = 5000;

fn make_db() -> (tempfile::TempDir, DbInstance) {
    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new(
        "sqlite",
        dir.path().join("read_path.db").to_str().unwrap(),
        Default::default(),
    )
    .unwrap();
    db.run_script(
        ":create docs { id: String => n: Int, text: String }",
        BTreeMap::new(),
        ScriptMutability::Mutable,
    )
    .unwrap();
    let mut i = 0;
    while i < N {
        let end = (i + 500).min(N);
        let rows: Vec<String> = (i..end)
            .map(|k| format!(r#"["k{k}",{k},"doc number {k} alpha beta"]"#))
            .collect();
        let script = format!(
            "?[id, n, text] <- [{}] :put docs {{ id => n, text }}",
            rows.join(",")
        );
        db.run_script(&script, BTreeMap::new(), ScriptMutability::Mutable)
            .unwrap();
        i = end;
    }
    (dir, db)
}

fn bench_read_path(c: &mut Criterion) {
    let (_dir, db) = make_db();
    let fixed_rules = db.get_fixed_rules();
    let custom_aggrs = db.get_custom_aggrs();
    let custom_bounded_meets = db.get_custom_bounded_meets();

    let mut params = BTreeMap::new();
    params.insert(
        "u".to_string(),
        DataValue::Str(format!("k{}", N / 2).into()),
    );
    params.insert("t".to_string(), DataValue::from(100i64));

    // Two shapes: a single-rule point read, and a multi-rule "retrieval-like"
    // query (filter → derived rule → order/limit) closer to the agent read path.
    let queries = [
        ("point", "?[id, n] := id = $u, *docs{ id, n }"),
        (
            "retrieval",
            "cand[id, n] := *docs{ id, n }, n > $t \
             top[id, n] := cand[id, n] \
             ?[id, n] := top[id, n] :order -n :limit 10",
        ),
    ];

    let mut group = c.benchmark_group("read_path");
    for (name, query) in queries {
        group.bench_with_input(BenchmarkId::new("parse_only", name), &query, |b, q| {
            b.iter(|| {
                let ast = parse_script(
                    q,
                    &params,
                    &fixed_rules,
                    CustomAggrRegistries {
                        meet: &custom_aggrs,
                        bounded: &custom_bounded_meets,
                    },
                    current_validity(),
                )
                .unwrap();
                criterion::black_box(&ast);
            })
        });
        group.bench_with_input(BenchmarkId::new("full_run", name), &query, |b, q| {
            b.iter(|| {
                let res = db
                    .run_script(q, params.clone(), ScriptMutability::Immutable)
                    .unwrap();
                criterion::black_box(res.rows.len());
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_read_path);
criterion_main!(benches);
