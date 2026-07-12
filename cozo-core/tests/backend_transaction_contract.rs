/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Transaction-contract guards for the two secondary backends fixed in 0.12.1.
//!
//! **`newrocksdb`** (`--features storage-new-rocksdb`) ran an
//! `OptimisticTransactionDB` but **discarded `for_update`** on `get`/`exists` and
//! the `write` flag on `transact`. An optimistic transaction only validates keys
//! it registered for conflict checking — via `get_for_update`, or, with a
//! snapshot set, every key it writes. It did neither, so the engine's
//! read-modify-write paths armed nothing: two transactions could read a key, both
//! write it, and both commit. **A silently lost update, on a publicly selectable
//! backend.** Fixed on both axes; see the discrimination note on the test, which
//! records that either one alone closes *this* scenario.
//!
//! **`sled`** (`--features storage-sled`) wrote `PUT_MARKER` in `del()`
//! (upstream #306), so a delete inside a transaction was recorded in the changes
//! overlay as a put-with-empty-value: the key survived the commit. Deletes
//! simply did not take.
//!
//! Run with:
//!   cargo test -p mnestic --features storage-new-rocksdb --test backend_transaction_contract
//!   cargo test -p mnestic --features storage-sled          --test backend_transaction_contract

#![cfg(any(feature = "storage-new-rocksdb", feature = "storage-sled"))]

#[cfg(feature = "storage-new-rocksdb")]
mod newrocks {
    use cozo::{DbInstance, ScriptMutability};
    use std::collections::BTreeMap;
    use std::sync::{Arc, Barrier};

    /// **The lost update.** Two transactions each read `v`, then write `v + 1`.
    /// With conflict detection armed, exactly one may commit; the other must be
    /// refused. Without it, both commit and one increment vanishes.
    ///
    /// The barrier forces the interleaving that matters: both reads happen before
    /// either write, which is the definition of a read-modify-write race.
    ///
    /// **Discrimination, measured — and it is not what you would guess.** Against
    /// the fully unfixed backend this reliably goes red: *"2 transactions reported
    /// a successful commit but the counter only reached 1."* But reverting
    /// **either** fix *alone* leaves it green, because each independently arms
    /// validation for this scenario: `get_for_update` registers the key in the
    /// conflict set, and the snapshot makes the commit validate written keys
    /// against the state the transaction read. So this test discriminates the
    /// *bug*, not either individual line — do not read a green here as evidence
    /// that one of the two changes is load-bearing on its own. Both are kept: the
    /// snapshot is what the pessimistic backend does, and `get_for_update` is the
    /// only one of the two that covers a read-key-A / write-key-B hazard, which
    /// this test does not exercise because no current engine path does.
    #[test]
    fn concurrent_read_modify_write_cannot_lose_an_update() {
        let dir = tempfile::tempdir().unwrap();
        let db = DbInstance::new(
            "newrocksdb",
            dir.path().join("d").to_str().unwrap(),
            Default::default(),
        )
        .unwrap();

        db.run_script(
            ":create counter {k: String => v: Int}",
            BTreeMap::new(),
            ScriptMutability::Mutable,
        )
        .unwrap();
        db.run_script(
            "?[k, v] <- [['c', 0]] :put counter {k => v}",
            BTreeMap::new(),
            ScriptMutability::Mutable,
        )
        .unwrap();

        // Both transactions read the counter, then both write read+1. `:put` over
        // an existing key takes the engine's read-modify-write path, which reads
        // `for_update`.
        let both_read = Arc::new(Barrier::new(2));
        let outcomes: Vec<bool> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let db = &db;
                    let barrier = Arc::clone(&both_read);
                    s.spawn(move || {
                        let tx = db.multi_transaction(true);
                        let cur = tx
                            .run_script("?[v] := *counter{k: 'c', v}", BTreeMap::new())
                            .unwrap()
                            .rows[0][0]
                            .get_int()
                            .unwrap();

                        // Neither writes until both have read.
                        barrier.wait();

                        if tx
                            .run_script(
                                &format!("?[k, v] <- [['c', {}]] :put counter {{k => v}}", cur + 1),
                                BTreeMap::new(),
                            )
                            .is_err()
                        {
                            return false;
                        }
                        tx.commit().is_ok()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let committed = outcomes.iter().filter(|ok| **ok).count();
        let final_v = db
            .run_script(
                "?[v] := *counter{k: 'c', v}",
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )
            .unwrap()
            .rows[0][0]
            .get_int()
            .unwrap();

        assert_eq!(
            committed as i64, final_v,
            "LOST UPDATE: {committed} transaction(s) reported a successful commit but the \
             counter only reached {final_v} — an acknowledged write was silently discarded. \
             Conflict validation is not armed."
        );
        assert!(
            committed >= 1,
            "at least one of the two transactions must be able to make progress"
        );
    }

    /// A transaction with no contention still commits — the snapshot and the
    /// `get_for_update` registration must not make ordinary writes fail.
    #[test]
    fn uncontended_read_modify_write_still_commits() {
        let dir = tempfile::tempdir().unwrap();
        let db = DbInstance::new(
            "newrocksdb",
            dir.path().join("d").to_str().unwrap(),
            Default::default(),
        )
        .unwrap();

        db.run_script(
            ":create counter {k: String => v: Int}",
            BTreeMap::new(),
            ScriptMutability::Mutable,
        )
        .unwrap();

        for i in 1..=3 {
            let tx = db.multi_transaction(true);
            tx.run_script(
                &format!("?[k, v] <- [['c', {i}]] :put counter {{k => v}}"),
                BTreeMap::new(),
            )
            .unwrap();
            tx.commit().unwrap();
        }

        let v = db
            .run_script(
                "?[v] := *counter{k: 'c', v}",
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )
            .unwrap()
            .rows[0][0]
            .get_int()
            .unwrap();
        assert_eq!(v, 3, "sequential writes must all land");
    }
}

#[cfg(feature = "storage-sled")]
mod sled {
    use cozo::{DbInstance, ScriptMutability};
    use std::collections::BTreeMap;

    /// **Upstream #306.** A `:rm` must actually remove the row.
    ///
    /// Discrimination: restore `PUT_MARKER` in `sled.rs`'s `del()` and this goes
    /// red — the row survives the delete.
    #[test]
    fn delete_actually_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let db = DbInstance::new(
            "sled",
            dir.path().join("d").to_str().unwrap(),
            Default::default(),
        )
        .unwrap();

        let run = |s: &str| {
            db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable)
                .unwrap_or_else(|e| panic!("script failed: {e:?}\n{s}"))
        };

        run(":create kv {k: Int => v: Int}");
        run("?[k, v] <- [[1, 10], [2, 20]] :put kv {k => v}");
        assert_eq!(run("?[k, v] := *kv{k, v}").rows.len(), 2);

        run("?[k] <- [[1]] :rm kv {k}");

        let remaining = run("?[k, v] := *kv{k, v} :order k");
        assert_eq!(
            remaining.rows.len(),
            1,
            "DELETE DID NOT TAKE: the removed key survived the commit — `del()` \
             recorded a put-with-empty-value instead of a deletion (upstream #306)"
        );
        assert_eq!(
            remaining.rows[0][0].get_int().unwrap(),
            2,
            "it deleted the wrong row"
        );
    }

    /// The deleted key must also stop *existing*, not merely stop being scanned —
    /// the marker the bug wrote was read back by `exists`, so an existence check
    /// kept answering `true` for a deleted key.
    #[test]
    fn deleted_key_no_longer_exists() {
        let dir = tempfile::tempdir().unwrap();
        let db = DbInstance::new(
            "sled",
            dir.path().join("d").to_str().unwrap(),
            Default::default(),
        )
        .unwrap();

        let run = |s: &str| {
            db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable)
                .unwrap_or_else(|e| panic!("script failed: {e:?}\n{s}"))
        };

        run(":create kv {k: Int => v: Int}");
        run("?[k, v] <- [[1, 10]] :put kv {k => v}");
        run("?[k] <- [[1]] :rm kv {k}");

        // `:insert` asserts the key does NOT already exist. After a real delete it
        // must succeed; against a ghost key it fails.
        db.run_script(
            "?[k, v] <- [[1, 99]] :insert kv {k => v}",
            BTreeMap::new(),
            ScriptMutability::Mutable,
        )
        .expect("re-inserting a deleted key must succeed — it still 'exists' otherwise");

        let rows = run("?[k, v] := *kv{k, v}");
        assert_eq!(rows.rows[0][1].get_int().unwrap(), 99);
    }
}
