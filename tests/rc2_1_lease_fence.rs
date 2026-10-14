//! RC2.1 — Fence de lease: un dueño VIVO no puede perder el workspace por
//! `lease release --force`, y un dueño obsoleto no puede publicar checkpoints
//! ni reportar completitud (verificación dentro de la MISMA transacción que
//! persiste el evento).

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

fn spawn_slow_run(
    env: &TestEnv,
    thread: &str,
    delay_ms: u64,
    content: &str,
) -> std::process::Child {
    let script = env.script(
        &format!("slow-{delay_ms}-{}", content.len()),
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
        .expect("spawn run lento")
}

fn wait_until_session(env: &TestEnv, thread: &str, timeout_s: u64) {
    let deadline = Instant::now() + Duration::from_secs(timeout_s);
    while Instant::now() < deadline {
        if !events_of(env, thread, "engine.session").is_empty() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("el run lento no llegó a establecer sesión");
}

fn lease_workspace(env: &TestEnv) -> String {
    let store = aeril::store::Store::open(&env.data.join("aeril.db")).unwrap();
    let ws: String = store
        .conn
        .query_row("SELECT workspace_id FROM leases LIMIT 1", [], |r| r.get(0))
        .unwrap();
    ws
}

fn lease_row_count(env: &TestEnv) -> i64 {
    let store = aeril::store::Store::open(&env.data.join("aeril.db")).unwrap();
    store
        .conn
        .query_row("SELECT COUNT(*) FROM leases", [], |r| r.get(0))
        .unwrap()
}

fn ready_count(env: &TestEnv, thread: &str) -> usize {
    env.checkpoint_list(thread)
        .iter()
        .filter(|c| c["state"] == "ready")
        .count()
}

/// Test A: release --force con dueño VIVO → rechazado; B bloqueado; A conserva
/// la exclusividad y completa.
#[test]
fn a_force_release_con_dueno_vivo_se_rechaza() {
    let env = TestEnv::new("rc21-a");
    let thread = env.thread_new("a");
    let mut a = spawn_slow_run(&env, &thread, 6000, "# escrito por A\n");
    wait_until_session(&env, &thread, 20);
    let ws = lease_workspace(&env);

    // Heartbeat NO vencido (default). El operador fuerza la liberación.
    let rel = env.aeril(&["lease", "release", "--workspace", &ws, "--force"]);
    assert!(
        !rel.status.success(),
        "un dueño VIVO no se libera ni con --force: {}",
        String::from_utf8_lossy(&rel.stdout)
    );
    let stderr = String::from_utf8_lossy(&rel.stderr);
    assert!(
        stderr.contains("VIVO") || stderr.contains("vivo"),
        "el error debe explicar que el dueño está vivo: {stderr}"
    );

    // B sigue bloqueado (el lease de A nunca se borró).
    let script_b = env.script("b", mock_edit("NOTES.md", "# B NO DEBE PASAR\n", "b"));
    let b = env.aeril_env(
        &[
            "run",
            "start",
            "--thread",
            &thread,
            "--prompt",
            "x",
            "--backend",
            &format!("mock:{}", script_b.display()),
        ],
        &[],
    );
    assert!(!b.status.success(), "B debe seguir bloqueado");
    assert!(
        String::from_utf8_lossy(&b.stderr).contains("sigue vivo")
            || String::from_utf8_lossy(&b.stderr).contains("en uso"),
        "stderr: {}",
        String::from_utf8_lossy(&b.stderr)
    );

    // A completa con SU contenido y libera el lease al terminar.
    let status = a.wait().expect("wait A");
    assert!(status.success(), "A completa normalmente");
    let wt = env.worktree_path(&thread);
    assert_eq!(
        std::fs::read_to_string(wt.join("NOTES.md")).unwrap(),
        "# escrito por A\n",
        "nadie modificó el workspace mientras A lo poseía"
    );
    assert_eq!(lease_row_count(&env), 0, "A libera su lease al terminar");
}

/// Test B: heartbeat vencido + dueño VIVO → release también se rechaza
/// (con el intervalo de renovación y TTL DEFAULT).
#[test]
fn b_force_release_con_heartbeat_vencido_y_dueno_vivo_se_rechaza() {
    let env = TestEnv::new("rc21-b");
    let thread = env.thread_new("b");
    let mut a = spawn_slow_run(&env, &thread, 8000, "# escrito por A (b)\n");
    wait_until_session(&env, &thread, 20);
    let ws = lease_workspace(&env);

    // Heartbeat "vencido" en la fila (equivalente a suspensión del dueño).
    {
        let store = aeril::store::Store::open(&env.data.join("aeril.db")).unwrap();
        store
            .conn
            .execute("UPDATE leases SET renewed_ms = 0", [])
            .unwrap();
    }
    let rel = env.aeril(&["lease", "release", "--workspace", &ws, "--force"]);
    assert!(
        !rel.status.success(),
        "aun con TTL vencido, un dueño vivo no se libera: {}",
        String::from_utf8_lossy(&rel.stdout)
    );

    // Ningún segundo run adquiere el workspace.
    let script_b = env.script("b2", mock_edit("NOTES.md", "# B NO DEBE PASAR (b)\n", "b"));
    let b = env.aeril_env(
        &[
            "run",
            "start",
            "--thread",
            &thread,
            "--prompt",
            "x",
            "--backend",
            &format!("mock:{}", script_b.display()),
        ],
        &[],
    );
    assert!(
        !b.status.success(),
        "B debe seguir bloqueado con TTL vencido"
    );
    let _ = a.kill();
    let _ = a.wait();
}

/// Test C: dueño muerto verificado → release sin force funciona y B completa.
#[test]
fn c_recuperacion_de_dueno_muerto_verificado() {
    let env = TestEnv::new("rc21-c");
    let thread = env.thread_new("c");
    let mut a = spawn_slow_run(&env, &thread, 30_000, "# de A (c)\n");
    wait_until_session(&env, &thread, 20);
    let ws = lease_workspace(&env);

    // Muerte real y verificada del dueño (SIGKILL + reapeado).
    let _ = a.kill();
    let st = a.wait().expect("wait A");
    assert!(!st.success(), "A murió por señal");
    std::thread::sleep(Duration::from_millis(300));

    let rel = env.aeril(&["lease", "release", "--workspace", &ws]);
    assert!(
        rel.status.success(),
        "dueño muerto ⇒ release sin --force: {}",
        String::from_utf8_lossy(&rel.stderr)
    );
    assert_eq!(lease_row_count(&env), 0);

    // B adquiere y completa sin nadie más escribiendo.
    let script_b = env.script(
        "b3",
        mock_edit("NOTES.md", "# B tras muerte verificada\n", "b"),
    );
    let run = env.run_start(&thread, "x", &script_b);
    assert_eq!(run["state"], "completed", "{run}");
    let wt = env.worktree_path(&thread);
    assert_eq!(
        std::fs::read_to_string(wt.join("NOTES.md")).unwrap(),
        "# B tras muerte verificada\n"
    );
}

/// Test D: fence transaccional — un dueño obsoleto (ownership robado por
/// fixture de DB, con intervalo de renovación default que no alcanza a
/// detectarlo en memoria) NO puede publicar checkpoint.ready ni reportar
/// completitud exitosa.
#[test]
fn d_dueno_obsoleto_no_publica_ready_ni_completa() {
    let env = TestEnv::new("rc21-d");
    let thread = env.thread_new("d");
    let a = spawn_slow_run(&env, &thread, 6000, "# de A (d)\n");
    wait_until_session(&env, &thread, 20);
    let ready_before = ready_count(&env, &thread);

    // Pérdida de ownership por fixture de DB (renovación default de 15 s:
    // el guard en memoria NO alcanza a verlo durante este run).
    let ws = lease_workspace(&env);
    {
        let store = aeril::store::Store::open(&env.data.join("aeril.db")).unwrap();
        store
            .conn
            .execute(
                &format!("UPDATE leases SET run_id = 'intruso' WHERE workspace_id = '{ws}'"),
                [],
            )
            .unwrap();
    }

    let out = a.wait_with_output().expect("wait A");
    let summary = parse_json(&out.stdout);
    assert_eq!(
        summary["state"], "failed",
        "dueño obsoleto: nunca completitud exitosa: {summary}"
    );
    assert!(
        summary["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("lease"),
        "razón explícita de la falla de fence: {}",
        summary["detail"]
    );

    // Inexistencia de ready nuevo y de completitud exitosa; resultado explícito.
    assert_eq!(
        ready_count(&env, &thread),
        ready_before,
        "no se publica checkpoint.ready con ownership perdido"
    );
    let ended = events_of(&env, &thread, "run.ended");
    assert_eq!(ended.last().unwrap()["payload"]["state"], "failed");
    let lost = events_of(&env, &thread, "workspace.lease_lost");
    assert!(
        !lost.is_empty(),
        "evento durable de pérdida de lease: {lost:?}"
    );
    let list = env.checkpoint_list(&thread);
    assert!(
        !list.iter().any(|c| c["state"] == "prepared"),
        "el prepared quedó rechazado por el fence (nada pendiente): {list:?}"
    );
}
