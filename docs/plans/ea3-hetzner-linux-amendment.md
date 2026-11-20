# EA3 (`mnestic#91`) — Hetzner Dedicated-Core Linux Amendment (Steal-Free Campaign)

Date: November 20, 2026.  
Supersedes: [ea3-fly-linux-amendment.md](ea3-fly-linux-amendment.md) ONLY on **host environment**, **fail-closed `hypervisor_steal` host gating**, and **Stage 1 wall-clock budget**.

## Prior Failed Attempt Disposition (No Pooling)

The prior Fly.io `performance-4x` Firecracker microVM Stage 1 attempt (`mnestic-ea3-20261120`, archived unmodified in `benchmark-artifacts/ea3-fly-20261120/stage1/`, reviewed `FAILED / NOT QUALIFIED` by independent `experiment-reviewer`, Fly app destroyed immediately after retrieval) suffered pervasive Firecracker hypervisor CPU steal (`steal_delta > 0` in 1,327/1,462 = 90.8% of active 1-second samples, `max_steal_delta = 38` ticks/s), failing p99 controls on 10/10 completed controls (including all 4 unmodified `origin/main` baseline controls) and exhausting the 7,200 s budget at 172/180 blocks because 173 × 31.4 s Linux preflights consumed 5,437.6 s. **Zero observations from the Fly run or any earlier macOS run are pooled or reused.**

## 1. Host & Admission Changes

- **Host:** Temporary dedicated-core Hetzner Cloud `CCX23` Linux server (`ubuntu-16gb-hel1-3`, `65.108.209.12`, Ubuntu 24.04.4 LTS, kernel `6.8.0-138-generic`, 4 dedicated AMD EPYC-Milan vCPUs, 15 GiB RAM, 143 GB free NVMe, 0 B swap) provisioned exclusively for this EA3 campaign and deleted immediately after artifacts are archived and retrieved.
- **Pre-campaign load-steal verification:** Verified `steal_delta == 0` across 15 consecutive 1-second samples under a full 4-thread CPU burn (`[0, 0, ..., 0]`, `total: 0`, `max: 0`).
- **Fail-closed `hypervisor_steal` gate (`scripts/benchmark_host.py`):** Any positive `/proc/stat` `steal_delta > 0` now fails closed with `reason = 'hypervisor_steal'` in both `preflight()` (preventing entry onto a steal-noisy host) and `Monitor.observe()` (invalidating any active Stage 1 block or triggering at most the single permitted Stage 2 contaminated retry).
- **Stage 1 wall-clock budget (`scripts/recording_phase3.py`):** Raised from `7,200 s` to **`10,800 s` (3 hours)** (`PRIOR_ATTEMPT_SECONDS = 0`, fresh campaign) to accommodate `180 blocks × ~31.4 s` Linux quiet preflight (~5,660 s) plus ~1,500–2,000 s execution and archiving/bootstrap with clean headroom.

## 2. Unchanged Invariants

- **Identities:** Candidate `txtime-search-indexes` (zero edits under `cozo-core/` or `cozorocks/` relative to 9/9 CI-green `5f2b4e44`), Baseline `origin/main` `94595f8e9b57941fcd3827aba78d0f8e61843dda`, RocksDB vendor `986b8b9f20893dec811d8ecdb97b6a47f20d322d` (single shared static `librocksdb.a` compiled on the host), `mnestic-benchmarks` `984d1ae9bf9e8856e0ac99f3083de77c9688173e`, seeds `1234`, `1235`, `1236`.
- **Unchanged gates & checkpoints:** All 18 Stage 1 recording controls (10 blocks × 100 pairs, 30 s quiet preflight, conservative upper95 `<= 1.05` on p50 and p99, zero retries), mandatory independent `experiment-reviewer` checkpoint between Stage 1 and Stage 2, all Stage 2 cells (`P1`, `T1`, `T2`, `C1`, `C2-*`, report-only `H1`) and ratio/recall thresholds, and independent `experiment-reviewer` Stage 2 review before un-drafting/merging `shuruheel/mnestic#91`.
- **JSON schema compatibility:** `phase: 3` is retained in `campaign-manifest.json` / `campaign-completion.json` so frozen `cozo-core/examples/keyed_txtime/inputs.rs` requires zero Rust changes; this amendment is uniquely identified by its SHA-256 recorded in `campaign-manifest.json:protocol_sha256`.
