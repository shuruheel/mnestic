"""Unit tests for Stage 2 EA3 qualification orchestrator & analyzer (ea3_qualify.py)."""
import json
from pathlib import Path
import tempfile
import unittest

import numpy as np

import benchmark_host as host
import ea3_qualify as qual


def _make_c2_rows(cell_name, include_reservation=True, overlap_all=True, final_after_ddl=True, wait_ns=1_000):
    events = []
    if include_reservation:
        events.append(dict(phase="build_reservation", start_ns=10, end_ns=20, completed=True, acquired_ns=None, release_begin_ns=None))
    events.extend([
        dict(phase="build_registration", start_ns=20, end_ns=40, completed=True, acquired_ns=None, release_begin_ns=None),
        dict(phase="build_backfill", start_ns=40, end_ns=80, completed=True, acquired_ns=None, release_begin_ns=None),
        dict(phase="build_catchup", start_ns=80, end_ns=100, completed=True, acquired_ns=None, release_begin_ns=None),
        dict(phase="build_publication", start_ns=100, end_ns=120, completed=True, acquired_ns=None, release_begin_ns=None),
        dict(phase="tt_ddl_section", start_ns=20, end_ns=40, completed=True, acquired_ns=21, release_begin_ns=39),
        dict(phase="tt_ddl_section", start_ns=80, end_ns=120, completed=True, acquired_ns=81, release_begin_ns=119),
    ])
    rows = [
        dict(
            cell=cell_name, writer=0, operation_id=1, input_id="ddl", status="ok",
            oracle_ok=True, trace_valid=True, end_to_end_ns=120,
            timing=dict(start_ns=10, end_ns=120, events=events),
        )
    ]
    op_id = 2
    for w in range(1, 5):
        # First writer op overlaps all 4 required phases when overlap_all is True.
        start_a = 15
        end_a = max(115, 30 + wait_ns) if overlap_all else max(35, 30 + wait_ns)
        rows.append(
            dict(
                cell=cell_name, writer=w, operation_id=op_id, input_id=f"w{w}-0", status="ok",
                oracle_ok=True, trace_valid=True, end_to_end_ns=end_a - start_a,
                timing=dict(
                    start_ns=start_a, end_ns=end_a,
                    events=[dict(phase="tt_commit_section", start_ns=25, acquired_ns=25 + wait_ns, end_ns=30 + wait_ns, completed=True, release_begin_ns=None)],
                ),
            )
        )
        op_id += 1
        # Final acknowledged commit after ddl_end (120) if final_after_ddl.
        s_f = 125 if final_after_ddl else 90
        rows.append(
            dict(
                cell=cell_name, writer=w, operation_id=op_id, input_id=f"w{w}-1", status="ok",
                oracle_ok=True, trace_valid=True, end_to_end_ns=10,
                timing=dict(
                    start_ns=s_f, end_ns=s_f + 10,
                    events=[dict(phase="tt_commit_section", start_ns=s_f, acquired_ns=s_f + 1, end_ns=s_f + 5, completed=True, release_begin_ns=None)],
                ),
            )
        )
        op_id += 1
    return rows


class Ea3QualifyTests(unittest.TestCase):
    def test_c2_accepts_five_emitted_spans_and_rejects_missing_phase_or_final_commit(self):
        rows = _make_c2_rows("c2_fts", include_reservation=True, overlap_all=True, final_after_ddl=True, wait_ns=10)
        res = qual.analyze_c2_cell(rows, "c2_fts", t2_p99=100)
        self.assertTrue(res["passed"])
        self.assertEqual(len(res["phases"]), 5)
        self.assertTrue(res["required_phases_covered"])
        self.assertTrue(res["writers_final_commit_after_ddl"])

        # Missing overlap on later phases must fail.
        rows_no_ov = _make_c2_rows("c2_fts", overlap_all=False, wait_ns=10)
        self.assertFalse(qual.analyze_c2_cell(rows_no_ov, "c2_fts", t2_p99=100)["passed"])

        # Missing final acknowledged commit after DDL must fail.
        rows_no_final = _make_c2_rows("c2_fts", final_after_ddl=False, wait_ns=10)
        self.assertFalse(qual.analyze_c2_cell(rows_no_final, "c2_fts", t2_p99=100)["passed"])

        # Wait exceeding 3x T2 p99 must fail.
        rows_slow = _make_c2_rows("c2_fts", wait_ns=400)
        self.assertFalse(qual.analyze_c2_cell(rows_slow, "c2_fts", t2_p99=100)["passed"])

    def test_c1_coupled_bootstrap_catches_single_slow_writer(self):
        # Writers 1..3 have 100ns wait, writer 4 has 500ns wait; T2 p99 is 100ns.
        # Pooled p99 and max-writer p99 are both 5x > 3x, so conservative gate fails.
        c1 = {s: [((i % 4) + 1, 500 if (i % 4) == 3 else 100) for i in range(400)] for s in qual.SEEDS}
        t2 = {s: [100] * 200 for s in qual.SEEDS}
        res = qual.analyze_c1_bootstrap(c1, t2, block_size=20, draws=200)
        self.assertFalse(res["passed"])
        self.assertFalse(res["max_writer_passed"])
        self.assertGreater(res["max_writer_upper95"], 3.0)

        # All writers <= 200ns vs T2 100ns -> ratio 2.0 <= 3.0 passes both pooled and max-writer.
        c1_ok = {s: [((i % 4) + 1, 200) for i in range(400)] for s in qual.SEEDS}
        res_ok = qual.analyze_c1_bootstrap(c1_ok, t2, block_size=20, draws=200)
        self.assertTrue(res_ok["passed"])
        self.assertTrue(res_ok["pooled_passed"])
        self.assertTrue(res_ok["max_writer_passed"])

    def test_bootstrap_rejects_mismatched_lengths(self):
        with self.assertRaisesRegex(ValueError, "paired length mismatch"):
            qual.bootstrap_ratio_upper95({1234: [10, 20]}, {1234: [10]}, 0.5, draws=10)

    def test_cell_major_order_places_h1_last(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp)
            dummy_ref = {"path": str(p / "x.json"), "sha256": "0" * 64}
            metas = {}
            for s in qual.SEEDS:
                metas[s] = dict(
                    seed=s,
                    batch_refs=[dummy_ref],
                    inc_vec_refs=[dummy_ref],
                    inc_texts=["alpha beta"],
                    q_warmup_fts=["alpha"],
                    q_warmup_vec_refs=[dummy_ref],
                    q_meas_ids=["q0"],
                    q_meas_fts=["alpha"],
                    q_meas_vec_refs=[dummy_ref],
                    exact_topk_ids=[["c0"]],
                    cid_to_pos={"c0": 0},
                    pr_warmup=["c0"],
                    pr_measured=["c0"],
                    pr_present_set=["c0"],
                    upd_warmup_meta=[("c0", 0, "note")],
                    upd_meas_meta=[("c0", 0, "note")],
                    upd_verify=dict(cid="c0", vi=0, kind="note", fresh_tok="alpha", stale_tok="oldtok"),
                    c1_workers_meta=[[("c0", 0, "note")]] * 4,
                    c2_workers_cids=[["c0"]] * 4,
                    c2_workers_kinds=[["note"]] * 4,
                    h1_bucket_ref=dummy_ref,
                    all_cids_ref=dummy_ref,
                    h1_expected_by_bucket={str(k): [["c0"]] for k in range(100)},
                )
            specs = qual.build_all_plans(metas, p / "plans")
            groups = [s["group"] for s in specs]
            first_h1 = groups.index("H1")
            self.assertTrue(all(g == "H1" for g in groups[first_h1:]))
            self.assertEqual(groups[0], "P1")
            self.assertTrue(all(s["report_only"] for s in specs[first_h1:]))
            self.assertTrue(all(not s["report_only"] for s in specs[:first_h1]))

    def test_linux_truncated_comm_and_instantaneous_cpu(self):
        # Truncated 15-char Linux comm for keyed_txtime_timing must still trigger competing_native_process.
        sample = host.observation(
            [(999, 1, 1000, 1.0, "keyed_txtime_ti")],
            excluded=set(),
            swap_raw="pswpout 0\n",
            disk=".",
            power_state=dict(platform="linux", ac_power=True, low_power_disabled=True),
        )
        self.assertEqual(host.invalid(sample, 0), "competing_native_process")

    def test_stage1_fail_closed_gate_rejects_unpassed_controls(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            controls = root / "controls"
            controls.mkdir()
            (controls / "campaign-manifest.json").write_text("{}")
            (controls / "campaign-completion.json").write_text(
                json.dumps(dict(all_recording_controls_passed=False, phase=4, completed_blocks=180))
            )
            with self.assertRaisesRegex(RuntimeError, "qualification blocked"):
                qual.run_qualification(root / "cand", root / "base", controls, root / "bench", root / "out")

    def test_resolve_canonical_attempt_prefers_clean_retry_attempt_dir(self):
        with tempfile.TemporaryDirectory() as tmp:
            runs = Path(tmp)
            (runs / "P1_x" / "attempt-1" / "run").mkdir(parents=True)
            (runs / "P1_x" / "attempt-2" / "run").mkdir(parents=True)
            self.assertEqual(
                qual.resolve_canonical_run_dir(runs, "P1_x"),
                runs / "P1_x" / "attempt-2" / "run",
            )
            self.assertEqual(
                qual.resolve_canonical_run_dir(runs, "P1_x", {"P1_x": "attempt-1"}),
                runs / "P1_x" / "attempt-1" / "run",
            )


if __name__ == "__main__":
    unittest.main()
