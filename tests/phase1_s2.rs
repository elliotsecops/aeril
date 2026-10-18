//! S2 — Visibilidad de runs (solo lectura).
//!
//! `aeril run list [--active]` debe reportar liveness real sin mutar nada:
//! un run vivo se ve vivo; un supervisor muerto con run sin resolver se ve
//! como `interrupted_candidate`; lease ausente ≠ supervisor muerto; un lease
//! de otro run no se presenta como propiedad válida; lecturas concurrentes no
//! cambian el estado de la DB.

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

fn run_list(env: &TestEnv, args: &[&str]) -> Vec<Value> {
    let mut full = vec!["run", "list"];
    full.extend_from_slice(args);
    let out = env.aeril(&full);
    assert!(
        out.status.success(),
        "run list falló: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    parse_json(&out.stdout)
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn spawn_run(env: &TestEnv, thread: &str, delay_ms: u64, content: &str) -> std::process::Child {
    let script = env.script(
        &format!("s2-{delay_ms}-{}", content.len()),
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

fn db(env: &TestEnv) -> aeril::store::Store {
    aeril::store::Store::open(&env.data.join("aeril.db")).unwrap()
}

fn insert_run_row(env: &TestEnv, thread: &str, state: &str, workspace_id: Option<&str>) -> String {
    let store = db(env);
    let run_id = aeril::ids::new_id();
    let now = aeril::timeutil::now_ms();
    store
        .conn
        .execute(
            "INSERT INTO runs (id, thread_id, workspace_id, repo_path, worktree_path, branch, base_oid,
                               backend, backend_version, state, started_ms)
             VALUES (?1, ?2, ?3, ?4, NULL, NULL, NULL, 'mock', 'test', ?5, ?6)",
            rusqlite::params![
                run_id,
                thread,
                workspace_id,
                env.repo.to_string_lossy(),
                state,
                now
            ],
        )
        .unwrap();
    run_id
}

fn insert_run_started_without_supervisor(env: &TestEnv, thread: &str, run_id: &str) {
    let mut store = db(env);
    aeril::store::events::append_typed(
        &mut store.conn,
        thread,
        Some(run_id),
        aeril::store::events::K_RUN_STARTED,
        "system",
        &aeril::store::events::payload::RunStarted {
            backend: "mock".into(),
            goal: "fixture".into(),
            workspace_id: "fixture-ws".into(),
            repo_path: env.repo.to_string_lossy().into(),
            worktree_path: String::new(),
            branch: String::new(),
            base_oid: String::new(),
            profile: None,
            resumed_session: None,
            supervisor_pid: None,
            supervisor_pid_start: None,
        },
    )
    .unwrap();
}

fn insert_lease_for(env: &TestEnv, ws: &str, run_id: &str, pid: i64, pid_start: u64) {
    let store = db(env);
    store
        .conn
        .execute(
            "INSERT OR REPLACE INTO leases (workspace_id, run_id, pid, pid_start, acquired_ms, renewed_ms, ttl_ms)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, 60000)",
            rusqlite::params![ws, run_id, pid, pid_start as i64, aeril::timeutil::now_ms()],
        )
        .unwrap();
}

// ── T1: run vivo visible desde otro proceso ─────────────────────────────────

#[test]
fn s2_run_activo_visible_desde_otro_proceso() {
    let env = TestEnv::new("s2-t1");
    let thread = env.thread_new("t1");
    let mut child = spawn_run(&env, &thread, 15_000, "# A (t1)\n");
    wait_until_session(&env, &thread, 25);

    // La consulta es un proceso distinto al supervisor.
    let active = run_list(&env, &["--active"]);
    assert_eq!(active.len(), 1, "{active:?}");
    let r = &active[0];
    assert_eq!(r["thread_id"], thread.as_str());
    assert_eq!(r["state"], "running");
    assert_eq!(r["status"], "running");
    assert_eq!(r["supervisor"]["status"], "alive");
    assert_eq!(r["lease"]["status"], "owned");
    assert_eq!(r["lease"]["owner_alive"], true);
    assert!(r["workspace_id"].as_str().is_some());

    let _ = child.kill();
    let _ = child.wait();
}

// ── T2: dos runs activos en threads separados, sin contaminación ────────────

#[test]
fn s2_dos_runs_activos_en_threads_separados() {
    let env = TestEnv::new("s2-t2");
    let ta = env.thread_new("t2a");
    let tb = env.thread_new("t2b");
    let mut a = spawn_run(&env, &ta, 15_000, "# A (t2)\n");
    let mut b = spawn_run(&env, &tb, 15_000, "# B (t2)\n");
    wait_until_session(&env, &ta, 25);
    wait_until_session(&env, &tb, 25);

    let active = run_list(&env, &["--active"]);
    assert_eq!(active.len(), 2, "{active:?}");
    let ra = active
        .iter()
        .find(|r| r["thread_id"] == ta.as_str())
        .unwrap();
    let rb = active
        .iter()
        .find(|r| r["thread_id"] == tb.as_str())
        .unwrap();
    assert_eq!(ra["status"], "running");
    assert_eq!(rb["status"], "running");
    assert_ne!(ra["workspace_id"], rb["workspace_id"]);
    assert_ne!(ra["run_id"], rb["run_id"]);
    // Cada thread ve exactamente su propio run activo.
    assert_eq!(run_list(&env, &["--active"]).len(), 2);

    let _ = a.kill();
    let _ = b.kill();
    let _ = a.wait();
    let _ = b.wait();
}

// ── T3: completados y fallidos; --limit y --active ──────────────────────────

#[test]
fn s2_completados_y_fallidos() {
    let env = TestEnv::new("s2-t3");
    let ok_thread = env.thread_new("t3ok");
    let s1 = env.script("ok", mock_edit("NOTES.md", "# ok (t3)\n", "x"));
    let ok = env.run_start(&ok_thread, "x", &s1);
    assert_eq!(ok["state"], "completed");

    let bad_thread = env.thread_new("t3bad");
    let bad = env.aeril_env(
        &[
            "run",
            "start",
            "--thread",
            &bad_thread,
            "--prompt",
            "x",
            "--backend",
            "mock:/ruta/inexistente.json",
        ],
        &[],
    );
    let bad_summary = parse_json(&bad.stdout);
    assert_eq!(bad_summary["state"], "failed");

    let all = run_list(&env, &[]);
    let ok_run = all
        .iter()
        .find(|r| r["thread_id"] == ok_thread.as_str())
        .unwrap();
    let bad_run = all
        .iter()
        .find(|r| r["thread_id"] == bad_thread.as_str())
        .unwrap();
    assert_eq!(ok_run["status"], "completed");
    assert_eq!(bad_run["status"], "failed");
    assert!(ok_run["ended_ms"].as_i64().is_some());

    // --active excluye terminales; --limit acota.
    let active = run_list(&env, &["--active"]);
    assert!(
        active.is_empty(),
        "sin activos tras completarse/fallar: {active:?}"
    );
    assert_eq!(run_list(&env, &["--limit", "1"]).len(), 1);
}

// ── T4: stale tras SIGKILL — reportado sin reconciliar ──────────────────────

#[test]
fn s2_stale_tras_sigkill_se_reporta_sin_mutaciones() {
    let env = TestEnv::new("s2-t4");
    let thread = env.thread_new("t4");
    let mut child = spawn_run(&env, &thread, 20_000, "# A (t4)\n");
    wait_until_session(&env, &thread, 25);
    let _ = child.kill();
    let _ = child.wait();
    std::thread::sleep(Duration::from_millis(300));

    // Lecturas repetidas: el run sigue sin resolver y NO se reconcilia.
    for _ in 0..3 {
        let active = run_list(&env, &["--active"]);
        assert_eq!(active.len(), 1);
        let r = &active[0];
        assert_eq!(r["status"], "interrupted_candidate", "{r:?}");
        assert_eq!(r["supervisor"]["status"], "dead");
        assert_eq!(r["state"], "running", "la columna persistida no se toca");
    }
    // Sin run.ended: la consulta no fabricó hechos.
    assert!(events_of(&env, &thread, "run.ended").is_empty());
    let persisted = db(&env)
        .conn
        .query_row(
            "SELECT state FROM runs WHERE thread_id = ?1 ORDER BY started_ms DESC LIMIT 1",
            [&thread],
            |r| r.get::<_, String>(0),
        )
        .unwrap();
    assert_eq!(persisted, "running");
}

// ── T5: supervisor desconocido (sin evidencia) y lease ausente ──────────────

#[test]
fn s2_supervisor_desconocido_y_lease_ausente() {
    let env = TestEnv::new("s2-t5");
    let thread = env.thread_new("t5");
    let run_id = insert_run_row(&env, &thread, "running", Some("ws-desconocido"));
    insert_run_started_without_supervisor(&env, &thread, &run_id);

    let active = run_list(&env, &["--active"]);
    let r = active
        .iter()
        .find(|r| r["run_id"] == run_id.as_str())
        .unwrap();
    assert_eq!(r["supervisor"]["status"], "unknown");
    assert_eq!(
        r["status"], "unknown",
        "sin evidencia no se declara vivo ni muerto"
    );
    assert_eq!(
        r["lease"]["status"], "missing",
        "lease ausente ≠ supervisor muerto"
    );
}

// ── T6: lease en manos de OTRO run (no es propiedad válida) ─────────────────

#[test]
fn s2_lease_de_otro_run_no_se_presenta_como_valido() {
    let env = TestEnv::new("s2-t6");
    let thread = env.thread_new("t6");
    let ws = "ws-compartido-t6";
    let run_a = insert_run_row(&env, &thread, "running", Some(ws));
    insert_run_started_without_supervisor(&env, &thread, &run_a);

    // Lease del MISMO workspace en manos de otro run con dueño VIVO (este test).
    let my_pid = std::process::id() as i64;
    let my_start = aeril::store::leases::process_start_time(my_pid).unwrap();
    let other_run = "run-ajeno-t6";
    insert_lease_for(&env, ws, other_run, my_pid, my_start);

    let active = run_list(&env, &["--active"]);
    let r = active
        .iter()
        .find(|r| r["run_id"] == run_a.as_str())
        .unwrap();
    assert_eq!(r["lease"]["status"], "held_by_other", "{r:?}");
    assert_eq!(r["lease"]["holder_run_id"], other_run);
    assert_eq!(r["lease"]["owner_alive"], true);
    assert_eq!(r["supervisor"]["status"], "unknown");
}

// ── T7: lecturas concurrentes no mutan la DB ────────────────────────────────

#[test]
fn s2_lecturas_concurrentes_no_mutan_la_db() {
    let env = TestEnv::new("s2-t7");
    let thread = env.thread_new("t7");
    let child = spawn_run(&env, &thread, 4_000, "# A (t7)\n");
    wait_until_session(&env, &thread, 25);

    // Lecturas durante la transición (run aún activo) desde hilos paralelos.
    std::thread::scope(|scope| {
        for _ in 0..3 {
            scope.spawn(|| {
                let _ = run_list(&env, &["--active"]);
                let _ = run_list(&env, &[]);
            });
        }
    });

    let out = child.wait_with_output().unwrap();
    let summary = parse_json(&out.stdout);
    assert_eq!(summary["state"], "completed");

    // Conteo total de eventos ANTES de las lecturas finales...
    let store = db(&env);
    let count_events = |s: &aeril::store::Store| -> i64 {
        s.conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE thread_id = ?1",
                [&thread],
                |r| r.get(0),
            )
            .unwrap()
    };
    let before = count_events(&store);
    let run_ended_before = events_of(&env, &thread, "run.ended").len();
    assert_eq!(run_ended_before, 1);

    // ...y DESPUÉS de más lecturas concurrentes: idéntico (read-only).
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                let _ = run_list(&env, &["--active"]);
                let _ = run_list(&env, &["--limit", "5"]);
            });
        }
    });
    let after = count_events(&db(&env));
    assert_eq!(before, after, "las consultas no agregan ni cambian eventos");
    let final_list = run_list(&env, &[]);
    let r = final_list
        .iter()
        .find(|r| r["thread_id"] == thread.as_str())
        .unwrap();
    assert_eq!(r["status"], "completed");
    assert_eq!(r["supervisor"]["status"], "dead", "el supervisor terminó");
}

// ── T8: filtro --active explícito ───────────────────────────────────────────

#[test]
fn s2_filtro_active_solo_no_resueltos() {
    let env = TestEnv::new("s2-t8");
    let done_thread = env.thread_new("t8done");
    let s1 = env.script("ok", mock_edit("NOTES.md", "# ok (t8)\n", "x"));
    let done = env.run_start(&done_thread, "x", &s1);
    assert_eq!(done["state"], "completed");

    let pending_thread = env.thread_new("t8pending");
    let pending_run = insert_run_row(&env, &pending_thread, "starting", Some("ws-t8"));

    let active = run_list(&env, &["--active"]);
    assert_eq!(active.len(), 1, "{active:?}");
    assert_eq!(active[0]["run_id"], pending_run.as_str());
    assert!(
        run_list(&env, &[]).len() >= 2,
        "sin filtro se ven terminales y no resueltos"
    );
}
