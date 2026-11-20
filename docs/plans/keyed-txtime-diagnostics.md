# Keyed/TxTime development diagnostics

Prepared November 14, 2026 against `f7dfcada`. Development seed 99 only;
qualification remains disabled. Original raw runs and thresholds are preserved.

Questions:

1. Does the FTS-create timeout occur while acquiring the exclusive relation
   lock, before reservation/backfill, while ordinary writers continue committing?
2. Does stopping new writer arrivals allow the same pending build to proceed?
3. Do reported publication overlaps occur during the actual TT critical section
   or while the build is still waiting for relation admission?
4. How much of per-writer overhead variation tracks changes in commit/finish
   order, and what do complete four-writer rounds show instead?

Add `test-hooks` spans for relation-read admission, relation-write admission and
build reservation. The read-admission span covers the synchronous single-query
path used by this harness; it is not universal transaction accounting. No
production lock policy changes in this diagnostic phase.

Run two new RocksDB FTS-create cells, each in a separate fresh database, with
identical original 120-row, 64-dimensional seed-99 inputs and original 10 ms
arrival cadence, with a 20-second development deadline. Use the same instrumented
executable and build manifest; save both exact plan hashes before launching.
One preserves arrivals throughout; one stops new arrivals after 2 seconds from
the writer-round start (before worker spawning), retaining actual per-writer
drain timestamps while
allowing already admitted operations to finish, then acknowledges one final
commit per writer after DDL. This drain is explicitly diagnostic, not C2 coverage
or a replacement for the failed original run. Preserve errors and all traces.
One attempt per cell; any further trial requires a labelled diagnostic revision.
Build serially with two Cargo jobs; no concurrent native work during runs. Keep
4 GiB RSS / 2 GiB free-disk limits and a 90-second supervisor deadline. No paid
services; at most 30 minutes build and five minutes execution budget.

Analyze existing 100-pair contention samples at complete-round level, paired by
round/input ID across all four writers; keep per-writer results and failures.
The round statistic is four-operation makespan, `max(end_ns) - min(start_ns)`;
compare on/off p50 and p99 makespans. Compute descriptive ratios plus percentile
95% bootstrap intervals (10,000 draws, seed 20261114), resampling contiguous
blocks of ten paired rounds, preserving all four writers and on/off pairing.
Also report finish-rank transitions. These are exploratory reanalyses of
seen development inputs, not preregistered confirmation or a negligible-bias
claim. An instrument-only microdiagnostic may additionally compare identical
fixed span paths on/off with no database mutation, 100 alternating pairs of
1,000 captures each; report per-capture cost and its limits separately.

Coverage analysis will keep phase envelopes separate from acquired TT-section
intervals and writer admission waits. Neither in-flight scripts nor a build
waiting for admission may be relabelled as a writer waiting on the TT section.
No retrospective C2 pass is permitted. Any proposed qualifying protocol change
must receive independent review before a new frozen campaign.
