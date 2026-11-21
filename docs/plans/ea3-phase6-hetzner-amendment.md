# EA3 (`mnestic#91`) — Phase 6 Hetzner Dedicated-Core Recording-Control Amendment

Date: November 21, 2026.  
Supersedes: [ea3-phase5-hetzner-amendment.md](ea3-phase5-hetzner-amendment.md) ONLY on **Phase 6 Stage 1 read control sample size (`READ_PAIRS = 3,000`, `N = 30,000`) and Option C Stage 1 preflight cadence (`30 s` per block-round + `2 s` per cell)**. Zero engine-code edits relative to CI-green candidate engine commit `fe6e0403`; all Stage 1 `<= 1.05` p50/p99 gates, continuous 1 s `Monitor` on all 180 blocks, Stage 2's 30 s per-cell preflights, and all Stage 2 NVMe EA3 release gates remain **100% unchanged**.

## Prior Failed Phases (Zero Pooling)

1. **Fly.io `performance-4x` (`benchmark-artifacts/ea3-fly-20261120/stage1/`):** Failed under 90.8% Firecracker hypervisor CPU steal and 7,200 s budget exhaustion (`172/180`). App destroyed.
2. **Hetzner `CCX23` Phase 3 (`benchmark-artifacts/ea3-hetzner-20261120/stage1/`):** `180/180` blocks, `0 / 918` steal samples, `18/18` p50 passed, `6/18` p99 passed (`N = 1,000` per-pair disk FS churn underpowered).
3. **Hetzner `CCX23` Phase 4 (`benchmark-artifacts/ea3-hetzner-phase4-20261121/stage1/`):** `180/180` blocks in `8,446.65 s`, `0 / 1,601` steal samples, `17/18` passed (`candidate-rocksdb-plain-contention` `N = 5,000` missed by `0.0015`: `1.0515`).
4. **Hetzner `CCX23` Phase 5 (`benchmark-artifacts/ea3-hetzner-phase5-20261121/stage1/`):** `180/180` blocks in `9,914.51 s`, `0 / 3,120` steal samples, **`17/18` passed** with **12/12 mutation controls (`N = 10,000`) passing with wide margin** (max `p99 u95 = 1.0313`; `candidate-rocksdb-plain-contention = 1.0281`). Single miss: `candidate-sqlite-txtime-read` (`N = 10,000`, `~0.4 s`/block, `p50 = 0.9997`, `conditional_iid u95 = 1.0303`, `hierarchical_block10 u95 = 1.0670` from a 24 µs single-block blip in block 8).

**Zero observations from Fly.io or Hetzner Phases 3, 4, or 5 are pooled or reused.**

## Phase 6 Stage 1 Changes (Reviewer-Approved Design — Identical Across Arms)

1. **Read controls (`READ_PAIRS = 3,000` / block × `10` blocks = `N = 30,000`, `sub_block = 300`):**
   - Triples read block duration (`~0.4 s -> ~1.2 s`) and per-block tail observations (`10 -> 30`) so isolated sub-millisecond OS timer ticks average out cleanly, adding only `~60 s` across all 60 read blocks. Retains `shared_readonly = true`, `warmup = 100`, `ScriptMutability::Immutable`, pre/post `logical_sha256` on all backends, plus byte-exact SQLite file immutability.
2. **Mutation controls (`MUTATION_PAIRS = 1,000` / block × `10` blocks = `N = 10,000`, `sub_block = 100`):**
   - Retained unchanged from Phase 5 (where **12/12** mutation controls passed with max `p99 u95 = 1.0313`), using `--scratch-dir /dev/shm/ea3-phase6-scratch`.
3. **Option C Stage 1 preflight cadence (disclosed prospectively; Stage 2 unchanged):**
   - Full `30 s` quiet preflight (`round_idle_seconds = 30`) before cell index `0` of each of the 10 block-rounds (`10×`).
   - `2 s` quiet preflight (`cell_idle_seconds = 2`, which on Linux executes `1` baseline tick + `2` clean 1 s delta ticks verifying `steal_delta == 0`, zero swapouts, zero competing native processes, `<= 25%` CPU, and `>= 2 GiB` free disk) before each remaining cell (`170×`).
   - Continuous 1 s fail-closed `Monitor` remains active during **every single one of the 180 blocks** (`0 / 5,639` combined steal samples across Phases 3–5 on this dedicated-core host).
   - Stage 2's per-cell `30 s` quiet preflights in `ea3_qualify.py` remain **100% unchanged**.
4. **Phase 6 isolation & budget (`phase == 6`, `BUDGET = 10,800 s`):**
   - `inputs.rs`, `recording_phase3.py`, and `ea3_qualify.py` all enforce `phase == 6`, blocking any Phase 3–5 artifact from unlocking Stage 2.
   - `ea3_qualify.py` unsets `MNESTIC_OVERHEAD_SCRATCH_DIR` so Stage 2 runs **100% on real NVMe disk (`/data/stage2-phase6`)**.
   - Expected Phase 6 Stage 1 wall time: `~5,240 s` (`~1 h 27 min`, ~90 min headroom within `10,800 s`).
