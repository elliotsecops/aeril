#!/usr/bin/env python3
"""Tests de seguridad y reproducibilidad del bench de 2 runs (S6-R1).

Cubre los findings A (ciclo de vida seguro del data dir) y B (limpieza
identity-aware de procesos). Solo stdlib (unittest); todas las rutas y
procesos usados son descartables y propiedad de este test.

Uso: python3 scripts/bench/test_two_run_safety.py
"""

import importlib.util
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("two_run", os.path.join(HERE, "two_run.py"))
two_run = importlib.util.module_from_spec(spec)
spec.loader.exec_module(two_run)


class SafeDataDirTest(unittest.TestCase):
    """Finding A: nunca borrar ni sobreescribir directorios ajenos."""

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="aeril-bench-safety-")

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_fresh_dir_ok(self):
        target = os.path.join(self.tmp, "fresh")
        p = two_run.safe_prepare_data_dir(target)
        self.assertEqual(p, os.path.realpath(target))
        self.assertTrue(os.path.isdir(target))
        self.assertTrue(os.path.exists(os.path.join(target, two_run.SENTINEL)))

    def test_owned_dir_repeat_ok(self):
        target = os.path.join(self.tmp, "owned")
        two_run.safe_prepare_data_dir(target)
        # Segunda corrida: sentinel presente ⇒ limpieza segura y re-creación.
        with open(os.path.join(target, "stale-artifact"), "w") as f:
            f.write("de la corrida anterior")
        p = two_run.safe_prepare_data_dir(target)
        self.assertTrue(os.path.isdir(p))
        self.assertTrue(os.path.exists(os.path.join(p, two_run.SENTINEL)))
        self.assertFalse(os.path.exists(os.path.join(p, "stale-artifact")))

    def test_canary_refusal_preserves_files(self):
        target = os.path.join(self.tmp, "canary")
        os.makedirs(target)
        canary = os.path.join(target, "precioso.txt")
        with open(canary, "w") as f:
            f.write("no tocar")
        with self.assertRaises(SystemExit) as cm:
            two_run.safe_prepare_data_dir(target)
        self.assertEqual(cm.exception.code, 2)
        self.assertTrue(os.path.exists(canary))
        with open(canary) as f:
            self.assertEqual(f.read(), "no tocar")
        self.assertFalse(os.path.exists(os.path.join(target, two_run.SENTINEL)))

    def test_root_refused(self):
        with self.assertRaises(SystemExit) as cm:
            two_run.safe_prepare_data_dir("/")
        self.assertEqual(cm.exception.code, 2)

    def test_home_refused(self):
        with self.assertRaises(SystemExit) as cm:
            two_run.safe_prepare_data_dir(os.path.expanduser("~"))
        self.assertEqual(cm.exception.code, 2)

    def test_symlink_refused(self):
        real = os.path.join(self.tmp, "real-dir")
        os.makedirs(real)
        with open(os.path.join(real, "dato.txt"), "w") as f:
            f.write("intacto")
        link = os.path.join(self.tmp, "link")
        os.symlink(real, link)
        with self.assertRaises(SystemExit) as cm:
            two_run.safe_prepare_data_dir(link)
        self.assertEqual(cm.exception.code, 2)
        self.assertTrue(os.path.exists(os.path.join(real, "dato.txt")))

    def test_ancestor_of_cwd_refused(self):
        # El repo (cwd) está dentro del target ⇒ rechazo sin tocar nada.
        ancestor = os.path.dirname(os.path.realpath(os.getcwd()))
        if ancestor == os.sep:
            self.skipTest("cwd en la raíz")
        with self.assertRaises(SystemExit) as cm:
            two_run.safe_prepare_data_dir(ancestor)
        self.assertEqual(cm.exception.code, 2)

    def test_system_dir_refused(self):
        with self.assertRaises(SystemExit) as cm:
            two_run.safe_prepare_data_dir("/etc/aeril-bench-should-not-exist")
        self.assertEqual(cm.exception.code, 2)


class IdentityTest(unittest.TestCase):
    """Finding B: señalizar SOLO procesos con identidad verificada."""

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="aeril-bench-ident-")

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _spawn_mockish(self):
        """Proceso descartable cuyo cmdline contiene __mock-agent (no es un
        mock real: es un `sleep` con un argumento testigo)."""
        proc = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(300)", "__mock-agent", "test"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if two_run.read_starttime(proc.pid) is not None:
                return proc
            time.sleep(0.02)
        self.fail("el proceso testigo no arrancó")

    def test_matching_identity_is_killed(self):
        proc = self._spawn_mockish()
        try:
            start = two_run.read_starttime(proc.pid)
            ident = two_run.capture_identity(proc.pid, expected_start=start)
            self.assertIsNotNone(ident)
            outcome = two_run.terminate_verified({"pid": proc.pid, "starttime": start})
            self.assertEqual(outcome["result"], "killed")
            proc.wait(timeout=5)
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()

    def test_wrong_starttime_never_signaled(self):
        proc = self._spawn_mockish()
        try:
            start = two_run.read_starttime(proc.pid)
            # Identidad con starttime incorrecto ⇒ no verificable ⇒ sin señal.
            outcome = two_run.terminate_verified({"pid": proc.pid, "starttime": start + 12345})
            self.assertEqual(outcome["result"], "uncertain")
            self.assertIsNone(proc.poll(), "el proceso NO debe ser señalado")
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()

    def test_non_mock_cmdline_never_signaled(self):
        proc = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(300)"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            start = two_run.read_starttime(proc.pid)
            self.assertIsNotNone(start)
            self.assertIsNone(
                two_run.capture_identity(proc.pid, expected_start=start),
                "un proceso sin __mock-agent no es identidad de backend",
            )
            outcome = two_run.terminate_verified({"pid": proc.pid, "starttime": start})
            self.assertEqual(outcome["result"], "uncertain")
            self.assertIsNone(proc.poll(), "el proceso ajeno NO debe ser señalado")
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()

    def test_dead_pid_not_signaled(self):
        proc = subprocess.Popen(
            [sys.executable, "-c", "pass"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        proc.wait(timeout=5)
        outcome = two_run.terminate_verified({"pid": proc.pid, "starttime": 123})
        self.assertEqual(outcome["result"], "already_dead")


class AssociationTest(unittest.TestCase):
    """Finding D#1: los backends candidatos se asocian SOLO a los runs de esta
    invocación (nunca a PIDs de runs ajenos persistidos en el store)."""

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="aeril-bench-assoc-")
        self.db = os.path.join(self.tmp, "store.db")
        con = sqlite3.connect(self.db)
        con.execute("CREATE TABLE events (kind TEXT, payload TEXT)")
        for run_id, pid in (("run-1", 1001), ("run-2", 1002), ("run-ajeno", 9999)):
            con.execute(
                "INSERT INTO events (kind, payload) VALUES (?, ?)",
                (
                    "backend.spawned",
                    json.dumps(
                        {"run_id": run_id, "pid": pid, "pid_start": 42, "program": "mock"}
                    ),
                ),
            )
        con.commit()
        con.close()

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_backend_payloads_only_own_runs(self):
        payloads = two_run.backend_payloads(self.db, {"run-1", "run-2"})
        pids = sorted(int(p["pid"]) for p in payloads)
        self.assertEqual(pids, [1001, 1002], "solo los runs de esta invocación")

    def test_empty_association_returns_nothing(self):
        self.assertEqual(two_run.backend_payloads(self.db, set()), [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
