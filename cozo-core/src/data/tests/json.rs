/*
 * Copyright 2022, The Cozo Project Authors.
 * Copyright 2026, Shan Rizvi (mnestic fork).
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
use crate::data::json::JsonValue;
use crate::data::value::{DataValue, JsonData, RegexWrapper, Validity, ValidityTs, Vector};
use crate::{DbInstance, ScriptMutability};
use ndarray::{array, s};
use serde_json::json;
use std::cmp::Reverse;
use std::collections::BTreeMap;

fn run(db: &DbInstance, script: &str) -> JsonValue {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("{script}: {e:?}"))
        .into_json()["rows"]
        .clone()
}
fn canonical(expr: &str, expected: JsonValue, text: &str) {
    let db = DbInstance::new("mem", "", "").unwrap();
    let script = format!("?[x,m,l,j,o,p,s] := x = {expr}, m = {{'v': x}}, l = [x], j = json(x), o = json_object('v', x), p = set_json_path({{}}, ['v'], x), s = to_string(x)");
    assert_eq!(
        run(&db, &script),
        json!([[expected, {"v":expected}, [expected], expected, {"v":expected}, {"v":expected}, text]]),
        "{expr}"
    );
    run(&db, ":create cells {id: Int => value: Json}");
    run(
        &db,
        &format!("?[id,value] := id = 1, value = {expr} :put cells {{id => value}}"),
    );
    assert_eq!(
        run(&db, "?[value] := *cells{value}"),
        json!([[expected]]),
        "stored {expr}"
    );
}

#[test]
fn canonical_uuid() {
    const UUID: &str = "a6bba931-e2a6-4040-8816-2b33603acf84";
    let expr = format!("to_uuid('{UUID}')");
    canonical(&expr, json!(UUID), UUID);
    let db = DbInstance::new("mem", "", "").unwrap();
    let rows = run(&db, &format!("?[a,b,c,d,e,f] := x = {expr}, a = {{x: 1}}, b = json_object(x,1), c = remove_json_path({{x: 2, 'a': 1}}, [x]), d = length(to_string(x)), e = to_string(x) == to_string(json(x)), f = get({{x: 3}}, [x])"));
    assert_eq!(rows, json!([[{UUID:1},{UUID:1},{"a":1},36,true,3]]));
    let result: JsonValue = serde_json::from_str(&db.run_script_str(
        "?[x,m] := x = rand_uuid_v4(), m = {'x': x}",
        "{}",
        false,
    ))
    .unwrap();
    assert!(result["rows"][0][0].is_string());
    assert_eq!(result["rows"][0][0], result["rows"][0][1]["x"]);
}
#[test]
fn canonical_bytes() {
    canonical("decode_base64('AQID')", json!("AQID"), "AQID");
}
#[test]
fn canonical_floats() {
    for (expr, value, text) in [
        ("1.0", json!(1.0), "1.0"),
        ("-0.0", json!(-0.0), "-0.0"),
        ("0.0/0.0", JsonValue::Null, "null"),
        ("1.0/0.0", json!("INFINITY"), "INFINITY"),
        ("-1.0/0.0", json!("NEGATIVE_INFINITY"), "NEGATIVE_INFINITY"),
    ] {
        canonical(expr, value, text);
    }
    assert_eq!(
        run(
            &DbInstance::new("mem", "", "").unwrap(),
            "?[x] := x = {'n': 0.0/0.0} == {'n': null}"
        ),
        json!([[true]])
    );
}
#[test]
fn canonical_int_exact() {
    canonical(
        "9007199254740993",
        json!(9007199254740993_i64),
        "9007199254740993",
    );
}
#[test]
fn canonical_vec() {
    canonical(
        "vec([0.1])",
        json!([0.10000000149011612]),
        "[0.10000000149011612]",
    );
    canonical("vec([0.1], 'F64')", json!([0.1]), "[0.1]");
    canonical(
        "vec([1.0/0.0, 0.0/0.0])",
        json!([null, null]),
        "[null,null]",
    );
    let f32s = array![1.0_f32, 2.0, 3.0, 4.0].slice_move(s![..;2]);
    let f64s = array![1.0_f64, 2.0, 3.0, 4.0].slice_move(s![..;2]);
    assert!(f32s.as_slice().is_none());
    assert!(f64s.as_slice().is_none());
    for value in [
        DataValue::Vec(Vector::F32(f32s)),
        DataValue::Vec(Vector::F64(f64s)),
    ] {
        assert_eq!(JsonValue::from(&value), json!([1.0, 3.0]));
        assert_eq!(JsonValue::from(value), json!([1.0, 3.0]));
    }
}
#[test]
fn canonical_validity() {
    let db = DbInstance::new("mem", "", "").unwrap();
    run(&db, ":create history {id: Int, v: Validity}");
    run(
        &db,
        "?[id,v] <- [[1,[123,true]],[2,[456,false]]] :put history {id,v}",
    );
    assert_eq!(
        run(
            &db,
            "?[id,v,m,s] := *history{id,v}, m = {'v':v}, s = to_string(v) :order id"
        ),
        json!([[1,[123,true],{"v":[123,true]},"[123,true]"],[2,[456,false],{"v":[456,false]},"[456,false]"]])
    );
    run(&db, ":create cells {id: Int => j: Json}");
    run(&db, "?[id,j] := *history{id,v}, j = v :put cells {id => j}");
    assert_eq!(
        run(&db, "?[id,j] := *cells{id,j} :order id"),
        json!([[1, [123, true]], [2, [456, false]]])
    );
}
#[test]
fn canonical_list_json_passthrough() {
    canonical("[1,'a',null]", json!([1, "a", null]), "[1,\"a\",null]");
    canonical("json({'k':[1,2]})", json!({"k":[1,2]}), "{\"k\":[1,2]}");
    canonical("'a'", json!("a"), "a");
    canonical("json('a')", json!("a"), "a");
    canonical(
        "[to_uuid('a6bba931-e2a6-4040-8816-2b33603acf84'),decode_base64('AQID')]",
        json!(["a6bba931-e2a6-4040-8816-2b33603acf84", "AQID"]),
        "[\"a6bba931-e2a6-4040-8816-2b33603acf84\",\"AQID\"]",
    );
}
#[test]
fn canonical_internal_variants() {
    let values = vec![
        DataValue::Null,
        DataValue::Bot,
        DataValue::Bool(true),
        DataValue::from(42),
        DataValue::from(1.0),
        DataValue::from("text"),
        DataValue::Bytes(vec![1, 2, 3]),
        DataValue::uuid(uuid::Uuid::nil()),
        DataValue::Regex(RegexWrapper(regex::Regex::new("a+").unwrap())),
        DataValue::List(vec![DataValue::from(1)]),
        DataValue::Set([3, 1, 2].into_iter().map(DataValue::from).collect()),
        DataValue::Vec(Vector::F32(array![1.0])),
        DataValue::Json(JsonData(json!({"id":[0,1,2]}))),
        DataValue::Validity(Validity {
            timestamp: ValidityTs(Reverse(123)),
            is_assert: Reverse(true),
        }),
    ];
    for v in values {
        assert_eq!(JsonValue::from(v.clone()), JsonValue::from(&v), "{v:?}");
    }
    assert_eq!(JsonValue::from(DataValue::Bot), JsonValue::Null);
    assert_eq!(
        JsonValue::from(DataValue::Set(
            [3, 1, 2].into_iter().map(DataValue::from).collect()
        )),
        json!([1, 2, 3])
    );
    assert_eq!(
        JsonValue::from(DataValue::Regex(RegexWrapper(
            regex::Regex::new("a+").unwrap()
        ))),
        json!("a+")
    );
    // Outbound string forms deliberately do not restore typed values on input.
    assert_eq!(
        DataValue::from(json!("00000000-0000-0000-0000-000000000000")),
        DataValue::from("00000000-0000-0000-0000-000000000000")
    );
}

#[test]
#[cfg(feature = "storage-sqlite")]
fn canonical_preserves_preexisting_json_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite");
    let old = json!({"u":[166,187,169,49,226,166,64,64,136,22,43,51,96,58,207,132],"inf":null});
    {
        let db = DbInstance::new("sqlite", &path, "").unwrap();
        run(&db, ":create cells {id: Int => j: Json}");
        db.run_script(
            "?[id,j] <- [[1,$j]] :put cells {id => j}",
            BTreeMap::from([("j".into(), DataValue::Json(JsonData(old.clone())))]),
            ScriptMutability::Mutable,
        )
        .unwrap();
    }
    let db = DbInstance::new("sqlite", &path, "").unwrap();
    run(&db,"?[id,j] := id=2, j={'u':to_uuid('a6bba931-e2a6-4040-8816-2b33603acf84'),'inf':1.0/0.0} :put cells {id => j}");
    assert_eq!(
        run(&db, "?[id,j] := *cells{id,j} :order id"),
        json!([[1,old],[2,{"u":"a6bba931-e2a6-4040-8816-2b33603acf84","inf":"INFINITY"}]])
    );
}
