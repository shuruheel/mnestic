"""Governed transactions from Python (spec docs/specs/python-governed-transactions.md).

Runs on every backend in MNESTIC_TEST_BACKENDS (default "mem,sqlite,rocksdb";
a backend the installed wheel lacks is skipped). Tests that need the commit
failure seam skip unless the wheel was built with the `test-hooks` feature.
"""
# SPDX-License-Identifier: MPL-2.0

import gc
import os
import shutil
import tempfile
import threading
import time
import unittest
import warnings

from mnestic import CozoDbPy, GovernedTx, MnesticError

BACKENDS = os.environ.get("MNESTIC_TEST_BACKENDS", "mem,sqlite,rocksdb").split(",")
HAS_HOOK = hasattr(CozoDbPy, "_fail_next_commit_for_tests")

PUT = "?[k, v] <- [[$k, $v]] :put kv {k => v}"
ROWS = "?[k, v] := *kv{k, v} :order k"


def available(backend):
    directory = tempfile.mkdtemp()
    try:
        CozoDbPy(backend, directory + "/probe", "{}").close()
        return True
    except Exception:
        return False
    finally:
        shutil.rmtree(directory, ignore_errors=True)


AVAILABLE = [b for b in BACKENDS if available(b)]


class Base(unittest.TestCase):
    backends = AVAILABLE

    def open(self, backend, path=None):
        directory = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, directory, True)
        db = CozoDbPy(backend, path or directory + "/db", "{}")
        self.addCleanup(db.close)
        db.run_script(":create kv {k: Int => v: Int}", {}, False)
        return db

    def rows(self, db):
        return db.run_script(ROWS, {}, True)["rows"]

    def rows_once_ended(self, db):
        """Rows, waiting out D9 refusals while an ended transaction's worker
        thread exits (it may hold the SQLite store lock until then)."""
        deadline = time.monotonic() + 5
        while True:
            try:
                return self.rows(db)
            except MnesticError as e:
                if e.code != "transaction::governed_write_open" or time.monotonic() > deadline:
                    raise
                time.sleep(0.01)

    def each(self, backends=None):
        for backend in backends or self.backends:
            with self.subTest(backend=backend):
                yield backend, self.open(backend)

    def assertCode(self, cm, code):
        self.assertIsInstance(cm.exception, MnesticError)
        self.assertEqual(cm.exception.code, code)


class Lifecycle(Base):
    def test_commit_persists_abort_does_not(self):
        for _, db in self.each():
            tx = db.governed_transact(True, timeout=10.0)
            tx.run_script(PUT, {"k": 1, "v": 10})
            self.assertEqual(tx.run_script(ROWS)["rows"], [[1, 10]])
            tx.commit()
            tx = db.governed_transact(True, timeout=10.0)
            tx.run_script(PUT, {"k": 2, "v": 20})
            tx.abort()
            self.assertEqual(self.rows(db), [[1, 10]])
            with self.assertRaises(MnesticError) as cm:
                tx.run_script(ROWS)
            self.assertCode(cm, "transaction::closed")
            tx.abort()  # no-op on an ended transaction

    def test_drop_rolls_back_and_warns(self):
        for _, db in self.each():
            tx = db.governed_transact(True, timeout=10.0)
            tx.run_script(PUT, {"k": 3, "v": 30})
            with warnings.catch_warnings(record=True) as caught:
                warnings.simplefilter("always")
                del tx
                gc.collect()
            self.assertTrue(any(issubclass(w.category, ResourceWarning) for w in caught))
            self.assertEqual(self.rows_once_ended(db), [])

    def test_failing_statement_rolls_back(self):
        for _, db in self.each():
            tx = db.governed_transact(True, timeout=10.0)
            tx.run_script(PUT, {"k": 1, "v": 10})
            with self.assertRaises(MnesticError) as first:
                tx.run_script("?[x] := *missing{x}")
            self.assertFalse(first.exception.maybe_committed)
            self.assertFalse(tx.is_open)
            with self.assertRaises(MnesticError) as cm:
                tx.run_script(ROWS)
            self.assertCode(cm, "transaction::closed")
            with self.assertRaises(MnesticError) as again:
                tx.commit()
            self.assertIs(again.exception, first.exception)
            tx.abort()
            self.assertEqual(self.rows(db), [])

    def test_double_commit_raises_closed(self):
        for _, db in self.each():
            tx = db.governed_transact(True, timeout=10.0)
            tx.commit()
            with self.assertRaises(MnesticError) as cm:
                tx.commit()
            self.assertCode(cm, "transaction::closed")
            tx.abort()

    def test_bad_params_leave_transaction_open(self):
        for _, db in self.each():
            tx = db.governed_transact(True, timeout=10.0)
            with self.assertRaises(Exception) as cm:
                tx.run_script(PUT, {"k": object(), "v": 1})
            self.assertNotIsInstance(cm.exception, MnesticError)
            tx.run_script(PUT, {"k": 1, "v": 1})
            tx.commit()
            self.assertEqual(self.rows(db), [[1, 1]])

    def test_read_only_rejects_write(self):
        for _, db in self.each():
            tx = db.governed_transact(False, timeout=10.0)
            with self.assertRaises(MnesticError):
                tx.run_script(PUT, {"k": 1, "v": 1})
            self.assertEqual(self.rows(db), [])


class Limits(Base):
    def test_deadline_mid_statement(self):
        for _, db in self.each():
            tx = db.governed_transact(True, timeout=0.3)
            started = time.monotonic()
            with self.assertRaises(MnesticError) as cm:
                tx.run_script("?[x] <- [[1]] :sleep 5")
            self.assertLess(time.monotonic() - started, 3)
            self.assertCode(cm, "eval::timeout")
            self.assertEqual(cm.exception.kind, "timeout")
            self.assertFalse(cm.exception.maybe_committed)

    def test_deadline_between_statements(self):
        for _, db in self.each():
            tx = db.governed_transact(True, timeout=0.3, idle_timeout=10.0)
            tx.run_script(PUT, {"k": 1, "v": 1})
            time.sleep(0.5)
            with self.assertRaises(MnesticError) as cm:
                tx.run_script(ROWS)
            self.assertCode(cm, "eval::timeout")
            self.assertEqual(self.rows(db), [])

    def test_idle_expiry(self):
        for _, db in self.each():
            tx = db.governed_transact(True, timeout=10.0, idle_timeout=0.2)
            tx.run_script(PUT, {"k": 1, "v": 1})
            time.sleep(0.5)
            with self.assertRaises(MnesticError) as cm:
                tx.commit()
            self.assertCode(cm, "eval::timeout")
            self.assertEqual(cm.exception.kind, "timeout")
            # The worker had already exited, so the commit cannot be ambiguous.
            self.assertFalse(cm.exception.maybe_committed)
            self.assertEqual(self.rows(db), [])

    def test_mem_limit(self):
        query = "tmp[a,b,c] := *base[a], *base[b], *base[c]\n?[count(a)] := tmp[a,_,_]"
        for _, db in self.each():
            db.run_script(":create base {x: Int}", {}, False)
            db.import_relations({"base": {"headers": ["x"], "rows": [[i] for i in range(200)]}})
            tx = db.governed_transact(False, timeout=30.0, mem_limit=100_000)
            with self.assertRaises(MnesticError) as cm:
                tx.run_script(query)
            self.assertCode(cm, "eval::mem_budget_exceeded")
            self.assertEqual(cm.exception.kind, "limit")
            # `:mem_limit` in the script only tightens a generous transaction limit.
            tx = db.governed_transact(False, timeout=30.0, mem_limit=10**12)
            with self.assertRaises(MnesticError) as cm:
                tx.run_script(query + " :mem_limit 100000")
            self.assertCode(cm, "eval::mem_budget_exceeded")

    def test_invalid_arguments_raise_value_error(self):
        db = self.open(self.backends[0])
        for kwargs in [
            {"timeout": 0}, {"timeout": -1.0}, {"timeout": float("nan")},
            {"timeout": float("inf")}, {"timeout": 1e20}, {"timeout": 1e-12},
            {"timeout": 5.0, "idle_timeout": 0}, {"timeout": 5.0, "idle_timeout": 1e-12},
            {"timeout": 5.0, "idle_timeout": 1e20}, {"timeout": 5.0, "idle_timeout": float("nan")},
            {"timeout": 5.0, "mem_limit": 0}, {"timeout": 5.0, "mem_limit": -5},
            {"timeout": 5.0, "mem_limit": 1e20}, {"timeout": 5.0, "mem_limit": 1e-12},
            {"timeout": 5.0, "mem_limit": 10**40}, {"timeout": 5.0, "mem_limit": True},
        ]:
            with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                db.governed_transact(True, **kwargs)
        with self.assertRaises(TypeError):
            db.governed_transact(True)  # timeout is required
        # An idle limit at or above the deadline is accepted and has no effect.
        db.governed_transact(True, timeout=1.0, idle_timeout=100.0).abort()


class ContextManager(Base):
    def test_clean_exit_commits(self):
        for _, db in self.each():
            with db.governed_transact(True, timeout=10.0) as tx:
                self.assertIsInstance(tx, GovernedTx)
                tx.run_script(PUT, {"k": 1, "v": 1})
            self.assertEqual(self.rows(db), [[1, 1]])

    def test_exception_aborts_and_propagates(self):
        for _, db in self.each():
            with self.assertRaises(KeyError):
                with db.governed_transact(True, timeout=10.0) as tx:
                    tx.run_script(PUT, {"k": 1, "v": 1})
                    raise KeyError("app")
            self.assertEqual(self.rows(db), [])

    def test_own_failure_propagates_unchanged(self):
        for _, db in self.each():
            with self.assertRaises(MnesticError) as cm:
                with db.governed_transact(True, timeout=10.0) as tx:
                    tx.run_script("?[x] := *missing{x}")
            self.assertNotEqual(cm.exception.code, "transaction::closed")

    def test_clean_exit_after_caught_error_raises_it(self):
        for _, db in self.each():
            with self.assertRaises(MnesticError) as cm:
                with db.governed_transact(True, timeout=10.0) as tx:
                    tx.run_script(PUT, {"k": 1, "v": 1})
                    try:
                        tx.run_script("?[x] := *missing{x}")
                    except MnesticError as e:
                        caught = e
            self.assertIs(cm.exception, caught)
            self.assertEqual(self.rows(db), [])

    def test_explicit_end_inside_block(self):
        for _, db in self.each():
            with db.governed_transact(True, timeout=10.0) as tx:
                tx.run_script(PUT, {"k": 1, "v": 1})
                tx.commit()
            with db.governed_transact(True, timeout=10.0) as tx:
                tx.run_script(PUT, {"k": 2, "v": 2})
                tx.abort()
            self.assertEqual(self.rows(db), [[1, 1]])


class Threads(Base):
    def test_cancel_from_another_thread(self):
        for _, db in self.each():
            tx = db.governed_transact(True, timeout=30.0)
            threading.Timer(0.2, tx.cancel).start()
            started = time.monotonic()
            with self.assertRaises(MnesticError) as cm:
                tx.run_script("?[x] <- [[1]] :sleep 20")
            self.assertLess(time.monotonic() - started, 5)
            self.assertCode(cm, "eval::killed")
            tx.cancel()  # idempotent on an ended transaction

    def test_cancel_long_evaluation(self):
        query = "r[n] := n = 0\nr[m] := r[n], m = n + 1, m < 100000000\n?[count(n)] := r[n]"
        for _, db in self.each(["mem"] if "mem" in self.backends else None):
            tx = db.governed_transact(False, timeout=60.0)
            threading.Timer(0.3, tx.cancel).start()
            started = time.monotonic()
            with self.assertRaises(MnesticError) as cm:
                tx.run_script(query)
            self.assertLess(time.monotonic() - started, 10)
            self.assertCode(cm, "eval::killed")

    def _gil_released(self, db, write):
        tx = db.governed_transact(write, timeout=30.0)
        done = threading.Event()

        def blocked():
            tx.run_script("?[x] <- [[1]] :sleep 1.5")
            done.set()

        thread = threading.Thread(target=blocked)
        thread.start()
        time.sleep(0.2)
        started = time.monotonic()
        db.run_script("?[x] <- [[1]]", {}, True)
        self.assertLess(time.monotonic() - started, 1.0)
        self.assertFalse(done.is_set())
        thread.join()
        tx.commit()

    def test_gil_released_read(self):
        for _, db in self.each():
            self._gil_released(db, False)

    def test_gil_released_write_rocksdb(self):
        if "rocksdb" not in self.backends:
            self.skipTest("rocksdb not in this wheel")
        for _, db in self.each(["rocksdb"]):
            self._gil_released(db, True)

    def test_two_threads_share_one_transaction(self):
        for _, db in self.each():
            tx = db.governed_transact(True, timeout=30.0)
            errors = []

            def work(base):
                try:
                    for i in range(20):
                        tx.run_script(PUT, {"k": base + i, "v": i})
                except Exception as e:  # pragma: no cover - reported below
                    errors.append(e)

            threads = [threading.Thread(target=work, args=(b,)) for b in (0, 100)]
            for t in threads:
                t.start()
            for t in threads:
                t.join(20)
                self.assertFalse(t.is_alive(), "deadlock")
            self.assertEqual(errors, [])
            tx.commit()
            self.assertEqual(len(self.rows(db)), 40)


class SqliteRefusal(Base):
    """D9: a governed write excludes other calls on the same SQLite handle."""

    def test_refused_while_governed_write_open(self):
        if "sqlite" not in self.backends:
            self.skipTest("sqlite not in this wheel")
        directory = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, directory, True)
        db = self.open("sqlite")
        with db.governed_transact(True, timeout=10.0) as tx:
            tx.run_script(PUT, {"k": 1, "v": 1})
            calls = {
                "run_script": lambda: db.run_script(ROWS, {}, True),
                "multi_transact": lambda: db.multi_transact(False),
                "governed_transact": lambda: db.governed_transact(False, timeout=1.0),
                "backup": lambda: db.backup(directory + "/backup.db"),
                "export_relations": lambda: db.export_relations(["kv"]),
                "import_relations": lambda: db.import_relations(
                    {"kv": {"headers": ["k", "v"], "rows": [[9, 9]]}}),
            }
            for name, call in calls.items():
                with self.subTest(call=name):
                    started = time.monotonic()
                    with self.assertRaises(MnesticError) as cm:
                        call()
                    self.assertLess(time.monotonic() - started, 0.5)
                    self.assertCode(cm, "transaction::governed_write_open")
                    self.assertEqual(cm.exception.kind, "conflict")
                    self.assertFalse(cm.exception.retryable)
                    self.assertFalse(cm.exception.maybe_committed)
            # The transaction itself keeps working.
            tx.run_script(PUT, {"k": 2, "v": 2})
        self.assertEqual(self.rows(db), [[1, 1], [2, 2]])
        db.multi_transact(False).abort()
        db.backup(directory + "/backup2.db")

    def test_refused_until_failed_transaction_ends(self):
        if "sqlite" not in self.backends:
            self.skipTest("sqlite not in this wheel")
        db = self.open("sqlite")
        tx = db.governed_transact(True, timeout=10.0)
        with self.assertRaises(MnesticError):
            tx.run_script("?[x] := *missing{x}")
        self.assertEqual(self.rows(db), [])

    def test_not_refused_for_read_or_rocksdb(self):
        cases = [("sqlite", False)] if "sqlite" in self.backends else []
        cases += [("rocksdb", True)] if "rocksdb" in self.backends else []
        for backend, write in cases:
            with self.subTest(backend=backend, write=write):
                db = self.open(backend)
                with db.governed_transact(write, timeout=10.0) as tx:
                    tx.run_script(ROWS)
                    self.assertEqual(self.rows(db), [])
                    db.export_relations(["kv"])

    def test_held_legacy_write_times_out_governed(self):
        if "sqlite" not in self.backends:
            self.skipTest("sqlite not in this wheel")
        db = self.open("sqlite")
        legacy = db.multi_transact(True)
        legacy.run_script(PUT, {"k": 1, "v": 1})
        tx = db.governed_transact(True, timeout=0.5)
        started = time.monotonic()
        with self.assertRaises(MnesticError) as cm:
            tx.run_script(ROWS)
        self.assertLess(time.monotonic() - started, 3)
        self.assertCode(cm, "eval::timeout")
        legacy.commit()
        # The parked governed worker takes the store lock as the legacy commit
        # releases it, sees its expired deadline and exits.
        self.assertEqual(self.rows_once_ended(db), [[1, 1]])


class Close(Base):
    def test_close_cancels_governed_transactions(self):
        for backend in self.backends:
            if backend == "mem":
                continue
            with self.subTest(backend=backend):
                directory = tempfile.mkdtemp()
                self.addCleanup(shutil.rmtree, directory, True)
                path = directory + "/db"
                db = self.open(backend, path)
                tx = db.governed_transact(True, timeout=30.0, idle_timeout=30.0)
                tx.run_script(PUT, {"k": 1, "v": 1})
                started = time.monotonic()
                db.close()
                self.assertLess(time.monotonic() - started, 0.5)
                with self.assertRaises(MnesticError):
                    tx.run_script(ROWS)
                deadline = time.monotonic() + 5
                while True:
                    try:
                        reopened = CozoDbPy(backend, path, "{}")
                        break
                    except Exception:
                        if time.monotonic() > deadline:
                            raise
                        time.sleep(0.05)
                try:
                    self.assertEqual(self.rows(reopened), [])
                finally:
                    reopened.close()

    def test_close_leaves_multi_transact_alone(self):
        for _, db in self.each():
            legacy = db.multi_transact(True)
            legacy.run_script(PUT, {"k": 1, "v": 1}) if db else None
            db.close()
            legacy.run_script(PUT, {"k": 2, "v": 2})
            legacy.commit()


class MaybeCommitted(Base):
    def test_flags_on_ordinary_errors(self):
        for _, db in self.each():
            with self.assertRaises(MnesticError) as cm:
                db.run_script("?[x] := *missing{x}", {}, True)
            self.assertFalse(cm.exception.maybe_committed)
            self.assertFalse(MnesticError("x").maybe_committed)
            self.assertFalse(MnesticError("x").retryable)

    @unittest.skipUnless(HAS_HOOK, "wheel built without test-hooks")
    def test_commit_failure_is_maybe_committed(self):
        for _, db in self.each():
            tx = db.governed_transact(True, timeout=10.0)
            tx.run_script(PUT, {"k": 1, "v": 1})
            db._fail_next_commit_for_tests()
            with self.assertRaises(MnesticError) as cm:
                tx.commit()
            self.assertTrue(cm.exception.maybe_committed)
            self.assertFalse(cm.exception.retryable)


def retryable_error():
    e = MnesticError({"ok": False, "code": "test::conflict", "message": "injected"})
    e.code, e.kind, e.retryable, e.maybe_committed = "test::conflict", "conflict", True, False
    return e


class RunGoverned(Base):
    def test_success_commits_and_returns(self):
        for _, db in self.each():
            result = db.run_governed(lambda tx: tx.run_script(PUT, {"k": 1, "v": 1}) and "ok")
            self.assertEqual(result, "ok")
            self.assertEqual(self.rows(db), [[1, 1]])

    def test_retries_conflict_then_succeeds(self):
        for _, db in self.each():
            calls = []

            def fn(tx):
                calls.append(1)
                tx.run_script(PUT, {"k": len(calls), "v": 0})
                if len(calls) < 3:
                    raise retryable_error()
                return len(calls)

            self.assertEqual(db.run_governed(fn, max_attempts=5), 3)
            self.assertEqual(self.rows(db), [[3, 0]])

    def test_stops_at_max_attempts(self):
        for _, db in self.each():
            calls = []

            def fn(tx):
                calls.append(1)
                raise retryable_error()

            with self.assertRaises(MnesticError) as cm:
                db.run_governed(fn, max_attempts=3)
            self.assertEqual(len(calls), 3)
            self.assertEqual(cm.exception.code, "test::conflict")

    def test_stops_at_overall_timeout(self):
        for _, db in self.each():
            calls = []

            def fn(tx):
                calls.append(1)
                raise retryable_error()

            started = time.monotonic()
            with self.assertRaises(MnesticError):
                db.run_governed(fn, timeout=0.4, max_attempts=1000)
            self.assertLess(time.monotonic() - started, 2)
            self.assertGreater(len(calls), 1)
            self.assertLess(len(calls), 1000)

    def test_never_retries_maybe_committed(self):
        for _, db in self.each():
            calls = []

            def fn(tx):
                calls.append(1)
                e = retryable_error()
                e.maybe_committed = True
                raise e

            with self.assertRaises(MnesticError):
                db.run_governed(fn, max_attempts=5)
            self.assertEqual(len(calls), 1)

    @unittest.skipUnless(HAS_HOOK, "wheel built without test-hooks")
    def test_ambiguous_commit_not_retried(self):
        for _, db in self.each():
            calls = []

            def fn(tx):
                calls.append(1)
                tx.run_script(PUT, {"k": 1, "v": 1})
                db._fail_next_commit_for_tests()

            with self.assertRaises(MnesticError) as cm:
                db.run_governed(fn, max_attempts=5)
            self.assertTrue(cm.exception.maybe_committed)
            self.assertEqual(len(calls), 1)

    def test_application_error_not_retried(self):
        for _, db in self.each():
            calls = []

            def fn(tx):
                calls.append(1)
                tx.run_script(PUT, {"k": 1, "v": 1})
                raise ValueError("app")

            with self.assertRaises(ValueError):
                db.run_governed(fn)
            self.assertEqual(len(calls), 1)
            self.assertEqual(self.rows(db), [])


class LegacyUnchanged(Base):
    def test_multi_transact(self):
        for _, db in self.each():
            tx = db.multi_transact(True)
            tx.run_script(PUT, {"k": 1, "v": 1})
            tx.commit()
            self.assertEqual(self.rows(db), [[1, 1]])
            tx = db.multi_transact(True)
            tx.run_script(PUT, {"k": 2, "v": 2})
            tx.abort()
            self.assertEqual(self.rows(db), [[1, 1]])


if __name__ == "__main__":
    unittest.main()
