#!/usr/bin/env python3
"""P2-S0 — Harness de factibilidad de Hermes autenticado (spike, no producto).

Security R1 (este archivo):
- Permisos FAIL-CLOSED: por defecto se DENIEGA todo pedido del agente; jamás se
  elige una opción `allow*` automáticamente. `--permissions
  approve-verified-edits` solo aprueba pedidos kind=="edit" cuyas rutas
  (content.diff/locations) se verifican CONFINADAS al repo descartable;
  `--permissions ask-human` exige aprobación humana explícita por pedido
  (además de la verificación). execute/shell/ambiguo/desconocido se deniega.
- HOME sanitizado POR PERFIL (`san-home-A`, `san-home-B`), 0700, sin
  credenciales del host; XDG_* apuntan al HOME sanitizado.
- `--env-mode` default = `sanitized`; el modo heredado (`default`) queda
  prohibido para `session/new`/`prompt`/`load` salvo autorización explícita
  (`--allow-inherited-auth-ops`), y nunca cuenta como evidencia de auth.
- Evidencia redactada: sin texto de mensajes/razonamiento del agente, sin
  títulos arbitrarios de tool calls; solo tipos de evento, estados, timings,
  identificadores y errores truncados. El canary se verifica en memoria
  (`--canary`) y solo se persiste el booleano.
- Un solo escritor por perfil: la verificación de responsividad se hace con un
  segundo `initialize` SOBRE LA MISMA conexión (`--ping-after`), nunca con un
  segundo proceso sobre el mismo HERMES_HOME.

Uso:
  s0_probe.py setup   --scratch DIR
  s0_probe.py probe   --home DIR --repo DIR [--env-mode sanitized|nohome|default]
                      [--permissions deny|approve-edits] [--hold SECS]
                      [--session-new] [--prompt TEXTO] [--canary VALOR]
                      [--load SESSION_ID] [--ping-after SECS]
                      [--env-allow VAR ...] [--json-out FILE]
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
SANITIZED_PATH = "/usr/bin:/bin"
# Variables no-secretas permitidas por lista explícita (p. ej. para OAuth con
# navegador). Nunca se copia el entorno completo como atajo.
# Lista blanca mínima para onboarding interactivo (display/browser).
# SSH_AUTH_SOCK queda FUERA a propósito: un socket de agente SSH otorga una
# capacidad de autenticación y no es una variable inocua.
SAFE_ENV_ALLOW = {
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XDG_RUNTIME_DIR",
    "BROWSER",
}


def log(msg):
    print(f"[s0] {msg}", file=sys.stderr, flush=True)


def san_home_for(profile_home):
    """HOME sanitizado por perfil: <parent>/san-home-<sufijo-de-perfil>."""
    parent = os.path.dirname(os.path.abspath(profile_home))
    base = os.path.basename(os.path.abspath(profile_home))
    suffix = base[2:] if base.startswith("p-") else base
    return os.path.join(parent, f"san-home-{suffix}")


def build_env(mode, san_home, hermes_home, env_allow=()):
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
        env["HOME"] = san_home
        env["XDG_CONFIG_HOME"] = os.path.join(san_home, ".config")
        env["XDG_CACHE_HOME"] = os.path.join(san_home, ".cache")
        env["XDG_DATA_HOME"] = os.path.join(san_home, ".local", "share")
        env["XDG_STATE_HOME"] = os.path.join(san_home, ".local", "state")
    # Defensa en profundidad (E2): `gh auth token` no debe poder leer la config
    # del host aunque HOME se herede por error; GH_CONFIG_DIR apunta a un dir
    # vacío del entorno sanitizado (gh lo respeta por encima de HOME/XDG).
    env["GH_CONFIG_DIR"] = os.path.join(san_home, ".config", "gh")
    for name in env_allow:
        if name not in SAFE_ENV_ALLOW:
            print(
                f"[s0] RECHAZADO: --env-allow solo admite variables no-secretas: "
                f"{sorted(SAFE_ENV_ALLOW)}",
                file=sys.stderr,
            )
            sys.exit(3)
        if name in os.environ:
            env[name] = os.environ[name]
    return env


def inherited_auth_guard(mode, auth_ops, allow_inherited):
    """Modo heredado prohibido para operaciones autenticadas salvo override
    explícito; nunca cuenta como evidencia de auth por perfil."""
    if mode == "default" and auth_ops and not allow_inherited:
        print(
            "[s0] RECHAZADO: session/new|prompt|load en modo `default` (entorno "
            "heredado) están prohibidos: pueden usar credenciales ambientales del "
            "host (p. ej. gemini) y NO son evidencia de autenticación por perfil. "
            "Usá --env-mode sanitized, o --allow-inherited-auth-ops para un "
            "diagnóstico explícito (marcado como no-evidencia).",
            file=sys.stderr,
        )
        sys.exit(3)


class FakeProc:
    """Proceso falso para tests unitarios del manejo de permisos."""

    def __init__(self, stdin):
        self.stdin = stdin
        self.pid = 0
        self.returncode = None

    def poll(self):
        return None


class AcpClient:
    def __init__(self, proc, home, repo, permissions="deny", ask_fn=None):
        self.proc = proc
        self.home = home
        self.repo = repo
        self.permissions_policy = permissions
        self.ask_fn = ask_fn or self._default_ask
        self.next_id = 1
        self.buf = b""
        self.event_kinds = {}
        self.permission_log = []
        self._transient_texts = []  # solo en memoria; nunca se persiste
        self.timings = {}
        self.session_id = None

    def _note(self, kind):
        self.event_kinds[kind] = self.event_kinds.get(kind, 0) + 1

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
            self._handle_permission(msg)
        elif method == "session/update":
            upd = (msg.get("params") or {}).get("update") or {}
            st = upd.get("sessionUpdate") or "unknown"
            self._note(st)
            if st in ("agent_message_chunk", "agent_thought_chunk"):
                c = upd.get("content")
                if isinstance(c, dict):
                    # Solo en memoria (verificación de canary); no se persiste.
                    self._transient_texts.append(str(c.get("text", ""))[:2000])
        else:
            self._note(f"other:{method}")

    def _verify_edit_scope(self, msg):
        """Verifica que un pedido `edit` esté CONFINADO al repo descartable.

        Rutas candidatas: bloques `content` de tipo `diff` (`path`) y
        `locations` (`path`). Reglas fail-closed:
        - kind debe ser exactamente "edit";
        - al menos una ruta verificable;
        - ninguna ruta absoluta/NUL/vacía;
        - SIN separadores de display multi-archivo V4A (`", "`) ni de
          move/rename (`" -> "`): el builder instalado los emite como string
          de display, no como paths verificables (F1);
        - TODAS deben resolver (realpath, siguiendo symlinks) dentro del repo.
        Devuelve (ok, reason, relative_paths).
        """
        params = msg.get("params") or {}
        tool = params.get("toolCall") or {}
        kind = str(tool.get("kind") or "unknown")
        if kind != "edit":
            return False, f"kind != edit ({kind})", []
        paths = []
        for block in tool.get("content") or []:
            if isinstance(block, dict) and block.get("type") == "diff" and block.get("path"):
                paths.append(str(block["path"]))
        for loc in tool.get("locations") or []:
            if isinstance(loc, dict) and loc.get("path"):
                paths.append(str(loc["path"]))
        if not paths:
            return False, "sin rutas verificables (content.diff/locations ausentes)", []
        repo_real = os.path.realpath(self.repo)
        rel = []
        for raw in paths:
            if not raw or "\x00" in raw or os.path.isabs(raw):
                return False, f"ruta inválida o absoluta: {raw!r}", []
            # F1 (R3): el builder V4A instalado surfacea multi-archivo como
            # `", ".join(paths)` (display), sin metadata por archivo ni
            # `locations`. Ese string NO es verificable: no se hace realpath
            # sobre él ni se parte en paths "verificados". Se rechaza entero.
            if ", " in raw:
                # Motivo sin eco del display crudo (política de evidencia R2).
                return (
                    False,
                    "metadata multi-archivo V4A no verificable (separador de join ', ')",
                    [],
                )
            if " -> " in raw:
                return (
                    False,
                    "display de move/rename no verificable (' -> ')",
                    [],
                )
            joined = os.path.realpath(os.path.join(repo_real, raw))
            if not joined.startswith(repo_real + os.sep):
                return False, f"ruta fuera del repo: {raw!r}", []
            rel.append(os.path.relpath(joined, repo_real))
        return True, "ok", rel

    def _default_ask(self, summary):
        """Aprobación humana explícita: solo con TTY interactiva; sin TTY,
        fail-closed (deniega)."""
        if not sys.stdin.isatty():
            log(f"permiso {summary}: sin TTY para aprobación humana ⇒ DENEGADO")
            return False
        print(f"[s0] pedido de permiso verificado: {summary}")
        try:
            answer = input("[s0] escribir 'allow' para aprobar (otra cosa deniega): ")
        except EOFError:
            return False
        return answer.strip() == "allow"

    def _handle_permission(self, msg):
        """FAIL-CLOSED: `deny` por defecto; nunca `allow*` automático.
        `approve-verified-edits` solo aprueba edits CONFINADOS al repo;
        `ask-human` exige aprobación humana explícita por pedido (y también
        verificación de alcance). Cualquier cosa dudosa se deniega/cancela."""
        params = msg.get("params") or {}
        options = params.get("options") or []
        tool = params.get("toolCall") or {}
        tool_kind = str(tool.get("kind") or "unknown")
        ok, reason, rel_paths = self._verify_edit_scope(msg)
        deny_id = next(
            (o.get("optionId") for o in options if str(o.get("optionId", "")) == "deny"),
            None,
        )
        allow_id = next(
            (
                o.get("optionId")
                for o in options
                if str(o.get("optionId", "")).startswith("allow")
            ),
            None,
        )
        approved = False
        if ok and allow_id is not None:
            if self.permissions_policy == "approve-verified-edits":
                approved = True
            elif self.permissions_policy == "ask-human":
                approved = bool(self.ask_fn({"kind": tool_kind, "paths": rel_paths}))
        if approved:
            outcome = {"outcome": "selected", "optionId": allow_id}
            decision = (
                "approved-verified-edit"
                if self.permissions_policy == "approve-verified-edits"
                else "approved-human"
            )
        elif deny_id is not None:
            outcome = {"outcome": "selected", "optionId": deny_id}
            decision = "denied"
        else:
            outcome = {"outcome": "cancelled"}
            decision = "cancelled-no-deny-option"
        self.send({"jsonrpc": "2.0", "id": msg.get("id"), "result": {"outcome": outcome}})
        # Evidencia mínima: tipo, decisión, motivo y rutas RELATIVAS verificadas.
        self.permission_log.append(
            {
                "tool_kind": tool_kind,
                "decision": decision,
                "reason": reason,
                "paths": rel_paths,
            }
        )

    def canary_seen(self, canary):
        return any(canary in t for t in self._transient_texts)


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
    manifest = {"scratch": scratch, "profiles": {}, "san_homes": {}, "repos": {}}
    for name in ("p-A", "p-B"):
        home = os.path.join(scratch, name)
        os.makedirs(home, mode=0o700, exist_ok=True)
        os.chmod(home, 0o700)
        san = san_home_for(home)
        os.makedirs(san, mode=0o700, exist_ok=True)
        os.chmod(san, 0o700)
        manifest["profiles"][name] = home
        manifest["san_homes"][name] = san
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
    san = args.san_home or san_home_for(home)
    inherited_auth_guard(
        args.env_mode,
        bool(args.session_new or args.prompt or args.load),
        args.allow_inherited_auth_ops,
    )
    env = build_env(args.env_mode, san, home, tuple(args.env_allow))
    report = {
        "env_mode": args.env_mode,
        "hermes_home": home,
        "san_home": san if args.env_mode == "sanitized" else None,
        "repo": repo,
        "permissions_policy": args.permissions,
        "pid": None,
        "timings_ms": {},
        "rss_kb": {},
        "session_id": None,
        "session_new_error": None,
        "prompt_error": None,
        "event_kinds": {},
        "permissions": [],
        "terminated": None,
        "inherited_mode_auth_ops": bool(
            args.env_mode == "default" and (args.session_new or args.prompt or args.load)
        ),
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
    client = AcpClient(proc, home, repo, permissions=args.permissions)
    try:
        t = time.monotonic()
        resp = client.request(
            "initialize",
            {
                "protocolVersion": 1,
                "clientCapabilities": {},
                "clientInfo": {"name": "aeril-p2s0-probe", "version": "0.2"},
            },
            timeout=args.init_timeout,
        )
        report["timings_ms"]["initialize"] = round((time.monotonic() - t) * 1000, 1)
        ir = resp.get("result") or {}
        report["auth_methods"] = [m.get("id") for m in (ir.get("authMethods") or [])]
        report["timings_ms"]["spawn_to_initialize"] = round((time.monotonic() - t0) * 1000, 1)
        if args.session_new:
            t = time.monotonic()
            try:
                r = client.request(
                    "session/new", {"cwd": repo, "mcpServers": []}, timeout=args.req_timeout
                )
                report["timings_ms"]["session_new"] = round((time.monotonic() - t) * 1000, 1)
                if "error" in r:
                    e = r["error"]
                    details = str((e.get("data") or {}).get("details", ""))[:200]
                    report["session_new_error"] = {
                        "code": e.get("code"),
                        "message": str(e.get("message"))[:200],
                        "details": details,
                    }
                else:
                    client.session_id = (r.get("result") or {}).get("sessionId")
                    report["session_id"] = client.session_id
            except (TimeoutError, RuntimeError) as e:
                report["session_new_error"] = {"message": str(e)[:200]}
        if args.load:
            try:
                r = client.request(
                    "session/load",
                    {"sessionId": args.load, "cwd": repo, "mcpServers": []},
                    timeout=args.req_timeout,
                )
                report["load_status"] = "ok" if "result" in r else "error"
                if "error" in r:
                    report["load_error"] = {
                        "code": r["error"].get("code"),
                        "message": str(r["error"].get("message"))[:200],
                    }
                client.session_id = args.load
            except (TimeoutError, RuntimeError) as e:
                report["load_error"] = {"message": str(e)[:200]}
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
                    report["prompt_error"] = {
                        "code": r["error"].get("code"),
                        "message": str(r["error"].get("message"))[:200],
                    }
                else:
                    report["prompt_stop_reason"] = (r.get("result") or {}).get("stopReason")
            except (TimeoutError, RuntimeError) as e:
                report["prompt_error"] = {"message": str(e)[:200]}
        if args.canary:
            report["canary_found"] = client.canary_seen(args.canary)
        time.sleep(2.0)
        report["rss_kb"]["idle_after_init"] = rss_kb(proc.pid)
        # Hold con ping opcional SOBRE LA MISMA conexión (un solo escritor).
        remaining = max(0.0, args.hold - (time.monotonic() - t0))
        if remaining > 0:
            log(f"{home}: hold {remaining:.0f}s (pid {proc.pid})")
        if args.ping_after:
            while (time.monotonic() - t0) < args.ping_after:
                time.sleep(0.2)
            t = time.monotonic()
            try:
                r = client.request(
                    "initialize",
                    {"protocolVersion": 1},
                    timeout=min(60.0, args.req_timeout),
                )
                report["ping_ok"] = "result" in r
                report["ping_ms"] = round((time.monotonic() - t) * 1000, 1)
            except (TimeoutError, RuntimeError) as e:
                report["ping_ok"] = False
                report["ping_error"] = str(e)[:200]
        if remaining > 0:
            time.sleep(max(0.0, args.hold - (time.monotonic() - t0)))
        report["rss_kb"]["idle_end_of_hold"] = rss_kb(proc.pid)
    except Exception as e:  # noqa: BLE001
        report["error"] = f"{type(e).__name__}: {e}"
    finally:
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
        report["event_kinds"] = client.event_kinds
        report["permissions"] = client.permission_log
        report["disk_kb_home"] = du_kb(home)
    out = json.dumps(report, indent=2)
    if args.json_out:
        with open(args.json_out, "w") as f:
            f.write(out + "\n")
    print(out)
    return 0 if "error" not in report else 1


def cmd_runbook(args):
    home = os.path.abspath(args.home)
    san = san_home_for(home)
    print(
        f"""Runbook de autenticación INTERACTIVA (usuario) — perfil: {home}
HOME sanitizado del perfil: {san}

  # 1) Autenticar ESTE perfil (entorno sanitizado, el mismo del runtime):
  env -i PATH=/usr/bin:/bin LANG=C.UTF-8 HOME={san} \\
      XDG_CONFIG_HOME={san}/.config XDG_CACHE_HOME={san}/.cache \\
      XDG_DATA_HOME={san}/.local/share XDG_STATE_HOME={san}/.local/state \\
      HERMES_HOME={home} {HERMES} auth add

  # 2) Verificar estado SIN exponer valores (solo etiquetas/estado):
  env -i PATH=/usr/bin:/bin LANG=C.UTF-8 HOME={san} HERMES_HOME={home} \\
      {HERMES} auth list

  # 3) Modelo por perfil (si hace falta): ídem con `model`.

Si el OAuth necesita variables de navegador, agregá SOLO las necesarias y
no-secretas (DISPLAY, WAYLAND_DISPLAY, XDG_RUNTIME_DIR, BROWSER) al `env -i`;
el harness lo permite con `--env-allow VAR`. NUNCA copies el entorno completo.

Reglas: no copiar auth.json, no imprimir tokens, no usar el perfil del host
(~/.hermes) ni credenciales ambientales (gh/gemini/codex/claude)."""
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
    p.add_argument(
        "--env-mode", choices=["sanitized", "nohome", "default"], default="sanitized"
    )
    p.add_argument("--san-home", default=None)
    p.add_argument(
        "--permissions",
        choices=["deny", "approve-verified-edits", "ask-human"],
        default="deny",
    )
    p.add_argument("--hold", type=float, default=0.0)
    p.add_argument("--ping-after", type=float, default=0.0)
    p.add_argument("--session-new", action="store_true")
    p.add_argument("--prompt", default=None)
    p.add_argument("--canary", default=None)
    p.add_argument("--load", default=None)
    p.add_argument("--env-allow", action="append", default=[])
    p.add_argument("--allow-inherited-auth-ops", action="store_true")
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
