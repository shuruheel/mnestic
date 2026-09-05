"""Verify actual wheel/sdist imports and atomic failure with an independent producer."""
import tempfile
from pathlib import Path

import pyarrow as pa
import pyarrow.ipc as ipc
import pyarrow.parquet as pq
from mnestic import CozoDbPy

with tempfile.TemporaryDirectory() as tmp:
    root = Path(tmp)
    table = pa.table({"id": [1, 2], "text": ['quote" and \\ 😀', "  edge  "]})
    for fmt in ("parquet", "arrow_ipc_file", "arrow_ipc_stream"):
        path = root / fmt
        if fmt == "parquet":
            pq.write_table(table, path)
        else:
            writer = ipc.new_file if fmt == "arrow_ipc_file" else ipc.new_stream
            with pa.OSFile(str(path), "wb") as sink:
                with writer(sink, table.schema) as stream:
                    stream.write_table(table)
        db = CozoDbPy("sqlite", str(root / (fmt + ".db")), "{}")
        db.run_script(":create rows {id: Int => text: String}", {}, False)
        report = db.import_columnar_file("rows", str(path), format=fmt, batch_rows=1)
        assert report["rows_processed"] == 2
        expected = [[1, 'quote" and \\ 😀'], [2, "  edge  "]]
        assert db.run_script("?[id,text] := *rows{id,text} :order id", {}, True)["rows"] == expected
        db.run_script(":create empty {id: Int => text: String}", {}, False)
        try:
            db.import_columnar_file("empty", str(path), format=fmt, batch_rows=1, max_rows=1)
        except Exception:
            pass
        else:
            raise AssertionError("row limit must fail")
        assert db.run_script("?[id] := *empty{id}", {}, True)["rows"] == []
        db.close()
        print(fmt, "PASS (values and atomic rollback)")
