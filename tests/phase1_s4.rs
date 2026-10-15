//! S4 — Ciclo de vida de workspaces (inspección read-only + borrado seguro).
//!
//! `aeril workspace list` debe describir ownership, validez y estado de
//! recuperación sin mutar nada; `aeril worktree rm` conserva sus garantías
//! (lease vivo no se desplaza ni con `--force`, RC2.1) y exige acción
//! explícita (`--unsafe-ok`) cuando el veredicto S0 bloquea el borrado.
//! La rehidratación desde checkpoint (feedback) se verifica de punta a punta.

mod common;
use common::*;
use serde_json::Value;
use std::path::Path;
use std::process::{Child, Command, Stdio};
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
    branch: Option<String>,
    base_oid: Option<String>,
}

fn runs_of(env: &TestEnv, thread_id: &str) -> Vec<RunRow> {
    let store = db(env);
    let mut stmt = store
        .conn
        .prepare(
            "SELECT id, state, workspace_id, branch, base_oid
             FROM runs WHERE thread_id = ?1 ORDER BY started_ms ASC, id ASC",
        )
        .unwrap();
    let rows = stmt
        .query_map([thread_id], |r| {
            Ok(RunRow {
                id: r.get(0)?,
                state: r.get(1)?,
                ws: r.get(2)?,
                branch: r.get(3)?,
                base_oid: r.get(4)?,
            })
        })
        .unwrap();
    rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
}

fn ws_list(env: &TestEnv, args: &[&str]) -> Vec<Value> {
    let mut full = vec!["workspace", "list"];
    full.extend_from_slice(args);
    env.aeril_ok(&full).as_array().cloned().unwrap_or_default()
}

fn entry<'a>(list: &'a [Value], ws: &str) -> &'a Value {
    list.iter()
        .find(|w| w["workspace_id"] == ws)
        .unwrap_or_else(|| panic!("workspace {ws} no está en la lista: {list:?}"))
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

fn count_sql(env: &TestEnv, sql: &str) -> i64 {
    db(env).conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn spawn_run_bg(env: &TestEnv, thread_id: &str, script: &Path) -> Child {
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
        .expect("spawn run")
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
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn sigkill(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

fn kill_mock_of(env: &TestEnv, thread_id: &str) {
    for p in event_payloads(env, thread_id, "backend.spawned") {
        if let Some(pid) = p["pid"].as_i64() {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    }
    // Dar tiempo a que init recoja al huérfano antes de clasificar identidad.
    std::thread::sleep(Duration::from_millis(400));
}

fn run_recover(env: &TestEnv) {
    let out = env.aeril(&["recover"]);
    assert!(out.status.success(), "recover falló");
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?} falló");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// ── 1. listing de un workspace verificado (y dedupe por workspace) ──────────

#[test]
fn s4_lista_workspace_verificado() {
    let env = TestEnv::new("s4-t1");
    let thread = env.thread_new("t1");
    let base = env.head_oid();
    let script = env.script("t1a", mock_edit("NOTES.md", "# t1-a\n", "x"));
    let r1 = env.run_start(&thread, "x", &script);
    assert_eq!(r1["state"], "completed");
    let run1 = runs_of(&env, &thread).remove(0);
    let ws1 = run1.ws.clone().expect("workspace del run");

    let list = ws_list(&env, &[]);
    assert_eq!(list.len(), 1, "un workspace: {list:?}");
    let w = entry(&list, &ws1);
    assert_eq!(w["thread_id"], thread.as_str());
    assert_eq!(w["latest_run_id"], run1.id.as_str());
    assert_eq!(w["latest_run_state"], "completed");
    assert_eq!(
        w["repo_path"].as_str().unwrap().trim_end_matches('/'),
        env.repo.to_string_lossy().as_ref(),
        "repo_path del workspace"
    );
    assert_eq!(w["base_oid"], base.as_str());
    assert_eq!(w["branch"], run1.branch.as_deref().unwrap());
    assert_eq!(w["worktree"]["status"], "ok");
    assert_eq!(w["lease"], Value::Null);
    assert_eq!(w["recovery_unsafe"], false);
    assert_eq!(w["reuse"]["eligible"], true);
    assert_eq!(w["status"], "verified_available");
    let wt = env.data.join("worktrees").join(&ws1);
    assert_eq!(w["worktree_path"], wt.to_string_lossy().as_ref());
    assert_eq!(w["expected_worktree_path"], wt.to_string_lossy().as_ref());
    assert!(wt.is_dir(), "el worktree existe");

    // Filtros acotados.
    assert_eq!(ws_list(&env, &["--thread", &thread]).len(), 1);
    assert_eq!(ws_list(&env, &["--thread", "no-existe"]).len(), 0);
    assert_eq!(ws_list(&env, &["--limit", "1"]).len(), 1);
    assert_eq!(
        ws_list(&env, &["--limit", "0"]).len(),
        1,
        "limit se acota a ≥1"
    );

    // Dedupe: un segundo run reutiliza el MISMO workspace (base igual, verdict
    // Safe) y la lista sigue mostrando UNA entrada con el run más reciente.
    let script2 = env.script("t1b", mock_edit("NOTES.md", "# t1-b\n", "x"));
    let r2 = env.run_start(&thread, "x", &script2);
    assert_eq!(r2["state"], "completed");
    let runs = runs_of(&env, &thread);
    assert_eq!(runs.len(), 2);
    assert_eq!(
        runs[0].ws.as_deref(),
        runs[1].ws.as_deref(),
        "el segundo run reutiliza el workspace"
    );
    let list = ws_list(&env, &[]);
    assert_eq!(list.len(), 1, "el workspace aparece una sola vez: {list:?}");
    assert_eq!(list[0]["latest_run_id"], runs[1].id.as_str());
}

// ── 2. dos workspaces independientes en el mismo repo ───────────────────────

#[test]
fn s4_dos_workspaces_independientes() {
    let env = TestEnv::new("s4-t2");
    let ta = env.thread_new("t2-a");
    let tb = env.thread_new("t2-b");
    let sa = env.script("t2a", mock_edit("NOTES.md", "# t2-a\n", "x"));
    let sb = env.script("t2b", mock_edit("NOTES.md", "# t2-b\n", "x"));
    assert_eq!(env.run_start(&ta, "x", &sa)["state"], "completed");
    assert_eq!(env.run_start(&tb, "x", &sb)["state"], "completed");
    let ws_a = runs_of(&env, &ta)[0].ws.clone().unwrap();
    let ws_b = runs_of(&env, &tb)[0].ws.clone().unwrap();
    assert_ne!(ws_a, ws_b);

    let list = ws_list(&env, &[]);
    assert_eq!(list.len(), 2);
    let a = entry(&list, &ws_a);
    let b = entry(&list, &ws_b);
    assert_eq!(a["status"], "verified_available");
    assert_eq!(b["status"], "verified_available");
    assert_eq!(a["repo_path"], b["repo_path"], "mismo repo");
    assert_ne!(a["worktree_path"], b["worktree_path"]);
    assert_ne!(a["branch"], b["branch"]);
    for w in [a, b] {
        assert!(Path::new(w["worktree_path"].as_str().unwrap()).is_dir());
        assert_eq!(w["worktree"]["status"], "ok");
    }
    // Filtro por thread: cada uno ve solo el suyo.
    assert_eq!(ws_list(&env, &["--thread", &ta]).len(), 1);
    assert_eq!(
        ws_list(&env, &["--thread", &ta])[0]["workspace_id"],
        ws_a.as_str()
    );
}

// ── 3. lease: dueño y liveness (proceso real) ───────────────────────────────

#[test]
fn s4_lease_liveness_real() {
    let env = TestEnv::new("s4-t3");
    let thread = env.thread_new("t3");
    let script = env.script(
        "t3",
        serde_json::json!({"message": "x", "delay_ms": 15000, "edits": []}),
    );
    let mut child = spawn_run_bg(&env, &thread, &script);
    assert!(
        wait_until(15000, || {
            runs_of(&env, &thread).iter().any(|r| r.state == "running")
        }),
        "el run no quedó activo"
    );
    let run = runs_of(&env, &thread).remove(0);
    let ws = run.ws.clone().unwrap();

    let list = ws_list(&env, &[]);
    let w = entry(&list, &ws);
    assert_eq!(w["status"], "verified_leased", "{w:?}");
    assert_eq!(w["lease"]["run_id"], run.id.as_str());
    assert_eq!(w["lease"]["owner_alive"], true);
    assert_eq!(w["lease"]["pid"], child.id() as i64);
    assert_eq!(w["supervisor"]["status"], "alive");
    assert_eq!(w["reuse"]["eligible"], false);
    assert!(
        w["reuse"]["reason"]
            .as_str()
            .unwrap()
            .contains("lease vivo"),
        "{w:?}"
    );

    // SIGKILL al supervisor: el lease queda sin dueño vivo, pero el run sigue
    // sin resolver y la consulta NO lo reconcilia.
    sigkill(child.id());
    let _ = child.wait();
    assert!(
        wait_until(10000, || {
            let list = ws_list(&env, &[]);
            entry(&list, &ws)["lease"]["owner_alive"] == false
        }),
        "el lease no reflejó la muerte del dueño"
    );
    let list = ws_list(&env, &[]);
    let w = entry(&list, &ws);
    assert_eq!(w["status"], "stale_or_uncertain", "{w:?}");
    assert_eq!(w["supervisor"]["status"], "dead");
    assert_eq!(
        w["latest_run_state"], "running",
        "la consulta no reconcilia"
    );
    assert_eq!(w["reuse"]["eligible"], false);

    // Limpieza: matar el backend huérfano y reconciliar.
    kill_mock_of(&env, &thread);
    run_recover(&env);
    assert_eq!(runs_of(&env, &thread)[0].state, "failed");
}

// ── 4. lease vivo bloquea el borrado (RC2.1) y el guard S4 exige acción ─────

#[test]
fn s4_lease_vivo_bloquea_borrado_y_rc21() {
    let env = TestEnv::new("s4-t4");
    let thread = env.thread_new("t4");
    let script = env.script(
        "t4",
        serde_json::json!({"message": "x", "delay_ms": 15000, "edits": []}),
    );
    let mut child = spawn_run_bg(&env, &thread, &script);
    assert!(
        wait_until(15000, || runs_of(&env, &thread)
            .iter()
            .any(|r| r.state == "running")),
        "el run no quedó activo"
    );
    let run = runs_of(&env, &thread).remove(0);
    let ws = run.ws.clone().unwrap();
    let wt = env.data.join("worktrees").join(&ws);

    // 1. `worktree rm` con dueño vivo: rechazado; el mensaje NO sugiere
    //    `lease release --force` como bypass (RC2.1).
    let out = env.aeril(&["worktree", "rm", "--thread", &thread]);
    assert!(!out.status.success(), "no debe borrar con lease vivo");
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        err.contains("NO se libera, ni con `--force`"),
        "mensaje RC2.1: {err}"
    );
    assert!(wt.is_dir(), "el worktree sigue ahí");

    // 2. `lease release --force` con dueño vivo: rechazado (RC2.1).
    let out = env.aeril(&["lease", "release", "--workspace", &ws, "--force"]);
    assert!(!out.status.success(), "force no desplaza a un dueño vivo");
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        err.contains("NO se libera, ni con --force"),
        "mensaje: {err}"
    );

    // 3. Supervisor muerto: sigue bloqueado (stale, backend quizá vivo) y
    //    exige `--unsafe-ok` o reconciliación.
    sigkill(child.id());
    let _ = child.wait();
    let out = env.aeril(&["worktree", "rm", "--thread", &thread]);
    assert!(!out.status.success(), "stale no se borra a ciegas");
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(err.contains("no es seguro para borrar"), "guard S4: {err}");
    assert!(wt.is_dir());

    // 4. Backend muerto + reconciliación ⇒ veredicto Safe ⇒ borrado normal.
    kill_mock_of(&env, &thread);
    run_recover(&env);
    let out = env.aeril(&["worktree", "rm", "--thread", &thread]);
    assert!(
        out.status.success(),
        "tras reconciliar debería borrar: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!wt.exists(), "worktree borrado");
    let v = parse_json(&out.stdout);
    assert_eq!(v["removed"], true);
    assert_eq!(v["unsafe_ok"], false);
}

// ── 5. worktree ausente: missing, no válido (y snapshot intacto) ────────────

#[test]
fn s4_worktree_ausente_no_es_valido() {
    let env = TestEnv::new("s4-t5");
    let thread = env.thread_new("t5");
    let script = env.script("t5", mock_edit("NOTES.md", "# t5\n", "x"));
    assert_eq!(env.run_start(&thread, "x", &script)["state"], "completed");
    let run = runs_of(&env, &thread).remove(0);
    let ws = run.ws.clone().unwrap();
    let ready = env.ready_snapshots(&thread);
    assert_eq!(ready.len(), 1);
    let (snap, oid) = ready[0].clone();

    let out = env.aeril(&["worktree", "rm", "--thread", &thread]);
    assert!(out.status.success());

    let list = ws_list(&env, &[]);
    let w = entry(&list, &ws);
    assert_eq!(w["worktree"]["status"], "missing");
    assert_eq!(w["status"], "missing_worktree", "{w:?}");
    assert_eq!(w["reuse"]["eligible"], false);
    assert_ne!(w["status"], "verified_available");

    // "Ausente" es del worktree, NO del snapshot: el checkpoint sigue ready.
    let ready = env.ready_snapshots(&thread);
    assert_eq!(ready.len(), 1, "el snapshot sobrevive al borrado");
    assert_eq!(ready[0].0, snap);
    assert_eq!(ready[0].1, oid);
    let show = env.aeril_ok(&[
        "checkpoint",
        "show",
        "--thread",
        &thread,
        "--snapshot",
        &snap,
    ]);
    assert!(
        show["patch"].as_str().unwrap().contains("# t5"),
        "el patch sigue revisable por OID"
    );
}

// ── 6. worktree inválido / mismatched: nunca reutilizable ───────────────────

#[test]
fn s4_invalid_y_mismatched_no_reutilizables() {
    let env = TestEnv::new("s4-t6");
    let ta = env.thread_new("t6-a");
    let tb = env.thread_new("t6-b");
    let s = env.script("t6", mock_edit("NOTES.md", "# t6\n", "x"));
    assert_eq!(env.run_start(&ta, "x", &s)["state"], "completed");
    assert_eq!(env.run_start(&tb, "x", &s)["state"], "completed");
    let ws_a = runs_of(&env, &ta)[0].ws.clone().unwrap();
    let ws_b = runs_of(&env, &tb)[0].ws.clone().unwrap();
    let wt_a = env.data.join("worktrees").join(&ws_a);
    let wt_b = env.data.join("worktrees").join(&ws_b);

    // a) Mismatched: un commit en el worktree mueve HEAD fuera de la base.
    git_out(&wt_a, &["commit", "--allow-empty", "-q", "-m", "tamper"]);
    // b) Invalid: borrar SOLO el directorio de trabajo (el admin queda roto).
    std::fs::remove_dir_all(&wt_b).unwrap();

    let list = ws_list(&env, &[]);
    let a = entry(&list, &ws_a);
    assert_eq!(a["worktree"]["status"], "mismatched", "{a:?}");
    assert_eq!(a["status"], "invalid_worktree");
    assert_eq!(a["reuse"]["eligible"], false);
    assert!(
        a["reuse"]["reason"]
            .as_str()
            .unwrap()
            .contains("no utilizable"),
        "{a:?}"
    );
    let b = entry(&list, &ws_b);
    assert_eq!(b["worktree"]["status"], "invalid", "{b:?}");
    assert_eq!(b["status"], "invalid_worktree");
    assert_eq!(b["reuse"]["eligible"], false);
    assert!(
        !b["worktree"]["detail"].as_str().unwrap().is_empty(),
        "diagnóstico del worktree inválido"
    );
}

// ── 6b. repo no disponible: diagnóstico por workspace ───────────────────────

#[test]
fn s4_repo_no_disponible_es_diagnostico() {
    let env = TestEnv::new("s4-t6b");
    // Repo 2 propio: se borra después del run.
    let repo2 = env.root.join("repo2");
    std::fs::create_dir_all(&repo2).unwrap();
    git(&repo2, &["init", "-q", "-b", "main"]);
    git(&repo2, &["config", "user.email", "t@t"]);
    git(&repo2, &["config", "user.name", "t"]);
    std::fs::write(repo2.join("NOTES.md"), "# repo2\n").unwrap();
    git(&repo2, &["add", "-A"]);
    git(&repo2, &["commit", "-q", "-m", "base"]);
    let t2 = env.aeril_ok(&[
        "thread",
        "new",
        "--repo",
        repo2.to_str().unwrap(),
        "--title",
        "t6b",
    ])["thread_id"]
        .as_str()
        .unwrap()
        .to_string();
    let script = env.script("t6b", mock_edit("NOTES.md", "# t6b\n", "x"));
    assert_eq!(env.run_start(&t2, "x", &script)["state"], "completed");
    let ws2 = runs_of(&env, &t2)[0].ws.clone().unwrap();

    // Thread 1 en el repo principal, como control.
    let t1 = env.thread_new("t6b-ctrl");
    assert_eq!(env.run_start(&t1, "x", &script)["state"], "completed");
    let ws1 = runs_of(&env, &t1)[0].ws.clone().unwrap();

    std::fs::remove_dir_all(&repo2).unwrap();
    let list = ws_list(&env, &[]);
    let w2 = entry(&list, &ws2);
    assert_eq!(w2["status"], "repo_unavailable", "{w2:?}");
    assert_eq!(w2["worktree"]["status"], "repo_unavailable");
    assert!(
        w2["worktree"]["detail"]
            .as_str()
            .unwrap()
            .contains("no se pudo abrir"),
        "diagnóstico explícito"
    );
    assert_eq!(w2["reuse"]["eligible"], false);
    let w1 = entry(&list, &ws1);
    assert_eq!(
        w1["status"], "verified_available",
        "el control no se afecta"
    );
}

// ── 7. recovery_unsafe: sin reuso automático; rm exige --unsafe-ok ──────────

#[test]
fn s4_unsafe_bloquea_reuso_y_rm_exige_unsafe_ok() {
    let env = TestEnv::new("s4-t7");
    let thread = env.thread_new("t7");
    let base = env.head_oid();
    let script = env.script("t7", mock_edit("NOTES.md", "# t7\n", "x"));
    assert_eq!(env.run_start(&thread, "x", &script)["state"], "completed");
    let run_old = runs_of(&env, &thread).remove(0);
    let ws_old = run_old.ws.clone().unwrap();
    let wt_old = env.data.join("worktrees").join(&ws_old);

    // Inyectar el estado inseguro durable (como lo haría la recuperación S0).
    {
        let mut store = db(&env);
        aeril::store::events::append_typed(
            &mut store.conn,
            &thread,
            Some(&run_old.id),
            aeril::store::events::K_WORKSPACE_RECOVERY_UNSAFE,
            "system",
            &aeril::store::events::payload::WorkspaceRecoveryUnsafe {
                workspace_id: ws_old.clone(),
                run_id: run_old.id.clone(),
                backend_state: "test".into(),
                detail: "inyectado en test S4".into(),
            },
        )
        .unwrap();
    }

    let list = ws_list(&env, &[]);
    let w = entry(&list, &ws_old);
    assert_eq!(w["status"], "unsafe_for_reuse", "{w:?}");
    assert_eq!(w["recovery_unsafe"], true);
    assert_eq!(w["recovery_unsafe_detail"], "inyectado en test S4");
    assert_eq!(w["reuse"]["eligible"], false);
    assert!(
        w["reuse"]["reason"].as_str().unwrap().contains("inseguro"),
        "{w:?}"
    );

    // Borrado: bloqueado por defecto; con acción explícita, permitido.
    let out = env.aeril(&["worktree", "rm", "--thread", &thread]);
    assert!(!out.status.success(), "unsafe no se borra solo");
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(err.contains("no es seguro para borrar"), "{err}");
    assert!(wt_old.is_dir());
    let out = env.aeril(&["worktree", "rm", "--thread", &thread, "--unsafe-ok"]);
    assert!(
        out.status.success(),
        "acción explícita: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!wt_old.exists());
    assert_eq!(parse_json(&out.stdout)["unsafe_ok"], true);

    // El reuso automático queda bloqueado para siempre: un run nuevo con la
    // MISMA base crea un workspace distinto.
    let script2 = env.script("t7b", mock_edit("NOTES.md", "# t7b\n", "x"));
    assert_eq!(env.run_start(&thread, "x", &script2)["state"], "completed");
    let runs = runs_of(&env, &thread);
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1].base_oid.as_deref(), Some(base.as_str()));
    assert_ne!(
        runs[1].ws.as_deref(),
        Some(ws_old.as_str()),
        "el workspace inseguro no se reutiliza"
    );
    let list = ws_list(&env, &[]);
    assert_eq!(entry(&list, &ws_old)["status"], "unsafe_for_reuse");
}

// ── 8. el borrado preserva checkpoints, refs y comentarios ──────────────────

#[test]
fn s4_borrado_preserva_historia() {
    let env = TestEnv::new("s4-t8");
    let thread = env.thread_new("t8");
    let script = env.script("t8", mock_edit("NOTES.md", "# t8\n", "x"));
    assert_eq!(env.run_start(&thread, "x", &script)["state"], "completed");
    let run = runs_of(&env, &thread).remove(0);
    let ws = run.ws.clone().unwrap();
    let branch = run.branch.clone().unwrap();
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
        "revisar t8",
    ]);
    let comment_id = c["comment_id"].as_str().unwrap().to_string();
    let head_before = env.head_oid();

    let out = env.aeril(&["worktree", "rm", "--thread", &thread]);
    assert!(out.status.success());

    // Checkpoint: ref + OID + patch siguen disponibles.
    assert_eq!(
        env.ready_snapshots(&thread),
        vec![(snap.clone(), oid.clone())]
    );
    let show = env.aeril_ok(&[
        "checkpoint",
        "show",
        "--thread",
        &thread,
        "--snapshot",
        &snap,
    ]);
    assert!(show["patch"].as_str().unwrap().contains("# t8"));
    // Comentario: legible tras el borrado (anclado a snapshot inmutable).
    let view = env.aeril_ok(&[
        "comment",
        "show",
        "--thread",
        &thread,
        "--comment",
        &comment_id,
    ]);
    assert_eq!(view["comment_id"], comment_id.as_str());
    assert_eq!(view["text"], "revisar t8");
    let list = env.aeril_ok(&["comment", "list", "--thread", &thread]);
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .any(|c| c["comment_id"] == comment_id.as_str()),
        "el comentario sigue en la lista"
    );
    // Rama del workspace: sigue existiendo (el contrato de rm no la borra).
    let tip = env.rev_parse(&format!("refs/heads/{branch}"));
    assert!(!tip.is_empty(), "la rama del workspace sobrevive");
    // El repo principal no se tocó.
    assert_eq!(env.head_oid(), head_before);
    // Y la lista lo reporta como worktree ausente, no como válido.
    let list = ws_list(&env, &[]);
    assert_eq!(entry(&list, &ws)["status"], "missing_worktree");
}

// ── 9. rehidratación: el feedback recrea desde el checkpoint correcto ───────

#[test]
fn s4_rehidratacion_desde_checkpoint() {
    let env = TestEnv::new("s4-t9");
    let ta = env.thread_new("t9-a");
    let tb = env.thread_new("t9-b");
    let sa = env.script("t9a", mock_edit("NOTES.md", "# t9-a\n", "x"));
    let sb = env.script("t9b", mock_edit("NOTES.md", "# t9-b\n", "x"));
    assert_eq!(env.run_start(&ta, "x", &sa)["state"], "completed");
    assert_eq!(env.run_start(&tb, "x", &sb)["state"], "completed");
    let run_a = runs_of(&env, &ta).remove(0);
    let ws_a = run_a.ws.clone().unwrap();
    let ws_b = runs_of(&env, &tb)[0].ws.clone().unwrap();
    let wt_a = env.data.join("worktrees").join(&ws_a);
    let wt_b = env.data.join("worktrees").join(&ws_b);
    let head_b_before = git_out(&wt_b, &["rev-parse", "HEAD"]);
    let ready = env.ready_snapshots(&ta);
    assert_eq!(ready.len(), 1);
    let (snap, snap_oid) = ready[0].clone();

    // Borrar el worktree de A; el snapshot queda en refs por OID.
    assert!(
        env.aeril(&["worktree", "rm", "--thread", &ta])
            .status
            .success()
    );
    assert!(!wt_a.exists());

    // Comentario sobre el snapshot + feedback: debe recrear desde ESE oid.
    let c = env.aeril_ok(&[
        "comment",
        "add",
        "--thread",
        &ta,
        "--snapshot",
        &snap,
        "--path",
        "NOTES.md",
        "--start",
        "1",
        "--end",
        "1",
        "--text",
        "seguir t9",
    ]);
    let comment_id = c["comment_id"].as_str().unwrap().to_string();
    let feedback = env.script("t9f", mock_edit("NOTES.md", "# t9-a-2\n", "x"));
    let out = env.aeril_ok(&[
        "feedback",
        "send",
        "--thread",
        &ta,
        "--comments",
        &comment_id,
        "--backend",
        &format!("mock:{}", feedback.display()),
    ]);
    assert_eq!(out["state"], "completed", "{out:?}");

    // 1) Evento de recreación con from_oid == snapshot elegido.
    let recreated = event_payloads(&env, &ta, "workspace.recreated");
    assert_eq!(recreated.len(), 1, "{recreated:?}");
    assert_eq!(recreated[0]["workspace_id"], ws_a.as_str());
    assert_eq!(recreated[0]["from_oid"], snap_oid.as_str());
    // 2) El worktree fue recreado en la base del snapshot (HEAD == oid).
    assert!(wt_a.is_dir(), "worktree recreado");
    assert_eq!(git_out(&wt_a, &["rev-parse", "HEAD"]), snap_oid);
    // 3) El segundo checkpoint es del turno de feedback (no mezcla de threads).
    assert_eq!(env.ready_snapshots(&ta).len(), 2);
    // 4) El OTRO thread no se toca.
    assert!(wt_b.is_dir());
    assert_eq!(git_out(&wt_b, &["rev-parse", "HEAD"]), head_b_before);
    let list = ws_list(&env, &[]);
    assert_eq!(entry(&list, &ws_b)["status"], "verified_available");
    // 5) La sesión del motor se registra APARTE de los archivos (attempted/
    //    accepted), sin afirmar restauración de contexto.
    let engine = event_payloads(&env, &ta, "engine.session");
    assert!(!engine.is_empty());
    assert!(
        engine.last().unwrap()["attempted"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "se registra el intento de sesión: {:?}",
        engine.last()
    );
}

// ── 10. listado read-only y determinista ────────────────────────────────────

#[test]
fn s4_listado_readonly_y_determinista() {
    let env = TestEnv::new("s4-t10");
    let ta = env.thread_new("t10-a");
    let tb = env.thread_new("t10-b");
    let s = env.script("t10", mock_edit("NOTES.md", "# t10\n", "x"));
    assert_eq!(env.run_start(&ta, "x", &s)["state"], "completed");
    assert_eq!(env.run_start(&tb, "x", &s)["state"], "completed");
    let ws_a = runs_of(&env, &ta)[0].ws.clone().unwrap();
    let ws_b = runs_of(&env, &tb)[0].ws.clone().unwrap();

    let events_before = count_sql(&env, "SELECT COUNT(*) FROM events");
    let runs_before = count_sql(&env, "SELECT COUNT(*) FROM runs");
    let leases_before = count_sql(&env, "SELECT COUNT(*) FROM leases");
    let head_a = git_out(
        &env.data.join("worktrees").join(&ws_a),
        &["rev-parse", "HEAD"],
    );
    let head_b = git_out(
        &env.data.join("worktrees").join(&ws_b),
        &["rev-parse", "HEAD"],
    );
    let mtime_a = std::fs::metadata(env.data.join("worktrees").join(&ws_a).join("NOTES.md"))
        .unwrap()
        .modified()
        .unwrap();

    let first = env.aeril_ok(&["workspace", "list"]);
    let second = env.aeril_ok(&["workspace", "list", "--limit", "10"]);
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap(),
        "listado determinista"
    );
    assert_eq!(first.as_array().unwrap().len(), 2);

    // Read-only: ni eventos, ni runs, ni leases, ni worktrees cambiaron.
    assert_eq!(
        count_sql(&env, "SELECT COUNT(*) FROM events"),
        events_before
    );
    assert_eq!(count_sql(&env, "SELECT COUNT(*) FROM runs"), runs_before);
    assert_eq!(
        count_sql(&env, "SELECT COUNT(*) FROM leases"),
        leases_before
    );
    assert_eq!(
        git_out(
            &env.data.join("worktrees").join(&ws_a),
            &["rev-parse", "HEAD"]
        ),
        head_a
    );
    assert_eq!(
        git_out(
            &env.data.join("worktrees").join(&ws_b),
            &["rev-parse", "HEAD"]
        ),
        head_b
    );
    assert_eq!(
        std::fs::metadata(env.data.join("worktrees").join(&ws_a).join("NOTES.md"))
            .unwrap()
            .modified()
            .unwrap(),
        mtime_a
    );
}
