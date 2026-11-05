"""Run with released 0.18 before warning removal, then compare with the candidate.

Exercises the parameterized type-list/write shapes used by MindGraph's
string_list_param callers. Legacy warning fixtures are deliberately separate.
Usage: python test_string_migration_019.py baseline|candidate OUTPUT [BASELINE]
"""
import hashlib
import importlib.metadata
import json
import sys
from pathlib import Path

from mnestic import CozoDbPy

CODE = "parser.string_decoding_changed"
VALUES = ['quote" and apostrophe\'', r"C:\Users\name", "path/to/name",
          "  edge\t\n", "é😀", "/* comment */", r"\uD800"]


def main(mode, output, baseline=None):
    assert mode in ("baseline", "candidate")
    if mode == "baseline":
        assert importlib.metadata.version("mnestic") == "0.18.0"
    db = CozoDbPy("mem", "", "{}")
    def run(script, params=None):
        return db.run_script(script, params or {}, False)["rows"]
    def warnings():
        rows = run("::warnings")
        run("::warnings clear")
        return [row[1] for row in rows]

    run(":create node {uid: Int => node_type: String}")
    records = []
    for i, value in enumerate(VALUES):
        run("?[uid,node_type] <- [[$uid,$type]] :put node {uid => node_type}",
            {"uid": i, "type": value})
        rows = run("?[uid,node_type] := *node{uid,node_type}, is_in(node_type,$types)",
                   {"types": [value]})
        assert rows == [[i, value]]
        codes = warnings()
        assert CODE not in codes, (value, codes)
        records.append({"case": "corrected_builder", "value": value, "rows": rows, "warnings": codes})
    for literal, expected in [(r'"a\nb"', "a\nb"), ('_"  raw  "_', "  raw  ")]:
        rows = run("?[x] <- [[" + literal + "]]")
        assert rows == [[expected]]
        codes = warnings()
        assert codes.count(CODE) == (1 if mode == "baseline" else 0)
        records.append({"case": "legacy_warning", "literal": literal, "rows": rows, "warnings": codes})
    for literal, code in [(r'"\uD800"', "parser::invalid_utf8_code"),
                          (r'"\q"', "parser::pest"), ('__ "gap" __', "parser::pest")]:
        try:
            run("?[x] <- [[" + literal + "]]")
        except Exception as error:
            assert code in str(error), str(error)
        else:
            raise AssertionError(literal)
        records.append({"case": "error", "literal": literal, "error_code": code, "warnings": warnings()})
    if baseline:
        old = json.loads(Path(baseline).read_text())["records"]
        without_warnings = lambda rows: [{k: v for k, v in row.items() if k != "warnings"} for row in rows]
        assert without_warnings(old) == without_warnings(records)
    Path(output).write_text(json.dumps({"mode": mode, "version": importlib.metadata.version("mnestic"),
        "fixture_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), "records": records}, indent=2) + "\n")
    db.close()
    print(mode, len(records), "fixtures PASS")


if __name__ == "__main__":
    main(*sys.argv[1:])
