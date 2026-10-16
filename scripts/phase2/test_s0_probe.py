#!/usr/bin/env python3
"""Tests de seguridad del harness P2-S0 (R1) — stdlib unittest.

Cubren: permisos fail-closed (nunca `allow*` automático), guard de modo
heredado, HOMEs sanitizados por perfil, env sanitizado sin variables de auth
del host y derivación de rutas. Sin credenciales, sin red, sin procesos reales
de Hermes.

Uso: python3 scripts/phase2/test_s0_probe.py
"""

import importlib.util
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("s0_probe", os.path.join(HERE, "s0_probe.py"))
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)


def make_client(policy):
    stdin = io.BytesIO()
    proc = probe.FakeProc(stdin)
    client = probe.AcpClient(proc, "/tmp/x", "/tmp/repo", permissions=policy)
    return client, stdin


def sent_messages(stdin):
    stdin.seek(0)
    return [json.loads(line) for line in stdin.getvalue().decode().splitlines() if line.strip()]


def permission_request(req_id, options, kind="edit"):
    return {
        "jsonrpc": "2.0",
        "id": req_id,
        "method": "session/request_permission",
        "params": {
            "toolCall": {"toolCallId": "tc-1", "title": "algo", "kind": kind},
            "options": options,
        },
    }


ALLOW_DENY = [
    {"optionId": "allow_once", "kind": "allow_once", "name": "Allow"},
    {"optionId": "deny", "kind": "reject_once", "name": "Deny"},
]
ONLY_ALLOW = [{"optionId": "allow_once", "kind": "allow_once", "name": "Allow"}]


class PermissionFailClosedTest(unittest.TestCase):
    def test_unsolicited_permission_denied_by_default(self):
        client, stdin = make_client("deny")
        client._handle_incoming(permission_request(7, ALLOW_DENY, kind="edit"))
        msgs = sent_messages(stdin)
        self.assertEqual(len(msgs), 1)
        outcome = msgs[0]["result"]["outcome"]
        self.assertEqual(outcome.get("optionId"), "deny")
        self.assertFalse(str(outcome.get("optionId", "")).startswith("allow"))
        self.assertEqual(client.permission_log, [{"tool_kind": "edit", "decision": "denied"}])

    def test_execute_kind_never_approved_even_with_approve_edits(self):
        client, stdin = make_client("approve-edits")
        client._handle_incoming(permission_request(8, ALLOW_DENY, kind="execute"))
        msgs = sent_messages(stdin)
        self.assertEqual(msgs[0]["result"]["outcome"].get("optionId"), "deny")
        self.assertEqual(client.permission_log[0]["decision"], "denied")

    def test_edit_kind_approved_only_with_explicit_policy(self):
        client, stdin = make_client("approve-edits")
        client._handle_incoming(permission_request(9, ALLOW_DENY, kind="edit"))
        msgs = sent_messages(stdin)
        self.assertEqual(msgs[0]["result"]["outcome"].get("optionId"), "allow_once")
        self.assertEqual(client.permission_log[0]["decision"], "approved-edit")

    def test_no_deny_option_cancels_not_approves(self):
        client, stdin = make_client("deny")
        client._handle_incoming(permission_request(10, ONLY_ALLOW, kind="edit"))
        msgs = sent_messages(stdin)
        outcome = msgs[0]["result"]["outcome"]
        self.assertEqual(outcome.get("outcome"), "cancelled")
        self.assertNotIn("optionId", outcome)
        self.assertEqual(client.permission_log[0]["decision"], "cancelled-no-deny-option")

    def test_unknown_kind_denied_with_approve_edits(self):
        client, stdin = make_client("approve-edits")
        client._handle_incoming(permission_request(11, ALLOW_DENY, kind="other"))
        self.assertEqual(sent_messages(stdin)[0]["result"]["outcome"].get("optionId"), "deny")


class InheritedModeGuardTest(unittest.TestCase):
    def test_default_mode_auth_ops_refused(self):
        with self.assertRaises(SystemExit) as cm:
            probe.inherited_auth_guard("default", True, False)
        self.assertEqual(cm.exception.code, 3)

    def test_default_mode_initialize_only_allowed(self):
        probe.inherited_auth_guard("default", False, False)  # no lanza

    def test_default_mode_with_explicit_override_allowed(self):
        probe.inherited_auth_guard("default", True, True)  # diagnóstico explícito

    def test_sanitized_mode_always_allowed(self):
        probe.inherited_auth_guard("sanitized", True, False)
        probe.inherited_auth_guard("nohome", True, False)


class EnvAndHomeTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="aeril-p2s0-test-")

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_san_home_per_profile_distinct(self):
        a = probe.san_home_for(os.path.join(self.tmp, "p-A"))
        b = probe.san_home_for(os.path.join(self.tmp, "p-B"))
        self.assertNotEqual(a, b)
        self.assertTrue(a.endswith("san-home-A"))
        self.assertTrue(b.endswith("san-home-B"))

    def test_setup_creates_independent_0700_homes(self):
        out = subprocess.run(
            [sys.executable, os.path.join(HERE, "s0_probe.py"), "setup", "--scratch", self.tmp],
            capture_output=True,
            text=True,
        )
        self.assertEqual(out.returncode, 0, out.stderr)
        manifest = json.loads(out.stdout)
        san_a = manifest["san_homes"]["p-A"]
        san_b = manifest["san_homes"]["p-B"]
        self.assertNotEqual(san_a, san_b)
        for d in (manifest["profiles"]["p-A"], manifest["profiles"]["p-B"], san_a, san_b):
            self.assertEqual(os.stat(d).st_mode & 0o777, 0o700, d)

    def test_sanitized_env_excludes_host_auth_vars(self):
        os.environ["GEMINI_API_KEY"] = "test-no-real"
        os.environ["ANTHROPIC_AUTH_TOKEN"] = "test-no-real"
        try:
            env = probe.build_env("sanitized", "/tmp/san", "/tmp/hermes")
            for leaked in ("GEMINI_API_KEY", "ANTHROPIC_AUTH_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"):
                self.assertNotIn(leaked, env)
            self.assertEqual(env["HOME"], "/tmp/san")
            self.assertEqual(env["HERMES_HOME"], "/tmp/hermes")
            for xdg in ("XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME"):
                self.assertTrue(env[xdg].startswith("/tmp/san"), xdg)
        finally:
            del os.environ["GEMINI_API_KEY"]
            del os.environ["ANTHROPIC_AUTH_TOKEN"]

    def test_env_allow_only_safe_vars(self):
        os.environ["DISPLAY"] = ":0"
        try:
            env = probe.build_env("sanitized", "/tmp/san", "/tmp/hermes", ("DISPLAY",))
            self.assertEqual(env.get("DISPLAY"), ":0")
        finally:
            del os.environ["DISPLAY"]
        with self.assertRaises(SystemExit) as cm:
            probe.build_env("sanitized", "/tmp/san", "/tmp/hermes", ("GEMINI_API_KEY",))
        self.assertEqual(cm.exception.code, 3)

    def test_cli_refuses_default_mode_auth_ops(self):
        out = subprocess.run(
            [
                sys.executable,
                os.path.join(HERE, "s0_probe.py"),
                "probe",
                "--home",
                os.path.join(self.tmp, "p-A"),
                "--repo",
                os.path.join(self.tmp, "repo-A"),
                "--env-mode",
                "default",
                "--session-new",
            ],
            capture_output=True,
            text=True,
        )
        self.assertEqual(out.returncode, 3, out.stderr)
        self.assertIn("RECHAZADO", out.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
