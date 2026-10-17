#!/usr/bin/env python3
"""Tests de seguridad del harness P2-S0 (R1 + R2) — stdlib unittest.

Cubren: permisos fail-closed con ALCANCE VERIFICADO (nunca `allow*` automático;
edits confinados al repo descartable; ask-human explícito), allowlist de entorno
sin SSH_AUTH_SOCK, guard de modo heredado, HOMEs sanitizados por perfil y
derivación de rutas. Sin credenciales, sin red, sin procesos reales de Hermes.

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


def make_client(policy, repo="/tmp/x", ask_fn=None):
    stdin = io.BytesIO()
    proc = probe.FakeProc(stdin)
    client = probe.AcpClient(proc, "/tmp/home", repo, permissions=policy, ask_fn=ask_fn)
    return client, stdin


def sent_messages(stdin):
    stdin.seek(0)
    return [json.loads(line) for line in stdin.getvalue().decode().splitlines() if line.strip()]


def permission_request(req_id, options, kind="edit", diff_paths=None, locations=None):
    tool = {"toolCallId": "tc-1", "title": "algo", "kind": kind}
    if diff_paths is not None:
        tool["content"] = [
            {"type": "diff", "path": p, "newText": "contenido-no-persistido"} for p in diff_paths
        ]
    if locations is not None:
        tool["locations"] = [{"path": p} for p in locations]
    return {
        "jsonrpc": "2.0",
        "id": req_id,
        "method": "session/request_permission",
        "params": {"toolCall": tool, "options": options},
    }


ALLOW_DENY = [
    {"optionId": "allow_once", "kind": "allow_once", "name": "Allow"},
    {"optionId": "deny", "kind": "reject_once", "name": "Deny"},
]
ONLY_ALLOW = [{"optionId": "allow_once", "kind": "allow_once", "name": "Allow"}]


class PermissionFailClosedTest(unittest.TestCase):
    def test_unsolicited_permission_denied_by_default(self):
        client, stdin = make_client("deny")
        client._handle_incoming(permission_request(7, ALLOW_DENY, diff_paths=["NOTES.md"]))
        msgs = sent_messages(stdin)
        self.assertEqual(len(msgs), 1)
        outcome = msgs[0]["result"]["outcome"]
        self.assertEqual(outcome.get("optionId"), "deny")
        self.assertFalse(str(outcome.get("optionId", "")).startswith("allow"))
        self.assertEqual(client.permission_log[0]["decision"], "denied")

    def test_execute_kind_never_approved_even_with_verified_policy(self):
        client, stdin = make_client("approve-verified-edits")
        client._handle_incoming(
            permission_request(8, ALLOW_DENY, kind="execute", diff_paths=["NOTES.md"])
        )
        self.assertEqual(sent_messages(stdin)[0]["result"]["outcome"].get("optionId"), "deny")
        self.assertEqual(client.permission_log[0]["decision"], "denied")

    def test_no_deny_option_cancels_not_approves(self):
        client, stdin = make_client("deny")
        client._handle_incoming(permission_request(10, ONLY_ALLOW, diff_paths=["NOTES.md"]))
        outcome = sent_messages(stdin)[0]["result"]["outcome"]
        self.assertEqual(outcome.get("outcome"), "cancelled")
        self.assertNotIn("optionId", outcome)
        self.assertEqual(client.permission_log[0]["decision"], "cancelled-no-deny-option")


class VerifiedEditScopeTest(unittest.TestCase):
    """R2: approve-verified-edits SOLO con destino verificado dentro del repo."""

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="aeril-p2s0-scope-")
        self.repo = os.path.join(self.tmp, "repo")
        os.makedirs(self.repo)
        with open(os.path.join(self.repo, "NOTES.md"), "w") as f:
            f.write("base\n")
        self.outside = os.path.join(self.tmp, "outside")
        os.makedirs(self.outside)

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_in_repo_diff_approved(self):
        client, stdin = make_client("approve-verified-edits", repo=self.repo)
        client._handle_incoming(permission_request(20, ALLOW_DENY, diff_paths=["NOTES.md"]))
        self.assertEqual(sent_messages(stdin)[0]["result"]["outcome"].get("optionId"), "allow_once")
        entry = client.permission_log[0]
        self.assertEqual(entry["decision"], "approved-verified-edit")
        self.assertEqual(entry["paths"], ["NOTES.md"])
        self.assertNotIn("newText", json.dumps(entry), "no se persiste contenido de diffs")

    def test_locations_in_repo_approved(self):
        client, stdin = make_client("approve-verified-edits", repo=self.repo)
        client._handle_incoming(
            permission_request(21, ALLOW_DENY, locations=["sub/dir/file.txt"])
        )
        self.assertEqual(sent_messages(stdin)[0]["result"]["outcome"].get("optionId"), "allow_once")

    def test_missing_paths_denied(self):
        client, stdin = make_client("approve-verified-edits", repo=self.repo)
        client._handle_incoming(permission_request(22, ALLOW_DENY))
        self.assertEqual(sent_messages(stdin)[0]["result"]["outcome"].get("optionId"), "deny")
        self.assertIn("sin rutas", client.permission_log[0]["reason"])

    def test_relative_escape_denied(self):
        client, stdin = make_client("approve-verified-edits", repo=self.repo)
        client._handle_incoming(permission_request(23, ALLOW_DENY, diff_paths=["../escape.txt"]))
        self.assertEqual(sent_messages(stdin)[0]["result"]["outcome"].get("optionId"), "deny")
        self.assertIn("fuera del repo", client.permission_log[0]["reason"])

    def test_absolute_path_denied(self):
        client, stdin = make_client("approve-verified-edits", repo=self.repo)
        client._handle_incoming(permission_request(24, ALLOW_DENY, diff_paths=["/etc/passwd"]))
        self.assertEqual(sent_messages(stdin)[0]["result"]["outcome"].get("optionId"), "deny")

    def test_symlink_escape_denied(self):
        os.symlink(self.outside, os.path.join(self.repo, "link"))
        client, stdin = make_client("approve-verified-edits", repo=self.repo)
        client._handle_incoming(permission_request(25, ALLOW_DENY, diff_paths=["link/evil.txt"]))
        self.assertEqual(sent_messages(stdin)[0]["result"]["outcome"].get("optionId"), "deny")
        self.assertIn("fuera del repo", client.permission_log[0]["reason"])

    def test_mixed_paths_one_outside_denies_all(self):
        client, stdin = make_client("approve-verified-edits", repo=self.repo)
        client._handle_incoming(
            permission_request(26, ALLOW_DENY, diff_paths=["NOTES.md", "../escape.txt"])
        )
        self.assertEqual(sent_messages(stdin)[0]["result"]["outcome"].get("optionId"), "deny")

    def test_ask_human_approves_only_with_explicit_yes_and_verification(self):
        client, stdin = make_client("ask-human", repo=self.repo, ask_fn=lambda _s: True)
        client._handle_incoming(permission_request(27, ALLOW_DENY, diff_paths=["NOTES.md"]))
        self.assertEqual(sent_messages(stdin)[0]["result"]["outcome"].get("optionId"), "allow_once")
        self.assertEqual(client.permission_log[0]["decision"], "approved-human")

    def test_ask_human_denied_when_user_says_no(self):
        client, stdin = make_client("ask-human", repo=self.repo, ask_fn=lambda _s: False)
        client._handle_incoming(permission_request(28, ALLOW_DENY, diff_paths=["NOTES.md"]))
        self.assertEqual(sent_messages(stdin)[0]["result"]["outcome"].get("optionId"), "deny")

    def test_ask_human_unverified_never_reaches_human(self):
        called = []
        client, stdin = make_client(
            "ask-human", repo=self.repo, ask_fn=lambda s: called.append(s) or True
        )
        client._handle_incoming(permission_request(29, ALLOW_DENY, diff_paths=["../escape.txt"]))
        self.assertEqual(sent_messages(stdin)[0]["result"]["outcome"].get("optionId"), "deny")
        self.assertEqual(called, [], "sin verificación no se consulta al humano")

    def test_default_ask_without_tty_fails_closed(self):
        client, _stdin = make_client("ask-human", repo=self.repo)
        self.assertFalse(client._default_ask({"kind": "edit", "paths": ["NOTES.md"]}))


class V4aMultiFileMetadataTest(unittest.TestCase):
    """F1 (R3): el builder real de Hermes 0.21.5
    (`acp_adapter/edit_approval.py::_proposal_for_patch_v4a`) surfacea
    multi-archivo como un string display `", ".join(paths)` en
    `content[0].path`, SIN `locations` ni metadata por archivo. Ese string NO
    es un path verificable: hay que rechazar el pedido completo."""

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="aeril-p2s0-v4a-")
        self.repo = os.path.join(self.tmp, "repo")
        os.makedirs(self.repo)
        with open(os.path.join(self.repo, "NOTES.md"), "w") as f:
            f.write("base\n")
        self.outside = os.path.join(self.tmp, "outside")
        os.makedirs(self.outside)

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _payload(self, display_path, req_id):
        # Réplica EXACTA del payload del builder instalado para multi-archivo.
        return permission_request(req_id, ALLOW_DENY, diff_paths=[display_path])

    def test_v4a_multifile_traversal_rejected(self):
        client, stdin = make_client("approve-verified-edits", repo=self.repo)
        client._handle_incoming(
            self._payload("inrepo.txt, ../outside/victim.txt", 30)
        )
        outcome = sent_messages(stdin)[0]["result"]["outcome"]
        self.assertEqual(outcome.get("optionId"), "deny", outcome)
        self.assertIn("multi-archivo", client.permission_log[0]["reason"])
        self.assertEqual(client.permission_log[0]["paths"], [])

    def test_v4a_multifile_all_inside_still_rejected(self):
        client, stdin = make_client("approve-verified-edits", repo=self.repo)
        client._handle_incoming(self._payload("a.txt, b.txt", 31))
        outcome = sent_messages(stdin)[0]["result"]["outcome"]
        self.assertEqual(outcome.get("optionId"), "deny", outcome)
        self.assertIn("multi-archivo", client.permission_log[0]["reason"])

    def test_v4a_multifile_ask_human_never_invokes_callback(self):
        called = []
        client, stdin = make_client(
            "ask-human", repo=self.repo, ask_fn=lambda s: called.append(s) or True
        )
        client._handle_incoming(
            self._payload("inrepo.txt, ../outside/victim.txt", 32)
        )
        outcome = sent_messages(stdin)[0]["result"]["outcome"]
        self.assertEqual(outcome.get("optionId"), "deny", outcome)
        self.assertEqual(called, [], "un pedido no verificable no llega al humano")
        self.assertNotIn("inrepo.txt, ../outside", json.dumps(client.permission_log))

    def test_move_display_metadata_rejected(self):
        client, stdin = make_client("approve-verified-edits", repo=self.repo)
        client._handle_incoming(self._payload("old.txt -> new.txt", 33))
        outcome = sent_messages(stdin)[0]["result"]["outcome"]
        self.assertEqual(outcome.get("optionId"), "deny", outcome)
        self.assertIn("move/rename", client.permission_log[0]["reason"])

    def test_single_file_write_inside_allowed_preserved(self):
        # Single-file: el builder usa paths[0] tal cual (p. ej. write_file).
        client, stdin = make_client("approve-verified-edits", repo=self.repo)
        client._handle_incoming(permission_request(34, ALLOW_DENY, diff_paths=["NOTES.md"]))
        outcome = sent_messages(stdin)[0]["result"]["outcome"]
        self.assertEqual(outcome.get("optionId"), "allow_once", outcome)
        self.assertEqual(client.permission_log[0]["decision"], "approved-verified-edit")

    def test_single_file_escapes_still_rejected(self):
        os.symlink(self.outside, os.path.join(self.repo, "link"))
        for req_id, path in ((35, "/etc/passwd"), (36, "../escape.txt"), (37, "link/evil.txt")):
            client, stdin = make_client("approve-verified-edits", repo=self.repo)
            client._handle_incoming(permission_request(req_id, ALLOW_DENY, diff_paths=[path]))
            outcome = sent_messages(stdin)[0]["result"]["outcome"]
            self.assertEqual(outcome.get("optionId"), "deny", (path, outcome))


def update_msg(kind, text):
    return {
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {"update": {"sessionUpdate": kind, "content": {"type": "text", "text": text}}},
    }


class L4MeasurementTest(unittest.TestCase):
    """F1 (R7-R1): L4 SOLO cuenta mensajes del agente del prompt NUEVO.

    Replay, thoughts y mensajes de usuario nunca son evidencia de recall."""

    CANARY = "CANARY-L4-abc12345"

    def setUp(self):
        self.client, _stdin = make_client("deny")

    def test_replay_only_never_counts(self):
        c = self.client
        c.phase = "load"
        c._handle_incoming(update_msg("agent_message_chunk", f"Memorizado: {self.CANARY}"))
        c.phase = "init"
        self.assertFalse(c.l4_canary_in_new_response(self.CANARY))
        self.assertEqual(len(c.replay_agent_texts), 1)

    def test_replay_plus_response_without_canary_false(self):
        c = self.client
        c.phase = "load"
        c._handle_incoming(update_msg("agent_message_chunk", f"Memorizado: {self.CANARY}"))
        c.phase = "prompt"
        c._handle_incoming(update_msg("agent_message_chunk", "No lo sé."))
        c.phase = "init"
        self.assertFalse(c.l4_canary_in_new_response(self.CANARY))

    def test_new_response_recall_true(self):
        c = self.client
        c.phase = "load"
        c._handle_incoming(update_msg("agent_message_chunk", "OK"))
        c.phase = "prompt"
        c._handle_incoming(update_msg("agent_message_chunk", "El identificador es "))
        c._handle_incoming(update_msg("agent_message_chunk", self.CANARY))
        c.phase = "init"
        self.assertTrue(c.l4_canary_in_new_response(self.CANARY))

    def test_fresh_no_load_false(self):
        c = self.client
        c.phase = "prompt"
        c._handle_incoming(update_msg("agent_message_chunk", "No tengo ese dato."))
        c.phase = "init"
        self.assertFalse(c.l4_canary_in_new_response(self.CANARY))

    def test_unknown_session_false(self):
        # load de id desconocido ⇒ replay vacío; respuesta sin canary ⇒ False
        c = self.client
        c.phase = "load"
        c.phase = "prompt"
        c._handle_incoming(update_msg("agent_message_chunk", "No dispongo del identificador."))
        c.phase = "init"
        self.assertFalse(c.l4_canary_in_new_response(self.CANARY))
        self.assertEqual(len(c.replay_agent_texts), 0)

    def test_cross_profile_false(self):
        # id de A bajo B: replay vacío; y un canary de A no aparece en la respuesta de B
        c = self.client
        c.phase = "prompt"
        c._handle_incoming(update_msg("agent_message_chunk", "No sé a qué te referís."))
        c.phase = "init"
        self.assertFalse(c.l4_canary_in_new_response("CANARY-DE-A-99999999"))

    def test_thought_chunk_never_counts(self):
        c = self.client
        c.phase = "load"
        c._handle_incoming(update_msg("agent_message_chunk", self.CANARY))
        c.phase = "prompt"
        c._handle_incoming(update_msg("agent_thought_chunk", f"¿Será {self.CANARY}?"))
        c._handle_incoming(update_msg("agent_message_chunk", "No lo sé."))
        c.phase = "init"
        self.assertFalse(c.l4_canary_in_new_response(self.CANARY))
        self.assertEqual(len(c.prompt_thought_texts), 1)

    def test_failed_turn_boundary_detected_and_l4_false(self):
        c = self.client
        c.phase = "prompt"
        c._handle_incoming(
            update_msg(
                "agent_message_chunk",
                "Your request was not processed. Send it again if you still want me to carry it out.",
            )
        )
        c.phase = "init"
        self.assertTrue(c.failed_turn_boundary_seen())
        self.assertFalse(c.l4_canary_in_new_response(self.CANARY))

    def test_real_response_not_flagged_as_failed_turn(self):
        c = self.client
        c.phase = "prompt"
        c._handle_incoming(update_msg("agent_message_chunk", "El identificador es CANARY-OK."))
        c.phase = "init"
        self.assertFalse(c.failed_turn_boundary_seen())


class InheritedModeGuardTest(unittest.TestCase):
    def test_default_mode_auth_ops_refused(self):
        with self.assertRaises(SystemExit) as cm:
            probe.inherited_auth_guard("default", True, False)
        self.assertEqual(cm.exception.code, 3)

    def test_default_mode_initialize_only_allowed(self):
        probe.inherited_auth_guard("default", False, False)

    def test_default_mode_with_explicit_override_allowed(self):
        probe.inherited_auth_guard("default", True, True)

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
        os.environ["SSH_AUTH_SOCK"] = "/tmp/fake-agent.sock"
        try:
            env = probe.build_env("sanitized", "/tmp/san", "/tmp/hermes")
            for leaked in (
                "GEMINI_API_KEY",
                "ANTHROPIC_AUTH_TOKEN",
                "GH_TOKEN",
                "GITHUB_TOKEN",
                "SSH_AUTH_SOCK",
            ):
                self.assertNotIn(leaked, env)
            self.assertEqual(env["HOME"], "/tmp/san")
            self.assertEqual(env["HERMES_HOME"], "/tmp/hermes")
            for xdg in ("XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME"):
                self.assertTrue(env[xdg].startswith("/tmp/san"), xdg)
        finally:
            del os.environ["GEMINI_API_KEY"]
            del os.environ["ANTHROPIC_AUTH_TOKEN"]
            del os.environ["SSH_AUTH_SOCK"]

    def test_gh_config_dir_points_into_san_home(self):
        env = probe.build_env("sanitized", "/tmp/san", "/tmp/hermes")
        self.assertEqual(env.get("GH_CONFIG_DIR"), "/tmp/san/.config/gh")
        env2 = probe.build_env("nohome", "/tmp/san", "/tmp/hermes")
        self.assertEqual(env2.get("GH_CONFIG_DIR"), "/tmp/san/.config/gh")

    def test_gh_config_dir_not_allowlisted(self):
        self.assertNotIn("GH_CONFIG_DIR", probe.SAFE_ENV_ALLOW)
        with self.assertRaises(SystemExit) as cm:
            probe.build_env("sanitized", "/tmp/san", "/tmp/hermes", ("GH_CONFIG_DIR",))
        self.assertEqual(cm.exception.code, 3)

    def test_ssh_auth_sock_not_allowlisted(self):
        self.assertNotIn("SSH_AUTH_SOCK", probe.SAFE_ENV_ALLOW)
        with self.assertRaises(SystemExit) as cm:
            probe.build_env("sanitized", "/tmp/san", "/tmp/hermes", ("SSH_AUTH_SOCK",))
        self.assertEqual(cm.exception.code, 3)

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
