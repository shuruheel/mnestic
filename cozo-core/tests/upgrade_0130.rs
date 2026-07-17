/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Cross-version release gate for the mnestic 0.12.2 -> 0.13.0 upgrade note.
//!
//! This test is intentionally ignored during the ordinary suite. The release
//! rehearsal script compiles this same source against v0.12.2 to seed affected
//! stores, against the 0.13.0 correctness union to execute the documented
//! repairs, and once more against v0.12.2 to prove storage compatibility.

use cozo::{DataValue, DbInstance, NamedRows, ScriptMutability};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

const PHASE_ENV: &str = "MNESTIC_UPGRADE_PHASE";
const ROOT_ENV: &str = "MNESTIC_UPGRADE_ROOT";

fn run(db: &DbInstance, script: &str) -> NamedRows {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|error| panic!("script failed: {error:?}\n--- script ---\n{script}"))
}

fn run_result(db: &DbInstance, script: &str) -> miette::Result<NamedRows> {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
}

fn open(root: &Path, name: &str) -> DbInstance {
    let path = root.join(name);
    DbInstance::new("sqlite", path.to_str().unwrap(), "")
        .unwrap_or_else(|error| panic!("failed to open {}: {error:?}", path.display()))
}

fn count(db: &DbInstance, script: &str) -> i64 {
    run(db, script).rows[0][0]
        .get_int()
        .unwrap_or_else(|| panic!("count query did not return an integer: {script}"))
}

fn query_strings(db: &DbInstance, script: &str) -> Vec<String> {
    run(db, script)
        .rows
        .into_iter()
        .map(|row| row[0].get_str().unwrap().to_string())
        .collect()
}

fn hnsw_unpaired(db: &DbInstance, relation: &str, index: &str) -> i64 {
    count(
        db,
        &format!(
            "edge[fr, to] := *{relation}:{index}{{fr_id: fr, to_id: to}}, fr != to\n\
             unpaired[fr, to] := edge[fr, to], not edge[to, fr]\n\
             ?[count(fr)] := unpaired[fr, to]"
        ),
    )
}

fn hnsw_nodes_for(db: &DbInstance, relation: &str, index: &str, id: i64) -> i64 {
    count(
        db,
        &format!(
            "node[other] := *{relation}:{index}{{fr_id: {id}, to_id: other}}\n\
             node[other] := *{relation}:{index}{{fr_id: other, to_id: {id}}}\n\
             ?[count(other)] := node[other]"
        ),
    )
}

fn seed_pre_epoch(root: &Path) {
    let db = open(root, "pre-epoch.db");
    run(
        &db,
        ":create events {k: String, at: Validity => value: String}",
    );
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        db.run_script(
            "?[k, at, value] <- [['moon', '1969-07-20T00:00:00Z', 'landed']] \
             :put events {k, at => value}",
            BTreeMap::new(),
            ScriptMutability::Mutable,
        )
    }));
    assert!(
        outcome.is_err(),
        "v0.12.2 precondition failed: the pre-epoch write did not panic"
    );
}

fn seed_hnsw(root: &Path) {
    let db = open(root, "hnsw.db");
    run(&db, ":create pts {id: Int => emb: <F32; 8>?}");

    let mut state = 0xD1B54A32D192ED03u64;
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((state >> 33) as f32 / (1u64 << 31) as f32) - 1.0
    };
    let rows = (0..300)
        .map(|id| {
            let values = (0..8)
                .map(|_| format!("{:.6}", next()))
                .collect::<Vec<_>>()
                .join(",");
            format!("[{id}, [{values}]]")
        })
        .collect::<Vec<_>>()
        .join(",");
    run(
        &db,
        &format!("?[id, emb] <- [{rows}] :put pts {{id => emb}}"),
    );
    run(
        &db,
        "::hnsw create pts:idx {dim: 8, m: 4, dtype: F32, fields: [emb], \
         distance: L2, ef_construction: 32}",
    );
    assert!(
        hnsw_unpaired(&db, "pts", "idx") > 0,
        "v0.12.2 precondition failed: bulk build produced no unpaired edge rows"
    );

    run(&db, "?[id, emb] <- [[0, null]] :put pts {id => emb}");
    assert!(
        hnsw_nodes_for(&db, "pts", "idx", 0) > 0,
        "v0.12.2 precondition failed: nulling a vector did not strand its HNSW nodes"
    );

    run(&db, ":create zero_pts {id: Int => emb: <F32; 2>}");
    run(
        &db,
        "?[id, emb] <- [[0, vec([0.0, 0.0])], [1, vec([1.0, 0.0])]] \
         :put zero_pts {id => emb}",
    );
    run(
        &db,
        "::hnsw create zero_pts:idx {dim: 2, m: 8, dtype: F32, fields: [emb], \
         distance: Cosine, ef_construction: 32}",
    );
}

fn corrupt_sqlite_value(path: &Path) {
    let connection = sqlite::open(path).unwrap();
    connection
        .execute(
            "UPDATE cozo SET v = x'910102' \
             WHERE instr(v, x'636f72727570742d6d65') > 0;",
        )
        .unwrap();
}

fn seed_corrupt_value(root: &Path) {
    let path = root.join("corrupt.db");
    {
        let db = open(root, "corrupt.db");
        run(&db, ":create damaged {k: Int => body: String}");
        run(
            &db,
            "::fts create damaged:fts {extractor: body, tokenizer: Simple, filters: [Lowercase]}",
        );
        run(
            &db,
            "?[k, body] <- [[1, 'intact'], [2, 'corrupt-me']] :put damaged {k => body}",
        );
    }
    corrupt_sqlite_value(&path);

    let scan = {
        let db = open(root, "corrupt.db");
        catch_unwind(AssertUnwindSafe(|| {
            run(&db, "?[k, body] := *damaged{k, body}")
        }))
    };
    assert!(
        scan.is_err(),
        "v0.12.2 precondition failed: a corrupt value blob did not panic a scan"
    );

    let repair = {
        let db = open(root, "corrupt.db");
        catch_unwind(AssertUnwindSafe(|| run(&db, "::repair_corrupt damaged")))
    };
    assert!(
        repair.is_err(),
        "v0.12.2 precondition failed: ::repair_corrupt did not panic on the corrupt row"
    );
}

fn seed_restore_collision(root: &Path) {
    let source = open(root, "restore-source.db");
    run(&source, ":create alpha {k: Int => value: String}");
    run(&source, ":create beta {k: Int => value: String}");
    run(
        &source,
        "?[k, value] <- [[1, 'alpha-one']] :put alpha {k => value}",
    );
    let backup = root.join("restore-original-backup.db");
    source.backup_db(&backup).unwrap();
    drop(source);

    let damaged = open(root, "restore-damaged.db");
    damaged.restore_backup(&backup).unwrap();
    run(&damaged, ":create gamma {k: Int => value: String}");
    run(
        &damaged,
        "?[k, value] <- [[2, 'gamma-two']] :put gamma {k => value}",
    );
    assert_eq!(
        count(&damaged, "?[count(k)] := *alpha{k}"),
        2,
        "v0.12.2 precondition failed: restored alpha and new gamma did not collide"
    );
}

fn seed_fts_ghost(root: &Path) {
    let db = open(root, "fts-ghost.db");
    run(&db, ":create doc {k: String => body: String}");
    run(
        &db,
        "?[k, body] <- [['k1', 'hello world alpha beta gamma']] :put doc {k => body}",
    );
    run(
        &db,
        "::fts create doc:fts {extractor: body, tokenizer: Simple, filters: [Lowercase]}",
    );
    db.import_relations(BTreeMap::from([(
        "doc".to_string(),
        NamedRows::new(
            vec!["k".to_string(), "body".to_string()],
            vec![vec![DataValue::from("k1"), DataValue::from("goodbye moon")]],
        ),
    )]))
    .unwrap();
    assert_eq!(
        count(
            &db,
            "hit[k] := ~doc:fts{k | query: 'hello', k: 10} ?[count(k)] := hit[k]",
        ),
        1,
        "v0.12.2 precondition failed: the ghost FTS posting was not present"
    );
}

fn seed(root: &Path) {
    assert!(
        fs::read_dir(root).unwrap().next().is_none(),
        "rehearsal root must be empty: {}",
        root.display()
    );
    seed_pre_epoch(root);
    seed_hnsw(root);
    seed_corrupt_value(root);
    seed_restore_collision(root);
    seed_fts_ghost(root);
    println!("SEED PASS: v0.12.2 fixtures created at {}", root.display());
}

fn verify_pre_epoch(root: &Path) {
    let db = open(root, "pre-epoch.db");
    run(
        &db,
        "?[k, at, value] <- [['moon', '1969-07-20T00:00:00Z', 'landed']] \
         :put events {k, at => value}",
    );
    let values = query_strings(
        &db,
        "?[value] := *events{k: 'moon', value @ '1969-12-01T00:00:00Z'}",
    );
    assert_eq!(values, vec!["landed"]);
}

fn verify_hnsw(root: &Path) {
    let db = open(root, "hnsw.db");
    assert!(hnsw_unpaired(&db, "pts", "idx") > 0);
    assert!(hnsw_nodes_for(&db, "pts", "idx", 0) > 0);

    let repair = run(&db, "::repair_corrupt pts");
    assert_eq!(repair.rows[0][0].get_int(), Some(0));
    run(&db, "::reindex pts");
    assert_eq!(hnsw_unpaired(&db, "pts", "idx"), 0);
    assert_eq!(hnsw_nodes_for(&db, "pts", "idx", 0), 0);
    assert_eq!(count(&db, "?[count(id)] := *pts{id}"), 300);
    assert!(
        !run(
            &db,
            "?[id] := ~pts:idx{id | query: vec([0.2, 0.1, 0.3, -0.2, 0.4, 0.1, -0.1, 0.2]), k: 10, ef: 64}",
        )
        .rows
        .is_empty()
    );

    let zero_search = run(
        &db,
        "?[id, dist] := ~zero_pts:idx{id | query: vec([1.0, 0.0]), k: 2, ef: 16, bind_distance: dist} :order id",
    );
    assert_eq!(zero_search.rows.len(), 2);
    assert!(zero_search
        .rows
        .iter()
        .all(|row| row[1].get_float().unwrap().is_finite()));
    run(&db, "::reindex zero_pts");
    let persisted = run(&db, "?[dist] := *zero_pts:idx{dist}");
    let distances = persisted
        .rows
        .iter()
        .filter_map(|row| row[0].get_float())
        .collect::<Vec<_>>();
    assert!(!distances.is_empty());
    assert!(distances.iter().all(|distance| distance.is_finite()));
}

fn verify_corrupt_value(root: &Path) {
    let db = open(root, "corrupt.db");
    let scan_error = run_result(&db, "?[k, body] := *damaged{k, body}")
        .expect_err("the corrupt row must be an ordinary query error");
    assert!(format!("{scan_error:?}").contains("eval::corrupt_value_blob"));
    let lookup_error = run_result(
        &db,
        "wanted[k] <- [[2]] ?[body] := wanted[k], *damaged{k, body}",
    )
    .expect_err("the point lookup must propagate the decode error");
    assert!(format!("{lookup_error:?}").contains("eval::corrupt_value_blob"));

    let repair = run(&db, "::repair_corrupt damaged");
    assert_eq!(repair.rows[0][0].get_int(), Some(1));
    run(&db, "::reindex damaged");
    assert_eq!(
        query_strings(&db, "?[body] := *damaged{k, body}"),
        vec!["intact"]
    );
    assert_eq!(
        count(
            &db,
            "hit[k] := ~damaged:fts{k | query: 'intact', k: 10} ?[count(k)] := hit[k]",
        ),
        1
    );
}

fn verify_restore_collision(root: &Path) {
    let damaged = open(root, "restore-damaged.db");
    assert_eq!(count(&damaged, "?[count(k)] := *alpha{k}"), 2);
    run(&damaged, ":create delta {k: Int => value: String}");
    run(
        &damaged,
        "?[k, value] <- [[3, 'delta-three']] :put delta {k => value}",
    );
    assert_eq!(count(&damaged, "?[count(k)] := *delta{k}"), 1);
    assert_eq!(count(&damaged, "?[count(k)] := *alpha{k}"), 2);
    drop(damaged);

    let recovered = open(root, "restore-recovered.db");
    recovered
        .restore_backup(root.join("restore-original-backup.db"))
        .unwrap();
    run(&recovered, ":create gamma {k: Int => value: String}");
    run(
        &recovered,
        "?[k, value] <- [[2, 'gamma-two']] :put gamma {k => value}",
    );
    assert_eq!(
        query_strings(&recovered, "?[value] := *alpha{k, value}"),
        vec!["alpha-one"]
    );
    assert_eq!(
        query_strings(&recovered, "?[value] := *gamma{k, value}"),
        vec!["gamma-two"]
    );
}

fn verify_fts_ghost(root: &Path) {
    let db = open(root, "fts-ghost.db");
    assert_eq!(
        count(
            &db,
            "hit[k] := ~doc:fts{k | query: 'hello', k: 10} ?[count(k)] := hit[k]",
        ),
        1
    );
    run(&db, "::reindex doc");
    assert_eq!(
        count(
            &db,
            "hit[k] := ~doc:fts{k | query: 'hello', k: 10} ?[count(k)] := hit[k]",
        ),
        0
    );
    assert_eq!(
        count(
            &db,
            "hit[k] := ~doc:fts{k | query: 'goodbye', k: 10} ?[count(k)] := hit[k]",
        ),
        1
    );
}

fn verify(root: &Path) {
    verify_pre_epoch(root);
    verify_hnsw(root);
    verify_corrupt_value(root);
    verify_restore_collision(root);
    verify_fts_ghost(root);
    println!("UPGRADE PASS: 0.13.0 recovery instructions succeeded");
}

fn backward_compatibility(root: &Path) {
    let pre_epoch = open(root, "pre-epoch.db");
    assert_eq!(count(&pre_epoch, "?[count(k)] := *events{k}"), 1);

    let hnsw = open(root, "hnsw.db");
    assert_eq!(count(&hnsw, "?[count(id)] := *pts{id}"), 300);

    let repaired = open(root, "corrupt.db");
    assert_eq!(
        query_strings(&repaired, "?[body] := *damaged{k, body}"),
        vec!["intact"]
    );

    let restored = open(root, "restore-recovered.db");
    assert_eq!(
        query_strings(&restored, "?[value] := *alpha{k, value}"),
        vec!["alpha-one"]
    );
    assert_eq!(
        query_strings(&restored, "?[value] := *gamma{k, value}"),
        vec!["gamma-two"]
    );

    let fts = open(root, "fts-ghost.db");
    assert_eq!(
        count(
            &fts,
            "hit[k] := ~doc:fts{k | query: 'goodbye', k: 10} ?[count(k)] := hit[k]",
        ),
        1
    );
    println!("BACKWARD PASS: v0.12.2 reopened the 0.13.0-updated stores");
}

#[test]
#[ignore = "release-only cross-version gate; run scripts/rehearse-0130-upgrade.sh"]
fn upgrade_0130_rehearsal() {
    let root = PathBuf::from(env::var(ROOT_ENV).expect("MNESTIC_UPGRADE_ROOT is required"));
    fs::create_dir_all(&root).unwrap();
    match env::var(PHASE_ENV).as_deref() {
        Ok("seed") => seed(&root),
        Ok("verify") => verify(&root),
        Ok("backward") => backward_compatibility(&root),
        other => panic!("{PHASE_ENV} must be seed, verify, or backward; got {other:?}"),
    }
}
