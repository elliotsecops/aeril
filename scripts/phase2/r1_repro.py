#!/usr/bin/env python3
"""P2-S1 R1 — reproducciones RED/GREEN de los 4 P1 (hallazgos de auditoría).

Fixtures 100% descartables: directorios temporales, Hermes fake controlado
por el auditor. Sin credenciales reales, sin tocar perfiles P2-S0, sin tocar
el Hermes del host.

Uso:
    python3 scripts/phase2/r1_repro.py --bin target/debug/aeril [--only P1-1]

Salida: una línea por escenario con el resultado observado y el veredicto
(RED = el candidato auditado viola el invariante; GREEN = fail-closed).

Escenarios:
  P1-1a  remove --clear-threads con run durable no-terminal (supervisor+backend
         muertos por SIGKILL) → debe DENEGAR.
  P1-1b  unassign durante run no-terminal + remove sin --clear-threads → debe
         DENEGAR en algún paso (no perder el vínculo durable).
  P1-1c  DB inaccesible (chmod 000) + remove → debe DENEGAR (fail-closed).
  P1-2   auth status: matriz adversarial (exit 1 con "logged in", negaciones,
         otro provider, contradictorio, vacío/malformado) → solo el positivo
         inequívoco con exit 0 puede ser logged_in.
  P1-3   warm: initialize adversarial (notificación con result anidado, id
         incorrecto, result null, error JSON-RPC, frame >1 MiB, EOF a mitad,
         notificaciones antes del válido, stream infinito) → solo el válido
         estricto puede aceptarse; el resto fail-closed.
  P1-4   supervisor SIGKILL con child sobreviviente → warm/run/remove deben
         DENEGAR mientras el child viva; tras matar al child y reconciliar,
         un writer nuevo legítimo debe poder arrancar.
"""

import argparse
import json
import os
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def read_starttime(pid):
    try:
        with open(f"/proc/{int(pid)}/stat", "rb") as f:
            tail = f.read().rsplit(b")", 1)[1].split()
        return int(tail[19])
    except (OSError, IndexError, ValueError):
        return None


def process_cmdline(pid):
    try:
        with open(f"/proc/{int(pid)}/cmdline", "rb") as f:
            return f.read().replace(b"\0", b" ").decode(errors="replace")
    except OSError:
        return ""


def kill_verified(pid, start=None):
    if not pid:
        return False
    if start is None:
        start = read_starttime(pid)
    if start is None or read_starttime(pid) != start:
        return False
    if "hermes" not in process_cmdline(pid):
        return False
    try:
        os.kill(int(pid), signal.SIGKILL)
        return True
    except OSError:
        return False


def _load_fake() -> str:
    here = Path(__file__).resolve()
    for base in [here.parent.parent.parent]:
        cand = base / "tests" / "common" / "fake_hermes.py"
        if cand.exists():
            return cand.read_text()
    raise SystemExit("no se encontró tests/common/fake_hermes.py")


FAKE_HERMES = _load_fake()

FLAG = "AERIL_HERMES_PROFILES"


class Fixture:
    def __init__(self, bin_path: Path, name: str):
        self.bin = bin_path
        self.root = Path(tempfile.mkdtemp(prefix=f"r1-{name}-"))
        self.data = self.root / "data"
        self.repo = self.root / "repo"
        self.bin_dir = self.root / "fakebin"
        self.data.mkdir(parents=True)
        self.repo.mkdir()
        self.bin_dir.mkdir()
        self._git(["init", "-q", "-b", "main"])
        self._git(["config", "user.email", "r1@aeril.local"])
        self._git(["config", "user.name", "R1"])
        (self.repo / "NOTES.md").write_text("# notas\n")
        self._git(["add", "-A"])
        self._git(["commit", "-q", "-m", "base"])
        hermes = self.bin_dir / "hermes"
        hermes.write_text(FAKE_HERMES)
        hermes.chmod(0o755)

    def _git(self, args):
        subprocess.run(["git", "-C", str(self.repo), *args], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def env(self, on=True, extra=None):
        env = dict(os.environ)
        env["AERIL_DATA_DIR"] = str(self.data)
        env["PATH"] = f"{self.bin_dir}:{env.get('PATH', '')}"
        env["AERIL_HERMES_BIN"] = str(self.bin_dir / "hermes")
        if on:
            env[FLAG] = "1"
        else:
            env.pop(FLAG, None)
        for k, v in (extra or {}).items():
            env[k] = v
        return env

    def aeril(self, args, on=True, extra=None, timeout=60):
        return subprocess.run([str(self.bin), *args], env=self.env(on, extra),
                              cwd=self.root, capture_output=True, text=True,
                              timeout=timeout)

    def spawn_aeril(self, args, on=True, extra=None):
        return subprocess.Popen([str(self.bin), *args], env=self.env(on, extra),
                                cwd=self.root, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, text=True)

    def profile_dir(self, agent):
        return self.data / "hermes" / agent

    def home(self, agent):
        return self.profile_dir(agent) / "home"

    def set_opt(self, agent, name, value):
        (self.home(agent) / name).write_text(value)

    def thread_new(self):
        out = self.aeril(["thread", "new", "--repo", str(self.repo), "--title", "r1"])
        assert out.returncode == 0, out.stderr
        return json.loads(out.stdout)["thread_id"]

    def create_profile(self, agent, auth_mode="positive"):
        out = self.aeril(["profile", "create", "--agent", agent])
        assert out.returncode == 0, f"create: {out.stderr}"
        if auth_mode:
            self.set_opt(agent, "fake-auth-mode", auth_mode)

    def cleanup(self):
        # Matar cualquier descendiente rastreado del fixture.
        for pidfile in self.data.rglob("fake-pid"):
            try:
                pid = int(pidfile.read_text().strip())
                start_file = pidfile.with_name("fake-pid-start")
                start = None
                if start_file.exists():
                    try:
                        start = int(start_file.read_text().strip())
                    except ValueError:
                        start = None
                kill_verified(pid, start)
            except (OSError, ValueError):
                pass
        for pidfile in self.data.rglob("fake-descendant-pid"):
            try:
                nums = [int(x) for x in pidfile.read_text().split()]
                for pid, start in zip(nums[0::2], nums[1::2]):
                    kill_verified(pid, start)
            except (OSError, ValueError):
                pass
        shutil.rmtree(self.root, ignore_errors=True)


def alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def wait_file(path, secs):
    deadline = time.time() + secs
    while time.time() < deadline:
        if path.exists():
            return True
        time.sleep(0.05)
    return path.exists()


def read_pid(path):
    try:
        return int(path.read_text().strip())
    except (OSError, ValueError):
        return None


RESULTS = []


def report(name, verdict, detail):
    RESULTS.append((name, verdict, detail))
    print(f"{name}: {verdict} — {detail}", flush=True)


def scenario_p1_1(bin_path, only):
    # P1-1a: run durable no-terminal (supervisor y backend muertos) + --clear-threads
    if not only or "P1-1a" in only:
        fx = Fixture(bin_path, "p11a")
        try:
            fx.create_profile("p11")
            fx.set_opt("p11", "fake-init-mode", "linger")
            t = fx.thread_new()
            assert fx.aeril(["profile", "assign", "--agent", "p11", "--thread", t]).returncode == 0
            run = fx.spawn_aeril(["run", "start", "--thread", t, "--prompt", "x", "--backend", "hermes"])
            assert wait_file(fx.home("p11") / "fake-pid", 15), "backend no arrancó"
            assert wait_file(fx.home("p11") / "fake-lingering", 15), "el backend no llegó al turno"
            fake = read_pid(fx.home("p11") / "fake-pid")
            fake_start = read_starttime(fake)
            os.kill(run.pid, signal.SIGKILL)      # supervisor muere
            run.wait()
            kill_verified(fake, fake_start)        # backend también muere (run durable)
            time.sleep(0.5)
            out = fx.aeril(["profile", "remove", "--agent", "p11", "--confirm", "--clear-threads"])
            deleted = not fx.profile_dir("p11").exists()
            if out.returncode == 0 and deleted:
                report("P1-1a", "RED", "remove --clear-threads ELIMINÓ el perfil con run durable no-terminal")
            else:
                report("P1-1a", "GREEN", f"denegado/inalterado (rc={out.returncode})")
                rec = fx.aeril(["recover"])
                out2 = fx.aeril(["profile", "remove", "--agent", "p11", "--confirm", "--clear-threads"])
                print(f"  P1-1a-recon: recover rc={rec.returncode}; remove post-reconciliación rc={out2.returncode} "
                      f"(perfil {'eliminado' if not fx.profile_dir('p11').exists() else 'intacto'})", flush=True)
        finally:
            fx.cleanup()

    # P1-1b: unassign durante run no-terminal + remove sin --clear-threads
    if not only or "P1-1b" in only:
        fx = Fixture(bin_path, "p11b")
        try:
            fx.create_profile("p11")
            fx.set_opt("p11", "fake-init-mode", "linger")
            t = fx.thread_new()
            assert fx.aeril(["profile", "assign", "--agent", "p11", "--thread", t]).returncode == 0
            run = fx.spawn_aeril(["run", "start", "--thread", t, "--prompt", "x", "--backend", "hermes"])
            assert wait_file(fx.home("p11") / "fake-pid", 15), "backend no arrancó"
            assert wait_file(fx.home("p11") / "fake-lingering", 15), "el backend no llegó al turno"
            fake = read_pid(fx.home("p11") / "fake-pid")
            fake_start = read_starttime(fake)
            os.kill(run.pid, signal.SIGKILL)
            run.wait()
            kill_verified(fake, fake_start)
            time.sleep(0.5)
            un = fx.aeril(["profile", "unassign", "--thread", t])
            out = fx.aeril(["profile", "remove", "--agent", "p11", "--confirm"])
            deleted = not fx.profile_dir("p11").exists()
            if un.returncode == 0 and out.returncode == 0 and deleted:
                report("P1-1b", "RED", "unassign + remove ELIMINARON el perfil con run durable no-terminal")
            else:
                report("P1-1b", "GREEN",
                       f"unassign rc={un.returncode}, remove rc={out.returncode}, dir={'gone' if deleted else 'intact'}")
        finally:
            fx.cleanup()

    # P1-1c: DB inaccesible ⇒ fail-closed
    if not only or "P1-1c" in only:
        fx = Fixture(bin_path, "p11c")
        try:
            fx.create_profile("p11")
            fx.thread_new()  # materializa la DB (thread new abre el store)
            db = fx.data / "aeril.db"
            if db.exists():
                db.chmod(0)
            out = fx.aeril(["profile", "remove", "--agent", "p11", "--confirm"])
            deleted = not fx.profile_dir("p11").exists()
            if db.exists():
                db.chmod(0o600)
            if out.returncode == 0 and deleted:
                report("P1-1c", "RED", "DB inaccesible ⇒ remove ELIMINÓ el perfil (chequeo saltado)")
            else:
                report("P1-1c", "GREEN", f"denegado con DB inaccesible (rc={out.returncode})")
        finally:
            fx.cleanup()


def scenario_p1_2(bin_path, only):
    if only and "P1-2" not in only:
        return
    matrix = [
        ("positive", "logged_in", True),
        ("positive-exit1", "NOT logged_in", False),
        ("not-currently", "NOT logged_in", False),
        ("not-signed", "NOT logged_in", False),
        ("logged-out", "NOT logged_in", False),
        ("no-credentials", "NOT logged_in", False),
        ("other-provider", "NOT logged_in", False),
        ("contradictory", "NOT logged_in", False),
        ("malformed", "NOT logged_in", False),
        ("empty", "NOT logged_in", False),
    ]
    fx = Fixture(bin_path, "p12")
    try:
        fx.create_profile("p12", auth_mode="")
        bad = []
        for mode, expectation, must_be_positive in matrix:
            fx.set_opt("p12", "fake-auth-mode", mode)
            out = fx.aeril(["profile", "status", "--agent", "p12"])
            status = "?"
            if out.returncode == 0:
                try:
                    status = json.loads(out.stdout)["auth_status"]
                except Exception:
                    status = "parse-error"
            got_positive = status == "logged_in"
            ok = got_positive == must_be_positive
            print(f"  P1-2[{mode}]: auth_status={status} (esperado {'logged_in' if must_be_positive else 'no-positive'})", flush=True)
            if not ok:
                bad.append(mode)
        if bad:
            report("P1-2", "RED", f"clasificación incorrecta en: {', '.join(bad)}")
        else:
            report("P1-2", "GREEN", "matriz adversarial completa sin positivos falsos")
    finally:
        fx.cleanup()


def scenario_p1_3(bin_path, only):
    if only and "P1-3" not in only:
        return
    cases = [
        ("valid", True),
        ("fragmented", True),
        ("notif-masquerade", False),
        ("string-result", False),
        ("wrong-id", False),
        ("null-result", False),
        ("error", False),
        ("bare-result", False),
        ("missing-protocol", False),
        ("oversized", False),
        ("eof-mid-frame", False),
        ("no-response", False),
        ("endless-notifications", False),
        ("notifications-then-valid", True),
    ]
    fx = Fixture(bin_path, "p13")
    try:
        fx.create_profile("p13")
        bad = []
        for mode, should_pass in cases:
            fx.set_opt("p13", "fake-init-mode", mode)
            try:
                out = fx.aeril(["profile", "warm", "--agent", "p13"], extra={
                    "AERIL_PROFILE_WARM_TIMEOUT_SECS": "2" if mode in
                    ("no-response", "endless-notifications") else "10"})
                passed = out.returncode == 0
            except subprocess.TimeoutExpired:
                passed = False
                out = None
            print(f"  P1-3[{mode}]: warm rc={'0(PASS)' if passed else 'non-zero'} "
                  f"(esperado {'PASS' if should_pass else 'reject'})", flush=True)
            if passed != should_pass:
                bad.append(mode)
            # limpiar procesos del fake que hayan quedado
            pid = read_pid(fx.home("p13") / "fake-pid")
            if pid:
                kill_verified(pid)
                time.sleep(0.1)
        if bad:
            report("P1-3", "RED", f"initialize aceptado/ rechazado incorrectamente en: {', '.join(bad)}")
        else:
            report("P1-3", "GREEN", f"validación estricta completa ({len(cases)}/{len(cases)})")
    finally:
        fx.cleanup()


def scenario_p1_4(bin_path, only):
    if only and "P1-4" not in only:
        return
    fx = Fixture(bin_path, "p14")
    try:
        fx.create_profile("p14")
        fx.set_opt("p14", "fake-init-mode", "linger")
        t = fx.thread_new()
        assert fx.aeril(["profile", "assign", "--agent", "p14", "--thread", t]).returncode == 0
        run = fx.spawn_aeril(["run", "start", "--thread", t, "--prompt", "x", "--backend", "hermes"])
        assert wait_file(fx.home("p14") / "fake-pid", 15), "backend no arrancó"
        assert wait_file(fx.home("p14") / "fake-lingering", 15), "el backend no llegó al turno"
        fake = read_pid(fx.home("p14") / "fake-pid")
        fake_start = read_starttime(fake)
        os.kill(run.pid, signal.SIGKILL)   # supervisor muere; child sobrevive
        run.wait()
        time.sleep(0.3)
        survivor = alive(fake)
        warm = fx.aeril(["profile", "warm", "--agent", "p14"],
                        extra={"AERIL_PROFILE_WARM_TIMEOUT_SECS": "2"})
        # Segundo run: si es ADMITIDO queda colgado en el fake linger ⇒ timeout
        # corto; un rechazo fail-closed sale rápido con rc != 0.
        warm2 = fx.spawn_aeril(["run", "start", "--thread", t, "--prompt", "y", "--backend", "hermes"])
        try:
            warm2.wait(timeout=8)
            run2_rc = warm2.returncode
            run2_admitted = run2_rc == 0
        except subprocess.TimeoutExpired:
            run2_admitted = True
            run2_rc = None
            warm2.kill()
            warm2.wait()
        rm = fx.aeril(["profile", "remove", "--agent", "p14", "--confirm", "--clear-threads"])
        second_ok = warm.returncode == 0
        if survivor and (second_ok or run2_admitted):
            report("P1-4", "RED",
                   f"child sobreviviente (pid {fake}): warm aceptado={second_ok}, run admitido={run2_admitted}, remove rc={rm.returncode}")
        else:
            report("P1-4", "GREEN",
                   f"survivor={survivor}, warm rc={warm.returncode}, run admitido={run2_admitted}, remove rc={rm.returncode}")
        # limpieza: matar al sobreviviente y verificar terminación
        kill_verified(fake, fake_start)
        time.sleep(0.3)
        print(f"  P1-4: sobreviviente terminado (alive={alive(fake)})", flush=True)
        # Reconciliación + writer nuevo legítimo
        rec = fx.aeril(["recover"])
        warm3 = fx.aeril(["profile", "warm", "--agent", "p14"],
                         extra={"AERIL_PROFILE_WARM_TIMEOUT_SECS": "5"})
        report("P1-4-recon", "GREEN" if warm3.returncode == 0 else "CHECK",
               f"recover rc={rec.returncode}; warm post-reconciliación rc={warm3.returncode}")
    finally:
        fx.cleanup()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--only", default=None, help="P1-1a,P1-2,... (substring match)")
    args = ap.parse_args()
    bin_path = Path(args.bin).resolve()
    assert bin_path.exists(), f"binario no existe: {bin_path}"
    only = [s.strip() for s in args.only.split(",")] if args.only else None

    scenario_p1_1(bin_path, only)
    scenario_p1_2(bin_path, only)
    scenario_p1_3(bin_path, only)
    scenario_p1_4(bin_path, only)

    print("\n=== resumen ===")
    reds = [r for r in RESULTS if r[1] == "RED"]
    for name, verdict, detail in RESULTS:
        print(f"{name}: {verdict} — {detail}")
    print(f"total: {len(RESULTS)} escenarios, {len(reds)} RED")
    return 1 if reds else 0


if __name__ == "__main__":
    sys.exit(main())
