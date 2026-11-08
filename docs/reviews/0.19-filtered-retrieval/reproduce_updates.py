"""Tiny deterministic filter/update/delete controls; no approximate recall claim."""
import tempfile
from pathlib import Path
from mnestic import CozoDbPy

failures = []
for backend in ("sqlite", "rocksdb"):
    with tempfile.TemporaryDirectory() as directory:
        db = CozoDbPy(backend, str(Path(directory)/"data"), "{}")
        def run(script, params=None):
            return db.run_script(script, params or {}, False)["rows"]
        run(":create docs {id: Int => kind: String, text: String, v: <F32;2>}")
        run("?[id,kind,text,v] <- $r :put docs {id=>kind,text,v}",
            {"r": [[i,"rare" if i in (20,22,24) else "common","alpha",[float(i),float(i)]] for i in range(25)]})
        run("::hnsw create docs:vec {dim:2,m:8,fields:[v],distance:L2,dtype:F32,ef_construction:50}")
        run("::fts create docs:fts {extractor:text,tokenizer:Simple,filters:[Lowercase]}")
        query="?[id,d] := ~docs:vec{id,kind | query:vec([0,0]),k:10,ef:50,filter:kind==$kind,bind_distance:d} :order d,id"
        def check(expected):
            for _ in range(2):
                rows=run(query,{"kind":"rare"})
                if [r[0] for r in rows] != expected:
                    failures.append((backend, expected, rows))
                assert run(query,{"kind":"absent"})==[]
            assert [r[0] for r in run("?[id] := ~docs:fts{id | query:'alpha',k:25,candidates:$ids} :order id",{"ids":expected})]==sorted(expected)
        check([20,22,24])
        run("?[id,kind,text,v] <- [[20,'common','alpha',[20,20]],[23,'rare','alpha',[22,22]]] :put docs {id=>kind,text,v}")
        check([22,23,24])  # equal distances are ordered by key
        run("?[id] <- [[22]] :rm docs {id}")
        check([23,24])
        try:
            run("{ ?[id] <- [[23]] :rm docs {id} } { ?[x] := *missing[x] }")
        except Exception:
            pass
        else:
            raise AssertionError("expected abort")
        check([23,24])
        db.close()
        print(backend,"observations", failures, flush=True)
assert not failures, failures
