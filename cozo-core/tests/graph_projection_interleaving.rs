/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The graph projection's freshness protocol, at the race windows
//! (`docs/specs/graph-projection.md` §5, "protocol interleaving tests").
//!
//! **Why rocksdb.** `mem` and `sqlite` pin a read transaction by holding a
//! `ShardedLock` read guard for its whole life, so a long-lived reader and a
//! concurrent writer cannot coexist — the schedules below are unreachable
//! there. RocksDB pins by taking a snapshot and takes no such guard, so a
//! reader really can sit on an old snapshot while a writer commits.
//!
//! **Why fences.** Two windows are too narrow to hit by racing threads. The
//! commit fence parks a writer between its durable storage commit and its
//! token bump, where `inflight` alone denies a stale hit. The build fence
//! parks a builder between its build and the produce rule, which is the only
//! window in which a legal insert becomes a refused one — a transaction's
//! watermark never moves, so Door 2 and `produce` agree on every other
//! schedule.
//!
//! Run with:
//! `cargo test -p mnestic --features storage-rocksdb,test-hooks --test graph_projection_interleaving`

#![cfg(all(
    feature = "storage-rocksdb",
    feature = "test-hooks",
    feature = "graph-algo"
))]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use cozo::{
    new_cozo_rocksdb, DataValue, Db, DbInstance, NamedRows, RocksDbStorage, ScriptMutability,
};

type RocksDb = Db<RocksDbStorage>;

fn run(db: &RocksDb, script: &str) -> NamedRows {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed: {script}\n{e:?}"))
}

/// `1→2, 3→4`: two components, so a third edge joining them is visible in the
/// component count without depending on group-id labelling.
fn fixture(dir: &std::path::Path) -> RocksDb {
    let db = new_cozo_rocksdb(dir).unwrap();
    run(&db, ":create knows {a: Int, b: Int}");
    run(&db, "?[a, b] <- [[1,2],[3,4]] :put knows {a, b}");
    run(&db, "::graph create g {edges: knows}");
    db
}

fn builds(db: &RocksDb) -> u64 {
    db.graph_projection_builds_for_tests()
}

/// Number of resident variants, read off `::graph list`.
fn resident(db: &RocksDb) -> usize {
    run(db, "::graph list")
        .rows
        .iter()
        .filter(|r| r[3] != DataValue::Null)
        .count()
}

const CC: &str = "?[n, c] <~ ConnectedComponents(graph: 'g')";

/// A one-shot gate. `arrived` fires when a thread reaches it; the thread then
/// waits for `release`.
struct Gate {
    arrived: mpsc::Receiver<()>,
    release: mpsc::Sender<()>,
}

impl Gate {
    fn wait_for_arrival(&self) {
        self.arrived.recv().expect("nobody reached the fence");
    }
    fn release(&self) {
        let _ = self.release.send(());
    }
}

/// Park the **first** thread to reach the given fence, and let every later one
/// through untouched.
///
/// One-shot on purpose. A second arrival is exactly what a broken single-flight
/// looks like, and if the fence parked it too it would queue on a receiver
/// nobody will ever send to — the mutation would deadlock the suite instead of
/// turning its `build_count` assertion red. Neither `send` nor `recv` unwraps,
/// so a panicking test thread cannot strand a database mid-commit either.
fn one_shot_fence() -> (Arc<dyn Fn() + Send + Sync>, Gate) {
    let (arrived_tx, arrived) = mpsc::channel::<()>();
    let (release, release_rx) = mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let first = AtomicBool::new(true);
    let fence = Arc::new(move || {
        if !first.swap(false, Ordering::SeqCst) {
            return;
        }
        let _ = arrived_tx.send(());
        let _ = release_rx.lock().unwrap().recv();
    });
    (fence, Gate { arrived, release })
}

/// The set of vertices the algorithm saw.
fn vertices(res: &NamedRows) -> BTreeSet<i64> {
    res.rows.iter().map(|r| r[0].get_int().unwrap()).collect()
}

// ---------------------------------------------------------------------- (a)

/// A writer parked after its storage commit and before its token bump must deny
/// a concurrent lookup a hit. In that window the entry is still resident and
/// still carries the token it was tagged with, so `inflight` is the *only* thing
/// standing between the lookup and a stale answer.
///
/// Once the writer is released it bumps the token and frees the entry, so the
/// next lookup rebuilds — forced by the eager free, with the token comparison
/// behind it as defence in depth (pinned separately, in-crate, by
/// `a_token_bump_alone_denies_the_hit_even_if_the_entry_survives`). A third
/// lookup, against the refilled cache, hits.
#[test]
fn a_commit_in_flight_denies_the_hit_via_inflight() {
    let dir = tempfile::tempdir().unwrap();
    let db = fixture(dir.path());

    run(&db, CC); // warm
    assert_eq!(resident(&db), 1);
    let warm_builds = builds(&db);

    let (fence, gate) = one_shot_fence();
    db.set_commit_fence_for_tests(Some(fence));

    let writer = {
        let db = db.clone();
        std::thread::spawn(move || run(&db, "?[a, b] <- [[2,3]] :put knows {a, b}"))
    };
    gate.wait_for_arrival();

    // The entry is still resident and still carries its old token — `inflight`
    // is the only thing standing between this lookup and a stale hit.
    assert_eq!(resident(&db), 1, "the writer has not reached its bump yet");
    let before = builds(&db);
    run(&db, CC);
    assert_eq!(builds(&db), before + 1, "inflight must deny the hit");
    // …and having missed, it published nothing: an unfresh producer cannot insert.
    assert_eq!(builds(&db), warm_builds + 1);

    db.set_commit_fence_for_tests(None);
    gate.release();
    writer.join().unwrap();

    // Post-commit the entry is gone — freed eagerly, because a bumped token
    // makes it permanently unmatchable — so this rebuilds and, being fresh,
    // publishes.
    assert_eq!(resident(&db), 0);
    let before = builds(&db);
    let res = run(&db, CC);
    assert_eq!(builds(&db), before + 1, "a freed entry must be rebuilt");
    assert_eq!(vertices(&res), BTreeSet::from([1, 2, 3, 4]));
    assert_eq!(resident(&db), 1);

    // Third time: a real hit.
    let before = builds(&db);
    run(&db, CC);
    assert_eq!(builds(&db), before, "a fresh lookup must hit");
}

// ---------------------------------------------------------------------- (b)

/// The direction the draft protocol got wrong. A reader pins its snapshot;
/// a writer commits and bumps; a *third* transaction populates a cache entry
/// that is perfectly fresh — for everyone but the reader. The reader must miss
/// (`token > watermark`), build ephemerally from its own snapshot, and see the
/// pre-write graph.
#[test]
fn a_fresh_entry_is_never_served_to_a_transaction_that_pinned_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = fixture(dir.path());
    run(&db, CC); // warm, so the reader below could plausibly hit

    // Pin a read snapshot, and prove it is live by using it once. The long-lived
    // reader runs on its own thread; `Db` is an `Arc` bundle, so the `DbInstance`
    // wrapper shares this database's projection cache.
    let reader = DbInstance::RocksDb(db.clone()).multi_transaction(false);
    let first = reader.run_script(CC, BTreeMap::new()).unwrap();
    assert_eq!(vertices(&first), BTreeSet::from([1, 2, 3, 4]));

    // A writer joins the two components. This bumps the token past the
    // reader's watermark and frees the entry.
    run(&db, "?[a, b] <- [[2,3]] :put knows {a, b}");
    assert_eq!(resident(&db), 0);

    // A fresh transaction repopulates the cache with the *new* graph.
    let fresh = run(&db, CC);
    assert_eq!(vertices(&fresh), BTreeSet::from([1, 2, 3, 4]));
    assert_eq!(resident(&db), 1);
    let published = builds(&db);

    // The reader must not be served it. It rebuilds from its own snapshot…
    let second = reader.run_script(CC, BTreeMap::new()).unwrap();
    assert_eq!(builds(&db), published + 1, "the old reader must rebuild");
    // …and its snapshot has two components, so the joined graph is not there.
    assert_eq!(components(&second), 2, "the reader was served a newer graph");
    assert_eq!(components(&fresh), 1);

    // (d) Snapshot consistency: the same query, twice, across the commit.
    assert_eq!(second.rows, first.rows);

    // …and the reader's ephemeral build published nothing over the fresh entry.
    assert_eq!(resident(&db), 1);
    reader.abort().unwrap();
}

/// Distinct component ids in a `ConnectedComponents` result.
fn components(res: &NamedRows) -> usize {
    res.rows
        .iter()
        .map(|r| r[1].get_int().unwrap())
        .collect::<BTreeSet<_>>()
        .len()
}

// --------------------------------------------------------------- tombstones

/// The Phase 3/4 review's high finding. A destroyed relation's freshness state
/// must be kept as a permanent tombstone, because destruction does not make
/// the id unresolvable: a reader whose snapshot predates the destroy still
/// resolves it through its own catalog view. If the state were purged, absence
/// would read as `{token: 0}` — fresh at every watermark — and two pre-destroy
/// readers could republish and then *consume* a projection with no token left
/// to arbitrate content versions:
///
/// reader T pins → writer adds rows → reader T3 pins (sees the new rows) →
/// `:replace` retires the id → T builds its old content and publishes at
/// token 0 → T3 takes the hit and is served content its own scan contradicts.
///
/// With the tombstone, T's produce is refused (token exceeds its watermark)
/// and T3 rebuilds from its own snapshot.
#[test]
fn a_snapshot_predating_a_replace_cannot_poison_the_cache() {
    let dir = tempfile::tempdir().unwrap();
    let db = fixture(dir.path());

    // T pins first, seeing edges {1→2, 3→4}.
    let t = DbInstance::RocksDb(db.clone()).multi_transaction(false);
    assert_eq!(
        vertices(&t.run_script(CC, BTreeMap::new()).unwrap()),
        BTreeSet::from([1, 2, 3, 4])
    );

    // A writer adds 5→6; T3 pins after it, sees all six vertices, and — being
    // fresh — legitimately republishes the six-vertex variant.
    run(&db, "?[a, b] <- [[5,6]] :put knows {a, b}");
    let t3 = DbInstance::RocksDb(db.clone()).multi_transaction(false);
    assert_eq!(
        vertices(&t3.run_script(CC, BTreeMap::new()).unwrap()),
        BTreeSet::from([1, 2, 3, 4, 5, 6])
    );
    assert_eq!(resident(&db), 1);

    // The relation is `:replace`d: the old id is retired, and the entry bound
    // to it is freed.
    run(&db, "?[a, b] <- [[7,8]] :replace knows {a: Int, b: Int}");
    assert_eq!(resident(&db), 0);

    // T queries. It resolves the OLD id through its pinned snapshot, builds
    // its own four-vertex content — and must NOT be allowed to publish it:
    // only the tombstone's token stands between this build and the cache.
    let t_res = t.run_script(CC, BTreeMap::new()).unwrap();
    assert_eq!(vertices(&t_res), BTreeSet::from([1, 2, 3, 4]));
    assert_eq!(
        resident(&db),
        0,
        "a pre-destroy snapshot published into the cache: the tombstone is gone"
    );

    // T3 queries. Same old id, different snapshot content. It must rebuild
    // from its own snapshot, never take an entry of T's.
    let before = builds(&db);
    let t3_res = t3.run_script(CC, BTreeMap::new()).unwrap();
    assert_eq!(builds(&db), before + 1, "T3 must rebuild, not hit");
    assert_eq!(
        vertices(&t3_res),
        BTreeSet::from([1, 2, 3, 4, 5, 6]),
        "T3 was served another snapshot's graph — the stale hit the tombstone exists to deny"
    );
    assert_eq!(resident(&db), 0, "neither pre-destroy snapshot may publish");

    t.abort().unwrap();
    t3.abort().unwrap();
}

// ---------------------------------------------------------------------- (c)

/// A commit landing while the single-flight winner is building: the winner's
/// snapshot is still self-consistent, so its result is served — but the
/// produce rule refuses it, because its sources are no longer what it claims.
/// The cache stays cold until a transaction that opened after the commit
/// fills it.
#[test]
fn a_commit_during_a_build_makes_the_produce_rule_refuse_the_result() {
    let dir = tempfile::tempdir().unwrap();
    let db = fixture(dir.path());
    assert_eq!(resident(&db), 0, "cold");

    let (fence, gate) = one_shot_fence();
    db.set_graph_build_fence_for_tests(Some(fence));

    let builder = {
        let db = db.clone();
        std::thread::spawn(move || vertices(&run(&db, CC)))
    };
    gate.wait_for_arrival();

    // The build is done; `produce` has not run. Land a commit.
    db.set_graph_build_fence_for_tests(None);
    run(&db, "?[a, b] <- [[5,6]] :put knows {a, b}");

    gate.release();
    let seen = builder.join().unwrap();

    // The builder's own snapshot predates the write, and it was served that.
    assert_eq!(seen, BTreeSet::from([1, 2, 3, 4]));
    // …and nothing was cached: an entry tagged with the pre-commit tokens would
    // be a stale hit for every transaction that follows.
    assert_eq!(resident(&db), 0, "produce must refuse a build it cannot vouch for");

    // The next query, opened after the commit, populates the cache normally.
    let res = run(&db, CC);
    assert_eq!(vertices(&res), BTreeSet::from([1, 2, 3, 4, 5, 6]));
    assert_eq!(resident(&db), 1);
}

// ------------------------------------------------------- single-flight, live

/// Eight concurrent cold readers coalesce into exactly one build.
///
/// The fence parks the first builder, so the other seven are still queued —
/// behind the single-flight slot — when the winner has finished building and
/// has not yet published. Give every caller its own slot instead, and all eight
/// build. The assertion is on `build_count`, not on the rows: eight rebuilds
/// return exactly the same answer, only slower.
#[test]
fn concurrent_cold_readers_coalesce_into_one_build() {
    let dir = tempfile::tempdir().unwrap();
    let db = fixture(dir.path());

    let (fence, gate) = one_shot_fence();
    db.set_graph_build_fence_for_tests(Some(fence));

    let before = builds(&db);
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let db = db.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                vertices(&run(&db, CC))
            })
        })
        .collect();

    gate.wait_for_arrival();
    // Let the seven losers reach the slot they will block on. Not load-bearing
    // for correctness — a loser that arrives after the winner publishes simply
    // takes the ordinary cache hit — but without it the test would not be
    // exercising single-flight at all.
    std::thread::sleep(std::time::Duration::from_millis(200));
    db.set_graph_build_fence_for_tests(None);
    gate.release();

    for t in threads {
        assert_eq!(t.join().unwrap(), BTreeSet::from([1, 2, 3, 4]));
    }
    assert_eq!(
        builds(&db),
        before + 1,
        "eight cold readers must coalesce into one build"
    );
    assert_eq!(resident(&db), 1);
}

// ------------------------------------------------------------ a writer alone

/// The whole protocol, without a single fence: a writer and a reader hammering
/// the same projection must never disagree with a scan of the sources. This is
/// the row that would catch a freshness bug the fenced tests schedule around.
#[test]
fn a_reader_never_disagrees_with_the_relation_under_write_churn() {
    let dir = tempfile::tempdir().unwrap();
    let db = fixture(dir.path());
    run(&db, "::graph drop g");
    run(&db, "?[a, b] <- [[1,2]] :put knows {a, b}");
    run(&db, "::graph create g {edges: knows}");

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let db = db.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            for i in 10..60i64 {
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                run(&db, &format!("?[a, b] <- [[{i}, {}]] :put knows {{a, b}}", i + 1));
            }
        })
    };

    for _ in 0..200 {
        // Both reads run in the same transaction, against one snapshot: the
        // cached CSR and a raw scan of the source must name the same vertices.
        let res = db
            .run_script(
                r#"
                cc[n, c] <~ ConnectedComponents(graph: 'g')
                scanned[n] := *knows[n, _]
                scanned[n] := *knows[_, n]
                ?[n] := cc[n, _], not scanned[n]
                ?[n] := scanned[n], not cc[n, _]
                "#,
                BTreeMap::new(),
                ScriptMutability::Mutable,
            )
            .unwrap();
        assert!(
            res.rows.is_empty(),
            "the projection and its source disagreed on {:?}",
            res.rows
        );
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().unwrap();
}
