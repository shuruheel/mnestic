"""mnestic quickstart: a persistent agent-memory store, end to end.

    pip install mnestic
    python quickstart.py [store_dir]

Walks the whole lifecycle a long-lived memory needs, and asserts every step so the
script doubles as a clean-environment release check (scripts/check-quickstart.sh):

  1. open a persistent store            5. update and delete, handling errors
  2. declare a schema and its indexes   6. back up
  3. write memories (one transaction)   7. close, reopen, verify
  4. retrieve: keyed lookup, hybrid      8. restore the backup elsewhere, verify
     search with per-leg attribution,
     filtered exact vector search

The embedder is a deterministic toy (hashed bag of words) so the script runs offline
with no model download. Swap `embed` for a real model; nothing else changes.
"""

from __future__ import annotations

import hashlib
import math
import sys
import tempfile
from pathlib import Path

from mnestic import CozoDbPy

DIM = 64


def embed(text: str) -> list[float]:
    """Toy embedding: hashed bag of words, L2-normalized. Replace with a real model."""
    v = [0.0] * DIM
    for word in text.lower().split():
        h = int.from_bytes(hashlib.sha256(word.encode()).digest()[:4], "little")
        v[h % DIM] += 1.0 if (h >> 31) & 1 else -1.0
    n = math.sqrt(sum(x * x for x in v)) or 1.0
    return [x / n for x in v]


def error_code(exc: Exception) -> str | None:
    """mnestic raises `Exception` whose first argument is a structured diagnostic dict:
    {code, message, causes, display, severity, ...}. Branch on `code`, show `display`."""
    diag = exc.args[0] if exc.args else None
    return diag.get("code") if isinstance(diag, dict) else None


def run(store: Path) -> None:
    # 1. Open a persistent store. "rocksdb" takes a directory; "sqlite" takes a file.
    #    RocksDB acknowledges a commit once its WAL is written; durable writes add an
    #    fsync per commit so an OS crash or power loss cannot lose acknowledged writes.
    db = CozoDbPy("rocksdb", str(store / "memory.db"), "{}")
    db.set_durable_writes(True)

    # 2. Schema. Keys go before `=>`. Create search indexes before loading data:
    #    every later write maintains them in the same transaction as the row.
    db.run_script(
        f":create memory {{id: String => text: String, kind: String, emb: <F32; {DIM}>}}",
        {}, False,
    )
    db.run_script(
        f"::hnsw create memory:vec {{dim: {DIM}, m: 16, dtype: F32, fields: [emb], "
        "distance: Cosine, ef_construction: 64}", {}, False,
    )
    db.run_script(
        "::fts create memory:fts {extractor: text, tokenizer: Simple, "
        "filters: [Lowercase, Stemmer('english')]}", {}, False,
    )

    # 3. Write. Always pass values as parameters ($rows), never by string formatting.
    #    One script is one transaction: all rows commit, or none do.
    memories = [
        ("m1", "The user prefers dark mode in every editor", "preference"),
        ("m2", "Deploys run from the main branch after CI passes", "procedure"),
        ("m3", "The staging database was retired in October", "fact"),
        ("m4", "The user is allergic to peanuts", "fact"),
        ("m5", "Prefer small pull requests that are easy to review", "preference"),
    ]
    rows = [[i, t, k, embed(t)] for i, t, k in memories]
    db.run_script(
        "?[id, text, kind, emb] <- $rows :put memory {id => text, kind, emb}",
        {"rows": rows}, False,
    )

    # 4a. Keyed lookup. Bind the key inside the braces (`id: $id`) so the engine seeks
    #     the primary key instead of scanning the relation.
    got = db.run_script("?[text] := *memory{id: $id, text}", {"id": "m4"}, True)
    assert got["rows"] == [["The user is allergic to peanuts"]], got

    # 4b. Hybrid search: vector + full text fused with reciprocal rank fusion in one
    #     call. `detailed` returns one row per (memory, leg) so you can see why it ranked.
    q = "what editor theme does the user like"
    hits = db.hybrid_search({
        "relation": "memory", "id_col": "id",
        "vector_index": "vec", "query_vector": embed(q), "vector_k": 5, "ef": 64,
        "fts_index": "fts", "query_text": "editor OR theme OR user", "fts_k": 5,
        "limit": 3, "detailed": True,
    })
    ranked = list(dict.fromkeys(r[0] for r in hits["rows"]))
    assert ranked[0] == "m1", hits
    print("hybrid top-3:", ranked[:3], "| legs for m1:",
          sorted({r[2] for r in hits["rows"] if r[0] == "m1"}))

    # 4c. Filtered vector search. When the filter is selective, score the eligible rows
    #     exactly: an ANN index searched with a filter can miss eligible neighbours that
    #     sit behind ineligible ones.
    pref = db.run_script(
        "?[id, d] := *memory{id, kind: $kind, emb}, d = cos_dist(emb, vec($q)) "
        ":order d :limit 2",
        {"kind": "preference", "q": embed("code review habits")}, True,
    )
    assert {r[0] for r in pref["rows"]} == {"m1", "m5"}, pref

    # 5. Update and delete. A write can be rejected with a structured error; it rolls
    #    back completely, so the previous row stays readable. Branch on the code.
    new_text = "The user switched to light mode in the terminal"
    try:
        db.run_script(
            "?[id, text, kind, emb] <- [[$id, $text, $kind, $emb]] "
            ":put memory {id => text, kind, emb}",
            {"id": "m1", "text": new_text, "kind": "preference", "emb": embed(new_text)},
            False,
        )
    except Exception as exc:  # noqa: BLE001
        if error_code(exc) == "hnsw::repair_limit":
            # The vector index could not repair its neighbourhood within its work
            # bounds. The update was not applied. Retrying the same write will not help;
            # see docs/specs/hnsw-mutation-repair.md (rebuild with ::reindex memory).
            print("update rejected by index repair bounds; previous value kept")
        else:
            raise
    db.run_script("?[id] <- [[$id]] :rm memory {id}", {"id": "m3"}, False)

    # Errors are data: a bad query raises with a code you can test for.
    try:
        db.run_script("?[x] := *no_such_relation{x}", {}, True)
        raise AssertionError("expected an error")
    except Exception as exc:  # noqa: BLE001
        assert error_code(exc), exc
        print("structured error code:", error_code(exc))

    # 6. Back up to a single file while the store is open.
    backup = store / "memory.backup"
    db.backup(str(backup))
    before = db.run_script("?[id, text] := *memory{id, text} :order id", {}, True)["rows"]
    db.close()

    # 7. Reopen in a new handle (as a restarted process would) and verify.
    db = CozoDbPy("rocksdb", str(store / "memory.db"), "{}")
    after = db.run_script("?[id, text] := *memory{id, text} :order id", {}, True)["rows"]
    assert after == before and "m3" not in {r[0] for r in after}, after
    again = db.hybrid_search({
        "relation": "memory", "id_col": "id",
        "vector_index": "vec", "query_vector": embed("peanut allergy"), "vector_k": 3,
        "fts_index": "fts", "query_text": "peanuts OR allergic", "fts_k": 3, "limit": 1,
    })
    assert again["rows"][0][0] == "m4", again
    db.close()

    # 8. Restore the backup into a fresh store and verify. Indexes come back with it.
    restored = CozoDbPy("rocksdb", str(store / "restored.db"), "{}")
    restored.restore(str(backup))
    assert restored.run_script(
        "?[id, text] := *memory{id, text} :order id", {}, True)["rows"] == before
    hit = restored.run_script(
        "?[id] := ~memory:fts{id | query: 'allergic', k: 1}", {}, True)["rows"]
    assert hit == [["m4"]], hit
    restored.close()
    print(f"ok: {len(before)} memories survived update, delete, reopen and restore")


if __name__ == "__main__":
    if len(sys.argv) > 1:
        target = Path(sys.argv[1])
        target.mkdir(parents=True, exist_ok=True)
        run(target)
    else:
        with tempfile.TemporaryDirectory() as d:
            run(Path(d))
