# Governed transactions from Python

Status: **DRAFT — not reviewed, not approved.** Selected as roadmap item 2 by the
owner on November 13, 2026. Passes run (workspace `spec-authoring` method): 1 (code
facts, verified against `main` at `fd0d3d0d` and the published 0.19.2 wheel on
November 13, 2026) and 2 (this draft). Pass 0 does not apply (no user interface).
Passes 3–5 (adversarial panel, prior-art grounding, simplification) have **not**
run.

## Goal

Expose the engine's [governed multi-transactions](governed-transactions.md) in
the Python package: one deadline for the whole transaction, a bounded wait
between statements, a per-statement memory limit, and rollback on any error.
No engine change.

## Verified current state

| Claim | Where | Consequence |
|---|---|---|
| `DbInstance::governed_transaction(write, options)` returns a client and a worker; the host must run `worker.run()` on a dedicated thread | `cozo-core/src/lib.rs:1085-1104`; `runtime/governed_transaction.rs:151-205` | The binding must own that thread |
| Options are an absolute `Instant` deadline (required), an idle limit (default 5 s, zero rejected) and an optional per-statement memory limit; database defaults only tighten them | `runtime/governed_transaction.rs:27-45`; [contract](governed-transactions.md#budgets-and-errors) | Python passes seconds and bytes |
| The client exposes `run_script(&mut self, …)`, consuming `commit` and `abort`, and a cloneable cancellation signal; dropping it cancels the worker | `runtime/governed_transaction.rs:77-102`, `:141-145` | Needs interior mutability behind a Python object |
| Terminal diagnostics: `eval::timeout`, `eval::killed`, `transaction::closed`; a query's own error (including `eval::mem_budget_exceeded`) is kept as the first error | `runtime/governed_transaction.rs:269`, `:277`, `:285`; contract | Codes to surface unchanged |
| Python has only `multi_transact(write)`, wrapping the legacy API: no deadline, no idle limit, no memory limit, errors do not end the transaction | `cozo-lib-python/src/lib.rs:909-914`, `:529`; 0.19.2 wheel exposes `multi_transact` with `abort`, `commit`, `run_script` | The gap |
| Legacy `commit` and `abort` raise a plain `Exception` with a string; `run_script` raises `MnesticError` | `cozo-lib-python/src/lib.rs:918-935` | New API should be uniform |
| `MnesticError` kinds: `timeout` for `eval::killed` or any code containing `timeout`; `limit` for codes containing `limit` or `budget`; `conflict` (retryable) from lock and busy messages | `cozo-lib-python/src/lib.rs:49-76` | Deadline and memory aborts already classify correctly; `transaction::closed` is `other` |
| Blocking engine calls release the GIL; numeric arguments are validated as finite and positive | `cozo-lib-python/src/lib.rs:579`; `:844-850` | Patterns to follow |

## Decisions

**D1 — API.**

```python
tx = db.governed_transact(write, timeout=30.0, idle_timeout=5.0, mem_limit=None)
rows = tx.run_script("?[k, v] := *kv{k, v}", {})
tx.commit()          # or tx.abort()
```

`timeout` is required, in seconds; the absolute deadline is fixed when
`governed_transact` is called, so time spent before the first statement counts.
`idle_timeout` is seconds between statements; `mem_limit` is estimated bytes
per statement. Non-finite or non-positive values raise `ValueError`.
`run_script` returns the same shape as `CozoDbPy.run_script`. Seam: a new
`#[pyclass] GovernedTx` and one method on `CozoDbPy` in
`cozo-lib-python/src/lib.rs`.

**D2 — The binding owns the worker thread.** `governed_transact` spawns one
named `std::thread` that runs `worker.run()` and exits when the transaction
ends, as the legacy API already does (`cozo-core/src/lib.rs:1123`). The worker
holds its own database handle, so `db.close()` returns immediately and storage
is released when the worker exits, at the latest at the deadline. The engine
contract asks hosts to bound admission; Python applications bound it by how
many transactions they open. No pool, no limit in the binding.

**D3 — Context manager.**

```python
with db.governed_transact(True, timeout=30.0) as tx:
    tx.run_script(":put kv {k, v}", {...})
```

A clean exit commits; an exception aborts and propagates. Leaving the block
after an explicit `commit()` or `abort()` does nothing. An object dropped
without either rolls back (the engine's drop behaviour).

**D4 — One caller at a time.** The client sits in a mutex taken with the GIL
released. A second thread calling the same transaction waits, at most until
the deadline. Calls after the transaction has ended raise
`transaction::closed`.

**D5 — `cancel()`.** Callable from any thread, including while another thread
is blocked in `run_script`; it triggers the engine's cooperative cancellation
and the blocked call raises `eval::killed`.

**D6 — Errors.** All three methods raise `MnesticError` through the existing
`script_error` path. No new `kind` values: a deadline or idle expiry is
`timeout`, a memory abort is `limit`, a lock conflict is `conflict`. For this
API `retryable` means the **whole transaction** can be rerun, since any error
ends it. Documented, not encoded: an error raised from `commit()` with kind
`timeout` does not prove rollback; callers reconcile with their own
idempotency key, as the engine contract states.

**D7 — Legacy API unchanged.** `multi_transact` keeps its behaviour and its
exception types.

## Invariants

1. No governed transaction exists without a deadline.
2. After any error, every later call on that object raises
   `transaction::closed` and nothing it wrote is committed.
3. Every blocking call releases the GIL.
4. One worker thread per open transaction; it exits no later than the deadline
   plus the engine's cooperative-cancellation latency.
5. Diagnostic codes from the engine reach Python unchanged.

## Acceptance tests

`cozo-lib-python/tests`, on SQLite and, where the wheel has it, RocksDB.

- Commit persists; abort and object drop do not.
- A failing statement rolls back earlier writes; the next call raises
  `transaction::closed`.
- Deadline expiry mid-statement and between statements: `kind == "timeout"`,
  `code == "eval::timeout"`. Idle expiry the same.
- `mem_limit` exceeded: `kind == "limit"`, `code == "eval::mem_budget_exceeded"`;
  a `:mem_limit` in the script can only tighten it.
- Read-only transaction rejects a write.
- Context manager: commits on clean exit, aborts on exception, no double commit.
- `cancel()` from a second thread unblocks a running statement with
  `eval::killed`.
- While one thread is blocked in a governed statement, another thread
  completes an ordinary query (GIL released).
- `db.close()` with an open transaction returns promptly; the worker exits by
  the deadline.
- Invalid `timeout`, `idle_timeout`, `mem_limit` raise `ValueError`.
- `multi_transact` behaviour is unchanged.

## Residual risks

- **Threads are unbounded by the binding.** An application that opens
  transactions without limit creates that many threads, each holding a
  snapshot (and on SQLite, blocking writers) until its deadline.
- **Cancellation is cooperative.** A statement inside storage or a custom
  fixed rule can outlive the deadline; the Python call returns, the worker
  finishes later.
- **Commit outcome can be unknown** after a timeout or cancel that races the
  storage commit.

## Layering ruling

Python binding only. No engine change, no MindGraph change. The LangGraph,
LangChain and LlamaIndex integrations do not adopt it in this release.

## Rejected alternatives

- **Changing `multi_transact` to be governed.** Different error semantics
  (any error ends the transaction) would break existing callers.
- **Explicit commit required inside `with`.** Diverges from the standard
  library's `sqlite3` and common driver behaviour, where a clean exit commits.
- **An asyncio API.** Callers can run the blocking calls in an executor; a
  native async surface needs its own cancellation design.
- **A new `kind` for closed transactions.** The contract already says any
  error ends the transaction; the code is available for callers who need it.
- **A thread pool or admission limit in the binding.** The engine contract
  forbids parking transactions on a shared pool, and a limit is application
  policy.
- **An `outcome_unknown` attribute on commit errors.** Considered for the
  commit race; left to documentation unless review asks for it.

## Batches (review units)

One batch: binding, tests, README section and changelog entry. Ships as a
patch release; crate and Python versions move together although the crate's
code does not change.

## Open questions for review

1. Should a clean `with` exit commit, or abort unless committed explicitly?
2. Is an `outcome_unknown` flag on commit errors worth adding now?
3. Does holding the client mutex across a statement create any path where a
   Python-side `cancel()` cannot reach a blocked call?
