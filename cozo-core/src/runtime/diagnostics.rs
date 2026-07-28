/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Structured query diagnostics (mnestic fork).
//!
//! The engine has long *known* things worth telling the query author — a rule
//! that still contains a Cartesian step after reordering, an import that
//! strands FTS/HNSW/LSH indexes, PageRank stopping unconverged, a graph
//! projection too big for its cache — and said them only through `log::warn!`,
//! where nothing programmatic can see them. A downstream project shipped a
//! full-scan traversal for months while the engine warned about it on every
//! call. This module gives those warnings a **stable, typed surface**:
//!
//! - [`emit`] both logs (behavior unchanged) and records a [`QueryWarning`]
//!   into a thread-local sink; query evaluation runs on the calling thread, so
//!   the sink needs no locking and parallel queries cannot interleave.
//! - `Db::flush_warnings` (called at the end of script runs and imports)
//!   drains the sink into a bounded per-`Db` ring buffer.
//! - `::warnings` / `::warnings clear` expose the ring through plain
//!   CozoScript — reachable from every binding with no FFI change — and
//!   `Db::recent_warnings` serves embedded Rust callers.
//!
//! Codes are dotted, stable identifiers (e.g. `query.cartesian_step`); the
//! `hint` is the actionable half an agent is expected to act on.

use std::cell::RefCell;

/// One structured warning. `code` is a stable identifier tests and agent
/// frameworks may match on; `message` is the human-readable finding (the same
/// text that reaches the log); `hint` says what to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryWarning {
    pub code: &'static str,
    pub message: String,
    pub hint: String,
}

thread_local! {
    static SINK: RefCell<Vec<QueryWarning>> = const { RefCell::new(Vec::new()) };
}

/// Log the warning (exactly as before) AND record it for the structured
/// surface. The log side keeps every existing consumer working; the sink side
/// is what `::warnings` reads.
pub(crate) fn emit(code: &'static str, message: String, hint: impl Into<String>) {
    log::warn!("{message}");
    SINK.with(|s| {
        s.borrow_mut().push(QueryWarning {
            code,
            message,
            hint: hint.into(),
        })
    });
}

/// Take everything the current thread has emitted since the last drain.
pub(crate) fn drain() -> Vec<QueryWarning> {
    SINK.with(|s| std::mem::take(&mut *s.borrow_mut()))
}
