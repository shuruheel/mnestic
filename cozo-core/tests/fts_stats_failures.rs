// SPDX-License-Identifier: MPL-2.0

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use cozo::{
    DataValue, Db, MemStorage, MultiTransaction, ScriptMutability, Storage, StoreTx, ValidityTs,
};
use crossbeam::channel::bounded;
use miette::{bail, Result};

#[derive(Default)]
struct Faults {
    delete_countdown: AtomicUsize,
    commit: AtomicBool,
}

#[derive(Clone, Default)]
struct FaultStorage {
    inner: MemStorage,
    faults: Arc<Faults>,
}

struct FaultTx<'s> {
    inner: <MemStorage as Storage<'s>>::Tx,
    faults: Arc<Faults>,
}

impl<'s> Storage<'s> for FaultStorage {
    type Tx = FaultTx<'s>;
    fn storage_kind(&self) -> &'static str {
        "fault-mem"
    }
    fn transact(&'s self, write: bool) -> Result<Self::Tx> {
        Ok(FaultTx {
            inner: self.inner.transact(write)?,
            faults: self.faults.clone(),
        })
    }
    fn range_compact(&'s self, lower: &[u8], upper: &[u8]) -> Result<()> {
        self.inner.range_compact(lower, upper)
    }
    fn batch_put<'a>(
        &'a self,
        data: Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>,
    ) -> Result<()> {
        self.inner.batch_put(data)
    }
}

impl<'s> StoreTx<'s> for FaultTx<'s> {
    fn get(&self, key: &[u8], update: bool) -> Result<Option<Vec<u8>>> {
        self.inner.get(key, update)
    }
    fn exists(&self, key: &[u8], update: bool) -> Result<bool> {
        self.inner.exists(key, update)
    }
    fn supports_par_put(&self) -> bool {
        false
    }
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.inner.put(key, value)
    }
    fn del(&mut self, key: &[u8]) -> Result<()> {
        // Rust 1.99 renamed this to try_update; keep the fixture compatible
        // with the project's Rust 1.98 development toolchain as well.
        #[allow(deprecated)]
        let previous =
            self.faults
                .delete_countdown
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
        if previous == Ok(1) {
            bail!("injected posting deletion failure");
        }
        self.inner.del(key)
    }
    fn del_range_from_persisted(&mut self, lower: &[u8], upper: &[u8]) -> Result<()> {
        self.inner.del_range_from_persisted(lower, upper)
    }
    fn commit(&mut self) -> Result<()> {
        if self.faults.commit.swap(false, Ordering::SeqCst) {
            bail!("injected storage commit failure");
        }
        self.inner.commit()
    }
    fn range_scan<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>
    where
        's: 'a,
    {
        self.inner.range_scan(lower, upper)
    }
    fn range_count<'a>(&'a self, lower: &[u8], upper: &[u8]) -> Result<usize>
    where
        's: 'a,
    {
        self.inner.range_count(lower, upper)
    }
    fn total_scan<'a>(&'a self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>
    where
        's: 'a,
    {
        self.inner.total_scan()
    }
    fn range_skip_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
        at: ValidityTs,
    ) -> Box<dyn Iterator<Item = Result<Vec<DataValue>>> + 'a> {
        self.inner.range_skip_scan_tuple(lower, upper, at)
    }
}

const QUERY: &str = "?[id,score] := ~doc:idx{id | query:'alpha',k:20,bind_score:score} :order id";

fn setup() -> (Db<FaultStorage>, Arc<Faults>) {
    let storage = FaultStorage::default();
    let faults = storage.faults.clone();
    let db = Db::new(storage).unwrap();
    db.initialize().unwrap();
    for script in [
        ":create doc {id:String=>body:String}",
        "::fts create doc:idx {extractor:body,tokenizer:Simple,filters:[Lowercase]}",
        "?[id,body] <- [['a','alpha beta gamma delta'],['b','alpha pad']] :put doc{id=>body}",
    ] {
        db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
            .unwrap();
    }
    (db, faults)
}

fn transaction(db: &Db<FaultStorage>) -> (MultiTransaction, std::thread::JoinHandle<()>) {
    let (send, payloads) = bounded(1);
    let (results, recv) = bounded(1);
    let db = db.clone();
    let worker = std::thread::spawn(move || db.run_multi_transaction(true, payloads, results));
    (
        MultiTransaction {
            sender: send,
            receiver: recv,
        },
        worker,
    )
}

#[test]
fn partial_posting_failure_cannot_be_committed_even_after_a_retry() {
    let (db, faults) = setup();
    let before = db
        .run_script(QUERY, BTreeMap::new(), ScriptMutability::Immutable)
        .unwrap()
        .rows;
    let (tx, worker) = transaction(&db);
    faults.delete_countdown.store(3, Ordering::SeqCst);
    assert!(tx
        .run_script("?[id]<-[['a']] :rm doc{id}", BTreeMap::new())
        .is_err());
    // Re-read and retry within the same legacy transaction. Neither may erase
    // the fact that a posting mutation was incomplete.
    tx.run_script(QUERY, BTreeMap::new()).unwrap();
    tx.run_script("?[id]<-[['a']] :rm doc{id}", BTreeMap::new())
        .unwrap();
    tx.run_script(QUERY, BTreeMap::new()).unwrap();
    assert!(tx
        .commit()
        .unwrap_err()
        .to_string()
        .contains("incomplete FTS mutation"));
    drop(tx);
    worker.join().unwrap();
    assert_eq!(
        db.run_script(QUERY, BTreeMap::new(), ScriptMutability::Immutable)
            .unwrap()
            .rows,
        before
    );
}

#[test]
fn storage_commit_failure_keeps_statistics_private_until_rollback_finishes() {
    let (db, faults) = setup();
    let before = db
        .run_script(QUERY, BTreeMap::new(), ScriptMutability::Immutable)
        .unwrap()
        .rows;
    let (tx, worker) = transaction(&db);
    tx.run_script("?[id]<-[['a']] :rm doc{id}", BTreeMap::new())
        .unwrap();
    faults.commit.store(true, Ordering::SeqCst);
    assert!(tx.commit().is_err());
    drop(tx);
    worker.join().unwrap();
    assert_eq!(
        db.run_script(QUERY, BTreeMap::new(), ScriptMutability::Immutable)
            .unwrap()
            .rows,
        before
    );
}

#[test]
fn ignored_partial_posting_error_still_rejects_outer_script_commit() {
    let (db, faults) = setup();
    let before = db
        .run_script(QUERY, BTreeMap::new(), ScriptMutability::Immutable)
        .unwrap()
        .rows;
    faults.delete_countdown.store(3, Ordering::SeqCst);
    let error = db
        .run_script(
            "%ignore_error {?[id]<-[['a']] :rm doc{id}} {?[v]<-[[1]]}",
            BTreeMap::new(),
            ScriptMutability::Mutable,
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("incomplete FTS mutation"),
        "{error:?}"
    );
    assert_eq!(faults.delete_countdown.load(Ordering::SeqCst), 0);
    assert_eq!(
        db.run_script(QUERY, BTreeMap::new(), ScriptMutability::Immutable)
            .unwrap()
            .rows,
        before
    );
}
