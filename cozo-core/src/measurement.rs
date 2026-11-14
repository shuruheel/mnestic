/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 */

//! Internal benchmark instrumentation. Only compiled with `test-hooks`.
//! Captures are thread-local, bounded, and opt-in. No supported production API.
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::{MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use miette::{ensure, Result};
use serde_derive::Serialize;

use crate::runtime::db::Db;
use crate::storage::Storage;
use crate::{DataValue, DbInstance, NamedRows};

const CAPACITY: usize = 256;
static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Process-local monotonic timestamp, comparable between capture threads.
pub fn now_ns() -> u64 {
    EPOCH
        .get_or_init(Instant::now)
        .elapsed()
        .as_nanos()
        .try_into()
        .expect("clock overflow")
}

/// An interval recorded without allocating while holding an engine lock.
#[derive(Clone, Debug, Serialize)]
pub struct Event {
    pub phase: &'static str,
    pub start_ns: u64,
    pub acquired_ns: Option<u64>,
    /// Last timestamp known to precede unlocking; absent for non-lock spans.
    pub release_begin_ns: Option<u64>,
    pub end_ns: u64,
    /// Span completed successfully, or lock was acquired and then released.
    /// A released lock does not imply the enclosing transaction committed.
    pub completed: bool,
}

#[derive(Debug, Serialize)]
pub struct Trace {
    pub operation_id: u64,
    pub enabled: bool,
    pub start_ns: u64,
    pub end_ns: u64,
    pub overflow: bool,
    pub events: Vec<Event>,
}

#[derive(Default)]
struct State {
    active: bool,
    enabled: bool,
    overflow: bool,
    events: Vec<Event>,
}
thread_local! { static STATE: RefCell<State> = RefCell::new(State::default()); }

struct CaptureGuard;
impl Drop for CaptureGuard {
    fn drop(&mut self) {
        STATE.with(|s| {
            let mut s = s.borrow_mut();
            s.active = false;
            s.enabled = false;
        });
    }
}

/// Time exactly the closure. Trace allocation/draining is outside this interval.
/// Nested captures on a thread are rejected. A panic disarms recording on unwind.
/// The caller assigns unique IDs across workers and keeps errors in its output.
pub fn capture<T>(operation_id: u64, enabled: bool, f: impl FnOnce() -> T) -> (T, Trace) {
    let _ = now_ns();
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        assert!(!s.active, "nested measurement capture");
        s.events = Vec::with_capacity(CAPACITY);
        s.overflow = false;
        s.active = true;
        s.enabled = enabled;
    });
    let guard = CaptureGuard;
    let start_ns = now_ns();
    let output = f();
    let end_ns = now_ns();
    let (events, overflow) = STATE.with(|s| {
        let mut s = s.borrow_mut();
        (std::mem::take(&mut s.events), s.overflow)
    });
    drop(guard);
    (
        output,
        Trace {
            operation_id,
            enabled,
            start_ns,
            end_ns,
            overflow,
            events,
        },
    )
}

fn record(event: Event) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if s.events.len() < CAPACITY {
            s.events.push(event);
        } else {
            s.overflow = true;
        }
    });
}

/// RAII also records early errors/unwinds. Never move a span to another thread.
pub(crate) struct Span {
    event: Option<Event>,
    _local: PhantomData<Rc<()>>,
}
impl Span {
    pub(crate) fn new(phase: &'static str) -> Self {
        let enabled = STATE.with(|s| s.borrow().enabled);
        Self {
            event: enabled.then(|| Event {
                phase,
                start_ns: now_ns(),
                acquired_ns: None,
                release_begin_ns: None,
                end_ns: 0,
                completed: false,
            }),
            _local: PhantomData,
        }
    }
    pub(crate) fn finish(mut self) {
        if let Some(e) = self.event.as_mut() {
            e.completed = true;
        }
    }
    /// Captures acquisition before recording anything into TLS.
    pub(crate) fn acquired<'a>(mut self, guard: MutexGuard<'a, ()>) -> SectionGuard<'a> {
        if let Some(e) = self.event.as_mut() {
            e.acquired_ns = Some(now_ns());
        }
        SectionGuard {
            guard: Some(guard),
            span: self,
        }
    }
}
impl Drop for Span {
    fn drop(&mut self) {
        if let Some(mut e) = self.event.take() {
            e.end_ns = now_ns();
            record(e);
        }
    }
}

/// Unlock first, then stamp release and append the event. No trace lock is taken.
pub(crate) struct SectionGuard<'a> {
    guard: Option<MutexGuard<'a, ()>>,
    span: Span,
}
impl Drop for SectionGuard<'_> {
    fn drop(&mut self) {
        let release_begin = self.span.event.as_ref().map(|_| now_ns());
        drop(self.guard.take());
        if let Some(mut e) = self.span.event.take() {
            e.release_begin_ns = release_begin;
            e.end_ns = now_ns();
            e.completed = true;
            record(e);
        }
    }
}

/// Benchmark-only equivalent of the protocol's exact T2 query. Catalog lookup
/// and transaction creation are excluded from execution, just as in run_query.
/// Batch construction, left scans, right decoding, residuals and output are timed.
pub fn t2_multi_get(db: &DbInstance, timeout: Duration) -> Result<NamedRows> {
    match db {
        DbInstance::Mem(db) => batch(db, timeout),
        #[cfg(feature = "storage-sqlite")]
        DbInstance::Sqlite(db) => batch(db, timeout),
        #[cfg(feature = "storage-rocksdb")]
        DbInstance::RocksDb(db) => batch(db, timeout),
        #[allow(unreachable_patterns)]
        _ => miette::bail!("T2 comparator supports mem, sqlite and rocksdb only"),
    }
}
fn batch<'s, S: Storage<'s>>(db: &'s Db<S>, timeout: Duration) -> Result<NamedRows> {
    let deadline = Instant::now() + timeout;
    let tx = db.transact()?;
    let left = tx.get_relation("edge", false)?;
    let right = tx.get_relation("node", false)?;
    ensure!(
        !left.has_txtime() && !right.has_txtime(),
        "T2 comparator requires plain relations"
    );
    ensure!(
        left.metadata
            .keys
            .iter()
            .map(|c| c.name.as_str())
            .eq(["from", "to"])
            && left.metadata.non_keys.is_empty()
            && right
                .metadata
                .keys
                .iter()
                .map(|c| c.name.as_str())
                .eq(["uid"])
            && right
                .metadata
                .non_keys
                .iter()
                .map(|c| c.name.as_str())
                .eq(["label", "tombstone_reason"]),
        "T2 comparator schema mismatch"
    );
    let span = Span::new("query_execution");
    // Some backend cursors can restart after returning None. Never repoll them.
    let mut scan = left.scan_all(&tx).fuse();
    let mut output = BTreeSet::new();
    loop {
        ensure!(Instant::now() < deadline, "T2 comparator timeout");
        let chunk = scan.by_ref().take(256).collect::<Result<Vec<_>>>()?;
        if chunk.is_empty() {
            break;
        }
        let keys: Vec<_> = chunk.iter().map(|r| &r[1..2]).collect();
        for (edge, node) in chunk.iter().zip(right.get_batch(&tx, &keys)?) {
            if node.is_some_and(|r| r[2] == DataValue::from("deleted")) {
                output.insert(edge.clone());
            }
        }
    }
    let rows = NamedRows::new(vec!["f".into(), "t".into()], output.into_iter().collect());
    span.finish();
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_errors_disabled_and_unwind() {
        let (_, trace) = capture(1, true, || {
            for _ in 0..CAPACITY + 1 {
                let _span = Span::new("error");
            }
        });
        assert!(trace.overflow);
        assert_eq!(trace.events.len(), CAPACITY);
        assert!(trace.events.iter().all(|e| !e.completed));
        let (_, trace) = capture(2, false, || Span::new("disabled").finish());
        assert!(trace.events.is_empty());
        let _ = std::panic::catch_unwind(|| capture(3, true, || panic!("unwind")));
        let (_, trace) = capture(4, true, || Span::new("recovered").finish());
        assert_eq!(trace.events.len(), 1);
        assert!(trace.events[0].completed);
    }
    #[test]
    fn threads_share_epoch_but_not_capture_state() {
        let start = now_ns();
        let threads: Vec<_> = (0..4)
            .map(|id| {
                std::thread::spawn(move || capture(id, true, || Span::new("worker").finish()).1)
            })
            .collect();
        for (id, worker) in threads.into_iter().enumerate() {
            let trace = worker.join().unwrap();
            assert_eq!(trace.operation_id, id as u64);
            assert!(start <= trace.start_ns && trace.start_ns <= trace.events[0].start_ns);
            assert!(trace.events[0].end_ns <= trace.end_ns);
            assert_eq!(trace.events.len(), 1);
        }
    }
}

#[cfg(test)]
mod engine_tests {
    use super::*;
    use crate::{ScriptMutability, ScriptRunOptions};
    use std::collections::BTreeMap;

    fn engines(mut test: impl FnMut(DbInstance)) {
        let dir = tempfile::tempdir().unwrap();
        let mut engines = vec!["mem"];
        #[cfg(feature = "storage-sqlite")]
        engines.push("sqlite");
        #[cfg(feature = "storage-rocksdb")]
        engines.push("rocksdb");
        for engine in engines {
            test(DbInstance::new(engine, dir.path().join(engine).to_str().unwrap(), "{}").unwrap());
        }
    }
    #[test]
    fn execution_includes_materialization_and_retains_errors() {
        engines(|db| {
            db.run_default(":create node {uid:String}").unwrap();
            db.run_default("?[uid] <- [['a'],['b']] :put node {uid}")
                .unwrap();
            let (rows, trace) = capture(1, true, || {
                db.run_default("?[uid] := *node{uid} :order -uid")
            });
            assert_eq!(
                rows.unwrap().rows,
                vec![vec![DataValue::from("b")], vec![DataValue::from("a")]]
            );
            let e = trace
                .events
                .iter()
                .find(|e| e.phase == "query_execution")
                .unwrap();
            assert!(e.completed && trace.start_ns <= e.start_ns && e.end_ns <= trace.end_ns);
            let (result, trace) = capture(2, true, || {
                db.run_default("?[uid] := *node{uid} :assert none")
            });
            assert!(result.is_err());
            assert!(trace
                .events
                .iter()
                .any(|e| e.phase == "query_execution" && !e.completed));
            let (result, trace) = capture(3, true, || db.run_default("this is not a query"));
            assert!(result.is_err());
            assert!(
                trace.events.is_empty(),
                "parse errors must not fabricate execution time"
            );
        });
    }
    #[test]
    fn batch_matches_t2_across_boundaries_duplicates_misses_and_residuals() {
        engines(|db| {
            db.run_default(":create edge {from:String,to:String}")
                .unwrap();
            db.run_default(":create node {uid:String => label:String,tombstone_reason:String?}")
                .unwrap();
            let nodes: Vec<_> = (0..300)
                .map(|i| {
                    serde_json::json!([
                        format!("n{i:04}"),
                        "label",
                        if i % 2 == 0 { Some("deleted") } else { None }
                    ])
                })
                .collect();
            let edges: Vec<_> = (0..600)
                .map(|i| serde_json::json!([format!("e{i:04}"), format!("n{:04}", i % 320)]))
                .collect();
            db.run_script(
                "?[uid,label,tombstone_reason] <- $rows :put node {uid=>label,tombstone_reason}",
                BTreeMap::from([("rows".into(), DataValue::from(serde_json::json!(nodes)))]),
                ScriptMutability::Mutable,
            )
            .unwrap();
            db.run_script(
                "?[from,to] <- $rows :put edge {from,to}",
                BTreeMap::from([("rows".into(), DataValue::from(serde_json::json!(edges)))]),
                ScriptMutability::Mutable,
            )
            .unwrap();
            let expected = db
                .run_default(
                    "?[f,t] := *edge{from:f,to:t}, *node{uid:t,tombstone_reason:'deleted'}",
                )
                .unwrap();
            let (actual, trace) = capture(4, true, || t2_multi_get(&db, Duration::from_secs(10)));
            assert_eq!(actual.unwrap().rows, expected.rows);
            assert_eq!(trace.events.len(), 1);
            assert!(trace.events[0].completed);
            assert_eq!(trace.events[0].phase, "query_execution");
        });
    }
    #[test]
    fn waiting_commit_timeout_is_an_unacquired_interval() {
        let db = DbInstance::new("mem", "", "{}").unwrap();
        db.run_default(":create t {id:Int,tt:TxTime => value:Int}")
            .unwrap();
        let DbInstance::Mem(inner) = &db else {
            unreachable!()
        };
        let guard = inner.tt_commit_lock.lock().unwrap();
        let writer = db.clone();
        let (result, trace) = std::thread::spawn(move || {
            capture(5, true, || {
                writer.run_script_with_options(
                    "?[id,value] <- [[1,2]] :put t {id=>value}",
                    BTreeMap::new(),
                    ScriptMutability::Mutable,
                    ScriptRunOptions {
                        timeout: Some(0.01),
                        ..Default::default()
                    },
                )
            })
        })
        .join()
        .unwrap();
        drop(guard);
        assert!(format!("{:?}", result.unwrap_err()).contains("timeout"));
        let e = trace
            .events
            .iter()
            .find(|e| e.phase == "tt_commit_section")
            .unwrap();
        assert!(!e.completed && e.acquired_ns.is_none() && e.end_ns > e.start_ns);
        let (result, trace) = capture(6, true, || {
            db.run_default("?[id,value] <- [[1,3]] :put t {id=>value}")
        });
        result.unwrap();
        let e = trace
            .events
            .iter()
            .find(|e| e.phase == "tt_commit_section")
            .unwrap();
        assert!(
            e.completed
                && e.start_ns <= e.acquired_ns.unwrap()
                && e.acquired_ns.unwrap() <= e.end_ns
        );
        assert!(trace
            .events
            .iter()
            .any(|e| e.phase == "commit" && e.completed));
    }
}
