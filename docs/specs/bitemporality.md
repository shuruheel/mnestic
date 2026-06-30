# Spec — Bitemporality

_Status: **Design spec — build-now** (decisions resolved 2026-06-30; the build freeze was removed 2026-06-30, so this is active engine work, not deferred). Companion to [`../../ROADMAP.md`](../../ROADMAP.md) and the platform-side valid-time spec [`mindgraph/docs/plans/bitemporal-valid-time.md`]. Grounded in the current valid-time code (citations are `file:line` in `cozo-core/src/`). The five §11 open decisions are now resolved (recommended, pending owner sign-off) in §13; step 2 (the commit clock) has no §11 dependency and can start immediately._

> **One-line goal.** Add an **engine-assigned transaction-time axis** alongside Cozo's existing valid-time axis, so a relation can answer **"what did the database *believe* at transaction-time T about valid-time V"** — opt-in per relation, zero cost for relations that don't use it, and degenerating to today's exact fast path at "now / now".

---

## 1. Why — the capability and why it's the marquee item

Cozo already has **valid time** (vt): *when a fact is true in the modeled world*. It does **not** have **transaction time** (tt): *when the database recorded (or corrected) its belief about that fact*. Without tt you cannot:

- **Reproduce a past query result** ("re-run the report exactly as it would have answered last Tuesday") — because corrections applied since then are now baked in.
- **Audit belief changes** ("when did we learn this, and what did we think before?") — the core of provenance/governance, and a converging table-stake for agent memory (memory poisoning, GDPR, "what did the agent know when it acted").
- **Distinguish a real-world change from a correction.** "Salary became 120 in March" (vt change) vs "we were wrong; it was always 120" (tt correction) are different facts; single-axis storage conflates them.

Nothing embedded serves bitemporality *in-engine* today (Graphiti's four-timestamp edge model is the product-level validation; it's assembled on top of Neo4j). This is the differentiator with no embedded competitor.

## 2. Current valid-time model (what exists — verified)

| Piece | Where | Behavior |
|---|---|---|
| `Validity { timestamp: ValidityTs(Reverse<i64>), is_assert: Reverse<bool> }` | `data/value.rs:99-140` | Single temporal axis. `Reverse` on both → newest-first, assert-before-retract at equal ts. |
| `ColType::Validity`, declared as a key column | `data/relation.rs:101`, grammar `cozoscript.pest:236` | `:create rel {k, v: Validity => d}` |
| Validity **must be the last key column** | `runtime/relation.rs:222-225` (`choose_index`) | Index selection rejects any index where validity isn't the trailing key. |
| memcmp key encoding | `data/memcmp.rs:116-123` | `VLD_TAG` + `!order_encode_i64(ts)` (BigEndian u64) + `(!is_assert as u8)`. Trailing position → versions of one logical key are contiguous, newest-first. |
| Write / retract | `query/stored.rs` (`put_into_relation`) | A **retraction is a new row** with `is_assert=false`, not a delete. |
| As-of read `@ <expr>` | grammar `cozoscript.pest:84` (`validity_clause = {"@" ~ expr}`); parse `parse/query.rs:1082-1098` (`expr2vld_spec`); carried as `valid_at: Option<ValidityTs>` on `FixedRuleArg::{Stored,NamedStored}` (`data/program.rs:372-390`) | `@ "NOW"` → current clock, `@ "END"` → oldest, numeric µs / ISO-8601 string otherwise. |
| Skip-scan | dispatch `fixed_rule/mod.rs:94-101` → `runtime/relation.rs:372-385` (`skip_scan_all`) → backend `range_skip_scan_tuple` → `data/tuple.rs:60-84` (`check_key_for_validity`) | Per logical key: seek to first version with `ts ≤ valid_at`; if retraction → skip the whole key (seek to `TERMINAL_VALIDITY`); else emit and seek to next key. **At `@ "NOW"` this is one seek per key — the fast path we must preserve.** |
| Clock | `data/functions.rs:2456-2466` (`current_validity()`) | Wall-clock microseconds, `Reverse`-wrapped. **Per-row, not monotonic, not transaction-scoped.** |
| **Commit clock** | `runtime/transact.rs:137-140` (`commit_tx`), `SessionTx` struct `:26-35` | **None.** Commit just calls `store_tx.commit()`. `SessionTx` has no timestamp field. |
| Tests / bench | `data/tests/validity.rs`, `benches/time_travel.rs` (1/10/100/1000 versions/key) | Pin assert/retract/`@"NOW"`/`@"END"` semantics + version-count latency. |

**Two facts that shape the whole design:** (a) the temporal axis is the *trailing* key component with `Reverse` ordering and a skip-scan that's a single seek at "now"; (b) **there is no transaction clock at all** — so tt is not "expose an existing thing," it's "add a new monotonic clock."

## 3. Design overview

- **Keep vt exactly as-is** (user-facing, user-settable, `@ vt`). Purely additive — no behavior change for existing relations or queries.
- **Add tt as a second, engine-assigned, trailing key component** *behind* vt: key layout for a bitemporal relation becomes
  `[RelationId][k1]…[k_{n-1}][Validity (vt)][TxTime (tt)]`.
  tt is **never user-settable** — the engine stamps it at commit (the whole point: tt records when *the database* learned the fact).
- **Opt-in per relation.** A relation declares bitemporality at `:create`; non-bitemporal and vt-only relations keep their exact current encoding and zero overhead.
- **Append-only.** Assertions and retractions are new (vt, tt) rows; corrections are new rows at a higher tt. `::evict` is the one deliberate exception (GDPR).
- **Resolution = two-level skip-scan** that **degenerates to today's single seek at (vt=now, tt=now)**.

### Why vt-outer / tt-inner (not the reverse)

With key order `…[vt-desc][tt-desc]`, the common query "current belief about the current world" (`@ "NOW" @! "NOW"`) seeks once to `(key, now, now)` and the first row is the answer — **identical cost to today**. tt-outer ordering would break that fast path (the current-belief query would span every tt group). So vt stays the outer of the two trailing components; tt is appended after it. This also means the existing "validity is last key column" invariant generalizes cleanly to "the (vt, tt) pair is the last two key columns."

### The resolution algorithm (`@ V @! T`)

For each logical key, walk vt-groups descending from `V`; within a vt-group, find the record with the greatest `tt ≤ T` (the belief held as of T about that vt-version):
- **assertion** → that is the answer for the key (it's the vt-latest version believed-asserted as of T);
- **retraction** → the key was believed-deleted as of (V, T) → emit nothing;
- **no record with tt ≤ T in this vt-group** (every belief about this vt-version was recorded after T) → fall through to the next lower vt-group.

At `T = now` (max tt) the inner search is trivially the first row of each vt-group, and at `V = now` the outer walk starts at the newest vt-group — collapsing to one seek per key. Historical T costs one extra sub-seek per examined vt-group; the regression budget (§9) bounds this.

## 4. Schema & syntax

### Opt-in at `:create`

Proposed (decision in §11): a relation becomes bitemporal when it declares a `TxTime`-typed trailing key column the engine fills, e.g.

```
:create belief {entity, v: Validity, tt: TxTime => value}
```

- `tt: TxTime` must be the **last key column**, immediately after the `Validity` column.
- The column is **engine-assigned**: supplying a value for it on `:put` is an error (mirrors how you can't write a computed column). This is what distinguishes tt from vt (vt is user-settable).
- Reuses the `ColType` machinery (`data/relation.rs:84-103`) + a `txtime_type` grammar rule mirroring `validity_type` (`cozoscript.pest:236`).

### Query syntax

- `@ <expr>` — valid-time, **unchanged**.
- `@! <expr>` — **new** transaction-time selector, optional, after `@`. `@! "NOW"` (default), `@! "END"` (earliest belief), numeric µs, or ISO-8601.
- Examples:
  - `*belief{entity, value}` — current belief about current world (both default to now). Fast path.
  - `*belief{entity, value} @ 2026-01-01` — what we *now* believe was true on Jan 1.
  - `*belief{entity, value} @ 2026-01-01 @! 2026-03-01` — what we believed **on Mar 1** was true on Jan 1.
  - `*belief{entity, value} @! 2026-03-01` — current-world value as we believed it on Mar 1.

Grammar: add `tx_validity_clause = {"@!" ~ expr}` after `validity_clause` in the four relation-access rules (`cozoscript.pest:80,81,88,89`); parse into a new `tx_valid_at: Option<ValidityTs>` alongside `valid_at` on `FixedRuleArg::{Stored,NamedStored}` (`data/program.rs:372-390`), evaluated by an `expr2vld_spec`-twin.

## 5. The transaction clock (the new, load-bearing mechanism)

tt must be **monotonic, collision-free, and identical for every write in one transaction** (a transaction is one atomic belief-update). Today none of this exists. Design:

- A **monotonic, wall-clock-floored commit counter**: `tt = max(physical_now_µs, last_tt + 1)`. Wall-clock-meaningful, strictly monotonic, never collides even within a microsecond or across a backward clock step. (Deliberately *not* a true hybrid logical clock — there is no separate logical component and no cross-node causality merge, neither of which a single-process embedded DB needs; the floor-to-wall-clock + increment is all that's required here.)
- **In-process source of truth = an `AtomicI64` high-water mark** on the `Db` (lock-free read/advance), **seeded at open** from a persisted system key and **persisted once per committing write-transaction** that touched a bitemporal relation.
- The whole committing transaction stamps its bitemporal writes with **one** tt, captured at commit time (or first bitemporal write, then frozen). Add a `tx_time: Option<ValidityTs>` field to `SessionTx` (`runtime/transact.rs:26-35`); set it lazily; persist + advance the high-water mark in `commit_tx` (`:137-140`).

**Concurrency caution (heed the 0.8.4 `avgdl` lesson).** The 0.8.4 changelog shows a single shared hot storage key, read-modify-written inside every transaction under RocksDB pessimistic locking, serialized all writers and lost updates. The tt high-water mark must **not** repeat that: the atomic is the in-process authority (no per-write storage read), and persistence is a single put on the committing tx (unavoidable and fine for belief-update workloads, which are not bulk-ingest-rate). Document the contract; if a high-contention bitemporal-write path ever appears, batch the persist. Single-process embedded DB makes the atomic authoritative; persistence is purely for restart correctness.

## 6. Write path

- `:put` into a bitemporal relation: the engine appends the frozen `tx_time` as the trailing tt component; user-supplied tt is rejected (§4).
- Assert vs retract continues to ride the existing `is_assert` bool inside the vt `Validity` (unchanged) — a retraction at (vt=V, tt=T) records "as of T we believe the fact ceased to hold at V." Corrections are just new rows at higher tt.
- Bulk path: `import_relations`/`batch_put` already skip secondary indexes; bitemporal stamping must be applied on the row-level `:put` path (`query/stored.rs`), and the bulk path either rejects bitemporal relations or stamps a single tt for the batch (decision §11).

## 7. System operations

| Op | Semantics | Notes |
|---|---|---|
| `::history rel {k…}` | Return **all** (vt, tt) records for the given key(s), structured/sorted — the full belief timeline. | Read-only; the introspection surface. |
| `::history_gc rel before_tt` | Drop records with `tt < before_tt` **that are superseded** — preserve, per (key, vt), the latest belief at or before the cutoff so that as-of-now and as-of-(≥cutoff) answers are unchanged. | MariaDB-shaped, per-relation, online. Mutating → read-only-guarded like its siblings. |
| `::evict rel {k…}` | **Hard-delete every record** (all vt, all tt) for a key. The one intentional break of append-only, for GDPR/right-to-be-forgotten. | Recorded in an audit trail (a system relation). Read-only-guarded. |

(`::describe` is the precedent for a sys op that mutates metadata and is read-only-guarded — see CHANGELOG-FORK 0.8.5.)

## 8. Backward compatibility & backend parity

- **Non-bitemporal relations**: byte-identical encoding, zero new cost. Pinned by a "single-axis unchanged" test.
- **vt-only relations** (`Validity` last, no `TxTime`): unchanged — `@ vt` behaves exactly as today; `@! ` on a non-bitemporal relation is a clear error.
- **Backends**: all logic is key-encoding + scan, so it rides the existing `range_skip_scan_tuple` trait across **rocks / sqlite / mem** by generalizing it to a two-axis `range_bitemporal_scan_tuple` (the single-axis function stays for vt-only relations). **No RocksDB user-defined timestamps** — rejected (see §12) (per-CF all-or-nothing, +8 B/key, single global GC floor, no SQLite parity; TiKV and CRDB both chose key encoding). Key-encoding gives us SQLite parity for free.

## 9. Testing & performance budget

- **Backend for tests: SQLite + `tempfile::tempdir()`** (the `mem` backend uses a different join operator; stored/scan-path regressions must use sqlite).
- **Pin the four bitemporal quadrants**: (now, now), (past-vt, now), (now, past-tt), (past-vt, past-tt); the **correction case** (a higher-tt row overrides an earlier belief about a past vt; the as-of-past-tt query still returns the old belief); **retraction vs eviction**; **`::history_gc` preserves as-of-now and as-of-(≥cutoff)**; **opt-in isolation** (non-bitemporal relations byte-identical; vt-only unchanged); **commit-clock monotonicity** across same-µs writes and a simulated backward clock step.
- **Regression budget**: ≤~10% on bitemporal relations (AeonG envelope), **zero** on non-bitemporal (opt-in). Extend `benches/time_travel.rs` with a bitemporal matrix (versions × corrections-depth) and confirm the (now, now) path matches the current single-axis number.

## 10. Phased implementation plan

1. **Spec + design review** — this doc; resolve §11. *(now)*
2. **Transaction-commit clock** — `AtomicI64` high-water mark on `Db`, persisted system key, `tx_time` on `SessionTx`, advance+persist in `commit_tx`. Testable in isolation (monotonicity, restart, backward-clock). *Foundational; no user-visible surface yet.*
3. **Schema opt-in + key encoding** — `TxTime` ColType + grammar; generalize the "validity is last key" invariant (`runtime/relation.rs:222-225`) to the (vt, tt) trailing pair; write-path stamping (`query/stored.rs`); reject user-supplied tt.
4. **Read path** — `@! tt` grammar (`cozoscript.pest`) + parse (`tx_valid_at` on `FixedRuleArg`) + the two-level skip-scan (`range_bitemporal_scan_tuple` across the three backends, generalizing `check_key_for_validity`).
5. **Sys ops** — `::history`, `::history_gc`, `::evict` (+ audit relation), all read-only-guarded.
6. **Benches + budget validation** — confirm ≤10% / zero, and the (now, now) fast-path parity.

Each step is its own PR with a failing test first, CHANGELOG-FORK entry, and `cargo test -p mnestic --lib` green. Steps 2–4 are the substance; 1 and 6 bracket them.

## 11. Open decisions (resolve before step 3)

1. **Opt-in surface.** A `TxTime` column type (as drafted) vs a relation-level flag (`:create rel {…} bitemporal`) vs reusing `Validity` with a second mode. The column type is the most explicit and reuses `ColType`/index machinery — **recommended** — but confirm we want the user to *name* the tt column (useful for `::history` projection) vs the engine hiding it.
2. **Syntax for tt selector.** `@! <expr>` (recommended) vs `@@` / `@tx`. `@!` reads as "and-also-at"; low grammar-collision risk (the `!`-prefixed forms are unused in relation-access position).
3. **Bulk path** (`import_relations`/`batch_put`) into a bitemporal relation: reject, or stamp one tt per batch? Recommend **stamp-one-tt-per-batch** (a bulk import is one belief event) with a clear note that it bypasses per-row vt defaulting.
4. **Relationship to MindGraph's tombstones/supersession.** MindGraph already has app-level tombstones + `Supersedes` edges (curation work). By the engine/cognitive-layer split, the engine ships the **mechanism** (bitemporal tt); whether MindGraph *adopts* it (collapsing its tombstones into tt) is a separate platform decision. They compose, don't duplicate: supersession is assertion-level cognitive lineage; bitemporality is storage-level versioning. Flag for the platform roadmap; **not** a blocker here.
5. **Event-time?** Explicitly **out of scope.** "When the real-world event occurred" is a domain attribute (a normal column), not a third temporal axis. vt already models "true in the world"; tt models "known to the DB." Two axes, no more (rejecting tri-temporal per §4's "default-on versioning rejected" discipline).

## 12. Rejected alternatives (with evidence)

- **RocksDB user-defined timestamps** — per-CF all-or-nothing, +8 B/key on every index, single global GC floor, no SQLite parity. TiKV and CRDB both rejected it for key encoding. ❌
- **SQL:2011 interval-column rewrites / interval-tree indexes** — wrong shape for a memcmp-keyed LSM store; reintroduce range-overlap complexity the skip-scan avoids. ❌
- **Default-on bitemporality** — SQL Server measures 10–20% high-write overhead; opt-in keeps non-temporal relations at exactly zero. ❌
- **Temporal path algebra / tri-temporal** — scope explosion; not pulled by any real need. ❌

## 13. Resolutions (2026-06-30 — recommended, pending owner sign-off)

The build freeze was removed 2026-06-30; this is now active engine work. The five §11 open decisions are resolved as below. Each adopts the spec's recommendation; the rationale and the rejected alternative are recorded so the decision is auditable. **Step 2 (the commit clock) depends on none of these** and can begin immediately; these gate **step 3** (schema opt-in) onward.

1. **Opt-in surface → a named `TxTime` column type.** A relation becomes bitemporal by declaring a trailing `tt: TxTime` key column immediately after the `Validity` column (e.g. `:create belief {entity, v: Validity, tt: TxTime => value}`). The column is **engine-assigned**: supplying a value for it on `:put` is an error (mirrors a computed column). Reuses the existing `ColType`/index machinery (`data/relation.rs:84-103`). The user **names** the column (not engine-hidden) so `::history` can project it and binder errors can reference it.
   _Rejected:_ a relation-level `bitemporal` flag (new grammar, less explicit, no natural `::history` projection name); reusing `Validity` with a second mode (overloads a stable, fast-path-critical type and risks the `@ "NOW"` single-seek encoding).

2. **Transaction-time selector syntax → `@! <expr>`.** Optional, after `@`. `@! "NOW"` (default), `@! "END"` (earliest belief), numeric µs, or ISO-8601. Reads as "and-also-at"; the `!`-prefixed forms are unused in relation-access position → low grammar-collision risk.
   _Rejected:_ `@@` (visually ambiguous against `@`); `@tx` (consumes an identifier that can collide with a column/var name).

3. **Bulk path (`import_relations`/`batch_put`) → stamp one tt per batch.** A bulk import is treated as a single belief event: all rows in the batch share one engine-assigned tt (captured once at batch commit). The bulk path still **rejects any user-supplied tt** and must document that it bypasses per-row vt defaulting. If a high-contention bitemporal bulk-write path ever appears, batch the high-water-mark persist (heed the 0.8.4 `avgdl` shared-hot-key lesson, §5).
   _Rejected:_ hard-rejecting bitemporal relations from the bulk path (too limiting for backfills/restores, which are exactly belief-bulk events).

4. **Relationship to MindGraph's tombstones/supersession → compose, do not collapse (for now).** The engine ships the **mechanism** (bitemporal tt). MindGraph continues to use its own **app-level tombstones + valid-time-oriented `Supersedes`** (the platform-side work specced in `mindgraph/docs/plans/bitemporal-valid-time.md`) and does **not** adopt engine tt as its supersession mechanism in this round. The two are **orthogonal axes**, not a stack: platform supersession/valid-time is *assertion-level cognitive lineage* ("when was the fact true in the world / which assertion wins"); engine tt is *storage-level versioning* ("what did the store believe at commit-time T"). A later unification — MindGraph's `node`/`edge` relations becoming engine-bitemporal so retraction + time-travel ride tt — is possible but **deferred**; the first concrete consumer that would pull it is **provenance-semirings R3** (retraction/TMS shares this storage axis). **Not a blocker** for engine steps 2–6.

5. **Event-time → confirmed out of scope.** Two axes only (vt = true-in-world, tt = known-to-DB). "When the real-world event occurred" stays a domain column (MindGraph's `Claim.event_date`, entity birth/death/founded dates), never a third temporal axis. No tri-temporal (consistent with the "default-on versioning rejected" discipline, §4).
