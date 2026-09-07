# Governed multi-transactions

Status: implemented in the B-L0 engine branch; unreleased. The legacy
`multi_transaction` and `run_multi_transaction` APIs remain compatible.

## Host API and ownership

On native targets, `DbInstance::governed_transaction(write, options)` returns
`(GovernedTransaction, GovernedTransactionWorker)`. The worker opens the storage
transaction only when the host calls `worker.run()` on its dedicated blocking
thread. Moving the worker does not start an engine-owned background thread.

The host must acquire admission once, then move its permit and database owner
into the actual worker closure. It must keep them until that closure exits,
including when an async waiter is cancelled. Do not park a transaction on
Rayon's shared query pool. For example, the ownership boundary can be:

```rust
fn run_admitted<P: Send + 'static>(
    worker: cozo::GovernedTransactionWorker,
    permit: P,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        worker.run();
        drop(permit);
    })
}
```

The client has private channels, mutable query methods and consuming
`commit`/`abort` methods. Each command/result channel holds at most one item.
Response delivery never blocks the worker. Dropping the client cancels the
worker; `client.cancellation()` supplies a separate, cloneable signal for hosts
whose client is inside a blocking call. Cancellation wakes inter-query waiting
and propagates into every query's poison without allowing ordinary per-query
cleanup to kill the parent transaction.

## Budgets and errors

`GovernedTransactionOptions::new(request_deadline)` requires an absolute
`Instant`. Its default inter-query idle limit is five seconds; zero is rejected.
The optional `mem_limit` is estimated bytes per statement.

- Capture the minimum of the supplied deadline and the Db default timeout when
  preparing the pair, before worker scheduling. Never re-arm it between queries.
- Copy the minimum of the per-call and Db memory limits onto the storage
  transaction. Each query's `:mem_limit` and `:timeout` can tighten its limits.
  Triggers inherit the transaction's limits and cancellation signal.
- Bound command waiting by the earlier of the absolute deadline and idle limit.
  Bound `:sleep` and relation-lock polling by the transaction deadline too.
- Flush the worker's structured warnings after every query, including parser,
  compiler and evaluation errors. `::warnings` remains a separate Db operation.
  Reads and clears access the in-memory ring directly, so they can inspect an
  active read or write transaction without waiting on its storage locks.
- Reject write queries in read-only mode. As with the legacy API, each command
  is one query program; imperative scripts and system commands are unsupported.
- **Every query error terminates and rolls back the governed transaction.**
  This stricter contract prevents committing earlier mutations after a failure.
  The first error retains the original diagnostic, including
  `eval::mem_budget_exceeded`, `eval::timeout` and `eval::killed`. A late command
  after idle expiry still receives `eval::timeout`, rather than a channel error.
  Subsequent use of an already failed client returns `transaction::closed`.
- Explicit abort drops the storage transaction before acknowledging completion.
  Commit errors propagate and publish no change-feed events. A successful commit
  is acknowledged only after the storage commit returns successfully.

Deadline and cancellation checks are cooperative. They do not preempt blocking
storage calls or arbitrary host fixed-rule code. The client can time out while
that code is still running; admission must remain with the worker. A cancellation
or timeout concurrent with a storage commit does **not** prove rollback. Hosts
must reconcile uncertain write outcomes using their idempotency contract.

Statement accounting is not a total RSS bound, nor a bound on retained transaction
state or application candidates across queries. MindGraph additionally requires
its own candidate-carrier bound and process admission; this API does not activate
those controls.

## Snapshots and callbacks

RocksDB readers pin a snapshot while other writers can commit. The current
SQLite backend holds a store-wide read guard: it preserves the snapshot by
holding concurrent writers until the reader closes. Both therefore give stable
reads, with different concurrency costs. The governed deadline and idle limit
also apply to these read transactions.

After a successful commit, callback delivery uses the remaining transaction
deadline. A bounded subscriber that stalls past it is disconnected and emits
`callback.delivery_timeout`, with a resubscribe/reconcile hint. Already committed
data stays committed. An unavailable consumer cannot retain the worker forever;
a disconnected change feed must reconcile against stored data. Legacy callback
delivery retains its existing behavior.

The channel choices use Crossbeam's documented
[bounded channels](https://docs.rs/crossbeam-channel/0.5.15/crossbeam_channel/fn.bounded.html),
[deadline sends](https://docs.rs/crossbeam-channel/0.5.15/crossbeam_channel/struct.Sender.html#method.send_deadline)
and nonblocking response delivery. The terminal diagnostic is published before
the worker disconnects its response channel, so connection teardown cannot erase
an already recorded query error.

## Acceptance and downstream gate

`cozo-core/tests/governed_transaction.rs` exercises memory/SQLite/RocksDB read-only
rejection, successful writes, abort, parse-failure rollback, all three memory
limit sources, deadlines, idle expiry and disconnect rollback. SQLite and RocksDB
have distinct concurrent-writer snapshot tests. Additional tests hold a host
fixed rule beyond client cancellation to prove actual worker permit ownership,
kill an active query after prior writes, inject commit failure and check the
absence of callbacks, and stall a callback subscriber after commit. The library
suite checks warning flushes on success, parse failure and evaluation failure.

CI runs the new integration suite in both the default/SQLite job and the RocksDB
job, including the commit-failure seam with `test-hooks`. Downstream MindGraph
must use a released or explicit immutable compatibility pin after engine
validation, migrate its existing mutable transaction callers as well as new read
snapshots, and complete host admission tests before governance activation.
