//! S5 — Concurrencia end-to-end (mock determinista).
//!
//! Valida que los mecanismos existentes (TUI N=2, recuperación S0, cancelación
//! RC2/B6, checkpoints Git, revisión por OID e integración FF con CAS) funcionan
//! JUNTOS bajo condiciones realistas. Topologías explícitas:
//!
//! - G1/G3/G4/G5/G7/G8: un proceso TUI (supervisores = hilos Rust del MISMO
//!   proceso) con backends mock como procesos hijos.
//! - G2a/G6/G9: procesos supervisor CLI independientes sobre el mismo store.
//! - G2b: falla SOLO el backend de A (grupo de procesos) dentro de la TUI;
//!   además se prueba SIGKILL del proceso TUI completo (ahí B NO sobrevive:
//!   ambos runs quedan sin resolver y se reconcilian).
//!
//! Fixture `{cwd_basename}` del mock: contenido único por worktree (el basename
//! del worktree es el workspace_id), sin cambios de producto.

mod common;
use common::pty::TuiPty;
use common::*;
use serde_json::Value;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::Barrier;
use std::time::{Duration, Instant};

// ── helpers de estado durable ────────────────────────────────────────────────

fn db(env: &TestEnv) -> aeril::store::Store {
    aeril::store::Store::open(&env.data.join("aeril.db")).unwrap()
}

#[derive(Debug, Clone)]
struct RunRow {
    id: String,
    state: String,
    ws: Option<String>,
    base_oid: Option<String>,
    started_ms: i64,
    ended_ms: Option<i64>,
}

fn runs_of(env: &TestEnv, thread_id: &str) -> Vec<RunRow> {
    let store = db(env);
    let mut stmt = store
        .conn
        .prepare(
            "SELECT id, state, workspace_id, base_oid, started_ms, ended_ms
             FROM runs WHERE thread_id = ?1 ORDER BY started_ms ASC, id ASC",
        )
        .unwrap();
    let rows = stmt
        .query_map([thread_id], |r| {
            Ok(RunRow {
                id: r.get(0)?,
                state: r.get(1)?,
                ws: r.get(2)?,
                base_oid: r.get(3)?,
                started_ms: r.get(4)?,
                ended_ms: r.get(5)?,
            })
        })
        .unwrap();
    rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
}

fn count_events(env: &TestEnv, thread_id: &str, kind: &str) -> i64 {
    let store = db(env);
    store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE thread_id = ?1 AND kind = ?2",
            rusqlite::params![thread_id, kind],
            |r| r.get(0),
        )
        .unwrap()
}

fn count_events_all(env: &TestEnv, kind: &str) -> i64 {
    let store = db(env);
    store
        .conn
        .query_row("SELECT COUNT(*) FROM events WHERE kind = ?1", [kind], |r| {
            r.get(0)
        })
        .unwrap()
}

fn count_run_ended_of(env: &TestEnv, run_id: &str) -> i64 {
    let store = db(env);
    store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE kind = 'run.ended' AND run_id = ?1",
            [run_id],
            |r| r.get(0),
        )
        .unwrap_or(0)
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

fn leases_all(env: &TestEnv) -> Vec<(String, String, i64, bool)> {
    let store = db(env);
    aeril::store::leases::list(&store.conn)
        .unwrap()
        .into_iter()
        .map(|(ws, i)| {
            let alive = aeril::store::leases::owner_alive(&i);
            (ws, i.run_id, i.pid, alive)
        })
        .collect()
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
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn sigkill(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

fn kill_group(pid: i64) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

fn is_alive(pid: i64) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn backend_pid_of(env: &TestEnv, thread_id: &str, run_id: &str) -> Option<i64> {
    event_payloads(env, thread_id, "backend.spawned")
        .into_iter()
        .find(|p| p["run_id"].as_str() == Some(run_id))
        .and_then(|p| p["pid"].as_i64())
}

fn run_recover(env: &TestEnv) {
    let out = env.aeril(&["recover"]);
    assert!(
        out.status.success(),
        "recover falló: {}",
        String::from_utf8_lossy(&out.stderr)
    );
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

fn rss_kb(pid: u32) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Ejecuta `run start` en un proceso propio. Si el SO mata el proceso
/// externamente (p. ej. presión de memoria en corridas paralelas del entorno
/// de desarrollo), reintenta UNA vez; un error REAL de la aplicación (exit
/// distinto de 0 con stderr) NO se reintenta.
fn run_process_once(
    env: &TestEnv,
    thread_id: &str,
    script: &Path,
    extra_env: &[(&str, &str)],
) -> Output {
    for attempt in 0..2 {
        let mut cmd = Command::new(BIN);
        cmd.args([
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
        .current_dir(&env.root);
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("run start");
        let killed_externally = out.status.code().is_none();
        if !killed_externally || attempt == 1 {
            return out;
        }
        eprintln!(
            "aviso: proceso de test matado externamente ({:?}); reintento único",
            out.status
        );
    }
    unreachable!()
}

fn parse_summary(out: &Output, label: &str) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "{label}: stdout no es JSON ({e}); status={:?}; stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn script_edit(delay_ms: u64, content: &str) -> Value {
    serde_json::json!({
        "message": "trabajando",
        "final_message": "listo",
        "delay_ms": delay_ms,
        "edits": [{"op": "write", "path": "NOTES.md", "content": content}],
    })
}

/// Script con contenido único por worktree (`{cwd_basename}` = workspace_id).
fn script_unique(delay_ms: u64) -> Value {
    script_edit(delay_ms, "# hecha por {cwd_basename}\n")
}

fn thread_new_spaced(env: &TestEnv, title: &str) -> String {
    std::thread::sleep(Duration::from_millis(30));
    env.thread_new(title)
}

// ── driver TUI (PTY) ─────────────────────────────────────────────────────────

struct Tui<'a> {
    env: &'a TestEnv,
    pty: TuiPty,
    sel: Option<String>,
}

impl<'a> Tui<'a> {
    fn launch(env: &'a TestEnv, backend: &str, extra_env: &[(&str, &str)]) -> Tui<'a> {
        let mut pty = TuiPty::spawn(env, &["--backend", backend], extra_env);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !pty.output_contains("Fase 1") {
            assert!(
                Instant::now() < deadline,
                "la TUI no dibujó el primer frame"
            );
            pty.pump(100);
        }
        Tui {
            env,
            pty,
            sel: None,
        }
    }

    fn settle(&mut self, ms: u64) {
        self.pty.pump(ms);
    }

    fn wait_db<F: FnMut() -> bool>(&mut self, timeout_ms: u64, mut f: F) -> bool {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            self.pty.pump(100);
            if f() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
        }
    }

    fn order(&self) -> Vec<String> {
        self.env
            .aeril_ok(&["thread", "list"])
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["thread_id"].as_str().unwrap().to_string())
            .collect()
    }

    fn navigate(&mut self, target: &str) {
        self.pty.send(b"r");
        self.pty.pump(200);
        self.pty.esc();
        self.pty.pump(120);
        if self.sel.is_none() {
            self.sel = self.order().first().cloned();
        }
        let order = self.order();
        let cur = self.sel.clone().unwrap();
        let ic = order.iter().position(|t| *t == cur).expect("sel en lista");
        let it = order
            .iter()
            .position(|t| *t == target)
            .expect("target en lista");
        if it > ic {
            for _ in 0..(it - ic) {
                self.pty.down();
            }
        } else {
            for _ in 0..(ic - it) {
                self.pty.up();
            }
        }
        self.pty.pump(120);
        self.sel = Some(target.to_string());
    }

    fn open(&mut self, target: &str) {
        self.navigate(target);
        self.pty.enter();
        self.pty.pump(150);
    }

    fn start_task(&mut self, target: &str, prompt: &str) {
        self.open(target);
        self.pty.send(b"n");
        self.pty.pump(120);
        self.pty.type_line(prompt);
        self.pty.pump(150);
    }

    fn cancel(&mut self, target: &str) {
        self.navigate(target);
        self.pty.send(b"x");
        self.pty.pump(150);
    }

    fn wait_run_active(&mut self, thread_id: &str, timeout_ms: u64) -> bool {
        let env = self.env;
        self.wait_db(timeout_ms, || {
            runs_of(env, thread_id).iter().any(|r| r.state == "running")
        })
    }

    fn wait_run_terminal(&mut self, thread_id: &str, timeout_ms: u64) -> bool {
        let env = self.env;
        self.wait_db(timeout_ms, || {
            runs_of(env, thread_id)
                .last()
                .map(|r| r.state != "running" && r.state != "starting")
                .unwrap_or(false)
        })
    }

    fn wait_session(&mut self, thread_id: &str, timeout_ms: u64) -> bool {
        let env = self.env;
        self.wait_db(timeout_ms, || {
            count_events(env, thread_id, "engine.session") >= 1
        })
    }
}

// ── G1: dos runs concurrentes desde una TUI ─────────────────────────────────

#[test]
fn g1_dos_runs_concurrentes_desde_una_tui() {
    let env = TestEnv::new("s5-g1");
    let b = thread_new_spaced(&env, "g1-b");
    let a = thread_new_spaced(&env, "g1-a");
    let head0 = env.head_oid();
    let script = env.script("g1", script_unique(5000));
    let backend = format!("mock:{}", script.display());

    let mut tui = Tui::launch(&env, &backend, &[]);
    tui.start_task(&a, "tarea A");
    assert!(tui.wait_run_active(&a, 15000), "A no quedó activo");
    tui.start_task(&b, "tarea B");
    assert!(tui.wait_run_active(&b, 15000), "B no quedó activo");
    assert!(tui.wait_session(&a, 15000));
    assert!(tui.wait_session(&b, 15000));

    // Concurrencia real: ambos ACTIVOS a la vez, vistos por otro proceso.
    let active = env.aeril_ok(&["run", "list", "--active"]);
    assert_eq!(active.as_array().unwrap().len(), 2, "{active:?}");
    // Dos leases simultáneamente válidos.
    let leases = leases_all(&env);
    assert_eq!(leases.len(), 2, "{leases:?}");
    assert!(leases.iter().all(|(_, _, _, alive)| *alive), "{leases:?}");

    assert!(tui.wait_run_terminal(&a, 25000));
    assert!(tui.wait_run_terminal(&b, 25000));
    let ra = runs_of(&env, &a).remove(0);
    let rb = runs_of(&env, &b).remove(0);
    assert_eq!(ra.state, "completed");
    assert_eq!(rb.state, "completed");
    // Identidades y workspaces distintos; paths de worktree distintos y reales.
    assert_ne!(ra.id, rb.id);
    let (wsa, wsb) = (ra.ws.clone().unwrap(), rb.ws.clone().unwrap());
    assert_ne!(wsa, wsb);
    let wta = env.data.join("worktrees").join(&wsa);
    let wtb = env.data.join("worktrees").join(&wsb);
    assert!(wta.is_dir() && wtb.is_dir());
    assert_ne!(wta, wtb);
    // Intervalos solapados.
    let (a0, a1) = (ra.started_ms, ra.ended_ms.unwrap());
    let (b0, b1) = (rb.started_ms, rb.ended_ms.unwrap());
    assert!(b0 < a1 && a0 < b1, "sin solape: A[{a0},{a1}] B[{b0},{b1}]");
    // Cambios de archivo aislados: cada worktree tiene SU workspace_id en el
    // contenido y no el del otro.
    let fa = std::fs::read_to_string(wta.join("NOTES.md")).unwrap();
    let fb = std::fs::read_to_string(wtb.join("NOTES.md")).unwrap();
    assert!(fa.contains(&wsa) && !fa.contains(&wsb), "A: {fa:?}");
    assert!(fb.contains(&wsb) && !fb.contains(&wsa), "B: {fb:?}");
    // Dos checkpoints independientes y listos, con los contenidos correctos.
    let ca = env.ready_snapshots(&a);
    let cb = env.ready_snapshots(&b);
    assert_eq!(ca.len(), 1);
    assert_eq!(cb.len(), 1);
    assert_ne!(ca[0].0, cb[0].0);
    let show_a = env.aeril_ok(&["checkpoint", "show", "--thread", &a, "--snapshot", &ca[0].0]);
    let show_b = env.aeril_ok(&["checkpoint", "show", "--thread", &b, "--snapshot", &cb[0].0]);
    assert!(show_a["patch"].as_str().unwrap().contains(&wsa));
    assert!(show_b["patch"].as_str().unwrap().contains(&wsb));
    // El checkout principal no se toca.
    assert_eq!(env.file("NOTES.md"), "# Notas\n\nVersión original.\n");
    assert_eq!(env.head_oid(), head0);
    assert_eq!(git_out(&env.repo, &["status", "--porcelain"]), "");
}

// ── G2a: crash de un supervisor CLI aislado (procesos separados) ────────────

#[test]
fn g2a_crash_supervisor_aislado() {
    let env = TestEnv::new("s5-g2a");
    let a = env.thread_new("g2a-a");
    let b = env.thread_new("g2a-b");

    // A: primer turno completado (checkpoint listo) y segundo turno largo que
    // será interrumpido por SIGKILL del supervisor.
    let quick = env.script("g2a-q", script_edit(0, "# turno 1 de A\n"));
    assert_eq!(env.run_start(&a, "x", &quick)["state"], "completed");
    let ckpt_a = env.ready_snapshots(&a);
    assert_eq!(ckpt_a.len(), 1);
    let slow = env.script(
        "g2a-slow",
        serde_json::json!({"message": "x", "delay_ms": 30000, "edits": []}),
    );
    let sup_a = spawn_run_bg(&env, &a, &slow);
    assert!(
        wait_until(15000, || runs_of(&env, &a).len() == 2
            && runs_of(&env, &a)[1].state == "running"),
        "A2 no quedó activo"
    );
    // B concurrente (proceso propio) que completa.
    let bscript = env.script("g2a-b", script_edit(6000, "# turno de B\n"));
    let sup_b = spawn_run_bg(&env, &b, &bscript);
    assert!(wait_until(15000, || runs_of(&env, &b)
        .iter()
        .any(|r| r.state == "running")));

    // SIGKILL al supervisor de A (solo su proceso).
    let a2 = runs_of(&env, &a)[1].clone();
    let ws_a = a2.ws.clone().unwrap();
    let mock_a = backend_pid_of(&env, &a, &a2.id).expect("backend de A2");
    assert!(
        is_alive(mock_a),
        "el backend de A2 sigue vivo tras el SIGKILL"
    );
    sigkill(sup_a.id());
    let mut sup_a = sup_a;
    let _ = sup_a.wait();

    // B continúa y completa.
    let out_b = sup_b.wait_with_output().unwrap();
    assert_eq!(
        parse_json(&out_b.stdout)["state"],
        "completed",
        "B debe completar"
    );
    assert_eq!(env.ready_snapshots(&b).len(), 1);

    // Recuperación: identifica A2 como interrumpido, una sola vez.
    let rec = env.aeril_ok(&["recover"]);
    let entry = rec
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["action"] == "run_interrupted" && e["run_id"] == a2.id.as_str())
        .expect("A2 reconciliado")
        .clone();
    let runs_a = runs_of(&env, &a);
    assert_eq!(runs_a[1].state, "failed");
    assert_eq!(
        count_run_ended_of(&env, &a2.id),
        1,
        "un único run.ended para A2"
    );
    assert_eq!(
        count_events(&env, &a, "checkpoint.ready"),
        1,
        "A2 no captura"
    );
    // Backend atribuible: verificado y terminado por la recuperación (o ya
    // muerto); sin incertidumbre ⇒ sin bloqueo conservador.
    assert_eq!(entry["unsafe_state"], false, "{entry}");
    let backend_desc = entry["backend"].as_str().unwrap_or_default();
    assert!(
        backend_desc.contains("terminado durante la recuperación")
            || backend_desc.contains("muerto"),
        "backend verificado: {entry}"
    );
    assert!(
        wait_until(5000, || !is_alive(mock_a)),
        "el backend de A2 no queda vivo"
    );
    assert_eq!(
        event_payloads(&env, &a, "workspace.recovery_unsafe").len(),
        0,
        "sin incertidumbre no hay bloqueo"
    );

    // El checkpoint previo de A sigue disponible para revisión/rehidratación.
    let still = env.ready_snapshots(&a);
    assert_eq!(still.len(), 1);
    assert_eq!(still[0].0, ckpt_a[0].0);
    let show = env.aeril_ok(&[
        "checkpoint",
        "show",
        "--thread",
        &a,
        "--snapshot",
        &still[0].0,
    ]);
    assert!(show["patch"].as_str().unwrap().contains("# turno 1 de A"));

    // Reconciliación repetida: idempotente (sin duplicados ni flags nuevos).
    run_recover(&env);
    assert_eq!(
        count_run_ended_of(&env, &a2.id),
        1,
        "sin duplicados tras 2º recover"
    );
    assert_eq!(count_events(&env, &a, "checkpoint.ready"), 1);

    // Sin backend vivo NI incertidumbre ⇒ reuso SEGURO del workspace (la
    // limpieza quedó verificada); el bloqueo conservador (identidad no
    // verificable) está cubierto por las suites S0/S0-R1/S4.
    let after = env.script("g2a-after", script_edit(0, "# tras recuperar\n"));
    assert_eq!(env.run_start(&a, "x", &after)["state"], "completed");
    let runs_a = runs_of(&env, &a);
    assert_eq!(
        runs_a[2].ws.as_deref(),
        Some(ws_a.as_str()),
        "con terminación verificada el reuso es seguro"
    );
}

// ── G2b-1: falla SOLO el backend de A dentro de la TUI ──────────────────────

#[test]
fn g2b_falla_backend_de_a_en_tui() {
    let env = TestEnv::new("s5-g2b1");
    let b = thread_new_spaced(&env, "g2b-b");
    let a = thread_new_spaced(&env, "g2b-a");
    let script = env.script(
        "g2b",
        serde_json::json!({
            "message": "x", "final_message": "fin", "delay_ms": 12000,
            "edits": [{"op": "write", "path": "NOTES.md", "content": "# {cwd_basename}\n"}],
        }),
    );
    let backend = format!("mock:{}", script.display());

    let mut tui = Tui::launch(&env, &backend, &[]);
    tui.start_task(&a, "A");
    assert!(tui.wait_run_active(&a, 15000));
    tui.start_task(&b, "B");
    assert!(tui.wait_run_active(&b, 15000));
    assert!(tui.wait_session(&a, 15000));
    assert!(tui.wait_session(&b, 15000));

    // Matar SOLO el grupo de procesos del backend de A.
    let run_a = runs_of(&env, &a).remove(0);
    let mock_a = backend_pid_of(&env, &a, &run_a.id).expect("backend de A");
    kill_group(mock_a);

    // A: resultado honesto (failed) sin checkpoint fabricado.
    assert!(tui.wait_run_terminal(&a, 15000), "A no terminó");
    assert_eq!(runs_of(&env, &a)[0].state, "failed");
    assert_eq!(count_events(&env, &a, "checkpoint.ready"), 0);

    // B sigue intacto y completa con su checkpoint.
    assert!(tui.wait_run_terminal(&b, 25000), "B no completó");
    assert_eq!(runs_of(&env, &b)[0].state, "completed");
    let ck = env.ready_snapshots(&b);
    assert_eq!(ck.len(), 1);
    assert!(
        tui.pty.alive(),
        "la TUI sobrevive a la muerte del backend de A"
    );
    // El evento de A está atribuido a A (no contaminó a B).
    assert_eq!(count_events(&env, &b, "run.ended"), 1);
    let runs_b = runs_of(&env, &b);
    let fb = std::fs::read_to_string(
        env.data
            .join("worktrees")
            .join(runs_b[0].ws.clone().unwrap())
            .join("NOTES.md"),
    )
    .unwrap();
    assert!(fb.contains(runs_b[0].ws.as_deref().unwrap()));
}

// ── G2b-2: SIGKILL del proceso TUI completo (B NO sobrevive) ────────────────

#[test]
fn g2b_sigkill_de_la_tui_completa_se_reconcilia() {
    let env = TestEnv::new("s5-g2b2");
    let b = thread_new_spaced(&env, "g2b2-b");
    let a = thread_new_spaced(&env, "g2b2-a");
    let script = env.script(
        "g2b2",
        serde_json::json!({"message": "x", "delay_ms": 30000, "edits": []}),
    );
    let backend = format!("mock:{}", script.display());

    let mut tui = Tui::launch(&env, &backend, &[]);
    tui.start_task(&a, "A");
    assert!(tui.wait_run_active(&a, 15000));
    tui.start_task(&b, "B");
    assert!(tui.wait_run_active(&b, 15000));
    assert!(tui.wait_session(&a, 15000));
    assert!(tui.wait_session(&b, 15000));
    let (run_a, run_b) = (runs_of(&env, &a)[0].clone(), runs_of(&env, &b)[0].clone());
    let (mock_a, mock_b) = (
        backend_pid_of(&env, &a, &run_a.id).unwrap(),
        backend_pid_of(&env, &b, &run_b.id).unwrap(),
    );

    // SIGKILL del proceso TUI: AMBOS runs quedan sin supervisor (no hay
    // aislamiento por hilo: los supervisores eran hilos del mismo proceso).
    sigkill(tui.pty.child.id());
    let _ = tui.pty.child.wait();
    assert!(wait_until(5000, || runs_of(&env, &a)[0].state == "running"
        && runs_of(&env, &b)[0].state == "running"));

    let rec = env.aeril_ok(&["recover"]);
    assert_eq!(runs_of(&env, &a)[0].state, "failed");
    assert_eq!(runs_of(&env, &b)[0].state, "failed");
    assert_eq!(count_run_ended_of(&env, &run_a.id), 1);
    assert_eq!(count_run_ended_of(&env, &run_b.id), 1);
    assert_eq!(count_events(&env, &a, "checkpoint.ready"), 0);
    assert_eq!(count_events(&env, &b, "checkpoint.ready"), 0);
    // Ambos backends quedan verificados (muertos o terminados por la
    // recuperación); ninguna incertidumbre ⇒ sin flags conservadores.
    for run in [&run_a.id, &run_b.id] {
        let entry = rec
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["action"] == "run_interrupted" && e["run_id"] == run.as_str())
            .unwrap_or_else(|| panic!("run {run} no reconciliado: {rec}"));
        assert_eq!(entry["unsafe_state"], false, "{entry}");
    }
    assert!(wait_until(5000, || !is_alive(mock_a) && !is_alive(mock_b)));
    // Reconciliación repetida: sin duplicados ni éxitos fabricados.
    run_recover(&env);
    assert_eq!(count_run_ended_of(&env, &run_a.id), 1);
    assert_eq!(count_run_ended_of(&env, &run_b.id), 1);
}

// ── G3: cancelación independiente (cooperativa y por plazo) ─────────────────

#[test]
fn g3_cancelacion_independiente_y_por_plazo() {
    let env = TestEnv::new("s5-g3");
    let b = thread_new_spaced(&env, "g3-b");
    let a = thread_new_spaced(&env, "g3-a");
    // TUI1: script que IGNORA la cancelación ⇒ A ejercita el plazo fijo
    // (AERIL_CANCEL_GRACE_SECS=3) y el slot-until-ack queda determinista;
    // B completa normalmente (no se cancela).
    let stubborn = env.script(
        "g3-stubborn",
        serde_json::json!({
            "message": "x", "final_message": "fin", "delay_ms": 12000,
            "ignore_cancel": true,
            "edits": [{"op": "write", "path": "NOTES.md", "content": "# {cwd_basename}\n"}],
        }),
    );
    let backend = format!("mock:{}", stubborn.display());

    let mut tui = Tui::launch(&env, &backend, &[("AERIL_CANCEL_GRACE_SECS", "6")]);
    tui.start_task(&a, "A");
    assert!(tui.wait_run_active(&a, 15000));
    tui.start_task(&b, "B");
    assert!(tui.wait_run_active(&b, 15000));
    assert!(tui.wait_session(&a, 15000));
    assert!(tui.wait_session(&b, 15000));

    // 1) Cancelar A por teclado.
    let run_a = runs_of(&env, &a)[0].clone();
    let mock_a = backend_pid_of(&env, &a, &run_a.id).unwrap();
    let t0 = Instant::now();
    tui.cancel(&a);
    // 2) El slot de A queda ocupado hasta el ack: un reemplazo inmediato se
    //    rechaza mientras B sigue vivo.
    tui.start_task(&a, "A-reemplazo");
    tui.settle(250);
    assert_eq!(runs_of(&env, &a).len(), 1, "sin reemplazo antes del ack");
    assert!(
        tui.pty.output_contains("ya tiene un run cancelando"),
        "gate visible: {}",
        tui.pty.output_lossy()
    );
    // 3) A termina por el plazo acotado: cancelado, sin checkpoint, backend
    //    terminado (no queda proceso atribuible).
    assert!(tui.wait_run_terminal(&a, 20000), "A no terminó por plazo");
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "plazo fijo (~6 s), no el delay del mock: {elapsed:?}"
    );
    assert_eq!(runs_of(&env, &a)[0].state, "cancelled");
    assert_eq!(count_events(&env, &a, "checkpoint.ready"), 0);
    assert!(
        wait_until(5000, || !is_alive(mock_a)),
        "backend de A terminado por plazo"
    );
    // 4) B sigue corriendo y completa con su checkpoint.
    assert!(tui.wait_run_terminal(&b, 25000));
    assert_eq!(runs_of(&env, &b)[0].state, "completed");
    assert_eq!(env.ready_snapshots(&b).len(), 1);

    // 5) Caso cooperativo: otro TUI (mismo store) con script que coopera.
    let c = thread_new_spaced(&env, "g3-c");
    let coop = env.script("g3-coop", script_unique(15000));
    let backend2 = format!("mock:{}", coop.display());
    let mut tui2 = Tui::launch(&env, &backend2, &[]);
    tui2.start_task(&c, "C");
    assert!(tui2.wait_run_active(&c, 15000));
    let run_c = runs_of(&env, &c)[0].clone();
    let mock_c = backend_pid_of(&env, &c, &run_c.id).unwrap();
    tui2.cancel(&c);
    assert!(
        tui2.wait_run_terminal(&c, 15000),
        "C no terminó (cooperativo)"
    );
    assert_eq!(runs_of(&env, &c)[0].state, "cancelled");
    assert_eq!(count_events(&env, &c, "checkpoint.ready"), 0);
    assert!(
        wait_until(5000, || !is_alive(mock_c)),
        "backend cooperativo muerto"
    );
    assert!(tui2.pty.alive());
}

// ── G4: revisión durable tras borrado del worktree ──────────────────────────

#[test]
fn g4_revision_durable_tras_borrado() {
    let env = TestEnv::new("s5-g4");
    let a = env.thread_new("g4-a");
    let b = env.thread_new("g4-b");
    let s = env.script("g4", script_unique(0));
    assert_eq!(env.run_start(&a, "x", &s)["state"], "completed");
    assert_eq!(env.run_start(&b, "x", &s)["state"], "completed");
    let run_a = runs_of(&env, &a).remove(0);
    let run_b = runs_of(&env, &b).remove(0);
    let (wsa, wsb) = (run_a.ws.clone().unwrap(), run_b.ws.clone().unwrap());
    let wt_b = env.data.join("worktrees").join(&wsb);
    let head_b_before = git_out(&wt_b, &["rev-parse", "HEAD"]);
    let head0 = env.head_oid();
    let ca = env.ready_snapshots(&a);
    let cb = env.ready_snapshots(&b);
    assert_eq!(ca.len(), 1);
    assert_eq!(cb.len(), 1);
    let comment = env.aeril_ok(&[
        "comment",
        "add",
        "--thread",
        &a,
        "--snapshot",
        &ca[0].0,
        "--path",
        "NOTES.md",
        "--start",
        "1",
        "--end",
        "1",
        "--text",
        "g4 revisión",
    ]);
    let cid = comment["comment_id"].as_str().unwrap().to_string();

    // Borrado seguro (sin --unsafe-ok: no hace falta).
    let out = env.aeril(&["worktree", "rm", "--thread", &a]);
    assert!(
        out.status.success(),
        "borrado seguro: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Refs + comentarios de A siguen accesibles.
    assert_eq!(env.ready_snapshots(&a), vec![ca[0].clone()]);
    let show = env.aeril_ok(&["checkpoint", "show", "--thread", &a, "--snapshot", &ca[0].0]);
    assert!(show["patch"].as_str().unwrap().contains(&wsa));
    let view = env.aeril_ok(&["comment", "show", "--thread", &a, "--comment", &cid]);
    assert_eq!(view["text"], "g4 revisión");
    let list = env.aeril_ok(&["comment", "list", "--thread", &a]);
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .any(|c| c["comment_id"] == cid.as_str())
    );
    // B no cambió; el repo principal tampoco.
    assert!(wt_b.is_dir());
    assert_eq!(git_out(&wt_b, &["rev-parse", "HEAD"]), head_b_before);
    assert_eq!(env.ready_snapshots(&b), vec![cb[0].clone()]);
    assert_eq!(env.head_oid(), head0);
}

// ── G5: restauración independiente por hilo desde SU checkpoint ─────────────

#[test]
fn g5_restauracion_independiente() {
    let env = TestEnv::new("s5-g5");
    let a = env.thread_new("g5-a");
    let b = env.thread_new("g5-b");
    let s = env.script("g5", script_unique(0));
    assert_eq!(env.run_start(&a, "x", &s)["state"], "completed");
    assert_eq!(env.run_start(&b, "x", &s)["state"], "completed");
    let (wsa, wsb) = (
        runs_of(&env, &a)[0].ws.clone().unwrap(),
        runs_of(&env, &b)[0].ws.clone().unwrap(),
    );
    let ca = env.ready_snapshots(&a);
    let cb = env.ready_snapshots(&b);
    assert_eq!(ca.len(), 1);
    assert_eq!(cb.len(), 1);
    assert_ne!(ca[0].1, cb[0].1);

    // Borrar ambos worktrees por el ciclo de vida guardado.
    assert!(
        env.aeril(&["worktree", "rm", "--thread", &a])
            .status
            .success()
    );
    assert!(
        env.aeril(&["worktree", "rm", "--thread", &b])
            .status
            .success()
    );

    // Reanudar cada hilo por feedback (recreación desde SU snapshot).
    let c_a = env.aeril_ok(&[
        "comment",
        "add",
        "--thread",
        &a,
        "--snapshot",
        &ca[0].0,
        "--path",
        "NOTES.md",
        "--start",
        "1",
        "--end",
        "1",
        "--text",
        "seguir A",
    ]);
    let c_b = env.aeril_ok(&[
        "comment",
        "add",
        "--thread",
        &b,
        "--snapshot",
        &cb[0].0,
        "--path",
        "NOTES.md",
        "--start",
        "1",
        "--end",
        "1",
        "--text",
        "seguir B",
    ]);
    let fa = env.script("g5-fa", script_edit(0, "# seguimos A\n"));
    let fb = env.script("g5-fb", script_edit(0, "# seguimos B\n"));
    let ra = env.aeril_ok(&[
        "feedback",
        "send",
        "--thread",
        &a,
        "--comments",
        c_a["comment_id"].as_str().unwrap(),
        "--backend",
        &format!("mock:{}", fa.display()),
    ]);
    assert_eq!(ra["state"], "completed", "{ra:?}");
    let rb = env.aeril_ok(&[
        "feedback",
        "send",
        "--thread",
        &b,
        "--comments",
        c_b["comment_id"].as_str().unwrap(),
        "--backend",
        &format!("mock:{}", fb.display()),
    ]);
    assert_eq!(rb["state"], "completed", "{rb:?}");

    // Recreación durable desde el OID EXACTO de cada hilo (sin cruces).
    let rec_a = event_payloads(&env, &a, "workspace.recreated");
    let rec_b = event_payloads(&env, &b, "workspace.recreated");
    assert_eq!(rec_a.len(), 1, "{rec_a:?}");
    assert_eq!(rec_b.len(), 1, "{rec_b:?}");
    assert_eq!(rec_a[0]["workspace_id"], wsa.as_str());
    assert_eq!(rec_a[0]["from_oid"], ca[0].1.as_str());
    assert_eq!(rec_b[0]["workspace_id"], wsb.as_str());
    assert_eq!(rec_b[0]["from_oid"], cb[0].1.as_str());
    let wta = env.data.join("worktrees").join(&wsa);
    let wtb = env.data.join("worktrees").join(&wsb);
    assert_eq!(
        git_out(&wta, &["rev-parse", "HEAD"]),
        ca[0].1,
        "A desde su OID"
    );
    assert_eq!(
        git_out(&wtb, &["rev-parse", "HEAD"]),
        cb[0].1,
        "B desde su OID"
    );
    assert_ne!(git_out(&wta, &["rev-parse", "HEAD"]), cb[0].1, "sin cruce");

    // Registro de sesión del motor SEPARADO de los archivos: se reporta
    // attempted/accepted/verified; con v0 `verified` es false y NO se afirma
    // restauración de contexto.
    for (thread, label) in [(&a, "A"), (&b, "B")] {
        let sessions = event_payloads(&env, thread, "engine.session");
        let last = sessions.last().expect("engine.session del feedback");
        assert!(
            last["attempted"].as_str().is_some_and(|s| !s.is_empty()),
            "attempted presente ({label}): {last}"
        );
        assert_eq!(last["verified"], false, "sin claim de contexto verificado");
        eprintln!(
            "G5 sesión {label}: attempted={} accepted={} verified={}",
            last["attempted"], last["accepted"], last["verified"]
        );
    }
}

// ── G6: fallo de auditoría de permisos aislado (procesos separados) ─────────

#[test]
fn g6_auditoria_de_permisos_aislada() {
    let env = TestEnv::new("s5-g6");
    let a = env.thread_new("g6-a");
    let b = env.thread_new("g6-b");
    let script = env.script("g6", script_unique(0));

    // A: fallo inyectado SOLO en su proceso (env por proceso, no global).
    // Procesos independientes; el objetivo es el aislamiento del fallo por
    // proceso (no la carrera), así que corren secuenciales y deterministas.
    let out_a = run_process_once(
        &env,
        &a,
        &script,
        &[("AERIL_FAULT_APPEND", "permission.requested")],
    );
    let out_b = run_process_once(&env, &b, &script, &[]);

    // A falla cerrado: sin auditoría no hay autorización ni edición.
    let sa = parse_summary(&out_a, "A");

    assert_eq!(sa["state"], "failed", "A: {sa:?}");
    assert!(
        sa["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("permission.requested"),
        "detalle: {sa:?}"
    );
    let wt_a = env
        .data
        .join("worktrees")
        .join(runs_of(&env, &a)[0].ws.clone().unwrap());
    assert_eq!(
        std::fs::read_to_string(wt_a.join("NOTES.md")).unwrap(),
        "# Notas\n\nVersión original.\n",
        "el pedido no persistido NO autoriza la edición"
    );
    assert_eq!(count_events(&env, &a, "permission.responded"), 0);
    assert_eq!(count_events(&env, &a, "permission.audit_failed"), 1);

    // B no fue afectado: completa con su edición y su auditoría.
    let sb = parse_summary(&out_b, "B");
    assert_eq!(sb["state"], "completed", "B: {sb:?}");
    assert!(count_events(&env, &b, "permission.responded") >= 1);
    assert_eq!(count_events(&env, &b, "permission.audit_failed"), 0);
    assert_eq!(count_events(&env, &b, "checkpoint.ready"), 1);
    // Sin contaminación cruzada de eventos.
    assert_eq!(count_events(&env, &a, "checkpoint.ready"), 0);
    assert_eq!(count_events(&env, &b, "run.ended"), 1);
}

// ── G7: mediciones del escenario real de dos runs ───────────────────────────

#[test]
fn g7_mediciones_con_dos_runs() {
    let env = TestEnv::new("s5-g7");
    // Historia en el store: 3 runs completados antes de medir el arranque.
    for i in 0..3 {
        let t = env.thread_new(&format!("g7-hist-{i}"));
        let s = env.script(&format!("g7-h{i}"), script_edit(0, "# hist\n"));
        assert_eq!(env.run_start(&t, "x", &s)["state"], "completed");
    }
    let b = thread_new_spaced(&env, "g7-b");
    let a = thread_new_spaced(&env, "g7-a");
    let script = env.script("g7", script_unique(5000));
    let backend = format!("mock:{}", script.display());

    let beacon = env.root.join("beacon.jsonl");
    let mut tui = Tui::launch(
        &env,
        &backend,
        &[("AERIL_BENCH_BEACON", beacon.to_str().unwrap())],
    );
    // Arranque con historia: primer frame + primera tecla procesada (beacons).
    assert!(
        wait_until(10000, || std::fs::read_to_string(&beacon)
            .map(|s| s.contains("first_draw"))
            .unwrap_or(false)),
        "sin beacon first_draw"
    );
    tui.pty.send(b"r");
    assert!(
        wait_until(10000, || std::fs::read_to_string(&beacon)
            .map(|s| s.contains("input_processed"))
            .unwrap_or(false)),
        "sin beacon input_processed"
    );
    let startup_ms = std::fs::read_to_string(&beacon)
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["stage"] == "input_processed")
        .and_then(|v| v["internal_us"].as_u64())
        .map(|us| us as f64 / 1000.0)
        .unwrap();
    assert!(
        startup_ms < 1500.0,
        "arranque debug con historia: {startup_ms} ms"
    );

    // Dos runs activos + medición de responsabilidad del event loop.
    tui.start_task(&a, "A");
    assert!(tui.wait_run_active(&a, 15000));
    let t0 = Instant::now();
    tui.start_task(&b, "B");
    assert!(tui.wait_run_active(&b, 15000));
    let react_ms = t0.elapsed().as_millis() as f64;

    // RSS: TUI y cada backend mock por separado (los mocks NO son Hermes).
    let tui_rss = rss_kb(tui.pty.child.id());
    let run_a = runs_of(&env, &a).remove(0);
    let run_b = runs_of(&env, &b).remove(0);
    let (ma, mb) = (
        backend_pid_of(&env, &a, &run_a.id).unwrap() as u32,
        backend_pid_of(&env, &b, &run_b.id).unwrap() as u32,
    );
    let (rss_a, rss_b) = (rss_kb(ma), rss_kb(mb));
    assert!(tui_rss < 200 * 1024, "TUI RSS {tui_rss} KB");
    assert!(rss_a < 200 * 1024 && rss_b < 200 * 1024);

    // Latencia de consultas con dos runs activos.
    let mut q_max = 0f64;
    for _ in 0..3 {
        for args in [vec!["run", "list", "--active"], vec!["workspace", "list"]] {
            let t = Instant::now();
            let out = env.aeril(&args);
            assert!(out.status.success());
            q_max = q_max.max(t.elapsed().as_millis() as f64);
        }
    }
    assert!(q_max < 1500.0, "query latency {q_max} ms");
    eprintln!(
        "G7: startup(historia)={startup_ms:.1} ms · TUI RSS={:.1} MB · mock A={:.1} MB · mock B={:.1} MB · combinado={:.1} MB · run-creation={react_ms:.0} ms · query max={q_max:.0} ms",
        tui_rss as f64 / 1024.0,
        rss_a as f64 / 1024.0,
        rss_b as f64 / 1024.0,
        (tui_rss + rss_a + rss_b) as f64 / 1024.0,
    );

    // Limpieza.
    tui.cancel(&a);
    assert!(tui.wait_run_terminal(&a, 15000));
    tui.cancel(&b);
    assert!(tui.wait_run_terminal(&b, 15000));
}

// ── G8: la TUI mantiene la restricción de un Hermes real por vez ────────────

#[test]
fn g8_hermes_una_sesion_por_vez() {
    let env = TestEnv::new("s5-g8");
    let b = thread_new_spaced(&env, "g8-b");
    let a = thread_new_spaced(&env, "g8-a");
    let hold = env.script("g8", script_unique(15000));
    let bindir = env.root.join("bin");
    std::fs::create_dir_all(&bindir).unwrap();
    let shim = bindir.join("hermes");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nexec \"{}\" __mock-agent --script \"{}\"\n",
            BIN,
            hold.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        bindir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let mut tui = Tui::launch(&env, "hermes", &[("PATH", &path)]);
    tui.start_task(&a, "Hermes A");
    assert!(tui.wait_run_active(&a, 15000));
    assert_eq!(runs_of(&env, &a)[0].state, "running");
    // Segundo Hermes simultáneo: rechazado y visible.
    tui.start_task(&b, "Hermes B");
    tui.settle(300);
    assert!(runs_of(&env, &b).is_empty());
    assert!(
        tui.pty.output_contains("Hermes real: un run por vez"),
        "{}",
        tui.pty.output_lossy()
    );
    assert!(runs_of(&env, &a)[0].base_oid.is_some());
    tui.cancel(&a);
    assert!(tui.wait_run_terminal(&a, 20000));
}

// ── G9: integración fast-forward concurrente (CAS) ──────────────────────────

#[test]
fn g9_integracion_concurrente_ff() {
    let env = TestEnv::new("s5-g9");
    let a = env.thread_new("g9-a");
    let b = env.thread_new("g9-b");
    let head0 = env.head_oid();
    let s = env.script("g9", script_unique(0));
    assert_eq!(env.run_start(&a, "x", &s)["state"], "completed");
    assert_eq!(env.run_start(&b, "x", &s)["state"], "completed");
    let (ra, rb) = (runs_of(&env, &a)[0].clone(), runs_of(&env, &b)[0].clone());
    assert_eq!(ra.base_oid.as_deref(), Some(head0.as_str()));
    assert_eq!(rb.base_oid.as_deref(), Some(head0.as_str()));
    // Rama destino NO chequeada (el checkout principal usa `main`; integrar a
    // una rama en uso está bloqueado por diseño), creada en el OID original.
    env.update_ref("refs/heads/destino", &head0);
    let ca = env.ready_snapshots(&a);
    let cb = env.ready_snapshots(&b);
    assert_eq!(ca.len(), 1);
    assert_eq!(cb.len(), 1);
    assert_ne!(ca[0].1, cb[0].1);

    // Dos intentos de integración concurrentes, sincronizados con una barrera.
    fn integrate_once(env: &TestEnv, thread: &str, snap: &str, expect: &str) -> Output {
        Command::new(BIN)
            .args([
                "integrate",
                "--thread",
                thread,
                "--snapshot",
                snap,
                "--to",
                "destino",
                "--expect",
                expect,
            ])
            .env("AERIL_DATA_DIR", &env.data)
            .current_dir(&env.root)
            .output()
            .unwrap()
    }
    let barrier = std::sync::Arc::new(Barrier::new(2));
    let env_ref: &TestEnv = &env;
    let expect_ref: &str = &head0;
    let mut results: Vec<(String, Output)> = std::thread::scope(|scope| {
        let h1 = {
            let barrier = barrier.clone();
            let (thread, snap) = (a.clone(), ca[0].0.clone());
            scope.spawn(move || {
                barrier.wait();
                let out = integrate_once(env_ref, &thread, &snap, expect_ref);
                (thread, out)
            })
        };
        let h2 = {
            let barrier = barrier.clone();
            let (thread, snap) = (b.clone(), cb[0].0.clone());
            scope.spawn(move || {
                barrier.wait();
                let out = integrate_once(env_ref, &thread, &snap, expect_ref);
                (thread, out)
            })
        };
        vec![h1.join().unwrap(), h2.join().unwrap()]
    });
    results.sort_by_key(|(_, o)| !o.status.success()); // ganador primero

    let (winner_thread, winner_out) = &results[0];
    let (loser_thread, loser_out) = &results[1];
    assert!(winner_out.status.success(), "un intento debe ganar");
    assert!(!loser_out.status.success(), "el otro debe perder");
    let wv = parse_json(&winner_out.stdout);
    let winner_oid = wv["target_oid_after"].as_str().unwrap().to_string();
    let expected_winner = if *winner_thread == a {
        ca[0].1.clone()
    } else {
        cb[0].1.clone()
    };
    assert_eq!(winner_oid, expected_winner, "gana el OID de su snapshot");
    // La ref destino quedó en el OID ganador.
    assert_eq!(git_out(&env.repo, &["rev-parse", "destino"]), winner_oid);
    // El perdedor reporta conflicto explícito (stale) sin pisar al ganador.
    let lerr = String::from_utf8_lossy(&loser_out.stderr).to_string();
    assert!(lerr.contains("se movió"), "perdedor explícito: {lerr}");
    let blocked = event_payloads(&env, loser_thread, "integration.blocked");
    assert!(!blocked.is_empty(), "bloqueo durable del perdedor");
    // Historia durable: exactamente UNA integración exitosa, aun tras repetir
    // la reconciliación.
    assert_eq!(count_events_all(&env, "integration.completed"), 1);
    run_recover(&env);
    run_recover(&env);
    assert_eq!(
        count_events_all(&env, "integration.completed"),
        1,
        "recover no fabrica un segundo éxito"
    );
    assert_eq!(git_out(&env.repo, &["rev-parse", "destino"]), winner_oid);
}
