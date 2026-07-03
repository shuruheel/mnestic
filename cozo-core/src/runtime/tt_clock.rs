/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 * Portions Copyright 2022, The Cozo Project Authors (the system-key idiom
 * in `tt_hwm_key` follows `storage_version_key` in runtime/transact.rs).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Transaction-time commit clock (mnestic fork; bitemporality step 2 of
//! `docs/specs/bitemporality.md` §5/§10, decisions §13.10).
//!
//! Allocates transaction-time (tt) values for tt-stamped relations:
//! `tt = max(physical_now_µs, last_tt + 1)` — wall-clock-meaningful, strictly
//! monotonic, collision-free within a µs and across backward clock steps.
//!
//! The in-process `AtomicI64` high-water mark is the authority; a system key
//! (`[Null, "TT_HWM"]` under `RelationId::SYSTEM`, mirroring
//! `STORAGE_VERSION`) persists it **inside the committing transaction** so a
//! crash cannot leave the persisted mark behind a committed tt. Allocation
//! happens at commit time under the per-`Db` `tt_commit_lock` critical
//! section (`SessionTx::commit_tx_with_tt`), so tt order == commit order ==
//! visibility order by construction. Values advanced in memory by
//! transactions that later abort are simply burned — monotonicity holds.
//!
//! No user-visible surface yet: nothing in the write path calls this until
//! step 3 (schema opt-in + stamping) lands.

use std::sync::atomic::{AtomicI64, Ordering};

use crate::data::tuple::TupleT;
use crate::data::value::DataValue;
use crate::runtime::relation::RelationId;

/// Encoded storage key of the persisted tt high-water mark. Same system-key
/// idiom as `storage_version_key` in `runtime/transact.rs`.
pub(crate) fn tt_hwm_key() -> Vec<u8> {
    let tuple = vec![DataValue::Null, DataValue::from("TT_HWM")];
    tuple.encode_as_key(RelationId::SYSTEM)
}

/// Current wall clock in microseconds since the Unix epoch. cfg-split like
/// `current_validity` (data/functions.rs) — bare `SystemTime::now()` panics
/// at runtime on wasm32.
pub(crate) fn wall_clock_micros() -> i64 {
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_micros() as i64
    }
    #[cfg(target_arch = "wasm32")]
    {
        (js_sys::Date::now() * 1000.) as i64
    }
}

/// The wall-clock-floored monotonic commit counter.
#[derive(Debug, Default)]
pub(crate) struct TtClock {
    hwm: AtomicI64,
}

impl TtClock {
    /// Seed at `Db` open: `max(persisted mark, wall clock)`. Seeding with the
    /// wall clock (not zero) keeps tts wall-clock-meaningful on fresh stores
    /// and rides over a backward clock step across a restart when a persisted
    /// mark exists. Monotone (`fetch_max`), so seeding is idempotent and safe
    /// to re-invoke (re-`initialize`, and step 3's restore re-seed) — it can
    /// never move the authority backwards past an allocated tt.
    pub(crate) fn seed(&self, persisted: Option<i64>) {
        let seed = persisted.unwrap_or(0).max(wall_clock_micros());
        self.hwm.fetch_max(seed, Ordering::AcqRel);
    }

    /// Allocate the next tt: `max(now_µs, last + 1)`. Strictly monotonic and
    /// unique across concurrent callers (CAS loop), though in production
    /// allocation only happens under the `tt_commit_lock`.
    pub(crate) fn advance(&self) -> i64 {
        self.advance_with_now(wall_clock_micros())
    }

    /// `advance` with an injected "now" — the testable core; pins backward
    /// clock steps and same-µs behavior without touching the real clock.
    pub(crate) fn advance_with_now(&self, now_us: i64) -> i64 {
        loop {
            let last = self.hwm.load(Ordering::Acquire);
            let candidate = now_us.max(last.saturating_add(1));
            if self
                .hwm
                .compare_exchange_weak(last, candidate, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return candidate;
            }
        }
    }

    /// Current high-water mark (last allocated tt, or the seed).
    pub(crate) fn peek(&self) -> i64 {
        self.hwm.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advance_is_strictly_monotonic_within_one_microsecond() {
        let clock = TtClock::default();
        clock.seed(None);
        let now = clock.peek(); // freeze "now"
        let a = clock.advance_with_now(now);
        let b = clock.advance_with_now(now);
        let c = clock.advance_with_now(now);
        assert!(a < b && b < c, "same-µs allocations must strictly increase");
    }

    #[test]
    fn advance_survives_backward_clock_step() {
        let clock = TtClock::default();
        clock.seed(None);
        let t = clock.advance();
        // NTP-style backward step of 5 seconds: monotonicity must hold.
        let after_step = clock.advance_with_now(t - 5_000_000);
        assert_eq!(after_step, t + 1);
        // And once the wall clock catches up past the HWM, tts track it again.
        let far_future = t + 60_000_000;
        assert_eq!(clock.advance_with_now(far_future), far_future);
    }

    #[test]
    fn seed_takes_max_of_persisted_and_wall_clock() {
        let clock = TtClock::default();
        let future = wall_clock_micros() + 3_600_000_000; // 1h ahead
        clock.seed(Some(future));
        assert_eq!(clock.peek(), future);

        let clock2 = TtClock::default();
        clock2.seed(Some(42)); // ancient persisted mark
        assert!(clock2.peek() >= wall_clock_micros() - 1_000_000);
    }

    #[test]
    fn concurrent_advances_are_unique_and_monotonic() {
        use std::collections::HashSet;
        use std::sync::Arc;
        let clock = Arc::new(TtClock::default());
        clock.seed(None);
        let mut handles = Vec::new();
        for _ in 0..8 {
            let c = clock.clone();
            handles.push(std::thread::spawn(move || {
                (0..1000).map(|_| c.advance()).collect::<Vec<_>>()
            }));
        }
        let mut all = Vec::new();
        for h in handles {
            let per_thread = h.join().unwrap();
            assert!(
                per_thread.windows(2).all(|w| w[0] < w[1]),
                "each caller must observe strictly increasing tts"
            );
            all.extend(per_thread);
        }
        let unique: HashSet<i64> = all.iter().copied().collect();
        assert_eq!(unique.len(), all.len(), "allocated tts must be unique");
    }

    #[test]
    fn commit_tt_persists_in_the_committing_tx_and_reseeds_on_reopen() {
        // sqlite backend: the stored path with real persistence.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tt.db");
        let path_str = path.to_str().unwrap().to_string();

        let far_future;
        {
            let db = crate::DbInstance::new("sqlite", &path_str, "").unwrap();
            let crate::DbInstance::Sqlite(inner) = &db else {
                panic!("expected sqlite instance")
            };
            // Push the in-memory clock well past the wall clock so the
            // persisted-mark path is observable on reopen (a wall-clock seed
            // alone could never reach it).
            far_future = inner
                .tt_clock()
                .advance_with_now(wall_clock_micros() + 3_600_000_000);

            let mut tx = inner.transact_write().unwrap();
            let tt = tx.commit_tx_with_tt().unwrap();
            assert_eq!(tt, far_future + 1);
        }

        // Reopen: seed must be >= the persisted mark, not just the wall clock.
        let db = crate::DbInstance::new("sqlite", &path_str, "").unwrap();
        let crate::DbInstance::Sqlite(inner) = &db else {
            panic!("expected sqlite instance")
        };
        assert!(
            inner.tt_clock().peek() > far_future,
            "reopen must re-seed from the persisted HWM ({}), got {}",
            far_future + 1,
            inner.tt_clock().peek()
        );
    }

    #[test]
    fn aborted_tx_does_not_persist_but_burns_values_monotonically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tt_abort.db");
        let path_str = path.to_str().unwrap().to_string();

        let db = crate::DbInstance::new("sqlite", &path_str, "").unwrap();
        let crate::DbInstance::Sqlite(inner) = &db else {
            panic!("expected sqlite instance")
        };

        // Allocate + persist inside a tx, then drop it without committing.
        // Deliberately hand-rolls the interior of commit_tx_with_tt WITHOUT
        // the lock (the real function always commits); a pre-commit crash is
        // the same rollback. HWM+data-rows same-tx atomicity is a step-3
        // test obligation once rows exist to stamp.
        let burned = {
            let mut tx = inner.transact_write().unwrap();
            let tt = tx.tt_clock.advance();
            tx.store_tx.put(&tt_hwm_key(), &tt.to_be_bytes()).unwrap();
            tt
            // tx dropped here -> rolled back
        };

        // The persisted key must not carry the aborted value...
        let tx = inner.transact().unwrap();
        let persisted = tx
            .store_tx
            .get(&tt_hwm_key(), false)
            .unwrap()
            .map(|v| i64::from_be_bytes(v.as_slice().try_into().unwrap()));
        assert_ne!(
            persisted,
            Some(burned),
            "aborted tx must not persist its tt"
        );

        // ...but the in-memory clock stays monotone past the burned value.
        let next = inner.tt_clock().advance();
        assert!(next > burned);
    }

    #[test]
    fn corrupt_persisted_hwm_refuses_to_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tt_corrupt.db");
        let path_str = path.to_str().unwrap().to_string();

        {
            let db = crate::DbInstance::new("sqlite", &path_str, "").unwrap();
            let crate::DbInstance::Sqlite(inner) = &db else {
                panic!("expected sqlite instance")
            };
            // Truncate the persisted mark to 3 bytes.
            let mut tx = inner.transact_write().unwrap();
            tx.store_tx.put(&tt_hwm_key(), &[1u8, 2, 3]).unwrap();
            tx.commit_tx().unwrap();
        }

        // Reopen must fail loudly, not silently fall back to the wall clock.
        let err = crate::DbInstance::new("sqlite", &path_str, "");
        assert!(err.is_err(), "corrupt tt HWM must refuse to open");
        let msg = format!("{:?}", err.err().unwrap());
        assert!(msg.contains("tt high-water mark is corrupt"), "got: {msg}");
    }

    #[cfg(feature = "storage-rocksdb")]
    #[test]
    fn overlapping_tt_commits_do_not_conflict_on_rocksdb() {
        // Regression for the snapshot-validation conflict: two temporally
        // overlapping pessimistic transactions both writing the TT_HWM key —
        // without put_externally_serialized the SECOND commit aborts with
        // `Resource busy` (the 0.8.4 avgdl hot-key failure mode).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tt_rocks");
        let path_str = path.to_str().unwrap().to_string();

        let db = crate::DbInstance::new("rocksdb", &path_str, "").unwrap();
        let crate::DbInstance::RocksDb(inner) = &db else {
            panic!("expected rocksdb instance")
        };

        // Both transactions begin (and snapshot) before either commits.
        let mut tx1 = inner.transact_write().unwrap();
        let mut tx2 = inner.transact_write().unwrap();
        let a = tx1.commit_tx_with_tt().unwrap();
        let b = tx2
            .commit_tx_with_tt()
            .expect("overlapping tt commit must not spuriously abort");
        assert!(b > a);
    }

    #[test]
    fn commit_tt_works_on_the_mem_backend() {
        // mem has no persistence; allocation + commit must still work.
        let db = crate::DbInstance::new("mem", "", "").unwrap();
        let crate::DbInstance::Mem(inner) = &db else {
            panic!("expected mem instance")
        };
        let mut tx = inner.transact_write().unwrap();
        let a = tx.commit_tx_with_tt().unwrap();
        drop(tx); // the mem write tx holds the store's write lock until dropped
        let mut tx2 = inner.transact_write().unwrap();
        let b = tx2.commit_tx_with_tt().unwrap();
        drop(tx2);
        assert!(b > a);
    }
}
