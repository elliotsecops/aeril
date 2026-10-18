//! S0-R1 — Seguridad de procesos en la reconciliación (D1/D2).
//!
//! Regresiones de los dos P1 de la auditoría independiente:
//! - D1: el líder del backend muere (reapeado o zombie) pero un miembro VIVO de
//!   su grupo sigue escribiendo. El workspace no puede declararse seguro.
//! - D2: evidencia de identidad incompleta (`pid_start=0`) o PID reutilizado:
//!   JAMÁS se envía una señal destructiva a un proceso no verificado.
//! - D4: un estado `workspace.recovery_unsafe` previo no se olvida en la
//!   decisión de reuso.
//!
//! Fixtures reales y acotados (procesos `sh` con grupo propio, cleanup
//! explícito); nada se ejecuta contra el repo real.

mod common;
use common::*;
use serde_json::Value;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

// ── helpers de entorno/store ────────────────────────────────────────────────

fn db(env: &TestEnv) -> aeril::store::Store {
    aeril::store::Store::open(&env.data.join("aeril.db")).unwrap()
}

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

fn dead_pid() -> i64 {
    let mut victim = Command::new("sleep").arg("30").spawn().unwrap();
    let pid = victim.id() as i64;
    victim.kill().unwrap();
    victim.wait().unwrap();
    pid
}

/// Fixture de un run stale (misma forma que `tests/phase1_s0.rs`).
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
    let run_id = aeril::ids::new_id();
    let now = aeril::timeutil::now_ms();
    {
        let store = db(env);
        store
            .conn
            .execute(
                "INSERT INTO runs (id, thread_id, workspace_id, repo_path, worktree_path, branch, base_oid,
                                   backend, backend_version, state, started_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'mock', 'safety-test', ?8, ?9)",
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
    }
    let mut store = db(env);
    let supervisor_payload = match supervisor {
        Some((pid, start)) => aeril::store::events::payload::RunStarted {
            backend: "mock".into(),
            goal: "fixture safety".into(),
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
        None => aeril::store::events::payload::RunStarted {
            backend: "mock".into(),
            goal: "fixture safety".into(),
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
    };
    aeril::store::events::append_typed(
        &mut store.conn,
        thread,
        Some(&run_id),
        aeril::store::events::K_RUN_STARTED,
        "system",
        &supervisor_payload,
    )
    .unwrap();
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
                program: "audit".into(),
            },
        )
        .unwrap();
    }
    run_id
}

fn find_entry<'a>(rec: &'a Value, run_id: &str) -> &'a Value {
    rec.as_array()
        .unwrap()
        .iter()
        .find(|e| e["run_id"] == run_id)
        .unwrap_or_else(|| panic!("sin entrada para {run_id}: {rec}"))
}

fn setup(env: &TestEnv, title: &str) -> (String, String, String) {
    let thread = env.thread_new(title);
    let s = env.script("boot", mock_edit("NOTES.md", "# boot\n", "boot"));
    let run = env.run_start(&thread, "boot", &s);
    assert_eq!(run["state"], "completed", "{run}");
    let store = db(env);
    let (ws, base): (String, String) = store
        .conn
        .query_row(
            "SELECT workspace_id, base_oid FROM runs WHERE thread_id = ?1
             ORDER BY started_ms DESC, id DESC LIMIT 1",
            [thread.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    (thread, ws, base)
}

// ── helpers de procesos reales ──────────────────────────────────────────────

/// Líder con grupo propio + hijo en el MISMO grupo que escribe en `outfile`.
/// El líder termina tras `leader_secs` (el llamador decide reapearlo o dejarlo
/// zombie).
fn spawn_group(outfile: &Path, leader_secs: u64) -> Child {
    let script = format!(
        "while :; do echo child >> '{}'; sleep 0.1; done & sleep {}",
        outfile.display(),
        leader_secs
    );
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.process_group(0);
    cmd.spawn().expect("spawn grupo")
}

fn proc_state(pid: i64) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .next()?
        .chars()
        .next()
}

/// Miembros VIVOS (no zombie) del grupo `pgid` — scan local independiente del
/// helper de producción (mejor valor de regresión).
fn live_members(pgid: i64) -> Vec<i64> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in rd.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<i64>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else {
            continue;
        };
        let Some(after) = stat.rsplit_once(')').map(|(_, a)| a) else {
            continue;
        };
        let fields: Vec<&str> = after.split_whitespace().collect();
        let state = fields.first().and_then(|s| s.chars().next()).unwrap_or('?');
        let pgrp = fields
            .get(2)
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(-1);
        if pgrp == pgid && !matches!(state, 'Z' | 'X') {
            out.push(pid);
        }
    }
    out
}

fn wait_until<F: FnMut() -> bool>(mut f: F, secs: u64) -> bool {
    let dl = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < dl {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    f()
}

fn kill_group(pgid: i64) {
    unsafe {
        libc::kill(-(pgid as i32), libc::SIGKILL);
    }
}

// ── T1: líder reapeado + hijo vivo en el mismo grupo ────────────────────────

#[test]
fn t1_leader_reapeado_con_hijo_vivo_no_permite_reuso() {
    let env = TestEnv::new("s0r1-t1");
    let (thread, ws, base) = setup(&env, "t1");
    let wt = env.data.join("worktrees").join(&ws);
    let orphan = wt.join("ORPHAN.txt");

    let mut leader = spawn_group(&orphan, 1);
    let leader_pid = leader.id() as i64;
    let start = aeril::store::leases::process_start_time(leader_pid).expect("starttime del líder");
    assert!(
        wait_until(
            || live_members(leader_pid).iter().any(|p| *p != leader_pid),
            5
        ),
        "el hijo del grupo no arrancó"
    );
    std::thread::sleep(Duration::from_millis(1400));
    let _ = leader.wait(); // reapeo: líder Absent, hijo vivo
    assert!(proc_state(leader_pid).is_none(), "líder reapeado");
    assert!(
        !live_members(leader_pid).is_empty(),
        "el hijo debe seguir vivo"
    );

    let dead = dead_pid();
    let rid = insert_stale_run(
        &env,
        &thread,
        "running",
        Some(&ws),
        Some(&base),
        Some((dead, 0)),
        Some((leader_pid, start)),
    );
    let rec = env.aeril_ok(&["recover"]);
    let entry = find_entry(&rec, &rid);
    assert_eq!(entry["action"], "run_interrupted", "{rec}");

    let members_after = live_members(leader_pid);
    if !members_after.is_empty() {
        assert_eq!(
            entry["unsafe_state"], true,
            "sin terminación verificada el run debe quedar inseguro: {entry}"
        );
        assert_eq!(
            events_of(&env, &thread, "workspace.recovery_unsafe").len(),
            1
        );
        let s2 = env.script("s2", mock_edit("NOTES.md", "# segundo (t1)\n", "dos"));
        let run2 = env.run_start(&thread, "dos", &s2);
        assert_eq!(run2["state"], "completed", "{run2}");
        assert_ne!(
            latest_workspace(&env, &thread).unwrap(),
            ws,
            "jamás se reutiliza el workspace mientras el hijo del grupo escribe"
        );
        assert_eq!(events_of(&env, &thread, "workspace.reuse_blocked").len(), 1);
        assert!(orphan.exists(), "el workspace viejo queda intacto");
    } else {
        assert_eq!(entry["unsafe_state"], false, "{entry}");
        let s2 = env.script("s2", mock_edit("NOTES.md", "# segundo (t1)\n", "dos"));
        let run2 = env.run_start(&thread, "dos", &s2);
        assert_eq!(run2["state"], "completed", "{run2}");
        assert_eq!(latest_workspace(&env, &thread).unwrap(), ws);
    }
    kill_group(leader_pid);
}

// ── T2: líder zombie + hijo vivo en el mismo grupo ──────────────────────────

#[test]
fn t2_leader_zombie_con_hijo_vivo_se_termina_o_bloquea() {
    let env = TestEnv::new("s0r1-t2");
    let (thread, ws, base) = setup(&env, "t2");
    let wt = env.data.join("worktrees").join(&ws);
    let orphan = wt.join("ORPHAN.txt");

    let mut leader = spawn_group(&orphan, 1);
    let leader_pid = leader.id() as i64;
    let start = aeril::store::leases::process_start_time(leader_pid).expect("starttime del líder");
    assert!(
        wait_until(|| proc_state(leader_pid) == Some('Z'), 6),
        "el líder debe quedar zombie"
    );
    assert!(
        !live_members(leader_pid).is_empty(),
        "el hijo debe seguir vivo"
    );

    let dead = dead_pid();
    let rid = insert_stale_run(
        &env,
        &thread,
        "running",
        Some(&ws),
        Some(&base),
        Some((dead, 0)),
        Some((leader_pid, start)),
    );
    let rec = env.aeril_ok(&["recover"]);
    let entry = find_entry(&rec, &rid);
    assert_eq!(entry["action"], "run_interrupted", "{rec}");

    let members_after = live_members(leader_pid);
    let s2 = env.script("s2", mock_edit("NOTES.md", "# segundo (t2)\n", "dos"));
    let run2 = env.run_start(&thread, "dos", &s2);
    assert_eq!(run2["state"], "completed", "{run2}");
    if members_after.is_empty() {
        assert_eq!(entry["unsafe_state"], false, "{entry}");
        assert_eq!(
            latest_workspace(&env, &thread).unwrap(),
            ws,
            "tras terminación verificada el reuso es seguro"
        );
        // El grupo terminó: nadie escribe más en el workspace.
        let size1 = std::fs::metadata(&orphan).map(|m| m.len()).unwrap_or(0);
        std::thread::sleep(Duration::from_millis(600));
        let size2 = std::fs::metadata(&orphan).map(|m| m.len()).unwrap_or(0);
        assert_eq!(size1, size2, "sin escritores tras la terminación del grupo");
    } else {
        assert_eq!(entry["unsafe_state"], true, "{entry}");
        assert_ne!(
            latest_workspace(&env, &thread).unwrap(),
            ws,
            "miembros vivos ⇒ no se reutiliza"
        );
    }
    let _ = leader.wait();
    kill_group(leader_pid);
}

// ── T3: pid_start=0 + proceso ajeno vivo ────────────────────────────────────

#[test]
fn t3_pid_start_cero_no_mata_proceso_ajeno_y_bloquea_reuso() {
    let env = TestEnv::new("s0r1-t3");
    let (thread, ws, base) = setup(&env, "t3");
    let wt = env.data.join("worktrees").join(&ws);
    let old_content = std::fs::read_to_string(wt.join("NOTES.md")).unwrap();

    let mut victim = Command::new("sleep").arg("60").spawn().unwrap();
    let victim_pid = victim.id() as i64;
    let dead = dead_pid();
    let rid = insert_stale_run(
        &env,
        &thread,
        "running",
        Some(&ws),
        Some(&base),
        Some((dead, 0)),
        Some((victim_pid, 0)),
    );
    let rec = env.aeril_ok(&["recover"]);
    let entry = find_entry(&rec, &rid);
    assert_eq!(entry["action"], "run_interrupted", "{rec}");
    assert_eq!(
        entry["unsafe_state"], true,
        "identidad no establecida ⇒ unsafe: {entry}"
    );
    assert!(
        victim.try_wait().unwrap().is_none(),
        "el proceso ajeno JAMÁS recibe señal con identidad desconocida"
    );
    assert_eq!(
        events_of(&env, &thread, "workspace.recovery_unsafe").len(),
        1
    );

    let s2 = env.script("s2", mock_edit("NOTES.md", "# segundo (t3)\n", "dos"));
    let run2 = env.run_start(&thread, "dos", &s2);
    assert_eq!(run2["state"], "completed", "{run2}");
    assert_ne!(
        latest_workspace(&env, &thread).unwrap(),
        ws,
        "workspace inseguro no se reutiliza"
    );
    assert_eq!(events_of(&env, &thread, "workspace.reuse_blocked").len(), 1);
    assert_eq!(
        std::fs::read_to_string(wt.join("NOTES.md")).unwrap(),
        old_content,
        "el workspace viejo queda intacto"
    );
    let _ = victim.kill();
    let _ = victim.wait();
}

// ── T4: PID reutilizado (starttime distinto, no cero) ───────────────────────

#[test]
fn t4_pid_reutilizado_no_recibe_senal() {
    let env = TestEnv::new("s0r1-t4");
    let (thread, ws, base) = setup(&env, "t4");

    let mut victim = Command::new("sleep").arg("60").spawn().unwrap();
    let victim_pid = victim.id() as i64;
    let actual = aeril::store::leases::process_start_time(victim_pid).unwrap();
    let wrong_start = actual + 10_000; // starttime distinto y no cero

    let dead = dead_pid();
    let rid = insert_stale_run(
        &env,
        &thread,
        "running",
        Some(&ws),
        Some(&base),
        Some((dead, 0)),
        Some((victim_pid, wrong_start)),
    );
    let rec = env.aeril_ok(&["recover"]);
    let entry = find_entry(&rec, &rid);
    assert_eq!(entry["action"], "run_interrupted", "{rec}");
    assert!(
        entry["backend"]
            .as_str()
            .unwrap_or_default()
            .contains("reutilizado"),
        "se distingue reutilización de identidad desconocida: {entry}"
    );
    assert!(
        victim.try_wait().unwrap().is_none(),
        "un PID reutilizado no recibe señal destructiva"
    );
    let _ = victim.kill();
    let _ = victim.wait();
}

// ── T5: grupo verificado se termina y habilita el reuso ─────────────────────

#[test]
fn t5_grupo_verificado_se_termina_y_permite_reuso() {
    let env = TestEnv::new("s0r1-t5");
    let (thread, ws, base) = setup(&env, "t5");
    let wt = env.data.join("worktrees").join(&ws);
    let orphan = wt.join("ORPHAN.txt");

    let mut leader = spawn_group(&orphan, 30);
    let leader_pid = leader.id() as i64;
    let start = aeril::store::leases::process_start_time(leader_pid).expect("starttime del líder");
    assert!(
        wait_until(
            || live_members(leader_pid).iter().any(|p| *p != leader_pid),
            5
        ),
        "el hijo del grupo no arrancó"
    );

    let dead = dead_pid();
    let rid = insert_stale_run(
        &env,
        &thread,
        "running",
        Some(&ws),
        Some(&base),
        Some((dead, 0)),
        Some((leader_pid, start)),
    );
    let rec = env.aeril_ok(&["recover"]);
    let entry = find_entry(&rec, &rid);
    assert_eq!(entry["action"], "run_interrupted", "{rec}");
    assert_eq!(entry["unsafe_state"], false, "{entry}");
    assert!(
        entry["backend"]
            .as_str()
            .unwrap_or_default()
            .contains("terminado"),
        "{entry}"
    );
    assert!(
        live_members(leader_pid).is_empty(),
        "el grupo verificado debe terminar por completo: {:?}",
        live_members(leader_pid)
    );

    let s2 = env.script("s2", mock_edit("NOTES.md", "# segundo (t5)\n", "dos"));
    let run2 = env.run_start(&thread, "dos", &s2);
    assert_eq!(run2["state"], "completed", "{run2}");
    assert_eq!(
        latest_workspace(&env, &thread).unwrap(),
        ws,
        "terminación verificada habilita el reuso"
    );
    let size1 = std::fs::metadata(&orphan).map(|m| m.len()).unwrap_or(0);
    std::thread::sleep(Duration::from_millis(600));
    let size2 = std::fs::metadata(&orphan).map(|m| m.len()).unwrap_or(0);
    assert_eq!(size1, size2, "ningún miembro del grupo sigue escribiendo");

    let _ = leader.wait();
    kill_group(leader_pid);
}

// ── T6: estado inseguro persistente (D4) ────────────────────────────────────

#[test]
fn t6_estado_inseguro_no_se_olvida_ni_con_el_pid_muerto() {
    let env = TestEnv::new("s0r1-t6");
    let (thread, ws, base) = setup(&env, "t6");
    let wt = env.data.join("worktrees").join(&ws);
    let old_content = std::fs::read_to_string(wt.join("NOTES.md")).unwrap();

    // 1) Estado inseguro: pid_start=0 con un proceso ajeno vivo.
    let mut victim = Command::new("sleep").arg("60").spawn().unwrap();
    let victim_pid = victim.id() as i64;
    let dead = dead_pid();
    let rid = insert_stale_run(
        &env,
        &thread,
        "running",
        Some(&ws),
        Some(&base),
        Some((dead, 0)),
        Some((victim_pid, 0)),
    );
    let rec = env.aeril_ok(&["recover"]);
    assert_eq!(find_entry(&rec, &rid)["unsafe_state"], true, "{rec}");

    // 2) Reconciliaciones repetidas: sin hechos nuevos ni duplicados.
    let rec2 = env.aeril_ok(&["recover"]);
    assert!(
        !rec2
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["run_id"] == rid.as_str()),
        "run ya resuelto: {rec2}"
    );
    assert_eq!(
        events_of(&env, &thread, "workspace.recovery_unsafe").len(),
        1,
        "un único evento unsafe"
    );

    // 3) El proceso ajeno muere: el PID queda muerto y sin miembros.
    let _ = victim.kill();
    let _ = victim.wait();
    std::thread::sleep(Duration::from_millis(200));

    // Aun así, la incertidumbre registrada NO se olvida.
    let rec3 = env.aeril_ok(&["recover"]);
    assert!(
        !rec3
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["run_id"] == rid.as_str())
    );

    let s2 = env.script("s2", mock_edit("NOTES.md", "# segundo (t6)\n", "dos"));
    let run2 = env.run_start(&thread, "dos", &s2);
    assert_eq!(run2["state"], "completed", "{run2}");
    assert_ne!(
        latest_workspace(&env, &thread).unwrap(),
        ws,
        "estado inseguro previo: jamás se reutiliza en silencio"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("NOTES.md")).unwrap(),
        old_content,
        "el workspace inseguro queda intacto"
    );
}
