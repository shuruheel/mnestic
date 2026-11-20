# EA3 (`mnestic#91`) — Linux Dedicated Fly VM Protocol Amendment

Date: November 20, 2026.  
Authorized by owner: November 18 ruling (host selection: temporary quiet Fly machine, deleted after campaign) and November 20 go-ahead to spin up the temporary Fly machine and execute the qualifying campaign for `shuruheel/mnestic#91`.

This amendment supersedes only the **host environment**, **baseline/candidate identities**, and **scope exclusion of already-merged EA1/EA2 cells** from [keyed-txtime-measurement-protocol.md](keyed-txtime-measurement-protocol.md), [keyed-txtime-measurement-amendment.md](keyed-txtime-measurement-amendment.md), and [keyed-txtime-controls-phase3.md](keyed-txtime-controls-phase3.md). Every statistical estimator, sample size, quality check, oracle, and latency ratio threshold remains **unchanged**. No earlier macOS observations are pooled or rescued.

## 1. Scope & Immutable Comparison Identities

EA1 (`shuruheel/mnestic#89`, planner keyed access, RocksDB `keyed_access_timing` 381–3414× passed) and EA2 (`shuruheel/mnestic#90`, keyed access for stored negation) are already merged on `origin/main` and live in `Fly v556`/`v557`. Therefore both arms carry identical EA1/EA2/E5/E6–E8 code, and cells `K1`–`K4` are excluded from this EA3-only campaign.

| Item | Identity |
|---|---|
| Baseline (`origin/main`) | `94595f8e9b57941fcd3827aba78d0f8e61843dda` + shared timing runner exported via `scripts/prepare_timing_baseline.py` (`measurement.rs`, `keyed_txtime_timing.rs`, `overhead.rs`, `inputs.rs` byte-identical across arms; zero engine/admission code copied) |
| Candidate (`txtime-search-indexes`, `#91`) | Branch `txtime-search-indexes` head containing `5f2b4e44f18d62d24af312c7852c5e9313985a1d` (9/9 CI checks green) plus this protocol/script amendment commit (zero edits under `cozo-core/src/` or `cozorocks/`) |
| RocksDB vendor | `986b8b9f20893dec811d8ecdb97b6a47f20d322d` (identical static archive built once on the Fly host and verified by SHA-256 across both arms) |
| Reference workload repository | `mnestic-benchmarks` (`scales.yaml:small`, `hybrid_recall/workload/generate.py`, git SHA `984d1ae9bf9e8856e0ac99f3083de77c9688173e` verified & recorded in manifest) |
| Qualifying dataset seeds | `1234`, `1235`, `1236` (fresh database per seed/arm/cell) |

## 2. Host Environment & Linux Admission Rules

- **Host:** Temporary dedicated-CPU Fly.io Linux VM (`performance-4x`, 4 dedicated x86_64 vCPUs, 8 GiB RAM, ephemeral scratch volume >= 30 GiB) created solely for this campaign and destroyed immediately after artifacts are archived and retrieved.
- **Compiler & Runtime Settings (identical across both arms):** `cargo build --locked -p mnestic --features storage-rocksdb,test-hooks --example keyed_txtime_timing --release -j 2`, `CARGO_PROFILE_RELEASE_DEBUG=0`, `CARGO_INCREMENTAL=0`, `RAYON_NUM_THREADS=2`, `OMP_NUM_THREADS=1`, `OPENBLAS_NUM_THREADS=1`. Record `uname -a`, `/proc/cpuinfo` model/cores, `/proc/meminfo`, `rustc -Vv`, and `librocksdb` archive hash in every build manifest.
- **Linux Host Admission (`scripts/benchmark_host.py`):**
  - Replaces macOS `pmset` (battery/low-power) with explicit Linux cloud-VM power state (`platform: "linux"`, `ac_power: true`, `low_power_disabled: true`).
  - Replaces macOS `vm_stat` (`Swapouts:`) with Linux `/proc/vmstat` (`pswpout`). Missing `pswpout` fails closed; any increase during preflight or execution marks `swapout_change` contamination.
  - Retains unchanged Phase 3 thresholds: `>= 2 GiB` free disk, zero competing native processes (`NATIVE`), `<= 25%` competing CPU for 30 consecutive 1-second preflight samples (max 5 minutes), and 5 consecutive competing CPU `> 25%` samples = `sustained_competing_cpu` contamination during a block/cell.
  - Retains unchanged process caps: `4 GiB` RSS, 30 s per ordinary operation, 15 min per index build.

## 3. Stage 1 — Recording Controls (Fresh Linux-Fly Phase 3 Budget: 7,200 s)

- 18 controls (`sqlite`, `rocksdb` × `baseline-plain`, `candidate-plain`, `candidate-txtime` × `read`, `single`, `contention`) × 10 predeclared blocks × 100 paired rounds = 1,000 pairs/control.
- Identical seed-99 120-row/64-d fixture, 100 read-only warm-ups per arm, clean snapshot resets, alternating arm order, exact F32-domain oracles, lossless verified `.jsonl.gz` archiving, and 10,000 paired hierarchical block-10 + conditional-IID bootstrap draws (seed `20261114`).
- **Gate:** Every control requires conservative one-sided 95% upper bound `<= 1.05` on both p50 and p99, and all 180 blocks complete & valid. No contaminated retry in Stage 1.
- **Fail-closed enforcement:** `cozo-core/examples/keyed_txtime/inputs.rs` refuses Stage 2 qualification unless `campaign-completion.json` records `all_recording_controls_passed == true`, `phase == 3`, `completed_blocks == 180`, `scripts_and_protocol_unchanged == true`, and matching SHA-256s for both frozen executables and the controls manifest.

## 4. Stage 2 — EA3 Qualification Cells (Budget: 6 Hours; Runs Only If Stage 1 Passes)

Reference `mnestic-benchmarks` `scales.yaml:small` workload per seed (`1234`, `1235`, `1236`): 10k entities, 50k edges, 40k chunks, 384-d F32 embeddings, 1,000 queries, 200 incremental chunks, `real_embeddings=false`.
Exact unchanged gates from `keyed-txtime-measurement-protocol.md` §4–§6 and `keyed-txtime-measurement-amendment.md`:

| Cell | Backend(s) | Sample & comparison | Unchanged gate |
|---|---|---|---|
| `P1` plain regression | SQLite, RocksDB | Baseline (`94595f8e`) vs candidate (`#91`) plain: 1,000 point reads, 1,000 FTS, 1,000 HNSW, 2,000 updates (+ no-index point-read/update control) | Read p50 and update p50/p99 candidate/baseline **<= 1.0** (one-sided 95% upper bound and every seed) |
| `T1` current search | SQLite, RocksDB | Candidate TxTime vs candidate plain: 1,000 FTS + 1,000 HNSW queries (after 100 warm-up queries) | Search p50 ratio **<= 1.5** separately per FTS/vector/backend; FTS IDs/scores match within `1e-6`; HNSW recall@10 lower bound (TxTime − plain) **>= -0.01** |
| `T2` single writer | SQLite, RocksDB | Candidate TxTime vs candidate plain: 2,000 single-row updates (200 warm-up updates) | Update p50 and p99 ratios each **<= 1.3** |
| `C1` four writers | RocksDB | 4 simultaneous single-row writers × 1,000 ops, disjoint keys, both indexes present | Section-wait p99 **<= 3×** paired T2 TxTime update p99 (and max per-writer p99 conservative gate) |
| `C2-FTS`, `C2-HNSW` | RocksDB | Detached 40k-row index create with 4 same-relation writers at 10 ms cadence | Max `tt_commit_section` wait **<= 3×** T2 TxTime p99 **and** max union(admission, tt_commit_wait) **<= 3×** T2 TxTime p99; in-flight writer overlap across all 4 phases (`build_registration`, `build_backfill`, `build_catchup`, `build_publication`) |
| `C2-companion-FTS`, `C2-companion-HNSW` | RocksDB | Detached 40k-row index create with 4 writers to an independent 40k-row TxTime relation at 10 ms cadence | Same max wait and union wait gates **<= 3×** T2 TxTime p99 and 4-phase overlap |
| `H1` historical fallback | SQLite, RocksDB | 5 versions × 40k keys, 100 secondary-value predicates at v3 snapshot | Report-only |

- Arm rotation across seeds (`1234`: baseline/plain first; `1235`: candidate/txtime first; `1236`: baseline/plain first).
- Bootstrap: 10,000 hierarchical bootstrap draws (seed `20261114`), resampling seeds then paired IDs (or contiguous 100-op blocks for sequential updates / C1 lock acquisitions).
- Contamination during a Stage 2 cell permits at most **one** complete fresh repeat of that cell within the 6-hour budget; no repeat for ratio failure or inconclusive CI.

## 5. Artifacts, Review & Cleanup

1. Independent `experiment-reviewer` reviews this amendment and updated scripts (`benchmark_host.py`, `timing_harness.py`, `recording_phase3.py`, `ea3_qualify.py`, `test_ea3_qualify.py`) before launching the Fly campaign.
2. **Stage 1 -> Stage 2 checkpoint:** Stage 1 recording controls run first on the Fly VM. Before launching Stage 2 qualification, the Stage 1 completion manifest and raw control results are retrieved and reviewed independently by `experiment-reviewer` (retaining `keyed-txtime-controls-phase3.md:68,150`). Stage 2 runs on the same Fly VM (with identical frozen binaries) only if Stage 1 passes 18/18 (180/180 blocks) AND passes independent Stage 1 results review.
3. All raw `.jsonl.gz`, manifests, host logs, resource CSVs, and gate summaries are retrieved to `/home/dev/Code/mindgraph/benchmark-artifacts/ea3-fly-20261120/` before destroying the Fly machine.
4. Temporary Fly machine (and any attached scratch volume) is destroyed immediately after verified retrieval (or immediately if Stage 1 fails).
5. Independent `experiment-reviewer` reviews Stage 2 raw artifacts and proposed conclusions before un-drafting or merging `shuruheel/mnestic#91`.
