"""0.18 string/JSON/FTS acceptance for a freshly built or installed binding."""
import unittest

from mnestic import CozoDbPy


class StringJsonContract(unittest.TestCase):
    def setUp(self):
        self.db = CozoDbPy("mem", "", "{}")

    def rows(self, script, params=None):
        return self.db.run_script(script, params or {}, False)["rows"]

    def test_string_literals(self):
        self.assertEqual(self.rows(r'''?[x] <- [["a\"b"]]'''), [['a"b']])
        self.assertEqual(self.rows('''?[x] <- [[___"#298594"___]]'''), [["#298594"]])
        self.assertEqual(self.rows(r'''?[x] <- [[" a\nb "]]'''), [[" a\nb "]])
        text = ' "#\\😀 '
        self.assertEqual(self.rows("?[x] <- [[$x]]", {"x": text}), [[text]])

    def test_nested_uuid_and_native_bytes_boundary(self):
        uid = "550e8400-e29b-41d4-a716-446655440000"
        row = self.rows(
            "?[u,b,j,s] := u=to_uuid($u), b=decode_base64('AQID'), "
            "j={'u':u,'b':b}, s=to_string(u)", {"u": uid}
        )[0]
        self.assertEqual(row, [uid, b"\x01\x02\x03", {"u": uid, "b": "AQID"}, uid])
        self.assertIsInstance(row[2]["u"], str)

    def test_json_column_and_opaque_payload(self):
        self.rows(":create cells {id: Int => j: Json}")
        old = {"u": list(range(16)), "inf": None}
        self.rows("?[id,j] <- [[1,$j]] :put cells {id => j}", {"j": old})
        self.rows("?[id,j] := id=2, j=decode_base64('AQID') :put cells {id => j}")
        self.assertEqual(self.rows("?[id,j] := *cells{id,j} :order id"), [[1, old], [2, "AQID"]])

    def test_failed_parse_keeps_warning_with_its_database(self):
        with self.assertRaises(Exception):
            self.rows(r'''?[x] <- [["a\uD800"]]''')
        other = CozoDbPy("mem", "", "{}")
        self.assertEqual(other.run_script("::warnings", {}, False)["rows"], [])
        warnings = self.rows("::warnings")
        self.assertEqual(sum(row[1] == "parser.string_decoding_changed" for row in warnings), 1)

    def test_prefix_normalization_and_integer_boost(self):
        self.rows(":create docs {id: Int => text: String}")
        self.rows("::fts create docs:idx {extractor: text, tokenizer: Simple, "
                  "filters: [Lowercase, AsciiFolding]}")
        self.rows("?[id,text] <- [[1,'Diwank'],[2,'Éléphant']] :put docs {id => text}")
        for query, expected in [("DI*^3", [[1]]), ('"ÉL"*', [[2]])]:
            self.assertEqual(self.rows(
                "?[id] := ~docs:idx{id | query: $q, k: 10}", {"q": query}
            ), expected)


if __name__ == "__main__":
    unittest.main()
