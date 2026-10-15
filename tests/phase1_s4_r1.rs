//! S4-R1 — Borrado de worktrees a prueba de dos defectos P1:
//!
//! - **D1**: un supervisor/backend VIVO con lease ausente no debe permitir el
//!   borrado por defecto (el lease no es la única evidencia de ejecución).
//! - **D2**: una adquisición de lease concurrente no debe perder la carrera
//!   contra el borrado (serialización por transacción IMMEDIATE).
//!
//! Tests con procesos mock reales y repositorios descartables. Las barreras
//! `AERIL_TEST_RM_BARRIER_*` son instrumentación test-only (compilada fuera en
//! release): sincronizan los dos interleavings de la carrera de forma
//! determinista, sin sleeps de producción.

mod common;
use common::*;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

// ── helpers ──────────────────────────────────────────────────────────────────

fn db(env: &TestEnv) -> aeril::store::Store {
    aeril::store::Store::open(&env.data.join("aeril.db")).unwrap()
}

#[derive(Debug, Clone)]
struct RunRow {
    id: String,
    state: String,
    ws: Option<String>,
}

fn runs_of(env: &TestEnv, thread_id: &str) -> Vec<RunRow> {
    let store = db(env);
    let mut stmt = store
        .conn
        .prepare(
            "SELECT id, state, workspace_id FROM runs WHERE thread_id = ?1
             ORDER BY started_ms ASC, id ASC",
        )
        .unwrap();
    let rows = stmt
        .query_map([thread_id], |r| {
            Ok(RunRow {
                id: r.get(0)?,
                state: r.get(1)?,
                ws: r.get(2)?,
            })
        })
        .unwrap();
    rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
}

fn lease_row(env: &TestEnv, ws: &str) -> Option<(String, i64, bool)> {
    let store = db(env);
    aeril::store::leases::get(&store.conn, ws)
        .unwrap()
        .map(|i| {
            (
                i.run_id.clone(),
                i.pid,
                aeril::store::leases::owner_alive(&i),
            )
        })
}

fn spawn_run_bg(env: &TestEnv, thread_id: &str, script: &Path) -> ChildGuard {
    ChildGuard::new(
        Command::new(BIN)
            .args([
                "run",
                "start",
                "--thread",
                thread_id,
                "--prompt",
                "x",
                "--backend",
                &format!("mock:{}", script.display()),
            ])
            .env("AERIL_DATA_DIR", &env.data)
            .current_dir(&env.root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn run"),
    )
}

fn spawn_rm_with_barrier(
    env: &TestEnv,
    thread_id: &str,
    barrier_env: &str,
    dir: &Path,
) -> ChildGuard {
    ChildGuard::new(
        Command::new(BIN)
            .args(["worktree", "rm", "--thread", thread_id])
            .env("AERIL_DATA_DIR", &env.data)
            .env(barrier_env, dir)
            .current_dir(&env.root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn worktree rm"),
    )
}

/// Child que se mata al salir del scope (tests de carrera no deben dejar
/// procesos colgados si una aserción falla).
struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        ChildGuard { child: Some(child) }
    }

    fn pid(&self) -> u32 {
        self.child.as_ref().expect("child vivo").id()
    }

    fn wait_output(mut self, timeout_ms: u64) -> Option<Output> {
        let mut child = self.child.take().expect("child vivo");
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return Some(child.wait_with_output().unwrap()),
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => panic!("try_wait: {e}"),
            }
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(c) = &mut self.child {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn wait_until<F: FnMut() -> bool>(timeout_ms: u64, mut f: F) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn sigkill(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

fn event_payloads(env: &TestEnv, thread_id: &str, kind: &str) -> Vec<Value> {
    let out = env.aeril(&["export", "--thread", thread_id, "--format", "jsonl"]);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            v.get("type").and_then(Value::as_str) == Some("event")
                && v.get("kind").and_then(Value::as_str) == Some(kind)
        })
        .filter_map(|v| v.get("payload").cloned())
        .collect()
}

fn kill_mock_of(env: &TestEnv, thread_id: &str) {
    for p in event_payloads(env, thread_id, "backend.spawned") {
        if let Some(pid) = p["pid"].as_i64() {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    }
    std::thread::sleep(Duration::from_millis(400));
}

fn run_recover(env: &TestEnv) {
    let out = env.aeril(&["recover"]);
    assert!(out.status.success(), "recover falló");
}

fn barrier_dir(env: &TestEnv, name: &str) -> PathBuf {
    let dir = env.root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ── R1 (D1): lease ausente con supervisor/backend vivos ⇒ rechazo ───────────

#[test]
fn r1_sin_lease_supervisor_vivo_se_rechaza() {
    let env = TestEnv::new("s4r1-t1");
    let thread = env.thread_new("r1");
    let script = env.script(
        "r1",
        serde_json::json!({"message": "trabajando", "delay_ms": 30000, "edits": []}),
    );
    let sup = spawn_run_bg(&env, &thread, &script);
    assert!(
        wait_until(15000, || {
            runs_of(&env, &thread).iter().any(|r| r.state == "running")
                && !event_payloads(&env, &thread, "backend.spawned").is_empty()
        }),
        "el run/backend no arrancó"
    );
    let run = runs_of(&env, &thread).pop().unwrap();
    let ws = run.ws.clone().unwrap();
    let wt = env.data.join("worktrees").join(&ws);
    assert!(wt.is_dir());

    // Fixture controlado: borrar la fila del lease (workspace "sin lease").
    {
        let store = db(&env);
        let n = store
            .conn
            .execute("DELETE FROM leases WHERE workspace_id = ?1", [&ws])
            .unwrap();
        assert_eq!(n, 1, "había exactamente un lease");
    }
    assert!(lease_row(&env, &ws).is_none());

    // D1: supervisor y backend VIVOS, lease ausente ⇒ el borrado debe rechazarse.
    let out = env.aeril(&["worktree", "rm", "--thread", &thread]);
    assert!(
        !out.status.success(),
        "D1: no debe borrarse con supervisor vivo aunque falte el lease (stderr: {})",
        String::from_utf8_lossy(&out.stderr)
    );
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        err.contains("supervisor"),
        "el motivo debe nombrar al supervisor: {err}"
    );
    assert!(wt.is_dir(), "el worktree queda intacto");

    // El backend mock sigue vivo (el intento no lo afectó).
    let mock_pid = event_payloads(&env, &thread, "backend.spawned")[0]["pid"]
        .as_i64()
        .unwrap() as i32;
    assert_eq!(
        unsafe { libc::kill(mock_pid, 0) },
        0,
        "el backend debe seguir vivo"
    );

    // Limpieza.
    kill_mock_of(&env, &thread);
    sigkill(sup.pid());
    let _ = sup.wait_output(5000);
}

// ── R2: supervisor desconocido ⇒ rechazo por defecto ────────────────────────

#[test]
fn r2_supervisor_desconocido_se_rechaza() {
    let env = TestEnv::new("s4r1-t2");
    let thread = env.thread_new("r2");
    let ws = format!("ws-fixture-r2-{}", aeril::ids::new_id());
    let wt = env.data.join("worktrees").join(&ws);
    // Fixture: run sin resolver SIN evidencia de supervisor (ni run.started ni
    // lease). Es el estado "identidad desconocida".
    {
        let store = db(&env);
        store
            .conn
            .execute(
                "INSERT INTO runs (id, thread_id, workspace_id, repo_path, worktree_path,
                                   branch, base_oid, backend, backend_version, state, started_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'mock', 'test', 'running', ?8)",
                rusqlite::params![
                    aeril::ids::new_id(),
                    thread,
                    ws,
                    env.repo.to_string_lossy(),
                    wt.to_string_lossy(),
                    format!("aeril/ws/{ws}"),
                    env.head_oid(),
                    aeril::timeutil::now_ms()
                ],
            )
            .unwrap();
    }
    let out = env.aeril(&["worktree", "rm", "--thread", &thread]);
    assert!(
        !out.status.success(),
        "supervisor desconocido ⇒ rechazo por defecto"
    );
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(err.contains("supervisor"), "{err}");
    assert!(!wt.exists(), "no se creó ni se tocó nada");
}

// ── R3: --unsafe-ok explícito (sin lease) preserva la historia ──────────────

#[test]
fn r3_unsafe_ok_permite_borrado_sin_lease_y_preserva_historia() {
    let env = TestEnv::new("s4r1-t3");
    let thread = env.thread_new("r3");
    let script = env.script("r3", mock_edit("NOTES.md", "# r3\n", "x"));
    assert_eq!(env.run_start(&thread, "x", &script)["state"], "completed");
    let run = runs_of(&env, &thread).pop().unwrap();
    let ws = run.ws.clone().unwrap();
    let wt = env.data.join("worktrees").join(&ws);
    let ready = env.ready_snapshots(&thread);
    assert_eq!(ready.len(), 1);
    let (snap, oid) = ready[0].clone();
    let c = env.aeril_ok(&[
        "comment",
        "add",
        "--thread",
        &thread,
        "--snapshot",
        &snap,
        "--path",
        "NOTES.md",
        "--start",
        "1",
        "--end",
        "1",
        "--text",
        "r3",
    ]);
    let comment_id = c["comment_id"].as_str().unwrap().to_string();
    let head_before = env.head_oid();

    // Fixture: run "sin resolver" sin evidencia de supervisor y sin lease.
    {
        let store = db(&env);
        store
            .conn
            .execute(
                "DELETE FROM events WHERE thread_id = ?1 AND kind = 'run.started'",
                [&thread],
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE runs SET state = 'running', ended_ms = NULL WHERE id = ?1",
                [&run.id],
            )
            .unwrap();
    }

    // Default: rechazado (supervisor desconocido).
    let out = env.aeril(&["worktree", "rm", "--thread", &thread]);
    assert!(!out.status.success());
    assert!(wt.is_dir());

    // --unsafe-ok: reconocimiento explícito de riesgo; sin lease, procede.
    let out = env.aeril(&["worktree", "rm", "--thread", &thread, "--unsafe-ok"]);
    assert!(
        out.status.success(),
        "override explícito: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!wt.exists());
    let v = parse_json(&out.stdout);
    assert_eq!(v["removed"], true);
    assert_eq!(v["unsafe_ok"], true, "el JSON declara el override");

    // Historia intacta: checkpoints, comentarios, refs, repo principal.
    assert_eq!(env.ready_snapshots(&thread), vec![(snap.clone(), oid)]);
    let show = env.aeril_ok(&[
        "checkpoint",
        "show",
        "--thread",
        &thread,
        "--snapshot",
        &snap,
    ]);
    assert!(show["patch"].as_str().unwrap().contains("# r3"));
    let view = env.aeril_ok(&[
        "comment",
        "show",
        "--thread",
        &thread,
        "--comment",
        &comment_id,
    ]);
    assert_eq!(view["text"], "r3");
    let branch = format!("refs/heads/aeril/ws/{ws}");
    assert!(
        !env.rev_parse(&branch).is_empty(),
        "la rama del workspace sobrevive"
    );
    assert_eq!(env.head_oid(), head_before);
}

// ── R4 (D2): la adquisición concurrente gana ⇒ el borrado aborta ────────────

#[test]
fn r4_adquisicion_concurrente_gana() {
    let env = TestEnv::new("s4r1-t4");
    let thread = env.thread_new("r4");
    let s1 = env.script("r4a", mock_edit("NOTES.md", "# r4-1\n", "x"));
    assert_eq!(env.run_start(&thread, "x", &s1)["state"], "completed");
    let run1 = runs_of(&env, &thread).pop().unwrap();
    let ws = run1.ws.clone().unwrap();
    let wt = env.data.join("worktrees").join(&ws);
    assert!(lease_row(&env, &ws).is_none(), "deletable de entrada");

    // rm queda pausado tras la preparación, antes de su decisión final.
    let dir = barrier_dir(&env, "barrier-r4");
    let rm = spawn_rm_with_barrier(&env, &thread, "AERIL_TEST_RM_BARRIER_PRE_TX", &dir);
    assert!(
        wait_until(10000, || dir.join("ready").exists()),
        "rm no llegó a la barrera"
    );

    // Competidor: un run nuevo reutiliza el workspace y ADQUIERE su lease
    // antes de la decisión final de rm.
    let s2 = env.script(
        "r4b",
        serde_json::json!({
            "message": "x", "final_message": "fin", "delay_ms": 1500,
            "edits": [{"op": "write", "path": "NOTES.md", "content": "# r4-2\n"}],
        }),
    );
    let comp = spawn_run_bg(&env, &thread, &s2);
    assert!(
        wait_until(15000, || lease_row(&env, &ws).is_some()),
        "el competidor no adquirió el lease"
    );

    // Liberar la barrera: rm decide con el lease ya adquirido.
    std::fs::write(dir.join("go"), b"1").unwrap();
    let out = rm.wait_output(20000).expect("rm no terminó");
    assert!(
        !out.status.success(),
        "D2: la adquisición ganó; rm debe abortar (stderr: {})",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(wt.is_dir(), "el worktree del run activo queda intacto");

    // El run activo completa sin perder su workspace verificado.
    let out2 = comp
        .wait_output(30000)
        .expect("el run competidor no terminó");
    let summary = parse_json(&out2.stdout);
    assert_eq!(summary["state"], "completed", "{summary:?}");
    let runs = runs_of(&env, &thread);
    assert_eq!(runs.len(), 2);
    assert_eq!(
        runs[1].ws.as_deref(),
        Some(ws.as_str()),
        "reutilizó el workspace"
    );
    assert!(
        std::fs::read_to_string(wt.join("NOTES.md"))
            .unwrap()
            .contains("# r4-2"),
        "el competidor escribió en su worktree intacto"
    );
    assert_eq!(
        env.ready_snapshots(&thread).len(),
        2,
        "ambos turnos capturaron"
    );
}

// ── R5 (D2): el borrado gana ⇒ el competidor recrea, no escribe en lo borrado ─

#[test]
fn r5_borrado_gana_y_el_competidor_recrea() {
    let env = TestEnv::new("s4r1-t5");
    let thread = env.thread_new("r5");
    let s1 = env.script("r5a", mock_edit("NOTES.md", "# r5-1\n", "x"));
    assert_eq!(env.run_start(&thread, "x", &s1)["state"], "completed");
    let run1 = runs_of(&env, &thread).pop().unwrap();
    let ws = run1.ws.clone().unwrap();
    let wt = env.data.join("worktrees").join(&ws);

    // rm entra a la sección crítica (transacción IMMEDIATE) y queda pausado
    // DENTRO de la transacción, con el lock de escritura tomado.
    let dir = barrier_dir(&env, "barrier-r5");
    let rm = spawn_rm_with_barrier(&env, &thread, "AERIL_TEST_RM_BARRIER_IN_TX", &dir);
    assert!(
        wait_until(10000, || dir.join("ready").exists()),
        "rm no entró a la sección crítica"
    );

    // El competidor intenta adquirir el lease mientras rm sostiene el lock.
    let s2 = env.script(
        "r5b",
        serde_json::json!({
            "message": "x", "final_message": "fin", "delay_ms": 1200,
            "edits": [{"op": "write", "path": "NOTES.md", "content": "# r5-2\n"}],
        }),
    );
    let comp = spawn_run_bg(&env, &thread, &s2);
    // Ventana acotada (< busy_timeout): la adquisición no puede completar.
    std::thread::sleep(Duration::from_millis(700));
    assert!(
        lease_row(&env, &ws).is_none(),
        "la adquisición no debe completar bajo el lock del borrado"
    );

    // Deletion wins: liberar la barrera ⇒ borra + commit.
    std::fs::write(dir.join("go"), b"1").unwrap();
    let out = rm.wait_output(20000).expect("rm no terminó");
    assert!(
        out.status.success(),
        "el borrado debía ganar: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(parse_json(&out.stdout)["removed"], true);
    assert!(!wt.exists(), "worktree borrado");

    // El competidor despierta y aplica sus reglas ordinarias: verificación
    // falla (Missing) ⇒ crea un workspace NUEVO; jamás escribe en el borrado.
    let out2 = comp
        .wait_output(30000)
        .expect("el run competidor no terminó");
    let summary = parse_json(&out2.stdout);
    assert_eq!(summary["state"], "completed", "{summary:?}");
    let runs = runs_of(&env, &thread);
    assert_eq!(runs.len(), 2);
    let new_ws = runs[1].ws.clone().unwrap();
    assert_ne!(new_ws, ws, "el competidor crea un workspace nuevo");
    let new_wt = env.data.join("worktrees").join(&new_ws);
    assert!(new_wt.is_dir());
    assert_ne!(new_wt, wt);
    assert!(!wt.exists(), "no se escribe en el path borrado");
    assert!(
        std::fs::read_to_string(new_wt.join("NOTES.md"))
            .unwrap()
            .contains("# r5-2"),
        "el trabajo del competidor vive en su workspace nuevo"
    );
    assert!(lease_row(&env, &ws).is_none(), "sin lease stale residual");
}

// ── R6: --unsafe-ok no pisa un lease (fence final) ──────────────────────────

#[test]
fn r6_unsafe_ok_no_pisa_lease() {
    let env = TestEnv::new("s4r1-t6");
    let thread = env.thread_new("r6");
    let script = env.script("r6", mock_edit("NOTES.md", "# r6\n", "x"));
    assert_eq!(env.run_start(&thread, "x", &script)["state"], "completed");
    let run = runs_of(&env, &thread).pop().unwrap();
    let ws = run.ws.clone().unwrap();
    let wt = env.data.join("worktrees").join(&ws);

    // Fixture: lease STALE (dueño probadamente muerto: pid inexistente).
    {
        let store = db(&env);
        store
            .conn
            .execute(
                "INSERT INTO leases (workspace_id, run_id, pid, pid_start, acquired_ms,
                                     renewed_ms, ttl_ms)
                 VALUES (?1, ?2, 99999999, 0, 0, ?3, 60000)",
                rusqlite::params![ws, run.id, aeril::timeutil::now_ms()],
            )
            .unwrap();
    }
    assert!(!lease_row(&env, &ws).unwrap().2, "dueño muerto");

    // R6: --unsafe-ok NO puede pisar el lease presente (fence final).
    let out = env.aeril(&["worktree", "rm", "--thread", &thread, "--unsafe-ok"]);
    assert!(
        !out.status.success(),
        "R6: --unsafe-ok no puede pisar un lease (stderr: {})",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(wt.is_dir());

    // Ciclo de vida existente: liberar el lease stale y reintentar.
    let out = env.aeril(&["lease", "release", "--workspace", &ws]);
    assert!(
        out.status.success(),
        "release de dueño muerto: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = env.aeril(&["worktree", "rm", "--thread", &thread, "--unsafe-ok"]);
    assert!(out.status.success());
    assert!(!wt.exists());
}

// ── R7: tras reconciliar y sin lease, el borrado normal sigue funcionando ───

#[test]
fn r7_flujo_de_recuperacion() {
    let env = TestEnv::new("s4r1-t7");
    let thread = env.thread_new("r7");
    let script = env.script(
        "r7",
        serde_json::json!({"message": "x", "delay_ms": 5000, "edits": []}),
    );
    let sup = spawn_run_bg(&env, &thread, &script);
    assert!(
        wait_until(15000, || runs_of(&env, &thread)
            .iter()
            .any(|r| r.state == "running")),
        "el run no quedó activo"
    );
    let run = runs_of(&env, &thread).pop().unwrap();
    let ws = run.ws.clone().unwrap();
    let wt = env.data.join("worktrees").join(&ws);

    // Supervisor muerto + backend huérfano muerto + reconciliación.
    sigkill(sup.pid());
    let _ = sup.wait_output(5000);
    kill_mock_of(&env, &thread);
    run_recover(&env);
    assert_eq!(runs_of(&env, &thread).pop().unwrap().state, "failed");

    // El lease stale restante bloquea hasta resolverlo por su ciclo de vida.
    let out = env.aeril(&["worktree", "rm", "--thread", &thread]);
    assert!(
        !out.status.success(),
        "un lease stale bloquea el borrado (fence final)"
    );
    let out = env.aeril(&["lease", "release", "--workspace", &ws]);
    assert!(
        out.status.success(),
        "release de dueño muerto: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Sin lease y con evidencia de terminación: borrado normal.
    let out = env.aeril(&["worktree", "rm", "--thread", &thread]);
    assert!(
        out.status.success(),
        "flujo de recuperación: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!wt.exists());
}
