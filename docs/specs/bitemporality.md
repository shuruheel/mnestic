# Spec — Bitemporality

_Status: **SHIPPED in 0.10.0 (2026-07-04)** — all implementation steps (2–6) landed; per-step annotations in §10. Originally: design spec — build-now (decisions resolved 2026-06-30; **revised 2026-07-02** after external contributor review + a full verification pass against source. The review challenged three §4 surface choices — the positional column rule, the `Validity`/`TxTime` naming split, and the `@!` selector — and verification confirmed the discomfort was load-bearing: `@!` has a real grammar collision §13.2 had denied, `@ "END"` was glossed backwards, and the §3 resolution algorithm was ambiguous about vt-group granularity. §13 decisions 1–2 are revised and decisions 6–10 added; **all ten signed off by the owner 2026-07-02** — implementation is unblocked end-to-end, starting with step 2, the commit clock.) Companion to [`../../ROADMAP.md`](../../ROADMAP.md) and the platform-side valid-time spec [`mindgraph/docs/plans/bitemporal-valid-time.md`]. Grounded in the current valid-time code (citations are `file:line` in `cozo-core/src/`)._

> **One-line goal.** Add an **engine-assigned transaction-time axis** alongside Cozo's existing valid-time axis, so a relation can answer **"what did the database *believe* at transaction-time T about valid-time V"** — opt-in per relation, zero cost for relations that don't use it, and degenerating to today's exact fast path at "now / now".

---

## 1. Why — the capability and why it's the marquee item

Cozo already has **valid time** (vt): *when a fact is true in the modeled world*. It does **not** have **transaction time** (tt): *when the database recorded (or corrected) its belief about that fact*. Without tt you cannot:

- **Reproduce a past query result** ("re-run the report exactly as it would have answered last Tuesday") — because corrections applied since then are now baked in.
- **Audit belief changes** ("when did we learn this, and what did we think before?") — the core of provenance/governance, and a converging table-stake for agent memory (memory poisoning, GDPR, "what did the agent know when it acted").
- **Distinguish a real-world change from a correction.** "Salary became 120 in March" (vt change) vs "we were wrong; it was always 120" (tt correction) are different facts; single-axis storage conflates them.

Nothing embedded serves bitemporality *in-engine* today (Graphiti's four-timestamp edge model is the product-level validation; it's assembled on top of Neo4j). Even the academic state of the art in temporal Datalog — **DatalogMTL / Temporal Vadalog** (interval-annotated facts `P(τ)@ϱ` with the ⊟/⊞/◆/◇/S/U operators) — is **unitemporal**: it reasons over valid time only and tracks nothing about when the system asserted or corrected a fact (design-partner review, 2026-07-02). Every regulator-facing question is a variant of "what did the system believe at time T, before the correction" — the axis that whole line of work doesn't have. This is the differentiator with no embedded competitor.

## 2. Current valid-time model (what exists — verified)

| Piece | Where | Behavior |
|---|---|---|
| `Validity { timestamp: ValidityTs(Reverse<i64>), is_assert: Reverse<bool> }` | `data/value.rs:99-140` | Single temporal axis. `Reverse` on both → newest-first, assert-before-retract at equal ts. |
| `ColType::Validity`, declared as a key column | `data/relation.rs:101`, grammar `cozoscript.pest:236` | `:create rel {k, v: Validity => d}` |
| Validity **must be the last key column to be `@`-queried** | enforced at **query** time: `query/ra.rs:350-356` (`InvalidTimeTravelScanning`); `choose_index` (`runtime/relation.rs:222-225`) additionally skips any secondary index whose trailing key isn't the relation's last key column | **No `:create`-time enforcement exists** — a relation with a non-trailing Validity column is legal today (the column behaves as plain data); it just can't be time-travel-scanned. |
| memcmp key encoding | `data/memcmp.rs:116-123` | `VLD_TAG` + `!order_encode_i64(ts)` (BigEndian u64) + `(!is_assert as u8)`. Trailing position → versions of one logical key are contiguous, newest-first; assert byte `0` sorts before retract byte `1` at equal ts. |
| Write / retract | `query/stored.rs:208` (`put_into_relation`) | A **retraction is a new row** with `is_assert=false`, not a delete. Coercion accepts explicit Validity, `"ASSERT"`/`"RETRACT"`, ISO-8601 strings, `[ts, bool]` lists (`data/relation.rs:333-388`) — i.e. **vt is user-settable, including into the past and future**. |
| As-of read `@ <expr>` | grammar `cozoscript.pest:84` (`validity_clause = {"@" ~ expr}`); parse `parse/query.rs:1082-1098` (`expr2vld_spec`, const-evaluated: Num µs or Str only); carried as `valid_at: Option<ValidityTs>` on `FixedRuleArg::{Stored,NamedStored}` (`data/program.rs:372-390`) | `@ "NOW"` → current clock; `@ "END"` → **end of time** (`MAX_VALIDITY_TS = ValidityTs(Reverse(i64::MAX))`, `data/functions.rs:2468` — the *final* state, including future-dated assertions; pinned by `data/tests/validity.rs:156-183`); numeric µs / ISO-8601 string otherwise. **A bare access with no `@` is a raw scan returning ALL versions.** |
| Skip-scan | dispatch `fixed_rule/mod.rs:94-101` → `runtime/relation.rs:372-385` (`skip_scan_all`) → backend `range_skip_scan_tuple` → `data/tuple.rs:60-84` (`check_key_for_validity`) | Per logical key: seek to first version with `ts ≤ valid_at`; if retraction → skip the whole key (seek to `TERMINAL_VALIDITY`, `data/functions.rs:2469-2472`); else emit and seek to next key. **At `@ "NOW"` this is one seek per key — the fast path we must preserve.** |
| Clock | `data/functions.rs:2456-2466` (`current_validity()`) | Wall-clock microseconds, `Reverse`-wrapped. Captured **once per script** (`runtime/db.rs:348`). **Per-script, not monotonic, not transaction-commit-scoped.** Two same-µs writes to one (key, vt) produce identical keys — the second silently replaces the first. |
| **Commit clock** | `runtime/transact.rs:137-140` (`commit_tx`), `SessionTx` struct `:26-35` | **None.** Commit just calls `store_tx.commit()`. `SessionTx` has no timestamp field. |
| Tests / bench | `data/tests/validity.rs`, `benches/time_travel.rs` (1/10/100/1000 versions/key) | Pin assert/retract/`@"NOW"`/`@"END"` semantics + version-count latency. **The equal-ts assert-shadows-retract behavior is currently unpinned** (no test writes assert+retract at one explicit ts) — §9 adds the pin. |

**Two facts that shape the whole design:** (a) the temporal axis is the *trailing* key component with `Reverse` ordering and a skip-scan that's a single seek at "now"; (b) **there is no transaction clock at all** — so tt is not "expose an existing thing," it's "add a new monotonic clock."

## 3. Design overview

- **Keep vt exactly as-is** (user-facing, user-settable, `@ vt`). Purely additive — no behavior change for existing relations or queries.
- **Add tt as a second, engine-assigned, trailing key component** *behind* vt: key layout for a bitemporal relation becomes
  `[RelationId][k1]…[k_{n-1}][Validity (vt)][TxTime (tt)]`.
  tt is **never user-settable** — the engine stamps it at commit (the whole point: tt records when *the database* learned the fact).
- **Opt-in per relation, per axis.** A relation declares its temporal axes at `:create` (§4): none, vt-only, **tt-only (system-versioned)**, or vt+tt (bitemporal). Non-temporal and vt-only relations keep their exact current encoding and zero overhead.
- **Append-only.** Assertions and retractions are new (vt, tt) rows; corrections are new rows at a higher tt. `::evict` is the one deliberate exception (GDPR).
- **Resolution = two-level skip-scan** that **degenerates to today's single seek at (vt=now, tt=current-belief)**.

### Why vt-outer / tt-inner (not the reverse)

With key order `…[vt-desc][tt-desc]`, the common query "current belief about the current world" (`@ "NOW"`, tt defaulted) seeks once to `(key, now, tt-max)` and the first row is the answer — **identical cost to today**. tt-outer ordering would break that fast path (the current-belief query would span every tt group). So vt stays the outer of the two trailing components; tt is appended after it. The existing "validity is last key column" behavior generalizes to "the temporal axes are the trailing key columns" (§4).

### The resolution algorithm (`@ (vt: V, tt: T)`)

**Definition — vt-group.** All records of a logical key sharing the same vt *timestamp*, **regardless of the vt `is_assert` flag**. This definition is load-bearing: the encoded order within a key is `[vt.ts desc][vt.is_assert: assert-first][tt desc]`, so one vt-group physically spans **two contiguous runs** (the assert run, then the retract run). Resolution must take the record with the greatest `tt ≤ T` **across both runs** (two bounded sub-seeks per examined vt-group). If resolution instead examined only the first (assert) run, a later-recorded cessation `(vt=V, retract)` at higher tt would be silently shadowed by the older assert — a wrong answer on the most ordinary correction there is.

For each logical key, walk vt-groups descending from `V`; within a vt-group, find the record with the greatest `tt ≤ T` across both is_assert runs (the belief held as of T about that vt-version):
- **assertion** → that is the answer for the key (it's the vt-latest version believed-asserted as of T);
- **retraction** → the key was believed-deleted as of (V, T): **emit nothing for this key and stop**. Older vt-groups do **not** shine through — this matches current single-axis semantics (`data/tuple.rs:74-77`), where a vt-retraction means "the fact ceased to hold at V," not "undo this version";
- **no record with tt ≤ T in this vt-group** (every belief about this vt-version was recorded after T) → fall through to the next lower vt-group.
- **tie-break at equal (vt.ts, tt):** assert wins (consistent with the existing equal-vt-ts rule). A transaction may not write both an assert and a retract for one (key, vt) — §6 rejects it, because both rows would carry the same tt and the tie would otherwise be unbreakable.

At `T = current-belief` (the default — resolves to end-of-tt-time, §4) the inner search is trivially the first row of each vt-group, and at `V = now` the outer walk starts at the newest vt-group — collapsing to one seek per key. Historical T costs one extra sub-seek per examined vt-group (two when the retract run must be checked); the regression budget (§9) bounds this.

### Correction semantics (what "corrections are just new rows" actually means)

The three correction shapes, made explicit so implementers and users don't have to re-derive them:

| Intent | Mechanic | As-of-now answer | As-of-old-tt answer |
|---|---|---|---|
| **Value correction** — "it was 120 at vt=March, not 130" | `:put (vt=March, assert, 120)` at higher tt | 120 at vt≥March | 130 (the belief then) |
| **Cessation** — "the fact ceased to hold at vt=V" | `:put (vt=V, "RETRACT")` at current tt | nothing at vt≥V | old belief intact |
| **Existence repudiation** — "the March assertion should never have existed; January's value (100) still stands" | **repudiation-by-copy**: `:put (vt=March, assert, 100)` at higher tt — re-assert the predecessor's value | 100 at vt≥March (value-correct, redundant row) | the wrong belief, as held |

Repudiation **cannot** be expressed as `(vt=March, retract)` — that means cessation, and retraction never falls through to January (see algorithm above). The copy idiom is value-correct and **`::history_gc`-stable** (gc preserves the latest belief per (key, vt) for both groups), but it has a documented **composition limit**: the copy is a snapshot, not a reference. If January's value is later itself corrected (to 95 at tt₃), the copied March row still answers 100 at vt≥March until the application re-copies. A **tt-level group-repudiation marker** (a third record kind meaning "as of tt, disbelieve this vt-group entirely" → resolution falls through to the predecessor) would compose correctly, at the cost of a three-state within-group resolution and a `::history_gc` invariant (the marker is an undroppable latest-belief). **v1 does not build the marker** (no current consumer; MindGraph keeps app-level tombstones/`Supersedes`, §13.4) — but the `TxTime` encoding **reserves the byte** that makes it purely additive later (§4), and §13.7 names the trigger for building it.

## 4. Schema & syntax

_This section was revised 2026-07-02 (external contributor review; §13.1/13.2 revised, 13.6–13.9 added)._

### Terminology: axes vs column types

| Temporal axis | Column type | Value | Set by |
|---|---|---|---|
| **vt** — valid time ("true in the world") | `Validity` | `{timestamp, is_assert}` — a time *and* an assert/retract flag; coercions accept `"ASSERT"`/`"RETRACT"`/ISO/`[ts,bool]` | the user (past, present, or future) |
| **tt** — transaction time ("known to the DB") | `TxTime` | a bare monotonic instant | the engine, at commit; user-supplied values rejected |

The type names are deliberately **asymmetric because the types are asymmetric** — `Validity` is a user-settable (timestamp, flag) pair with rich coercions; `TxTime` is a bare engine-stamped instant. A symmetric rename (`VTime`/`TxTime`) was considered and rejected: `Validity` is shipped surface (`::columns` prints it, `data/relation.rs:43`; every existing script and the upstream doc corpus uses it), and an alias would create two canonical spellings of one type — more cognitive load, not less (§13.9). **Consistency lives at the axis level instead**: `vt`/`tt` are the axis names everywhere — in this spec, in the selector syntax below, and in `::history` output.

### Opt-in at `:create` — the temporal-axis rule

> **The rule (one sentence): temporal axes are the trailing key columns, in the fixed order vt-then-tt; declare any subset — at most one of each.**

```
:create plain   {k => v}                          # no temporal axis (unchanged)
:create vt_rel  {k, v: Validity => val}           # valid time only (unchanged, shipped)
:create audit   {k, tt: TxTime => val}            # transaction time only — system-versioned (new)
:create belief  {entity, v: Validity, tt: TxTime => value}   # bitemporal (new)
```

This is the contributor-requested cardinality constraint ("at most one of each") — with the trailing-position requirement kept, because it is not a stylistic compromise but the physical mechanism: trailing `Reverse`-encoded temporal components are what make all versions of one logical key byte-contiguous and newest-first (`data/memcmp.rs:116-123`), which is what makes the skip-scan one seek per key (`data/tuple.rs:60-84`). *Engine-side column rearrangement* was evaluated and rejected (§12): declared key order **is** the physical primary index in a memcmp store — it determines positional binding (`*rel[a, b, c]`), prefix-seekability (fork gotcha #1), `::columns` output, and the backup wire format (`encode_as_key` has ~20 call sites spanning scans, export/import, compact, and HNSW). A hidden permutation layer would buy no capability, and silent `:create`-time normalization would make positional access bind against an order different from the user's source text — a no-error wrong-binding trap.

**Enforcement — fail at `:create`, not at query time.** Because `TxTime` is a new type with no non-temporal use, malformed declarations are **`:create`-time errors** whose message includes the exact corrected declaration, copy-pasteable. All of these are rejected: `TxTime` not last among keys; `TxTime` before `Validity`; `TxTime` not adjacent to `Validity` (when both are present); more than one `TxTime`; more than one `Validity` when `TxTime` is present; `TxTime` as a non-key (value) column. This is deliberately **stricter than vt's shipped behavior** (a misplaced `Validity` is legal at `:create` and only errors when `@`-queried, `query/ra.rs:350-356` — we can't hard-error there without breaking legal plain-column uses; the `ra.rs` error text should be upgraded to state the required form). One §9 test per rejected shape.

### tt-only relations (system-versioned) — in scope for v1

`{k, tt: TxTime => v}` is a **system-versioned relation**: every write is stamped with commit-tt; the default read is the current state; `@ (tt: T)` time-travels. This is the SQL:2011 system-versioned-table / Datomic shape — arguably the *most common* temporal need (pure audit) — and it **reuses the entire existing single-axis machinery**: the trailing temporal component rides the same encoding and the same `check_key_for_validity` skip-scan, which is purely mechanical about what the axis *means*. Verified deltas — exactly three, not two:

1. **Stamping**: the engine writes the trailing component from the commit clock (§5); it is encoded exactly like a `Validity` value, so the scan runs unmodified.
2. **Rejection**: any user-supplied value for the tt column on `:put`/`import_relations` is an error.
3. **`:rm` remap**: today `remove_from_relation` addresses the exact row by full key *including* the temporal column (`query/stored.rs:136-143`, extractors `:941-946`) — impossible when the user cannot name tt, and a physical delete would break the audit guarantee anyway. On tt-stamped relations `:rm {k}` therefore **appends a retraction at commit-tt** (the tt encoding's flag byte carries assert/retract in tt-only relations — see representation below). Never a physical delete; `::evict` is the only physical delete.

Note the no-fall-through retraction rule — debatable as a *valid-time* modeling choice — is **definitionally correct** for transaction time: "latest belief ≤ T is 'deleted'" means the key is absent as of T.

**Answer to the review question "is a lone `TxTime` just a `Validity` with default-NOW?" — no.** Mechanically the storage is identical; semantically they differ on every load-bearing property. vt's default is wall-clock captured once per *script* (`runtime/db.rs:348`), non-monotonic, colliding (same-µs writes silently replace each other), and above all **user-overridable** — history stays falsifiable. tt is commit-scoped, strictly monotonic, collision-free, and **non-overridable — the non-overridability is the audit guarantee**. A default is not a constraint. (Retraction differs too: a vt retract can be back- or future-dated; a tt retract can only say "deleted at this commit.")

Sequencing: tt-only lands as **step 4a** (§10) — it exercises the commit clock, stamping, rejection, and `:rm` remap through the *already-shipped* single-axis scan, de-risking the genuinely new two-level scan of step 4b.

### `TxTime` representation

- **memcmp encoding mirrors `Validity`**: type tag + `!order_encode_i64(ts)` + one trailing flag byte. In **tt-only** relations the flag byte is the assert/retract bit (deletion support, above). In **bitemporal** relations retract rides vt's `is_assert` (§6), so the tt flag byte **must be 0; non-zero is reserved** for the future group-repudiation marker (§3) — reserving it now costs one byte per row on opt-in relations only and makes the marker additive instead of a relation-format migration.
- Decodes to a Validity-shaped value (implementation may reuse `DataValue::Validity`); displays and serializes as a timestamp.
- Relations declaring `TxTime` are **unreadable by pre-fork/upstream builds** (new `ColType`); state this in the CHANGELOG-FORK entry.
- Rejection of user-supplied tt is a new check in the `:put` column-binding path (`query/stored.rs`) — the real precedent is how import rejects writes into index relations (`ImportIntoIndex`, `runtime/db.rs`), not "computed columns" (Cozo has none).

### Query syntax — one `@` clause, labeled axes

`@!` (the previous proposal) is **withdrawn** — verification found the collision §13.2's rationale denied: `!` is a live prefix operator in expr position (`negate`, `cozoscript.pest:126-128`, wired at `parse/expr.rs:48`), so `@! "NOW"` already parses today as `vt = negate("NOW")` and dies at const-eval; a naive `validity_clause? ~ tx_validity_clause?` grammar would never even reach the tt clause (PEG greedy), and any fix leaves `@!x` vs `@ !x` whitespace-sensitive forever. See §12.

Instead, the **interior** of the existing validity clause is extended with an order-free **labeled form** — the contributor's one-clause instinct, minus the positional footgun of `@ (V, T)` (both axes are timestamps; a silent swap would return wrong answers in the marquee audit feature):

```
*rel{…}                      # no clause: vt bare-scan behavior unchanged; tt defaults to current belief
*rel{…} @ "2026-01-01"       # vt as-of — exactly today's syntax, unchanged
*rel{…} @ (vt: "2026-01-01")                      # explicit spelling of the same
*rel{…} @ (tt: "2026-03-01")                      # the relation as it stood on Mar 1 (vt untouched)
*rel{…} @ (vt: "2026-01-01", tt: "2026-03-01")    # what we believed on Mar 1 was true on Jan 1
*rel{…} @ (tt: "2026-03-01", vt: "2026-01-01")    # same — labels are order-free
```

**Semantics: the axes are independent — each has its own default whether or not the other is specified.** vt keeps its shipped semantics (no selector = the bare scan over all vt records, retracts-as-rows included; a selector = as-of resolution). tt defaults to **current belief**; only an explicit `tt:` selector (or `:as_of`) reaches historical beliefs. This yields the **migration invariant**: adding a `tt: TxTime` column to an existing relation changes no existing query's results (up to corrections actually recorded) — a plain relation's bare scan still returns current rows after becoming tt-only; a vt relation's bare scan still returns its (vt, flag) records, now resolved to current belief per (key, vt); nobody has to add selectors to existing code. Without this default, every count/join would silently multiply by version-count the moment a relation opts in.

| Clause | vt | tt |
|---|---|---|
| *(none)* | bare scan (all vt records — shipped behavior) | current belief (end-of-tt-time) |
| `@ V` / `@ (vt: V)` | as-of V | current belief |
| `@ (tt: T)` | bare scan (all vt records) | as-of T |
| `@ (vt: V, tt: T)` | as-of V | as-of T |

(On a tt-only relation the vt column of this table is vacuous: no clause = current state; `@ (tt: T)` = state as of T; retracted keys are absent once resolved.)

- **Bare `@ E` always means valid time, on every relation.** On a tt-only relation, `@ E` is a clear error ("system-versioned relation; no valid-time axis; use `@ (tt: …)`") — the axis must be named rather than inferred, so the same syntax never silently changes meaning across schemas.
- **tt tokens**: `"NOW"` (default), numeric µs, ISO-8601. The tt default and `"NOW"` resolve to **end-of-tt-time** (the `MAX_VALIDITY_TS` sentinel), *not* the wall clock: "current belief" means *all committed beliefs*, and because tt is monotonic the two only differ after a backward wall-clock step — where wall-clock-NOW would silently hide the newest beliefs (HWM > wall clock). End-of-tt-time is also what preserves the single-seek fast path. `"END"` is accepted as a synonym; **there is no "earliest belief" token** — an as-of point before the first record correctly answers "nothing was known"; "what did we originally believe" is a `::history` question. (The previous draft's `@! "END"` = "earliest belief" was doubly wrong: in `expr2vld_spec`, `"END"` maps to `MAX_VALIDITY_TS` = end-of-time, `parse/query.rs:1091`.)
- **Quote your dates.** `@ 2026-01-01` (unquoted) is integer arithmetic — `2026-1-1 = 2024` µs-since-epoch — and is silently accepted as a vt spec (no date literal exists in the grammar, `cozoscript.pest:216`). This footgun ships today; all examples in this spec are quoted. A parse-time lint (a bare small-integer constant in temporal position warning "did you mean a quoted ISO date?") was deferred from step 4 — no warning channel exists; revisit with a diagnostics channel (see §10 step 4b).
- **Errors**: `@ (tt: …)` against a relation with no tt axis errors at RA-build time (same site as `InvalidTimeTravelScanning`, `query/ra.rs:350-356`). The validity clause remains syntactically unavailable on rule/temp-store atoms and on `search_apply` (FTS/HNSW) atoms — the grammar has never attached it there; document that temporal selection applies to stored-relation atoms only.
- **Per-atom granularity is preserved** — each stored-relation atom carries its own selector, so one query can join current state against historical belief.

**Grammar** — only the *interior* of `validity_clause` changes; the four attachment sites (`cozoscript.pest:80,81,88,89`) are untouched:

```pest
validity_clause = {"@" ~ (temporal_axes | expr)}
temporal_axes   = {"(" ~ axis_pair ~ ("," ~ axis_pair)? ~ ")"}
axis_pair       = {("vt" | "tt") ~ ":" ~ expr}
```

`@ ("NOW")` still parses as vt via the `grouping` fallback (`cozoscript.pest:134`); `@ (vt: …)` is a parse error today, so the form is claimed backward-compatibly. `vt`/`tt` are **soft labels** recognized only after `@ (` — no reserved-word pollution, no collision with columns/vars named `vt`/`tt` (keep that containment when the clause grammar evolves). Duplicate labels are a parse error with a span. Each axis expr parses via `expr2vld_spec` (or its tt twin) at the existing call sites (`parse/query.rs:695,759,941,986`), landing in `valid_at` + a new `tx_valid_at: Option<ValidityTs>` on `FixedRuleArg::{Stored,NamedStored}`. A terse per-atom alias (e.g. `@tt E` with the standard `!XID_CONTINUE` keyword guard, cf. `or_op` at `cozoscript.pest:93`) can be **added later without breakage** if partners want brevity; shipping it first would leave two spellings forever.

### `:as_of` — whole-query belief pinning

The spec's #1 motivating use case ("re-run the report exactly as it would have answered last Tuesday," §1) is whole-query, and with per-atom-only syntax, forgetting the selector on one atom of a multi-atom rule silently mixes current belief into a "reproduced" report — invisible by construction. So v1 ships one query option:

```
?[…] := *belief{…}, *audit{…}
:as_of "2026-06-24T09:00:00"
```

`:as_of <expr>` (grammar: the `:timeout expr` pattern, `cozoscript.pest:136-156`) sets the default tt for **every tt-stamped relation atom in that query block** that lacks an explicit `tt:` selector; explicit per-atom selectors win. Scope is per query block, like every other epilogue option — multi-block scripts repeat it per block. Using `:as_of` in a block that references no tt-stamped relation is an error (typo guard). Plain and vt-only relations are unaffected — a mixed query is only **partially** reproducible, which the docs must say loudly. No vt analog (valid-time defaults are not a reproducibility hazard). This is deliberately the entire feature: no strict mode, no session-level pin — additive later if pulled (§13.8).

## 5. The transaction clock (the new, load-bearing mechanism)

tt must be **monotonic, collision-free, and identical for every write in one transaction** (a transaction is one atomic belief-update). Today none of this exists. Design:

- A **monotonic, wall-clock-floored commit counter**: `tt = max(physical_now_µs, last_tt + 1)`. Wall-clock-meaningful, strictly monotonic, never collides even within a microsecond or across a backward clock step. (Deliberately *not* a true hybrid logical clock — there is no separate logical component and no cross-node causality merge, neither of which a single-process embedded DB needs.)
- **In-process source of truth = an `AtomicI64` high-water mark** on the `Db`, **seeded at open** as `max(persisted system key, wall clock)` and **persisted inside every committing write-transaction** that touched a tt-stamped relation — the persist rides the same storage transaction as the data, so "persisted HWM ≥ every committed tt" holds atomically and no crash window exists between advancing and persisting.

### tt order must equal commit order (the visibility requirement)

Reproducibility demands: **a reader must never observe rows appearing at a tt ≤ a point it has already read.** Freezing tt at the transaction's *first bitemporal write* violates this — two concurrent write transactions can commit in the opposite order of their frozen tts (A freezes tt=100, B freezes tt=101 and commits; a reader queries; A commits later, inserting history *beneath* the reader's horizon). RocksDB pessimistic transactions permit exactly this interleaving. Resolution (§13.10):

- **Allocate tt at commit, with buffered stamping.** `:put`s into tt-stamped relations buffer their rows on `SessionTx` during statement execution; `commit_tx` takes a short per-`Db` critical section — allocate tt from the atomic, encode + write the buffered rows, commit, persist the HWM — and releases. tt order, commit order, and visibility order coincide by construction, and the critical section holds no user-visible locks (buffered bitemporal keys are fresh — nobody else can hold them — and the HWM system key is only ever touched inside this section), so it cannot deadlock against user transactions.
- **Documented consequence — deferred read-your-writes within one script**: a multi-block script that writes a tt-stamped relation and *reads it back in a later block* sees pre-transaction state; the writes materialize at commit. This matches the model — a transaction is *one* belief event, and shouldn't query its own half-formed belief — but it is a real divergence from vt relations (where later blocks see earlier blocks' puts) and must be documented + pinned in §9.
- Rejected: *hold a mutex from first write to commit* (a bitemporal tx blocked on a user row-lock while holding the mutex stalls every other bitemporal committer — lock-timeout livelock); *stable-tt watermark for readers* (correct, but a shared in-flight set is precisely the contended hot structure the 0.8.4 lesson warns about); *document the anomaly* (breaks the marquee guarantee — not acceptable).

**Concurrency caution (heed the 0.8.4 `avgdl` lesson).** The 0.8.4 changelog shows a single shared hot storage key, read-modify-written inside every transaction under RocksDB pessimistic locking, serialized all writers and lost updates. The tt HWM must **not** repeat that: the atomic is the in-process authority (no per-write storage read), the persist is a single put on the committing tx, and the commit critical section is O(buffered rows) with no user code inside. Belief-update workloads are not bulk-ingest-rate; if a high-contention bitemporal write path ever appears, batch the persist and revisit.

**Single-instance authority.** The atomic is authoritative only while exactly **one live `Db` instance** writes the store. rocksdb enforces this with its `LOCK` file. **sqlite does not** — two handles on one file would run independent HWMs (duplicate/non-monotonic tts); document tt-stamped relations on sqlite as single-handle-only (or take an advisory lock when one exists). The `mem` backend keeps the HWM unpersisted — consistent with everything else about `mem`.

## 6. Write path

| Op on a tt-stamped relation | Semantics |
|---|---|
| `:put` | Row buffered; engine appends commit-tt at commit (§5). User-supplied tt → error (§4). On bitemporal relations assert/retract continues to ride vt's `is_assert`, unchanged — a retraction at (vt=V, tt=T) records "as of T we believe the fact ceased to hold at V"; corrections are new rows at higher tt (worked table in §3). |
| `:put` same (key, vt) twice in one tx | Both rows would carry the same (vt, tt) — identical full key, silent last-write-wins. **v1 documents this** (one belief event per tx; last statement wins) — upgrading to an error needs SessionTx-local dedup, deferred. §9 pins the behavior either way. |
| assert **and** retract of one (key, vt) in one tx | **Error.** Same tt on both → unbreakable resolution tie (§3). |
| `:rm` | **Appends a retraction at commit-tt** on tt-only relations (flag byte; values snapshot the key's latest row at statement time). Never a physical delete (§4). On **bitemporal** relations `:rm {k, vt}` performs the cessation remap (shipped in step 4c); removal without a vt is a valid-time statement (`:put` with vt `"RETRACT"`). Same-transaction put+rm of one key is rejected (tie). |
| `:update` / `:delete` / `:replace` | `:delete` = `:rm` with an existence assertion (fails on missing **or believed-deleted** keys). `:update` targets the current belief's own vt-group (shipped in step 4c); `:replace` of an existing TxTime relation rejected outright (Cozo's `:replace` is destroy-and-recreate — it would drop history, which is not expressible on a tt axis). |
| `:insert` / `:ensure` / `:ensure_not` | Existence checks evaluate at (vt=NOW, tt=current-belief). |
| `import_relations` (user data import, `runtime/db.rs:561`) | Rejects user-supplied tt; stamps **one tt per batch** (a bulk import is one belief event; §13.3). Bypasses per-row vt defaulting — document. |
| `import_from_backup` / restore (`runtime/db.rs:710,743`) | `restore_backup` (whole store, empty target) **preserves tt bytes verbatim** and **re-seeds the HWM from the backup's persisted mark** (sufficient: persisted ≥ every committed tt inside any consistent backup — no row scan needed). *Step-3 deviation:* `import_from_backup` is **rejected** for TxTime relations — a partial import into a live store carries a foreign clock's tts; restore the full store or re-ingest. The two import paths must never be conflated: user import stamps, restore preserves. |

## 7. System operations

| Op | Semantics | Notes |
|---|---|---|
| `::history rel {k…}` | All (vt, tt) records for the given key(s) — the full belief timeline. Signature mirrors `::columns`/`::indices` conventions: full key required in v1 (prefix/whole-relation forms deferred); output columns `…keys, vt_ts, op, tt, …values` (`op` ∈ assert/retract; on tt-only relations `vt_ts` is absent); ordering key-asc, vt-desc, tt-desc; `limit`/`offset` options. On a vt-only or plain relation: clear error. | Read-only; the introspection surface. |
| `::history_gc rel before_tt` | Drop records with `tt < before_tt` **that are superseded** — preserve, per (key, vt), the latest belief at or before the cutoff so that as-of-now and as-of-(≥cutoff) answers are unchanged. **Persists a per-relation `tt_gc_floor`** in metadata; a subsequent `@ (tt: T)` with `T < floor` is an **error** — without the floor, post-GC historical queries would silently return a reconstruction presented as the historical belief. Deletion runs in a single transaction in v1 (chunked online gc deferred until pulled by relation size — see §10 step 5). | MariaDB-shaped, per-relation. Mutating → read-only-guarded like its siblings. |
| `::evict rel {k…}` | **Hard-delete every record** (all vt, all tt) for a key. The one intentional break of append-only, for GDPR/right-to-be-forgotten. Writes an audit row **in the same transaction** to a reserved system relation (schema: relation name, **salted key-hash** — storing the key itself would re-enshrine the PII the eviction removes; an explicit `unredacted` flag opts out — row count, eviction tt). | Read-only-guarded. |

(`::describe` is the precedent for a sys op that mutates metadata and is read-only-guarded — see CHANGELOG-FORK 0.8.5.)

## 8. Backward compatibility & backend parity

- **Non-temporal relations**: byte-identical encoding, zero new cost. Pinned by a "single-axis unchanged" test.
- **vt-only relations** (`Validity` last, no `TxTime`): unchanged — `@ vt` behaves exactly as today; `@ (tt: …)` on them is a clear error.
- **Secondary & search indexes**: `::index create` on a tt-stamped relation is **rejected in v1** (deferred — buffered stamping conflicts with statement-time index maintenance, and there are zero consumers; see §10 step 5 deviation). The design when legalized: index tuples are written in the same per-index put loop (`query/stored.rs`) and must carry the same commit-tt (buffered with the base rows, §5); backfill preserves existing tts; `choose_index`'s trailing-validity check (`runtime/relation.rs:222-225`) generalizes to "the temporal-axis suffix must trail in the index too." `::fts`/`::hnsw`/`::lsh` creation on tt-stamped relations is **rejected in v1** with a clear error (search indexes have no versioned-read story; revisit on real pull — and note `search_apply` atoms take no validity clause anyway, §4).
- **Backends**: all logic is key-encoding + scan, so it rides the existing `range_skip_scan_tuple` trait across **rocks / sqlite / mem** by generalizing it to a two-axis `range_bitemporal_scan_tuple` (the single-axis function stays for vt-only *and tt-only* relations). **No RocksDB user-defined timestamps** — rejected (§12). sqlite single-handle caveat in §5.
- **Pre-fork readers** cannot open relations declaring `TxTime` (§4) — CHANGELOG-FORK + docs note.

## 9. Testing & performance budget

- **Backend for tests: SQLite + `tempfile::tempdir()`** (the `mem` backend uses a different join operator; stored/scan-path regressions must use sqlite).
- **Clock**: monotonicity across same-µs writes; simulated backward wall-clock step (writes stay monotone; default read still sees them — pins end-of-tt-time default, §4); restart re-seed (incl. after restore, §6); crash-atomicity of HWM persist (same-tx).
- **Write path**: user-supplied tt rejected on `:put` **and** `import_relations`; bulk import stamps exactly one tt; same-tx same-(key,vt) double-put pinned (last-write-wins); same-tx assert+retract rejected; `:rm` appends retraction (tt-only and bitemporal); deferred read-your-writes within one script pinned (§5); `:create` rejection of all six malformed `TxTime` shapes (§4).
- **Read path — the four quadrants**: (now, now), (past-vt, now), (now, past-tt), (past-vt, past-tt); the **correction case** (higher-tt row overrides an earlier belief; as-of-past-tt still returns the old belief); **cessation at an existing vt-group** (retract found across the is_assert-run boundary — pins the §3 vt-group definition); **repudiation-by-copy** + its **chained-correction staleness** (documents the §3 composition limit); **equal-(vt.ts, tt) assert-wins tie** and the **currently-unpinned single-axis equal-ts shadowing** (add to `data/tests/validity.rs`); **retraction vs eviction**; the full §4 semantics table (each cell); the **migration invariant** (adding `tt: TxTime` to a plain/vt relation leaves every existing query's results unchanged, up to corrections); `@ E` on tt-only errors, `@ (tt:…)` on vt-only errors, `:as_of` (applies, explicit-wins, no-tt-relation error); unquoted-date lint.
- **Sys ops**: `::history` output schema; `::history_gc` preserves as-of-now and as-of-(≥cutoff) **and** errors below the floor; `::evict` audit row (salted hash, same-tx).
- **Durability**: export→import and backup→restore preserve tt bytes + re-seed HWM; opt-in isolation (non-temporal byte-identical; vt-only unchanged).
- **Regression budget**: ≤~10% on bitemporal relations, **zero** on non-temporal (opt-in), measured against the baseline **"the identical workload on a vt-only relation at `@ "NOW"`"**. (The 10% envelope follows AeonG — a built-in temporal graph DB reporting ~9.75% overhead vs its non-temporal baseline.) Extend `benches/time_travel.rs` with a bitemporal matrix (versions × corrections-depth) and confirm the (now, current-belief) path matches the current single-axis number; add a tt-only current-read parity bench. **MEASURED 2026-07-03 (step 6; criterion medians, 1000 keys, sqlite / rocksdb):** point reads at (now, current-belief) **+3.8–8.8% / +8.2–11.5%** vs the baseline ✓; tt-only current reads at-or-below baseline ✓; non-temporal untouched (dispatch-level) ✓; full scans: 1 version/key **~2× FASTER** than the baseline on both backends (the sequential walk out-runs the skip scan's per-key seeks); deeper matrices over budget (c0 +21–53%, c2 up to ~2×) — *recorded deviation*: the two-level walk's structural floor is two backend probes per key (assert + retract run) vs the single-axis scan's one, and corrections are physical rows a vt-only relation cannot represent; revisit only on a real scan-heavy deep-version workload. Getting here required the step-6 pinned-cursor overrides + byte-spliced bounds + landing reuse — the generic per-probe default measured 4–8×.

## 10. Phased implementation plan

1. **Spec + design review** — this doc; §13 sign-off. *(done 2026-07-02)*
2. **Transaction-commit clock** — `AtomicI64` HWM on `Db`, persisted system key (same-tx), commit-time allocation on `SessionTx` under the §13.10 critical section (§5). Testable in isolation (monotonicity, restart, backward-clock, crash-atomicity). *Foundational; no user-visible surface yet; depends on no §13 revision. **Shipped 2026-07-03** (CHANGELOG-FORK 0.10.0) — incl. review hardening: snapshot-validation bypass for the HWM key on RocksDB, monotone seeding, loud corruption bail, poison recovery, wasm cfg-guard.*
3. **Schema opt-in + key encoding + write path** — `TxTime` ColType + grammar + the six `:create` rejections (§4); TxTime encoding incl. reserved flag byte; **buffered stamping** (rows buffered on `SessionTx`, stamped at commit by the step-2 clock) + user-tt rejection + `:rm` remap + same-tx rules (§6); both import paths (§6), **incl. the restore re-seed** (`max(persisted, max restored tt, wall clock)` — the monotone `fetch_max` seed makes it safe to call) and the HWM+rows same-tx atomicity test owed by step 2.
4. **Read path** — *(also owes: the bitemporal `:rm {k, vt}` remap and `:update`/`:insert`/`:ensure` semantics deferred from step 3 — see §6.)* **4a: tt-only reads — SHIPPED 2026-07-03** (labeled selector, current-state default, negation fix; CHANGELOG-FORK 0.10.0) (relax the `ra.rs:442-448` type gate; selector parsing; rides the existing single-axis scan — de-risks 4b). **4b: bitemporal reads — SHIPPED 2026-07-03** (CHANGELOG-FORK 0.10.0): labeled selectors + the two-level probe-driven scan (generic across backends; per-backend seek overrides = step 6) + `:as_of`. *Deferrals recorded:* the date lint (no warning channel — revisit with a diagnostics channel). **4c: SHIPPED 2026-07-03** — the §6 write ops (`:insert` incl. bitemporal no-records-at-any-vt semantics, `:update` targeting the current belief's own vt-group, `:ensure`/`:ensure_not` with temporal-binding rejection, bitemporal `:rm {k, vt}` cessation remap); one-belief-event conflicts throughout (CHANGELOG-FORK 0.10.0). *Review fix:* temporal-column joins fall back to materialized joins — incl. the pre-existing upstream defect on vt-only relations.
5. **Sys ops — SHIPPED 2026-07-03** (CHANGELOG-FORK 0.10.0): `::history` (+limit/offset), `::history_gc` + persisted floor (below-floor reads error; v1 single-tx — chunked online gc deferred on size pull), `::evict` + salted-hash audit rows in `mnestic_evict_audit` (same-tx; `unredacted` opt-out), read-only-guarded. *Deviation recorded:* B-tree index legalization on tt relations deferred (buffered stamping vs statement-time index maintenance; zero consumers); search-index rejections shipped in step 3.
6. **Benches + budget validation — SHIPPED 2026-07-03** (CHANGELOG-FORK 0.10.0): `benches/time_travel.rs` rewritten as a stable criterion matrix (versions × corrections, point + scan, sqlite/rocksdb via `MNESTIC_BACKEND`); §9 gates measured — fast-path parity ✓ (+3.8–11.5%), tt-only parity ✓, v1 scans ~2× faster than baseline; deep-version scan deviation recorded in §9. Landed the anticipated per-backend seek overrides (pinned cursor + `HybridProbe` far-hint), byte-spliced probe bounds, lazy landing decode, and landing reuse in `data/bitemporal.rs`.

Each step is its own PR with a failing test first, CHANGELOG-FORK entry, and `cargo test -p mnestic --lib` green. Steps 2–4 are the substance; 1 and 6 bracket them.

## 11. Open decisions

All resolved and signed off — see §13 (items 1–5 resolved 2026-06-30; 1–2 revised and 6–10 added 2026-07-02; owner sign-off on all ten recorded 2026-07-02):

1. **Opt-in surface** → RESOLVED, revised — §13.1 (named `TxTime` column; temporal-axis rule; `:create` enforcement; tt-only in scope).
2. **Syntax for tt selector** → RESOLVED, revised — §13.2 (labeled `@ (vt: …, tt: …)`; `@!` withdrawn).
3. **Bulk path** → RESOLVED — §13.3 (stamp one tt per batch).
4. **Relationship to MindGraph's tombstones/supersession** → RESOLVED — §13.4 (compose, don't collapse).
5. **Event-time** → RESOLVED — §13.5 (out of scope; two axes only).

## 12. Rejected alternatives (with evidence)

- **RocksDB user-defined timestamps** — per-CF all-or-nothing, +8 B/key on every index, single global GC floor, no SQLite parity. TiKV and CRDB both rejected it for key encoding. ❌
- **SQL:2011 interval-column rewrites / interval-tree indexes** — wrong shape for a memcmp-keyed LSM store; reintroduce range-overlap complexity the skip-scan avoids. ❌
- **Default-on bitemporality** — SQL Server measures 10–20% high-write overhead; opt-in keeps non-temporal relations at exactly zero. ❌
- **Temporal path algebra / tri-temporal** — scope explosion; not pulled by any real need. ❌
- **`@! <expr>` tt selector** *(withdrawn 2026-07-02)* — `!` is a live prefix operator in expr position (`negate`, `cozoscript.pest:126-128`), so `@! X` parses **today** as `vt = negate(X)`; the previous "unused in relation-access position" claim was wrong. A naive grammar addition mis-parses standalone `@! T` (PEG greedy); the lookahead fix leaves `@!x` / `@ !x` whitespace-sensitivity forever; and `!` connotes negation. Any future `!`-prefixed clause proposal in relation-access position has the same collision — don't re-propose. ❌
- **Positional pair `@ (V, T)`** (contributor's literal proposal) — grammatically claimable (a two-element paren form is a parse error today), but both axes are timestamps: a silent vt/tt swap is unde­tectable and returns wrong answers in the audit feature itself; and it breaks on tt-only relations (bare `@ T` would have to change meaning per schema). The labeled form keeps the one-clause shape and kills the footgun. ❌
- **List convention `@ [V, T]`** — "zero grammar change" is a trap: the `@` expr is const-evaluated *with params substituted at parse time* (`parse/expr.rs:186-194`), so `@ $p` with a caller-supplied 2-list would silently flip a vt point query into a bitemporal selector — value-shape-driven semantics, injection-shaped. ❌
- **`@@` / `@tx` sigils** — `@@` visually ambiguous; `@tx`-style is feasible with a keyword guard (`!XID_CONTINUE`, cf. `cozoscript.pest:93`) and is the natural **later terse alias** for `@ (tt: …)` if partners want brevity — but not the primary form (two stacked sigil-clauses, no tt-only story). Deferred, not rejected outright. ⏸
- **`VTime` alias / symmetric type renames** — `::columns` Display prints `Validity` (`data/relation.rs:43`), the shipped corpus says `Validity`, and symmetric names would misrepresent genuinely asymmetric types (§4). Two spellings for one type is a permanent doc tax; axis-level naming (vt/tt) delivers the consistency instead. ❌
- **Engine-side column rearrangement / silent `:create` normalization** (contributor's relaxation; re-challenged in round 2, 2026-07-02: *"trailing position in the database is fine; trailing position in `:create` is debatable — let the parser move the columns internally; the authoring surface does not need to follow the internal representation"*). The principle is right in general — most languages let declaration order diverge from representation — but the premise fails for CozoScript, on three grounds. **(1) `:create` column order is already semantics, not presentation.** The declared key tuple *is* the physical primary index: it defines sort order, which prefix lookups are seekable (no cost-based optimizer — "the human is the optimizer" is the engine contract, and index matching is prefix-only, fork gotcha #1), and positional binding (`*rel[a, b, c]` and fixed-rule args bind by declared position). There is no merely-cosmetic ordering available to free up; the trailing-temporal rule adds one constraint to an ordering that is already meaningful everywhere. This is the `repr(C)` case, not the `repr(Rust)` case — the declaration is the layout contract. **(2) Silent reordering trades a one-time error for a permanent read hazard.** Under normalization, `{tt: TxTime, k => v}` followed by `*rel[t, k]` binds `k` into `t` with no error; `::columns`, export, and backup round-trips all show an order the source text doesn't. An error at `:create` teaches once, at authorship; normalization mis-teaches every future reader. (Even SQL — the least physical query surface — never reorders declared columns.) **(3) It cannot be done uniformly.** A non-trailing `Validity` is legal *today* as a plain data column (shipped semantics; the parser cannot know whether a mid-key `Validity` is a mistake or a deliberate sort column), so only `TxTime` could float — two temporal types with different declaration behavior is strictly more cognitive load, by the review's own criterion. The full-permutation alternative (logical→physical mapping at every tuple boundary: ~20 `encode_as_key` call sites, write-path extractors `query/stored.rs:941-946`, backup wire format) buys no capability for its blast radius. **The ergonomics obligation is conceded and lands on the diagnostic**: the `:create` rejection includes the exact corrected declaration, copy-pasteable (§4) — the parser *could* trivially accept-and-reorder; declining is deliberate, because in this engine a declaration is a physical promise and the engine won't silently rewrite promises. *(Round 2 closed 2026-07-02: challenger parked it — "remains debatable and at the same time is probably not worth it here and now.")* ❌
- **tt as a provenance annotation instead of a storage axis** *(design-partner proposal, 2026-07-02: compose DatalogMTL-style valid-time reasoning with the provenance-semiring layer carrying `⟨asserted-at, superseded-at⟩` tags — "bitemporal isn't a feature to acquire, it's a composition")*. Right framing **for a reasoning layer built on someone else's store** — and it validates our layering: the composition is exactly what provenance-semirings **R3** (retraction/TMS under time-travel) is — shipped in 0.10.0 as the `:reconcile` relation op, and §13.4 already records "compose, don't collapse." But it does not *replace* the storage axis, for three engine-level reasons. (1) **The annotation still needs the clock**: `asserted-at` must be stamped monotonically, atomically, and non-falsifiably at commit — that mechanism *is* §5; relocating where the value lives doesn't remove the substrate. (2) **Annotations don't give as-of reads**: tags as row data mean every historical read is a filter over `superseded-at` on every scan and index (the SQL:2011 pattern), not one seek — the trailing-key encoding is what makes "what did we believe at T" O(seek), and `::history_gc`/`::evict` (GDPR) need the storage layer to understand the axis. (3) **Base vs derived**: semiring tags annotate *derivations*; R2 (shipped in 0.10.0) resolved persistence as tags-as-columns via `:put` — no new key format — and annotated belief history works on tt relations. XTDB bakes tt into the store not for lack of a provenance layer but because tt is a transaction-manager concern. The two compose — engine tt for base facts, semiring provenance for derivations, R3 as the join — rather than substitute. ❌ (as a replacement) / ✅ (as the R3 composition, already sequenced)
- **DatalogMTL interval operators (⊟/⊞/◆/◇/S/U) as the vt query surface** — valid-time *interval reasoning* ("held continuously for 3 months") is a real capability gap distinct from valid-time *storage* (which we have), and possible future work over the existing vt axis. Not this spec: scope explosion (cf. the temporal-path-algebra rejection), and no current consumer pull. Two design lessons recorded for whenever it is pulled: Vadalog's own optimizer shows the four unary operators aren't closed under composition — the engine internally rewrites them to a generalized interval-transform operator `T⟨e1,e2⟩`, so the operator surface is ergonomics, not the semantic core (target the general form, skin the operators); and S/U (since/until) are excluded from that rewriting — structurally second-class, a genuine complexity boundary. ⏸

## 13. Resolutions (2026-06-30; items 1–2 revised & 6–10 added 2026-07-02 — **signed off by owner 2026-07-02**)

The 2026-07-02 revision was triggered by external contributor review (three hesitations: the positional column rule "feels like a compromise"; `Validity`/`TxTime` naming inconsistency; `@!` "throws me for a loop") plus a verification pass that confirmed real defects behind two of the three (the `@!` grammar collision; the ambiguity/diagnostics story around the positional rule) and found independent errors (the `"END"` gloss; the vt-group ambiguity; the clock-visibility hazard). Each resolution records rationale and rejected alternatives so the decision is auditable. (Historical: step 2 — the commit clock — depended on none of these and began immediately; these gated step 3 onward. All steps have since shipped in 0.10.0.)

1. **Opt-in surface → a named `TxTime` column type under the temporal-axis rule** *(revised)*. Temporal axes are the trailing key columns, fixed order vt-then-tt, at most one of each, any subset declarable — including **tt-only** (see 6). Malformed `TxTime` declarations are **`:create`-time errors** naming the exact required form (six rejected shapes, §4) — stricter than vt's shipped query-time-only enforcement, deliberately. The user names the column (not engine-hidden) so `::history` can project it and binder errors can reference it. The column is engine-assigned; supplying a value on any write path is an error.
   _Rejected:_ relation-level `bitemporal` flag (less explicit; no `::history` projection name); reusing `Validity` with a second mode (overloads a stable fast-path-critical type); **cardinality-only constraint with engine column rearrangement, and silent `:create` normalization** (§12, incl. the round-2 authoring-vs-storage challenge — in CozoScript declaration order is already semantics; silent reordering is a permanent read hazard; and it can't be uniform across Validity/TxTime).

2. **Transaction-time selector → the labeled axis form `@ (vt: …, tt: …)`** *(revised — `@!` withdrawn)*. Bare `@ E` stays valid-time everywhere, forever; `@ (tt: T)` selects belief; labels are order-free, duplicates error; unlabeled axes default per §4's table; the tt default is end-of-tt-time. Grammar touches only `validity_clause`'s interior — lighter than `@!`, which needed all four attachment sites *plus* a collision fix. A terse alias (`@tt E`) is additive later.
   _Rejected:_ `@!` (verified grammar collision + permanent whitespace hazard, §12); positional `@ (V, T)` (silent-swap footgun, no tt-only story); `@ [V, T]` list convention (param-shape injection); `@@`/`@tx` as primary (§12).

3. **Bulk path (`import_relations`) → stamp one tt per batch.** A bulk import is one belief event; user-supplied tt rejected; bypassed per-row vt defaulting documented. **Distinct from backup restore, which preserves tt bytes verbatim and re-seeds the HWM** (§6) — conflating the two would either falsify audit history or break restores.
   _Rejected:_ hard-rejecting tt-stamped relations from bulk import (kills backfills/restores, which are exactly belief-bulk events).

4. **Relationship to MindGraph's tombstones/supersession → compose, do not collapse (for now).** Unchanged from 2026-06-30: the engine ships the mechanism; MindGraph keeps app-level tombstones + `Supersedes` this round (platform work specced separately). First concrete consumer that would pull unification was thought to be **provenance-semirings R3** — but R3 shipped in 0.10.0 as `:reconcile` without pulling it; next candidate consumer is the platform valid-time work (`mindgraph/docs/plans/bitemporal-valid-time.md`), not started. Was not a blocker for steps 2–6 (all shipped).

5. **Event-time → confirmed out of scope.** Two axes only (vt = true-in-world, tt = known-to-DB). "When the real-world event occurred" stays a domain column, never a third temporal axis.

6. **tt-only (system-versioned) relations → in scope for v1** *(new)*. `{k, tt: TxTime => v}` reuses the entire single-axis machinery with three deltas (stamp, reject, `:rm`-remap — §4); lands as step 4a, de-risking the two-level scan. A lone `TxTime` is **not** "`Validity` with default-NOW" — engine-assigned, monotonic, collision-free, non-overridable is a different contract, and the non-overridability *is* the audit guarantee.
   _Rejected:_ deferring tt-only (keeps the arbitrary-feeling "TxTime requires Validity" coupling — the exact friction the review flagged — and unserves the most common temporal use case); letting bare `@ T` mean tt on tt-only relations (same syntax, schema-dependent meaning — the cognitive-load bleed the review objected to).

7. **Correction semantics → document repudiation-by-copy; reserve the marker byte; defer the marker** *(new)*. §3 now defines the vt-group (both is_assert runs — non-deferrable, silent-wrong-answer risk), the three correction shapes, the copy idiom, and its composition limit. The `TxTime` flag byte is required-0/reserved on bitemporal relations so a future tt-level group-repudiation marker is additive. **Trigger to build the marker:** provenance-semirings R3 (TMS/retraction) or the first design-partner case of chained corrections on a repudiated vt-group. *(R3 shipped in 0.10.0 as `:reconcile` without needing the marker — the remaining trigger is the design-partner chained-corrections case.)*
   _Rejected:_ building the marker now (new write surface + three-state hot-path resolution + a gc invariant, zero current consumers — overbuild); shipping TxTime as a bare 8-byte component (retrofit = relation-format migration).

8. **`:as_of` query option → in scope for v1, tightly scoped** *(new)*. Per-block default tt; explicit selectors win; error when no tt-stamped relation is referenced; partial-reproducibility on mixed queries documented. Serves the #1 motivating use case; cost is one option rule + a parse-time default into machinery step 4 builds anyway.
   _Rejected:_ per-atom-only (one forgotten selector silently corrupts a "reproduced" report); session-level pins / strict mode (speculative — additive later).

9. **Naming → keep `Validity` + `TxTime`; consistency at the axis level (vt/tt)** *(new — answers the review's second hesitation)*. The types are structurally asymmetric and honestly named; `vt`/`tt` are the consistent pair, used in the selector, `::history` columns, and all docs; §4's terminology table is the canonical statement.
   _Rejected:_ `VTime` alias (introspection prints `Validity`; two spellings, one type); wholesale rename (breaks shipped scripts).

10. **Clock allocation & visibility → commit-time allocation with buffered stamping** *(new)*. tt order == commit order == visibility order by construction; HWM persist rides the committing tx (no crash window); documented deferred read-your-writes within one script (§5). Single-instance authority invariant stated per backend (sqlite caveat).
    _Rejected:_ freeze-at-first-write (late-committing straggler inserts history beneath a reader's horizon — breaks reproducibility, the marquee); first-write mutex held to commit (lock-timeout livelock against user row-locks); reader watermark (shared in-flight set = the 0.8.4-shaped hot structure); documenting the anomaly (not acceptable for the marquee guarantee).
