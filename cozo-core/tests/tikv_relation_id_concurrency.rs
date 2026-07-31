#![cfg(feature = "storage-tikv")]

use cozo::{new_cozo_tikv, NamedRows, ScriptMutability};
use miette::Result;
use serde_json::json;
use std::collections::BTreeMap;
use std::env;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;
use uuid::Uuid;

fn retry_pessimistic_create(mut create: impl FnMut() -> Result<NamedRows>) {
    for attempt in 1..=10 {
        match create() {
            Ok(_) => return,
            Err(error) if attempt < 10 && format!("{error:?}").contains("PessimisticRetry") => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("relation creation failed on attempt {attempt}: {error:?}"),
        }
    }
    unreachable!("the retry loop returns or panics on every final attempt");
}

/// Requires a live, disposable TiKV cluster. CI starts one and invokes this
/// ignored test explicitly; ordinary local test runs remain self-contained.
#[test]
#[ignore = "requires MNESTIC_TIKV_PD_ENDPOINT pointing at a live TiKV cluster"]
fn two_clients_allocate_distinct_relation_ids_concurrently() {
    let pd_endpoint = env::var("MNESTIC_TIKV_PD_ENDPOINT")
        .expect("MNESTIC_TIKV_PD_ENDPOINT must name a live PD endpoint");

    // Construct both clients before either relation exists. This preserves the
    // stale in-process counter state that caused two processes to reuse an ID.
    // The persisted counter's get-for-update serializes their allocations;
    // TiKV may ask the losing pessimistic transaction to retry from a fresh
    // start timestamp, which the helper above does explicitly.
    let left_db = Arc::new(new_cozo_tikv(vec![pd_endpoint.clone()], false).unwrap());
    let right_db = Arc::new(new_cozo_tikv(vec![pd_endpoint], false).unwrap());

    let suffix = Uuid::new_v4().simple().to_string();
    let seed_name = format!("tikv_seed_{suffix}");
    let left_name = format!("tikv_left_{suffix}");
    let right_name = format!("tikv_right_{suffix}");

    // Advance only left_db's in-process counter. Under the vulnerable code,
    // right_db remains stale and reuses this relation's physical ID.
    left_db
        .run_script(
            &format!(":create {seed_name} {{k: Int => v: String}}"),
            BTreeMap::new(),
            ScriptMutability::Mutable,
        )
        .unwrap();

    let start = Arc::new(Barrier::new(2));

    thread::scope(|scope| {
        let left_db = Arc::clone(&left_db);
        let left_start = Arc::clone(&start);
        let left_script = format!(":create {left_name} {{k: Int => v: String}}");
        let left = scope.spawn(move || {
            left_start.wait();
            retry_pessimistic_create(|| {
                left_db.run_script(&left_script, BTreeMap::new(), ScriptMutability::Mutable)
            });
        });

        let right_db = Arc::clone(&right_db);
        let right_start = Arc::clone(&start);
        let right_script = format!(":create {right_name} {{k: Int => v: String}}");
        let right = scope.spawn(move || {
            right_start.wait();
            retry_pessimistic_create(|| {
                right_db.run_script(&right_script, BTreeMap::new(), ScriptMutability::Mutable)
            });
        });

        left.join().unwrap();
        right.join().unwrap();
    });

    // Equal relation IDs alias the physical key prefix. Using the same logical
    // key in all three relations therefore turns the allocator bug into an
    // observable cross-relation overwrite, while distinct IDs preserve every
    // value. The seed makes this fail under the old code whichever concurrent
    // transaction wins: stale right_db aliases the seed or left's relation.
    left_db
        .run_script(
            &format!(r#"?[k, v] <- [[1, "seed"]] :put {seed_name} {{k => v}}"#),
            BTreeMap::new(),
            ScriptMutability::Mutable,
        )
        .unwrap();
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

    let seed_rows = left_db
        .run_script(
            &format!("?[v] := *{seed_name}{{k: 1, v}}"),
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .unwrap()
        .into_json();
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

    assert_eq!(seed_rows["rows"], json!([["seed"]]));
    assert_eq!(left_rows["rows"], json!([["left"]]));
    assert_eq!(right_rows["rows"], json!([["right"]]));
}
