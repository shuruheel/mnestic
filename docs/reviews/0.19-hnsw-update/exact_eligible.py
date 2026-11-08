"""Correctness example for exact eligible-key retrieval; no timing or budget claim.

Uses only existing CozoScript and disposable SQLite/RocksDB databases. Run with
the same isolated Python interpreters as investigate.py.
"""
import argparse
import hashlib
import importlib.metadata
import json
from pathlib import Path
import tempfile

import mnestic

from investigate import CASES, INDEX


QUERY = """
eligible[id] <- $eligible_rows
?[id,d] := eligible[id], *docs{id,kind,v}, kind == $kind,
           d = l2_dist(v,vec($q))
:order d,id
:limit $k
"""


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    results = []
    for backend in ("sqlite", "rocksdb"):
        for name, mutation in CASES.items():
            with tempfile.TemporaryDirectory() as directory:
                db = mnestic.CozoDbPy(backend, str(Path(directory) / "data"), "{}")

                def run(script, params=None):
                    return db.run_script(script, params or {}, False)["rows"]

                run(":create docs {id:Int=>kind:String,text:String,v:<F32;2>}")
                run("?[id,kind,text,v] <- $r :put docs{id=>kind,text,v}", {
                    "r": [[i, "rare" if i in (20, 22, 24) else "common", "alpha",
                           [float(i), float(i)]] for i in range(25)]})
                run(INDEX)
                initial_rows = run("?[id,kind,text,v] := *docs{id,kind,text,v} :order id")
                for stage in ("before", "after"):
                    if stage == "after":
                        error = None
                        try:
                            run(mutation)
                        except Exception as exc:
                            error = str(exc)
                            if name != "rollback_update":
                                raise
                        assert bool(error) == (name == "rollback_update")
                    rows = run("?[id,kind,text,v] := *docs{id,kind,text,v} :order id")
                    if name == "rollback_update":
                        assert rows == initial_rows
                    candidate_sets = {
                        "all": list(range(26)),
                        "empty": [],
                        "missing": [99],
                        "duplicates_missing_ineligible": [22, 22, 23, 24, 99, 0],
                        "single": [24],
                        "ties": [22, 23],
                        "ineligible": [0, 1],
                    }
                    for label, keys in candidate_sets.items():
                        for k in (1, 3, 30):
                            params = {"eligible_rows": [[key] for key in keys],
                                      "kind": "rare", "q": [0, 0], "k": k}
                            expected = sorted(
                                [[r[0], sum(x * x for x in r[3])]
                                 for r in rows if r[0] in set(keys) and r[1] == "rare"],
                                key=lambda r: (r[1], r[0]))[:k]
                            actual = run(QUERY, params)
                            assert actual == expected, (backend, name, stage, label, k, actual, expected)
                            results.append({"backend": backend, "mutation": name,
                                            "stage": stage, "candidates": label, "k": k,
                                            "actual": actual, "expected": expected})
                    if stage == "after" and name == "original_combined":
                        explanation = run("::explain {" + QUERY + "}", params)
                        results.append({"backend": backend, "plan": explanation})
                db.close()
    extension = next(Path(mnestic.__file__).parent.glob("*.so"))
    report = {"package": importlib.metadata.version("mnestic"), "query": QUERY,
              "extension_sha256": hashlib.sha256(extension.read_bytes()).hexdigest(),
              "fixture_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "timing": "not measured", "results": results}
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(f"PASS: {len(results) - 2} exact-query assertions; 2 query plans captured")


if __name__ == "__main__":
    main()
