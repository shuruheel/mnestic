# mnestic

**mnestic** is an independently maintained fork of
[CozoDB](https://github.com/cozodb/cozo) — a transactional,
relational-graph-vector database that uses Datalog for queries ("the
hippocampus for AI"). The fork continues the project as a substrate for
**agentic memory**, with performance, correctness, and operational work on top
of upstream `481af05` (the last upstream commit, 2024-12-04).

> mnestic is **not** the official CozoDB and is not affiliated with or endorsed
> by its original authors. All credit for the original design belongs to Ziyang
> Hu and the Cozo Project Authors. See
> [`FORK.md`](https://github.com/shuruheel/mnestic/blob/main/FORK.md) for
> provenance and licensing, and
> [`CHANGELOG-FORK.md`](https://github.com/shuruheel/mnestic/blob/main/CHANGELOG-FORK.md)
> for what diverges from upstream.

## What mnestic adds over CozoDB

Upstream's last commit was 2024-12-04. mnestic continues the engine, with these
capabilities on top of it:

- **Cached graph projections** — `::graph create G { edges: knows }` names an
  in-memory adjacency that twelve graph algorithms reuse across queries instead
  of rebuilding on every call. Always fresh: a projection never serves data
  differing from what the consuming transaction's own scan would return, and a
  write to a source frees what was built from it.
  ([spec](https://github.com/shuruheel/mnestic/blob/main/docs/specs/graph-projection.md))
- **Budgeted weighted traversal** — `BudgetedTraversal` expands cheapest-first
  from a set of seeds, over non-negative weights, under a global distinct-node
  budget (plus optional cost ceiling and exact hop bound) and an in-expansion
  admission gate, emitting each admitted node's `(cost, parent, depth)`.
  Deterministic by construction, interruptible, and able to consume a cached
  graph projection — the primitive for filling a fixed context window with the
  cheapest graph neighborhood around a set of search hits.
  ([spec](https://github.com/shuruheel/mnestic/blob/main/docs/specs/budgeted-traversal.md))
- **Bitemporality** — a `TxTime` column type with a crash-safe monotone commit
  clock, `:as_of` reads, the two-level `(valid time, transaction time)`
  resolution, and `::history` / `::history_gc` / `::evict`.
  ([spec](https://github.com/shuruheel/mnestic/blob/main/docs/specs/bitemporality.md))
- **Provenance semirings** — user-defined absorptive combines inside recursion
  (`Db::register_custom_aggr`), the `min_cost_k` bounded-meet aggregate returning
  the *k* best derivations with their evidence chains, and `:reconcile`
  recompute-based belief revision.
  ([spec](https://github.com/shuruheel/mnestic/blob/main/docs/specs/provenance-semirings.md))
- **Skyline / Pareto-frontier aggregates** — `pareto_min` / `pareto_max` keep, per
  group, the non-dominated set over a numeric vector (native componentwise
  dominance), surfacing a *contested set* — answers none of which beats another —
  instead of collapsing to one winner. Reachable from every binding through plain
  `run_script`; arbitrary caller-defined dominance is available in Rust via
  `register_bounded_meet_aggr`.
  ([spec](https://github.com/shuruheel/mnestic/blob/main/docs/specs/antichain-bounded-meet.md))
- **In-engine hybrid retrieval** — reciprocal-rank fusion over vector, full-text
  and graph legs as one Datalog-composable fixed rule, with MMR diversification.
- **Read-only Cypher** — an openCypher subset translated to CozoScript (alpha;
  feature `cypher`, off by default).
  ([spec](https://github.com/shuruheel/mnestic/blob/main/docs/specs/cypher-read.md))
- **Faster lookups and plans** — equality pushdown turns post-filter point
  lookups into keyed seeks (~28× at 5k rows), plus a deterministic greedy join
  reorder and an opt-in factorized `count()` rewrite.
- **Non-blocking vector index builds** — HNSW builds in RAM in parallel and no
  longer blocks reads for minutes; search-path neighbour vectors batch-fetch
  through RocksDB `MultiGet`.
- **Operational recovery** — `::reindex` rebuilds a relation's HNSW / FTS / LSH
  indexes in place from the configuration the database already stores, so a bulk
  load or a backup restore (neither of which maintains those indexes) has a repair
  path that is not "drop it and reconstruct the creation script by hand"; and
  `::repair_corrupt` surgically deletes truncated tuples instead of forcing you to
  drop a database that fails an integrity check.
- **Interruptibility that works** — `::kill` and `:timeout` abort running
  queries, including long graph-adjacency builds.

Everything else — CozoScript, the storage engines, the data model — is upstream
CozoDB, unchanged unless noted in
[`CHANGELOG-FORK.md`](https://github.com/shuruheel/mnestic/blob/main/CHANGELOG-FORK.md).

## New in 0.12.2

**A float in a validity position was silently read — and written — one million
times too small, landing in 1970.**

Validity and transaction-time stamps are integer *microseconds* since the epoch.
`now()` and `parse_timestamp()` return float *seconds*. The engine coerced one
into the other without a word, so:

- `@ parse_timestamp('2024-06-01T00:00:00Z')` returned **zero rows and no error**
  — the misread lands *before* any row was asserted, which is indistinguishable
  from "no data yet".
- A `:put` of `[parse_timestamp(…), true]` into a `Validity` column **succeeded**
  and stamped the row at 1970. The row reads back correctly on an ordinary query;
  the damage is visible only under time travel — precisely where a bitemporal
  database is meant to be trustworthy.

All four affected paths — the `@` selector, `@ (tt: …)` / `:as_of`, the
`validity(...)` constructor, and the write path — now reject a float and say what
to write instead.

**Upgrade action — and note carefully *where* it bites.** The schema still compiles; it is
the next **write** that now fails. The idiom `Validity default [floor(now()), true]` — and any
spelling that yields a *whole-numbered* float, so `floor(now())`, `round(now())`, or
`parse_timestamp(...)` on a whole second — has been silently writing 1970 into your valid-time
axis. It now errors on write. (Bare `[now(), true]` already errored before this release, but
only by luck: `now()` returns a *fractional* float, and the coercion only ever accepted
whole-numbered ones.) Write instead:
```
last_seen: Validity default [to_int(now() * 1000000), true]
```

We found exactly one caller of the broken idiom anywhere, and it was **our own
HNSW test**, inherited from upstream, which had been stamping every row it wrote
at 1970 for as long as the test existed. It never asserted on the value, so it
never noticed. If it was in our test suite, it is in someone's schema.

**What this deliberately does *not* fix.** An integer in *seconds* (`@ 1704067200`)
is still accepted and still silently returns nothing. Valid time is an abstract,
user-settable logical clock — the tutorial itself queries `@ 2019` — so no
magnitude check can distinguish a wrong-unit timestamp from a legitimate small
one, and any such check would be wrong. The real answer is a typed conversion,
which lands with the datetime library. Until then: integer microseconds is the
low-level form, and the string forms (`@ '2024-06-01'`,
`@ '2024-06-01T12:00:00Z'`) are the safe ones.

Public Rust API is byte-identical to 0.12.1.

Full detail is in
[`CHANGELOG-FORK.md`](https://github.com/shuruheel/mnestic/blob/main/CHANGELOG-FORK.md).

### Upgrading to 0.13.0

**Pre-1970 timestamps.** mnestic now accepts a date before the Unix epoch
wherever it accepts a timestamp string. It used to panic — and on the `mem` and
`sqlite` backends the panic happened while the store's write guard was held,
poisoning the lock and killing the database. Pre-epoch writes that previously
panicked were never committed, so **no stored data is affected; simply re-run
them.** No action is required on upgrade.

**HNSW indexes may need one rebuild.** Three separate bugs left stale data in
HNSW indexes built by any release through 0.12.2:

- A `:put` or `update` that set a row's vector column to `null` (or shortened a
  list-of-vectors column) left the row's old graph nodes behind. This is not a
  stale-result bug — the search reads a node's vector back from the base row, so
  a single stranded node makes **every** vector query on that relation fail with
  `Cannot interpret null as vector`, or panic.
- `::hnsw create` over a relation that **already had rows** wrote one-directional
  edges. A later `:rm` — or a re-`:put` that changes a vector, i.e. re-embedding —
  can then strand an orphan edge, and a search may fail with `Cannot find
  compound key for HNSW`.
- An all-zero vector (what a failed or absent embedding produces) made cosine
  distance `NaN`, which wedged the search heap and silently degraded results.

Upgrading alone restores **correct query results** for the zero-vector case. For
the other two, the stale rows are on disk. Rebuild once, per affected relation:

```text
::reindex <relation>
```

Your rows are untouched and nothing is deleted; `::reindex` rebuilds index
relations only. It is safe to run on an index that is already failing. An index
created on an **empty** relation and populated only with non-null vectors by
`:put`, without later nulling or shortening a vector field, is unaffected by the
two on-disk bugs.

**`::repair_corrupt` does not fix any of the above** and will report `removed: 0`.

**Corrupt value blobs are now an error, not a panic.** A corrupt value in a
stored relation used to panic the process — through the Python wheel that was a
`PanicException`, a `BaseException` subclass that `except Exception:` does not
catch. It is now an ordinary query error (`eval::corrupt_value_blob`, naming the
key). If you hit it, run `::repair_corrupt <relation>` to drop the unreadable
rows, **then** `::reindex <relation>` if that relation carries an HNSW/FTS/LSH
index — repair cannot evict the dead row's index postings, because it cannot
decode the row to know what they were.

**`restore_backup` could mint colliding relation ids.** In any release through
0.12.2, a relation created **after** a `restore_backup` into a fresh store could
be given an id a restored relation already owned — silently sharing one
keyspace, so reads of either returned both. **On upgrade, opening the store stops
any further collisions with no action on your part**, and logs an error naming
any relations that are already entangled.

**If that error fires, the entangled rows cannot be separated.** The store never
recorded which relation wrote which row. **Do not run `::repair_corrupt` on them
— it deletes the narrower relation's rows.** Recover by restoring the
**original** backup into a **fresh** store with this build. (If your only backup
was taken *from* the already-damaged store, it carries the entanglement.) Stores
where `restore_backup` was never called, or where no relation was created
afterwards, are unaffected.

**`hybrid_search`: every fusion leg now needs a distinct label.** Two graph legs
sharing a label were never fused as two lists — reciprocal-rank fusion groups by
the label, so the second was silently merged into the first.
**`GraphLeg::default()` labels every leg `"graph"`, so two defaulted legs collided
by construction**; in Python, `label` is optional and defaults to `"graph"`, so
the same applies. This now **errors at build time** instead of returning a wrong
ranking. Give each leg its own label (`"semantic"` and `"text"` are reserved). A
single graph leg is unaffected.

**`GraphLeg` no longer re-scores its own seeds.** A seed reachable from itself —
guaranteed at hop 2 whenever `undirected: true`, and possible at hop 1 via a
self-loop or an edge from a second seed — was re-entering its own ranked list.
Seeds that legitimately match the vector or keyword query are still returned
and still rank where those legs put them; only the spurious graph-leg
contribution is gone. Rankings will shift. No migration.

**One query that used to return zero rows now raises.** A query reading a stored
relation by a **fully-bound key**, with a filter that errors at evaluation time,
silently returned `Ok([])`; it now raises that error. (The same query with an
unbound key already raised — the engine was giving two different answers to the
same logical query depending on the plan it chose.)


## Importable name

The published crate is `mnestic`, but the importable Rust crate name is `cozo`,
so existing CozoDB code works unchanged:

```toml
[dependencies]
mnestic = "0.12.2"
```

```rust
use cozo::{DbInstance, ScriptMutability};
```

The query language (CozoScript / Datalog) and engine semantics are unchanged
unless noted in the fork changelog.

## Features

Default is `compact` (SQLite backend). RocksDB, vector (HNSW), full-text search,
and graph-algorithm features match upstream Cozo 0.7.x. See the crate docs and
the [upstream CozoDB documentation](https://docs.cozodb.org/) for the query
language and feature flags.

## License

Mozilla Public License 2.0. Original work © 2022 The Cozo Project Authors;
fork modifications © 2026 Shan Rizvi.
