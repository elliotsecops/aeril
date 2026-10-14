#!/usr/bin/env python3
"""Repro/medición B1: amplificación de memoria del pipeline ACP.

Lanza `aeril run start` con un backend mock que emite `frames` notificaciones
válidas de `frame_bytes` mientras muestrea el RSS del proceso Aeril.
Reporta: bytes totales de entrada, RSS base, RSS pico y delta.

Uso: b1_mem_probe.py <bin> --frames N --frame-bytes B [--inflight-bytes B]
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time


def sh(*args, cwd=None, env=None):
    return subprocess.run(args, cwd=cwd, env=env, capture_output=True, text=True)


def rss_kb(pid):
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS"):
                    return int(line.split()[1])
    except OSError:
        return None
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("bin")
    ap.add_argument("--frames", type=int, default=160)
    ap.add_argument("--frame-bytes", type=int, default=300_000)
    ap.add_argument("--inflight-bytes", type=int, default=None)
    ap.add_argument("--max-line-bytes", type=int, default=None)
    args = ap.parse_args()

    root = tempfile.mkdtemp(prefix="rc2-b1-")
    repo = os.path.join(root, "repo")
    data = os.path.join(root, "data")
    os.makedirs(repo)
    os.makedirs(data)
    sh("git", "init", "-q", "-b", "main", repo)
    sh("git", "-C", repo, "config", "user.email", "t@t")
    sh("git", "-C", repo, "config", "user.name", "t")
    with open(os.path.join(repo, "NOTES.md"), "w") as f:
        f.write("# Notas\n\nbase\n")
    sh("git", "-C", repo, "add", "-A")
    sh("git", "-C", repo, "commit", "-qm", "base")

    script = os.path.join(root, "flood.json")
    with open(script, "w") as f:
        json.dump(
            {
                "flood": {
                    "frames": args.frames,
                    "frame_bytes": args.frame_bytes,
                },
                "final_message": "fin",
                "edits": [],
            },
            f,
        )

    env = dict(os.environ)
    env["AERIL_DATA_DIR"] = data
    if args.inflight_bytes:
        env["AERIL_ACP_INFLIGHT_BYTES"] = str(args.inflight_bytes)
    if args.max_line_bytes:
        env["AERIL_ACP_MAX_LINE_BYTES"] = str(args.max_line_bytes)

    th = sh(args.bin, "thread", "new", "--repo", repo, "--title", "b1", env=env)
    thread_id = json.loads(th.stdout)["thread_id"]

    proc = subprocess.Popen(
        [
            args.bin, "run", "start", "--thread", thread_id, "--prompt", "x",
            "--backend", f"mock:{script}",
        ],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    base = None
    peak = 0
    t0 = time.monotonic()
    while proc.poll() is None and time.monotonic() - t0 < 120:
        r = rss_kb(proc.pid)
        if r:
            if base is None:
                base = r
            peak = max(peak, r)
        time.sleep(0.02)
    out, err = proc.communicate(timeout=30)
    total_bytes = args.frames * args.frame_bytes
    summary = None
    try:
        summary = json.loads(out)
    except Exception:
        pass
    report = {
        "input_bytes": total_bytes,
        "input_mb": round(total_bytes / 1e6, 1),
        "rss_base_mb": round(base / 1024, 1) if base else None,
        "rss_peak_mb": round(peak / 1024, 1),
        "rss_delta_mb": round((peak - base) / 1024, 1) if base else None,
        "state": (summary or {}).get("state"),
        "detail": (summary or {}).get("detail"),
        "inflight_budget_bytes": args.inflight_bytes,
        "exit": proc.returncode,
    }
    print(json.dumps(report, indent=2))
    shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
