//! G1 — clasificación de turno fallido con Hermes fake (flujo de perfiles).
//!
//! El ACP puede responder `end_turn` aunque el proveedor haya fallado el turno
//! (aviso terminal en el texto del agente). El run DEBE cerrarse `failed`, sin
//! checkpoint. El fallback textual es EXACTO y está ligado a la versión de
//! Hermes: citas o versiones no soportadas no deben producir falsos fallos.

mod common;
use common::*;
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

const FAKE_HERMES: &str = include_str!("common/fake_hermes.py");

struct P2 {
    env: TestEnv,
    bin: PathBuf,
    fake_version: String,
    path_orig: String,
}

impl P2 {
    fn new(name: &str, fake_version: &str) -> P2 {
        let env = TestEnv::new(name);
        let bin = env.root.join("fakebin");
        std::fs::create_dir_all(&bin).unwrap();
        let hermes = bin.join("hermes");
        std::fs::write(&hermes, FAKE_HERMES).unwrap();
        std::fs::set_permissions(&hermes, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path_orig = std::env::var("PATH").unwrap_or_default();
        P2 {
            env,
            bin,
            fake_version: fake_version.into(),
            path_orig,
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .env("AERIL_DATA_DIR", &self.env.data)
            .env("AERIL_HERMES_PROFILES", "1")
            .env("PATH", format!("{}:{}", self.bin.display(), self.path_orig))
            .env("AERIL_HERMES_BIN", self.bin.join("hermes"))
            .env("FAKE_VERSION", &self.fake_version)
            .current_dir(&self.env.root);
        cmd
    }

    fn out(&self, args: &[&str]) -> Output {
        self.cmd(args).output().expect("aeril")
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

    fn home(&self, agent: &str) -> PathBuf {
        self.env.data.join("hermes").join(agent).join("home")
    }

    /// Perfil creado, autenticado (fake) y asignado a un thread.
    fn setup(&self, agent: &str) -> String {
        self.ok(&["profile", "create", "--agent", agent]);
        std::fs::write(self.home(agent).join("fake-auth"), "1").unwrap();
        let thread = self.env.thread_new("g1");
        self.ok(&["profile", "assign", "--agent", agent, "--thread", &thread]);
        thread
    }

    fn run_turn(&self, agent: &str, thread: &str, mode: &str) -> Value {
        std::fs::write(self.home(agent).join("fake-init-mode"), mode).unwrap();
        self.ok(&[
            "run",
            "start",
            "--thread",
            thread,
            "--prompt",
            "x",
            "--backend",
            "hermes",
        ])
    }

    fn checkpoints(&self, thread: &str) -> Vec<Value> {
        self.ok(&["checkpoint", "list", "--thread", thread])
            .as_array()
            .cloned()
            .unwrap_or_default()
    }
}

fn assert_failed_without_checkpoint(p2: &P2, agent: &str, thread: &str, mode: &str) {
    let v = p2.run_turn(agent, thread, mode);
    assert_eq!(v["state"], "failed", "modo {mode}: {v}");
    assert_eq!(
        v["stop_reason"].as_str(),
        Some("end_turn"),
        "el ACP reportó end_turn; el fallo surge del aviso terminal"
    );
    let detail = v["detail"].as_str().unwrap_or("");
    assert!(
        detail.contains("turno fallido"),
        "modo {mode}: detalle inesperado: {detail}"
    );
    assert!(
        p2.checkpoints(thread).is_empty(),
        "modo {mode}: un turno fallido no debe capturar checkpoint"
    );
    let runs = p2.ok(&["run", "list", "--thread", thread]);
    let arr = runs.as_array().cloned().unwrap_or_default();
    assert!(
        arr.iter().any(|r| r["state"] == "failed"),
        "modo {mode}: estado durable failed esperado"
    );
}

#[test]
fn exact_failed_turn_notice_marks_run_failed() {
    let p2 = P2::new("g1-exact", "hermes 0.21.9+test");
    let thread = p2.setup("g1");
    assert_failed_without_checkpoint(&p2, "g1", &thread, "failed-turn");
}

#[test]
fn partial_failed_turn_notice_marks_run_failed() {
    let p2 = P2::new("g1-partial", "hermes 0.21.9+test");
    let thread = p2.setup("g1");
    assert_failed_without_checkpoint(&p2, "g1", &thread, "failed-turn-partial");
}

#[test]
fn failed_turn_notice_split_across_chunks_marks_run_failed() {
    let p2 = P2::new("g1-split", "hermes 0.21.9+test");
    let thread = p2.setup("g1");
    assert_failed_without_checkpoint(&p2, "g1", &thread, "failed-turn-split");
}

#[test]
fn quoted_notice_in_a_larger_message_stays_completed() {
    let p2 = P2::new("g1-quoted", "hermes 0.21.9+test");
    let thread = p2.setup("g1");
    let v = p2.run_turn("g1", &thread, "quoted-notice");
    assert_eq!(
        v["state"], "completed",
        "una respuesta que CITA el aviso no es un turno fallido: {v}"
    );
}

#[test]
fn unsupported_hermes_version_disables_textual_fallback() {
    let p2 = P2::new("g1-version", "hermes 0.22.0+test");
    let thread = p2.setup("g1");
    let v = p2.run_turn("g1", &thread, "failed-turn");
    assert_eq!(
        v["state"], "completed",
        "sin versión soportada no se aplica el fallback textual (re-validar): {v}"
    );
}

#[test]
fn normal_turn_completes() {
    let p2 = P2::new("g1-normal", "hermes 0.21.9+test");
    let thread = p2.setup("g1");
    let v = p2.run_turn("g1", &thread, "valid");
    assert_eq!(v["state"], "completed", "{v}");
}
