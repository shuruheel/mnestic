/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What the cached graph projection buys, and what it costs.
//!
//! On LDBC SNB sf1 (10,620 persons / 438,900 directed edges) a canned
//! `ConnectedComponents` spends ~136 ms, of which ~54 ms is the scan of
//! `*knows` and ~50 ms is the CSR build — **~86% setup, rebuilt every call**.
//! The projection caches the CSR and revalidates it against the consuming
//! transaction's snapshot, so the setup is paid once per write, not once per
//! query.
//!
//! Four groups:
//!
//! - `cold`: positional form, i.e. today's behaviour (scan + build + kernel)
//! - `warm`: `graph: 'g'` against a resident, unchanged variant
//! - `invalidated`: `graph: 'g'` with a one-row write before every call —
//!   the "never worse than today" bound, in the worst case
//! - `list`: `::graph list`, so the observability surface is not free-riding
//!
//! Run: `cargo bench -p mnestic --bench graph_projection`

use std::collections::BTreeMap;

use cozo::{DataValue, DbInstance, ScriptMutability};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

/// Vertices in the synthetic graph. Each has `DEGREE` out-edges, so the edge
/// relation holds `N * DEGREE` rows — the same order as LDBC sf1's `knows`.
const N: i64 = 20_000;
const DEGREE: i64 = 20;

fn run(db: &DbInstance, script: &str) {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("{script}\n{e:?}"));
}

/// A deterministic "small-world"-ish graph: `i -> (i*k + 7k) mod N` for k in 1..=DEGREE.
/// No randomness, so every run of the bench sees exactly the same CSR.
fn make_db() -> DbInstance {
    let db = DbInstance::new("mem", "", Default::default()).unwrap();
    run(&db, ":create knows {a: Int, b: Int}");

    let mut rows: Vec<DataValue> = Vec::with_capacity((N * DEGREE) as usize);
    for i in 0..N {
        for k in 1..=DEGREE {
            let j = (i * k + 7 * k) % N;
            rows.push(DataValue::List(vec![
                DataValue::from(i),
                DataValue::from(j),
            ]));
        }
    }
    // One `:put` per 100k rows keeps the constant rule's parse tree sane.
    for chunk in rows.chunks(100_000) {
        db.run_script(
            "?[a, b] <- $rows :put knows {a, b}",
            BTreeMap::from([("rows".to_string(), DataValue::List(chunk.to_vec()))]),
            ScriptMutability::Mutable,
        )
        .unwrap();
    }
    run(&db, "::graph create g {edges: knows}");
    db
}

/// A self-loop on a fresh synthetic id. It invalidates `g` while leaving the
/// timed kernel's asymptotics alone — but note it is NOT structure-neutral:
/// every edge endpoint becomes a CSR vertex, so each write adds one new
/// singleton component to the graph. Note also that the write itself sits
/// inside the timed region below, so the "invalidated" numbers carry the
/// write's cost — a bias *against* the projection, i.e. conservative for the
/// "never worse than today" bound they exist to check.
fn dirty(db: &DbInstance, i: i64) {
    db.run_script(
        "?[a, b] <- [[$i, $i]] :put knows {a, b}",
        BTreeMap::from([("i".to_string(), DataValue::from(i))]),
        ScriptMutability::Mutable,
    )
    .unwrap();
}

const KERNELS: &[(&str, &str, &str)] = &[
    (
        "connected_components",
        "?[n, c] <~ ConnectedComponents(*knows[a, b])",
        "?[n, c] <~ ConnectedComponents(graph: 'g')",
    ),
    (
        "pagerank",
        "?[n, r] <~ PageRank(*knows[a, b])",
        "?[n, r] <~ PageRank(graph: 'g')",
    ),
    (
        "clustering_coefficients",
        "?[n, c, t, d] <~ ClusteringCoefficients(*knows[a, b])",
        "?[n, c, t, d] <~ ClusteringCoefficients(graph: 'g')",
    ),
];

fn bench(c: &mut Criterion) {
    let db = make_db();

    let mut cold = c.benchmark_group("cold");
    cold.sample_size(10);
    for (name, positional, _) in KERNELS {
        cold.bench_function(BenchmarkId::from_parameter(name), |b| {
            b.iter(|| run(&db, positional))
        });
    }
    cold.finish();

    let mut warm = c.benchmark_group("warm");
    warm.sample_size(10);
    for (name, _, projected) in KERNELS {
        // Prime the variant so the first sample is a hit like every other.
        run(&db, projected);
        warm.bench_function(BenchmarkId::from_parameter(name), |b| {
            b.iter(|| run(&db, projected))
        });
    }
    warm.finish();

    // The degenerate case the freshness protocol is judged on: a write between
    // every pair of queries. Must stay within noise of `cold` — a projection
    // under total write churn degrades to build-per-query, never worse.
    let mut invalidated = c.benchmark_group("invalidated");
    invalidated.sample_size(10);
    for (name, _, projected) in KERNELS {
        let mut i = N;
        invalidated.bench_function(BenchmarkId::from_parameter(name), |b| {
            b.iter(|| {
                i += 1;
                dirty(&db, i);
                run(&db, projected)
            })
        });
    }
    invalidated.finish();

    c.bench_function("list", |b| b.iter(|| run(&db, "::graph list")));
}

criterion_group!(benches, bench);
criterion_main!(benches);
