/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The `test-hooks` commit fence, exercised from outside the crate — which is
//! the only place it is for. The graph-projection freshness protocol
//! (`src/runtime/graph_projection.rs`) has one race window that no
//! single-threaded test can reach: after a writer's storage commit has
//! returned, but before its freshness token has moved. In that window `inflight`
//! is the sole thing denying a concurrent reader a stale cache hit. Phase 4's
//! interleaving suite parks a writer there; this file proves the parking works,
//! and that the fence fires when — and only when — the protocol says it does.
//!
//! Run with: `cargo test -p mnestic --features test-hooks --test commit_fence`

#![cfg(feature = "test-hooks")]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};

use cozo::{new_cozo_mem, ScriptMutability};

fn run(db: &cozo::Db<cozo::MemStorage>, script: &str) -> cozo::NamedRows {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap()
}

/// The fence runs once per *writing* commit, and never for a read-only one:
/// a transaction with an empty dirty set takes the fast path and does no
/// protocol bookkeeping at all.
#[test]
fn the_fence_fires_for_writing_commits_only() {
    let db = new_cozo_mem().unwrap();
    run(&db, ":create r {a: Int => b: Int}");

    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    db.set_commit_fence_for_tests(Some(Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    })));

    run(&db, "?[a] := *r[a, _]");
    assert_eq!(hits.load(Ordering::SeqCst), 0, "a read commits nothing");

    run(&db, "?[a, b] <- [[1, 1]] :put r {a => b}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    run(&db, "?[a] <- [[1]] :rm r {a}");
    assert_eq!(hits.load(Ordering::SeqCst), 2);

    db.set_commit_fence_for_tests(None);
    run(&db, "?[a, b] <- [[2, 2]] :put r {a => b}");
    assert_eq!(hits.load(Ordering::SeqCst), 2, "the fence was removed");
}

/// The fence really does park the committing thread: the writer cannot return
/// until the fence is released. This is what lets Phase 4 hold a commit open
/// while another thread probes the cache.
#[test]
fn the_fence_parks_the_committing_thread() {
    let db = Arc::new(new_cozo_mem().unwrap());
    run(&db, ":create r {a: Int => b: Int}");

    let (parked_tx, parked_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::sync_channel::<()>(0);
    let release_rx = std::sync::Mutex::new(release_rx);
    db.set_commit_fence_for_tests(Some(Arc::new(move || {
        parked_tx.send(()).unwrap();
        let _ = release_rx.lock().unwrap().recv();
    })));

    let writer = {
        let db = db.clone();
        std::thread::spawn(move || run(&db, "?[a, b] <- [[1, 1]] :put r {a => b}"))
    };

    parked_rx.recv().unwrap();
    assert!(
        !writer.is_finished(),
        "the writer is held inside its commit"
    );

    drop(release_tx);
    writer.join().unwrap();
    db.set_commit_fence_for_tests(None);

    assert_eq!(run(&db, "?[a] := *r[a, _]").rows.len(), 1);
}
