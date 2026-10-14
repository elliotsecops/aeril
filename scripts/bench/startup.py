#!/usr/bin/env python3
"""Medición externa del arranque, RSS idle y caché residente por thread.

Método (RC1 §4):
- Corre la TUI REAL en un pty con `AERIL_BENCH_BEACON`.
- T0 se toma en este proceso externo justo antes del fork.
- Endpoint de "UI utilizable": la primera entrada (Enter) ABRE un thread real
  y se renderiza su timeline + resúmenes de comentarios (`thread_opened`).
- `--mem`: mide RSS tras 5 s idle (sin abrir thread), luego abre el thread y
  vuelve a medir tras 3 s: la DIFERENCIA es la caché residente por thread
  (presupuesto: ≤ 10 MiB), medida como proxy de proceso completo.
- Cualquier muestra fallida ⇒ el gate FALLA (n_fail == 0 obligatorio).
- Condiciones declaradas: caché de páginas WARM del host (no se vacía la
  caché del SO) y fstype real del data dir (no se disfraza tmpfs de NVMe).

Uso:
  startup.py --bin target/release/aeril --data-dir bench-data/bench \
             [--runs 30] [--mem] [--json] [--max-ms 300] [--max-rss-mb 50] \
             [--max-delta-mib 10]
"""

import argparse
import json
import os
import pty
import signal
import statistics
import subprocess
import sys
import time

SAN_MB = 1024 * 1024


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


def read_beacon(path):
    if not os.path.exists(path):
        return []
    with open(path) as f:
        return [json.loads(line) for line in f if line.strip()]


def wait_stage(path, stage, timeout, fd):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        for line in read_beacon(path):
            if line.get("stage") == stage:
                return line
        if fd is not None:
            r, _, _ = pty.select([fd], [], [], 0.02)
            if r:
                try:
                    os.read(fd, 65536)
                except OSError:
                    pass
        time.sleep(0.002)
    return None


def read_rss_kb(pid):
    with open(f"/proc/{pid}/status") as f:
        for line in f:
            if line.startswith("VmRSS"):
                return int(line.split()[1])
    return None


def sample(bin_path, data_dir, timeout_s, mem_probe):
    beacon = os.path.join(data_dir, f".beacon-{os.getpid()}-{time.monotonic_ns()}")
    env = dict(os.environ)
    env["AERIL_DATA_DIR"] = data_dir
    env["AERIL_BENCH_BEACON"] = beacon

    t0 = time.perf_counter()
    pid, fd = pty.fork()
    if pid == 0:  # hijo
        os.environ.update(env)
        try:
            os.execv(bin_path, [bin_path])
        except Exception:
            os._exit(127)

    result = {"ok": False}
    try:
        if wait_stage(beacon, "first_draw", timeout_s, fd) is None:
            result["error"] = "no llegó first_draw"
            return result, pid
        if mem_probe:
            time.sleep(5.0)
            result["rss_idle_kb"] = read_rss_kb(pid)
        os.write(fd, b"\r")  # Enter: abre un thread real
        done = wait_stage(beacon, "thread_opened", timeout_s, fd)
        if done is None:
            result["error"] = "la entrada no abrió un thread (thread_opened ausente)"
            return result, pid
        result["external_ms"] = (time.perf_counter() - t0) * 1000.0
        result["internal_us"] = done.get("internal_us")
        if mem_probe:
            time.sleep(3.0)
            result["rss_thread_kb"] = read_rss_kb(pid)
        result["ok"] = True
        return result, pid
    finally:
        try:
            os.write(fd, b"q")
        except OSError:
            pass
        deadline = time.monotonic() + 3
        reaped = False
        while time.monotonic() < deadline:
            try:
                wpid, _status = os.waitpid(pid, os.WNOHANG)
                if wpid == pid:
                    reaped = True
                    break
            except ChildProcessError:
                reaped = True
                break
            time.sleep(0.02)
        if not reaped:
            try:
                os.kill(pid, signal.SIGKILL)
                os.waitpid(pid, 0)
            except OSError:
                pass
        try:
            os.unlink(beacon)
        except OSError:
            pass


def percentile(sorted_vals, p):
    if not sorted_vals:
        return None
    idx = max(0, min(len(sorted_vals) - 1, int(-(-len(sorted_vals) * p // 100)) - 1))
    return sorted_vals[idx]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--data-dir", required=True)
    ap.add_argument("--runs", type=int, default=30)
    ap.add_argument("--mem", action="store_true", help="mide RSS idle y con thread abierto")
    ap.add_argument("--json", action="store_true")
    ap.add_argument("--max-ms", type=float, default=300.0)
    ap.add_argument("--max-rss-mb", type=float, default=50.0)
    ap.add_argument("--max-delta-mib", type=float, default=10.0)
    ap.add_argument("--timeout", type=float, default=60.0)
    args = ap.parse_args()

    samples = []
    mem_run_ms = None
    rss_idle_kb = None
    rss_thread_kb = None
    failures = []
    runs = 1 if args.mem else args.runs
    for i in range(runs):
        res, _pid = sample(args.bin, args.data_dir, args.timeout, args.mem and i == 0)
        if res.get("ok"):
            if args.mem and i == 0:
                # En modo RSS la muestra incluye la espera deliberada de 5 s:
                # se reporta como dato, pero NO evalúa el presupuesto de arranque.
                mem_run_ms = res["external_ms"]
            else:
                samples.append(res["external_ms"])
            if "rss_idle_kb" in res:
                rss_idle_kb = res["rss_idle_kb"]
            if "rss_thread_kb" in res:
                rss_thread_kb = res["rss_thread_kb"]
        else:
            failures.append(res.get("error", "?"))

    samples_sorted = sorted(samples)
    delta_kb = (
        rss_thread_kb - rss_idle_kb
        if rss_idle_kb is not None and rss_thread_kb is not None
        else None
    )
    report = {
        "measurement": "arranque → entrada procesada que ABRE un thread (timeline + comentarios), externo, pty",
        "build_profile": "release (lto=thin, strip=symbols, codegen-units=1)",
        "dataset": "fixtures sintéticas del perfil v0.4 (100×1000×10, con comentarios)",
        "cache_condition": "warm page cache del host (no se vació la caché del SO)",
        "method": "spawn externo + beacons internos; p95 sobre muestras externas; n_fail debe ser 0",
        "host": host_info(args.data_dir),
        "n_ok": len(samples),
        "n_fail": len(failures),
        "failures": failures[:5],
        "samples_ms": [round(s, 2) for s in samples_sorted],
        "mem_run_ms_informativo": round(mem_run_ms, 2) if mem_run_ms else None,
        "p50_ms": round(statistics.median(samples_sorted), 2) if samples else None,
        "p95_ms": round(percentile(samples_sorted, 95), 2) if samples else None,
        "max_ms": round(max(samples_sorted), 2) if samples else None,
        "min_ms": round(min(samples_sorted), 2) if samples else None,
        "rss_idle_kb": rss_idle_kb,
        "rss_idle_mb": round(rss_idle_kb / 1024, 1) if rss_idle_kb else None,
        "rss_thread_open_kb": rss_thread_kb,
        "rss_thread_open_mb": round(rss_thread_kb / 1024, 1) if rss_thread_kb else None,
        "per_thread_delta_mib": round(delta_kb / 1024, 2) if delta_kb is not None else None,
        "budget_startup_ms": args.max_ms,
        "budget_rss_mb": args.max_rss_mb,
        "budget_delta_mib": args.max_delta_mib,
    }

    ok = True
    if args.mem:
        report["startup_verdict"] = "N/A (modo RSS: mide idle vs thread abierto)"
        if failures:
            ok = False
    elif samples and report["p95_ms"] <= args.max_ms and len(failures) == 0:
        report["startup_verdict"] = "PASS"
    else:
        ok = False
        report["startup_verdict"] = "FAIL"
    if rss_idle_kb is not None:
        report["rss_verdict"] = "PASS" if report["rss_idle_mb"] <= args.max_rss_mb else "FAIL"
        if report["rss_verdict"] == "FAIL":
            ok = False
    if delta_kb is not None:
        report["delta_verdict"] = (
            "PASS" if report["per_thread_delta_mib"] <= args.max_delta_mib else "FAIL"
        )
        if report["delta_verdict"] == "FAIL":
            ok = False

    if args.json:
        print(json.dumps(report, ensure_ascii=False, indent=2))
    else:
        print(
            f"arranque (abre thread): p50={report['p50_ms']}ms p95={report['p95_ms']}ms "
            f"max={report['max_ms']}ms ({report['n_ok']} ok / {report['n_fail']} fallos) "
            f"→ {report['startup_verdict']} (≤{args.max_ms}ms, 0 fallos)"
        )
        if rss_idle_kb is not None:
            print(
                f"RSS idle: {report['rss_idle_mb']} MB → {report['rss_verdict']} "
                f"(≤{args.max_rss_mb} MB)"
            )
        if delta_kb is not None:
            print(
                f"caché residente por thread: {report['per_thread_delta_mib']} MiB "
                f"→ {report['delta_verdict']} (≤{args.max_delta_mib} MiB)"
            )
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
