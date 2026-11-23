// SPDX-License-Identifier: MPL-2.0
//
// HNSW A/B harness: insert/replace cost and recall@10 against brute force, on
// three paths (incremental build on a plain relation, the same on a tt-only
// relation, and the `::hnsw create` bulk build). RocksDB by default; any
// engine compiled in can be named (the default build has only sqlite/mem).
//
//   cargo run --release -p mnestic --features storage-rocksdb --example hnsw_ab \
//       -- <n> <dim> <seed> [m=16] [ef_construction=50] [engine=rocksdb]
//
// Data: `n` vectors drawn around 200 Gaussian centres, `n/10` replacements and
// 200 queries from the same distribution; Cosine distance. Defaults match
// mindgraph's index (`m: 16`, `ef_construction: 50`). Output is one line per
// phase plus recall@10 and mean query time at ef 16/32/64/128.
//
// For an A/B, run it at the same arguments in two checkouts (e.g. a worktree
// of origin/main and your branch), with two or more seeds each, on one
// otherwise idle machine. Level assignment uses an unseeded RNG, so a seed
// fixes the data but not the graph: treat recall differences under ~0.5
// points and timing differences under ~5% as noise. First used for the
// layer-0 insertion-degree change (mnestic #105).
use std::collections::BTreeMap;
use std::time::Instant;

use cozo::{DataValue, DbInstance, ScriptMutability};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

const K: usize = 10;

fn gauss(rng: &mut StdRng) -> f32 {
    // Box-Muller
    let u1: f64 = rng.gen_range(1e-12..1.0);
    let u2: f64 = rng.gen_range(0.0..1.0);
    ((-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()) as f32
}

fn cos_dist(a: &[f32], b: &[f32]) -> f64 {
    let (mut d, mut na, mut nb) = (0f64, 0f64, 0f64);
    for i in 0..a.len() {
        d += (a[i] * b[i]) as f64;
        na += (a[i] * a[i]) as f64;
        nb += (b[i] * b[i]) as f64;
    }
    1.0 - d / (na.sqrt() * nb.sqrt())
}

fn rows_param(data: &[(i64, Vec<f32>)]) -> DataValue {
    DataValue::List(
        data.iter()
            .map(|(k, v)| {
                DataValue::List(vec![
                    DataValue::from(*k),
                    DataValue::List(v.iter().map(|x| DataValue::from(*x as f64)).collect()),
                ])
            })
            .collect(),
    )
}

fn put(db: &DbInstance, rel: &str, data: &[(i64, Vec<f32>)]) {
    let mut p = BTreeMap::new();
    p.insert("rows".to_string(), rows_param(data));
    db.run_script(
        &format!("input[k, x] <- $rows ?[k, e] := input[k, x], e = vec(x) :put {rel} {{k => e}}"),
        p,
        ScriptMutability::Mutable,
    )
    .unwrap();
}

fn create_index(db: &DbInstance, rel: &str, dim: usize, m: usize, efc: usize) {
    db.run_script(
        &format!("::hnsw create {rel}:idx {{ dim: {dim}, dtype: F32, fields: [e], distance: Cosine, m: {m}, ef_construction: {efc}, extend_candidates: false, keep_pruned_connections: false }}"),
        Default::default(),
        ScriptMutability::Mutable,
    )
    .unwrap();
}

fn recall(
    db: &DbInstance,
    rel: &str,
    data: &BTreeMap<i64, Vec<f32>>,
    queries: &[Vec<f32>],
    label: &str,
) {
    let truth: Vec<Vec<i64>> = queries
        .iter()
        .map(|q| {
            let mut d: Vec<(f64, i64)> = data.iter().map(|(k, v)| (cos_dist(q, v), *k)).collect();
            d.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
            d.into_iter().take(K).map(|x| x.1).collect()
        })
        .collect();
    for ef in [16usize, 32, 64, 128] {
        let mut hit = 0usize;
        let t = Instant::now();
        for (q, tr) in queries.iter().zip(&truth) {
            let mut p = BTreeMap::new();
            p.insert(
                "q".to_string(),
                DataValue::List(q.iter().map(|x| DataValue::from(*x as f64)).collect()),
            );
            let res = db
                .run_script(
                    &format!("?[k] := ~{rel}:idx{{k | query: vec($q), k: {K}, ef: {ef}}}"),
                    p,
                    ScriptMutability::Immutable,
                )
                .unwrap();
            for row in res.rows {
                if tr.contains(&row[0].get_int().unwrap()) {
                    hit += 1;
                }
            }
        }
        let us = t.elapsed().as_micros() as f64 / queries.len() as f64;
        println!(
            "{label} ef={ef:<3} recall@10={:.4} query_us={us:.0}",
            hit as f64 / (queries.len() * K) as f64
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: hnsw_ab <n> <dim> <seed> [m=16] [ef_construction=50] [engine=rocksdb]");
        std::process::exit(2);
    }
    let n: usize = args[1].parse().unwrap();
    let dim: usize = args[2].parse().unwrap();
    let seed: u64 = args[3].parse().unwrap();
    let m: usize = args.get(4).map_or(16, |s| s.parse().unwrap());
    let efc: usize = args.get(5).map_or(50, |s| s.parse().unwrap());
    let engine = args.get(6).map_or("rocksdb", String::as_str);
    println!("n={n} dim={dim} seed={seed} m={m} ef_construction={efc} engine={engine}");
    let mut rng = StdRng::seed_from_u64(seed);
    let n_centers = 200;
    let centers: Vec<Vec<f32>> = (0..n_centers)
        .map(|_| (0..dim).map(|_| gauss(&mut rng)).collect())
        .collect();
    let point = |rng: &mut StdRng| -> Vec<f32> {
        let c = &centers[rng.gen_range(0..n_centers)];
        c.iter().map(|x| x + 0.6 * gauss(rng)).collect()
    };
    let data: Vec<(i64, Vec<f32>)> = (0..n as i64).map(|k| (k, point(&mut rng))).collect();
    let n_upd = n / 10;
    let updates: Vec<(i64, Vec<f32>)> = (0..n_upd)
        .map(|_| (rng.gen_range(0..n as i64), point(&mut rng)))
        .collect();
    let queries: Vec<Vec<f32>> = (0..200).map(|_| point(&mut rng)).collect();

    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new(engine, dir.path().join("db").to_str().unwrap(), "").unwrap();
    let schema = |rel: &str| format!(":create {rel} {{k: Int => e: <F32; {dim}>}}");
    db.run_script(
        &schema("inc"),
        Default::default(),
        ScriptMutability::Mutable,
    )
    .unwrap();
    db.run_script(
        &schema("bulk"),
        Default::default(),
        ScriptMutability::Mutable,
    )
    .unwrap();
    db.run_script(
        &format!(":create ttinc {{k: Int, tt: TxTime => e: <F32; {dim}>}}"),
        Default::default(),
        ScriptMutability::Mutable,
    )
    .unwrap();

    // Incremental: index first, then 100-row batches through the insert path.
    create_index(&db, "inc", dim, m, efc);
    let t = Instant::now();
    for chunk in data.chunks(100) {
        put(&db, "inc", chunk);
    }
    let ins = t.elapsed();
    let t = Instant::now();
    for chunk in updates.chunks(10) {
        put(&db, "inc", chunk);
    }
    let upd = t.elapsed();
    println!(
        "incremental insert_total_s={:.2} per_row_us={:.0} | replace_total_s={:.2} per_row_us={:.0}",
        ins.as_secs_f64(),
        ins.as_micros() as f64 / n as f64,
        upd.as_secs_f64(),
        upd.as_micros() as f64 / n_upd as f64
    );
    let mut cur: BTreeMap<i64, Vec<f32>> = data.iter().cloned().collect();
    for (k, v) in &updates {
        cur.insert(*k, v.clone());
    }
    recall(&db, "inc", &cur, &queries, "incremental");

    // Same workload on a tt-only (TxTime) relation.
    create_index(&db, "ttinc", dim, m, efc);
    let t = Instant::now();
    for chunk in data.chunks(100) {
        put(&db, "ttinc", chunk);
    }
    let ins = t.elapsed();
    let t = Instant::now();
    for chunk in updates.chunks(10) {
        put(&db, "ttinc", chunk);
    }
    let upd = t.elapsed();
    println!(
        "txtime insert_total_s={:.2} per_row_us={:.0} | replace_total_s={:.2} per_row_us={:.0}",
        ins.as_secs_f64(),
        ins.as_micros() as f64 / n as f64,
        upd.as_secs_f64(),
        upd.as_micros() as f64 / n_upd as f64
    );
    recall(&db, "ttinc", &cur, &queries, "txtime");

    // Bulk: data first, then the parallel flat build.
    for chunk in data.chunks(1000) {
        put(&db, "bulk", chunk);
    }
    let t = Instant::now();
    create_index(&db, "bulk", dim, m, efc);
    println!("bulk build_s={:.2}", t.elapsed().as_secs_f64());
    let base: BTreeMap<i64, Vec<f32>> = data.iter().cloned().collect();
    recall(&db, "bulk", &base, &queries, "bulk");
}
