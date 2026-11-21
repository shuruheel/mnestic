# EA3 (`mnestic#91`) — Phase 4 Hetzner Dedicated-Core Recording-Control Amendment

Date: November 21, 2026.  
Supersedes: [ea3-hetzner-linux-amendment.md](ea3-hetzner-linux-amendment.md) ONLY on **Phase 4 Stage 1 recording-control statistical power & scratch placement** (zero engine-code edits relative to CI-green candidate engine commit `fe6e0403`, which adds unlocked chunked catch-up and snapshot-pinned `VectorCache` on top of Phase 3's `7854cb56`; all Stage 1 `<= 1.05` p50/p99 gates and all Stage 2 NVMe EA3 release gates remain **100% unchanged**).

## Prior Failed Phase 3 Dispositions (Zero Pooling)

1. **Fly.io `performance-4x` (`benchmark-artifacts/ea3-fly-20261120/stage1/`):** Failed under 90.8% Firecracker hypervisor CPU steal and 7,200 s budget exhaustion (`172/180`). Fly app destroyed immediately.
2. **Hetzner `CCX23` Phase 3 (`benchmark-artifacts/ea3-hetzner-20261120/stage1/`):** Completed `180/180` blocks cleanly in `6,928 s` with **`0 / 918` active CPU steal samples**, **18/18 p50 passed** (`0.9969`–`1.0048`, `u95` `1.0006`–`1.0216`), and symmetric p99 point ratios (9 < 1.00, 9 > 1.00, mean `0.9970`), yet only `6/18` passed `p99 u95 <= 1.05` (5/6 unmodified `origin/main` baseline controls failed). Independent `experiment-reviewer` exact-null analysis proved `N = 1,000` pairs (`10 × 100` = 10 tail observations) under per-pair snapshot-copy FS churn has near-zero joint power across 18 controls even at exact `0.000%` true overhead (`5/5` exact-null failures on `baseline-sqlite-plain-single`).

**Zero observations from Fly.io, Hetzner Phase 3, or prior macOS runs are pooled or reused.**

## 1. Phase 4 Stage 1 Recording-Control Changes (Shared Identically Across Arms)

All changes live solely in the shared timing runner (`cozo-core/examples/keyed_txtime/{overhead.rs,inputs.rs}`, exported byte-identically to baseline `94595f8e` via `prepare_timing_baseline.py`) and Python orchestration scripts — **zero edits under `cozo-core/src/` or `cozorocks/`**.

1. **Read-only controls (6 controls — `shared_readonly = true`, `PAIRS = 1,000` / block, `N = 10,000` total):**
   - Copy/verify/open/warm (`100` warmups) the seed snapshot **once per block**, then run `1,000` back-to-back alternating `on`/`off` paired reads (`pair % 2 == 0` → `[true, false]`, `pair % 2 == 1` → `[false, true]`) on the same open `DbInstance` (all warmup, measured, and verify ops enforced `ScriptMutability::Immutable`), proving zero logical mutation via pre/post SHA-256 over `::relations` + `export_relations` across every stored relation (`block_start.logical_sha256 == block_end.logical_sha256`) on all backends, plus byte-exact `final_files == seed_inventory` on SQLite (RocksDB rotates `LOG`/`MANIFEST`/`OPTIONS`/WAL metadata on open/close without altering stored relations).
   - Eliminates 200 inter-pair snapshot copies per block, restores microsecond-level temporal pairing, and increases tail observations above `q = 0.99` by **10×** (from 10 to 100).
2. **Mutation controls (12 controls — `shared_readonly = false`, `PAIRS = 500` / block, `warmup = 20`, `N = 5,000` total, `tmpfs` scratch):**
   - Retains full byte-identical matched-snapshot isolation (`initial == seed_inventory` per arm, exact F32-domain oracles, FTS fresh/stale/vector/history verification).
   - Increases pairs from `100` to `500` per block (`5×` tail observations above `q = 0.99`: 50 instead of 10), scaling hierarchical `block10` sub-blocks proportionally (`sub_block = n_pairs // 10`).
   - Places Stage 1's ephemeral per-arm snapshot copies on RAM-backed `tmpfs` (`--scratch-dir /dev/shm/ea3-phase4-scratch`, `MNESTIC_OVERHEAD_SCRATCH_DIR`) while keeping `seed-database`, `snapshots.jsonl`, and `records.jsonl` on NVMe `/data`. Removing NVMe/ext4 `fsync` variance from Stage 1 makes the `<= 1.05` recording-hook check strictly **more conservative** (smaller, cleaner denominator for detecting CPU/TLS recording overhead in `measurement.rs`).
3. **Phase 4 isolation (`phase == 4`):**
   - `inputs.rs`, `recording_phase3.py`, and `ea3_qualify.py` all require `phase == 4`, preventing any Phase 3 artifact from unlocking Phase 4 qualification.
   - `ea3_qualify.py` explicitly unsets `MNESTIC_OVERHEAD_SCRATCH_DIR` so **all Stage 2 qualification cells (`P1`, `T1`, `T2`, `C1`, `C2-*`, `H1`) run 100% on real NVMe disk (`/data/stage2-qual`)**.

## 2. Pre-Freeze Null Power Verification & Unchanged Gates

- Before launching frozen Phase 4 Stage 1, run a bounded development-only power check on `65.108.209.12` (unpooled, discarded before Stage 1) on representative read and mutation controls verifying `p50`/`p99 conservative_upper95 <= 1.05` and clean null power.
- **Every Stage 1 threshold (`p50 u95 <= 1.05`, `p99 u95 <= 1.05`, 18/18 controls, 180/180 blocks, zero retries, `steal_delta == 0`, `10,800 s` budget) and every Stage 2 EA3 release gate (`P1 <= 1.0`, `T1 <= 1.5`, `T2 <= 1.3`, `C1 <= 3×`, `C2 <= 3×`, exact FTS/HNSW oracles, seeds `1234`/`1235`/`1236`) remain unchanged.**
- Mandatory independent `experiment-reviewer` checkpoints remain in place: (1) protocol-before-run `READY` on immutable commit, (2) Stage 1 -> Stage 2 results checkpoint, and (3) Stage 2 results review before un-drafting/merging `shuruheel/mnestic#91`.
