"""Two-process upgrade acceptance: create with published 0.17, verify with 0.18."""
import json
import sys
from pathlib import Path

from mnestic import CozoDbPy

UID = "550e8400-e29b-41d4-a716-446655440000"


def run(db, script, params=None):
    return db.run_script(script, params or {}, False)["rows"]


def read(db):
    return run(db, "?[id,s,j] := *cells{id,s,j} :order id")


def check_old(db, expected):
    assert read(db) == expected
    assert run(db, "?[name] := ~people:idx{name | query:'DI*',k:10}") == [["Diwank"]]
    assert run(db, "::query run escaped") == [["a\nb"]]
    assert run(db, "::query run spaced") == [["  edge  "]]
    assert run(db, "::query run raw") == [["  raw  "]]
    assert read(db) == expected, "invocation must not rewrite stored values"


def main(mode, directory):
    root = Path(directory)
    root.mkdir(parents=True, exist_ok=True)
    for engine in ("sqlite", "rocksdb"):
        db = CozoDbPy(engine, str(root / (engine + ".db")), "{}")
        golden = root / (engine + ".json")
        if mode == "create":
            run(db, ":create cells {id: Int => s: String, j: Json}")
            run(db, r'''?[id,s,j] := id=1, s="a\nb", j={'u':to_uuid($u),'b':decode_base64('AQID'),'inf':1.0/0.0} :put cells {id => s,j}''', {"u": UID})
            run(db, r'''?[id,s,j] := id=2, s="  edge  ", j={'note':'edge'} :put cells {id => s,j}''')
            expected = read(db)
            assert expected[0][1] == r"a\nb"
            assert isinstance(expected[0][2]["u"], list)
            assert expected[0][2]["b"] == [1, 2, 3]
            assert expected[0][2]["inf"] is None
            golden.write_text(json.dumps(expected), encoding="utf-8")
            run(db, ":create people {name: String}")
            run(db, "?[name] <- [['Diwank']] :put people {name}")
            run(db, "::fts create people:idx {extractor:name,tokenizer:Simple,filters:[Lowercase]}")
            assert run(db, "?[name] := ~people:idx{name | query:'DI*',k:10}") == []
            for name, literal in [("escaped", r'"a\nb"'), ("spaced", '"  edge  "'), ("raw", '_"  raw  "_')]:
                run(db, "::query create " + name + " { ?[x] <- [[" + literal + "]] }")
            assert run(db, "::query run escaped") == [[r"a\nb"]]
            db.backup(str(root / (engine + "-old-backup.db")))
        elif mode == "verify":
            expected = json.loads(golden.read_text(encoding="utf-8"))
            check_old(db, expected)
            restored = CozoDbPy(engine, str(root / (engine + "-restored.db")), "{}")
            restored.restore(str(root / (engine + "-old-backup.db")))
            check_old(restored, expected)
            restored.close()
            run(db, r'''?[id,s,j] := id=3, s="a\nb", j={'u':to_uuid($u),'b':decode_base64('AQID'),'inf':1.0/0.0} :put cells {id => s,j}''', {"u": UID})
            rows = read(db)
            assert rows[:2] == expected
            assert rows[2] == [3, "a\nb", {"u": UID, "b": "AQID", "inf": "INFINITY"}]
            db.backup(str(root / (engine + "-new-backup.db")))
            restored = CozoDbPy(engine, str(root / (engine + "-new-restored.db")), "{}")
            restored.restore(str(root / (engine + "-new-backup.db")))
            assert read(restored) == rows
            restored.close()
        else:
            raise ValueError(mode)
        db.close()
        print(engine, mode, "PASS")


if __name__ == "__main__":
    main(*sys.argv[1:])
