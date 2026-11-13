# Governed transactions from Python

Status: **DRAFT — all review passes run (0–5), awaiting owner approval.** Selected as roadmap item 2
by the owner on November 13, 2026. Passes (workspace `spec-authoring` method):

| Pass | Date | Result |
|---|---|---|
| 1 Code facts | Nov 13 | Verified against `main` `fd0d3d0d` and the 0.19.2 wheel |
| 2 Draft | Nov 13 | Merged as `23cae8a4` (PR #73) |
| 3 Panel (code grounding and API adversary) | Nov 13 | 9 findings, 0 blockers; all folded. Changes: cancellation handle outside the client lock (D5), SQLite store-lock behaviour stated (D2, residual risks), `__exit__` error handling (D3), overflow-safe validation (D1), binding-made `transaction::closed` (D6), `close()` behaviour decided (D2) |
| 4 Prior art (PEP 249/sqlite3, psycopg 3, SQLAlchemy, DuckDB, FoundationDB, Spanner, CockroachDB, PostgreSQL, PyO3) | Nov 13 | 5 confirmed, 2 gaps, 2 contradictions, all folded: `maybe_committed` flag (D6), `close()` cancels open transactions (D2), idle-limit interaction and guidance (D1), GIL rules (D4), retry helper (D8, the essential omission) |
| 5 Simplification | Nov 13 | See *Simplification* below |

Pass 0 does not apply (no user interface).

## Goal

Expose the engine's [governed multi-transactions](governed-transactions.md) in
the Python package: one deadline for the whole transaction, a bounded wait
between statements, a per-statement memory limit, and rollback on any error.
No engine change.

## Verified current state

| Claim | Where | Consequence |
|---|---|---|
| `DbInstance::governed_transaction(write, options)` returns a client and a worker; the host must run `worker.run()` on a dedicated thread. Limits are captured when the pair is made; the Db default timeout can only tighten them | `cozo-core/src/lib.rs:1085-1104`; `runtime/governed_transaction.rs:151-205`; [contract](governed-transactions.md#budgets-and-errors) | The binding owns the thread; deadline counts from the call |
| Options: absolute `Instant` deadline (required), idle limit (default 5 s; a zero duration is rejected with a plain engine error), optional per-statement memory limit | `runtime/governed_transaction.rs:27-45`, `:168-170` | Python passes seconds and bytes |
| The client has `run_script(&mut self, …)`, consuming `commit`/`abort`, and a separate cloneable cancellation handle; dropping the client cancels | `runtime/governed_transaction.rs:48-63`, `:77-102`, `:141-145` | The cancellation handle can live outside any lock |
| The client waits only on the result channel until the deadline; it does not watch the cancel flag. After the deadline it reports `eval::timeout` even if cancelled | `runtime/governed_transaction.rs:104-119`, `:121-138` | `cancel()` is prompt only where the worker checks for it |
| After `commit`/`abort` the client is consumed; the engine's `closed()` diagnostic is private | `runtime/governed_transaction.rs:95-102`, `:282-288` | The binding makes its own `transaction::closed` error |
| On SQLite a governed transaction holds an in-process store lock for its whole life (write: exclusive; read: shared, blocking writers); opening the transaction blocks on that lock with no timeout | `storage/sqlite.rs:183-208`; `runtime/db.rs:846-851` | Other queries on the same Db wait; the worker can be parked before its deadline applies |
| The worker owns its own `DbInstance` clone; `CozoDbPy.close()` only drops the binding's handle | `runtime/governed_transaction.rs:151-160`, `:187-199`; `cozo-lib-python/src/lib.rs:906-908` | Without a change, an open transaction would survive `close()`; D2 cancels it |
| Python has only `multi_transact(write)` over the legacy API: no deadline, idle or memory limit, errors do not end the transaction; its `commit`/`abort` raise a plain `Exception` | `cozo-lib-python/src/lib.rs:909-914`, `:918-935` | The gap; new API should be uniform |
| `MnesticError` kinds: `timeout` for `eval::killed` or codes containing `timeout`; `limit` for codes containing `limit` or `budget`; `conflict` (retryable) from lock and busy messages. Its docstring defines `retryable` as "rerunning the same call can succeed" | `cozo-lib-python/src/lib.rs:35-76` | Deadline and memory aborts classify correctly; docstring needs a governed clause |
| The existing numeric check tests finiteness and sign, raises plain `Exception`, and converts with `Duration::from_secs_f64`, which panics on overflow | `cozo-lib-python/src/lib.rs:844-853` | Not a pattern to copy as is (D1) |

## Decisions

**D1 — API.**

```python
tx = db.governed_transact(write, timeout=30.0, idle_timeout=5.0, mem_limit=None)
rows = tx.run_script("?[k, v] := *kv{k, v}", {})
tx.commit()          # or tx.abort()
```

`timeout` is required, in seconds; the absolute deadline is fixed at the call,
so time before the first statement counts, as FoundationDB measures its
timeout from transaction creation. Requiring it departs from PostgreSQL's
`transaction_timeout` and FoundationDB, where the default is disabled: an
open transaction here holds a snapshot, and on SQLite the store lock. `idle_timeout` is seconds between
statements; `mem_limit` is estimated bytes per statement, a Python `int ≥ 1`.
Validation raises `ValueError` explicitly: non-finite or non-positive values,
values `Duration::try_from_secs_f64` rejects, deadlines `Instant::checked_add`
cannot represent, and an `idle_timeout` that rounds to zero. An `idle_timeout` at or
above `timeout` is accepted and has no effect, as PostgreSQL ignores the longer
of its two limits. `run_script` returns the same shape as `CozoDbPy.run_script`. Seam: a `#[pyclass]
GovernedTx` and one `CozoDbPy` method in `cozo-lib-python/src/lib.rs`.

**D2 — The binding owns the worker thread.** `governed_transact` spawns one
named thread with `std::thread::Builder` running `worker.run()`; a spawn
failure raises `MnesticError` (the unstarted worker and the client are dropped,
which is safe and cancels). The worker never takes the GIL or touches a Python
object. `CozoDbPy` keeps the cancellation handles of its open governed
transactions; `db.close()` cancels them, so closing the database ends its
transactions as `sqlite3`, psycopg and SQLAlchemy do, and a RocksDB path is
released once their workers exit. Python applications bound concurrency by how
many transactions they open; the binding adds no pool or limit.

**D3 — Context manager.**

```python
with db.governed_transact(True, timeout=30.0) as tx:
    tx.run_script(":put kv {k, v}", {...})
```

- Clean exit commits. If the transaction already ended (an error caught inside
  the block, idle or deadline expiry), the commit raises that error.
- Exit on exception aborts and suppresses any error from the abort, so the
  original exception propagates unchanged (SQLAlchemy's order); on Python
  3.11+ the suppressed error is attached with `add_note`.
- After an explicit `commit()` or `abort()`, exit does nothing.
- `abort()` on a transaction that already ended, including after a
  successful `commit()`, returns without error, as `sqlite3`'s `rollback()`
  does with no transaction open.
- `commit()` on a transaction that already failed raises its error, as
  psycopg does; DuckDB's silent rollback on such a commit is the pitfall
  avoided.
- An object dropped without commit or abort rolls back and emits a
  `ResourceWarning`. Drop timing depends on the interpreter (reference cycles,
  PyPy, shutdown); the README says not to rely on it.

**D4 — One caller at a time.** The client sits in a `Mutex<Option<_>>` that
is locked only after the GIL is released (inside `allow_threads`), never while
holding it, which avoids the GIL/mutex lock-order deadlock the PyO3 guide
warns about. A second thread calling the same transaction waits, at most until
the deadline.

**D5 — `cancel()`.** The binding clones the engine's cancellation handle at
construction and keeps it beside the mutex, never inside it, so `cancel()`
never waits for a running statement. A blocked `run_script` then raises
`eval::killed` as soon as the worker reaches a cancellation check (evaluation
steps, `:sleep`, relation-lock waits). If the worker is inside storage or host
code, the call stays blocked until the deadline and raises `eval::timeout`.
`cancel()` is idempotent and does nothing on an ended transaction. A cancel
that races `commit()` never guarantees rollback (FoundationDB and psycopg say
the same); the commit error carries `maybe_committed` (D6).

**D6 — Errors.** Engine errors raise `MnesticError` through `script_error`.
Calls on a transaction that has ended raise a binding-made `MnesticError` with
`code == "transaction::closed"`. Argument errors (bad `params`) raise before
the engine sees the call and leave the transaction open. No new `kind`: a
deadline or idle expiry is `timeout`, a memory abort is `limit`, a lock
conflict is `conflict`.

Every error from a governed transaction also has `maybe_committed`:
- `False` for any error from `run_script`, from `abort()`, from validation, and
  for `transaction::closed`: nothing was committed.
- `True` for a `timeout`, `killed` or storage error raised by `commit()`: the
  storage commit may have completed.

`retryable` is `True` only when the transaction certainly did not commit and
rerunning it can succeed (a conflict), so it is never `True` together with
`maybe_committed`. This follows FoundationDB (`retryable_not_committed`,
`maybe_committed`), CockroachDB (40001 retry versus 40003 ambiguous result) and
Spanner, which all separate the two; a single flag would either invite double
application or hide a safe retry. The `MnesticError` docstring gains both
meanings: for governed transactions `retryable` refers to the whole
transaction; `maybe_committed` is `False` on every non-governed error.

**D7 — Legacy API unchanged.** `multi_transact` keeps its behaviour and
exception types; `db.close()` does not cancel it.

**D8 — Retry helper.**

```python
result = db.run_governed(fn, write=True, timeout=30.0, max_attempts=3)
```

Calls `fn(tx)` in a fresh governed transaction and commits. On an error with
`retryable` it sleeps with jittered exponential backoff (50 ms doubling) and
retries, until `max_attempts` or the overall `timeout`, which spans all
attempts; it never retries an error with `maybe_committed`. The last error is
raised unchanged. `fn` must be safe to run more than once: effects outside the
transaction repeat. FoundationDB's `@fdb.transactional`, Spanner's
`run_in_transaction` and CockroachDB's documented client loop ship the same
loop because hand-written ones retry ambiguous commits or never stop.

## Invariants

1. No governed transaction exists without a deadline.
2. After any engine error, every later call raises `transaction::closed` and
   nothing the transaction wrote is committed.
3. Every blocking call releases the GIL.
4. One worker thread per open transaction, which never takes the GIL. Once it holds its storage
   transaction it exits no later than the deadline plus cooperative-cancellation
   latency; before that, on SQLite, it can wait as long as another in-process
   transaction holds the store lock.
5. Engine diagnostic codes reach Python unchanged.
6. `retryable` and `maybe_committed` are never both true.

## Acceptance tests

`cozo-lib-python/tests`, on SQLite and, where the wheel has it, RocksDB.

- Commit persists; abort and object drop do not.
- A failing statement rolls back earlier writes; the next call raises
  `transaction::closed`; `abort()` then returns without error.
- Deadline expiry mid-statement and between statements, and idle expiry:
  `kind == "timeout"`, `code == "eval::timeout"`.
- `mem_limit` exceeded: `kind == "limit"`, `code == "eval::mem_budget_exceeded"`;
  `:mem_limit` in the script only tightens it.
- Read-only transaction rejects a write.
- Context manager: commits on clean exit; aborts on exception and re-raises the
  original exception, including when the exception is the transaction's own
  failure; clean exit after a caught error raises it; no double commit.
- `cancel()` from a second thread, while a statement runs a long evaluation or
  `:sleep`, raises `eval::killed` promptly.
- GIL release: a read-only governed transaction blocked in a statement while
  another thread completes an ordinary read (on SQLite a governed **write**
  blocks other queries, so the write case runs on RocksDB only).
- SQLite: a held legacy write transaction plus a governed transaction with a
  short deadline returns `eval::timeout` to the caller.
- `db.close()` with an open governed transaction returns promptly and cancels
  it: its next call raises, nothing commits, and the RocksDB path can be
  reopened once the worker exits. An open `multi_transact` is unaffected.
- `maybe_committed`: false on `run_script` errors and conflicts; true on a
  commit interrupted by deadline (injected with the engine's `test-hooks`
  commit seam); never with `retryable`.
- `run_governed`: retries an injected conflict and succeeds; stops at
  `max_attempts` and at the overall timeout with the last error; does not
  retry a `maybe_committed` error.
- Two Python threads using one transaction concurrently: no deadlock; calls
  serialize.
- A dropped, uncommitted transaction emits `ResourceWarning`.
- Invalid `timeout`, `idle_timeout`, `mem_limit`, including `1e20` and `1e-12`,
  raise `ValueError`, never `PanicException`.
- `multi_transact` behaviour is unchanged.

## Residual risks

- **On SQLite a governed write transaction blocks every other query on the same
  Db**, including queries issued from inside its own `with` block. Such a query
  waits until the transaction's idle limit rolls it back, and the clean exit
  then raises `eval::timeout`. The README must say this; RocksDB does not have
  it.
- **The 5 s default idle limit** rolls back a block that pauses longer between
  statements, for example to call an external API or a model. The error
  surfaces at the next call or at commit as `eval::timeout`. The README says
  to pass a longer `idle_timeout` for such blocks, or do slow work before
  opening the transaction.
- **Threads are unbounded by the binding.** Each open transaction holds a
  thread and a snapshot (and on SQLite the store lock) until it ends.
- **Cancellation is cooperative.** A statement inside storage or a custom fixed
  rule can outlive the deadline; the Python call returns, the worker later.
- **Commit outcome can be unknown** after a timeout or cancel that races the
  storage commit.

## Layering ruling

Python binding only. No engine change, no MindGraph change. The LangGraph,
LangChain and LlamaIndex integrations do not adopt it in this release.

## Rejected alternatives

- **Changing `multi_transact` to be governed.** Different error semantics would
  break existing callers.
- **Explicit commit required inside `with`.** Diverges from the standard
  library's `sqlite3`, where a clean exit commits.
- **Transactions surviving `db.close()`** (the post-panel draft, for parity
  with `multi_transact`). Prior art: closing the handle ends its transactions
  in `sqlite3`, psycopg and SQLAlchemy; a leaked transaction keeping a RocksDB
  path locked after `close()` would surprise notebook and test users.
- **An asyncio API.** Callers can run the blocking calls in an executor; a
  native async surface needs its own cancellation design.
- **A new `kind` for closed transactions.** The code is available to callers.
- **A thread pool or admission limit in the binding.** The engine contract
  forbids parking transactions on a shared pool; a limit is application policy.
- **One `retryable` flag covering ambiguous commits** (the first draft).
  Contradicted by every retrying system surveyed (D6).
- **A `force_rollback` option** (psycopg) for dry-run blocks. No consumer;
  `abort()` inside the block does the same.

## Batches (review units)

One batch: binding, retry helper, tests, README section (SQLite blocking,
idle limit, drop timing), `MnesticError` docstring, changelog entry. Ships as a patch
release; crate and Python versions move together although the crate's code
does not change.

## Resolved questions

1. *Clean `with` exit: commit or abort?* Commit — `sqlite3`, psycopg 3 and
   SQLAlchemy all commit on clean exit.
2. *An `outcome_unknown` flag?* Yes, as `maybe_committed` (D6).

## Open questions (owner)

1. Should the binding refuse ordinary queries on a SQLite Db while a governed
   write transaction from the same Db is open, rather than letting them wait?

## Simplification

Requirement-neutral cuts, applied:
- `force_rollback` not added (rejected alternatives).
- The retry helper takes no custom backoff or retry predicate; one fixed
  policy, `max_attempts` and the shared timeout only.
- No new `kind` values; `maybe_committed` is one attribute, `False` on every
  non-governed error rather than absent, so callers need no `hasattr`.

Requirement changes, **not** applied (owner decision):
- **Drop the retry helper (D8) from this batch.** It is the prior-art pass's
  essential omission, and small; keeping it is recommended. Dropping it would
  leave the new flags without the loop that makes them safe to use.
