#!/usr/bin/env python3
"""P2-S0 — Harness de factibilidad de Hermes autenticado (spike, no producto).

Narrow scope:
- Cliente ACP mínimo (ndjson/JSON-RPC) para `initialize`, `session/new`,
  `session/prompt`, `session/load`, y respuesta a `session/request_permission`.
- Modos de entorno: `default` (hereda), `sanitized` (env mínimo + HOME
  sanitizado), `nohome` (env mínimo sin HOME).
- Mediciones: timings de initialize/session, RSS por /proc, uso de disco.
- NUNCA lee/imprime/copia credenciales: no toca auth.json; los únicos campos
  de auth que puede registrar son etiquetas/estados si se piden por CLI aparte.

Uso:
  s0_probe.py setup   --scratch DIR
  s0_probe.py probe   --home DIR --repo DIR [--env-mode default|sanitized|nohome]
                      [--hold SECS] [--session-new] [--prompt TEXTO]
                      [--load SESSION_ID] [--json-out FILE]
  s0_probe.py runbook --home DIR
"""

import argparse
import json
import os
import select
import signal
import subprocess
import sys
import time

HERMES = "/home/elliot/.local/bin/hermes"
MIN_ENV_KEYS = ("PATH",)
SANITIZED_PATH = "/usr/bin:/bin"


def log(msg):
    print(f"[s0] {msg}", file=sys.stderr, flush=True)


def build_env(mode, home_san, hermes_home):
    if mode == "default":
        env = dict(os.environ)
        env["HERMES_HOME"] = hermes_home
        return env
    env = {
        "PATH": SANITIZED_PATH,
        "TERM": "dumb",
        "LANG": "C.UTF-8",
        "HERMES_HOME": hermes_home,
    }
    if mode == "sanitized":
        env["HOME"] = home_san
    return env


class AcpClient:
    def __init__(self, proc, home, repo):
        self.proc = proc
        self.home = home
        self.repo = repo
        self.next_id = 1
        self.buf = b""
        self.events = []  # resumen acotado
        self.permissions = 0
        self.timings = {}
        self.session_id = None

    def send(self, obj):
        data = (json.dumps(obj) + "\n").encode()
        self.proc.stdin.write(data)
        self.proc.stdin.flush()

    def request(self, method, params, timeout=600):
        rid = self.next_id
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        return self._await_response(rid, timeout)

    def _read_line(self, timeout):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if b"\n" in self.buf:
                line, self.buf = self.buf.split(b"\n", 1)
                return line
            r, _, _ = select.select([self.proc.stdout], [], [], 0.1)
            if r:
                chunk = os.read(self.proc.stdout.fileno(), 65536)
                if not chunk:
                    return None
                self.buf += chunk
        raise TimeoutError("sin respuesta del backend")

    def _await_response(self, rid, timeout):
        while True:
            line = self._read_line(timeout)
            if line is None:
                raise RuntimeError("el backend cerró el canal")
            try:
                msg = json.loads(line)
            except ValueError:
                continue
            if msg.get("id") == rid and ("result" in msg or "error" in msg):
                return msg
            self._handle_incoming(msg)

    def _handle_incoming(self, msg):
        method = msg.get("method")
        if method == "session/request_permission":
            self.permissions += 1
            params = msg.get("params", {})
            options = (params.get("options") or [])
            allow = next(
                (o.get("optionId") for o in options if str(o.get("optionId", "")).startswith("allow")),
                None,
            )
            self.send(
                {
                    "jsonrpc": "2.0",
                    "id": msg.get("id"),
                    "result": {"outcome": {"outcome": "selected", "optionId": allow or "deny"}},
                }
            )
            # Solo metadatos no sensibles del pedido.
            self.events.append(
                {
                    "kind": "permission",
                    "title": str(params.get("toolCall", {}).get("title", ""))[:120],
                    "answered": allow or "deny",
                }
            )
        elif method == "session/update":
            upd = (msg.get("params") or {}).get("update") or {}
            st = upd.get("sessionUpdate")
            if st in ("agent_message_chunk", "agent_thought_chunk"):
                text = ""
                c = upd.get("content")
                if isinstance(c, dict):
                    text = str(c.get("text", ""))
                self.events.append({"kind": st, "text": text[:400]})
            else:
                self.events.append({"kind": st or "unknown"})
        else:
            self.events.append({"kind": f"other:{method}"})


def rss_kb(pid):
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS"):
                    return int(line.split()[1])
    except OSError:
        return None
    return None


def du_kb(path):
    total = 0
    for root, _dirs, files in os.walk(path):
        for name in files:
            try:
                total += os.path.getsize(os.path.join(root, name))
            except OSError:
                pass
    return total // 1024


def cmd_setup(args):
    scratch = os.path.abspath(args.scratch)
    os.makedirs(scratch, mode=0o700, exist_ok=True)
    manifest = {"scratch": scratch, "profiles": {}, "repos": {}, "san_home": None}
    for name in ("p-A", "p-B"):
        home = os.path.join(scratch, name)
        os.makedirs(home, mode=0o700, exist_ok=True)
        manifest["profiles"][name] = home
    san = os.path.join(scratch, "san-home")
    os.makedirs(san, mode=0o700, exist_ok=True)
    manifest["san_home"] = san
    for name in ("repo-A", "repo-B"):
        repo = os.path.join(scratch, name)
        os.makedirs(repo, exist_ok=True)
        if not os.path.exists(os.path.join(repo, ".git")):
            subprocess.run(["git", "init", "-q", "-b", "main", repo], check=True)
            subprocess.run(["git", "-C", repo, "config", "user.email", "p2s0@aeril.local"], check=True)
            subprocess.run(["git", "-C", repo, "config", "user.name", "P2S0 Probe"], check=True)
            with open(os.path.join(repo, "NOTES.md"), "w") as f:
                f.write("# P2-S0\n\nbase\n")
            subprocess.run(["git", "-C", repo, "add", "-A"], check=True)
            subprocess.run(["git", "-C", repo, "commit", "-q", "-m", "base"], check=True)
        manifest["repos"][name] = repo
    print(json.dumps(manifest, indent=2))
    return 0


def cmd_probe(args):
    home = os.path.abspath(args.home)
    repo = os.path.abspath(args.repo)
    san = args.san_home or os.path.join(os.path.dirname(home), "san-home")
    env = build_env(args.env_mode, san, home)
    report = {
        "env_mode": args.env_mode,
        "hermes_home": home,
        "repo": repo,
        "pid": None,
        "timings_ms": {},
        "rss_kb": {},
        "session_id": None,
        "session_new_error": None,
        "prompt_error": None,
        "events": [],
        "permissions": 0,
        "terminated": None,
    }
    t0 = time.monotonic()
    proc = subprocess.Popen(
        [HERMES, "acp"],
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        cwd=repo,
    )
    report["pid"] = proc.pid
    client = AcpClient(proc, home, repo)
    try:
        t = time.monotonic()
        resp = client.request(
            "initialize",
            {
                "protocolVersion": 1,
                "clientCapabilities": {},
                "clientInfo": {"name": "aeril-p2s0-probe", "version": "0.1"},
            },
            timeout=args.init_timeout,
        )
        report["timings_ms"]["initialize"] = round((time.monotonic() - t) * 1000, 1)
        report["initialize_result"] = resp.get("result")
        report["timings_ms"]["spawn_to_initialize"] = round((time.monotonic() - t0) * 1000, 1)
        if args.session_new:
            t = time.monotonic()
            try:
                r = client.request(
                    "session/new", {"cwd": repo, "mcpServers": []}, timeout=args.req_timeout
                )
                report["timings_ms"]["session_new"] = round((time.monotonic() - t) * 1000, 1)
                if "error" in r:
                    report["session_new_error"] = r["error"]
                else:
                    client.session_id = (r.get("result") or {}).get("sessionId")
                    report["session_id"] = client.session_id
            except (TimeoutError, RuntimeError) as e:
                report["session_new_error"] = str(e)
        if args.load:
            try:
                r = client.request(
                    "session/load",
                    {"sessionId": args.load, "cwd": repo, "mcpServers": []},
                    timeout=args.req_timeout,
                )
                report["load_result"] = r.get("result")
                report["load_error"] = r.get("error")
                client.session_id = args.load
            except (TimeoutError, RuntimeError) as e:
                report["load_error"] = str(e)
        if args.prompt and client.session_id:
            t = time.monotonic()
            try:
                r = client.request(
                    "session/prompt",
                    {
                        "sessionId": client.session_id,
                        "prompt": [{"type": "text", "text": args.prompt}],
                    },
                    timeout=args.prompt_timeout,
                )
                report["timings_ms"]["prompt"] = round((time.monotonic() - t) * 1000, 1)
                if "error" in r:
                    report["prompt_error"] = r["error"]
                else:
                    report["prompt_result"] = r.get("result")
            except (TimeoutError, RuntimeError) as e:
                report["prompt_error"] = str(e)
        # Hold: RSS idle estabilizado + responsive check opcional.
        time.sleep(2.0)
        report["rss_kb"]["idle_after_init"] = rss_kb(proc.pid)
        remaining = max(0.0, args.hold - (time.monotonic() - t0))
        if remaining > 0:
            log(f"{home}: hold {remaining:.0f}s (pid {proc.pid})")
            time.sleep(remaining)
        report["rss_kb"]["idle_end_of_hold"] = rss_kb(proc.pid)
    except Exception as e:  # noqa: BLE001
        report["error"] = f"{type(e).__name__}: {e}"
    finally:
        # Terminación controlada: SIGTERM → espera 5 s → SIGKILL.
        t = time.monotonic()
        if proc.poll() is None:
            proc.send_signal(signal.SIGTERM)
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
        rc = proc.returncode
        report["terminated"] = (
            f"signal {-rc}" if rc is not None and rc < 0 else f"exit {rc}"
        )
        report["timings_ms"]["terminate"] = round((time.monotonic() - t) * 1000, 1)
        report["events"] = client.events[-40:]
        report["permissions"] = client.permissions
        report["disk_kb_home"] = du_kb(home)
    out = json.dumps(report, indent=2)
    if args.json_out:
        with open(args.json_out, "w") as f:
            f.write(out + "\n")
    print(out)
    return 0 if "error" not in report else 1


def cmd_runbook(args):
    home = os.path.abspath(args.home)
    print(
        f"""Runbook de autenticación INTERACTIVA (usuario) — perfil: {home}

  # 1) Autenticar ESTE perfil (el usuario completa el flujo del proveedor):
  HERMES_HOME={home} {HERMES} auth add
  #    (alternativas soportadas: `auth upgrade` para Nous, o `auth add` de API key)

  # 2) Verificar estado SIN exponer valores (solo etiquetas/estado):
  HERMES_HOME={home} {HERMES} auth status
  HERMES_HOME={home} {HERMES} auth list

  # 3) Modelo por perfil (si hace falta):
  HERMES_HOME={home} {HERMES} model

Reglas: no copiar auth.json, no imprimir tokens, no reutilizar el perfil del
host (~/.hermes) ni credenciales ambientales (gh)."""
    )
    return 0


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)

    s = sub.add_parser("setup")
    s.add_argument("--scratch", required=True)
    s.set_defaults(fn=cmd_setup)

    p = sub.add_parser("probe")
    p.add_argument("--home", required=True)
    p.add_argument("--repo", required=True)
    p.add_argument("--env-mode", choices=["default", "sanitized", "nohome"], default="default")
    p.add_argument("--san-home", default=None)
    p.add_argument("--hold", type=float, default=0.0)
    p.add_argument("--session-new", action="store_true")
    p.add_argument("--prompt", default=None)
    p.add_argument("--load", default=None)
    p.add_argument("--json-out", default=None)
    p.add_argument("--init-timeout", type=float, default=300.0)
    p.add_argument("--req-timeout", type=float, default=300.0)
    p.add_argument("--prompt-timeout", type=float, default=600.0)
    p.set_defaults(fn=cmd_probe)

    r = sub.add_parser("runbook")
    r.add_argument("--home", required=True)
    r.set_defaults(fn=cmd_runbook)

    args = ap.parse_args()
    return args.fn(args)


if __name__ == "__main__":
    sys.exit(main())
