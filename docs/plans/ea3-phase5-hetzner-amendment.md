# EA3 (`mnestic#91`) — Phase 5 Hetzner Dedicated-Core Recording-Control Amendment

Date: November 21, 2026.  
Supersedes: [ea3-phase4-hetzner-amendment.md](ea3-phase4-hetzner-amendment.md) ONLY on **Phase 5 mutation control sample size (`MUTATION_PAIRS = 1,000`, `N = 10,000` across all 18 controls), bit-exact `numpy.partition` bootstrap acceleration, and Stage 1 wall budget (`12,600 s`)**. Zero engine-code edits relative to CI-green candidate engine commit `fe6e0403`; all 180 × 30 s host preflights, all Stage 1 `<= 1.05` p50/p99 gates, and all Stage 2 NVMe EA3 release gates remain **100% unchanged**.

## Prior Failed Phases (Zero Pooling)

1. **Fly.io `performance-4x` (`benchmark-artifacts/ea3-fly-20261120/stage1/`):** Failed under 90.8% Firecracker hypervisor CPU steal and 7,200 s budget exhaustion (`172/180`). App destroyed.
2. **Hetzner `CCX23` Phase 3 (`benchmark-artifacts/ea3-hetzner-20261120/stage1/`):** `180/180` blocks, `0 / 918` steal samples, `18/18` p50 passed, `6/18` p99 passed (`N = 1,000` per-pair disk FS churn underpowered).
3. **Hetzner `CCX23` Phase 4 (`benchmark-artifacts/ea3-hetzner-phase4-20261121/stage1/`):** `180/180` blocks in `8,446.65 s`, `0 / 1,601` steal samples, **`17/18` controls passed** (`6/6` `N = 10,000` reads `<= 1.0195`, `6/6` baseline `<= 1.0375`, `6/6` `txtime` `<= 1.0307`, `6/6` single `<= 1.0446`). Single miss (`+0.0015`): `candidate-rocksdb-plain-contention` (`N = 5,000`, `p50 = 0.9990`, `p99 = 1.0149`, `conditional_iid u95 = 1.0358`, `hierarchical_block10 u95 = 1.0515`). Exact-null sign-flip simulation on that cell at `N = 5,000` gave `8/10` (`20%` false-fail rate) under `hierarchical_block10`.

**Zero observations from Fly.io, Hetzner Phase 3, Hetzner Phase 4, or prior macOS runs are pooled or reused.**

## Phase 5 Stage 1 Changes (Reviewer Option B — Identical Across Arms)

1. **Uniform `N = 10,000` across all 18 controls:**
   - Read controls remain `shared_readonly = true`, `1,000` pairs/block × `10` blocks = `N = 10,000` (`warmup = 100`, `logical_sha256` on all backends + byte-exact file equality on SQLite).
   - Mutation controls (`single` and `contention`) increase from `500` to **`1,000` pairs/block × `10` blocks = `N = 10,000`** (`sub_block = 100`, `warmup = 20`, `--scratch-dir /dev/shm/ea3-phase5-scratch`), reducing the `hierarchical_block10` half-width by `1/sqrt(2)` (`~0.036 -> ~0.025`).
2. **Bit-exact `numpy.partition` acceleration in `recording_phase3.estimates()`:**
   - Preserves the exact `random.Random(20261114)` draw order (`10,000` draws for both `hierarchical_block10` and `conditional_iid`), replacing only `sorted(values)[k]` with `int(np.partition(values, (k50, k99))[k])`. Verified bit-for-bit identical against pure-Python `sorted()` in `test_recording_phase3.py`.
3. **Unchanged host admission & budget headroom:**
   - Retains all **180 × 30 s** quiet Linux host preflights (`idle_seconds = 30`, `other_cpu_percent = 25`) and continuous 1 s fail-closed `Monitor` (`steal_delta == 0`, zero swapouts, zero competing native processes).
   - Sets Stage 1 wall budget `BUDGET = 12,600 s` (3.5 h; expected wall time `~9,200 s` ≈ 2 h 33 min with ~55 min headroom; `cell_seconds = 300`).
4. **Phase 5 isolation (`phase == 5`):**
   - `inputs.rs`, `recording_phase3.py`, and `ea3_qualify.py` all enforce `phase == 5`, blocking any Phase 3 or Phase 4 artifact from unlocking Stage 2.
   - `ea3_qualify.py` unsets `MNESTIC_OVERHEAD_SCRATCH_DIR` so all Stage 2 qualification cells run **100% on real NVMe disk (`/data/stage2-phase5`)**.
   - All Stage 1 `<= 1.05` thresholds, all Stage 2 EA3 release gates, and all independent `experiment-reviewer` checkpoints remain unchanged.
