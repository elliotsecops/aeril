#!/usr/bin/env python3
"""Medición externa de arranque (y RAM idle) de la TUI de Aeril.

Método (v0.4 §6 de HANDOFF):
- Corre la TUI REAL en un pty, con `AERIL_BENCH_BEACON` apuntando a un archivo.
- T0 se toma en este proceso EXTERNO justo antes del fork.
- La app escribe `first_draw` cuando terminó el primer frame y
  `input_processed` cuando procesó la primera tecla (flecha abajo), que es la
  definición acordada de "UI utilizable" (entrada procesada, no frame vacío).
- p95 sobre las muestras externas; se reporta también el tiempo interno.

Condición de caché: se etiqueta explícitamente. Por defecto los samples son
con page cache WARM del host (no se vacía la caché del SO: sería una operación
disruptiva que requiere autorización). Con `--cold` se etiqueta "cold-requested"
pero NO se ejecuta ningún drop de cachés: el reporte queda como no verificado.

Uso:
  startup.py --bin target/release/aeril --data-dir bench-data/bench \
             [--runs 30] [--mem] [--json] [--max-ms 300] [--max-rss-mb 50]
"""

import argparse
import json
import os
import pty
import select
import signal
import statistics
import sys
import time

SAN_MB = 1024 * 1024


def host_info():
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
        import subprocess

        dev = subprocess.run(
            ["df", "--output=source", os.path.abspath(".")],
            capture_output=True,
            text=True,
        ).stdout.splitlines()[-1]
        info["storage"] = dev
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
        # Drenar salida del pty para que la app no se bloquee escribiendo.
        if fd is not None:
            r, _, _ = select.select([fd], [], [], 0.02)
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


def sample(bin_path, data_dir, timeout_s, idle_probe):
    beacon = os.path.join(data_dir, f".beacon-{os.getpid()}-{time.monotonic_ns()}")
    if os.path.exists(beacon):
        os.unlink(beacon)
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
        first = wait_stage(beacon, "first_draw", timeout_s, fd)
        if first is None:
            result["error"] = "no llegó first_draw"
            return result, pid
        os.write(fd, b"\x1b[B")  # flecha abajo: entrada
        done = wait_stage(beacon, "input_processed", timeout_s, fd)
        if done is None:
            result["error"] = "no procesó la entrada"
            return result, pid
        result["external_ms"] = (time.perf_counter() - t0) * 1000.0
        result["internal_us"] = done.get("internal_us")
        if idle_probe:
            time.sleep(5.0)
            rss = read_rss_kb(pid)
            result["rss_kb"] = rss
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
                wpid, status = os.waitpid(pid, os.WNOHANG)
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
    ap.add_argument("--mem", action="store_true", help="mide RSS tras 5 s idle")
    ap.add_argument("--json", action="store_true")
    ap.add_argument("--max-ms", type=float, default=300.0)
    ap.add_argument("--max-rss-mb", type=float, default=50.0)
    ap.add_argument("--timeout", type=float, default=30.0)
    args = ap.parse_args()

    samples = []
    rss_kb = None
    failures = []
    for i in range(args.runs):
        res, _pid = sample(args.bin, args.data_dir, args.timeout, args.mem and i == 0)
        if res.get("ok"):
            samples.append(res["external_ms"])
            if "rss_kb" in res:
                rss_kb = res["rss_kb"]
        else:
            failures.append(res.get("error", "?"))
        if args.mem and rss_kb is not None and i == 0:
            break  # con --mem alcanza una muestra

    samples_sorted = sorted(samples)
    report = {
        "measurement": "startup-to-useful-ui (external, pty, tecla procesada)",
        "build_profile": "release (lto=thin, strip=symbols, codegen-units=1)",
        "dataset": "fixtures sintéticas del perfil v0.4 (ver dataset.json)",
        "cache_condition": "warm page cache del host (no se vació caché del SO)",
        "method": "spawn externo + beacon interno; p95 sobre muestras externas",
        "host": host_info(),
        "n_ok": len(samples),
        "n_fail": len(failures),
        "failures": failures[:3],
        "p50_ms": round(statistics.median(samples_sorted), 2) if samples else None,
        "p95_ms": round(percentile(samples_sorted, 95), 2) if samples else None,
        "max_ms": round(max(samples_sorted), 2) if samples else None,
        "min_ms": round(min(samples_sorted), 2) if samples else None,
        "rss_kb": rss_kb,
        "rss_mb": round(rss_kb / 1024, 1) if rss_kb else None,
        "budget_startup_ms": args.max_ms,
        "budget_rss_mb": args.max_rss_mb,
    }
    ok = True
    if samples:
        if report["p95_ms"] > args.max_ms:
            ok = False
            report["startup_verdict"] = "FAIL"
        else:
            report["startup_verdict"] = "PASS"
    else:
        ok = False
        report["startup_verdict"] = "FAIL (sin muestras)"
    if rss_kb is not None:
        report["rss_verdict"] = "PASS" if report["rss_mb"] <= args.max_rss_mb else "FAIL"
        if report["rss_verdict"] == "FAIL":
            ok = False

    if args.json:
        print(json.dumps(report, ensure_ascii=False, indent=2))
    else:
        print(
            f"arranque: p50={report['p50_ms']}ms p95={report['p95_ms']}ms "
            f"max={report['max_ms']}ms ({report['n_ok']}/{args.runs} ok) "
            f"→ {report['startup_verdict']} (≤{args.max_ms}ms)"
        )
        if rss_kb is not None:
            print(
                f"RSS idle (5 s): {report['rss_mb']} MB → {report['rss_verdict']} "
                f"(≤{args.max_rss_mb} MB)"
            )
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
