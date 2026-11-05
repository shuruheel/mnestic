"""Small correctness-only HNSW update matrix; no performance measurements.

Read public base rows and internal graph rows from disposable fixture databases.
Never edits graph rows. Compare identical data before/after a supported rebuild.
"""
import argparse
from collections import Counter, defaultdict
import hashlib
import importlib.metadata
import json
from pathlib import Path
import tempfile

import mnestic

INDEX = "::hnsw create docs:vec {fields:[v],dim:2,dtype:F32,distance:L2,m:8,ef_construction:50}"
CASES = {
    "metadata_only": "?[id,kind,text,v] <- [[20,'common','alpha',[20,20]]] :put docs{id=>kind,text,v}",
    "duplicate_update": "?[id,kind,text,v] <- [[23,'rare','alpha',[22,22]]] :put docs{id=>kind,text,v}",
    "distinct_update": "?[id,kind,text,v] <- [[23,'rare','alpha',[22.5,22.5]]] :put docs{id=>kind,text,v}",
    "delete_bridge": "?[id] <- [[23]] :rm docs{id}",
    "delete_leaf": "?[id] <- [[24]] :rm docs{id}",
    "insert_duplicate": "?[id,kind,text,v] <- [[25,'rare','alpha',[22,22]]] :put docs{id=>kind,text,v}",
    "original_combined": "?[id,kind,text,v] <- [[20,'common','alpha',[20,20]],[23,'rare','alpha',[22,22]]] :put docs{id=>kind,text,v}",
    "rollback_update": "{ ?[id,kind,text,v] <- [[23,'rare','alpha',[22,22]]] :put docs{id=>kind,text,v} } { ?[x] := *missing019[x] }",
}


def snapshot(run):
    rows = run("?[id,kind,text,v] := *docs{id,kind,text,v} :order id")
    exact = [row[0] for row in sorted(rows, key=lambda r: (sum(x*x for x in r[3]), r[0]))]
    eligible = [row[0] for row in sorted(rows, key=lambda r: (sum(x*x for x in r[3]), r[0])) if row[1] == "rare"]
    graph = run("?[layer,src,dst,ignored] := *docs:vec{layer,fr_id:src,to_id:dst,ignore_link:ignored}, layer<=0 :order layer,src,dst")
    adjacency = defaultdict(set)
    for layer, src, dst, ignored in graph:
        if layer == 0 and not ignored and src != dst:
            adjacency[src].add(dst)
    reachable = {}
    for start in exact:
        seen, todo = {start}, [start]
        while todo:
            for dest in adjacency[todo.pop()] - seen:
                seen.add(dest); todo.append(dest)
        reachable[str(start)] = sorted(seen)
    searches = {}
    for ef in (50, 1600):
        for name, predicate in (("unfiltered", ""), ("filtered", ", filter:kind=='rare'")):
            script = f"?[id,d] := ~docs:vec{{id,kind | query:vec([0,0]),k:30,ef:{ef},bind_distance:d{predicate}}} :order d,id"
            found = run(script)
            truth = eligible if name == "filtered" else exact
            searches[f"{name}_{ef}"] = {"rows": found, "missing": sorted(set(truth)-{r[0] for r in found}),
                "unexpected": sorted({r[0] for r in found}-set(truth))}
    return {"base": rows, "exact": exact, "eligible": eligible, "graph": graph,
            "reachable_layer0": reachable, "searches": searches}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--repetitions", type=int, default=10)
    args = parser.parse_args()
    extension = next(Path(mnestic.__file__).parent.glob("*.so"))
    records = []
    for backend in ("sqlite", "rocksdb"):
        for name, mutation in CASES.items():
            for repetition in range(args.repetitions):
                with tempfile.TemporaryDirectory() as directory:
                    db = mnestic.CozoDbPy(backend, str(Path(directory)/"data"), "{}")
                    def run(script, params=None):
                        return db.run_script(script, params or {}, False)["rows"]
                    run(":create docs {id:Int=>kind:String,text:String,v:<F32;2>}")
                    run("?[id,kind,text,v] <- $r :put docs{id=>kind,text,v}",
                        {"r": [[i,"rare" if i in (20,22,24) else "common","alpha",[float(i),float(i)]] for i in range(25)]})
                    run(INDEX)
                    before = snapshot(run)
                    error = None
                    try: run(mutation)
                    except Exception as exc:
                        error = str(exc)
                        if name != "rollback_update": raise
                    assert bool(error) == (name == "rollback_update")
                    after = snapshot(run)
                    if name == "rollback_update":
                        assert before["base"] == after["base"] and before["graph"] == after["graph"]
                    run("::hnsw drop docs:vec")
                    run(INDEX)
                    rebuilt = snapshot(run)
                    assert after["base"] == rebuilt["base"]
                    assert not any(x["unexpected"] for s in (before,after,rebuilt) for x in s["searches"].values())
                    records.append({"backend":backend,"case":name,"repetition":repetition,
                                    "mutation":mutation,"error":error,"before":before,"after":after,"rebuilt":rebuilt})
                    db.close()
    report = {"package":importlib.metadata.version("mnestic"),
        "extension_sha256":hashlib.sha256(extension.read_bytes()).hexdigest(),
        "fixture_sha256":hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "index":INDEX,"repetitions":args.repetitions,"hnsw_rng":"uncontrolled",
        "timing":"not measured","records":records}
    args.output.write_text(json.dumps(report,separators=(",",":"))+"\n")
    for backend in ("sqlite","rocksdb"):
        for case in CASES:
            subset=[r for r in records if r["backend"]==backend and r["case"]==case]
            counts={stage:sum(bool(r[stage]["searches"]["unfiltered_50"]["missing"]) for r in subset)
                    for stage in ("before","after","rebuilt")}
            print(backend,case,"unfiltered omissions",counts)


if __name__ == "__main__": main()
