"""Two-process released 0.18 -> candidate reopen/backup/index acceptance."""
import json
import sys
from pathlib import Path

from mnestic import CozoDbPy


def main(mode, directory):
    root = Path(directory)
    root.mkdir(parents=True, exist_ok=True)
    for backend in ("sqlite", "rocksdb"):
        db = CozoDbPy(backend, str(root / (backend + ".db")), "{}")
        def run(script, params=None, target=None):
            return (target or db).run_script(script, params or {}, False)["rows"]
        scripts = [
            "?[id,text,v] := *docs{id,text,v} :order id",
            "?[id,score] := ~docs:fts{id | query:'alpha',k:10,bind_score:score} :order -score,id",
            "?[id,d] := ~docs:vec{id | query:vec([1,0]),k:3,ef:50,bind_distance:d} :order d,id",
            "::query run escaped",
        ]
        def snapshot(target=None):
            return [run(script, target=target) for script in scripts]
        golden = root / (backend + ".json")
        if mode == "create":
            run(":create docs {id: Int => text: String, v: <F32; 2>}")
            run("?[id,text,v] <- $rows :put docs {id => text,v}",
                {"rows": [[1, 'alpha "é😀"', [1., 0.]], [2, "alpha alpha beta", [0., 1.]], [3, "  beta\\path  ", [-1., 0.]]]})
            run("::fts create docs:fts {extractor:text,tokenizer:Simple,filters:[Lowercase]}")
            run("::hnsw create docs:vec {dim:2,fields:[v],distance:L2,dtype:F32,m:8,ef_construction:50}")
            run(r'::query create escaped { ?[x] <- [["a\nb"]] }')
            golden.write_text(json.dumps(snapshot()))
            db.backup(str(root / (backend + "-backup.db")))
        elif mode == "verify":
            expected = json.loads(golden.read_text())
            assert snapshot() == expected
            restored = CozoDbPy(backend, str(root / (backend + "-restored.db")), "{}")
            restored.restore(str(root / (backend + "-backup.db")))
            assert snapshot(restored) == expected
            restored.close()
            try:
                run("{ ?[id] <- [[1]] :rm docs {id} } { ?[x] := *missing[x] }")
            except Exception:
                pass
            else:
                raise AssertionError("expected rollback")
            assert snapshot() == expected, "rollback changed rows or index ranking"
            run("?[id,text,v] <- [[4,'alpha gamma',[0.5,0.5]]] :put docs {id => text,v}")
            assert [4] in run("?[id] := ~docs:fts{id | query:'gamma',k:10}")
            run("::reindex docs")
            after = snapshot()
            db.close()
            db = CozoDbPy(backend, str(root / (backend + ".db")), "{}")
            assert snapshot() == after
        else:
            raise ValueError(mode)
        db.close()
        print(backend, mode, "PASS")


if __name__ == "__main__":
    main(*sys.argv[1:])
