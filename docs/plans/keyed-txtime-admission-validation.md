# Admission-fix development validation

November 14, 2026. This follows the preserved diagnostic campaign; it does not
replace failed runs or enable qualification. Main 272c7b7 (0.19.3) is integrated.

After source review and correctness tests, build one release runner with the
new admission policy. Record the merge commit plus source patch and per-file
hashes, native archive/header hashes, toolchain, executable hash and exact plans.

Run two seed-99 RocksDB cells, FTS-create then HNSW-create, once each in fresh
databases. Retain the original 120 rows, 64 dimensions, four writer schedules
and 10 ms pacing. Use a 20-second development deadline, 90-second supervisor
cap, 4 GiB RSS and minimum 2 GiB free disk. Writers continue through publication
and a final acknowledgment each. No writer drain intervention. Plans are frozen
in `keyed-txtime-admission-20261114-v1` before execution. Preserve failures; no
replacement runs or tuning based on these results.

Question: does the new transaction/maintenance admission ordering let these
builds complete while preserving writer progress and final correctness? Require
all planned operations, four final acknowledgments, phase events, valid traces
and verification checks. Record transaction/maintenance admission separately
from the relation and TT section waits. Missing natural phase contention remains
visible; development conformance is not C2 coverage or latency acceptance.

Builds run sequentially with two Cargo jobs; no builds overlap measurements.
Budget: 30 minutes compilation and 5 minutes execution, local only. Background
load is recorded and limits any timing interpretation. Compare completion and
mechanism with prior diagnostics, not numerical speedup across source versions.
Resource failure stops execution and is retained; it is not a passing result.
