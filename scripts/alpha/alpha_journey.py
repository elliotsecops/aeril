#!/usr/bin/env python3
"""Alpha journey — recorrido reproducible de la TUI desde una instalación limpia.

Modos:
  happy : crea tarea por CLI, elige agente por flag, corre, revisa, selecciona
          el checkpoint (Down x50 + Up), comenta, envía feedback, integra a main
          (requiere que `main` NO esté en el checkout; se usa una rama `dev`).
  naive : intenta el recorrido sin conocer la arquitectura: comenta sin
          seleccionar el checkpoint y documenta el footgun (el texto se
          interpreta como atajos; 'i' abre integración).

El fixture es desechable; el agente es mock (sin credenciales). El script no
toca procesos ajenos: solo termina la TUI que él mismo lanza.
"""

import argparse
import fcntl
import json
import os
import pty
import re
import signal
import struct
import subprocess
import sys
import termios
import time
import uuid
from pathlib import Path

COLS, ROWS = 180, 50


class Screen:
    def __init__(self, c, r):
        self.cols, self.rows = c, r
        self.buf = [[" "] * c for _ in range(r)]
        self.r = self.c = 0

    def put(self, ch):
        if self.r < self.rows and self.c < self.cols:
            self.buf[self.r][self.c] = ch
            self.c += 1

    def feed(self, data):
        i, n = 0, len(data)
        while i < n:
            ch = data[i]
            if ch == "\x1b":
                m = re.match(r"\x1b\[([0-9;?]*)([A-Za-z])", data[i:])
                if m:
                    p, cmd = m.group(1), m.group(2)
                    nums = [int(x) for x in p.split(";") if x.isdigit()]
                    if cmd == "H":
                        self.r = max(0, min((nums[0] - 1) if nums else 0, self.rows - 1))
                        self.c = max(0, min((nums[1] - 1) if len(nums) > 1 else 0, self.cols - 1))
                    elif cmd == "J" and (nums[0] if nums else 0) == 2:
                        self.buf = [[" "] * self.cols for _ in range(self.rows)]
                        self.r = self.c = 0
                    elif cmd == "K":
                        for x in range(self.c, self.cols):
                            self.buf[self.r][x] = " "
                    i += m.end()
                    continue
                m2 = re.match(r"\x1b\][^\x07\x1b]*(\x07|\x1b\\)", data[i:])
                if m2:
                    i += m2.end()
                    continue
                i += 1
                continue
            if ch == "\n":
                self.r = min(self.rows - 1, self.r + 1)
            elif ch == "\r":
                self.c = 0
            elif ch >= " ":
                self.put(ch)
            i += 1

    def text(self):
        return "\n".join("".join(r).rstrip() for r in self.buf).rstrip("\n")


class Journey:
    def __init__(self, binary, mode):
        self.bin = str(Path(binary).resolve())
        self.mode = mode
        self.root = Path("/tmp/opencode") / f"alpha-{mode}-{uuid.uuid4().hex[:8]}"
        self.repo, self.data, self.shots = self.root / "proyecto", self.root / "data", self.root / "shots"
        self.proc = self.fd = None
        self.raw = b""
        self.steps = []

    def sanitize(self, t):
        t = re.sub(r"/home/[^\s'\"]*", "<path>", t)
        t = re.sub(r"/tmp/opencode[^\s'\"]*", "<fixture>", t)
        t = re.sub(r"[\w.+-]+@[\w.-]+\.\w+", "<email>", t)
        return re.sub(r"\b(gh[opusr]_[A-Za-z0-9]{6,}|sk-[A-Za-z0-9_-]{6,})\b", "<token>", t)

    def pump(self, sec=0.5):
        end = time.time() + sec
        while time.time() < end:
            try:
                os.set_blocking(self.fd, False)
                chunk = os.read(self.fd, 65536)
                if chunk:
                    self.raw += chunk
            except (BlockingIOError, OSError):
                time.sleep(0.04)

    def screen(self):
        s = Screen(COLS, ROWS)
        s.feed(self.raw.decode("utf-8", errors="replace"))
        return s.text()

    def shot(self, name, note, keys=""):
        self.pump(0.35)
        text = self.sanitize(self.screen())
        self.shots.mkdir(parents=True, exist_ok=True)
        (self.shots / f"{name}.txt").write_text(text + "\n")
        self.steps.append({"step": name, "note": note, "keys": keys, "screen": text})
        return text

    def send(self, k, wait=0.6):
        os.write(self.fd, k if isinstance(k, bytes) else k.encode())
        self.pump(wait)

    def wait_for(self, needle, timeout=30):
        end = time.time() + timeout
        while time.time() < end:
            self.pump(0.3)
            if needle.lower() in self.screen().lower():
                return True
        return False

    def launch(self, args):
        self.raw = b""
        pid, master = pty.fork()
        if pid == 0:
            env = dict(os.environ)
            env.update({"AERIL_DATA_DIR": str(self.data), "TERM": "xterm-256color"})
            os.chdir(self.repo)
            os.execve(self.bin, [self.bin] + args, env)
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        self.proc, self.fd = pid, master

    def stop(self):
        if self.proc:
            try:
                os.kill(self.proc, signal.SIGTERM)
            except OSError:
                pass
            try:
                os.waitpid(self.proc, 0)
            except OSError:
                pass
            try:
                os.close(self.fd)
            except OSError:
                pass
            self.proc = self.fd = None

    def cli(self, args):
        env = dict(os.environ)
        env["AERIL_DATA_DIR"] = str(self.data)
        return subprocess.run([self.bin] + args, cwd=self.root, env=env,
                              capture_output=True, text=True)

    def run(self):
        for d in (self.root, self.data, self.repo, self.shots):
            d.mkdir(parents=True, exist_ok=True)
        git = ["git", "-C", str(self.repo)]
        subprocess.run(["git", "init", "-q", "-b", "main", str(self.repo)], check=True)
        subprocess.run(git + ["config", "user.email", "demo@example.test"], check=True)
        subprocess.run(git + ["config", "user.name", "Demo"], check=True)
        (self.repo / "NOTES.md").write_text("# Notas del proyecto\n\nEstado inicial.\n")
        subprocess.run(git + ["add", "-A"], check=True)
        subprocess.run(git + ["commit", "-qm", "base"], check=True)
        if self.mode == "happy":
            # Libera `main` para poder integrar desde la TUI (hallazgo A5).
            subprocess.run(git + ["checkout", "-qb", "dev"], check=True)

        th = self.cli(["thread", "new", "--repo", str(self.repo), "--title", "Demo alpha"])
        thread_id = json.loads(th.stdout)["thread_id"]
        script = self.root / "mock.json"
        script.write_text(json.dumps({
            "message": "trabajando",
            "final_message": "listo",
            "delay_ms": 2500,
            "edits": [{"op": "append", "path": "NOTES.md", "content": "\n- avance del agente\n"}],
        }))
        self.launch(["--backend", f"mock:{script}"])
        assert self.wait_for("Demo alpha", 20), "la tarea no apareció"
        self.send("\r")
        self.shot("01-task-open", "Tarea abierta (Enter)")

        self.send("n")
        self.send("revisá el repo y actualizá NOTES.md")
        self.send("\r", 0.6)
        self.shot("02-working", "Estado 'está trabajando'")
        self.wait_for("completed", 40)
        self.shot("03-completed", "Estado 'terminó con este resultado'")

        self.send("\t")
        if self.mode == "happy":
            self.send("\x1b[B" * 50)  # Down: satura en el último evento (end)
            self.send("\x1b[A")      # Up: checkpoint.ready
        else:
            self.send("\x1b[B")      # naive: mueve sin garantizar checkpoint
        self.shot("04-review", "Vista de review / checkpoint seleccionado", "Tab")

        self.send("c")
        self.send("el cambio se ve bien, falta contexto")
        self.shot("05-comment-input", "Intento de comentario", "c + texto")
        self.send("\r")
        self.shot("06-comment-result", "Resultado del intento de comentario")

        self.send("f", 1.0)
        self.shot("07-feedback", "Envío de feedback (f)")
        self.wait_for("completed", 45)
        self.shot("08-feedback-result", "Resultado del feedback")

        self.send("i")
        self.shot("09-integrate-input", "Entrada de integración (i)")
        self.send("\r", 1.0)
        self.shot("10-integrate-result", "Resultado de la integración")
        self.send("q", 0.8)
        self.stop()

        log = subprocess.run(git + ["log", "--oneline", "-4", "main"],
                             capture_output=True, text=True).stdout
        out = {
            "binary": self.bin,
            "mode": self.mode,
            "root": str(self.root),
            "thread_id": thread_id,
            "main_log": self.sanitize(log),
            "steps": self.steps,
        }
        (self.root / "steps.json").write_text(json.dumps(out, indent=2))
        print(json.dumps({k: out[k] for k in ("root", "mode", "main_log")}, indent=2))
        for s in self.steps:
            print(f"{s['step']:<24} {s['note']}")
        return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default="target/release/aeril")
    ap.add_argument("--mode", choices=["happy", "naive"], default="happy")
    args = ap.parse_args()
    j = Journey(args.bin, args.mode)
    try:
        j.run()
    finally:
        j.stop()
    return 0


if __name__ == "__main__":
    sys.exit(main())
