//! P2-S1 R1 — regresión adversarial de los 4 hallazgos P1 + hardening H1/H2/H4.
//!
//! Reproduce las rutas de fallo del candidato auditado (39eaf15) contra el
//! candidato corregido, con Hermes fake controlado por el auditor, fixtures
//! descartables y crashes REALES (SIGKILL de supervisor), no filas fabricadas.
//! Sin credenciales reales; los perfiles autenticados de P2-S0 no se tocan.

mod common;
use common::*;
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const FAKE_HERMES: &str = include_str!("common/fake_hermes.py");

struct R1 {
    env: TestEnv,
    bin: PathBuf,
    path_orig: String,
}

impl R1 {
    fn new(name: &str) -> R1 {
        let env = TestEnv::new(name);
        let bin = env.root.join("fakebin");
        std::fs::create_dir_all(&bin).unwrap();
        let hermes = bin.join("hermes");
        std::fs::write(&hermes, FAKE_HERMES).unwrap();
        std::fs::set_permissions(&hermes, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path_orig = std::env::var("PATH").unwrap_or_default();
        R1 {
            env,
            bin,
            path_orig,
        }
    }

    fn env_pairs(&self) -> Vec<(String, String)> {
        vec![
            ("AERIL_HERMES_PROFILES".into(), "1".into()),
            (
                "PATH".into(),
                format!("{}:{}", self.bin.display(), self.path_orig),
            ),
            (
                "AERIL_HERMES_BIN".into(),
                self.bin.join("hermes").to_string_lossy().into_owned(),
            ),
        ]
    }

    fn cmd(&self, args: &[&str], extra: &[(&str, String)]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .env("AERIL_DATA_DIR", &self.env.data)
            .current_dir(&self.env.root);
        for (k, v) in self.env_pairs() {
            cmd.env(k, v);
        }
        for (k, v) in extra {
            cmd.env(k, v);
        }
        cmd
    }

    fn out(&self, args: &[&str]) -> std::process::Output {
        self.cmd(args, &[]).output().expect("aeril")
    }

    fn ok(&self, args: &[&str]) -> Value {
        let out = self.out(args);
        assert!(
            out.status.success(),
            "aeril {args:?} falló:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        parse_json(&out.stdout)
    }

    fn err(&self, args: &[&str]) -> String {
        let out = self.out(args);
        assert!(
            !out.status.success(),
            "aeril {args:?} debía fallar y salió 0:\nstdout: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    }

    fn create(&self, agent: &str) {
        self.ok(&["profile", "create", "--agent", agent]);
    }

    fn mark_auth(&self, agent: &str) {
        std::fs::write(self.home(agent).join("fake-auth"), "1").unwrap();
    }

    fn set_init_mode(&self, agent: &str, mode: &str) {
        std::fs::write(self.home(agent).join("fake-init-mode"), mode).unwrap();
    }

    fn set_auth_mode(&self, agent: &str, mode: &str) {
        std::fs::write(self.home(agent).join("fake-auth-mode"), mode).unwrap();
    }

    fn agent_dir(&self, agent: &str) -> PathBuf {
        self.env.data.join("hermes").join(agent)
    }

    fn home(&self, agent: &str) -> PathBuf {
        self.agent_dir(agent).join("home")
    }

    fn thread_new(&self) -> String {
        self.env.thread_new("r1")
    }

    fn view(&self, agent: &str) -> Value {
        self.ok(&["profile", "list"])
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["agent_id"] == agent)
            .unwrap_or_else(|| panic!("perfil {agent} ausente"))
            .clone()
    }

    /// Run hermes en background con el perfil asignado.
    fn spawn_run(&self, thread: &str) -> Child {
        self.cmd(
            &[
                "run",
                "start",
                "--thread",
                thread,
                "--prompt",
                "x",
                "--backend",
                "hermes",
            ],
            &[],
        )
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap()
    }

    fn wait_for(&self, path: &Path, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if path.exists() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        path.exists()
    }

    fn fake_pid(&self, agent: &str) -> i32 {
        std::fs::read_to_string(self.home(agent).join("fake-pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }
}

fn kill_pid(pid: i32, sig: i32) {
    unsafe {
        libc::kill(pid, sig);
    }
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn reap(mut child: Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }
    }
}

/// Crash genuino: arranca un run (fake linger), SIGKILL al supervisor y al
/// backend, dejando filas durables no-terminales. Devuelve (thread, agent).
fn genuine_crash(r1: &R1, agent: &str) -> String {
    r1.create(agent);
    r1.mark_auth(agent);
    r1.set_init_mode(agent, "linger");
    let t = r1.thread_new();
    r1.ok(&["profile", "assign", "--agent", agent, "--thread", &t]);
    let run = r1.spawn_run(&t);
    assert!(
        r1.wait_for(&r1.home(agent).join("fake-pid"), 15),
        "backend no arrancó"
    );
    assert!(
        r1.wait_for(&r1.home(agent).join("fake-lingering"), 15),
        "el backend no llegó al turno"
    );
    let fake = r1.fake_pid(agent);
    kill_pid(run.id() as i32, libc::SIGKILL);
    reap(run);
    kill_pid(fake, libc::SIGKILL);
    std::thread::sleep(Duration::from_millis(300));
    t
}

// ── P1-1: borrado seguro con runs activos/no reconciliados ──────────────────

#[test]
fn r1_p1_1a_unresolved_run_blocks_removal_even_with_clear_threads() {
    let r1 = R1::new("r1-p11a");
    let t = genuine_crash(&r1, "p11");
    // Run durable no-terminal (supervisor y backend muertos) ⇒ HOLD.
    let err = r1.err(&[
        "profile",
        "remove",
        "--agent",
        "p11",
        "--confirm",
        "--clear-threads",
    ]);
    assert!(err.contains("no-terminal"), "mensaje: {err}");
    assert!(r1.agent_dir("p11").exists(), "el perfil no debe borrarse");
    // Reconciliación acotada (`aeril recover`) ⇒ ahora sí.
    r1.ok(&["recover"]);
    r1.ok(&[
        "profile",
        "remove",
        "--agent",
        "p11",
        "--confirm",
        "--clear-threads",
    ]);
    assert!(!r1.agent_dir("p11").exists());
    let _ = t;
}

#[test]
fn r1_p1_1b_unassign_during_unresolved_run_denied() {
    let r1 = R1::new("r1-p11b");
    let t = genuine_crash(&r1, "p11");
    // No se puede borrar el vínculo durable de un run no-terminal.
    let err = r1.err(&["profile", "unassign", "--thread", &t]);
    assert!(err.contains("no-terminal"), "mensaje: {err}");
    // Y por lo tanto remove sin --clear-threads tampoco puede colarse.
    let err = r1.err(&["profile", "remove", "--agent", "p11", "--confirm"]);
    assert!(
        err.contains("no-terminal") || err.contains("thread"),
        "mensaje: {err}"
    );
    assert!(r1.agent_dir("p11").exists());
    // Reasignar a otro perfil tampoco.
    r1.create("p11-otro");
    let err = r1.err(&["profile", "assign", "--agent", "p11-otro", "--thread", &t]);
    assert!(err.contains("no-terminal"), "mensaje: {err}");
    // Tras reconciliar, unassign funciona.
    r1.ok(&["recover"]);
    let v = r1.ok(&["profile", "unassign", "--thread", &t]);
    assert_eq!(v["unassigned"], true);
}

#[test]
fn r1_p1_1c_unreadable_database_denies_removal() {
    let r1 = R1::new("r1-p11c");
    r1.create("p11");
    let _ = r1.thread_new(); // materializa la DB (thread new abre el store)
    let db = r1.env.data.join("aeril.db");
    assert!(db.exists(), "la DB debe existir tras thread new");
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o000)).unwrap();
    let err = r1.err(&["profile", "remove", "--agent", "p11", "--confirm"]);
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        err.contains("base") || err.contains("fail-closed"),
        "mensaje: {err}"
    );
    assert!(
        r1.agent_dir("p11").exists(),
        "no debe borrarse con DB ilegible"
    );
}

#[test]
fn r1_p1_1g_missing_database_denies_removal() {
    let r1 = R1::new("r1-p11g");
    r1.create("p11");
    // Sin base de evidencia (nunca se abrió el store) ⇒ HOLD fail-closed.
    assert!(!r1.env.data.join("aeril.db").exists());
    let err = r1.err(&["profile", "remove", "--agent", "p11", "--confirm"]);
    assert!(err.contains("base de evidencia"), "mensaje: {err}");
    assert!(r1.agent_dir("p11").exists());
    // Materializar la DB (p. ej. `thread new`) habilita el camino normal.
    let _ = r1.thread_new();
    r1.ok(&["profile", "remove", "--agent", "p11", "--confirm"]);
    assert!(!r1.agent_dir("p11").exists());
}

#[test]
fn r1_p1_1d_active_run_blocks_removal() {
    let r1 = R1::new("r1-p11d");
    r1.create("p11");
    r1.mark_auth("p11");
    r1.set_init_mode("p11", "linger");
    let t = r1.thread_new();
    r1.ok(&["profile", "assign", "--agent", "p11", "--thread", &t]);
    let run = r1.spawn_run(&t);
    assert!(r1.wait_for(&r1.home("p11").join("fake-pid"), 15));
    assert!(r1.wait_for(&r1.home("p11").join("fake-lingering"), 15));
    let err = r1.err(&[
        "profile",
        "remove",
        "--agent",
        "p11",
        "--confirm",
        "--clear-threads",
    ]);
    assert!(
        err.contains("no-terminal") || err.contains("escritor"),
        "mensaje: {err}"
    );
    assert!(r1.agent_dir("p11").exists());
    // limpieza
    kill_pid(r1.fake_pid("p11"), libc::SIGKILL);
    kill_pid(run.id() as i32, libc::SIGTERM);
    reap(run);
}

#[test]
fn r1_p1_1e_terminal_run_allows_explicit_removal() {
    let r1 = R1::new("r1-p11e");
    r1.create("p11");
    r1.mark_auth("p11");
    let t = r1.thread_new();
    r1.ok(&["profile", "assign", "--agent", "p11", "--thread", &t]);
    // Run completo (fake default: initialize válido + turno end_turn).
    let v = r1.ok(&[
        "run",
        "start",
        "--thread",
        &t,
        "--prompt",
        "x",
        "--backend",
        "hermes",
    ]);
    assert_eq!(v["state"], "completed");
    // Asociación histórica sin --clear-threads ⇒ rechazo explícito…
    let err = r1.err(&["profile", "remove", "--agent", "p11", "--confirm"]);
    assert!(err.contains("thread"), "mensaje: {err}");
    // …con --clear-threads ⇒ permitido (run terminal).
    r1.ok(&[
        "profile",
        "remove",
        "--agent",
        "p11",
        "--confirm",
        "--clear-threads",
    ]);
    assert!(!r1.agent_dir("p11").exists());
}

#[test]
fn r1_p1_1f_partial_crash_removal_is_idempotent() {
    let r1 = R1::new("r1-p11f");
    let _ = r1.thread_new(); // materializa la DB (remove la exige)
    r1.create("p11");
    // Crash entre borrado de FS y registro: dir ausente, entrada presente.
    std::fs::remove_dir_all(r1.agent_dir("p11")).unwrap();
    r1.ok(&["profile", "remove", "--agent", "p11", "--confirm"]);
    // El registro quedó íntegro y sin la entrada.
    let reg: Value =
        serde_json::from_slice(&std::fs::read(r1.env.data.join("hermes/agents.json")).unwrap())
            .unwrap();
    assert!(reg["agents"].as_object().unwrap().is_empty());
    // Segundo remove: no encontrado, sin corrupción.
    let err = r1.err(&["profile", "remove", "--agent", "p11", "--confirm"]);
    assert!(err.contains("no existe"), "mensaje: {err}");
    let reg: Value =
        serde_json::from_slice(&std::fs::read(r1.env.data.join("hermes/agents.json")).unwrap())
            .unwrap();
    assert!(reg["agents"].as_object().unwrap().is_empty());
}

// ── P1-2: matriz de auth (integración + admisión del Runner) ────────────────

#[test]
fn r1_p1_2_auth_matrix_never_false_positive() {
    let r1 = R1::new("r1-p12");
    r1.create("p12");
    let matrix = [
        ("positive", true),
        ("positive-exit1", false),
        ("not-currently", false),
        ("not-signed", false),
        ("logged-out", false),
        ("no-credentials", false),
        ("other-provider", false),
        ("contradictory", false),
        ("malformed", false),
        ("empty", false),
    ];
    for (mode, must_positive) in matrix {
        r1.set_auth_mode("p12", mode);
        let v = r1.ok(&["profile", "status", "--agent", "p12"]);
        let positive = v["auth_status"] == "logged_in";
        assert_eq!(
            positive, must_positive,
            "modo {mode}: auth_status={} (esperado positive={must_positive})",
            v["auth_status"]
        );
        assert!(
            v["auth_checked_ms"].as_i64().is_some(),
            "todo verdicto debe persistir timestamp fresco"
        );
    }
    // Recuperación: unknown → logged_in al volver el positivo.
    r1.set_auth_mode("p12", "malformed");
    assert_eq!(
        r1.ok(&["profile", "status", "--agent", "p12"])["auth_status"],
        "unknown"
    );
    r1.set_auth_mode("p12", "positive");
    assert_eq!(
        r1.ok(&["profile", "status", "--agent", "p12"])["auth_status"],
        "logged_in"
    );
}

#[test]
fn r1_p1_2_runner_admission_rejects_exit1_positive_text() {
    let r1 = R1::new("r1-p12b");
    r1.create("p12");
    r1.set_auth_mode("p12", "positive-exit1");
    let t = r1.thread_new();
    r1.ok(&["profile", "assign", "--agent", "p12", "--thread", &t]);
    let err = r1.err(&[
        "run",
        "start",
        "--thread",
        &t,
        "--prompt",
        "x",
        "--backend",
        "hermes",
    ]);
    assert!(
        err.contains("sin autenticación verificada"),
        "mensaje: {err}"
    );
    // El rechazo no deja lock tomado.
    assert_eq!(r1.view("p12")["lock_state"], "free");
}

// ── P1-3: initialize estricto ───────────────────────────────────────────────

#[test]
fn r1_p1_3_warm_strict_initialize_matrix() {
    let r1 = R1::new("r1-p13");
    r1.create("p13");
    r1.mark_auth("p13");
    let cases = [
        ("valid", true),
        ("fragmented", true),
        ("notif-masquerade", false),
        ("string-result", false),
        ("wrong-id", false),
        ("null-result", false),
        ("error", false),
        ("bare-result", false),
        ("missing-protocol", false),
        ("oversized", false),
        ("eof-mid-frame", false),
        ("no-response", false),
        ("endless-notifications", false),
        ("notifications-then-valid", true),
    ];
    for (mode, should_pass) in cases {
        r1.set_init_mode("p13", mode);
        let out = r1
            .cmd(
                &["profile", "warm", "--agent", "p13"],
                &[(
                    "AERIL_PROFILE_WARM_TIMEOUT_SECS",
                    if matches!(mode, "no-response" | "endless-notifications" | "hang") {
                        "2".to_string()
                    } else {
                        "10".to_string()
                    },
                )],
            )
            .output()
            .unwrap();
        let passed = out.status.success();
        assert_eq!(
            passed,
            should_pass,
            "modo {mode}: warm passed={passed} (esperado {should_pass})\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // Limpieza del fake por si quedó vivo.
        let pidfile = r1.home("p13").join("fake-pid");
        if let Ok(txt) = std::fs::read_to_string(&pidfile)
            && let Ok(pid) = txt.trim().parse::<i32>()
        {
            kill_pid(pid, libc::SIGKILL);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn r1_p1_3_noncooperative_child_is_terminated() {
    let r1 = R1::new("r1-p13b");
    r1.create("p13");
    r1.mark_auth("p13");
    r1.set_init_mode("p13", "ignore-term");
    let out = r1
        .cmd(
            &["profile", "warm", "--agent", "p13"],
            &[("AERIL_PROFILE_WARM_TIMEOUT_SECS", "2".to_string())],
        )
        .output()
        .unwrap();
    assert!(!out.status.success(), "warm con child hostil debe fallar");
    let pid = r1.fake_pid("p13");
    // Escalada acotada SIGTERM→SIGKILL con verificación: no debe quedar vivo.
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !alive(pid),
        "el child que ignora SIGTERM debe ser terminado (sin huérfanos)"
    );
    assert_eq!(r1.view("p13")["lock_state"], "free");
}

// ── P1-4: exclusión de escritor ante crash del supervisor ───────────────────

#[test]
fn r1_p1_4_surviving_writer_after_supervisor_sigkill() {
    let r1 = R1::new("r1-p14");
    r1.create("p14");
    r1.mark_auth("p14");
    r1.set_init_mode("p14", "linger");
    let t = r1.thread_new();
    r1.ok(&["profile", "assign", "--agent", "p14", "--thread", &t]);

    // 1-2) writer y lock activos
    let run = r1.spawn_run(&t);
    assert!(
        r1.wait_for(&r1.home("p14").join("fake-pid"), 15),
        "backend no arrancó"
    );
    assert!(
        r1.wait_for(&r1.home("p14").join("fake-lingering"), 15),
        "el backend no llegó al turno"
    );
    let fake = r1.fake_pid("p14");
    assert_eq!(r1.view("p14")["lock_state"], "held");

    // 3-4) SIGKILL al supervisor; el child SOBREVIVE y retiene el lock (fd heredado)
    kill_pid(run.id() as i32, libc::SIGKILL);
    reap(run);
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        alive(fake),
        "el backend debe sobrevivir al SIGKILL del supervisor"
    );
    assert_eq!(
        r1.view("p14")["lock_state"],
        "held",
        "el lock debe seguir tomado por el child (fd heredado)"
    );

    // 5-6) segundo warm y segundo run deben RECHAZARSE
    let warm = r1
        .cmd(
            &["profile", "warm", "--agent", "p14"],
            &[("AERIL_PROFILE_WARM_TIMEOUT_SECS", "2".to_string())],
        )
        .output()
        .unwrap();
    assert!(
        !warm.status.success(),
        "warm con writer sobreviviente debe fallar"
    );
    let mut run2 = r1.spawn_run(&t);
    // admitido = sigue vivo tras 8 s (colgado en el turno) o salió 0.
    let admitted = match run2.wait_timeout(Duration::from_secs(8)) {
        Some(st) => st.success(),
        None => {
            let _ = run2.kill();
            let _ = run2.wait();
            true
        }
    };
    assert!(
        !admitted,
        "segundo run con writer sobreviviente debe rechazarse"
    );
    // El sobreviviente NO fue terminado por el rechazo (nadie "admite matando").
    assert!(
        alive(fake),
        "el rechazo no debe terminar al escritor sobreviviente"
    );

    // 7) borrado rechazado
    let rm = r1.err(&[
        "profile",
        "remove",
        "--agent",
        "p14",
        "--confirm",
        "--clear-threads",
    ]);
    assert!(
        rm.contains("no-terminal") || rm.contains("escritor") || rm.contains("procesos"),
        "mensaje: {rm}"
    );

    // 8-9) terminar el child (verificando identidad/terminación) y reconciliar
    kill_pid(fake, libc::SIGKILL);
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(fake) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!alive(fake), "el child debe estar terminado");
    let _rec = r1.ok(&["recover"]); // reconciliación explícita

    // 10) writer nuevo legítimo
    let warm3 = r1
        .cmd(
            &["profile", "warm", "--agent", "p14"],
            &[("AERIL_PROFILE_WARM_TIMEOUT_SECS", "10".to_string())],
        )
        .output()
        .unwrap();
    assert!(
        warm3.status.success(),
        "warm legítimo post-reconciliación debe funcionar:\nstderr: {}",
        String::from_utf8_lossy(&warm3.stderr)
    );
    // Y el borrado explícito completa tras reconciliar.
    r1.ok(&[
        "profile",
        "remove",
        "--agent",
        "p14",
        "--confirm",
        "--clear-threads",
    ]);
    assert!(!r1.agent_dir("p14").exists());
    let _ = t;
}

// helper de timeout para Child
trait WaitTimeout {
    fn wait_timeout(&mut self, d: Duration) -> Option<std::process::ExitStatus>;
}
impl WaitTimeout for Child {
    fn wait_timeout(&mut self, d: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + d;
        loop {
            match self.try_wait() {
                Ok(Some(st)) => return Some(st),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                _ => return None,
            }
        }
    }
}

// ── H1: lock no verificable ⇒ fail-closed ───────────────────────────────────

#[test]
fn r1_h1_unreadable_lock_is_unknown_and_blocks_removal() {
    let r1 = R1::new("r1-h1");
    let _ = r1.thread_new(); // materializa la DB (remove la exige)
    r1.create("h1");
    let lock = r1.agent_dir("h1").join("aeril.lock");
    // El lock se materializa en el primer warm/run; lo creamos como lo haría
    // el subsistema (0600) para inyectar el fallo de lectura.
    std::fs::write(&lock, b"").unwrap();
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o000)).unwrap();
    let v = r1.ok(&["profile", "status", "--agent", "h1"]);
    assert_eq!(v["lock_state"], "unknown", "lock ilegible = incertidumbre");
    assert_eq!(v["lock_held"], false);
    let err = r1.err(&["profile", "remove", "--agent", "h1", "--confirm"]);
    assert!(err.contains("lock"), "mensaje: {err}");
    assert!(r1.agent_dir("h1").exists());
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).unwrap();
    r1.ok(&["profile", "remove", "--agent", "h1", "--confirm"]);
}

// ── H2: concurrencia de metadata sin lost-update ────────────────────────────

#[test]
fn r1_h2_concurrent_meta_updates_no_lost_update() {
    let r1 = R1::new("r1-h2");
    r1.create("h2");
    r1.mark_auth("h2");
    let mut kids: Vec<(&str, Child)> = Vec::new();
    for _ in 0..6 {
        kids.push((
            "status",
            r1.cmd(&["profile", "status", "--agent", "h2"], &[])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        ));
    }
    kids.push((
        "warm",
        r1.cmd(&["profile", "warm", "--agent", "h2"], &[])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    for (name, k) in kids {
        let out = k.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{name} concurrente falló (status {:?}):\nstdout: {}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // RMW serializado: AMBOS campos deben sobrevivir.
    let meta: Value =
        serde_json::from_slice(&std::fs::read(r1.agent_dir("h2").join("profile.json")).unwrap())
            .unwrap();
    assert_eq!(
        meta["auth_status"], "logged_in",
        "auth_status perdido por carrera"
    );
    assert!(
        meta["warmed_ms"].as_i64().is_some(),
        "warmed_ms perdido por carrera"
    );
}

// ── H4: flag ON con perfil creado/asignado/autenticado corre completo ───────

#[test]
fn r1_h4_flag_on_full_run_with_managed_profile() {
    let r1 = R1::new("r1-h4");
    r1.create("h4");
    r1.mark_auth("h4");
    let t = r1.thread_new();
    r1.ok(&["profile", "assign", "--agent", "h4", "--thread", &t]);
    let v = r1.ok(&[
        "run",
        "start",
        "--thread",
        &t,
        "--prompt",
        "hola",
        "--backend",
        "hermes",
    ]);
    assert_eq!(v["state"], "completed");
    // El run quedó con vínculo durable al perfil (schema v4).
    let runs = r1.ok(&["run", "list", "--thread", &t]);
    let arr = runs.as_array().cloned().unwrap_or_default();
    assert!(!arr.is_empty(), "run list debe mostrar el run completado");
    assert_eq!(r1.view("h4")["lock_state"], "free");
}
