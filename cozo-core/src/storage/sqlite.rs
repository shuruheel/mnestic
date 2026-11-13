/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ::sqlite::Connection;
use crossbeam::sync::{ShardedLock, ShardedLockReadGuard, ShardedLockWriteGuard};
use either::{Either, Left, Right};
use miette::{bail, miette, IntoDiagnostic, Result};
use sqlite::{ConnectionThreadSafe, State, Statement};

use crate::data::tuple::{check_key_for_validity, Tuple};
use crate::data::value::ValidityTs;
use crate::runtime::relation::{try_decode_tuple_from_kv, try_extend_tuple_from_v};
use crate::storage::{Storage, StoreTx};
use crate::utils::swap_option_result;

/// The Sqlite storage engine
#[derive(Clone)]
pub struct SqliteStorage {
    lock: Arc<ShardedLock<()>>,
    name: PathBuf,
    pool: Arc<Mutex<Vec<ConnectionThreadSafe>>>,
    options: Arc<SqliteOptions>,
}

/// Connection settings for the SQLite engine (mnestic fork, 0.19.2).
///
/// Before 0.19.2 every connection ran with SQLite's defaults: a rollback
/// journal, a 2 MB page cache and no busy timeout. A single-row write then
/// paid a journal copy of every touched page plus two fsyncs, and most reads
/// missed the cache. The defaults below keep the same durability
/// (`synchronous = FULL`: an acknowledged commit survives power loss) while
/// appending to a write-ahead log instead.
///
/// From JSON options (`DbInstance::new("sqlite", path, options)`), all keys
/// optional: `{"journal_mode": "wal" | "delete" | "keep", "synchronous":
/// "full" | "normal", "cache_mb": 32, "busy_timeout_ms": 5000, "mmap_mb": 0}`.
#[derive(Clone, Debug, serde_derive::Deserialize)]
// Unknown keys are ignored, as before 0.19.2: hosts pass one options object to
// whichever engine they open (e.g. RocksDB's `shared_memory`).
#[serde(default)]
pub struct SqliteOptions {
    /// `wal` (default), `delete` (single-file rollback journal, the pre-0.19.2
    /// behaviour) or `keep` (leave the file's existing mode untouched).
    pub journal_mode: String,
    /// `full` (default) or `normal`. In WAL mode `normal` can lose the most
    /// recent commits on power loss (never on a process crash) and never
    /// corrupts the database.
    pub synchronous: String,
    /// Page cache per pooled connection, in MiB. Allocated only as pages are read.
    pub cache_mb: u32,
    /// How long a connection waits for another process's lock before failing.
    pub busy_timeout_ms: u32,
    /// Memory-map up to this many MiB of the file for reads (0 disables).
    pub mmap_mb: u32,
}

impl Default for SqliteOptions {
    fn default() -> Self {
        Self {
            journal_mode: "wal".to_string(),
            synchronous: "full".to_string(),
            cache_mb: 32,
            busy_timeout_ms: 5000,
            mmap_mb: 0,
        }
    }
}

impl SqliteOptions {
    /// Self-contained single file: backup outputs stay one portable file.
    pub(crate) fn single_file() -> Self {
        Self {
            journal_mode: "delete".to_string(),
            ..Self::default()
        }
    }
    /// Opening someone else's file (a backup being restored or imported from)
    /// must not rewrite its header.
    pub(crate) fn keep_file_mode() -> Self {
        Self {
            journal_mode: "keep".to_string(),
            ..Self::default()
        }
    }

    fn validate(&self) -> Result<()> {
        match self.journal_mode.as_str() {
            "wal" | "delete" | "keep" => {}
            other => bail!("sqlite option journal_mode must be wal, delete or keep, got {other:?}"),
        }
        match self.synchronous.as_str() {
            "full" | "normal" => {}
            other => bail!("sqlite option synchronous must be full or normal, got {other:?}"),
        }
        Ok(())
    }

    /// Per-connection settings; SQLite does not persist these in the file.
    fn configure(&self, conn: &Connection) -> Result<()> {
        let cache_kib = i64::from(self.cache_mb) * 1024;
        let mmap_bytes = i64::from(self.mmap_mb) * 1024 * 1024;
        conn.execute(format!(
            "pragma busy_timeout = {}; pragma synchronous = {}; \
             pragma cache_size = -{cache_kib}; pragma mmap_size = {mmap_bytes};",
            self.busy_timeout_ms, self.synchronous
        ))
        .into_diagnostic()
    }
}

/// Create a sqlite backed database.
/// Supports concurrent readers but only a single writer.
///
/// You must provide a disk-based path: `:memory:` is not OK.
/// If you want a pure memory storage, use [`new_cozo_mem`](crate::new_cozo_mem).
///
/// Uses [`SqliteOptions::default`]: write-ahead logging with full sync.
pub fn new_cozo_sqlite(path: impl AsRef<Path>) -> Result<crate::Db<SqliteStorage>> {
    new_cozo_sqlite_with_options(path, SqliteOptions::default())
}

/// Create a sqlite backed database with explicit connection settings.
///
/// A newly created file stores its key-value table `WITHOUT ROWID` (0.19.2),
/// so a key lookup or range scan reads one B-tree instead of an index plus
/// the table. Existing files keep their layout; `::backup` writes the new
/// layout, and opening that backup file migrates.
pub fn new_cozo_sqlite_with_options(
    path: impl AsRef<Path>,
    options: SqliteOptions,
) -> Result<crate::Db<SqliteStorage>> {
    if path.as_ref().to_str() == Some("") {
        bail!("empty path for sqlite storage")
    }
    options.validate()?;
    let conn = Connection::open_thread_safe(&path).into_diagnostic()?;
    options.configure(&conn)?;
    match options.journal_mode.as_str() {
        "wal" => conn
            .execute("pragma journal_mode = wal;")
            .into_diagnostic()?,
        "delete" => conn
            .execute("pragma journal_mode = delete;")
            .into_diagnostic()?,
        _ => {}
    }
    let query = r#"
        create table if not exists cozo
        (
            k BLOB primary key,
            v BLOB
        ) without rowid;
    "#;
    let mut statement = conn.prepare(query).unwrap();
    while statement.next().into_diagnostic()? != State::Done {}
    drop(statement);

    let ret = crate::Db::new(SqliteStorage {
        lock: Default::default(),
        name: PathBuf::from(path.as_ref()),
        pool: Arc::new(Mutex::new(vec![conn])),
        options: Arc::new(options),
    })?;

    ret.initialize()?;
    Ok(ret)
}

impl<'s> Storage<'s> for SqliteStorage {
    type Tx = SqliteTx<'s>;

    fn transact(&'s self, write: bool) -> Result<Self::Tx> {
        let conn = {
            let pooled = self.pool.lock().unwrap_or_else(|e| e.into_inner()).pop();
            match pooled {
                None => {
                    let conn = Connection::open_thread_safe(&self.name).into_diagnostic()?;
                    self.options.configure(&conn)?;
                    conn
                }
                Some(conn) => conn,
            }
        };
        let lock = if write {
            Right(self.lock.write().unwrap_or_else(|e| e.into_inner()))
        } else {
            Left(self.lock.read().unwrap_or_else(|e| e.into_inner()))
        };
        if write {
            // IMMEDIATE takes SQLite's write lock up front, so a competing
            // process makes this wait (busy_timeout) instead of failing a
            // deferred lock upgrade halfway through the transaction.
            let mut stmt = conn.prepare("begin immediate;").into_diagnostic()?;
            while stmt.next().into_diagnostic()? != State::Done {}
        }
        Ok(SqliteTx {
            lock,
            storage: self,
            conn: Some(conn),
            stmts: [
                Mutex::new(None),
                Mutex::new(None),
                Mutex::new(None),
                Mutex::new(None),
            ],
            committed: false,
        })
    }

    fn batch_put<'a>(
        &'a self,
        data: Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>,
    ) -> Result<()> {
        let mut tx = self.transact(true)?;
        for result in data {
            let (key, val) = result?;
            tx.put(&key, &val)?;
        }
        tx.commit()?;
        Ok(())
    }

    fn range_compact(&'_ self, _lower: &[u8], _upper: &[u8]) -> Result<()> {
        let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        while pool.pop().is_some() {}
        Ok(())
    }

    fn storage_kind(&self) -> &'static str {
        "sqlite"
    }
}

pub struct SqliteTx<'a> {
    lock: Either<ShardedLockReadGuard<'a, ()>, ShardedLockWriteGuard<'a, ()>>,
    storage: &'a SqliteStorage,
    conn: Option<ConnectionThreadSafe>,
    stmts: [Mutex<Option<Statement<'a>>>; N_CACHED_QUERIES],
    committed: bool,
}

unsafe impl Sync for SqliteTx<'_> {}

const N_QUERIES: usize = 7;
const N_CACHED_QUERIES: usize = 4;
const QUERIES: [&str; N_QUERIES] = [
    "select v from cozo where k = ?;",
    "insert into cozo(k, v) values (?, ?) on conflict(k) do update set v=excluded.v;",
    "delete from cozo where k = ?;",
    "select 1 from cozo where k = ?;",
    "select k, v from cozo where k >= ? and k < ? order by k;",
    "select k, v from cozo where k >= ? and k < ? order by k limit 1;",
    "select count(*) from cozo where k >= ? and k < ?;",
];

const GET_QUERY: usize = 0;
const PUT_QUERY: usize = 1;
const DEL_QUERY: usize = 2;
const EXISTS_QUERY: usize = 3;
const RANGE_QUERY: usize = 4;
const SKIP_RANGE_QUERY: usize = 5;
const COUNT_RANGE_QUERY: usize = 6;

impl Drop for SqliteTx<'_> {
    fn drop(&mut self) {
        if let Right(ShardedLockWriteGuard { .. }) = self.lock {
            if !self.committed {
                let query = r#"rollback;"#;
                let _ = self.conn.as_ref().unwrap().execute(query);
            }
        }
        let mut pool = self.storage.pool.lock().unwrap_or_else(|e| e.into_inner());
        let conn = self.conn.take().unwrap();
        pool.push(conn)
    }
}

impl<'s> SqliteTx<'s> {
    fn ensure_stmt(&self, idx: usize) {
        let mut stmt = self.stmts[idx].lock().unwrap();
        if stmt.is_none() {
            let query = QUERIES[idx];
            let prepared = self.conn.as_ref().unwrap().prepare(query).unwrap();

            // Casting away the lifetime!
            // This is OK because we are abiding by the contract of the underlying C pointer,
            // as required by Sqlite's implementation
            let prepared = unsafe { std::mem::transmute(prepared) };

            *stmt = Some(prepared)
        }
    }
}

impl<'s> StoreTx<'s> for SqliteTx<'s> {
    fn get(&self, key: &[u8], _for_update: bool) -> Result<Option<Vec<u8>>> {
        self.ensure_stmt(GET_QUERY);
        let mut statement = self.stmts[GET_QUERY].lock().unwrap();
        let statement = statement.as_mut().unwrap();
        statement.reset().unwrap();

        statement.bind((1, key)).unwrap();
        Ok(match statement.next().into_diagnostic()? {
            State::Row => {
                let res = statement.read::<Vec<u8>, _>(0).into_diagnostic()?;
                Some(res)
            }
            State::Done => None,
        })
    }

    fn put(&mut self, key: &[u8], val: &[u8]) -> Result<()> {
        self.par_put(key, val)
    }

    fn supports_par_put(&self) -> bool {
        true
    }

    fn par_put(&self, key: &[u8], val: &[u8]) -> Result<()> {
        self.ensure_stmt(PUT_QUERY);
        let mut statement = self.stmts[PUT_QUERY].lock().unwrap();
        let statement = statement.as_mut().unwrap();
        statement.reset().unwrap();

        statement.bind((1, key)).unwrap();
        statement.bind((2, val)).unwrap();
        while statement.next().into_diagnostic()? != State::Done {}
        Ok(())
    }

    fn del(&mut self, key: &[u8]) -> Result<()> {
        self.par_del(key)
    }

    fn par_del(&self, key: &[u8]) -> Result<()> {
        self.ensure_stmt(DEL_QUERY);
        let mut statement = self.stmts[DEL_QUERY].lock().unwrap();
        let statement = statement.as_mut().unwrap();
        statement.reset().unwrap();

        statement.bind((1, key)).unwrap();
        while statement.next().into_diagnostic()? != State::Done {}

        Ok(())
    }

    fn del_range_from_persisted(&mut self, lower: &[u8], upper: &[u8]) -> Result<()> {
        let query = r#"
                delete from cozo where k >= ? and k < ?;
            "#;
        let mut statement = self.conn.as_ref().unwrap().prepare(query).unwrap();

        statement.bind((1, lower)).unwrap();
        statement.bind((2, upper)).unwrap();
        while statement.next().unwrap() != State::Done {}
        Ok(())
    }

    fn exists(&self, key: &[u8], _for_update: bool) -> Result<bool> {
        self.ensure_stmt(EXISTS_QUERY);
        let mut statement = self.stmts[EXISTS_QUERY].lock().unwrap();
        let statement = statement.as_mut().unwrap();
        statement.reset().unwrap();

        statement.bind((1, key)).unwrap();
        Ok(match statement.next().into_diagnostic()? {
            State::Row => true,
            State::Done => false,
        })
    }

    fn commit(&mut self) -> Result<()> {
        if let Right(ShardedLockWriteGuard { .. }) = self.lock {
            if !self.committed {
                let query = r#"commit;"#;
                let mut statement = self.conn.as_ref().unwrap().prepare(query).unwrap();
                while statement.next().into_diagnostic()? != State::Done {}
                self.committed = true;
            } else {
                bail!("multiple commits")
            }
        }
        Ok(())
    }

    fn range_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a>
    where
        's: 'a,
    {
        // Range scans cannot use cached prepared statements, as several of them
        // can be used at the same time.
        let query = QUERIES[RANGE_QUERY];
        let mut statement = self.conn.as_ref().unwrap().prepare(query).unwrap();
        statement.bind((1, lower)).unwrap();
        statement.bind((2, upper)).unwrap();
        Box::new(TupleIter(statement))
    }

    fn range_skip_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
        valid_at: ValidityTs,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        let query = QUERIES[SKIP_RANGE_QUERY];
        let statement = self.conn.as_ref().unwrap().prepare(query).unwrap();
        Box::new(SkipIter {
            stmt: statement,
            valid_at,
            next_bound: lower.to_vec(),
            upper_bound: upper.to_vec(),
        })
    }

    fn range_bitemporal_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
        vt_at: Option<ValidityTs>,
        tt_at: ValidityTs,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        // Step-6 seek override: ONE prepared statement for the whole walk,
        // reset + rebound per real seek; sequential probes ride the open
        // cursor (`HybridProbe`). The generic default re-prepares the range
        // query per probe, which dominates.
        let query = QUERIES[RANGE_QUERY];
        let statement = self.conn.as_ref().unwrap().prepare(query).unwrap();
        let mut probe = crate::data::bitemporal::HybridProbe::new(SqliteSeekCursor {
            stmt: statement,
            upper_bound: upper.to_vec(),
        });
        Box::new(crate::data::bitemporal::BitemporalIter::new(
            move |bound: &[u8], far: bool| probe.probe(bound, far),
            lower.to_vec(),
            vt_at,
            tt_at,
        ))
    }

    fn range_scan<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>
    where
        's: 'a,
    {
        let query = QUERIES[RANGE_QUERY];
        let mut statement = self.conn.as_ref().unwrap().prepare(query).unwrap();
        statement.bind((1, lower)).unwrap();
        statement.bind((2, upper)).unwrap();
        Box::new(RawIter(statement))
    }

    fn range_count<'a>(&'a self, lower: &[u8], upper: &[u8]) -> Result<usize>
    where
        's: 'a,
    {
        let query = QUERIES[COUNT_RANGE_QUERY];
        let mut statement = self.conn.as_ref().unwrap().prepare(query).unwrap();
        statement.bind((1, lower)).unwrap();
        statement.bind((2, upper)).unwrap();
        match statement.next() {
            Ok(State::Done) => bail!("range count query returned no rows"),
            Ok(State::Row) => {
                let k = statement.read::<i64, _>(0).unwrap();
                Ok(k as usize)
            }
            Err(err) => bail!(err),
        }
    }

    fn total_scan<'a>(&'a self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>
    where
        's: 'a,
    {
        let statement = self
            .conn
            .as_ref()
            .unwrap()
            .prepare("select k, v from cozo order by k;")
            .unwrap();
        Box::new(RawIter(statement))
    }
}

struct TupleIter<'l>(Statement<'l>);

impl<'l> Iterator for TupleIter<'l> {
    type Item = Result<Tuple>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.0.next() {
            Ok(State::Done) => None,
            Ok(State::Row) => {
                let k = self.0.read::<Vec<u8>, _>(0).unwrap();
                let v = self.0.read::<Vec<u8>, _>(1).unwrap();
                Some(try_decode_tuple_from_kv(&k, &v, None))
            }
            Err(err) => Some(Err(miette!(err))),
        }
    }
}

struct RawIter<'l>(Statement<'l>);

impl<'l> Iterator for RawIter<'l> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.0.next() {
            Ok(State::Done) => None,
            Ok(State::Row) => {
                let k = self.0.read::<Vec<u8>, _>(0).unwrap();
                let v = self.0.read::<Vec<u8>, _>(1).unwrap();
                Some(Ok((k, v)))
            }
            Err(err) => Some(Err(miette!(err))),
        }
    }
}

/// Pinned-statement cursor for the bitemporal walk (bitemporality step 6).
/// `seek` = reset + rebind the lower bound; `step` = continue the open
/// cursor. The SQL (`k >= ? and k < ?`) enforces the upper bound.
struct SqliteSeekCursor<'l> {
    stmt: Statement<'l>,
    upper_bound: Vec<u8>,
}

impl<'l> SqliteSeekCursor<'l> {
    fn read_row(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        match self.stmt.next().into_diagnostic()? {
            State::Done => Ok(None),
            State::Row => {
                let k = self.stmt.read::<Vec<u8>, _>(0).unwrap();
                let v = self.stmt.read::<Vec<u8>, _>(1).unwrap();
                Ok(Some((k, v)))
            }
        }
    }
}

impl<'l> crate::data::bitemporal::SeekCursor for SqliteSeekCursor<'l> {
    fn seek(&mut self, bound: &[u8]) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        if bound >= self.upper_bound.as_slice() {
            return Ok(None);
        }
        self.stmt.reset().into_diagnostic()?;
        self.stmt.bind((1, bound)).unwrap();
        self.stmt.bind((2, &self.upper_bound as &[u8])).unwrap();
        self.read_row()
    }

    fn step(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        self.read_row()
    }
}

struct SkipIter<'l> {
    stmt: Statement<'l>,
    valid_at: ValidityTs,
    next_bound: Vec<u8>,
    upper_bound: Vec<u8>,
}

impl<'l> SkipIter<'l> {
    fn next_inner(&mut self) -> Result<Option<Tuple>> {
        loop {
            self.stmt.reset().into_diagnostic()?;
            self.stmt.bind((1, &self.next_bound as &[u8])).unwrap();
            self.stmt.bind((2, &self.upper_bound as &[u8])).unwrap();

            match self.stmt.next().into_diagnostic()? {
                State::Done => return Ok(None),
                State::Row => {
                    let k = self.stmt.read::<Vec<u8>, _>(0).unwrap();
                    let (ret, nxt_bound) = check_key_for_validity(&k, self.valid_at, None);
                    self.next_bound = nxt_bound;
                    if let Some(mut tup) = ret {
                        let v = self.stmt.read::<Vec<u8>, _>(1).unwrap();
                        try_extend_tuple_from_v(&mut tup, &v)?;
                        return Ok(Some(tup));
                    }
                }
            }
        }
    }
}

impl<'l> Iterator for SkipIter<'l> {
    type Item = Result<Tuple>;

    fn next(&mut self) -> Option<Self::Item> {
        swap_option_result(self.next_inner())
    }
}
