#![cfg(feature = "storage-tikv")]

use cozo::{new_cozo_tikv, ScriptMutability};
use serde_json::json;
use std::collections::BTreeMap;
use std::env;
use std::sync::{Arc, Barrier};
use std::thread;
use uuid::Uuid;

/// Requires a live, disposable TiKV cluster. CI starts one and invokes this
/// ignored test explicitly; ordinary local test runs remain self-contained.
#[test]
#[ignore = "requires MNESTIC_TIKV_PD_ENDPOINT pointing at a live TiKV cluster"]
fn two_clients_allocate_distinct_relation_ids_concurrently() {
    let pd_endpoint = env::var("MNESTIC_TIKV_PD_ENDPOINT")
        .expect("MNESTIC_TIKV_PD_ENDPOINT must name a live PD endpoint");

    // Construct both clients before either relation exists. This preserves the
    // stale in-process counter state that caused two processes to reuse an ID.
    // Pessimistic transactions make both creates succeed while the persisted
    // counter's get-for-update serializes their allocations.
    let left_db = Arc::new(new_cozo_tikv(vec![pd_endpoint.clone()], false).unwrap());
    let right_db = Arc::new(new_cozo_tikv(vec![pd_endpoint], false).unwrap());

    let suffix = Uuid::new_v4().simple().to_string();
    let left_name = format!("tikv_left_{suffix}");
    let right_name = format!("tikv_right_{suffix}");
    let start = Arc::new(Barrier::new(2));

    thread::scope(|scope| {
        let left_db = Arc::clone(&left_db);
        let left_start = Arc::clone(&start);
        let left_script = format!(":create {left_name} {{k: Int => v: String}}");
        let left = scope.spawn(move || {
            left_start.wait();
            left_db.run_script(&left_script, BTreeMap::new(), ScriptMutability::Mutable)
        });

        let right_db = Arc::clone(&right_db);
        let right_start = Arc::clone(&start);
        let right_script = format!(":create {right_name} {{k: Int => v: String}}");
        let right = scope.spawn(move || {
            right_start.wait();
            right_db.run_script(&right_script, BTreeMap::new(), ScriptMutability::Mutable)
        });

        left.join().unwrap().unwrap();
        right.join().unwrap().unwrap();
    });

    // Equal relation IDs alias the physical key prefix. Using the same logical
    // key in each relation therefore turns the allocator bug into observable
    // cross-relation overwrite, while distinct IDs preserve both values.
    left_db
        .run_script(
            &format!(r#"?[k, v] <- [[1, "left"]] :put {left_name} {{k => v}}"#),
            BTreeMap::new(),
            ScriptMutability::Mutable,
        )
        .unwrap();
    right_db
        .run_script(
            &format!(r#"?[k, v] <- [[1, "right"]] :put {right_name} {{k => v}}"#),
            BTreeMap::new(),
            ScriptMutability::Mutable,
        )
        .unwrap();

    let left_rows = left_db
        .run_script(
            &format!("?[v] := *{left_name}{{k: 1, v}}"),
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .unwrap()
        .into_json();
    let right_rows = right_db
        .run_script(
            &format!("?[v] := *{right_name}{{k: 1, v}}"),
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .unwrap()
        .into_json();

    assert_eq!(left_rows["rows"], json!([["left"]]));
    assert_eq!(right_rows["rows"], json!([["right"]]));
}
