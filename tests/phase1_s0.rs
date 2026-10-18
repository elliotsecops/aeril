//! S0 — Reconciliación durable de runs interrumpidos.
//!
//! Repositorios y stores aislados; procesos Aeril reales + backend mock; los
//! fixtures de runs stale se construyen con inserciones directas en la DB
//! (evidencia de supervisor/backend incluida), nunca contra el repo real.

mod common;
use common::*;
use serde_json::Value;
use std::process::Command;
use std::time::{Duration, Instant};

fn events_of(env: &TestEnv, thread: &str, kind: &str) -> Vec<Value> {
    let out = env.aeril(&["export", "--thread", thread, "--format", "jsonl"]);
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            v.get("type").and_then(Value::as_str) == Some("event")
                && v.get("kind").and_then(Value::as_str) == Some(kind)
        })
        .collect()
}

fn runs_of(env: &TestEnv, thread: &str) -> Vec<Value> {
    let out = env.aeril(&["export", "--thread", thread, "--format", "jsonl"]);
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v.get("type").and_then(Value::as_str) == Some("run"))
        .collect()
}

fn spawn_run(env: &TestEnv, thread: &str, delay_ms: u64, content: &str) -> std::process::Child {
    let script = env.script(
        &format!("s0-{delay_ms}-{}", content.len()),
        serde_json::json!({
            "message": "trabajando",
            "final_message": "listo",
            "delay_ms": delay_ms,
            "edits": [{"op": "write", "path": "NOTES.md", "content": content}],
        }),
    );
    Command::new(BIN)
        .args([
            "run",
            "start",
            "--thread",
            thread,
            "--prompt",
            "x",
            "--backend",
            &format!("mock:{}", script.display()),
        ])
        .env("AERIL_DATA_DIR", &env.data)
        .current_dir(&env.root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn run")
}

fn wait_until_session(env: &TestEnv, thread: &str, timeout_s: u64) {
    let deadline = Instant::now() + Duration::from_secs(timeout_s);
    while Instant::now() < deadline {
        if !events_of(env, thread, "engine.session").is_empty() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("el run no llegó a establecer sesión");
}

/// PID de un proceso ya muerto y reapeado (evidencia de supervisor muerto).
fn dead_pid() -> i64 {
    let mut victim = Command::new("sleep").arg("30").spawn().unwrap();
    let pid = victim.id() as i64;
    victim.kill().unwrap();
    victim.wait().unwrap();
    pid
}

fn db(env: &TestEnv) -> aeril::store::Store {
    aeril::store::Store::open(&env.data.join("aeril.db")).unwrap()
}

#[allow(clippy::too_many_arguments)]
fn insert_stale_run(
    env: &TestEnv,
    thread: &str,
    state: &str,
    workspace_id: Option<&str>,
    base_oid: Option<&str>,
    supervisor: Option<(i64, u64)>,
    backend: Option<(i64, u64)>,
) -> String {
    let store = db(env);
    let run_id = aeril::ids::new_id();
    let now = aeril::timeutil::now_ms();
    store
        .conn
        .execute(
            "INSERT INTO runs (id, thread_id, workspace_id, repo_path, worktree_path, branch, base_oid,
                               backend, backend_version, state, started_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'mock', 'test', ?8, ?9)",
            rusqlite::params![
                run_id,
                thread,
                workspace_id,
                env.repo.to_string_lossy(),
                workspace_id.map(|w| env.data.join("worktrees").join(w).to_string_lossy().into_owned()),
                workspace_id.map(|w| format!("aeril/ws/{w}")),
                base_oid,
                state,
                now
            ],
        )
        .unwrap();
    let mut store = db(env);
    if let Some((pid, start)) = supervisor {
        aeril::store::events::append_typed(
            &mut store.conn,
            thread,
            Some(&run_id),
            aeril::store::events::K_RUN_STARTED,
            "system",
            &aeril::store::events::payload::RunStarted {
                backend: "mock".into(),
                goal: "fixture".into(),
                workspace_id: workspace_id.unwrap_or("fixture-ws").into(),
                repo_path: env.repo.to_string_lossy().into(),
                worktree_path: String::new(),
                branch: String::new(),
                base_oid: base_oid.unwrap_or_default().into(),
                profile: None,
                resumed_session: None,
                supervisor_pid: Some(pid),
                supervisor_pid_start: Some(start),
            },
        )
        .unwrap();
    } else {
        // Superviso desconocido: run.started sin evidencia de supervisor.
        aeril::store::events::append_typed(
            &mut store.conn,
            thread,
            Some(&run_id),
            aeril::store::events::K_RUN_STARTED,
            "system",
            &aeril::store::events::payload::RunStarted {
                backend: "mock".into(),
                goal: "fixture legado".into(),
                workspace_id: workspace_id.unwrap_or("fixture-ws").into(),
                repo_path: env.repo.to_string_lossy().into(),
                worktree_path: String::new(),
                branch: String::new(),
                base_oid: base_oid.unwrap_or_default().into(),
                profile: None,
                resumed_session: None,
                supervisor_pid: None,
                supervisor_pid_start: None,
            },
        )
        .unwrap();
    }
    if let Some((pid, start)) = backend {
        aeril::store::events::append_typed(
            &mut store.conn,
            thread,
            Some(&run_id),
            aeril::store::events::K_BACKEND_SPAWNED,
            "system",
            &aeril::store::events::payload::BackendSpawned {
                run_id: run_id.clone(),
                pid,
                pid_start: start,
                program: "sleep".into(),
            },
        )
        .unwrap();
    }
    run_id
}

fn latest_run_id(env: &TestEnv, thread: &str) -> String {
    db(env)
        .conn
        .query_row(
            "SELECT id FROM runs WHERE thread_id = ?1 ORDER BY started_ms DESC, id DESC LIMIT 1",
            [thread],
            |r| r.get(0),
        )
        .unwrap()
}

fn latest_workspace(env: &TestEnv, thread: &str) -> Option<String> {
    db(env)
        .conn
        .query_row(
            "SELECT workspace_id FROM runs WHERE thread_id = ?1 AND workspace_id IS NOT NULL
             ORDER BY started_ms DESC, id DESC LIMIT 1",
            [thread],
            |r| r.get(0),
        )
        .ok()
}

// ── T1: SIGKILL mid-turn → interrumpido durable e idempotente ────────────────

#[test]
fn s0_sigkill_mid_turn_reconcilia_duradero_e_idempotente() {
    let env = TestEnv::new("s0-t1");
    let thread = env.thread_new("t1");
    let mut child = spawn_run(&env, &thread, 20_000, "# A (t1)\n");
    wait_until_session(&env, &thread, 25);
    let run_id = latest_run_id(&env, &thread);

    // Muerte abrupta del SUPERVISOR (como un cierre forzado de Aeril).
    let _ = child.kill();
    let _ = child.wait();
    std::thread::sleep(Duration::from_millis(300));

    let rec = env.aeril_ok(&["recover"]);
    let arr = rec.as_array().unwrap();
    let interrupted: Vec<&Value> = arr
        .iter()
        .filter(|e| e["action"] == "run_interrupted")
        .collect();
    assert_eq!(interrupted.len(), 1, "un run reconciliado: {rec}");
    assert_eq!(interrupted[0]["run_id"], run_id.as_str());

    // Hecho durable: run.ended failed con razón "interrumpido"; nada fabricado.
    let ended = events_of(&env, &thread, "run.ended");
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0]["payload"]["state"], "failed");
    assert!(
        ended[0]["payload"]["reason"]
            .as_str()
            .unwrap()
            .contains("interrumpido"),
        "razón: {}",
        ended[0]["payload"]["reason"]
    );
    assert!(events_of(&env, &thread, "checkpoint.ready").is_empty());
    let run = runs_of(&env, &thread).pop().unwrap();
    assert_eq!(run["state"], "failed");

    // Idempotencia: repetir no agrega hechos terminales.
    for _ in 0..2 {
        let again = env.aeril_ok(&["recover"]);
        assert!(
            !again
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["action"] == "run_interrupted"),
            "sin reconciliaciones nuevas: {again}"
        );
    }
    assert_eq!(events_of(&env, &thread, "run.ended").len(), 1);
}

// ── T2: supervisor VIVO no se toca ───────────────────────────────────────────

#[test]
fn s0_supervisor_vivo_no_se_reconcilia() {
    let env = TestEnv::new("s0-t2");
    let thread = env.thread_new("t2");
    let child = spawn_run(&env, &thread, 6000, "# A (t2)\n");
    wait_until_session(&env, &thread, 25);
    let run_id = latest_run_id(&env, &thread);

    let rec = env.aeril_ok(&["recover"]);
    let arr = rec.as_array().unwrap();
    assert!(
        !arr.iter().any(|e| e["action"] == "run_interrupted"),
        "un supervisor vivo no se reconcilia: {rec}"
    );
    assert!(
        arr.iter()
            .any(|e| e["action"] == "run_supervisor_alive" && e["run_id"] == run_id.as_str()),
        "se reporta supervisor vivo: {rec}"
    );

    // El run sigue su curso y completa normalmente (backend intacto).
    let out = child.wait_with_output().unwrap();
    let summary = parse_json(&out.stdout);
    assert_eq!(summary["state"], "completed", "{summary}");
    let ended = events_of(&env, &thread, "run.ended");
    assert_eq!(ended.last().unwrap()["payload"]["state"], "completed");
}

// ── T3: fixtures starting/running con supervisor muerto ──────────────────────

#[test]
fn s0_starting_y_running_con_supervisor_muerto_se_reconcilian() {
    let env = TestEnv::new("s0-t3");
    let thread = env.thread_new("t3");
    let dead = dead_pid();
    let r1 = insert_stale_run(&env, &thread, "starting", None, None, Some((dead, 0)), None);
    let r2 = insert_stale_run(&env, &thread, "running", None, None, Some((dead, 0)), None);

    let rec = env.aeril_ok(&["recover"]);
    let interrupted: Vec<&Value> = rec
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"] == "run_interrupted")
        .collect();
    assert_eq!(interrupted.len(), 2, "{rec}");
    let ids: Vec<&str> = interrupted
        .iter()
        .map(|e| e["run_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&r1.as_str()) && ids.contains(&r2.as_str()));
    // Sin evidencia de backend ⇒ estado inseguro explícito.
    assert!(
        interrupted.iter().all(|e| e["unsafe_state"] == true),
        "{interrupted:?}"
    );

    // Un único run.ended por run, incluso repitiendo.
    env.aeril_ok(&["recover"]);
    assert_eq!(events_of(&env, &thread, "run.ended").len(), 2);
}

// ── T4: backend huérfano VIVO se termina y el reuso queda habilitado ─────────

#[test]
fn s0_backend_huerfano_se_termina_antes_de_reusar() {
    use std::os::unix::process::CommandExt;
    let env = TestEnv::new("s0-t4");
    let thread = env.thread_new("t4");
    let s1 = env.script("s1", mock_edit("NOTES.md", "# uno (t4)\n", "uno"));
    let run1 = env.run_start(&thread, "uno", &s1);
    assert_eq!(run1["state"], "completed");
    let ws = latest_workspace(&env, &thread).unwrap();
    let base = db(&env)
        .conn
        .query_row(
            "SELECT base_oid FROM runs WHERE thread_id = ?1 ORDER BY started_ms DESC LIMIT 1",
            [&thread],
            |r| r.get::<_, String>(0),
        )
        .unwrap();

    // Backend huérfano VIVO: proceso real con su propio grupo (como los lanza Aeril).
    let mut victim = Command::new("sleep")
        .arg("300")
        .process_group(0)
        .spawn()
        .unwrap();
    let vpid = victim.id() as i64;
    let vstart = aeril::store::leases::process_start_time(vpid).unwrap();

    let dead = dead_pid();
    let stale = insert_stale_run(
        &env,
        &thread,
        "running",
        Some(&ws),
        Some(&base),
        Some((dead, 0)),
        Some((vpid, vstart)),
    );

    let rec = env.aeril_ok(&["recover"]);
    let interrupted = rec
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["action"] == "run_interrupted" && e["run_id"] == stale.as_str())
        .expect("run reconciliado")
        .clone();
    assert_eq!(interrupted["unsafe_state"], false, "{rec}");
    assert!(
        interrupted["backend"]
            .as_str()
            .unwrap()
            .contains("terminado durante la recuperación"),
        "{interrupted}"
    );
    // El huérfano fue terminado de verdad (SIGKILL al grupo, starttime
    // verificado); un zombie reapeable cuenta como terminado.
    assert!(
        aeril::recovery::process_dead_or_zombie(vpid),
        "el backend huérfano debe estar muerto"
    );
    let _ = victim.wait();

    // Sin backend vivo ni incertidumbre ⇒ el workspace SE reutiliza.
    let s2 = env.script("s2", mock_edit("NOTES.md", "# dos (t4)\n", "dos"));
    let run2 = env.run_start(&thread, "dos", &s2);
    assert_eq!(run2["state"], "completed", "{run2}");
    assert_eq!(
        latest_workspace(&env, &thread).unwrap(),
        ws,
        "reuso permitido tras verificar terminación"
    );
}

// ── T5: backend DESCONOCIDO bloquea el reuso (workspace nuevo, sin tocar el viejo) ─

#[test]
fn s0_backend_desconocido_bloquea_reuso_y_no_toca_el_viejo() {
    let env = TestEnv::new("s0-t5");
    let thread = env.thread_new("t5");
    let s1 = env.script("s1", mock_edit("NOTES.md", "# uno (t5)\n", "uno"));
    let run1 = env.run_start(&thread, "uno", &s1);
    assert_eq!(run1["state"], "completed");
    let ws = latest_workspace(&env, &thread).unwrap();
    let base = db(&env)
        .conn
        .query_row(
            "SELECT base_oid FROM runs WHERE thread_id = ?1 ORDER BY started_ms DESC LIMIT 1",
            [&thread],
            |r| r.get::<_, String>(0),
        )
        .unwrap();
    let old_wt = env.data.join("worktrees").join(&ws);
    let old_content = std::fs::read_to_string(old_wt.join("NOTES.md")).unwrap();

    // Run stale SIN evidencia de backend (supervisor muerto).
    let dead = dead_pid();
    let stale = insert_stale_run(
        &env,
        &thread,
        "running",
        Some(&ws),
        Some(&base),
        Some((dead, 0)),
        None,
    );

    let rec = env.aeril_ok(&["recover"]);
    let interrupted = rec
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["action"] == "run_interrupted" && e["run_id"] == stale.as_str())
        .expect("run reconciliado")
        .clone();
    assert_eq!(interrupted["unsafe_state"], true, "{rec}");
    assert_eq!(
        events_of(&env, &thread, "workspace.recovery_unsafe").len(),
        1,
        "estado inseguro explícito y durable"
    );

    // Reuso bloqueado: run nuevo en OTRO workspace; el viejo queda intacto.
    let s2 = env.script("s2", mock_edit("NOTES.md", "# dos (t5)\n", "dos"));
    let run2 = env.run_start(&thread, "dos", &s2);
    assert_eq!(run2["state"], "completed", "{run2}");
    let new_ws = latest_workspace(&env, &thread).unwrap();
    assert_ne!(new_ws, ws, "no se reutiliza un workspace sin evidencia");
    assert_eq!(
        events_of(&env, &thread, "workspace.reuse_blocked").len(),
        1,
        "bloqueo auditado"
    );
    assert_eq!(
        std::fs::read_to_string(old_wt.join("NOTES.md")).unwrap(),
        old_content,
        "el workspace viejo no fue tocado"
    );
}

// ── T6: un segundo thread y sus artefactos quedan intactos ───────────────────

#[test]
fn s0_segundo_thread_y_artefactos_preservados() {
    let env = TestEnv::new("s0-t6");
    // Thread A: trabajo revisado (checkpoint + comentario).
    let thread_a = env.thread_new("t6a");
    let sa = env.script(
        "a",
        mock_edit("NOTES.md", "# de A (t6)\n\ncomentable\n", "a"),
    );
    let run_a = env.run_start(&thread_a, "a", &sa);
    let sid_a = run_a["snapshot"]["snapshot_id"]
        .as_str()
        .unwrap()
        .to_string();
    let oid_a = run_a["snapshot"]["oid"].as_str().unwrap().to_string();
    let ca = env.aeril_ok(&[
        "comment",
        "add",
        "--thread",
        &thread_a,
        "--snapshot",
        &sid_a,
        "--path",
        "NOTES.md",
        "--start",
        "3",
        "--end",
        "3",
        "--text",
        "nota A",
    ]);
    let cid_a = ca["comment_id"].as_str().unwrap().to_string();
    let ref_before = env.rev_parse(&format!("refs/aeril/checkpoints/{sid_a}"));

    // Thread B: run que será interrumpido.
    let thread_b = env.thread_new("t6b");
    let mut child = spawn_run(&env, &thread_b, 20_000, "# B (t6)\n");
    wait_until_session(&env, &thread_b, 25);
    let _ = child.kill();
    let _ = child.wait();
    std::thread::sleep(Duration::from_millis(300));
    env.aeril_ok(&["recover"]);

    // A intacto: ref, run, comentario (blob verificado) y estado del comentario.
    assert_eq!(
        env.rev_parse(&format!("refs/aeril/checkpoints/{sid_a}")),
        ref_before
    );
    assert_eq!(
        runs_of(&env, &thread_a).pop().unwrap()["state"],
        "completed"
    );
    let view = env.aeril_ok(&[
        "comment",
        "show",
        "--thread",
        &thread_a,
        "--comment",
        &cid_a,
    ]);
    assert_eq!(view["blob_verified"], true);
    assert_eq!(view["snapshot_oid"].as_str().unwrap(), oid_a);
    assert_eq!(view["state"], "open");
    // B reconciliado, con su run.ended failed.
    assert_eq!(
        events_of(&env, &thread_b, "run.ended")[0]["payload"]["state"],
        "failed"
    );
}

// ── T7: checkpoint legítimo pendiente se recupera; el run NO. ────────────────

#[test]
fn s0_checkpoint_pendiente_se_recupera_y_el_run_no_se_fabrica() {
    let env = TestEnv::new("s0-t7");
    let thread = env.thread_new("t7");
    let script = env.script("s", mock_edit("NOTES.md", "# capturado (t7)\n", "x"));
    let out = env.aeril_env(
        &[
            "run",
            "start",
            "--thread",
            &thread,
            "--prompt",
            "x",
            "--backend",
            &format!("mock:{}", script.display()),
        ],
        &[("AERIL_CRASH_AFTER", "verify")],
    );
    assert_eq!(
        signal_of(&out.status),
        Some(9),
        "SIGKILL entre verify y ready"
    );

    let rec = env.aeril_ok(&["recover"]);
    let arr = rec.as_array().unwrap();
    assert!(
        arr.iter().any(|e| e["action"] == "completed_ready"),
        "checkpoint del protocolo Phase 0 recuperado: {rec}"
    );
    assert!(
        arr.iter().any(|e| e["action"] == "run_interrupted"),
        "el run sigue siendo un fallo explícito: {rec}"
    );
    // Contenido del checkpoint = objetos inmutables; run.ended failed, no completado.
    let list = env.checkpoint_list(&thread);
    assert_eq!(list.iter().filter(|c| c["state"] == "ready").count(), 1);
    let ended = events_of(&env, &thread, "run.ended");
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0]["payload"]["state"], "failed");
}
