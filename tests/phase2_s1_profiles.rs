//! P2-S1 — perfiles Hermes gestionados: ciclo de vida, locks de escritor,
//! crash consistency, fail-closed, wiring del Runner y rollback del flag.
//!
//! Todo corre con un **Hermes fake controlado por el auditor** (script Python
//! en un PATH propio) y perfiles sintéticos sin credenciales reales. Nunca se
//! toca el Hermes real, `auth.json`, ni los perfiles autenticados de P2-S0.
//!
//! El fake se configura por archivos dentro de `HERMES_HOME` (que el entorno
//! sanitizado siempre expone):
//! - `fake-auth`    → existe ⇒ `auth status` responde "logged in".
//! - `fake-mode`    → "hang" | "slow:<segundos>" | "linger" (default: turno ok).
//! - `fake-env.json`/`fake-pid` → los escribe el fake para verificación.

mod common;
use common::*;
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const FAKE_HERMES: &str = r#"#!/usr/bin/python3
import json, os, sys, time

args = sys.argv[1:]
home = os.environ.get("HERMES_HOME") or ""
dump = os.environ.get("FAKE_DUMP") or ""
if home:
    dump = os.path.join(home, "fake-env.json")

def mode():
    p = os.path.join(home, "fake-mode")
    try:
        with open(p) as f:
            return f.read().strip()
    except OSError:
        return "full"

if args and args[0] == "--version":
    print("hermes 0.0.0-fake")
    sys.exit(0)

if len(args) >= 2 and args[0] == "acp" and args[1] == "--version":
    print("hermes 0.0.0-fake")
    sys.exit(0)

if args and args[0] == "auth":
    provider = args[2] if len(args) > 2 else "nous"
    marker = os.path.join(home, "fake-auth") if home else ""
    if marker and os.path.exists(marker):
        print(provider + ": logged in")
    else:
        print(provider + ": not logged in")
    sys.exit(0)

if args and args[0] == "acp":
    if dump:
        keys = ["HERMES_HOME","HOME","XDG_CONFIG_HOME","XDG_CACHE_HOME",
                "XDG_DATA_HOME","XDG_STATE_HOME","GH_CONFIG_DIR","PATH"]
        data = {k: os.environ.get(k, "") for k in keys}
        try:
            with open(dump, "w") as f:
                json.dump(data, f)
        except OSError:
            pass
    if home:
        try:
            with open(os.path.join(home, "fake-pid"), "w") as f:
                f.write(str(os.getpid()))
        except OSError:
            pass
    m = mode()
    if m == "hang":
        time.sleep(600)
        sys.exit(0)
    if m.startswith("slow:"):
        time.sleep(float(m.split(":", 1)[1]))
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except Exception:
            continue
        mid = msg.get("id")
        method = msg.get("method")
        if method == "initialize":
            out = {"jsonrpc":"2.0","id":mid,"result":{
                "protocolVersion":1,
                "agentInfo":{"name":"fake-hermes","version":"0.0.0-fake"},
                "agentCapabilities":{"loadSession":True},
                "authMethods":[]}}
            print(json.dumps(out), flush=True)
        elif method == "session/new":
            out = {"jsonrpc":"2.0","id":mid,"result":{
                "sessionId":"fake-sess",
                "models":{"availableModels":[],"currentModelId":"fake"},
                "modes":{"currentModeId":"default","availableModes":[]}}}
            print(json.dumps(out), flush=True)
        elif method == "session/prompt":
            if m == "linger":
                time.sleep(600)
                continue
            note = {"jsonrpc":"2.0","method":"session/update","params":{
                "update":{"sessionUpdate":"agent_message_chunk",
                          "content":{"type":"text","text":"hecho (fake)"},
                          "messageId":"m1"}}}
            print(json.dumps(note), flush=True)
            out = {"jsonrpc":"2.0","id":mid,"result":{"stopReason":"end_turn"}}
            print(json.dumps(out), flush=True)
        elif mid is not None:
            out = {"jsonrpc":"2.0","id":mid,"result":{}}
            print(json.dumps(out), flush=True)
    sys.exit(0)

sys.exit(0)
"#;

struct P2 {
    env: TestEnv,
    bin: PathBuf,
    path_orig: String,
}

impl P2 {
    fn new(name: &str) -> P2 {
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
            path_orig,
        }
    }

    fn path_env(&self) -> String {
        format!("{}:{}", self.bin.display(), self.path_orig)
    }

    /// Variables del flag ON (perfiles habilitados, hermes fake).
    fn on(&self) -> Vec<(String, String)> {
        vec![
            ("AERIL_HERMES_PROFILES".into(), "1".into()),
            ("PATH".into(), self.path_env()),
            (
                "AERIL_HERMES_BIN".into(),
                self.bin.join("hermes").to_string_lossy().into_owned(),
            ),
        ]
    }

    fn cmd(&self, flag_on: bool, args: &[&str], extra: &[(&str, String)]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .env("AERIL_DATA_DIR", &self.env.data)
            .current_dir(&self.env.root);
        if flag_on {
            for (k, v) in self.on() {
                cmd.env(k, v);
            }
        } else {
            cmd.env("PATH", self.path_env());
        }
        for (k, v) in extra {
            cmd.env(k, v);
        }
        cmd
    }

    fn out_on(&self, args: &[&str], extra: &[(&str, String)]) -> std::process::Output {
        self.cmd(true, args, extra).output().expect("aeril")
    }

    fn ok_on(&self, args: &[&str]) -> Value {
        let out = self.out_on(args, &[]);
        assert!(
            out.status.success(),
            "aeril {args:?} falló:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        parse_json(&out.stdout)
    }

    /// Ejecuta con flag ON y exige fallo; devuelve stdout+stderr.
    fn err_on(&self, args: &[&str]) -> String {
        self.err_on_env(args, &[])
    }

    fn err_on_env(&self, args: &[&str], extra: &[(&str, String)]) -> String {
        let out = self.out_on(args, extra);
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

    fn create(&self, agent: &str) -> Value {
        self.ok_on(&["profile", "create", "--agent", agent])
    }

    fn agent_dir(&self, agent: &str) -> PathBuf {
        self.env.data.join("hermes").join(agent)
    }

    fn home(&self, agent: &str) -> PathBuf {
        self.agent_dir(agent).join("home")
    }

    fn san_home(&self, agent: &str) -> PathBuf {
        self.agent_dir(agent).join("san-home")
    }

    fn mark_auth(&self, agent: &str) {
        std::fs::write(self.home(agent).join("fake-auth"), "1").unwrap();
    }

    fn set_mode(&self, agent: &str, mode: &str) {
        std::fs::write(self.home(agent).join("fake-mode"), mode).unwrap();
    }

    fn list(&self) -> Vec<Value> {
        self.ok_on(&["profile", "list"])
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    fn view(&self, agent: &str) -> Value {
        self.list()
            .into_iter()
            .find(|v| v["agent_id"] == agent)
            .unwrap_or_else(|| panic!("perfil {agent} ausente en list"))
    }

    fn assigned_thread(&self) -> String {
        self.env.thread_new("p2s1")
    }

    /// Espera hasta `secs` a que exista `path`.
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
}

fn mode_bits(p: &Path) -> u32 {
    std::fs::symlink_metadata(p)
        .unwrap_or_else(|e| panic!("metadata de {}: {e}", p.display()))
        .permissions()
        .mode()
        & 0o777
}

fn kill_pid(pid: i32, sig: i32) {
    unsafe {
        libc::kill(pid, sig);
    }
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

// ── T1: feature flag OFF (default) ──────────────────────────────────────────

#[test]
fn t1_flag_off_refuses_profile_management() {
    let p2 = P2::new("p2s1-t1");
    let out = p2
        .cmd(false, &["profile", "create", "--agent", "a"], &[])
        .output()
        .unwrap();
    assert!(!out.status.success(), "flag OFF debe rechazar");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("deshabilitados"),
        "mensaje inesperado: {text}"
    );
    assert!(
        !p2.env.data.join("hermes").exists(),
        "flag OFF no debe crear nada"
    );
}

// ── T2: create — layout, permisos, metadata ─────────────────────────────────

#[test]
fn t2_create_layout_permissions_metadata() {
    let p2 = P2::new("p2s1-t2");
    let v = p2.create("agent-a");
    assert_eq!(v["agent_id"], "agent-a");
    assert_eq!(v["registered"], true);
    assert_eq!(v["dir_state"], "ok");

    let dir = p2.agent_dir("agent-a");
    assert!(p2.home("agent-a").is_dir(), "home/ debe existir");
    assert!(p2.san_home("agent-a").is_dir(), "san-home/ debe existir");
    assert_eq!(mode_bits(&dir), 0o700, "dir del perfil debe ser 0700");
    assert_eq!(mode_bits(&p2.home("agent-a")), 0o700);
    assert_eq!(mode_bits(&p2.san_home("agent-a")), 0o700);
    assert_eq!(mode_bits(&dir.join("profile.json")), 0o600);
    assert_eq!(
        mode_bits(&p2.env.data.join("hermes").join("agents.json")),
        0o600
    );

    let meta: Value =
        serde_json::from_slice(&std::fs::read(dir.join("profile.json")).unwrap()).unwrap();
    assert_eq!(meta["agent_id"], "agent-a");
    assert_eq!(meta["provider"], "nous");
    assert!(meta["created_ms"].as_i64().unwrap() > 0);
    // Sin credenciales: profile.json es metadata-only.
    let text = std::fs::read_to_string(dir.join("profile.json")).unwrap();
    for banned in ["auth.json", "token", "secret", "oauth", "api_key"] {
        assert!(
            !text.to_lowercase().contains(banned),
            "profile.json no debe contener {banned:?}"
        );
    }

    let reg: Value = serde_json::from_slice(
        &std::fs::read(p2.env.data.join("hermes").join("agents.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(reg["version"], 1);
    assert!(
        reg["agents"]["agent-a"]["thread_ids"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

// ── T3: create idempotente + reparación de create interrumpido ──────────────

#[test]
fn t3_create_idempotent_and_repair() {
    let p2 = P2::new("p2s1-t3");
    p2.create("agent-b");
    p2.create("agent-b"); // idempotente
    let reg: Value = serde_json::from_slice(
        &std::fs::read(p2.env.data.join("hermes").join("agents.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(reg["agents"].as_object().unwrap().len(), 1);

    // Create interrumpido (dir registrado sin profile.json) → list incomplete.
    std::fs::remove_file(p2.agent_dir("agent-b").join("profile.json")).unwrap();
    assert_eq!(p2.view("agent-b")["dir_state"], "incomplete");
    // create repara (idempotente) y no adopta nada más.
    let v = p2.create("agent-b");
    assert_eq!(v["dir_state"], "ok");
    assert!(p2.agent_dir("agent-b").join("profile.json").is_file());

    // Directorio ajeno (no vacío, sin metadata) → fail-closed, sin adopción.
    let planted = p2.env.data.join("hermes").join("planted");
    std::fs::create_dir_all(&planted).unwrap();
    std::fs::write(planted.join("datos.txt"), "ajeno").unwrap();
    let err = p2.err_on(&["profile", "create", "--agent", "planted"]);
    assert!(err.contains("no reconocido"), "mensaje: {err}");
    assert_eq!(p2.view("planted")["registered"], false);
    assert_eq!(p2.view("planted")["dir_state"], "orphan");

    // Directorio vacío (create interrumpido antes de escribir metadata) → repara.
    std::fs::create_dir_all(p2.env.data.join("hermes").join("empty-a")).unwrap();
    let v = p2.ok_on(&["profile", "create", "--agent", "empty-a"]);
    assert_eq!(v["registered"], true);
    assert_eq!(v["dir_state"], "ok");
}

// ── T4: list — estados y huérfanos sin adopción silenciosa ──────────────────

#[test]
fn t4_list_states_and_orphans() {
    let p2 = P2::new("p2s1-t4");
    p2.create("sano");
    std::fs::create_dir_all(p2.env.data.join("hermes").join("huerfano")).unwrap();
    std::fs::write(p2.env.data.join("hermes").join("huerfano").join("x"), "x").unwrap();

    let list = p2.list();
    assert_eq!(list.len(), 2);
    let sano = list.iter().find(|v| v["agent_id"] == "sano").unwrap();
    assert_eq!(sano["registered"], true);
    assert_eq!(sano["dir_state"], "ok");
    let orf = list.iter().find(|v| v["agent_id"] == "huerfano").unwrap();
    assert_eq!(orf["registered"], false);
    assert_eq!(orf["dir_state"], "orphan");

    // El huérfano NO se adoptó: el registro sigue con un solo agente.
    let reg: Value = serde_json::from_slice(
        &std::fs::read(p2.env.data.join("hermes").join("agents.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(reg["agents"].as_object().unwrap().len(), 1);
}

// ── T5: assign/unassign — mapeo estable y unicidad ──────────────────────────

#[test]
fn t5_assign_unassign_mapping() {
    let p2 = P2::new("p2s1-t5");
    p2.create("ag-a");
    p2.create("ag-b");
    let t = p2.assigned_thread();

    let err = p2.err_on(&["profile", "assign", "--agent", "fantasma", "--thread", &t]);
    assert!(err.contains("no registrado"), "mensaje: {err}");

    p2.ok_on(&["profile", "assign", "--agent", "ag-a", "--thread", &t]);
    assert_eq!(p2.view("ag-a")["thread_ids"][0], t);
    // Reasignar al mismo thread lo quita del anterior (unicidad).
    p2.ok_on(&["profile", "assign", "--agent", "ag-b", "--thread", &t]);
    assert!(p2.view("ag-a")["thread_ids"].as_array().unwrap().is_empty());
    assert_eq!(p2.view("ag-b")["thread_ids"][0], t);

    let v = p2.ok_on(&["profile", "unassign", "--thread", &t]);
    assert_eq!(v["unassigned"], true);
    assert!(p2.view("ag-b")["thread_ids"].as_array().unwrap().is_empty());
    let v = p2.ok_on(&["profile", "unassign", "--thread", &t]);
    assert_eq!(v["unassigned"], false);
}

// ── T6: status — auth sin leer credenciales ─────────────────────────────────

#[test]
fn t6_status_auth_without_credentials() {
    let p2 = P2::new("p2s1-t6");
    p2.create("auth-a");
    let v = p2.ok_on(&["profile", "status", "--agent", "auth-a"]);
    assert_eq!(v["auth_status"], "absent");
    assert_eq!(v["lock_held"], false);

    p2.mark_auth("auth-a");
    let v = p2.ok_on(&["profile", "status", "--agent", "auth-a"]);
    assert_eq!(v["auth_status"], "logged_in");
    assert!(v["disk_kb"].is_u64(), "disk_kb debe reportarse");

    // Aeril jamás crea ni necesita `auth.json` para el estado de auth.
    assert!(
        !p2.agent_dir("auth-a").join("auth.json").exists(),
        "el estado de auth no debe materializar credenciales"
    );
}

// ── T7: warm — éxito, progreso y warmed_ms ──────────────────────────────────

#[test]
fn t7_warm_ok_records_env_and_warmed_ms() {
    let p2 = P2::new("p2s1-t7");
    p2.create("warm-a");
    p2.mark_auth("warm-a");
    let v = p2.ok_on(&["profile", "warm", "--agent", "warm-a"]);
    assert!(v["warmed_ms"].as_i64().unwrap() >= 0);
    assert!(p2.view("warm-a")["warmed_ms"].as_i64().is_some());

    // El fake registró el entorno sanitizado del spawn de warm.
    let dump: Value =
        serde_json::from_slice(&std::fs::read(p2.home("warm-a").join("fake-env.json")).unwrap())
            .unwrap();
    assert_eq!(
        dump["HERMES_HOME"].as_str().unwrap(),
        p2.home("warm-a").to_string_lossy()
    );
    assert_eq!(
        dump["HOME"].as_str().unwrap(),
        p2.san_home("warm-a").to_string_lossy()
    );
    assert_eq!(
        dump["GH_CONFIG_DIR"].as_str().unwrap(),
        p2.san_home("warm-a").join(".config/gh").to_string_lossy()
    );
    assert!(
        dump["XDG_CONFIG_HOME"]
            .as_str()
            .unwrap()
            .starts_with(&p2.san_home("warm-a").to_string_lossy().to_string())
    );
    assert!(dump["PATH"].as_str().unwrap().contains("/usr/bin:/bin"));
}

// ── T8: warm — timeout acotado, sin dejar warmed_ms ─────────────────────────

#[test]
fn t8_warm_timeout_bounded() {
    let p2 = P2::new("p2s1-t8");
    p2.create("warm-hang");
    p2.mark_auth("warm-hang");
    p2.set_mode("warm-hang", "hang");
    let err = p2.err_on_env(
        &["profile", "warm", "--agent", "warm-hang"],
        &[("AERIL_PROFILE_WARM_TIMEOUT_SECS", "1".into())],
    );
    assert!(
        err.contains("plazo") || err.contains("falló"),
        "mensaje: {err}"
    );
    let v = p2.view("warm-hang");
    assert_eq!(v["warmed_ms"], Value::Null);
    assert_eq!(v["lock_held"], false, "el lock se libera al fallar warm");
    assert_eq!(v["dir_state"], "ok", "warm fallido no corrompe el perfil");
}

// ── T9: lock de escritor — warm lento en background ─────────────────────────

#[test]
fn t9_writer_lock_contention_and_release() {
    let p2 = P2::new("p2s1-t9");
    p2.create("lock-a");
    p2.mark_auth("lock-a");
    p2.set_mode("lock-a", "slow:3");

    let mut warm = p2
        .cmd(true, &["profile", "warm", "--agent", "lock-a"], &[])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    assert!(
        p2.wait_for(&p2.home("lock-a").join("fake-pid"), 10),
        "el fake debe arrancar"
    );

    // Con el lock retenido: status lo ve, warm/remove fallan (fail-closed).
    let v = p2.ok_on(&["profile", "status", "--agent", "lock-a"]);
    assert_eq!(v["lock_held"], true);
    let err = p2.err_on(&["profile", "warm", "--agent", "lock-a"]);
    assert!(err.contains("escritor"), "mensaje: {err}");
    let err = p2.err_on(&[
        "profile",
        "remove",
        "--agent",
        "lock-a",
        "--confirm",
        "--clear-threads",
    ]);
    assert!(err.contains("escritor"), "mensaje: {err}");

    // El warm lento termina bien y libera el lock.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(Some(st)) = warm.try_wait() {
            assert!(st.success(), "warm lento debe completar");
            break;
        }
        assert!(Instant::now() < deadline, "warm lento no terminó");
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = warm.wait();
    let v = p2.ok_on(&["profile", "status", "--agent", "lock-a"]);
    assert_eq!(v["lock_held"], false);

    // Sin threads asignados, remove --confirm funciona y limpia todo.
    p2.ok_on(&["profile", "remove", "--agent", "lock-a", "--confirm"]);
    assert!(!p2.agent_dir("lock-a").exists());
    assert!(p2.list().is_empty());
}

// ── T10: remove — guardas explícitas, symlink swap, huérfanos ───────────────

#[test]
fn t10_remove_guards() {
    let p2 = P2::new("p2s1-t10");
    p2.create("rm-a");
    // Sin --confirm ⇒ rechazo.
    let err = p2.err_on(&["profile", "remove", "--agent", "rm-a"]);
    assert!(err.contains("--confirm"), "mensaje: {err}");
    assert!(p2.agent_dir("rm-a").exists());

    // Con thread asignado ⇒ rechazo salvo --clear-threads.
    let t = p2.assigned_thread();
    p2.ok_on(&["profile", "assign", "--agent", "rm-a", "--thread", &t]);
    let err = p2.err_on(&["profile", "remove", "--agent", "rm-a", "--confirm"]);
    assert!(err.contains("thread"), "mensaje: {err}");
    p2.ok_on(&[
        "profile",
        "remove",
        "--agent",
        "rm-a",
        "--confirm",
        "--clear-threads",
    ]);
    assert!(!p2.agent_dir("rm-a").exists());

    // Symlink swap ⇒ fail-closed (aun con metadata válida del otro lado).
    p2.create("rm-link");
    let target = p2.env.root.join("swap-target");
    std::fs::create_dir_all(target.join("home")).unwrap();
    std::fs::write(
        target.join("profile.json"),
        serde_json::json!({
            "agent_id": "rm-link", "created_ms": 1, "hermes_version": "",
            "provider": "nous", "model": null, "auth_status": "unknown"
        })
        .to_string(),
    )
    .unwrap();
    std::fs::remove_dir_all(p2.agent_dir("rm-link")).unwrap();
    std::os::unix::fs::symlink(&target, p2.agent_dir("rm-link")).unwrap();
    let err = p2.err_on(&["profile", "remove", "--agent", "rm-link", "--confirm"]);
    assert!(err.contains("symlink"), "mensaje: {err}");
    assert!(
        std::fs::symlink_metadata(p2.agent_dir("rm-link"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "el symlink no debe seguir en pie tras el rechazo"
    );
}

// ── T11: crash consistency — remove/create interrumpidos ────────────────────

#[test]
fn t11_interrupted_ops_reconcile() {
    let p2 = P2::new("p2s1-t11");
    p2.create("crash-a");
    // Remove interrumpido: dir borrado, registro intacto.
    std::fs::remove_dir_all(p2.agent_dir("crash-a")).unwrap();
    assert_eq!(p2.view("crash-a")["dir_state"], "missing");
    assert_eq!(p2.view("crash-a")["registered"], true);
    // remove es idempotente: limpia el registro sin tocar nada más.
    p2.ok_on(&["profile", "remove", "--agent", "crash-a", "--confirm"]);
    assert!(p2.list().is_empty());
    let reg: Value = serde_json::from_slice(
        &std::fs::read(p2.env.data.join("hermes").join("agents.json")).unwrap(),
    )
    .unwrap();
    assert!(reg["agents"].as_object().unwrap().is_empty());
    // El registro sigue siendo JSON válido tras las operaciones.
    assert!(reg["updated_ms"].as_i64().unwrap() > 0);
}

// ── T12: concurrencia del registro — sin corrupción ─────────────────────────

#[test]
fn t12_registry_lock_concurrent_ops() {
    let p2 = P2::new("p2s1-t12");
    let mut kids = Vec::new();
    for agent in ["par-a", "par-b", "par-c"] {
        kids.push(
            p2.cmd(true, &["profile", "create", "--agent", agent], &[])
                .spawn()
                .unwrap(),
        );
    }
    for k in kids {
        let out = k.wait_with_output().unwrap();
        assert!(out.status.success(), "create paralelo debe salir 0");
    }
    let reg: Value = serde_json::from_slice(
        &std::fs::read(p2.env.data.join("hermes").join("agents.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(reg["agents"].as_object().unwrap().len(), 3);

    // Mismo agent_id en paralelo ⇒ idempotente, una sola entrada.
    let mut kids = Vec::new();
    for _ in 0..3 {
        kids.push(
            p2.cmd(true, &["profile", "create", "--agent", "same-a"], &[])
                .spawn()
                .unwrap(),
        );
    }
    for k in kids {
        let out = k.wait_with_output().unwrap();
        assert!(out.status.success());
    }
    let reg: Value = serde_json::from_slice(
        &std::fs::read(p2.env.data.join("hermes").join("agents.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(reg["agents"].as_object().unwrap().len(), 4);
}

// ── T13: Runner — fail-closed sin asignación / sin auth ─────────────────────

#[test]
fn t13_runner_fail_closed() {
    let p2 = P2::new("p2s1-t13");
    // Sin perfil asignado ⇒ el run hermes se rechaza (sin fallback al host).
    let t = p2.assigned_thread();
    let out = p2.out_on(
        &[
            "run",
            "start",
            "--thread",
            &t,
            "--prompt",
            "hola",
            "--backend",
            "hermes",
        ],
        &[],
    );
    assert!(!out.status.success(), "run sin perfil debe fallar");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("no tiene perfil asignado"), "mensaje: {text}");

    // Perfil asignado pero sin auth verificada ⇒ rechazo fail-closed.
    p2.create("run-a");
    p2.ok_on(&["profile", "assign", "--agent", "run-a", "--thread", &t]);
    let out = p2.out_on(
        &[
            "run",
            "start",
            "--thread",
            &t,
            "--prompt",
            "hola",
            "--backend",
            "hermes",
        ],
        &[],
    );
    assert!(!out.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("sin autenticación verificada"),
        "mensaje: {text}"
    );
    // El rechazo no deja el lock tomado.
    assert_eq!(p2.view("run-a")["lock_held"], false);
}

// ── T14: Runner — env sanitizado + lock durante todo el writer ──────────────

#[test]
fn t14_runner_profile_env_and_writer_lock() {
    let p2 = P2::new("p2s1-t14");
    p2.create("live-a");
    p2.mark_auth("live-a");
    p2.set_mode("live-a", "linger"); // initialize ok; el turno queda colgado
    let t = p2.assigned_thread();
    p2.ok_on(&["profile", "assign", "--agent", "live-a", "--thread", &t]);

    let run = p2
        .cmd(
            true,
            &[
                "run",
                "start",
                "--thread",
                &t,
                "--prompt",
                "hola",
                "--backend",
                "hermes",
            ],
            &[],
        )
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    assert!(
        p2.wait_for(&p2.home("live-a").join("fake-pid"), 15),
        "el backend fake debe arrancar"
    );

    // Entorno del spawn = sanitizado del perfil (nunca el del host).
    let dump: Value =
        serde_json::from_slice(&std::fs::read(p2.home("live-a").join("fake-env.json")).unwrap())
            .unwrap();
    assert_eq!(
        dump["HERMES_HOME"].as_str().unwrap(),
        p2.home("live-a").to_string_lossy()
    );
    assert_eq!(
        dump["HOME"].as_str().unwrap(),
        p2.san_home("live-a").to_string_lossy()
    );
    assert_eq!(
        dump["XDG_DATA_HOME"].as_str().unwrap(),
        p2.san_home("live-a").join(".local/share").to_string_lossy()
    );
    assert_eq!(
        dump["GH_CONFIG_DIR"].as_str().unwrap(),
        p2.san_home("live-a").join(".config/gh").to_string_lossy()
    );

    // El lock vive TODO el writer: status lo ve; warm y un segundo run fallan.
    assert_eq!(p2.view("live-a")["lock_held"], true);
    let err = p2.err_on(&["profile", "warm", "--agent", "live-a"]);
    assert!(err.contains("escritor"), "mensaje: {err}");
    // Segundo run sobre OTRO thread del mismo perfil (workspace nuevo, sin
    // conflicto de lease): debe fallar por el lock del escritor.
    let t2 = p2.assigned_thread();
    p2.ok_on(&["profile", "assign", "--agent", "live-a", "--thread", &t2]);
    let out = p2.out_on(
        &[
            "run",
            "start",
            "--thread",
            &t2,
            "--prompt",
            "otra",
            "--backend",
            "hermes",
        ],
        &[],
    );
    assert!(!out.status.success(), "segundo run debe fallar (lock)");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("escritor"), "mensaje: {text}");

    // Simular muerte del run: matar backend + CLI; el lock debe liberarse.
    let pid: i32 = std::fs::read_to_string(p2.home("live-a").join("fake-pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    kill_pid(pid, libc::SIGKILL);
    kill_pid(run.id() as i32, libc::SIGTERM);
    reap(run);
    // Sin proceso vivo ⇒ remove explícito completa.
    p2.ok_on(&[
        "profile",
        "remove",
        "--agent",
        "live-a",
        "--confirm",
        "--clear-threads",
    ]);
    assert!(!p2.agent_dir("live-a").exists());
}

// ── T15: rollback — flag OFF preserva el comportamiento certificado ─────────

#[test]
fn t15_flag_off_preserves_certified_behavior() {
    let p2 = P2::new("p2s1-t15");
    // Preparar un perfil asignado (flag ON) y luego correr con flag OFF.
    p2.create("off-a");
    p2.mark_auth("off-a");
    let t = p2.assigned_thread();
    p2.ok_on(&["profile", "assign", "--agent", "off-a", "--thread", &t]);

    let dump_path = p2.env.root.join("off-env.json");
    let out = p2
        .cmd(
            false,
            &[
                "run",
                "start",
                "--thread",
                &t,
                "--prompt",
                "hola",
                "--backend",
                "hermes",
            ],
            &[("FAKE_DUMP", dump_path.to_string_lossy().into_owned())],
        )
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "flag OFF: run hermes con el fake debe completar igual que antes:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // El backend heredó el entorno del host (FAKE_DUMP) y NO el del perfil.
    let dump: Value = serde_json::from_slice(&std::fs::read(&dump_path).unwrap()).unwrap();
    assert_eq!(
        dump["HERMES_HOME"].as_str().unwrap(),
        "",
        "flag OFF no debe inyectar HERMES_HOME del perfil"
    );
    assert_ne!(
        dump["HOME"].as_str().unwrap(),
        p2.san_home("off-a").to_string_lossy(),
        "flag OFF no debe usar el HOME sanitizado del perfil"
    );
    assert!(
        !p2.home("off-a").join("fake-env.json").exists(),
        "flag OFF no debe usar el entorno sanitizado del perfil"
    );
    // El perfil no fue tocado (sin lock residual).
    assert_eq!(p2.view("off-a")["lock_held"], false);
    assert!(p2.agent_dir("off-a").exists());

    // Con flag ON, el backend mock sigue funcionando sin perfil (no-hermes).
    let script = p2.env.script(
        "mock",
        serde_json::json!({"final_message": "ok", "edits": []}),
    );
    let v = p2.ok_on(&[
        "run",
        "start",
        "--thread",
        &t,
        "--prompt",
        "mock",
        "--backend",
        &format!("mock:{}", script.display()),
    ]);
    assert_eq!(v["state"], "completed");
}
