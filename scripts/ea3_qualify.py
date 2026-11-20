#!/usr/bin/env python3
"""Stage 2 EA3 qualification generator, runner, and analyzer (ea3-fly-linux-amendment.md).

Never runs unless Stage 1 Phase-3 recording controls passed 18/18 (180/180 blocks).
Enforced both here and fail-closed in Rust (`cozo-core/examples/keyed_txtime/inputs.rs`).
"""
import argparse
import csv
import gzip
import json
import os
from pathlib import Path
import platform
import random
import shutil
import subprocess
import sys
import time

import numpy as np

import analyze_timing_diagnostics as diag
import benchmark_host as host
import recording_phase3 as phase
import timing_harness as harness

SEEDS = [1234, 1235, 1236]
BOOTSTRAP_DRAWS = 10000
BOOTSTRAP_SEED = 20261114
QUAL_BUDGET_SECONDS = 6 * 3600
MAX_CELL_SECONDS = 1800
REQUIRED_C2_PHASES = {
    "build_registration",
    "build_backfill",
    "build_catchup",
    "build_publication",
}
FTS_DDL = "::fts create chunk:fts {extractor:text,tokenizer:Simple,filters:[Lowercase]}"
HNSW_DDL = "::hnsw create chunk:vec {dim:384,m:16,dtype:F32,fields:[emb],distance:Cosine,ef_construction:64}"
COMP_FTS_DDL = "::fts create chunk_comp:fts {extractor:text,tokenizer:Simple,filters:[Lowercase]}"
COMP_HNSW_DDL = "::hnsw create chunk_comp:vec {dim:384,m:16,dtype:F32,fields:[emb],distance:Cosine,ef_construction:64}"


def save_json(path, obj):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("x") as f:
        json.dump(obj, f, separators=(",", ":"), sort_keys=True)


def save_pretty(path, obj):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("x") as f:
        json.dump(obj, f, indent=2, sort_keys=True)
        f.write("\n")


def fts_query_str(text):
    seen, terms = set(), []
    for tok in text.split():
        low = tok.lower()
        if low not in seen:
            seen.add(low)
            terms.append(low)
    return " OR ".join(terms)


def exact_topk_cosine(corpus_norm, query_norm, k=10):
    """Exact tie-aware top-k cosine neighbors in float32 using L2-normalized arrays."""
    topk = []
    for start in range(0, len(query_norm), 100):
        sims = query_norm[start:start + 100] @ corpus_norm.T
        for row in sims:
            idx = np.argpartition(-row, k)[:k]
            kth = float(np.min(row[idx]))
            tied = np.where(row >= kth - 1e-6)[0].tolist()
            topk.append(tied)
    return topk


def generate_seed_inputs(bench_repo, seed, out_dir):
    sys.path.insert(0, str(Path(bench_repo).resolve()))
    from hybrid_recall.workload.generate import generate
    from hybrid_recall.workload.schema import chunk_kind

    out_dir = Path(out_dir).resolve()
    out_dir.mkdir(parents=True, exist_ok=True)
    w = generate(
        "small",
        entities=10000,
        edges=50000,
        chunks=40000,
        dim=384,
        queries=1000,
        graph_hops=2,
        k=10,
        clusters=64,
        seed=seed,
        pool_n=100,
        rrf_k=60.0,
        real_embeddings=False,
        topic_size=200,
        chunk_words=10,
        query_words=4,
        summary_words=6,
        incremental=200,
    )
    order = sorted(range(len(w.chunks)), key=lambda i: w.chunks[i].id)
    input_hashes = {}

    def write_input(name, payload):
        p = out_dir / name
        save_json(p, payload)
        h = phase.file_hash(p)
        input_hashes[str(p)] = h
        return {"path": str(p), "sha256": h}

    batch_refs = []
    for b_idx, start in enumerate(range(0, len(order), 2000)):
        rows = []
        for idx in order[start:start + 2000]:
            ch = w.chunks[idx]
            rows.append([ch.id, ch.text, w.chunk_emb[ch.vec_idx].tolist(), chunk_kind(ch.id)])
        batch_refs.append(write_input(f"seed{seed}_load_{b_idx:02d}.json", {"r": rows}))

    # Save compact L2-normalized float32 matrices for exact HNSW distance & recall@10 verification.
    c_emb = np.asarray(w.chunk_emb, dtype=np.float32)
    q_emb = np.asarray(w.query_emb, dtype=np.float32)
    c_norm = c_emb / np.maximum(np.linalg.norm(c_emb, axis=1, keepdims=True), np.float32(1e-12))
    q_norm = q_emb / np.maximum(np.linalg.norm(q_emb, axis=1, keepdims=True), np.float32(1e-12))
    np.save(out_dir / f"seed{seed}_corpus_norm.npy", c_norm)
    np.save(out_dir / f"seed{seed}_query_norm.npy", q_norm)

    id_by_pos = [c.id for c in w.chunks]
    cid_to_pos = {c.id: i for i, c in enumerate(w.chunks)}
    cid_to_text = {c.id: c.text for c in w.chunks}
    topk_indices = exact_topk_cosine(c_norm, q_norm, k=10)
    exact_topk_ids = [[id_by_pos[j] for j in idxs] for idxs in topk_indices]

    # Point reads: 100 warmup + 500 present + 500 absent.
    rng = random.Random(seed)
    all_cids = [w.chunks[i].id for i in order]
    shuffled_cids = list(all_cids)
    rng.shuffle(shuffled_cids)
    pr_warmup = shuffled_cids[:100]
    pr_present = shuffled_cids[100:600]
    pr_absent = [f"missing_{i:05d}" for i in range(500)]
    pr_measured = pr_present + pr_absent
    rng.shuffle(pr_measured)

    # Search warmup: first 100 inc_queries; measured: 1,000 queries.
    search_warmup = [
        {"id": q.id, "fts": fts_query_str(q.text), "vec": w.inc_query_emb[q.vec_idx].tolist()}
        for q in w.inc_queries[:100]
    ]
    search_measured = [
        {"id": q.id, "fts": fts_query_str(q.text), "vec": w.query_emb[q.vec_idx].tolist()}
        for q in w.queries
    ]

    # Updates (T2 / P1): 200 warmup + 2,000 measured on disjoint existing CIDs.
    inc_payloads = [(c.text, w.inc_chunk_emb[c.vec_idx].tolist()) for c in w.inc_chunks]
    upd_cids = shuffled_cids[600:2800]
    upd_warmup_meta = [(upd_cids[i], i % len(inc_payloads), chunk_kind(upd_cids[i])) for i in range(200)]
    upd_meas_meta = [(upd_cids[200 + i], i % len(inc_payloads), chunk_kind(upd_cids[200 + i])) for i in range(2000)]

    # Pick an updated CID whose old and new text have at least one distinct token for post-update FTS verification.
    last_cid, last_vi, last_kind = upd_meas_meta[-1]
    old_toks = set(cid_to_text[last_cid].lower().split())
    new_toks = set(inc_payloads[last_vi][0].lower().split())
    fresh_tok = sorted(new_toks - old_toks)[0] if (new_toks - old_toks) else sorted(new_toks)[0]
    stale_tok = sorted(old_toks - new_toks)[0] if (old_toks - new_toks) else None

    # C1: 4 disjoint writers × 1,000 ops.
    c1_base = shuffled_cids[2800:6800]
    c1_workers_meta = [
        [(c1_base[wi * 1000 + i], (wi * 50 + i) % len(inc_payloads), chunk_kind(c1_base[wi * 1000 + i]))
         for i in range(1000)]
        for wi in range(4)
    ]

    # C2: 4 disjoint writers × 4,000 target CIDs.
    c2_base = shuffled_cids[6800:22800]
    c2_workers = [c2_base[wi * 4000:(wi + 1) * 4000] for wi in range(4)]

    # Shared external vector files (compact plans, zero vector duplication).
    inc_vec_refs = [
        write_input(f"seed{seed}_inc_vec_{i:03d}.json", {"emb": inc_payloads[i][1]})
        for i in range(len(inc_payloads))
    ]
    q_warmup_vec_refs = [
        write_input(f"seed{seed}_qw_vec_{i:03d}.json", {"q": search_warmup[i]["vec"]})
        for i in range(len(search_warmup))
    ]
    q_meas_vec_refs = [
        write_input(f"seed{seed}_qm_vec_{i:04d}.json", {"q": search_measured[i]["vec"]})
        for i in range(len(search_measured))
    ]

    # H1 bucket table (tiny ~1 MB instead of 3.7 GB of repeated vectors): maps each cid -> bucket 0..99.
    h1_buckets = [[w.chunks[idx].id, f"{pos % 100}"] for pos, idx in enumerate(order)]
    h1_bucket_ref = write_input(f"seed{seed}_h1_buckets.json", {"b": h1_buckets})
    all_cids_ref = write_input(f"seed{seed}_all_cids.json", {"keys": [[cid] for cid in all_cids]})
    h1_expected_by_bucket = {str(k): [] for k in range(100)}
    for cid, b in h1_buckets:
        h1_expected_by_bucket[b].append([cid])
    for b in h1_expected_by_bucket:
        h1_expected_by_bucket[b].sort()

    meta = dict(
        seed=seed,
        batch_refs=batch_refs,
        inc_vec_refs=inc_vec_refs,
        inc_texts=[p[0] for p in inc_payloads],
        q_warmup_fts=[q["fts"] for q in search_warmup],
        q_warmup_vec_refs=q_warmup_vec_refs,
        q_meas_ids=[q["id"] for q in search_measured],
        q_meas_fts=[q["fts"] for q in search_measured],
        q_meas_vec_refs=q_meas_vec_refs,
        exact_topk_ids=exact_topk_ids,
        cid_to_pos=cid_to_pos,
        pr_warmup=pr_warmup,
        pr_measured=pr_measured,
        pr_present_set=pr_present,
        upd_warmup_meta=upd_warmup_meta,
        upd_meas_meta=upd_meas_meta,
        upd_verify=dict(cid=last_cid, vi=last_vi, kind=last_kind, fresh_tok=fresh_tok, stale_tok=stale_tok),
        c1_workers_meta=c1_workers_meta,
        c2_workers_cids=c2_workers,
        c2_workers_kinds=[[chunk_kind(cid) for cid in cids] for cids in c2_workers],
        h1_bucket_ref=h1_bucket_ref,
        all_cids_ref=all_cids_ref,
        h1_expected_by_bucket=h1_expected_by_bucket,
        input_hashes=input_hashes,
    )
    save_pretty(out_dir / f"seed{seed}_meta.json", meta)
    return meta


def base_setup(meta, temporal, with_fts=True, with_hnsw=True, companion=False):
    schema = ":create chunk {cid:String" + (",tt:TxTime" if temporal else "") + " => text:String,emb:<F32;384>,kind:String}"
    ops = [harness.op("schema", schema)]
    put = "?[cid,text,emb,kind] <- $r :put chunk {cid=>text,emb,kind}"
    for i, ref in enumerate(meta["batch_refs"]):
        ops.append(harness.op(f"load-{i:02d}", put, params_file=ref))
    if with_fts:
        ops.append(harness.op("fts-create", FTS_DDL))
    if with_hnsw:
        ops.append(harness.op("hnsw-create", HNSW_DDL))
    if companion:
        c_schema = ":create chunk_comp {cid:String,tt:TxTime => text:String,emb:<F32;384>,kind:String}"
        ops.append(harness.op("comp-schema", c_schema))
        c_put = "?[cid,text,emb,kind] <- $r :put chunk_comp {cid=>text,emb,kind}"
        for i, ref in enumerate(meta["batch_refs"]):
            ops.append(harness.op(f"comp-load-{i:02d}", c_put, params_file=ref))
        ops.append(harness.op("comp-fts-create", COMP_FTS_DDL))
        ops.append(harness.op("comp-hnsw-create", COMP_HNSW_DDL))
    return ops


def put_row_op(op_id, rel, cid, text, kind, vec_ref, temporal, warmup=False):
    phases = ["commit"] + (["tt_commit_section"] if temporal else [])
    script = f"?[cid,text,emb,kind] <- [[$cid,$text,vec($emb),$kind]] :put {rel} {{cid=>text,emb,kind}}"
    return harness.op(
        op_id,
        script,
        params={"cid": cid, "text": text, "kind": kind},
        params_file=vec_ref,
        warmup=warmup,
        phases=phases,
    )


def update_verify_ops(meta, temporal):
    uv = meta["upd_verify"]
    last_cid, last_vi, last_kind = uv["cid"], uv["vi"], uv["kind"]
    inc_text = meta["inc_texts"][last_vi]
    vec_ref = meta["inc_vec_refs"][last_vi]
    ops = [
        harness.op("count", "?[count(cid)] := *chunk{cid}", rows=[[40000]], immutable=True),
        harness.op(
            "final-row",
            "?[cid,text,kind,same_vec] := *chunk{cid,text,emb,kind}, cid=$id, same_vec = (emb == vec($emb))",
            params={"id": last_cid, "text": inc_text, "kind": last_kind},
            params_file=vec_ref,
            rows=[[last_cid, inc_text, last_kind, True]],
            immutable=True,
        ),
        harness.op(
            "final-fts-fresh",
            "?[cid] := ~chunk:fts{cid | query:$q,k:40000}, cid == $id",
            params={"q": uv["fresh_tok"], "id": last_cid},
            rows=[[last_cid]],
            immutable=True,
        ),
        harness.op(
            "final-vec-hit",
            "?[cid] := ~chunk:vec{cid | query:vec($emb),k:50,ef:100}, cid == $id",
            params={"id": last_cid},
            params_file=vec_ref,
            rows=[[last_cid]],
            immutable=True,
        ),
    ]
    if uv["stale_tok"]:
        ops.append(
            harness.op(
                "final-fts-stale",
                "?[cid] := ~chunk:fts{cid | query:$q,k:40000}, cid == $id",
                params={"q": uv["stale_tok"], "id": last_cid},
                rows=[],
                immutable=True,
            )
        )
    if temporal:
        ops.append(
            harness.op(
                "final-hist",
                "::history chunk [[$id]]",
                params={"id": last_cid},
                expected_row_count=2,
                immutable=True,
            )
        )
    return ops


def build_all_plans(metas, plans_dir):
    """Build plans in frozen cell-major order (P1 -> T1 -> T2 -> C1 -> C2-* -> H1), with H1 last."""
    plans_dir = Path(plans_dir).resolve()
    plans_dir.mkdir(parents=True, exist_ok=True)
    grouped = {
        "P1": [],
        "T1": [],
        "T2": [],
        "C1": [],
        "C2_FTS": [],
        "C2_HNSW": [],
        "C2_companion_FTS": [],
        "C2_companion_HNSW": [],
        "H1": [],
    }

    def add(group, name, arm, backend, seed, plan_obj, report_only=False):
        p = plans_dir / f"{name}.json"
        save_json(p, plan_obj)
        grouped[group].append(
            dict(
                group=group,
                name=name,
                arm=arm,
                backend=backend,
                seed=seed,
                report_only=report_only,
                path=str(p),
                sha256=phase.file_hash(p),
            )
        )

    for backend in ["sqlite", "rocksdb"]:
        for seed in SEEDS:
            meta = metas[seed]
            present_set = set(meta["pr_present_set"])
            inc_texts = meta["inc_texts"]
            inc_refs = meta["inc_vec_refs"]
            p1_arms = [("baseline", False), ("candidate", False)] if seed != 1235 else [("candidate", False), ("baseline", False)]
            tt_arms = [("plain", False), ("txtime", True)] if seed != 1235 else [("txtime", True), ("plain", False)]

            pr_ops = [
                harness.op(f"pr-w-{i:03d}", "?[cid] := *chunk{cid}, cid=$id", params={"id": cid}, rows=[[cid]], immutable=True, warmup=True)
                for i, cid in enumerate(meta["pr_warmup"])
            ] + [
                harness.op(f"pr-{i:04d}", "?[cid] := *chunk{cid}, cid=$id", params={"id": cid},
                           rows=[[cid]] if cid in present_set else [], immutable=True, phases=["query_execution"])
                for i, cid in enumerate(meta["pr_measured"])
            ]
            fts_ops = [
                harness.op(f"fts-w-{i:03d}", "?[cid,score] := ~chunk:fts{cid | query:$q,k:100,bind_score:score} :order -score, cid",
                           params={"q": q}, immutable=True, warmup=True)
                for i, q in enumerate(meta["q_warmup_fts"])
            ] + [
                harness.op(f"fts-{i:04d}", "?[cid,score] := ~chunk:fts{cid | query:$q,k:100,bind_score:score} :order -score, cid",
                           params={"q": q}, immutable=True, save_rows=True, phases=["query_execution"])
                for i, q in enumerate(meta["q_meas_fts"])
            ]
            vec_ops = [
                harness.op(f"vec-w-{i:03d}", "?[cid,dist] := ~chunk:vec{cid | query:vec($q),k:100,ef:100,bind_distance:dist} :order dist, cid",
                           params_file=ref, immutable=True, warmup=True)
                for i, ref in enumerate(meta["q_warmup_vec_refs"])
            ] + [
                harness.op(f"vec-{i:04d}", "?[cid,dist] := ~chunk:vec{cid | query:vec($q),k:100,ef:100,bind_distance:dist} :order dist, cid",
                           params_file=ref, immutable=True, save_rows=True, phases=["query_execution"])
                for i, ref in enumerate(meta["q_meas_vec_refs"])
            ]

            # P1 plain regression (fresh DB per arm/cell; separate from T1/T2 plain arms).
            for arm, _ in p1_arms:
                upd_ops = [
                    put_row_op(f"upd-w-{i:03d}", "chunk", cid, inc_texts[vi], kind, inc_refs[vi], False, warmup=True)
                    for i, (cid, vi, kind) in enumerate(meta["upd_warmup_meta"])
                ] + [
                    put_row_op(f"upd-{i:04d}", "chunk", cid, inc_texts[vi], kind, inc_refs[vi], False, warmup=False)
                    for i, (cid, vi, kind) in enumerate(meta["upd_meas_meta"])
                ]
                add(
                    "P1", f"P1_noindex_{backend}_{arm}_s{seed}", arm, backend, seed,
                    dict(
                        version=1, purpose="qualification", seed=seed, backend=backend,
                        setup=base_setup(meta, temporal=False, with_fts=False, with_hnsw=False),
                        steps=[
                            dict(mode="serial", cell="p1_noindex_point", operations=pr_ops),
                            dict(mode="serial", cell="p1_noindex_update", operations=upd_ops),
                        ],
                        verify=[harness.op("count", "?[count(cid)] := *chunk{cid}", rows=[[40000]], immutable=True)],
                    ),
                )
                add(
                    "P1", f"P1_search_{backend}_{arm}_s{seed}", arm, backend, seed,
                    dict(
                        version=1, purpose="qualification", seed=seed, backend=backend,
                        setup=base_setup(meta, temporal=False, with_fts=True, with_hnsw=True),
                        steps=[
                            dict(mode="serial", cell="point_read", operations=pr_ops),
                            dict(mode="serial", cell="fts_search", operations=fts_ops),
                            dict(mode="serial", cell="vec_search", operations=vec_ops),
                        ],
                        verify=[harness.op("count", "?[count(cid)] := *chunk{cid}", rows=[[40000]], immutable=True)],
                    ),
                )
                add(
                    "P1", f"P1_update_{backend}_{arm}_s{seed}", arm, backend, seed,
                    dict(
                        version=1, purpose="qualification", seed=seed, backend=backend,
                        setup=base_setup(meta, temporal=False, with_fts=True, with_hnsw=True),
                        steps=[dict(mode="serial", cell="update", operations=upd_ops)],
                        verify=update_verify_ops(meta, temporal=False),
                    ),
                )

            # T1 current search & T2 single writer (rotated candidate plain vs txtime).
            for label, temporal in tt_arms:
                add(
                    "T1", f"T1_{backend}_{label}_s{seed}", "candidate", backend, seed,
                    dict(
                        version=1, purpose="qualification", seed=seed, backend=backend,
                        setup=base_setup(meta, temporal=temporal, with_fts=True, with_hnsw=True),
                        steps=[
                            dict(mode="serial", cell="fts_search", operations=fts_ops),
                            dict(mode="serial", cell="vec_search", operations=vec_ops),
                        ],
                        verify=[harness.op("count", "?[count(cid)] := *chunk{cid}", rows=[[40000]], immutable=True)],
                    ),
                )
                upd_ops_t2 = [
                    put_row_op(f"upd-w-{i:03d}", "chunk", cid, inc_texts[vi], kind, inc_refs[vi], temporal, warmup=True)
                    for i, (cid, vi, kind) in enumerate(meta["upd_warmup_meta"])
                ] + [
                    put_row_op(f"upd-{i:04d}", "chunk", cid, inc_texts[vi], kind, inc_refs[vi], temporal, warmup=False)
                    for i, (cid, vi, kind) in enumerate(meta["upd_meas_meta"])
                ]
                add(
                    "T2", f"T2_{backend}_{label}_s{seed}", "candidate", backend, seed,
                    dict(
                        version=1, purpose="qualification", seed=seed, backend=backend,
                        setup=base_setup(meta, temporal=temporal, with_fts=True, with_hnsw=True),
                        steps=[dict(mode="serial", cell="update", operations=upd_ops_t2)],
                        verify=update_verify_ops(meta, temporal=temporal),
                    ),
                )

            # H1 historical fallback (report-only, placed last in global order).
            h1_setup = base_setup(meta, temporal=True, with_fts=False, with_hnsw=False)
            h1_setup.append(harness.op("h1-bucket-schema", ":create h1_bucket {cid:String => b:String}"))
            h1_setup.append(harness.op("h1-bucket-load", "?[cid,b] <- $b :put h1_bucket {cid=>b}", params_file=meta["h1_bucket_ref"]))
            for ver in [2, 3]:
                h1_setup.append(
                    harness.op(
                        f"v{ver}-put",
                        "?[cid,text,emb,kind] := *chunk{cid,text:t0,emb}, *h1_bucket{cid,b}, text = concat(t0, $suf), kind = concat($kp, b) :put chunk {cid=>text,emb,kind}",
                        params={"suf": f" v{ver}", "kp": f"kind_v{ver}_"},
                        timeout_seconds=120,
                    )
                )
            h1_setup.append(
                harness.op(
                    "capture-v3-tt",
                    "?[max(t)] := *chunk{tt}, t = to_int(tt)",
                    capture={"name": "v3_tt", "column": "max(t)", "row": 0},
                    immutable=True,
                )
            )
            for ver in [4, 5]:
                h1_setup.append(
                    harness.op(
                        f"v{ver}-put",
                        "?[cid,text,emb,kind] := *chunk{cid,text:t0,emb}, *h1_bucket{cid,b}, text = concat(t0, $suf), kind = concat($kp, b) :put chunk {cid=>text,emb,kind}",
                        params={"suf": f" v{ver}", "kp": f"kind_v{ver}_"},
                        timeout_seconds=120,
                    )
                )
            h1_ops = [
                harness.op(
                    f"h1-{k:03d}",
                    "?[cid] := *chunk{cid, kind @ (tt: $v3_tt)}, kind == $k :order cid",
                    params={"k": f"kind_v3_{k}"},
                    params_from={"v3_tt": "v3_tt"},
                    rows=meta["h1_expected_by_bucket"][str(k)],
                    immutable=True,
                    phases=["query_execution"],
                )
                for k in range(100)
            ]
            add(
                "H1", f"H1_{backend}_txtime_s{seed}", "candidate", backend, seed,
                dict(
                    version=1, purpose="qualification", seed=seed, backend=backend,
                    setup=h1_setup,
                    steps=[dict(mode="serial", cell="h1_history", operations=h1_ops)],
                    verify=[
                        harness.op("logical-count", "?[count(cid)] := *chunk{cid}", rows=[[40000]], immutable=True),
                        harness.op("physical-count", "::history chunk $keys", params_file=meta["all_cids_ref"], project_row_count=True, rows=[[200000]], save_rows=True, immutable=True),
                    ],
                ),
                report_only=True,
            )

    # RocksDB-only concurrency cells: C1, C2-FTS, C2-HNSW, C2-companion-FTS, C2-companion-HNSW.
    for seed in SEEDS:
        meta = metas[seed]
        inc_texts = meta["inc_texts"]
        inc_refs = meta["inc_vec_refs"]

        c1_workers = [
            [put_row_op(f"w{wi}-op-{i:04d}", "chunk", cid, inc_texts[vi], kind, inc_refs[vi], True)
             for i, (cid, vi, kind) in enumerate(w_meta)]
            for wi, w_meta in enumerate(meta["c1_workers_meta"])
        ]
        add(
            "C1", f"C1_rocksdb_txtime_s{seed}", "candidate", "rocksdb", seed,
            dict(
                version=1, purpose="qualification", seed=seed, backend="rocksdb",
                setup=base_setup(meta, temporal=True, with_fts=True, with_hnsw=True),
                steps=[dict(mode="concurrent", cell="c1_writers", workers=c1_workers)],
                verify=[harness.op("count", "?[count(cid)] := *chunk{cid}", rows=[[40000]], immutable=True)],
            ),
        )

        def c2_workers_for(rel):
            workers = []
            for wi in range(4):
                cids = meta["c2_workers_cids"][wi]
                kinds = meta["c2_workers_kinds"][wi]
                w_ops = []
                for cycle in range(2):
                    for idx, (cid, kind) in enumerate(zip(cids, kinds)):
                        vi = (wi * 50 + idx + cycle) % len(inc_refs)
                        txt = f"{inc_texts[vi]} c{cycle}w{wi}"
                        w_ops.append(put_row_op(f"w{wi}-c{cycle}-{idx:04d}", rel, cid, txt, kind, inc_refs[vi], True))
                workers.append(w_ops)
            return workers

        c2_configs = [
            ("C2_FTS", "chunk", False, True, FTS_DDL, False, "fts"),
            ("C2_HNSW", "chunk", True, False, HNSW_DDL, False, "vec"),
            ("C2_companion_FTS", "chunk_comp", False, True, FTS_DDL, True, "fts"),
            ("C2_companion_HNSW", "chunk_comp", True, False, HNSW_DDL, True, "vec"),
        ]
        for cname, target_rel, with_fts, with_hnsw, ddl, companion, built_kind in c2_configs:
            c2_verify = [harness.op("count", "?[count(cid)] := *chunk{cid}", rows=[[40000]], immutable=True)]
            if companion:
                c2_verify.append(harness.op("comp-count", "?[count(cid)] := *chunk_comp{cid}", rows=[[40000]], immutable=True))
            if built_kind == "fts":
                c2_verify.append(
                    harness.op(
                        "built-fts-probe",
                        "?[cid] := ~chunk:fts{cid | query:$q,k:10}",
                        params={"q": meta["q_meas_fts"][0]},
                        immutable=True,
                    )
                )
            else:
                c2_verify.append(
                    harness.op(
                        "built-vec-probe",
                        "?[cid] := ~chunk:vec{cid | query:vec($q),k:10,ef:100}",
                        params_file=meta["q_meas_vec_refs"][0],
                        expected_row_count=10,
                        immutable=True,
                    )
                )
            add(
                cname, f"{cname}_rocksdb_s{seed}", "candidate", "rocksdb", seed,
                dict(
                    version=1, purpose="qualification", seed=seed, backend="rocksdb",
                    setup=base_setup(meta, temporal=True, with_fts=with_fts, with_hnsw=with_hnsw, companion=companion),
                    steps=[dict(
                        mode="build",
                        cell=cname.lower(),
                        ddl=harness.op(
                            "ddl",
                            ddl,
                            timeout_seconds=900,
                            phases=["build_registration", "build_backfill", "build_catchup", "build_publication", "tt_ddl_section"],
                        ),
                        workers=c2_workers_for(target_rel),
                        interval_ms=10,
                        max_attempts_per_writer=100000,
                        max_seconds=900,
                    )],
                    verify=c2_verify,
                ),
            )

    ordered = []
    for g in ["P1", "T1", "T2", "C1", "C2_FTS", "C2_HNSW", "C2_companion_FTS", "C2_companion_HNSW", "H1"]:
        ordered.extend(grouped[g])
    return ordered


def load_cell_rows(run_dir):
    rows = []
    for p in sorted(Path(run_dir).iterdir()):
        if not (p.name.endswith(".jsonl") or p.name.endswith(".jsonl.gz")):
            continue
        if p.name.startswith("snapshots.jsonl"):
            continue
        opener = gzip.open if p.name.endswith(".gz") else open
        with opener(p, "rt") as f:
            for line in f:
                r = json.loads(line)
                if "timing" in r:
                    rows.append(r)
    return rows


def validate_cell_run_provenance(run_dir, allow_incomplete=False):
    run_dir = Path(run_dir)
    required = [
        run_dir / "plan.json",
        run_dir / "manifest.json",
        run_dir / "summary.json",
        run_dir.with_suffix(".supervisor.json"),
        run_dir.with_suffix(".resources.csv"),
        run_dir.with_suffix(".completion.json"),
    ]
    missing = [str(p) for p in required if not p.exists()]
    if missing:
        if allow_incomplete:
            return False, f"missing files: {missing}"
        raise ValueError(f"{run_dir}: missing provenance files {missing}")
    summary = json.loads((run_dir / "summary.json").read_text())
    completion = json.loads(run_dir.with_suffix(".completion.json").read_text())
    supervisor = json.loads(run_dir.with_suffix(".supervisor.json").read_text())
    manifest = json.loads((run_dir / "manifest.json").read_text())
    with run_dir.with_suffix(".resources.csv").open() as rf:
        res_rows = list(csv.DictReader(rf))
    resources_ok = bool(res_rows) and all(
        float(r["rss_kib"]) <= 4096 * 1024 and float(r["free_mib"]) >= 2048 for r in res_rows
    )
    hashes_ok = (
        manifest.get("plan_sha256") == supervisor.get("plan_sha256")
        and manifest.get("executable_sha256") == supervisor.get("binary_sha256")
    )
    ok = (
        summary.get("execution_complete") is True
        and completion.get("exit_code") == 0
        and not completion.get("stop_reason")
        and resources_ok
        and hashes_ok
    )
    if not ok and not allow_incomplete:
        raise ValueError(f"{run_dir}: invalid cell completion/provenance (summary={summary}, completion={completion}, resources_ok={resources_ok}, hashes_ok={hashes_ok})")
    return ok, None if ok else "incomplete"


def bootstrap_ratio_upper95(per_seed_num, per_seed_den, q, block_size=None, draws=BOOTSTRAP_DRAWS):
    """10,000 hierarchical bootstrap replicates (seed 20261114): resample seeds then paired ops/blocks."""
    rng = random.Random(BOOTSTRAP_SEED)
    seeds = sorted(per_seed_num.keys())
    for s in seeds:
        if len(per_seed_num[s]) != len(per_seed_den[s]) or not per_seed_num[s]:
            raise ValueError(f"paired length mismatch for seed {s}: {len(per_seed_num[s])} vs {len(per_seed_den[s])}")
    obs_num = sum(diag.quantile(per_seed_num[s], q) for s in seeds) / len(seeds)
    obs_den = sum(diag.quantile(per_seed_den[s], q) for s in seeds) / len(seeds)
    obs_ratio = obs_num / obs_den
    per_seed_ratios = {
        str(s): diag.quantile(per_seed_num[s], q) / diag.quantile(per_seed_den[s], q)
        for s in seeds
    }
    boot = []
    for _ in range(draws):
        chosen = [seeds[rng.randrange(len(seeds))] for _ in range(len(seeds))]
        nums, dens = [], []
        for s in chosen:
            a, b = per_seed_num[s], per_seed_den[s]
            n = len(a)
            if block_size is None:
                idxs = [rng.randrange(n) for _ in range(n)]
            else:
                nb = max(1, n // block_size)
                starts = [rng.randrange(nb) * block_size for _ in range(nb)]
                idxs = [i for st in starts for i in range(st, min(st + block_size, n))]
            nums.append(diag.quantile([a[i] for i in idxs], q))
            dens.append(diag.quantile([b[i] for i in idxs], q))
        boot.append((sum(nums) / len(nums)) / (sum(dens) / len(dens)))
    return dict(
        observed_mean_seed_ratio=obs_ratio,
        upper95=diag.quantile(boot, 0.95),
        per_seed_ratios=per_seed_ratios,
        per_seed_num_ns={str(s): diag.quantile(per_seed_num[s], q) for s in seeds},
        per_seed_den_ns={str(s): diag.quantile(per_seed_den[s], q) for s in seeds},
    )


def analyze_c1_bootstrap(c1_events_by_seed, t2_den_by_seed, block_size=100, draws=BOOTSTRAP_DRAWS):
    """Coupled 100-acquisition block bootstrap retaining writer identity for pooled & max-per-writer p99."""
    rng = random.Random(BOOTSTRAP_SEED)
    seeds = sorted(c1_events_by_seed.keys())

    def seed_metrics(events, t2_vals):
        all_waits = [w for _, w in events]
        by_w = {wid: [] for wid in range(1, 5)}
        for wid, w in events:
            by_w[wid].append(w)
        pooled = diag.quantile(all_waits, 0.99)
        w_p99 = {str(wid): diag.quantile(vals, 0.99) for wid, vals in by_w.items() if vals}
        max_w = max(w_p99.values())
        t2_p99 = diag.quantile(t2_vals, 0.99)
        return pooled, max_w, t2_p99, w_p99

    obs_pooled_n, obs_maxw_n, obs_d = [], [], []
    per_seed_pooled, per_seed_maxw, per_writer_detail = {}, {}, {}
    for s in seeds:
        p, mw, d, wp = seed_metrics(c1_events_by_seed[s], t2_den_by_seed[s])
        obs_pooled_n.append(p)
        obs_maxw_n.append(mw)
        obs_d.append(d)
        per_seed_pooled[str(s)] = p / d
        per_seed_maxw[str(s)] = mw / d
        per_writer_detail[str(s)] = dict(t2_p99_ns=d, writer_p99_ns=wp, max_writer_ratio=mw / d)

    boot_pooled, boot_maxw = [], []
    for _ in range(draws):
        chosen = [seeds[rng.randrange(len(seeds))] for _ in range(len(seeds))]
        p_nums, mw_nums, dens = [], [], []
        for s in chosen:
            ev = c1_events_by_seed[s]
            t2 = t2_den_by_seed[s]
            nb_e = max(1, len(ev) // block_size)
            nb_t = max(1, len(t2) // block_size)
            ie = [i for st in (rng.randrange(nb_e) * block_size for _ in range(nb_e)) for i in range(st, min(st + block_size, len(ev)))]
            it = [i for st in (rng.randrange(nb_t) * block_size for _ in range(nb_t)) for i in range(st, min(st + block_size, len(t2)))]
            sampled_ev = [ev[i] for i in ie]
            all_w = [w for _, w in sampled_ev]
            by_w = {wid: [] for wid in range(1, 5)}
            for wid, w in sampled_ev:
                by_w[wid].append(w)
            p_nums.append(diag.quantile(all_w, 0.99))
            mw_nums.append(max(diag.quantile(vals, 0.99) for vals in by_w.values() if vals))
            dens.append(diag.quantile([t2[i] for i in it], 0.99))
        den_mean = sum(dens) / len(dens)
        boot_pooled.append((sum(p_nums) / len(p_nums)) / den_mean)
        boot_maxw.append((sum(mw_nums) / len(mw_nums)) / den_mean)

    pooled_upper95 = diag.quantile(boot_pooled, 0.95)
    maxw_upper95 = diag.quantile(boot_maxw, 0.95)
    pooled_ok = pooled_upper95 <= 3.0 and all(v <= 3.0 for v in per_seed_pooled.values())
    maxw_ok = maxw_upper95 <= 3.0 and all(v <= 3.0 for v in per_seed_maxw.values())
    return dict(
        threshold=3.0,
        observed_mean_seed_ratio=(sum(obs_pooled_n) / len(obs_pooled_n)) / (sum(obs_d) / len(obs_d)),
        upper95=pooled_upper95,
        per_seed_ratios=per_seed_pooled,
        max_writer_observed_mean_seed_ratio=(sum(obs_maxw_n) / len(obs_maxw_n)) / (sum(obs_d) / len(obs_d)),
        max_writer_upper95=maxw_upper95,
        max_writer_per_seed_ratios=per_seed_maxw,
        per_writer_detail=per_writer_detail,
        pooled_passed=pooled_ok,
        max_writer_passed=maxw_ok,
        passed=pooled_ok and maxw_ok,
    )


def analyze_c2_cell(rows, cell_name, t2_p99):
    """Validate C2 DDL & writers including 5 emitted spans (`build_reservation` + 4 required phases)."""
    cell_rows = [r for r in rows if r["cell"] == cell_name]
    bad = [r for r in cell_rows if r["status"] != "ok" or not r.get("oracle_ok") or not r.get("trace_valid") or not r.get("timing")]
    ddl_rows = [r for r in cell_rows if r["writer"] == 0]
    if len(ddl_rows) != 1 or bad:
        return dict(passed=False, reason=f"invalid/failed rows: bad={len(bad)}, ddl={len(ddl_rows)}")
    ddl_end = ddl_rows[0]["timing"]["end_ns"]
    by_w = {w: [] for w in range(1, 5)}
    for r in cell_rows:
        if r["writer"] in by_w:
            by_w[r["writer"]].append(r)
    writers_ok = all(
        len(wrows) >= 2 and max(wrows, key=lambda r: r["operation_id"])["timing"]["start_ns"] >= ddl_end
        for wrows in by_w.values()
    )
    ba = diag.build_analysis(rows, cell_name)
    max_tt = max((w["tt_wait_union_ns"] for w in ba["writer_waits"]), default=0)
    max_union = ba["max_writer_admission_and_tt_wait_union_ns"]
    phase_map = {p["phase"]: p for p in ba["phases"]}
    required_ok = all(
        name in phase_map and phase_map[name]["completed"] and phase_map[name]["scripts_inflight"] > 0
        for name in REQUIRED_C2_PHASES
    )
    ok = (
        ba["ddl_status"] == "ok"
        and ba["writer_errors"] == 0
        and writers_ok
        and required_ok
        and (max_tt <= 3.0 * t2_p99)
        and (max_union <= 3.0 * t2_p99)
    )
    return dict(
        ddl_ns=ba["ddl_ns"],
        t2_p99_ns=t2_p99,
        max_tt_wait_ns=max_tt,
        max_tt_ratio=max_tt / t2_p99,
        max_union_wait_ns=max_union,
        max_union_ratio=max_union / t2_p99,
        writer_operations=ba["writer_operations"],
        writer_errors=ba["writer_errors"],
        writers_final_commit_after_ddl=writers_ok,
        required_phases_covered=required_ok,
        phases=ba["phases"],
        passed=ok,
    )


def resolve_canonical_run_dir(runs_dir, name, canonical_attempts=None):
    runs_dir = Path(runs_dir)
    if canonical_attempts and name in canonical_attempts:
        return runs_dir / name / canonical_attempts[name] / "run"
    # Fallback: prefer attempt-2 if attempt-2/run exists, else attempt-1/run, else legacy flat dir.
    for att in ("attempt-2", "attempt-1"):
        cand = runs_dir / name / att / "run"
        if cand.exists():
            return cand
    return runs_dir / name


def analyze_qualification(output_dir, inputs_dir, canonical_attempts=None):
    output_dir = Path(output_dir).resolve()
    inputs_dir = Path(inputs_dir).resolve()
    metas = {s: json.loads((inputs_dir / f"seed{s}_meta.json").read_text()) for s in SEEDS}
    c_norms = {s: np.load(inputs_dir / f"seed{s}_corpus_norm.npy") for s in SEEDS}
    q_norms = {s: np.load(inputs_dir / f"seed{s}_query_norm.npy") for s in SEEDS}
    runs_dir = output_dir / "runs"

    def durations(plan_name, cell, expected_count):
        run_dir = resolve_canonical_run_dir(runs_dir, plan_name, canonical_attempts)
        validate_cell_run_provenance(run_dir)
        rows = load_cell_rows(run_dir)
        # Validate setup & verify rows too.
        sv_bad = [
            r for r in rows
            if r["cell"] in ("setup", "verify") and (r["status"] != "ok" or not r.get("oracle_ok") or not r.get("trace_valid"))
        ]
        if sv_bad:
            raise ValueError(f"{plan_name}: setup/verify had {len(sv_bad)} failed rows")
        if len({r["operation_id"] for r in rows}) != len(rows):
            raise ValueError(f"{plan_name}: duplicate operation_id")
        picked = [r for r in rows if r["cell"] == cell and not r.get("warmup")]
        picked.sort(key=lambda r: r["input_id"])
        if len(picked) != expected_count:
            raise ValueError(f"{plan_name}/{cell}: expected {expected_count} rows, got {len(picked)}")
        if len({r["input_id"] for r in picked}) != expected_count:
            raise ValueError(f"{plan_name}/{cell}: duplicate input_id")
        errs = [
            r for r in picked
            if r["status"] != "ok" or not r.get("oracle_ok") or not r.get("trace_valid") or r.get("end_to_end_ns", 0) <= 0
        ]
        if errs:
            raise ValueError(f"{plan_name}/{cell} had {len(errs)} invalid/failed/nonpositive rows")
        return [r["end_to_end_ns"] for r in picked], picked

    gates = {}
    all_passed = True

    def record(name, stats, threshold):
        nonlocal all_passed
        seed_ok = all(v <= threshold for v in stats["per_seed_ratios"].values())
        passed = stats["upper95"] <= threshold and seed_ok
        if not passed:
            all_passed = False
        gates[name] = dict(threshold=threshold, passed=passed, seed_ok=seed_ok, **stats)

    # P1 plain regression (<= 1.0)
    for backend in ["sqlite", "rocksdb"]:
        for label, p_tpl, cell, q, bsz, cnt in [
            (f"P1_noindex_point_p50_{backend}", "P1_noindex_{b}_{a}_s{s}", "p1_noindex_point", 0.5, None, 1000),
            (f"P1_noindex_update_p50_{backend}", "P1_noindex_{b}_{a}_s{s}", "p1_noindex_update", 0.5, 100, 2000),
            (f"P1_noindex_update_p99_{backend}", "P1_noindex_{b}_{a}_s{s}", "p1_noindex_update", 0.99, 100, 2000),
            (f"P1_point_p50_{backend}", "P1_search_{b}_{a}_s{s}", "point_read", 0.5, None, 1000),
            (f"P1_fts_p50_{backend}", "P1_search_{b}_{a}_s{s}", "fts_search", 0.5, None, 1000),
            (f"P1_vec_p50_{backend}", "P1_search_{b}_{a}_s{s}", "vec_search", 0.5, None, 1000),
            (f"P1_update_p50_{backend}", "P1_update_{b}_{a}_s{s}", "update", 0.5, 100, 2000),
            (f"P1_update_p99_{backend}", "P1_update_{b}_{a}_s{s}", "update", 0.99, 100, 2000),
        ]:
            cand, base = {}, {}
            for s in SEEDS:
                cd, cr = durations(p_tpl.format(b=backend, a="candidate", s=s), cell, cnt)
                bd, br = durations(p_tpl.format(b=backend, a="baseline", s=s), cell, cnt)
                if [r["input_id"] for r in cr] != [r["input_id"] for r in br]:
                    raise ValueError(f"{label} s{s}: input_id alignment mismatch")
                cand[s], base[s] = cd, bd
            record(label, bootstrap_ratio_upper95(cand, base, q, block_size=bsz), 1.0)

    # T1 current search (<= 1.5) + FTS exact tolerance 1e-6 + HNSW unique valid IDs, exact cosine distance 1e-6, recall@10 >= -0.01
    quality = {}
    for backend in ["sqlite", "rocksdb"]:
        for kind, cell in [("fts", "fts_search"), ("vec", "vec_search")]:
            tt_d, pl_d, tt_rows, pl_rows = {}, {}, {}, {}
            for s in SEEDS:
                tt_d[s], tt_rows[s] = durations(f"T1_{backend}_txtime_s{s}", cell, 1000)
                pl_d[s], pl_rows[s] = durations(f"T1_{backend}_plain_s{s}", cell, 1000)
                if [r["input_id"] for r in tt_rows[s]] != [r["input_id"] for r in pl_rows[s]]:
                    raise ValueError(f"T1_{kind}_{backend} s{s}: input_id mismatch")
            record(f"T1_{kind}_p50_{backend}", bootstrap_ratio_upper95(tt_d, pl_d, 0.5, block_size=None), 1.5)
            if kind == "fts":
                mismatches = 0
                for s in SEEDS:
                    for rt, rp in zip(tt_rows[s], pl_rows[s]):
                        ot, op = rt.get("output_rows", []), rp.get("output_rows", [])
                        if len(ot) != len(op):
                            mismatches += 1
                            continue
                        for (idt, st_), (idp, sp_) in zip(ot, op):
                            if idt != idp or abs(float(st_) - float(sp_)) > 1e-6 + 1e-6 * max(abs(float(st_)), abs(float(sp_))):
                                mismatches += 1
                                break
                quality[f"T1_fts_exact_{backend}"] = dict(mismatches=mismatches, passed=(mismatches == 0))
                if mismatches != 0:
                    all_passed = False
            else:
                diffs_by_seed = {}
                means = {}
                invalid_id_or_dup = 0
                dist_mismatches = 0
                for s in SEEDS:
                    gold = metas[s]["exact_topk_ids"]
                    cid_to_pos = metas[s]["cid_to_pos"]
                    cn, qn = c_norms[s], q_norms[s]
                    diffs, r_tt_list, r_pl_list = [], [], []
                    for qi, (rt, rp) in enumerate(zip(tt_rows[s], pl_rows[s])):
                        qv = qn[qi]
                        for out in (rt.get("output_rows", []), rp.get("output_rows", [])):
                            seen_ids = set()
                            for cid, dist in out:
                                if cid not in cid_to_pos or cid in seen_ids:
                                    invalid_id_or_dup += 1
                                    continue
                                seen_ids.add(cid)
                                expected_d = float(1.0 - float(np.dot(qv, cn[cid_to_pos[cid]])))
                                if abs(float(dist) - expected_d) > 1e-6 + 1e-6 * max(abs(float(dist)), abs(expected_d)):
                                    dist_mismatches += 1
                        g = set(gold[qi])
                        t_ids = list(dict.fromkeys(row[0] for row in rt.get("output_rows", [])[:10]))
                        p_ids = list(dict.fromkeys(row[0] for row in rp.get("output_rows", [])[:10]))
                        r_tt = sum(1 for x in t_ids if x in g) / 10.0
                        r_pl = sum(1 for x in p_ids if x in g) / 10.0
                        r_tt_list.append(r_tt)
                        r_pl_list.append(r_pl)
                        diffs.append(r_tt - r_pl)
                    diffs_by_seed[s] = diffs
                    means[str(s)] = dict(txtime=sum(r_tt_list) / len(r_tt_list), plain=sum(r_pl_list) / len(r_pl_list))
                rng = random.Random(BOOTSTRAP_SEED)
                boot_diff = []
                for _ in range(BOOTSTRAP_DRAWS):
                    chosen = [SEEDS[rng.randrange(len(SEEDS))] for _ in range(len(SEEDS))]
                    s_means = [
                        sum(diffs_by_seed[s][rng.randrange(len(diffs_by_seed[s]))] for _ in range(len(diffs_by_seed[s]))) / len(diffs_by_seed[s])
                        for s in chosen
                    ]
                    boot_diff.append(sum(s_means) / len(s_means))
                lower95 = diag.quantile(boot_diff, 0.05)
                passed = (lower95 >= -0.01) and (invalid_id_or_dup == 0) and (dist_mismatches == 0)
                if not passed:
                    all_passed = False
                quality[f"T1_hnsw_quality_{backend}"] = dict(
                    per_seed_recall10=means,
                    diff_lower95=lower95,
                    invalid_id_or_dup=invalid_id_or_dup,
                    dist_mismatches=dist_mismatches,
                    passed=passed,
                )

    # T2 single writer (<= 1.3)
    t2_tt_rocksdb = {}
    for backend in ["sqlite", "rocksdb"]:
        tt_d, pl_d = {}, {}
        for s in SEEDS:
            td, tr = durations(f"T2_{backend}_txtime_s{s}", "update", 2000)
            pd, pr = durations(f"T2_{backend}_plain_s{s}", "update", 2000)
            if [r["input_id"] for r in tr] != [r["input_id"] for r in pr]:
                raise ValueError(f"T2_{backend} s{s}: input_id mismatch")
            tt_d[s], pl_d[s] = td, pd
        if backend == "rocksdb":
            t2_tt_rocksdb = tt_d
        record(f"T2_update_p50_{backend}", bootstrap_ratio_upper95(tt_d, pl_d, 0.5, block_size=100), 1.3)
        record(f"T2_update_p99_{backend}", bootstrap_ratio_upper95(tt_d, pl_d, 0.99, block_size=100), 1.3)

    # C1 four writers (RocksDB): per-tx max section wait + coupled writer-preserving 100-block bootstrap.
    c1_events_by_seed = {}
    for s in SEEDS:
        run_dir = resolve_canonical_run_dir(runs_dir, f"C1_rocksdb_txtime_s{s}", canonical_attempts)
        validate_cell_run_provenance(run_dir)
        rows = load_cell_rows(run_dir)
        wrows = [r for r in rows if r["cell"] == "c1_writers" and r["writer"] > 0]
        if len(wrows) != 4000:
            raise ValueError(f"C1 s{s}: expected 4000 writer rows, got {len(wrows)}")
        by_w_count = {w: 0 for w in range(1, 5)}
        tx_records = []
        for r in wrows:
            if r["status"] != "ok" or not r.get("oracle_ok") or not r.get("trace_valid"):
                raise ValueError(f"C1 s{s} failed row {r['operation_id']}")
            by_w_count[r["writer"]] += 1
            tx_waits = [
                (e["start_ns"], (e["acquired_ns"] if e["acquired_ns"] is not None else e["end_ns"]) - e["start_ns"])
                for e in r["timing"]["events"] if e["phase"] == "tt_commit_section"
            ]
            if not tx_waits:
                raise ValueError(f"C1 s{s}: missing tt_commit_section trace on {r['operation_id']}")
            earliest = min(st for st, _ in tx_waits)
            max_wait = max(dur for _, dur in tx_waits)
            tx_records.append((earliest, r["writer"], max_wait))
        if any(c != 1000 for c in by_w_count.values()):
            raise ValueError(f"C1 s{s}: uneven writer counts {by_w_count}")
        tx_records.sort(key=lambda x: x[0])
        c1_events_by_seed[s] = [(wid, w) for _, wid, w in tx_records]

    c1_res = analyze_c1_bootstrap(c1_events_by_seed, t2_tt_rocksdb, block_size=100)
    gates["C1_section_wait_p99_rocksdb"] = c1_res
    if not c1_res["passed"]:
        all_passed = False

    # C2 build with writers (RocksDB)
    c2_results = {}
    for cname in ["C2_FTS", "C2_HNSW", "C2_companion_FTS", "C2_companion_HNSW"]:
        per_seed = {}
        cname_passed = True
        for s in SEEDS:
            run_dir = resolve_canonical_run_dir(runs_dir, f"{cname}_rocksdb_s{s}", canonical_attempts)
            validate_cell_run_provenance(run_dir)
            rows = load_cell_rows(run_dir)
            sv_bad = [r for r in rows if r["cell"] in ("setup", "verify") and (r["status"] != "ok" or not r.get("oracle_ok") or not r.get("trace_valid"))]
            if sv_bad:
                res = dict(passed=False, reason=f"setup/verify failed ({len(sv_bad)})")
            else:
                res = analyze_c2_cell(rows, cname.lower(), diag.quantile(t2_tt_rocksdb[s], 0.99))
            if not res["passed"]:
                cname_passed = False
            per_seed[str(s)] = res
        if not cname_passed:
            all_passed = False
        c2_results[cname] = dict(passed=cname_passed, per_seed=per_seed)

    # H1 historical fallback (report-only; timeouts/failures recorded cleanly without aborting gated summary).
    h1_results = {}
    for backend in ["sqlite", "rocksdb"]:
        h1_results[backend] = {}
        for s in SEEDS:
            run_dir = resolve_canonical_run_dir(runs_dir, f"H1_{backend}_txtime_s{s}", canonical_attempts)
            if not run_dir.exists():
                h1_results[backend][str(s)] = dict(status="missing")
                continue
            prov_ok, prov_msg = validate_cell_run_provenance(run_dir, allow_incomplete=True)
            rows = load_cell_rows(run_dir)
            hrows = [r for r in rows if r["cell"] == "h1_history"]
            ok_durs = [r["end_to_end_ns"] for r in hrows if r["status"] == "ok" and r.get("oracle_ok") and r.get("trace_valid")]
            errors = [r["status"] for r in hrows if r["status"] != "ok"]
            phys_rows = [r.get("output_rows") for r in rows if r["cell"] == "verify" and r["input_id"] == "physical-count"]
            h1_results[backend][str(s)] = dict(
                status="complete" if (prov_ok and len(ok_durs) == 100 and not errors) else "incomplete_or_timeout",
                provenance_note=prov_msg,
                total_rows=len(hrows),
                ok_count=len(ok_durs),
                error_count=len(errors),
                p50_ns=diag.quantile(ok_durs, 0.5) if ok_durs else None,
                p99_ns=diag.quantile(ok_durs, 0.99) if ok_durs else None,
                physical_count_verify=phys_rows[0] if phys_rows else None,
            )

    summary = dict(
        version=1,
        protocol="docs/plans/ea3-fly-linux-amendment.md",
        seeds=SEEDS,
        all_gates_passed=all_passed,
        gates=gates,
        quality=quality,
        c2=c2_results,
        h1_report_only=h1_results,
    )
    save_pretty(output_dir / "qualification-summary.json", summary)
    return summary


def run_qualification(candidate_dir, baseline_dir, controls_dir, bench_repo, output_dir):
    candidate_dir, baseline_dir, controls_dir, bench_repo, output_dir = (
        Path(p).resolve() for p in [candidate_dir, baseline_dir, controls_dir, bench_repo, output_dir]
    )
    output_dir.mkdir(parents=True)
    started = time.monotonic()
    env = dict(RAYON_NUM_THREADS="2", OMP_NUM_THREADS="1", OPENBLAS_NUM_THREADS="1")
    os.environ.update(env)

    # Mirror every Rust inputs.rs check up front before generating inputs.
    comp_path = controls_dir / "campaign-completion.json"
    cman_path = controls_dir / "campaign-manifest.json"
    comp = json.loads(comp_path.read_text())
    cman_sha = phase.file_hash(cman_path)
    if not (
        comp.get("all_recording_controls_passed") is True
        and comp.get("phase") == 3
        and comp.get("completed_blocks") == 180
        and comp.get("scripts_and_protocol_unchanged") is True
        and comp.get("campaign_manifest_sha256") == cman_sha
    ):
        raise RuntimeError("Stage 1 recording controls did not pass 180/180 cleanly; qualification blocked")

    builds = {}
    for arm, bdir in [("candidate", candidate_dir), ("baseline", baseline_dir)]:
        info = json.loads((bdir / "build.json").read_text())
        exe = bdir / "keyed_txtime_timing"
        if phase.file_hash(exe) != info["executable_sha256"]:
            raise ValueError(f"{arm} executable hash mismatch")
        builds[arm] = dict(
            directory=str(bdir),
            executable=str(exe),
            executable_sha256=info["executable_sha256"],
            build_manifest=str(bdir / "build.json"),
            build_manifest_sha256=phase.file_hash(bdir / "build.json"),
            build=info,
        )
    if builds["baseline"]["build"]["source"]["head"] != phase.BASELINE_HEAD:
        raise ValueError("baseline head differs from protocol")
    for key in ["native", "rustc"]:
        if builds["candidate"]["build"][key] != builds["baseline"]["build"][key]:
            raise ValueError(f"mismatched build provenance: {key}")
    if phase.semantic_environment(builds["candidate"]["build"]) != phase.semantic_environment(builds["baseline"]["build"]):
        raise ValueError("mismatched compiler/profile flags")

    inputs_dir = output_dir / "inputs"
    plans_dir = output_dir / "plans"
    runs_dir = output_dir / "runs"
    runs_dir.mkdir()

    metas = {}
    all_input_hashes = {}
    for seed in SEEDS:
        meta = generate_seed_inputs(bench_repo, seed, inputs_dir)
        metas[seed] = meta
        all_input_hashes.update(meta["input_hashes"])
    all_specs = build_all_plans(metas, plans_dir)

    bench_head = subprocess.check_output(["git", "-C", str(bench_repo), "rev-parse", "HEAD"], text=True).strip()
    proto_path = Path(__file__).resolve().parents[1] / "docs/plans/ea3-fly-linux-amendment.md"
    scripts_list = [
        Path(__file__),
        Path(host.__file__),
        Path(harness.__file__),
        Path(phase.__file__),
        Path(diag.__file__),
    ]
    qual_manifest = dict(
        version=1,
        qualification_authorized=True,
        protocol=str(proto_path),
        protocol_sha256=phase.file_hash(proto_path),
        script_sha256={str(p.resolve()): phase.file_hash(p) for p in scripts_list},
        mnestic_benchmarks_head=bench_head,
        python_version=sys.version,
        numpy_version=np.__version__,
        host=dict(platform=sys.platform, uname=platform.uname()._asdict()),
        environment=env,
        controls_completion=dict(path=str(comp_path), sha256=phase.file_hash(comp_path)),
        controls_manifest=dict(path=str(cman_path), sha256=cman_sha),
        builds={arm: {k: v for k, v in b.items() if k != "build"} for arm, b in builds.items()},
        inputs=all_input_hashes,
        plans={
            s["sha256"]: dict(
                name=s["name"],
                group=s["group"],
                seed=s["seed"],
                backend=s["backend"],
                arm=s["arm"],
                report_only=s["report_only"],
                executable_sha256=builds[s["arm"]]["executable_sha256"],
            )
            for s in all_specs
        },
        order=[s["name"] for s in all_specs],
    )
    qman_path = output_dir / "qualification-manifest.json"
    save_pretty(qman_path, qual_manifest)
    os.environ["MNESTIC_QUALIFICATION_MANIFEST"] = str(qman_path)

    completed_cells = []
    contaminated_retries = []
    canonical_attempts = {}
    stop_reason = None
    summary = None

    try:
        for spec in all_specs:
            name = spec["name"]
            arm = builds[spec["arm"]]
            attempt = 0
            while True:
                remaining = QUAL_BUDGET_SECONDS - (time.monotonic() - started)
                if remaining <= 0:
                    stop_reason = dict(reason="wall_budget_exceeded", cell=name)
                    raise RuntimeError("Stage 2 6-hour qualification wall budget exceeded")
                attempt += 1
                att_dir = runs_dir / name / f"attempt-{attempt}"
                att_dir.mkdir(parents=True, exist_ok=True)
                run_out = att_dir / "run"
                pre = host.preflight(
                    run_out.with_suffix(".preflight.jsonl"),
                    output_dir,
                    max_seconds=min(300, remaining),
                    quiet_seconds=30,
                )
                save_pretty(run_out.with_suffix(".preflight-result.json"), pre)
                if not pre["eligible"]:
                    stop_reason = dict(reason="host_preflight", cell=name, detail=pre)
                    raise RuntimeError(f"host preflight failed before {name}: {pre}")
                remaining = QUAL_BUDGET_SECONDS - (time.monotonic() - started)
                if remaining <= 0:
                    stop_reason = dict(reason="wall_budget_exceeded", cell=name)
                    raise RuntimeError("Stage 2 6-hour qualification wall budget exceeded")
                monitor = host.Monitor(run_out.with_suffix(".host.jsonl"), output_dir, pre["final_sample"]["swapouts"])
                try:
                    harness.supervise(
                        arm["executable"],
                        spec["path"],
                        run_out,
                        arm["build_manifest"],
                        min(MAX_CELL_SECONDS, remaining),
                        4096,
                        2048,
                        monitor,
                    )
                except RuntimeError as err:
                    monitor.close()
                    comp_cell = (
                        json.loads(run_out.with_suffix(".completion.json").read_text())
                        if run_out.with_suffix(".completion.json").exists()
                        else {}
                    )
                    reason = comp_cell.get("stop_reason")
                    if attempt == 1 and reason in ("competing_native_process", "swapout_change", "sustained_competing_cpu", "hypervisor_steal"):
                        if run_out.exists():
                            phase.archive(run_out)
                            _cleanup_db(run_out)
                        contaminated_retries.append(dict(cell=name, reason=reason, attempt=attempt, path=str(att_dir)))
                        continue
                    if spec["report_only"]:
                        # Report-only H1 timeout/failure is preserved without aborting gated summary emission.
                        if run_out.exists():
                            phase.archive(run_out)
                            _cleanup_db(run_out)
                        canonical_attempts[name] = f"attempt-{attempt}"
                        completed_cells.append(name)
                        break
                    stop_reason = dict(reason="cell_failed", cell=name, attempt=attempt, error=str(err))
                    raise
                else:
                    monitor.close()
                phase.archive(run_out)
                _cleanup_db(run_out)
                canonical_attempts[name] = f"attempt-{attempt}"
                completed_cells.append(name)
                break
        summary = analyze_qualification(output_dir, inputs_dir, canonical_attempts=canonical_attempts)
    except Exception as exc:
        if stop_reason is None:
            stop_reason = dict(reason="unexpected_error", error=str(exc))
        raise
    finally:
        completion = dict(
            version=1,
            status="complete" if (summary is not None and stop_reason is None) else "incomplete",
            all_gates_passed=bool(summary and summary.get("all_gates_passed")),
            elapsed_s=time.monotonic() - started,
            completed_cells=len(completed_cells),
            total_cells=len(all_specs),
            canonical_attempts=canonical_attempts,
            contaminated_retries=contaminated_retries,
            stop_reason=stop_reason,
            qualification_manifest_sha256=phase.file_hash(qman_path),
        )
        save_pretty(output_dir / "qualification-completion.json", completion)
    return summary


def _cleanup_db(run_out):
    db_path = Path(run_out) / "database"
    if db_path.is_dir():
        shutil.rmtree(db_path)
    elif db_path.is_file():
        db_path.unlink()
    for suffix in ("-wal", "-shm"):
        side = Path(run_out) / f"database{suffix}"
        if side.is_file():
            side.unlink()


def main():
    p = argparse.ArgumentParser(__doc__)
    sub = p.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    r.add_argument("--candidate", required=True)
    r.add_argument("--baseline", required=True)
    r.add_argument("--controls", required=True)
    r.add_argument("--bench-repo", required=True)
    r.add_argument("--output", required=True)
    a = sub.add_parser("analyze")
    a.add_argument("--output", required=True)
    a.add_argument("--inputs", required=True)
    args = p.parse_args()
    if args.cmd == "run":
        res = run_qualification(args.candidate, args.baseline, args.controls, args.bench_repo, args.output)
        print(json.dumps({"all_gates_passed": res["all_gates_passed"]}, indent=2))
    else:
        res = analyze_qualification(args.output, args.inputs)
        print(json.dumps({"all_gates_passed": res["all_gates_passed"]}, indent=2))


if __name__ == "__main__":
    main()
