//! RC2-B5: exclusividad del lease de workspace con dueño vivo.
//!
//! Invariante: Aeril NUNCA autoriza a dos runs supervisados vivos a usar el
//! mismo workspace. Un TTL vencido con el proceso dueño vivo NO transfiere
//! ownership; la pérdida de lease aborta al dueño sin reportar completitud.

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

fn spawn_slow_run_with_env(
    env: &TestEnv,
    thread: &str,
    delay_ms: u64,
    extra_env: &[(&str, &str)],
) -> std::process::Child {
    let script = env.script(
        "slow-rc2",
        serde_json::json!({
            "message": "trabajando",
            "final_message": "listo",
            "delay_ms": delay_ms,
            "edits": [{"op": "write", "path": "NOTES.md", "content": "# escrito por A\n"}],
        }),
    );
    let mut cmd = Command::new(BIN);
    cmd.args([
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
    .stderr(std::process::Stdio::piped());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.spawn().expect("spawn run lento")
}

fn wait_until_session(env: &TestEnv, thread: &str, timeout_s: u64) {
    let deadline = Instant::now() + Duration::from_secs(timeout_s);
    while Instant::now() < deadline {
        if !events_of(env, thread, "engine.session").is_empty() {
            return;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    panic!("el run lento no llegó a establecer sesión");
}

/// Envejece el heartbeat del lease (simula suspensión/vencimiento del TTL).
fn age_lease_row(env: &TestEnv) {
    let store = aeril::store::Store::open(&env.data.join("aeril.db")).unwrap();
    let changed = store
        .conn
        .execute("UPDATE leases SET renewed_ms = 0", [])
        .unwrap();
    assert!(changed >= 1, "había un lease que envejecer");
}

fn workspace_with_lease(env: &TestEnv) -> String {
    let store = aeril::store::Store::open(&env.data.join("aeril.db")).unwrap();
    let ws: String = store
        .conn
        .query_row("SELECT workspace_id FROM leases LIMIT 1", [], |r| r.get(0))
        .unwrap();
    ws
}

#[test]
fn b5_ttl_vencido_con_dueno_vivo_no_transfiere_ownership() {
    let env = TestEnv::new("rc2-b5-live");
    let thread = env.thread_new("b5v");
    let mut a = spawn_slow_run_with_env(&env, &thread, 20_000, &[]);
    wait_until_session(&env, &thread, 25);

    // Heartbeat vencido MIENTRAS el proceso dueño sigue vivo.
    age_lease_row(&env);

    // B intenta el mismo workspace: debe quedar BLOQUEADO (fail-closed).
    let script_b = env.script(
        "b",
        mock_edit("NOTES.md", "# escrito por B (NO DEBE PASAR)\n", "b"),
    );
    let out = env.aeril_env(
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
        !out.status.success(),
        "B no debe adquirir un workspace con dueño vivo: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("sigue vivo") || stderr.contains("en uso"),
        "error explícito de exclusividad: {stderr}"
    );

    // El contenido de A (cuando termine) no fue pisado por B.
    let status = a.wait().expect("wait A");
    assert!(status.success(), "A termina normalmente");
    let wt = env
        .worktree_path_opt(&thread)
        .expect("workspace de A existe");
    assert_eq!(
        std::fs::read_to_string(wt.join("NOTES.md")).unwrap(),
        "# escrito por A\n",
        "nadie pisó el trabajo de A"
    );
}

#[test]
fn b5_suspension_con_ttl_corto_sigue_bloqueando_y_luego_libera() {
    let env = TestEnv::new("rc2-b5-stop");
    let thread = env.thread_new("b5s");
    let ttl_env = [
        ("AERIL_LEASE_TTL_MS", "1500"),
        ("AERIL_LEASE_RENEW_SECS", "1"),
    ];
    let mut a = spawn_slow_run_with_env(&env, &thread, 12_000, &ttl_env);
    wait_until_session(&env, &thread, 25);

    // Suspensión real del proceso dueño: no renueva, pero sigue VIVO.
    unsafe {
        libc::kill(a.id() as i32, libc::SIGSTOP);
    }
    std::thread::sleep(Duration::from_millis(3000)); // > TTL

    let script_b = env.script("b", mock_edit("NOTES.md", "# B no debe pasar\n", "b"));
    let out = env.aeril_env(
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
        &ttl_env,
    );
    assert!(
        !out.status.success(),
        "proceso suspendido = dueño vivo: B bloqueado"
    );

    // Reanudar: A termina y RECIÉN ahí B puede correr.
    unsafe {
        libc::kill(a.id() as i32, libc::SIGCONT);
    }
    let status = a.wait().expect("wait A");
    assert!(status.success(), "A retoma y completa");

    let script_c = env.script("c", mock_edit("NOTES.md", "# ahora sí B\n", "c"));
    let run = env.aeril_env(
        &[
            "run",
            "start",
            "--thread",
            &thread,
            "--prompt",
            "x",
            "--backend",
            &format!("mock:{}", script_c.display()),
        ],
        &ttl_env,
    );
    let summary = parse_json(&run.stdout);
    assert_eq!(summary["state"], "completed", "B corre tras A: {summary}");
}

#[test]
fn b5_perdida_de_lease_aborta_al_dueno_sin_reportar_completitud() {
    let env = TestEnv::new("rc2-b5-lost");
    let thread = env.thread_new("b5l");
    let ttl_env = [("AERIL_LEASE_RENEW_SECS", "1")];
    let mut a = spawn_slow_run_with_env(&env, &thread, 30_000, &ttl_env);
    wait_until_session(&env, &thread, 25);
    let ws = workspace_with_lease(&env);

    // Ownership robado por fuera (proceso "intruso"): el próximo renew de A
    // falla (0 filas) y A debe ABORTAR su run, no seguir a ciegas.
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

    // A debe morir solo en pocos segundos (renew cada 1 s).
    let deadline = Instant::now() + Duration::from_secs(25);
    let mut exited = false;
    while Instant::now() < deadline {
        if a.try_wait().unwrap().is_some() {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(exited, "A debe abortar al perder el lease");
    let out = a.wait_with_output().expect("output A");
    let summary = parse_json(&out.stdout);
    assert_eq!(
        summary["state"], "failed",
        "nunca completitud normal: {summary}"
    );
    assert!(
        summary["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("lease"),
        "razón durable del aborto: {}",
        summary["detail"]
    );

    // Eventos durables: lease perdido + run.ended failed, SIN checkpoint.
    let lost = events_of(&env, &thread, "workspace.lease_lost");
    assert_eq!(lost.len(), 1, "evento durable de pérdida: {lost:?}");
    let ended = events_of(&env, &thread, "run.ended");
    assert_eq!(ended.last().unwrap()["payload"]["state"], "failed");
    assert!(
        events_of(&env, &thread, "checkpoint.ready").is_empty(),
        "no se captura checkpoint tras perder el lease"
    );

    // El backend de A fue terminado (sin huérfanos de su script).
    std::thread::sleep(Duration::from_millis(300));
    let store = aeril::store::Store::open(&env.data.join("aeril.db")).unwrap();
    let closed: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM runs WHERE thread_id = ?1 AND state = 'running'",
            [&thread],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(closed, 0, "no quedan runs 'running' colgados");
}

#[test]
fn b5_pid_reusado_no_habilita_toma_de_control() {
    let env = TestEnv::new("rc2-b5-reuse");
    let thread = env.thread_new("b5r");
    // Un run normal para tener workspace.
    let script = env.script("s", mock_edit("NOTES.md", "# v2\n", "x"));
    let run = env.run_start(&thread, "x", &script);
    assert_eq!(run["state"], "completed");
    let ws: String = {
        let store = aeril::store::Store::open(&env.data.join("aeril.db")).unwrap();
        store
            .conn
            .query_row(
                "SELECT workspace_id FROM runs WHERE workspace_id IS NOT NULL ORDER BY started_ms DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };
    // Lease "fantasma": PID vivo (el del test) pero con starttime DISTINTO al
    // real (simula reutilización de PID). Debe considerarse dueño NO vivo.
    let my_pid = std::process::id() as i64;
    {
        let store = aeril::store::Store::open(&env.data.join("aeril.db")).unwrap();
        store
            .conn
            .execute(
                &format!(
                    "INSERT INTO leases (workspace_id, run_id, pid, pid_start, acquired_ms, renewed_ms, ttl_ms)
                     VALUES ('{ws}', 'run-fantasma', {my_pid}, 1, 0, 0, 60000)"
                ),
                [],
            )
            .unwrap();
    }
    let script2 = env.script("s2", mock_edit("NOTES.md", "# v3\n", "x"));
    let run2 = env.run_start(&thread, "x", &script2);
    assert_eq!(
        run2["state"], "completed",
        "pid reusado ⇒ dueño muerto: {run2}"
    );
    let takeover = events_of(&env, &thread, "workspace.lease_taken_over");
    assert_eq!(
        takeover.last().unwrap()["payload"]["reason"],
        "pid reusado (starttime distinto)"
    );
}

#[test]
fn b5_rm_bloqueado_con_ttl_vencido_y_dueno_vivo() {
    let env = TestEnv::new("rc2-b5-rm");
    let thread = env.thread_new("b5rm");
    let mut a = spawn_slow_run_with_env(&env, &thread, 20_000, &[]);
    wait_until_session(&env, &thread, 25);
    age_lease_row(&env);

    let rm = env.aeril(&["worktree", "rm", "--thread", &thread]);
    assert!(
        !rm.status.success(),
        "borrar con dueño vivo (aun con TTL vencido) debe fallar"
    );
    let stderr = String::from_utf8_lossy(&rm.stderr);
    assert!(
        stderr.contains("dueño VIVO") && stderr.contains("NO se libera, ni con `--force`"),
        "error explícito (RC2.1): {stderr}"
    );
    let _ = a.kill();
    let _ = a.wait();
}

#[test]
fn b5_dueno_muerto_recupera_sin_autorizar_dos_escritores() {
    let env = TestEnv::new("rc2-b5-dead");
    let thread = env.thread_new("b5d");
    let mut a = spawn_slow_run_with_env(&env, &thread, 30_000, &[]);
    wait_until_session(&env, &thread, 25);
    // Muerte real del dueño (sin liberar lease).
    let _ = a.kill();
    let _ = a.wait();
    std::thread::sleep(Duration::from_millis(300));

    let script_b = env.script("b", mock_edit("NOTES.md", "# reemplazo limpio\n", "b"));
    let run = env.run_start(&thread, "x", &script_b);
    assert_eq!(
        run["state"], "completed",
        "dueño muerto ⇒ recuperación: {run}"
    );
    let takeover = events_of(&env, &thread, "workspace.lease_taken_over");
    assert_eq!(takeover.last().unwrap()["payload"]["reason"], "pid muerto");
}
