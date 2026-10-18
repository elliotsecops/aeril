#!/usr/bin/env python3
"""P2-S1 R3 targeted reproducer.

Runs against an externally supplied Aeril binary and the canonical fake
Hermes. It uses only disposable fixtures and never touches real profiles.

Usage:
    python3 scripts/phase2/r3_repro.py --bin target/debug/aeril
    python3 scripts/phase2/r3_repro.py --bin /tmp/r2-baseline/target/debug/aeril
"""

import argparse
import json
import os
import pty
import select
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path

FLAG = "AERIL_HERMES_PROFILES"


def proc_start(pid):
    try:
        with open(f"/proc/{int(pid)}/stat", "rb") as f:
            tail = f.read().rsplit(b")", 1)[1].split()
        return int(tail[19])
    except (OSError, IndexError, ValueError):
        return None


def kill_verified(pid, start=None):
    if not pid:
        return False
    if start is None:
        start = proc_start(pid)
    if start is None or proc_start(pid) != start:
        return False
    try:
        with open(f"/proc/{int(pid)}/cmdline", "rb") as f:
            cmd = f.read().replace(b"\0", b" ").decode(errors="replace")
    except OSError:
        return False
    if "hermes" not in cmd:
        return False
    try:
        os.kill(int(pid), signal.SIGKILL)
        return True
    except OSError:
        return False


class Fixture:
    def __init__(self, bin_path, fake_path, name):
        self.bin = Path(bin_path).resolve()
        self.root = Path(tempfile.mkdtemp(prefix=f"r3-{name}-"))
        self.data = self.root / "data"
        self.repo = self.root / "repo"
        self.fakebin = self.root / "fakebin"
        self.data.mkdir(parents=True)
        self.repo.mkdir()
        self.fakebin.mkdir()
        self.fake = self.fakebin / "hermes"
        shutil.copyfile(fake_path, self.fake)
        self.fake.chmod(0o755)
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "r3@aeril.local")
        self.git("config", "user.name", "R3")
        (self.repo / "NOTES.md").write_text("# notes\n")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "base")

    def git(self, *args):
        subprocess.run(
            ["git", "-C", str(self.repo), *args],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

    def env(self, on=True, extra=None):
        env = dict(os.environ)
        env["AERIL_DATA_DIR"] = str(self.data)
        env["PATH"] = f"{self.fakebin}:{env.get('PATH', '')}"
        env["AERIL_HERMES_BIN"] = str(self.fake)
        if on:
            env[FLAG] = "1"
        else:
            env.pop(FLAG, None)
        for key, value in (extra or {}).items():
            env[key] = value
        return env

    def aeril(self, args, on=True, extra=None, timeout=60):
        return subprocess.run(
            [str(self.bin), *args],
            env=self.env(on, extra),
            cwd=self.root,
            capture_output=True,
            text=True,
            timeout=timeout,
        )

    def thread_new(self):
        out = self.aeril(
            ["thread", "new", "--repo", str(self.repo), "--title", "r3"]
        )
        assert out.returncode == 0, out.stderr
        return json.loads(out.stdout)["thread_id"]

    def create(self, agent):
        out = self.aeril(["profile", "create", "--agent", agent])
        assert out.returncode == 0, out.stderr

    def assign(self, agent, thread):
        out = self.aeril(["profile", "assign", "--agent", agent, "--thread", thread])
        assert out.returncode == 0, out.stderr

    def set_opt(self, agent, name, value):
        (self.data / "hermes" / agent / "home" / name).write_text(value)

    def list_profiles(self):
        out = self.aeril(["profile", "list"])
        if out.returncode != 0:
            return []
        try:
            return json.loads(out.stdout)
        except Exception:
            return []

    def lock_state(self, agent):
        for row in self.list_profiles():
            if row.get("agent_id") == agent:
                return row.get("lock_state", "missing")
        return "missing"

    def agent_dir(self, agent):
        return self.data / "hermes" / agent

    def home(self, agent):
        return self.agent_dir(agent) / "home"

    def run_start(self, thread, extra=None):
        return self.aeril(
            [
                "run",
                "start",
                "--thread",
                thread,
                "--prompt",
                "x",
                "--backend",
                "hermes",
            ],
            extra=extra,
        )

    def cleanup(self):
        for pattern in (
            "fake-pid",
            "fake-escaped-pid",
            "fake-auth-descendant-pid",
        ):
            for path in self.data.rglob(pattern):
                try:
                    nums = [int(x) for x in path.read_text().split()]
                    for pid, start in zip(nums[0::2], nums[1::2]):
                        kill_verified(pid, start)
                except (OSError, ValueError):
                    continue
        shutil.rmtree(self.root, ignore_errors=True)


def mark_removing(fx, agent):
    path = fx.data / "hermes" / "agents.json"
    data = json.loads(path.read_text())
    data["agents"][agent]["removing"] = True
    path.write_text(json.dumps(data))


def report(name, verdict, detail):
    print(f"{name}: {verdict} — {detail}", flush=True)


def auth_cases(bin_path, fake_path):
    for name, mode, timeout_s in (
        ("auth-success-descendant", "auth-escape-clean", None),
        ("auth-timeout-descendant", "auth-timeout-escape", "1"),
        ("auth-malformed-descendant", "auth-malformed-escape", "1"),
    ):
        fx = Fixture(bin_path, fake_path, name)
        try:
            fx.create("a")
            fx.set_opt("a", "fake-auth-mode", mode)
            fx.set_opt("a", "fake-init-mode", "valid")
            thread = fx.thread_new()
            fx.assign("a", thread)
            extra = {}
            if timeout_s:
                extra["AERIL_PROFILE_AUTH_TIMEOUT_SECS"] = timeout_s
            out = fx.run_start(thread, extra)
            deadline = time.time() + 8
            while (
                not (fx.home("a") / "fake-auth-descendant-pid").exists()
                and time.time() < deadline
            ):
                time.sleep(0.05)
            state = fx.lock_state("a")
            if state == "held":
                report(name, "RED", f"auth descendant retained writer lock (run rc={out.returncode})")
            elif state == "free":
                report(name, "GREEN", f"auth descendant did not retain lock (run rc={out.returncode})")
            else:
                report(name, "CHECK", f"lock_state={state}")
        finally:
            fx.cleanup()


def tombstone_cases(bin_path, fake_path):
    fx = Fixture(bin_path, fake_path, "tombstone")
    try:
        fx.create("a")
        fx.set_opt("a", "fake-auth-mode", "positive")
        thread = fx.thread_new()
        fx.assign("a", thread)
        mark_removing(fx, "a")
        run = fx.run_start(thread)
        warm = fx.aeril(["profile", "warm", "--agent", "a"])
        admitted = run.returncode == 0
        warmed = warm.returncode == 0
        if admitted or warmed:
            report(
                "tombstone-blocks-use",
                "RED",
                f"run_admitted={admitted} warm_succeeded={warmed}",
            )
        else:
            report("tombstone-blocks-use", "GREEN", "run and warm rejected")
    finally:
        fx.cleanup()


def pump(master, seconds=0.2):
    deadline = time.time() + seconds
    while time.time() < deadline:
        ready, _, _ = select.select([master], [], [], 0.05)
        if ready:
            try:
                os.read(master, 8192)
            except OSError:
                return


def tui_owner_case(bin_path, fake_path):
    fx = Fixture(bin_path, fake_path, "tui-owner")
    master = None
    tui = None
    try:
        fx.create("a")
        fx.set_opt("a", "fake-auth-mode", "positive")
        fx.set_opt("a", "fake-init-mode", "valid")
        thread = fx.thread_new()
        fx.assign("a", thread)
        master, slave = pty.openpty()
        env = fx.env(True, {"AERIL_BACKEND": "hermes", "TERM": "xterm-256color"})
        tui = subprocess.Popen(
            [str(fx.bin)],
            env=env,
            cwd=fx.root,
            stdin=slave,
            stdout=slave,
            stderr=slave,
        )
        os.close(slave)
        pump(master, 1.5)
        os.write(master, b"\r")
        pump(master, 0.5)
        os.write(master, b"n")
        pump(master, 0.3)
        os.write(master, b"tui goal\r")
        deadline = time.time() + 20
        completed = False
        while time.time() < deadline:
            pump(master, 0.1)
            try:
                conn = sqlite3.connect(fx.data / "aeril.db")
                row = conn.execute(
                    "SELECT state FROM runs ORDER BY started_ms DESC LIMIT 1"
                ).fetchone()
                conn.close()
                if row and row[0] == "completed":
                    completed = True
                    break
            except sqlite3.Error:
                pass
        if not completed:
            report("tui-owner-cleanup", "CHECK", "TUI run did not complete")
            return
        deadline = time.time() + 6
        while fx.lock_state("a") != "free" and time.time() < deadline:
            time.sleep(0.05)
        warm = fx.aeril(["profile", "warm", "--agent", "a"])
        if warm.returncode == 0:
            report("tui-owner-cleanup", "GREEN", "live TUI did not block warm")
        else:
            report(
                "tui-owner-cleanup",
                "RED",
                f"live TUI left false HOLD: {warm.stderr.strip()[:200]}",
            )
    finally:
        if tui:
            tui.kill()
            tui.wait()
        if master is not None:
            try:
                os.close(master)
            except OSError:
                pass
        fx.cleanup()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin", required=True)
    parser.add_argument(
        "--fake",
        default=str(Path(__file__).resolve().parents[2] / "tests/common/fake_hermes.py"),
    )
    args = parser.parse_args()
    if not Path(args.bin).exists():
        raise SystemExit(f"binary not found: {args.bin}")
    auth_cases(args.bin, args.fake)
    tombstone_cases(args.bin, args.fake)
    tui_owner_case(args.bin, args.fake)


if __name__ == "__main__":
    main()
