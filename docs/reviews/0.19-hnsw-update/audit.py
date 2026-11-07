"""Audit retained evidence without loading the native extension or running ANN."""
from collections import defaultdict
import gzip
import hashlib
import json
from pathlib import Path


ROOT = Path(__file__).parent


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def audit():
    manifest = json.loads((ROOT / "manifest.json").read_text())
    for name, expected in manifest["files"].items():
        assert digest(ROOT / name) == expected, name
    summary = {}
    for build in ("baseline", "candidate"):
        data = json.loads(gzip.decompress((ROOT / f"{build}.json.gz").read_bytes()))
        assert data["fixture_sha256"] == digest(ROOT / "investigate.py")
        assert data["extension_sha256"] == manifest[build]["extension_sha256"]
        assert len(data["records"]) == 160
        counts = defaultdict(lambda: defaultdict(int))
        failures = 0
        for record in data["records"]:
            for stage in ("before", "after", "rebuilt"):
                snap = record[stage]
                ids = {row[0] for row in snap["base"]}
                eligible = {row[0] for row in snap["base"] if row[1] == "rare"}
                adjacency = defaultdict(set)
                for layer, src, dst, ignored in snap["graph"]:
                    if layer == 0 and src != dst and not ignored:
                        adjacency[src].add(dst)
                for start in ids:
                    seen, todo = {start}, [start]
                    while todo:
                        for dest in adjacency[todo.pop()] - seen:
                            seen.add(dest)
                            todo.append(dest)
                    assert sorted(seen) == snap["reachable_layer0"][str(start)]
                for ef in (50, 1600):
                    for name, truth in (("unfiltered", ids), ("filtered", eligible)):
                        result = snap["searches"][f"{name}_{ef}"]
                        found = [row[0] for row in result["rows"]]
                        assert len(found) == len(set(found))
                        assert set(found) <= truth
                        assert sorted(truth - set(found)) == result["missing"]
                        assert not result["unexpected"]
                        if stage != "after":
                            assert not result["missing"]
                unfiltered = set(snap["searches"]["unfiltered_50"]["missing"])
                assert unfiltered == set(snap["searches"]["unfiltered_1600"]["missing"])
                assert set(snap["searches"]["filtered_50"]["missing"]) == unfiltered & eligible
                assert set(snap["searches"]["filtered_1600"]["missing"]) == unfiltered & eligible
                assert unfiltered == ids - set(snap["reachable_layer0"]["0"])
                if stage == "after" and unfiltered:
                    failures += 1
                    counts[record["case"]][record["backend"]] += 1
            assert record["after"]["base"] == record["rebuilt"]["base"]
            if record["case"] == "rollback_update":
                assert record["before"] == record["after"]
            if record["case"] != "delete_leaf":
                assert [row for row in record["after"]["base"] if row[0] == 24] == [
                    [24, "rare", "alpha", [24.0, 24.0]]]
        exact = json.loads(gzip.decompress((ROOT / f"exact-{build}.json.gz").read_bytes()))
        assert exact["fixture_sha256"] == digest(ROOT / "exact_eligible.py")
        assert exact["extension_sha256"] == data["extension_sha256"]
        checks = [r for r in exact["results"] if "actual" in r]
        assert len(checks) == 672
        assert all(r["actual"] == r["expected"] for r in checks)
        plans = [r for r in exact["results"] if "plan" in r]
        assert len(plans) == 2
        assert all(any(row[4] == "stored_prefix_join" for row in r["plan"]) for r in plans)
        summary[build] = {"fixtures": len(data["records"]), "omission_fixtures": failures,
                          "omissions_by_case_backend": dict(counts), "exact_assertions": len(checks)}
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    audit()
