/*
 * Copyright 2023, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Regression coverage for pre-epoch timestamp parsing and valid-time storage.

use cozo::{DbInstance, ScriptMutability};
use serde_json::json;

fn run(db: &DbInstance, script: &str) -> Result<serde_json::Value, String> {
    db.run_script(script, Default::default(), ScriptMutability::Mutable)
        .map(|r| json!(r.rows))
        .map_err(|e| format!("{e:?}"))
}

fn mem() -> DbInstance {
    let db = DbInstance::new("mem", "", "").unwrap();
    run(&db, ":create ev {k: String, at: Validity => v: String}").unwrap();
    db
}

fn strs(rows: &serde_json::Value) -> Vec<Vec<String>> {
    rows.as_array()
        .unwrap()
        .iter()
        .map(|r| {
            r.as_array()
                .unwrap()
                .iter()
                .map(|c| c["Str"].as_str().expect("a string column").to_string())
                .collect()
        })
        .collect()
}

fn only_float(rows: &serde_json::Value) -> f64 {
    rows[0][0]["Num"]["Float"].as_f64().expect("a float")
}

fn vld_stamps(rows: &serde_json::Value) -> Vec<i64> {
    rows.as_array()
        .unwrap()
        .iter()
        .map(|r| {
            r.as_array()
                .unwrap()
                .iter()
                .find_map(|c| c["Validity"]["timestamp"].as_i64())
                .expect("a validity column")
        })
        .collect()
}

const MICROS_1969_07_20: i64 = -14_256_000_000_000;
const MICROS_1971_01_01: i64 = 31_536_000_000_000;
const MICROS_1970_06_01: i64 = 13_046_400_000_000;

#[test]
fn parse_timestamp_accepts_pre_epoch() {
    let db = mem();
    let rows = run(&db, "?[x] := x = parse_timestamp('1969-07-20T00:00:00Z')").unwrap();
    assert_eq!(only_float(&rows), MICROS_1969_07_20 as f64 / 1e6);

    let rows = run(&db, "?[x] := x = parse_timestamp('2024-06-01T00:00:00Z')").unwrap();
    assert_eq!(only_float(&rows), 1_717_200_000.0);
}

#[test]
fn write_path_accepts_pre_epoch_and_db_survives() {
    let db = mem();
    run(
        &db,
        "?[k, at, v] <- [['a', '1969-07-20T00:00:00Z', 'MOON']] :put ev {k, at => v}",
    )
    .expect("a pre-epoch validity string must be writable");

    let rows = run(&db, "?[k] := *ev{k}").expect("the Db must survive the write");
    assert_eq!(rows.as_array().unwrap().len(), 1);

    let rows = run(&db, "?[k, at, v] := *ev{k, at, v @ 'END'}").unwrap();
    assert_eq!(vld_stamps(&rows), vec![MICROS_1969_07_20]);
}

#[test]
fn read_path_time_travels_across_the_epoch() {
    let db = mem();
    run(
        &db,
        &format!(
            "?[k, at, v] <- [['a', [{MICROS_1969_07_20}, true], 'MOON'], \
             ['a', [{MICROS_1971_01_01}, true], 'SEVENTYONE']] :put ev {{k, at => v}}"
        ),
    )
    .unwrap();

    let rows = run(&db, "?[k, at, v] := *ev{k, at, v}").unwrap();
    assert_eq!(
        vld_stamps(&rows),
        vec![MICROS_1971_01_01, MICROS_1969_07_20]
    );

    let rows = run(&db, "?[k, v] := *ev{k, v @ '1970-06-01T00:00:00Z'}").unwrap();
    assert_eq!(strs(&rows), vec![vec!["a", "MOON"]]);

    let rows = run(&db, "?[k, v] := *ev{k, v @ '1972-01-01T00:00:00Z'}").unwrap();
    assert_eq!(strs(&rows), vec![vec!["a", "SEVENTYONE"]]);

    let rows = run(&db, "?[k, v] := *ev{k, v @ '1969-01-01T00:00:00Z'}").unwrap();
    assert!(strs(&rows).is_empty());

    let by_str = run(&db, "?[k, v] := *ev{k, v @ '1970-06-01T00:00:00Z'}").unwrap();
    let by_int = run(
        &db,
        &format!("?[k, v] := *ev{{k, v @ {MICROS_1970_06_01}}}"),
    )
    .unwrap();
    assert_eq!(by_str, by_int);

    let bare = run(&db, "?[k, v] := *ev{k, v @ '1970-06-01'}").unwrap();
    assert_eq!(bare, by_int);
}

#[test]
fn sentinels_and_retraction_still_work() {
    let db = mem();
    run(
        &db,
        "?[k, at, v] <- [['a', '1969-07-20T00:00:00Z', 'MOON']] :put ev {k, at => v}",
    )
    .unwrap();

    assert_eq!(
        strs(&run(&db, "?[k, v] := *ev{k, v @ 'NOW'}").unwrap()),
        vec![vec!["a", "MOON"]]
    );
    assert_eq!(
        strs(&run(&db, "?[k, v] := *ev{k, v @ 'END'}").unwrap()),
        vec![vec!["a", "MOON"]]
    );

    run(
        &db,
        "?[k, at, v] <- [['a', '~1969-12-01T00:00:00Z', 'MOON']] :put ev {k, at => v}",
    )
    .unwrap();
    assert!(strs(&run(&db, "?[k, v] := *ev{k, v @ 'NOW'}").unwrap()).is_empty());
    assert_eq!(
        strs(&run(&db, "?[k, v] := *ev{k, v @ '1969-09-01T00:00:00Z'}").unwrap()),
        vec![vec!["a", "MOON"]]
    );

    assert!(run(
        &db,
        "?[k, at, v] <- [['z', [9223372036854775807, true], 'x']] :put ev {k, at => v}"
    )
    .is_err());
    assert!(run(
        &db,
        "?[k, at, v] <- [['z', [-9223372036854775808, true], 'x']] :put ev {k, at => v}"
    )
    .is_err());
}

#[test]
fn write_path_pre_epoch_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("preepoch.db");
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
    run(&db, ":create ev {k: String, at: Validity => v: String}").unwrap();
    run(
        &db,
        "?[k, at, v] <- [['a', '1969-07-20T00:00:00Z', 'MOON']] :put ev {k, at => v}",
    )
    .unwrap();
    let rows = run(&db, "?[k] := *ev{k}").expect("sqlite: the Db must survive");
    assert_eq!(rows.as_array().unwrap().len(), 1);
}
