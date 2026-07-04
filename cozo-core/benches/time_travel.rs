/*
 * Copyright 2022, The Cozo Project Authors (original vt-only workload design).
 * Copyright 2026, Shan Rizvi (mnestic fork: criterion rewrite + bitemporal matrix).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Temporal-read budget bench (bitemporality step 6, `docs/specs/bitemporality.md` §9).
//!
//! The §9 regression budget: bitemporal reads ≤ ~10% over **"the identical
//! workload on a vt-only relation at `@ 'NOW'`"**; tt-only current reads at
//! parity with the same baseline (they ride the same single-axis machinery);
//! zero impact on non-temporal relations (opt-in — different dispatch path;
//! `plain` is included as a reference row).
//!
//! Matrix: versions-per-key × corrections-depth. Every `bt_{V}_{C}` relation
//! holds, per key, V vt-groups × (C+1) records (the original belief plus C
//! corrections, each correction pass a separate import ⇒ its own tt).
//! The gate cell is `bt_{V}_0` vs `vt_{V}`: identical logical content,
//! identical physical row count — the delta is pure two-level-resolution
//! machinery. C > 0 cells measure correction-depth sensitivity (a vt-only
//! relation cannot represent corrections, so those rows are informational).
//!
//! Workloads: point reads (`k = $id`) and full-scan aggregation (`sum(x)`),
//! plus an as-of-past-tt point read (the time-travel axis itself).
//!
//! Run:  cargo bench -p mnestic --bench time_travel
//! Backend: sqlite by default; MNESTIC_BACKEND=mem|rocksdb to override
//! (rocksdb needs `--features storage-rocksdb`).

use std::cell::Cell;
use std::collections::BTreeMap;

use cozo::{DataValue, DbInstance, NamedRows, ScriptMutability, Validity};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

const N_KEYS: i64 = 1000;
const VERSIONS: [i64; 3] = [1, 10, 100];
const CORRECTIONS: [i64; 2] = [0, 2];

fn backend() -> String {
    std::env::var("MNESTIC_BACKEND").unwrap_or_else(|_| "sqlite".to_string())
}

fn run(db: &DbInstance, script: &str, params: BTreeMap<String, DataValue>) -> NamedRows {
    db.run_script(script, params, ScriptMutability::Mutable)
        .unwrap()
}

fn import(db: &DbInstance, rel: &str, headers: &[&str], rows: Vec<Vec<DataValue>>) {
    let mut data = BTreeMap::new();
    data.insert(
        rel.to_string(),
        NamedRows {
            headers: headers.iter().map(|s| s.to_string()).collect(),
            rows,
            next: None,
        },
    );
    db.import_relations(data).unwrap();
}

struct Setup {
    _dir: tempfile::TempDir,
    db: DbInstance,
    /// A committed mid-history tt of `bt_10_2` (after the first correction
    /// pass), for the as-of-past-tt read.
    past_tt: i64,
}

fn make_db() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new(
        &backend(),
        dir.path().join("time_travel.db").to_str().unwrap(),
        "",
    )
    .unwrap();

    run(&db, ":create plain {k: Int => x: Int}", BTreeMap::new());
    import(
        &db,
        "plain",
        &["k", "x"],
        (0..N_KEYS)
            .map(|k| vec![DataValue::from(k), DataValue::from(k)])
            .collect(),
    );

    run(
        &db,
        ":create tt_only {k: Int, tt: TxTime => x: Int}",
        BTreeMap::new(),
    );
    import(
        &db,
        "tt_only",
        &["k", "x"],
        (0..N_KEYS)
            .map(|k| vec![DataValue::from(k), DataValue::from(k)])
            .collect(),
    );

    for v in VERSIONS {
        let rel = format!("vt_{v}");
        run(
            &db,
            &format!(":create {rel} {{k: Int, vld: Validity => x: Int}}"),
            BTreeMap::new(),
        );
        import(
            &db,
            &rel,
            &["k", "vld", "x"],
            (0..N_KEYS)
                .flat_map(|k| {
                    (0..v).map(move |ver| {
                        vec![
                            DataValue::from(k),
                            DataValue::Validity(Validity::from((ver, true))),
                            DataValue::from(k * 1000 + ver),
                        ]
                    })
                })
                .collect(),
        );
    }

    for v in VERSIONS {
        for c in CORRECTIONS {
            let rel = format!("bt_{v}_{c}");
            run(
                &db,
                &format!(":create {rel} {{k: Int, vld: Validity, tt: TxTime => x: Int}}"),
                BTreeMap::new(),
            );
            // pass 0 = original belief; each later pass re-asserts every
            // (k, vld) with a new value — one import per pass, one tt per pass
            for pass in 0..=c {
                import(
                    &db,
                    &rel,
                    &["k", "vld", "x"],
                    (0..N_KEYS)
                        .flat_map(|k| {
                            (0..v).map(move |ver| {
                                vec![
                                    DataValue::from(k),
                                    DataValue::Validity(Validity::from((ver, true))),
                                    DataValue::from(k * 1000 + ver * 10 + pass),
                                ]
                            })
                        })
                        .collect(),
                );
            }
        }
    }

    // a mid-history tt for the as-of read: the first correction pass of
    // bt_10_2 (::history returns integer-µs tts, newest first)
    let hist = run(&db, "::history bt_10_2 [[0]] 100", BTreeMap::new());
    let tt_idx = hist.headers.iter().position(|h| h == "tt").unwrap();
    let mut tts: Vec<i64> = hist
        .rows
        .iter()
        .map(|r| r[tt_idx].get_int().unwrap())
        .collect();
    tts.sort_unstable();
    tts.dedup();
    assert_eq!(tts.len(), 3, "three import passes = three tts");
    let past_tt = tts[1];

    // sanity: the gate cell resolves the same version as its baseline
    // (values encode differently: vt = k*1000 + ver, bt = k*1000 + ver*10 + pass)
    let params = BTreeMap::from([("id".to_string(), DataValue::from(7))]);
    let base = run(&db, "?[x] := *vt_10{k: $id, x @ 'NOW'}", params.clone());
    let bt = run(
        &db,
        "?[x] := *bt_10_0{k: $id, x @ (vt: 'NOW')}",
        params.clone(),
    );
    let base_ver = base.rows[0][0].get_int().unwrap() - 7000;
    let bt_x = bt.rows[0][0].get_int().unwrap() - 7000;
    assert_eq!(
        base_ver,
        bt_x / 10,
        "gate cell must resolve the same version"
    );
    assert_eq!(bt_x % 10, 0, "the c0 cell has only pass 0");
    // corrected cell: current belief = the LAST correction pass…
    let bt2 = run(
        &db,
        "?[x] := *bt_10_2{k: $id, x @ (vt: 'NOW')}",
        params.clone(),
    );
    assert_eq!(bt2.rows[0][0].get_int().unwrap() - 7000, base_ver * 10 + 2);
    // …and as-of the first correction's tt = pass 1
    let bt2p = run(
        &db,
        &format!("?[x] := *bt_10_2{{k: $id, x @ (vt: 'NOW', tt: {past_tt})}}"),
        params,
    );
    assert_eq!(bt2p.rows[0][0].get_int().unwrap() - 7000, base_ver * 10 + 1);

    Setup {
        _dir: dir,
        db,
        past_tt,
    }
}

fn bench_point_reads(c: &mut Criterion) {
    let setup = make_db();
    let db = &setup.db;
    let mut group = c.benchmark_group(format!("point/{}", backend()));
    let next_id = Cell::new(0i64);
    let id_param = move || {
        let i = next_id.get();
        next_id.set((i + 7919) % N_KEYS); // large prime stride, deterministic
        BTreeMap::from([("id".to_string(), DataValue::from(i))])
    };

    group.bench_function("plain", |b| {
        b.iter(|| run(db, "?[x] := *plain{k: $id, x}", id_param()))
    });
    group.bench_function("tt_only_current", |b| {
        b.iter(|| run(db, "?[x] := *tt_only{k: $id, x}", id_param()))
    });
    for v in VERSIONS {
        group.bench_function(BenchmarkId::new("vt_now", v), |b| {
            b.iter(|| {
                run(
                    db,
                    &format!("?[x] := *vt_{v}{{k: $id, x @ 'NOW'}}"),
                    id_param(),
                )
            })
        });
    }
    for v in VERSIONS {
        for cd in CORRECTIONS {
            group.bench_function(
                BenchmarkId::new("bt_now_current", format!("v{v}_c{cd}")),
                |b| {
                    b.iter(|| {
                        run(
                            db,
                            &format!("?[x] := *bt_{v}_{cd}{{k: $id, x @ (vt: 'NOW')}}"),
                            id_param(),
                        )
                    })
                },
            );
        }
    }
    // the time-travel axis itself: as-of a mid-history tt
    let past_tt = setup.past_tt;
    group.bench_function("bt_now_at_past_tt/v10_c2", |b| {
        b.iter(|| {
            run(
                db,
                &format!("?[x] := *bt_10_2{{k: $id, x @ (vt: 'NOW', tt: {past_tt})}}"),
                id_param(),
            )
        })
    });
    group.finish();
}

fn bench_scans(c: &mut Criterion) {
    let setup = make_db();
    let db = &setup.db;
    let mut group = c.benchmark_group(format!("scan/{}", backend()));
    group.sample_size(10);

    group.bench_function("plain", |b| {
        b.iter(|| run(db, "?[sum(x)] := *plain{x}", BTreeMap::new()))
    });
    group.bench_function("tt_only_current", |b| {
        b.iter(|| run(db, "?[sum(x)] := *tt_only{x}", BTreeMap::new()))
    });
    for v in VERSIONS {
        group.bench_function(BenchmarkId::new("vt_now", v), |b| {
            b.iter(|| {
                run(
                    db,
                    &format!("?[sum(x)] := *vt_{v}{{x @ 'NOW'}}"),
                    BTreeMap::new(),
                )
            })
        });
    }
    for v in VERSIONS {
        for cd in CORRECTIONS {
            group.bench_function(
                BenchmarkId::new("bt_now_current", format!("v{v}_c{cd}")),
                |b| {
                    b.iter(|| {
                        run(
                            db,
                            &format!("?[sum(x)] := *bt_{v}_{cd}{{x @ (vt: 'NOW')}}"),
                            BTreeMap::new(),
                        )
                    })
                },
            );
        }
    }
    group.finish();
}

criterion_group!(benches, bench_point_reads, bench_scans);
criterion_main!(benches);
