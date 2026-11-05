"""Rollback must restore FTS ranking as well as stored rows.

Fixture adapted from GrumpyChubbyCat's MPL-2.0 Mnestic PR #69,
cf0dc189f4d7d75eec7d2d955933f690882c62a1. Run against separate baseline/candidate
installations; released 0.18.0 is expected to fail the score assertion.
"""
# SPDX-License-Identifier: MPL-2.0

import os
import tempfile
import unittest

from mnestic import CozoDbPy

QUERY = "?[id,score] := ~doc:idx{id | query:'alpha',k:20,bind_score:score} :order id"
TOP = "?[id,score] := ~doc:idx{id | query:'alpha',k:1,bind_score:score}"


class FtsTransactionStats(unittest.TestCase):
    def test_failed_script_preserves_rows_scores_and_top_result(self):
        for backend in os.environ.get("MNESTIC_TEST_BACKENDS", "mem,sqlite,rocksdb").split(","):
            with self.subTest(backend=backend), tempfile.TemporaryDirectory() as directory:
                db = CozoDbPy(backend, directory + "/db", "{}")
                try:
                    def run(script):
                        return db.run_script(script, {}, False)["rows"]

                    run(":create doc {id:String => body:String}")
                    run("::fts create doc:idx {extractor:body,tokenizer:Simple,filters:[Lowercase]}")
                    db.run_script(
                        "?[id,body] <- $rows :put doc {id=>body}",
                        {"rows": [["a", "alpha alpha " + " ".join(["pad"] * 8)],
                                  ["b", "alpha pad pad"], ["c", " ".join(["pad"] * 100)]]},
                        False,
                    )
                    before, top = run(QUERY), run(TOP)
                    self.assertEqual(top[0][0], "a")
                    with self.assertRaisesRegex(Exception, "query::relation_not_found"):
                        run("{?[id]<-[['c']] :rm doc {id}} {?[v] := *missing_relation{v}}")
                    self.assertEqual(run("?[id] := *doc{id} :order id"), [["a"], ["b"], ["c"]])
                    self.assertEqual(run(QUERY), before)
                    self.assertEqual(run(TOP), top)
                finally:
                    db.close()


if __name__ == "__main__":
    unittest.main()
