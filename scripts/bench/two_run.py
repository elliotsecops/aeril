#!/usr/bin/env python3
"""Medición del escenario REAL de dos runs mock concurrentes (release).

S6 §3: reproduce el escenario de G1 con el binario release y reporta:
- RSS de la TUI durante dos runs activos (VmRSS).
- RSS de cada backend mock por separado (NO es Hermes: no se infiere).
- RSS combinado (TUI + mocks).
- Arranque de una SEGUNDA TUI con dos runs activos en el store (beacons
  internos: first_draw → input_processed), es decir "startup with two active
  runs" sobre historia activa.
- Latencia de consultas (`run list --active`, `workspace list`).
- Condiciones del host (CPU/RAM/fstype del data dir).

S6-R1 (seguridad y reproducibilidad):
- Ciclo de vida SEGURO del data dir: solo se limpia un directorio que lleva el
  sentinel `.aeril-bench-two-run` (propiedad establecida por una corrida
  previa); un directorio ajeno se rechaza sin tocarlo. Targets inseguros
  (raíz, home, ancestros del checkout, directorios del sistema, symlinks)
  se rechazan.
- Limpieza de backends con IDENTIDAD: los PIDs se asocian a los runs de ESTA
  invocación, se verifica `/proc/<pid>/stat` starttime + cmdline (`__mock-agent`)
  antes de señalar, y se señaliza por `pidfd` (Python ≥3.9 con
  `signal.pidfd_send_signal`) para cerrar la carrera de reutilización de PID.
  Identidad no verificable ⇒ NO se señala (fail-closed) y se reporta.
- Validación de integridad: `"ok": true` exige exactamente 2 runs activos en
  workspaces distintos, 2 backends vivos con identidad verificada, TUI viva y
  RSS válidos (nunca ceros silenciosos).

Método: pty real + beacons + lectura del store con sqlite3 (stdlib). El data
dir es DESCARTABLE (se recrea solo si el sentinel lo acredita).

Uso:
  two_run.py --bin target/release/aeril --data-dir bench-data/two-run [--json]

Códigos de salida: 0 ok · 1 medición inválida · 2 rechazo de seguridad del
data dir · 3 ok con incertidumbre de limpieza (backends no verificables).
"""

import argparse
import hashlib
import json
import os
import pty
import shutil
import signal
import sqlite3
import subprocess
import sys
import time

SENTINEL = ".aeril-bench-two-run"
SYSTEM_DENY = (
    "/etc",
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/boot",
    "/dev",
    "/proc",
    "/sys",
    "/run",
    "/var",
)

KEY_ESC = b"\x1b"
KEY_DOWN = b"\x1b[B"
KEY_UP = b"\x1b[A"
KEY_ENTER = b"\r"


def refuse(msg):
    print(f"two_run: RECHAZADO: {msg}", file=sys.stderr)
    sys.exit(2)


# ── ciclo de vida seguro del data dir (finding A) ───────────────────────────


def safe_prepare_data_dir(raw):
    """Prepara el data dir del benchmark. Solo borra directorios acreditados
    por el sentinel; cualquier otro existente se rechaza sin tocarlo."""
    abs_path = os.path.abspath(raw)
    if os.path.islink(abs_path):
        refuse(f"el target es un symlink: {abs_path}")
    p = os.path.realpath(abs_path)
    home = os.path.realpath(os.path.expanduser("~"))
    cwd = os.path.realpath(os.getcwd())
    if p == os.sep or p == home:
        refuse(f"target inseguro (raíz o home): {p}")
    if p == cwd or cwd.startswith(p + os.sep):
        refuse(f"el target contiene el checkout: {p}")
    if p in SYSTEM_DENY or any(p.startswith(d + os.sep) for d in SYSTEM_DENY):
        refuse(f"target dentro de un directorio del sistema: {p}")
    if len([c for c in p.split(os.sep) if c]) < 2:
        refuse(f"target demasiado superficial para ser descartable: {p}")
    if os.path.exists(p):
        if not os.path.isdir(p):
            refuse(f"el target existe y no es un directorio: {p}")
        if not os.path.exists(os.path.join(p, SENTINEL)):
            refuse(
                f"el directorio existe y NO lleva el sentinel {SENTINEL} "
                f"(no pertenece a este benchmark): {p}; no se borra ni se "
                f"sobreescribe. Usá otro --data-dir o removelo manualmente."
            )
        shutil.rmtree(p)
    os.makedirs(p)
    with open(os.path.join(p, SENTINEL), "w") as f:
        f.write(f"aeril bench two-run\nowner_pid={os.getpid()}\ncreated_ms={int(time.time()*1000)}\n")
    return p


# ── identidad de procesos (finding B) ───────────────────────────────────────


def read_starttime(pid):
    """Campo 22 de /proc/<pid>/stat (starttime en ticks). None si no se puede."""
    try:
        with open(f"/proc/{pid}/stat", "rb") as f:
            data = f.read()
        tail = data[data.rfind(b")") + 2 :].split()
        return int(tail[19])  # estado = campo 3 ⇒ starttime = índice 19
    except (OSError, IndexError, ValueError):
        return None


def read_cmdline(pid):
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as f:
            return f.read().replace(b"\0", b" ").decode(errors="replace").strip()
    except OSError:
        return None


def read_exe(pid):
    try:
        return os.readlink(f"/proc/{pid}/exe")
    except OSError:
        return None


def capture_identity(pid, expected_start=None, require_mock=True):
    """Identidad verificable AHORA: starttime + cmdline (+ exe informativo).

    Devuelve None si el proceso no existe, no se puede leer, no es un mock
    (`__mock-agent` ausente) o el starttime no coincide con el esperado.
    """
    start = read_starttime(pid)
    cmd = read_cmdline(pid)
    if start is None or cmd is None:
        return None
    if require_mock and "__mock-agent" not in cmd:
        return None
    if expected_start is not None and start != expected_start:
        return None
    return {"pid": pid, "starttime": start, "cmdline": cmd, "exe": read_exe(pid)}


def terminate_verified(candidate):
    """Señala SIGKILL SOLO si la identidad se verifica (starttime + mock).

    Usa pidfd como handle estable: verificación → pidfd_open → re-verificación
    → señal por pidfd (cierra la carrera de reutilización de PID entre la
    verificación y el envío). Sin pidfd disponible, cae a verificar-y-kill
    inmediato (ventana residual mínima, documentada). Nunca señala PIDs no
    verificados.
    """
    pid = candidate["pid"]
    expected = candidate.get("starttime")
    if not os.path.exists(f"/proc/{pid}"):
        return {"pid": pid, "result": "already_dead"}
    if capture_identity(pid, expected_start=expected) is None:
        return {
            "pid": pid,
            "result": "uncertain",
            "reason": "identidad no verificable (starttime/cmdline no coinciden)",
        }
    fd = None
    try:
        if hasattr(os, "pidfd_open") and hasattr(signal, "pidfd_send_signal"):
            fd = os.pidfd_open(pid, 0)
            if capture_identity(pid, expected_start=expected) is None:
                return {
                    "pid": pid,
                    "result": "uncertain",
                    "reason": "identidad cambió entre verificación y pidfd",
                }
            signal.pidfd_send_signal(fd, signal.SIGKILL)
        else:
            os.kill(pid, signal.SIGKILL)
        return {"pid": pid, "result": "killed"}
    except ProcessLookupError:
        return {"pid": pid, "result": "already_dead"}
    except OSError as e:
        return {"pid": pid, "result": "uncertain", "reason": f"señal falló: {e}"}
    finally:
        if fd is not None:
            os.close(fd)


# ── utilidades de medición ──────────────────────────────────────────────────


def host_info(data_dir):
    info = {"os": "linux", "arch": "x86_64"}
    try:
        with open("/proc/cpuinfo") as f:
            for line in f:
                if line.startswith("model name"):
                    info["cpu"] = line.split(":", 1)[1].strip()
                    break
        with open("/proc/meminfo") as f:
            for line in f:
                if line.startswith("MemTotal"):
                    info["ram_gb"] = round(int(line.split()[1]) / 1024 / 1024, 1)
                    break
        out = subprocess.run(
            ["df", "--output=source,fstype", os.path.abspath(data_dir)],
            capture_output=True,
            text=True,
        ).stdout.splitlines()
        if len(out) >= 2:
            parts = out[-1].split()
            info["storage"] = parts[0]
            info["storage_fstype"] = parts[1]
    except Exception as e:  # noqa: BLE001
        info["host_info_error"] = str(e)
    return info


def read_rss_kb(pid):
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS"):
                    return int(line.split()[1])
    except OSError:
        return None
    return None


def sha256_of(path):
    try:
        h = hashlib.sha256()
        with open(path, "rb") as f:
            for chunk in iter(lambda: f.read(1024 * 1024), b""):
                h.update(chunk)
        return h.hexdigest()
    except OSError:
        return None


def read_beacon(path):
    if not os.path.exists(path):
        return []
    with open(path) as f:
        return [json.loads(line) for line in f if line.strip()]


def drain(fd, seconds):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        r, _, _ = pty.select([fd], [], [], 0.02)
        if r:
            try:
                os.read(fd, 65536)
            except OSError:
                return
        time.sleep(0.002)


def spawn_tui(bin_path, data_dir, beacon=None, backend=None):
    env = dict(os.environ)
    env["AERIL_DATA_DIR"] = data_dir
    env["TERM"] = "xterm-256color"
    if beacon:
        env["AERIL_BENCH_BEACON"] = beacon
    args = [bin_path]
    if backend:
        args += ["--backend", backend]
    pid, fd = pty.fork()
    if pid == 0:
        os.environ.update(env)
        try:
            os.execv(bin_path, args)
        except Exception:
            os._exit(127)
    return pid, fd


def wait_stage(beacon, stage, timeout, fd):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        for line in read_beacon(beacon):
            if line.get("stage") == stage:
                return line
        drain(fd, 0.05)
    return None


def sql(db_path, query, params=()):
    con = sqlite3.connect(db_path)
    try:
        return con.execute(query, params).fetchall()
    finally:
        con.close()


def runs_running(db_path):
    return sql(
        db_path,
        "SELECT r.id, r.thread_id, r.workspace_id FROM runs r "
        "WHERE r.state = 'running' ORDER BY r.started_ms",
    )


def backend_payloads(db_path, run_ids):
    """Payloads `backend.spawned` de LOS runs de esta invocación (finding B#1)."""
    out = []
    for (payload,) in sql(
        db_path,
        "SELECT payload FROM events WHERE kind = 'backend.spawned'",
    ):
        try:
            p = json.loads(payload)
        except Exception:  # noqa: BLE001
            continue
        if p.get("run_id") in run_ids and p.get("pid"):
            out.append(p)
    return out


def wait_db(fn, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if fn():
            return True
        time.sleep(0.05)
    return False


class Invalid(Exception):
    """Medición inválida: el reporte sale con ok=false y código 1."""


def fail(report, msg):
    report["ok"] = False
    report["error"] = msg
    print(f"two_run: medición INVÁLIDA: {msg}", file=sys.stderr)
    return report


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--data-dir", required=True)
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()

    data_dir = safe_prepare_data_dir(args.data_dir)
    db_path = os.path.join(data_dir, "aeril.db")
    repo = os.path.join(data_dir, "repo")
    os.makedirs(repo)
    run = lambda *a, **k: subprocess.run(a, check=True, capture_output=True, **k)  # noqa: E731
    run("git", "init", "-q", "-b", "main", repo)
    run("git", "-C", repo, "config", "user.email", "bench@aeril")
    run("git", "-C", repo, "config", "user.name", "bench")
    with open(os.path.join(repo, "NOTES.md"), "w") as f:
        f.write("# Notas\n\nVersión original.\n")
    run("git", "-C", repo, "add", "-A")
    run("git", "-C", repo, "commit", "-q", "-m", "base")

    env = dict(os.environ)
    env["AERIL_DATA_DIR"] = data_dir

    def cli(*a):
        return subprocess.run(
            [args.bin, *a], check=True, capture_output=True, text=True, env=env
        )

    threads = {}
    for title in ("two-b", "two-a"):  # el último creado queda primero
        out = json.loads(cli("thread", "new", "--repo", repo, "--title", title).stdout)
        threads[title] = out["thread_id"]
        time.sleep(0.05)

    script = os.path.join(data_dir, "mock.json")
    with open(script, "w") as f:
        json.dump(
            {
                "message": "trabajando",
                "final_message": "listo",
                "delay_ms": 20000,
                "edits": [
                    {
                        "op": "write",
                        "path": "NOTES.md",
                        "content": "# hecha por {cwd_basename}\n",
                    }
                ],
            },
            f,
        )
    backend = f"mock:{script}"

    report = {
        "host": host_info(data_dir),
        "method": "pty real + beacons + sqlite; limpieza identity-aware (pidfd)",
        "bin_path": os.path.abspath(args.bin),
        "bin_sha256": sha256_of(args.bin),
    }
    # Runs/backends de ESTA invocación (para asociación y limpieza).
    known_run_ids = []
    identities = {}  # pid -> identidad capturada mientras estaba vivo
    pid1 = None
    fd1 = None
    cleanup = {"killed": [], "already_dead": [], "uncertain": []}
    exit_code = 1
    try:
        beacon1 = os.path.join(data_dir, ".beacon-tui1")
        pid1, fd1 = spawn_tui(args.bin, data_dir, beacon=beacon1, backend=backend)
        if wait_stage(beacon1, "first_draw", 10, fd1) is None:
            raise Invalid("TUI1 no dibujó el primer frame")
        # Abrir A (selección inicial) y lanzar el run.
        os.write(fd1, KEY_ENTER)
        drain(fd1, 0.2)
        os.write(fd1, b"n")
        drain(fd1, 0.2)
        os.write(fd1, b"tarea A")
        os.write(fd1, KEY_ENTER)
        if not wait_db(lambda: len(runs_running(db_path)) == 1, 15):
            raise Invalid("el run A no arrancó")
        known_run_ids = [r[0] for r in runs_running(db_path)]
        # Ir a B y lanzar el segundo run.
        os.write(fd1, KEY_ESC)
        drain(fd1, 0.2)
        os.write(fd1, KEY_DOWN)
        drain(fd1, 0.2)
        os.write(fd1, KEY_ENTER)
        drain(fd1, 0.2)
        os.write(fd1, b"n")
        drain(fd1, 0.2)
        os.write(fd1, b"tarea B")
        os.write(fd1, KEY_ENTER)
        if not wait_db(lambda: len(runs_running(db_path)) == 2, 15):
            raise Invalid("el run B no arrancó")
        known_run_ids = [r[0] for r in runs_running(db_path)]
        drain(fd1, 1.0)  # dejar estabilizar RSS

        # ── validación de integridad (finding D) ────────────────────────────
        runs = runs_running(db_path)
        if len(runs) != 2:
            raise Invalid(f"se esperaban 2 runs activos, hay {len(runs)}")
        run_ids = [r[0] for r in runs]
        workspaces = [r[2] for r in runs]
        if len(set(run_ids)) != 2 or len(set(workspaces)) != 2 or None in workspaces:
            raise Invalid(f"runs/workspaces no distintos: {runs}")
        payloads = backend_payloads(db_path, set(run_ids))
        pids = sorted({int(p["pid"]) for p in payloads})
        if len(pids) != 2:
            raise Invalid(f"se esperaban 2 backends distintos, hay {pids}")
        for p in payloads:
            pid = int(p["pid"])
            expected = int(p.get("pid_start") or 0) or None
            ident = capture_identity(pid, expected_start=expected)
            if ident is None:
                raise Invalid(
                    f"identidad del backend {pid} no verificable (pid_start={expected})"
                )
            identities[pid] = ident
        try:
            os.kill(pid1, 0)
        except OSError:
            raise Invalid("la TUI no está viva")

        tui_rss = read_rss_kb(pid1)
        if not tui_rss or tui_rss <= 0:
            raise Invalid("RSS de la TUI inválido")
        mock_rss = {}
        for pid in pids:
            rss = read_rss_kb(pid)
            if not rss or rss <= 0:
                raise Invalid(f"RSS del backend {pid} inválido")
            mock_rss[str(pid)] = rss
        combined = tui_rss + sum(mock_rss.values())

        # Arranque de una SEGUNDA TUI con dos runs activos en el store.
        beacon2 = os.path.join(data_dir, ".beacon-tui2")
        pid2, fd2 = spawn_tui(args.bin, data_dir, beacon=beacon2)
        try:
            if wait_stage(beacon2, "first_draw", 10, fd2) is None:
                raise Invalid("TUI2 no dibujó el primer frame")
            os.write(fd2, b"r")  # primera tecla procesada
            ev = wait_stage(beacon2, "input_processed", 10, fd2)
            if ev is None:
                raise Invalid("TUI2 sin input_processed")
            startup_active_ms = ev["internal_us"] / 1000.0
        finally:
            try:
                os.kill(pid2, signal.SIGKILL)
                os.waitpid(pid2, 0)
            except OSError:
                pass
            try:
                os.close(fd2)
            except OSError:
                pass
        # La medición de arranque debe haber sido con 2 runs todavía activos.
        if len(runs_running(db_path)) != 2:
            raise Invalid("los runs cambiaron durante la medición de arranque")

        # Latencia de consultas con dos runs activos.
        q = []
        for _ in range(5):
            for args_q in (["run", "list", "--active"], ["workspace", "list"]):
                t = time.perf_counter()
                cli(*args_q)
                q.append((time.perf_counter() - t) * 1000.0)

        report.update(
            {
                "ok": True,
                "runs_active": 2,
                "run_ids": run_ids,
                "workspaces": workspaces,
                "backend_pids": pids,
                "backend_identity": {str(k): v for k, v in identities.items()},
                "tui_rss_kb": tui_rss,
                "mock_rss_kb": mock_rss,
                "combined_rss_kb": combined,
                "startup_two_active_ms": round(startup_active_ms, 2),
                "query_latency_ms": {
                    "max": round(max(q), 2),
                    "avg": round(sum(q) / len(q), 2),
                    "samples": len(q),
                },
            }
        )
        exit_code = 0
    except Invalid as e:
        fail(report, str(e))
        exit_code = 1
    except Exception as e:  # noqa: BLE001
        fail(report, f"error inesperado: {e}")
        exit_code = 1
    finally:
        cleanup = cleanup_processes(pid1, fd1, db_path, known_run_ids, identities)
    report["cleanup"] = cleanup
    if exit_code == 0 and cleanup["uncertain"]:
        print(
            f"two_run: advertencia: limpieza con incertidumbre: {cleanup['uncertain']}",
            file=sys.stderr,
        )
        exit_code = 3
    return finish(report, cleanup, exit_code)


def cleanup_processes(pid1, fd1, db_path, known_run_ids, identities):
    """Limpieza identity-aware: primero el hijo TUI (propio), luego una gracia
    para la salida normal de los backends, y solo después verificación +
    pidfd-señal de los PIDs asociados a los runs de esta invocación."""
    result = {"killed": [], "already_dead": [], "uncertain": []}
    if pid1 is not None:
        try:
            os.kill(pid1, signal.SIGKILL)
            os.waitpid(pid1, 0)  # hijo propio: reapear siempre
        except OSError:
            pass
    if fd1 is not None:
        try:
            os.close(fd1)
        except OSError:
            pass
    # Gracia: al morir la TUI, los mocks detectan el cierre de stdio y salen
    # solos; la mayoría no requiere señal.
    time.sleep(1.5)
    candidates = []
    if db_path and os.path.exists(db_path):
        try:
            for p in backend_payloads(db_path, set(known_run_ids)):
                pid = int(p["pid"])
                ident = identities.get(pid)
                candidates.append(
                    {
                        "pid": pid,
                        # Preferir el starttime capturado en vivo; si no, el
                        # persistido por Aeril (backend.spawned.pid_start).
                        "starttime": (ident or {}).get("starttime")
                        or (int(p.get("pid_start") or 0) or None),
                    }
                )
        except sqlite3.Error:
            pass
    for cand in candidates:
        outcome = terminate_verified(cand)
        result[outcome["result"]].append(outcome)
    return result


def finish(report, cleanup, exit_code):
    report.setdefault("cleanup", cleanup)
    if "ok" not in report:
        report["ok"] = False
    if exit_code == 0 and not report["ok"]:
        exit_code = 1
    print(json.dumps(report, indent=2))
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
