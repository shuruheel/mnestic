/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Governed transactions for Python (spec `docs/specs/python-governed-transactions.md`).
//!
//! The binding owns one named worker thread per open transaction (D2). That
//! thread runs the engine's `GovernedTransactionWorker` and never takes the
//! GIL. The blocking client sits in a mutex that is only locked after the GIL
//! has been released (D4); the cancellation handle lives beside it (D5).

use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pyo3::exceptions::{PyResourceWarning, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyLong};

use cozo::{DbInstance, GovernedTransaction, GovernedTransactionCancellation};

use crate::{
    binding_error, convert_params, named_rows_to_py, script_error, tx_closed_error, TX_CLOSED_CODE,
};

struct Entry {
    cancel: GovernedTransactionCancellation,
    write: bool,
}

/// The open governed transactions of one `CozoDbPy` handle. Entries are added
/// before the worker starts and removed when the worker thread exits, or
/// earlier when a commit or abort has succeeded (the storage transaction is
/// dropped before the engine replies).
#[derive(Default)]
pub(crate) struct Registry {
    next_id: AtomicU64,
    open: Mutex<HashMap<u64, Entry>>,
}

impl Registry {
    fn register(&self, cancel: GovernedTransactionCancellation, write: bool) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, Entry { cancel, write });
        id
    }

    fn remove(&self, id: u64) {
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }

    pub(crate) fn has_write(&self) -> bool {
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|e| e.write)
    }

    /// Cancel every open transaction (D2: `close()` ends them). Workers remove
    /// their own entries as they exit.
    pub(crate) fn cancel_all(&self) {
        for entry in self.open.lock().unwrap_or_else(|e| e.into_inner()).values() {
            entry.cancel.cancel();
        }
    }
}

/// Seconds argument → positive, representable `Duration` (D1). Never panics.
pub(crate) fn parse_seconds(name: &str, value: &Bound<'_, PyAny>) -> PyResult<Duration> {
    if value.is_instance_of::<PyBool>() {
        return Err(PyTypeError::new_err(format!(
            "{name} must be a number of seconds, not a bool"
        )));
    }
    let secs: f64 = value.extract()?;
    if !secs.is_finite() || secs <= 0.0 {
        return Err(PyValueError::new_err(format!(
            "{name} must be a finite number of seconds greater than zero, got {secs}"
        )));
    }
    let duration = Duration::try_from_secs_f64(secs)
        .map_err(|_| PyValueError::new_err(format!("{name} is too large: {secs} seconds")))?;
    if duration.is_zero() {
        return Err(PyValueError::new_err(format!(
            "{name} rounds to zero: {secs} seconds"
        )));
    }
    Ok(duration)
}

pub(crate) fn deadline_after(name: &str, duration: Duration) -> PyResult<Instant> {
    Instant::now().checked_add(duration).ok_or_else(|| {
        PyValueError::new_err(format!(
            "{name} is too large: {} seconds",
            duration.as_secs_f64()
        ))
    })
}

/// `mem_limit` → estimated bytes, an int ≥ 1 (D1).
pub(crate) fn parse_mem_limit(value: Option<&Bound<'_, PyAny>>) -> PyResult<Option<usize>> {
    let Some(value) = value else { return Ok(None) };
    if value.is_none() {
        return Ok(None);
    }
    if value.is_instance_of::<PyBool>() || !value.is_instance_of::<PyLong>() {
        return Err(PyValueError::new_err(format!(
            "mem_limit must be an int number of bytes ≥ 1, got {value}"
        )));
    }
    if value.lt(1)? {
        return Err(PyValueError::new_err(format!(
            "mem_limit must be ≥ 1, got {value}"
        )));
    }
    let bytes: usize = value
        .extract()
        .map_err(|_| PyValueError::new_err(format!("mem_limit is too large: {value}")))?;
    Ok(Some(bytes))
}

/// Open a governed transaction and start its worker thread (D1, D2).
pub(crate) fn open(
    py: Python<'_>,
    db: DbInstance,
    registry: &Arc<Registry>,
    write: bool,
    deadline: Instant,
    idle_timeout: Duration,
    mem_limit: Option<usize>,
) -> PyResult<GovernedTx> {
    let mut options = cozo::GovernedTransactionOptions::new(deadline);
    options.idle_timeout = idle_timeout;
    options.mem_limit = mem_limit;
    let (client, worker) = db
        .governed_transaction(write, options)
        .map_err(|e| script_error(py, e, None))?;
    let cancel = client.cancellation();
    let id = registry.register(cancel.clone(), write);
    let worker_registry = registry.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("mnestic-governed-tx-{id}"))
        .spawn(move || {
            worker.run();
            worker_registry.remove(id);
        });
    let handle = match spawned {
        Ok(handle) => handle,
        Err(e) => {
            // The unstarted worker was dropped with the closure; dropping the
            // client cancels. Nothing was opened.
            registry.remove(id);
            drop(client);
            return Err(binding_error(
                py,
                "transaction::spawn_failed",
                &format!("could not start the governed transaction thread: {e}"),
                "other",
                false,
            ));
        }
    };
    Ok(GovernedTx {
        client: Mutex::new(Some(client)),
        cancel,
        worker: Mutex::new(Some(handle)),
        registry: registry.clone(),
        id,
        failure: Mutex::new(None),
        explicit_end: AtomicBool::new(false),
    })
}

enum CommitOutcome {
    Closed,
    Committed,
    Failed(miette::Report, bool),
}

/// A governed transaction: one deadline for the whole transaction, a bounded
/// wait between statements, an optional per-statement memory limit, and
/// rollback on any error. Use as a context manager: a clean exit commits, an
/// exception aborts. Created by `CozoDbPy.governed_transact`.
#[pyclass(module = "mnestic")]
pub(crate) struct GovernedTx {
    /// Locked only inside `allow_threads`, never while holding the GIL (D4).
    client: Mutex<Option<GovernedTransaction>>,
    /// Beside the client mutex, never inside it (D5).
    cancel: GovernedTransactionCancellation,
    worker: Mutex<Option<JoinHandle<()>>>,
    registry: Arc<Registry>,
    id: u64,
    /// The error that ended the transaction; touched only with the GIL held.
    failure: Mutex<Option<PyErr>>,
    /// `commit()` or `abort()` was called explicitly: `__exit__` does nothing.
    explicit_end: AtomicBool,
}

impl GovernedTx {
    fn worker_finished(&self) -> bool {
        self.worker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_none_or(|h| h.is_finished())
    }

    /// After an engine error the worker exits at once (except when it is stuck
    /// past the deadline). Wait briefly for it so a SQLite handle is not
    /// refused (D9) by a transaction that has already ended. Call without the
    /// GIL.
    fn settle_after_failure(&self) {
        let until = Instant::now() + Duration::from_millis(200);
        while !self.worker_finished() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(1));
        }
        if self.worker_finished() {
            self.registry.remove(self.id);
        }
    }

    fn stored_failure(&self, py: Python<'_>) -> Option<PyErr> {
        self.failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|e| e.clone_ref(py))
    }

    fn record_failure(&self, py: Python<'_>, err: &PyErr) {
        let mut slot = self.failure.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(err.clone_ref(py));
        }
    }

    pub(crate) fn finish_commit(&self, py: Python<'_>) -> PyResult<()> {
        let outcome = py.allow_threads(|| {
            let taken = self.client.lock().unwrap_or_else(|e| e.into_inner()).take();
            match taken {
                None => CommitOutcome::Closed,
                Some(client) => {
                    // A worker that already exited cannot commit: its error is
                    // certainly not ambiguous.
                    let alive = !self.worker_finished();
                    match client.commit() {
                        Ok(()) => CommitOutcome::Committed,
                        Err(e) => {
                            self.settle_after_failure();
                            CommitOutcome::Failed(e, alive)
                        }
                    }
                }
            }
        });
        match outcome {
            CommitOutcome::Closed => Err(self
                .stored_failure(py)
                .unwrap_or_else(|| tx_closed_error(py))),
            CommitOutcome::Committed => {
                self.registry.remove(self.id);
                Ok(())
            }
            CommitOutcome::Failed(report, alive) => {
                let err = script_error(py, report, None);
                let value = err.value_bound(py);
                let code: Option<String> =
                    value.getattr("code").ok().and_then(|c| c.extract().ok());
                let kind: String = value
                    .getattr("kind")
                    .ok()
                    .and_then(|k| k.extract().ok())
                    .unwrap_or_default();
                let maybe_committed =
                    alive && kind != "conflict" && code.as_deref() != Some(TX_CLOSED_CODE);
                if maybe_committed {
                    let _ = value.setattr("maybe_committed", true);
                    let _ = value.setattr("retryable", false);
                }
                self.record_failure(py, &err);
                Err(err)
            }
        }
    }

    pub(crate) fn do_abort(&self, py: Python<'_>) -> PyResult<()> {
        let outcome = py.allow_threads(|| {
            let taken = self.client.lock().unwrap_or_else(|e| e.into_inner()).take();
            taken.map(|client| {
                let result = client.abort();
                if result.is_err() {
                    self.settle_after_failure();
                }
                result
            })
        });
        match outcome {
            None => Ok(()),
            Some(Ok(())) => {
                self.registry.remove(self.id);
                Ok(())
            }
            Some(Err(report)) => {
                let err = script_error(py, report, None);
                let code: Option<String> = err
                    .value_bound(py)
                    .getattr("code")
                    .ok()
                    .and_then(|c| c.extract().ok());
                // The transaction already ended (idle or deadline expiry, a
                // cancel, or a worker that is gone): nothing can commit, so
                // abort has done its job (D3).
                match code.as_deref() {
                    Some("eval::timeout") | Some("eval::killed") | Some(TX_CLOSED_CODE) => Ok(()),
                    _ => Err(err),
                }
            }
        }
    }

    /// Clean `with` exit and the retry helper's commit: does nothing after an
    /// explicit `commit()`/`abort()`, otherwise commits (raising a stored
    /// failure).
    pub(crate) fn exit_clean(&self, py: Python<'_>) -> PyResult<()> {
        if self.explicit_end.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.finish_commit(py)
    }

    /// Abort on an exception path; never raises. Returns the abort error, if
    /// any, so the caller can attach it to the original exception.
    pub(crate) fn exit_abort(&self, py: Python<'_>) -> Option<PyErr> {
        if self.explicit_end.swap(true, Ordering::SeqCst) {
            return None;
        }
        self.do_abort(py).err()
    }
}

#[pymethods]
impl GovernedTx {
    /// Run one query in the transaction. Returns the same shape as
    /// `CozoDbPy.run_script`. Any engine error rolls the transaction back;
    /// later calls raise `transaction::closed`.
    #[pyo3(signature = (query, params=None))]
    fn run_script(
        &self,
        py: Python<'_>,
        query: &str,
        params: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<PyObject> {
        // Argument errors raise here, before the engine sees the call, and
        // leave the transaction open (D6).
        let params = match params {
            Some(p) => convert_params(p.as_gil_ref())?,
            None => Default::default(),
        };
        let outcome = py.allow_threads(|| {
            let mut guard = self.client.lock().unwrap_or_else(|e| e.into_inner());
            let client = guard.as_mut()?;
            let result = client.run_script(query, params);
            if result.is_err() {
                // The engine client is finished; dropping it cancels.
                *guard = None;
                drop(guard);
                self.settle_after_failure();
            }
            Some(result)
        });
        match outcome {
            None => Err(tx_closed_error(py)),
            Some(Ok(rows)) => Ok(named_rows_to_py(rows, py)),
            Some(Err(report)) => {
                let err = script_error(py, report, Some(query));
                self.record_failure(py, &err);
                Err(err)
            }
        }
    }

    /// Commit. Raises the transaction's error if it already failed, and
    /// `transaction::closed` after a commit or abort. An error with
    /// `maybe_committed` means the outcome is unknown.
    fn commit(&self, py: Python<'_>) -> PyResult<()> {
        self.explicit_end.store(true, Ordering::SeqCst);
        self.finish_commit(py)
    }

    /// Roll back. Does nothing on a transaction that already ended.
    fn abort(&self, py: Python<'_>) -> PyResult<()> {
        self.explicit_end.store(true, Ordering::SeqCst);
        self.do_abort(py)
    }

    /// Request rollback from any thread without waiting for a running
    /// statement. A blocked `run_script` raises `eval::killed` at the worker's
    /// next cancellation check. Idempotent. A cancel racing `commit()` does not
    /// guarantee rollback.
    fn cancel(&self) {
        self.cancel.cancel();
    }

    /// True until the transaction commits, aborts or fails.
    #[getter]
    fn is_open(&self, py: Python<'_>) -> bool {
        py.allow_threads(|| {
            self.client
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some()
        }) && !self.worker_finished()
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    #[pyo3(signature = (exc_type=None, exc=None, _tb=None))]
    fn __exit__(
        &self,
        py: Python<'_>,
        exc_type: Option<&Bound<'_, PyAny>>,
        exc: Option<&Bound<'_, PyAny>>,
        _tb: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        let raised = exc_type.is_some_and(|t| !t.is_none());
        if !raised {
            self.exit_clean(py)?;
            return Ok(false);
        }
        // Abort, suppress any abort error, keep the original exception (D3).
        if let Some(abort_err) = self.exit_abort(py) {
            if let Some(exc) = exc {
                if exc.hasattr("add_note").unwrap_or(false) {
                    let note = format!("mnestic: the abort that followed also failed: {abort_err}");
                    let _ = exc.call_method1("add_note", (note,));
                }
            }
        }
        Ok(false)
    }
}

impl Drop for GovernedTx {
    fn drop(&mut self) {
        let open = self.client.get_mut().map(|c| c.is_some()).unwrap_or(false);
        if open {
            // Dropping the client below cancels the worker, which rolls back.
            Python::with_gil(|py| {
                let category = py.get_type_bound::<PyResourceWarning>();
                let _ = PyErr::warn_bound(
                    py,
                    &category,
                    "mnestic: governed transaction dropped without commit() or abort(); it was rolled back",
                    1,
                );
            });
        }
    }
}

/// A uniform sample in [0, 1) without a new dependency.
fn jitter() -> f64 {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    (hasher.finish() >> 11) as f64 / (1u64 << 53) as f64
}

fn flag(err: &PyErr, py: Python<'_>, name: &str) -> bool {
    err.value_bound(py)
        .getattr(name)
        .and_then(|v| v.is_truthy())
        .unwrap_or(false)
}

/// D8: run `func(tx)` in a fresh governed transaction and commit; retry
/// `retryable` errors with jittered exponential backoff (50 ms doubling)
/// until `max_attempts` or the overall deadline. Never retries an error with
/// `maybe_committed`. The last error is raised unchanged.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_governed(
    py: Python<'_>,
    open_tx: &dyn Fn(Python<'_>, Instant) -> PyResult<GovernedTx>,
    func: &Bound<'_, PyAny>,
    deadline: Instant,
    max_attempts: u32,
) -> PyResult<PyObject> {
    let mut attempt = 0u32;
    let mut backoff = 0.05f64;
    loop {
        attempt += 1;
        let tx = Py::new(py, open_tx(py, deadline)?)?;
        let err = match func.call1((tx.clone_ref(py),)) {
            Ok(value) => match tx.borrow(py).exit_clean(py) {
                Ok(()) => return Ok(value.unbind()),
                Err(e) => e,
            },
            Err(e) => {
                let _ = tx.borrow(py).exit_abort(py);
                e
            }
        };
        drop(tx);
        if flag(&err, py, "maybe_committed") || !flag(&err, py, "retryable") {
            return Err(err);
        }
        if attempt >= max_attempts {
            return Err(err);
        }
        let pause = Duration::from_secs_f64(backoff * (0.5 + 0.5 * jitter()));
        backoff *= 2.0;
        match Instant::now().checked_add(pause) {
            Some(resume) if resume < deadline => {}
            _ => return Err(err),
        }
        py.allow_threads(|| std::thread::sleep(pause));
        py.check_signals()?;
    }
}
