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

Método: pty real + beacons + lectura del store con sqlite3 (stdlib). El data
dir es DESCARTABLE: se recrea en cada corrida.

Uso:
  two_run.py --bin target/release/aeril --data-dir bench-data/two-run [--json]
"""

import argparse
import json
import os
import pty
import shutil
import signal
import sqlite3
import subprocess
import sys
import time

KEY_ESC = b"\x1b"
KEY_DOWN = b"\x1b[B"
KEY_UP = b"\x1b[A"
KEY_ENTER = b"\r"


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


def wait_beacon(beacon, stage, timeout, fd):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        for line in read_beacon(beacon):
            if line.get("stage") == stage:
                return line
        drain(fd, 0.05)
    return None


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


def backend_pids(db_path):
    out = []
    for (payload,) in sql(
        db_path, "SELECT payload FROM events WHERE kind = 'backend.spawned'"
    ):
        try:
            p = json.loads(payload)
            if p.get("pid"):
                out.append(int(p["pid"]))
        except Exception:  # noqa: BLE001
            pass
    return out


def wait_db(fn, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if fn():
            return True
        time.sleep(0.05)
    return False


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--data-dir", required=True)
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()

    data_dir = os.path.abspath(args.data_dir)
    if os.path.exists(data_dir):
        shutil.rmtree(data_dir)
    os.makedirs(data_dir)
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
        out = json.loads(
            cli("thread", "new", "--repo", repo, "--title", title).stdout
        )
        threads[title] = out["thread_id"]
        time.sleep(0.05)
    order = [threads["two-a"], threads["two-b"]]

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

    report = {"host": host_info(data_dir), "method": "pty real + beacons + sqlite"}
    beacon1 = os.path.join(data_dir, ".beacon-tui1")
    pid1, fd1 = spawn_tui(args.bin, data_dir, beacon=beacon1, backend=backend)
    try:
        assert wait_stage(beacon1, "first_draw", 10, fd1), "TUI1 no dibujó"
        # Abrir A (selección inicial) y lanzar el run.
        os.write(fd1, KEY_ENTER)
        drain(fd1, 0.2)
        os.write(fd1, b"n")
        drain(fd1, 0.2)
        os.write(fd1, b"tarea A")
        os.write(fd1, KEY_ENTER)
        assert wait_db(lambda: len(runs_running(db_path)) == 1, 15), "A no arrancó"
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
        assert wait_db(lambda: len(runs_running(db_path)) == 2, 15), "B no arrancó"
        drain(fd1, 1.0)  # dejar estabilizar RSS

        tui_rss = read_rss_kb(pid1)
        mocks = backend_pids(db_path)
        mock_rss = {str(p): read_rss_kb(p) for p in mocks}
        combined = tui_rss + sum(v or 0 for v in mock_rss.values())

        # Arranque de una SEGUNDA TUI con dos runs activos en el store.
        beacon2 = os.path.join(data_dir, ".beacon-tui2")
        pid2, fd2 = spawn_tui(args.bin, data_dir, beacon=beacon2)
        try:
            assert wait_stage(beacon2, "first_draw", 10, fd2), "TUI2 no dibujó"
            os.write(fd2, b"r")  # primera tecla procesada
            ev = wait_stage(beacon2, "input_processed", 10, fd2)
            assert ev is not None, "TUI2 sin input_processed"
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
    finally:
        for p in backend_pids(db_path):
            try:
                os.kill(p, signal.SIGKILL)
            except OSError:
                pass
        try:
            os.kill(pid1, signal.SIGKILL)
            os.waitpid(pid1, 0)
        except OSError:
            pass
        try:
            os.close(fd1)
        except OSError:
            pass

    if args.json:
        print(json.dumps(report, indent=2))
    else:
        print(report)
    return 0 if report.get("ok") else 1


if __name__ == "__main__":
    sys.exit(main())
