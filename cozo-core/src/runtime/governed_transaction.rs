/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Bounded, host-owned multi-transaction workers. Native targets only.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam::channel::{bounded, Receiver, Sender};
use miette::{bail, Diagnostic, Report, Result};
use thiserror::Error;

use crate::{DataValue, DbInstance, NamedRows, Poison, TransactionPayload};

/// Limits for one entire multi-transaction, including time queued before its
/// worker starts and time between queries. Db defaults can only tighten these.
/// Memory is estimated per statement, not a bound on total allocator RSS or
/// data retained by the application across statements.
#[derive(Clone, Copy, Debug)]
pub struct GovernedTransactionOptions {
    /// Absolute wall-clock deadline. Required: there is no unlimited worker.
    pub deadline: Instant,
    /// Maximum wait between commands. Must be positive. Defaults to 5 seconds.
    pub idle_timeout: Duration,
    /// Per-statement estimated memory limit; combined with the Db default and
    /// each query's `:mem_limit` by taking the minimum.
    pub mem_limit: Option<usize>,
}

impl GovernedTransactionOptions {
    /// Use an existing request deadline; do not re-arm it for every query.
    pub fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            idle_timeout: Duration::from_secs(5),
            mem_limit: None,
        }
    }
}

/// A cancellation signal independent of the blocking client. Cancelling is
/// cooperative: the host must retain worker-owned resources until `run` exits.
#[derive(Clone)]
pub struct GovernedTransactionCancellation {
    flag: Arc<AtomicBool>,
    wake: Sender<()>,
}

impl GovernedTransactionCancellation {
    /// Request rollback and wake an idle worker. A storage commit already in
    /// progress cannot be undone by cancellation.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
        let _ = self.wake.try_send(());
    }
}

/// Serialized blocking client. Dropping it cancels the worker; it does not
/// join it. Every query error terminates and rolls back the transaction.
/// Private channels and mutable methods prevent response miscorrelation.
pub struct GovernedTransaction {
    commands: Sender<TransactionPayload>,
    results: Receiver<NamedRows>,
    cancellation: GovernedTransactionCancellation,
    terminal: Arc<Mutex<Option<Report>>>,
    deadline: Instant,
    finished: bool,
}

impl GovernedTransaction {
    /// A separately owned signal for cancellation of a blocked client call.
    pub fn cancellation(&self) -> GovernedTransactionCancellation {
        self.cancellation.clone()
    }

    /// Run one query on the transaction's snapshot. Imperative scripts and
    /// system commands are unsupported, as with the legacy multi-transaction.
    pub fn run_script(
        &mut self,
        script: &str,
        params: BTreeMap<String, DataValue>,
    ) -> Result<NamedRows> {
        self.request(TransactionPayload::Query((script.to_owned(), params)))
    }

    /// Commit once. A timeout or cancellation concurrent with a storage commit
    /// does not prove rollback; reconcile using application idempotency keys.
    pub fn commit(mut self) -> Result<()> {
        self.request(TransactionPayload::Commit).map(|_| ())
    }

    /// Roll back once. The worker drops the storage transaction before replying.
    pub fn abort(mut self) -> Result<()> {
        self.request(TransactionPayload::Abort).map(|_| ())
    }

    fn request(&mut self, payload: TransactionPayload) -> Result<NamedRows> {
        if self.finished {
            return Err(closed());
        }
        let finishing = !matches!(payload, TransactionPayload::Query(_));
        if self.commands.send_deadline(payload, self.deadline).is_err() {
            return Err(self.failure());
        }
        match self.results.recv_deadline(self.deadline) {
            Ok(rows) => {
                self.finished = finishing;
                Ok(rows)
            }
            Err(_) => Err(self.failure()),
        }
    }

    fn failure(&mut self) -> Report {
        self.finished = true;
        // The worker publishes the original diagnostic before dropping its
        // channels. In particular, a late call after an idle timeout must not
        // erase eval::timeout into a generic send/disconnect error.
        let recorded = self.terminal.lock().unwrap().take();
        let error = recorded.unwrap_or_else(|| {
            if Instant::now() >= self.deadline {
                timeout()
            } else if self.cancellation.flag.load(Ordering::Relaxed) {
                cancelled()
            } else {
                closed()
            }
        });
        self.cancellation.cancel();
        error
    }
}

impl Drop for GovernedTransaction {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

/// The actual transaction owner. Move this into a host-managed blocking thread
/// together with its admission permit and database owner, then call `run`.
/// No transaction is opened until `run`; dropping an unstarted worker is safe.
/// Do not park it on a shared query/evaluation thread pool.
pub struct GovernedTransactionWorker {
    db: DbInstance,
    pub(crate) write: bool,
    pub(crate) options: GovernedTransactionOptions,
    commands: Receiver<TransactionPayload>,
    results: Sender<NamedRows>,
    cancel_wake: Receiver<()>,
    pub(crate) poison: Poison,
    terminal: Arc<Mutex<Option<Report>>>,
}

impl GovernedTransactionWorker {
    pub(crate) fn pair(
        db: DbInstance,
        write: bool,
        options: GovernedTransactionOptions,
    ) -> Result<(GovernedTransaction, Self)> {
        if options.idle_timeout.is_zero() {
            bail!("governed transaction idle timeout must be positive");
        }
        let (commands, command_rx) = bounded(1);
        let (results, result_rx) = bounded(1);
        let (wake, cancel_wake) = bounded(1);
        let poison = Poison::with_deadline(Some(options.deadline));
        let terminal = Arc::new(Mutex::new(None));
        let client = GovernedTransaction {
            commands,
            results: result_rx,
            cancellation: GovernedTransactionCancellation {
                flag: poison.flag.clone(),
                wake,
            },
            terminal: terminal.clone(),
            deadline: options.deadline,
            finished: false,
        };
        Ok((
            client,
            Self {
                db,
                write,
                options,
                commands: command_rx,
                results,
                cancel_wake,
                poison,
                terminal,
            },
        ))
    }

    /// Run to termination on the current thread. Returning means the storage
    /// transaction has been dropped, including on query errors and disconnects.
    /// Errors are delivered to the client with their original diagnostic codes.
    pub fn run(self) {
        let result = match &self.db {
            DbInstance::Mem(db) => db.run_governed_transaction(&self),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => db.run_governed_transaction(&self),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => db.run_governed_transaction(&self),
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => db.run_governed_transaction(&self),
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => db.run_governed_transaction(&self),
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => db.run_governed_transaction(&self),
        };
        if let Err(error) = result {
            *self.terminal.lock().unwrap() = Some(error);
        }
    }

    pub(crate) fn next(&self) -> Result<TransactionPayload> {
        self.poison.check()?;
        let idle_deadline = Instant::now()
            .checked_add(self.options.idle_timeout)
            .unwrap_or(self.options.deadline)
            .min(self.options.deadline);
        let payload = crossbeam::select! {
            recv(self.commands) -> command => command.map_err(|_| cancelled()),
            recv(self.cancel_wake) -> _ => Err(cancelled()),
            default(idle_deadline.saturating_duration_since(Instant::now())) => Err(timeout()),
        }?;
        self.poison.check()?;
        Ok(payload)
    }

    pub(crate) fn reply(&self, rows: NamedRows) -> Result<()> {
        // One request at a time, hence at most one response. Never block while
        // retaining a snapshot if the caller abandons or violates the protocol.
        self.results.try_send(rows).map_err(|_| closed())
    }

    pub(crate) fn sleep(&self, duration: Duration, block_deadline: Option<Instant>) -> Result<()> {
        self.poison.check()?;
        let deadline = block_deadline
            .unwrap_or(self.options.deadline)
            .min(self.options.deadline);
        let end = Instant::now()
            .checked_add(duration)
            .unwrap_or(deadline)
            .min(deadline);
        crossbeam::select! {
            recv(self.cancel_wake) -> _ => return Err(cancelled()),
            default(end.saturating_duration_since(Instant::now())) => {},
        }
        self.poison.check()?;
        if Instant::now() >= deadline {
            return Err(timeout());
        }
        Ok(())
    }
}

fn timeout() -> Report {
    #[derive(Debug, Error, Diagnostic)]
    #[error("Governed transaction exceeded its deadline or inter-query idle limit")]
    #[diagnostic(code(eval::timeout))]
    struct TransactionTimeout;
    TransactionTimeout.into()
}

fn cancelled() -> Report {
    #[derive(Debug, Error, Diagnostic)]
    #[error("Governed transaction was cancelled")]
    #[diagnostic(code(eval::killed))]
    struct TransactionCancelled;
    TransactionCancelled.into()
}

fn closed() -> Report {
    #[derive(Debug, Error, Diagnostic)]
    #[error("Governed transaction is closed")]
    #[diagnostic(code(transaction::closed))]
    struct TransactionClosed;
    TransactionClosed.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warnings_flush_on_success_parse_failure_and_evaluation_failure() {
        for script in ["?[x] <- [[1]]", "invalid query [", "?[x] := *missing[x]"] {
            let db = DbInstance::default();
            let (mut client, worker) = db
                .governed_transaction(
                    false,
                    GovernedTransactionOptions::new(Instant::now() + Duration::from_secs(5)),
                )
                .unwrap();
            let thread = std::thread::spawn(move || {
                // Models a warning emitted earlier on this worker by an
                // extension. Even a subsequent parse failure must flush it.
                crate::runtime::diagnostics::emit(
                    "test.worker_warning",
                    "worker warning".into(),
                    "test fixture",
                );
                worker.run();
            });
            let result = client.run_script(script, BTreeMap::new());
            let warnings = db.run_default("::warnings").unwrap();
            assert_eq!(warnings.rows.len(), 1, "{script}");
            assert_eq!(warnings.rows[0][1].get_str(), Some("test.worker_warning"));
            if result.is_ok() {
                client.abort().unwrap();
            }
            thread.join().unwrap();
        }
    }
}
