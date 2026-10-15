//! S3 — TUI supervisando DOS runs mock concurrentes.
//!
//! Gates T1–T9 del handoff S3, ejercitando el flujo real de usuario: se
//! conduce la TUI por teclas a través de un PTY (`common::pty`) y se verifica
//! el estado durable (runs/leases/checkpoints/eventos) y las garantías de
//! ciclo de vida (capacidad, cancelación independiente, salida segura).
//!
//! Restricción de producto: Hermes real sigue siendo 1-run-por-vez (T9).

mod common;
use common::pty::TuiPty;
use common::*;
use serde_json::Value;
use std::time::{Duration, Instant};

// ── helpers de estado durable ────────────────────────────────────────────────

fn sleep_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

#[derive(Debug, Clone)]
struct RunRow {
    id: String,
    state: String,
    backend: String,
    ws: String,
    wt: String,
    started_ms: i64,
    ended_ms: Option<i64>,
}

fn db(env: &TestEnv) -> aeril::store::Store {
    aeril::store::Store::open(&env.data.join("aeril.db")).unwrap()
}

fn runs_of(env: &TestEnv, thread_id: &str) -> Vec<RunRow> {
    let store = db(env);
    let mut stmt = store
        .conn
        .prepare(
            "SELECT id, state, backend, COALESCE(workspace_id,''), COALESCE(worktree_path,''),
                    started_ms, ended_ms
             FROM runs WHERE thread_id = ?1 ORDER BY started_ms ASC, id ASC",
        )
        .unwrap();
    let rows = stmt
        .query_map([thread_id], |r| {
            Ok(RunRow {
                id: r.get(0)?,
                state: r.get(1)?,
                backend: r.get(2)?,
                ws: r.get(3)?,
                wt: r.get(4)?,
                started_ms: r.get(5)?,
                ended_ms: r.get(6)?,
            })
        })
        .unwrap();
    rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
}

fn leases_of(env: &TestEnv) -> Vec<(String, String, i64)> {
    let store = db(env);
    let mut stmt = store
        .conn
        .prepare("SELECT workspace_id, run_id, pid FROM leases")
        .unwrap();
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap();
    rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
}

fn event_count(env: &TestEnv, thread_id: &str, kind: &str) -> i64 {
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

fn active_runs_cli(env: &TestEnv) -> Vec<Value> {
    env.aeril_ok(&["run", "list", "--active"])
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn thread_order_cli(env: &TestEnv) -> Vec<String> {
    env.aeril_ok(&["thread", "list"])
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["thread_id"].as_str().unwrap().to_string())
        .collect()
}

fn script_delay(delay_ms: u64, content: &str) -> Value {
    serde_json::json!({
        "message": "trabajando",
        "final_message": "listo",
        "delay_ms": delay_ms,
        "edits": [{"op": "write", "path": "NOTES.md", "content": content}],
    })
}

// ── driver de la TUI ─────────────────────────────────────────────────────────

struct Tui<'a> {
    env: &'a TestEnv,
    pty: TuiPty,
    /// Thread que se cree seleccionado en la lista (la TUI preserva la
    /// selección por identidad al recargar; el driver la modela igual).
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

    fn sync_sel(&mut self) {
        if self.sel.is_none() {
            self.sel = thread_order_cli(self.env).first().cloned();
        }
    }

    /// Mueve la selección de la lista al thread `target` (asume vista de lista).
    /// Primero refresca (`r`): la lista de la TUI se reordena cuando los runs
    /// actualizan `updated_ms`, y el orden que ve el usuario es el de la TUI,
    /// no el de la CLI.
    fn navigate(&mut self, target: &str) {
        self.pty.send(b"r");
        self.pty.pump(200);
        self.pty.esc();
        self.pty.pump(120);
        self.sync_sel();
        let order = thread_order_cli(self.env);
        let cur = self.sel.clone().unwrap();
        let ic = order
            .iter()
            .position(|t| *t == cur)
            .expect("selección actual presente en la lista");
        let it = order
            .iter()
            .position(|t| *t == target)
            .expect("thread objetivo presente en la lista");
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

    /// Flujo real de "nuevo run": abrir el thread, `n`, escribir objetivo, Enter.
    fn start_task(&mut self, target: &str, prompt: &str) {
        self.open(target);
        self.pty.send(b"n");
        self.pty.pump(120);
        self.pty.type_line(prompt);
        self.pty.pump(150);
    }

    /// `x` sobre el thread seleccionado (pide cancelación; el slot sigue ocupado).
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
            event_count(env, thread_id, "engine.session") >= 1
        })
    }
}

/// Crea threads con separación temporal para que el orden de la lista
/// (`updated_ms DESC`) sea determinista: el ÚLTIMO creado es el primero.
fn thread_new_spaced(env: &TestEnv, title: &str) -> String {
    sleep_ms(30);
    env.thread_new(title)
}

// ── T1: dos runs concurrentes con intervalos solapados ──────────────────────

#[test]
fn t1_dos_runs_concurrentes_solapan() {
    let env = TestEnv::new("s3-t1");
    // Orden [A, B]: A es el primero de la lista (selección inicial).
    let b = thread_new_spaced(&env, "t1-b");
    let a = thread_new_spaced(&env, "t1-a");
    let script = env.script("t1", script_delay(5_000, "# t1\n"));
    let backend = format!("mock:{}", script.display());

    let mut tui = Tui::launch(&env, &backend, &[]);
    tui.start_task(&a, "tarea A");
    assert!(tui.wait_run_active(&a, 15_000), "A no quedó activo");
    tui.start_task(&b, "tarea B");
    assert!(tui.wait_run_active(&b, 15_000), "B no quedó activo");
    assert!(tui.wait_session(&a, 15_000), "A sin sesión");
    assert!(tui.wait_session(&b, 15_000), "B sin sesión");

    // Ambos ACTIVOS a la vez, vistos por OTRO proceso (lectura S2).
    let active = active_runs_cli(&env);
    assert_eq!(active.len(), 2, "activos concurrentes: {active:?}");
    let ids: Vec<&str> = active
        .iter()
        .map(|r| r["run_id"].as_str().unwrap())
        .collect();
    let wss: Vec<&str> = active
        .iter()
        .map(|r| r["workspace_id"].as_str().unwrap())
        .collect();
    assert_ne!(ids[0], ids[1], "run_ids distintos");
    assert_ne!(wss[0], wss[1], "workspaces distintos");
    for r in &active {
        assert_eq!(r["status"], "running");
        assert_eq!(r["supervisor"]["status"], "alive");
        assert_eq!(r["lease"]["status"], "owned");
    }

    // Leases DURANTE la ejecución: uno por workspace, de su propio run.
    let leases = leases_of(&env);
    assert_eq!(leases.len(), 2, "leases activos: {leases:?}");
    let mut lease_ws: Vec<String> = leases.iter().map(|(ws, _, _)| ws.clone()).collect();
    lease_ws.sort();
    let mut want_ws: Vec<String> = wss.iter().map(|s| s.to_string()).collect();
    want_ws.sort();
    assert_eq!(lease_ws, want_ws);

    // Esperar terminales y verificar SOLAPAMIENTO por intervalos durables.
    assert!(tui.wait_run_terminal(&a, 25_000), "A no terminó");
    assert!(tui.wait_run_terminal(&b, 25_000), "B no terminó");
    let ra = runs_of(&env, &a).remove(0);
    let rb = runs_of(&env, &b).remove(0);
    assert_eq!(ra.state, "completed");
    assert_eq!(rb.state, "completed");
    let (a0, a1) = (ra.started_ms, ra.ended_ms.expect("A ended_ms"));
    let (b0, b1) = (rb.started_ms, rb.ended_ms.expect("B ended_ms"));
    assert!(
        b0 < a1 && a0 < b1,
        "intervalos NO solapados: A[{a0},{a1}] B[{b0},{b1}]"
    );
    assert_ne!(ra.id, rb.id);
    assert_ne!(ra.ws, rb.ws);
    assert_ne!(ra.wt, rb.wt, "worktrees distintos");

    // Checkpoints independientes por thread.
    let ca = env.ready_snapshots(&a);
    let cb = env.ready_snapshots(&b);
    assert_eq!(ca.len(), 1, "A debe tener 1 checkpoint ready");
    assert_eq!(cb.len(), 1, "B debe tener 1 checkpoint ready");
    assert_ne!(ca[0].0, cb[0].0, "snapshots distintos");
}

// ── T2: capacidad máxima con dos activos ────────────────────────────────────

#[test]
fn t2_capacidad_maxima_rechaza_tercero() {
    let env = TestEnv::new("s3-t2");
    let c = thread_new_spaced(&env, "t2-c");
    let b = thread_new_spaced(&env, "t2-b");
    let a = thread_new_spaced(&env, "t2-a");
    let script = env.script("t2", script_delay(12_000, "# t2\n"));
    let backend = format!("mock:{}", script.display());

    let mut tui = Tui::launch(&env, &backend, &[]);
    tui.start_task(&a, "A");
    assert!(tui.wait_run_active(&a, 15_000));
    tui.start_task(&b, "B");
    assert!(tui.wait_run_active(&b, 15_000));
    assert!(tui.wait_session(&a, 15_000));
    assert!(tui.wait_session(&b, 15_000));
    assert_eq!(active_runs_cli(&env).len(), 2);
    tui.settle(600); // ventana quieta: sin transiciones durante la navegación

    // Tercer run en C: rechazado por capacidad, sin crear run.
    tui.start_task(&c, "C");
    tui.settle(300);
    assert!(
        runs_of(&env, &c).is_empty(),
        "un tercer run no debe crearse"
    );
    assert!(tui.pty.alive(), "la TUI sigue viva");
    assert!(
        tui.pty.output_contains("capacidad máxima"),
        "mensaje de capacidad visible: {}",
        tui.pty.output_lossy()
    );
    // La capacidad sigue ocupada por A y B (no se liberó por el rechazo).
    assert_eq!(runs_of(&env, &a).len(), 1);
    assert_eq!(runs_of(&env, &b).len(), 1);

    // Limpieza: cancelar A y B y esperar ack.
    tui.cancel(&a);
    assert!(
        tui.wait_run_terminal(&a, 15_000),
        "A no terminó tras cancelar"
    );
    tui.cancel(&b);
    assert!(
        tui.wait_run_terminal(&b, 15_000),
        "B no terminó tras cancelar"
    );
    assert_eq!(
        runs_of(&env, &a).last().unwrap().state,
        "cancelled",
        "A cancelado"
    );
    assert_eq!(
        runs_of(&env, &b).last().unwrap().state,
        "cancelled",
        "B cancelado"
    );
}

// ── T3: un run por thread (rechazo de duplicado) ────────────────────────────

#[test]
fn t3_un_run_por_thread() {
    let env = TestEnv::new("s3-t3");
    let b = thread_new_spaced(&env, "t3-b");
    let a = thread_new_spaced(&env, "t3-a");
    let script = env.script("t3", script_delay(12_000, "# t3\n"));
    let backend = format!("mock:{}", script.display());

    let mut tui = Tui::launch(&env, &backend, &[]);
    tui.start_task(&a, "A1");
    assert!(tui.wait_run_active(&a, 15_000));
    tui.settle(300);

    // Segundo run en A mientras A está activo: rechazado, sin crear run.
    tui.start_task(&a, "A2");
    tui.settle(300);
    assert_eq!(runs_of(&env, &a).len(), 1, "no debe duplicarse el run");
    assert!(
        tui.pty.output_contains("ya tiene un run"),
        "motivo visible: {}",
        tui.pty.output_lossy()
    );
    assert!(tui.pty.alive());

    // Limpieza.
    tui.cancel(&a);
    assert!(tui.wait_run_terminal(&a, 15_000));
    let _ = b;
}

// ── T4: cancelación independiente (A se cancela, B completa) ────────────────

#[test]
fn t4_cancelacion_independiente() {
    let env = TestEnv::new("s3-t4");
    let b = thread_new_spaced(&env, "t4-b");
    let a = thread_new_spaced(&env, "t4-a");
    let script = env.script("t4", script_delay(8_000, "# t4\n"));
    let backend = format!("mock:{}", script.display());

    let mut tui = Tui::launch(&env, &backend, &[]);
    tui.start_task(&a, "A");
    assert!(tui.wait_run_active(&a, 15_000));
    tui.start_task(&b, "B");
    assert!(tui.wait_run_active(&b, 15_000));
    assert!(tui.wait_session(&a, 15_000));
    assert!(tui.wait_session(&b, 15_000));
    tui.settle(400);

    // Cancelar A: debe terminar en cancelled, sin checkpoint del turno.
    tui.cancel(&a);
    assert!(
        tui.wait_run_terminal(&a, 20_000),
        "A no terminó tras cancelar"
    );
    assert_eq!(runs_of(&env, &a).last().unwrap().state, "cancelled");
    assert!(
        env.ready_snapshots(&a).is_empty(),
        "turno cancelado no publica checkpoint"
    );

    // B NO fue interrumpido: sigue activo y completa con checkpoint.
    assert!(
        active_runs_cli(&env).iter().any(|r| r["thread_id"] == b),
        "B debe seguir activo tras cancelar A"
    );
    assert!(tui.wait_run_terminal(&b, 25_000), "B no completó");
    assert_eq!(runs_of(&env, &b).last().unwrap().state, "completed");
    assert_eq!(env.ready_snapshots(&b).len(), 1, "B publica su checkpoint");
    assert_eq!(event_count(&env, &a, "checkpoint.ready"), 0);
    assert!(tui.pty.alive());
}

// ── T5: carrera de cancelación — el slot no se libera antes del ack ─────────

#[test]
fn t5_slot_no_se_libera_antes_del_ack() {
    let env = TestEnv::new("s3-t5");
    let a = thread_new_spaced(&env, "t5-a");
    // Backend que NO coopera: la cancelación se resuelve por plazo (grace 3s),
    // dejando una ventana determinista para probar el gate.
    let script = env.script(
        "t5",
        serde_json::json!({
            "message": "trabajando",
            "delay_ms": 30_000,
            "edits": [],
            "ignore_cancel": true,
        }),
    );
    let backend = format!("mock:{}", script.display());

    // Plazo amplio: bajo carga (suite completa en paralelo) el intento
    // inmediato debe seguir cayendo DENTRO de la ventana de cancelación.
    let mut tui = Tui::launch(&env, &backend, &[("AERIL_CANCEL_GRACE_SECS", "8")]);
    tui.start_task(&a, "A");
    assert!(tui.wait_run_active(&a, 15_000));
    tui.settle(300);

    tui.cancel(&a);
    // Inmediatamente: reemplazo en A ⇒ rechazado (slot en cancelación).
    tui.start_task(&a, "A-reemplazo");
    tui.settle(300);
    assert_eq!(
        runs_of(&env, &a).len(),
        1,
        "no debe existir un run de reemplazo antes del ack"
    );
    assert!(
        tui.pty.output_contains("ya tiene un run cancelando"),
        "motivo visible: {}",
        tui.pty.output_lossy()
    );

    // El plazo mata al backend terco: ack terminal (cancelled).
    assert!(
        tui.wait_run_terminal(&a, 20_000),
        "A no terminó por plazo de cancelación"
    );
    assert_eq!(runs_of(&env, &a).last().unwrap().state, "cancelled");

    // Tras el ack, el slot se libera: el reemplazo ahora SÍ se acepta. El ack
    // llega cuando la TUI procesa la nota terminal (puede demorar bajo carga),
    // así que se reintenta con límite; cada intento rechazado no crea run.
    let mut a2_ok = false;
    for _ in 0..20 {
        tui.start_task(&a, "A2");
        if tui.wait_db(1_000, || runs_of(&env, &a).len() == 2) {
            a2_ok = true;
            break;
        }
    }
    assert!(a2_ok, "A2 debía arrancar tras el ack terminal");
    assert!(tui.wait_run_active(&a, 15_000), "A2 debía quedar activo");
    assert_eq!(runs_of(&env, &a).len(), 2, "A2 crea un segundo run");

    // Limpieza: cancelar A2 (por plazo otra vez) y esperar.
    tui.cancel(&a);
    assert!(tui.wait_run_terminal(&a, 20_000));
    assert_eq!(runs_of(&env, &a).last().unwrap().state, "cancelled");
}

// ── T6: aislamiento de eventos entre threads (éxito/error/tardío) ───────────

#[test]
fn t6_aislamiento_de_eventos_entre_threads() {
    let env = TestEnv::new("s3-t6");
    // D: thread sobre un repo que se elimina (fuerza error temprano del Runner).
    let repo2 = env.root.join("repo-d");
    std::fs::create_dir_all(&repo2).unwrap();
    common::git(&repo2, &["init", "-q", "-b", "main"]);
    common::git(&repo2, &["config", "user.email", "test@aeril.local"]);
    common::git(&repo2, &["config", "user.name", "Aeril Test"]);
    std::fs::write(repo2.join("README.md"), "# repo-d\n").unwrap();
    common::git(&repo2, &["add", "-A"]);
    common::git(&repo2, &["commit", "-q", "-m", "base"]);
    let d = env.aeril_ok(&[
        "thread",
        "new",
        "--repo",
        repo2.to_str().unwrap(),
        "--title",
        "t6-d",
    ])["thread_id"]
        .as_str()
        .unwrap()
        .to_string();
    std::fs::remove_dir_all(&repo2).unwrap();

    let b = thread_new_spaced(&env, "t6-b");
    let a = thread_new_spaced(&env, "t6-a");
    let script = env.script("t6", script_delay(12_000, "# t6\n"));
    let backend = format!("mock:{}", script.display());

    let mut tui = Tui::launch(&env, &backend, &[]);
    tui.start_task(&a, "A");
    assert!(tui.wait_run_active(&a, 15_000));
    tui.start_task(&b, "B");
    assert!(tui.wait_run_active(&b, 15_000));
    assert!(tui.wait_session(&a, 15_000));
    assert!(tui.wait_session(&b, 15_000));
    tui.settle(500);

    // Terminal de A (cancelación) mientras B sigue vivo.
    tui.cancel(&a);
    assert!(tui.wait_run_terminal(&a, 20_000));
    assert_eq!(runs_of(&env, &a).last().unwrap().state, "cancelled");

    // El ack/imprevisto de A NO tocó el slot de B: B sigue activo y un
    // segundo run en B sigue rechazado.
    assert!(
        active_runs_cli(&env).iter().any(|r| r["thread_id"] == b),
        "B debe seguir activo"
    );
    tui.start_task(&b, "B2");
    tui.settle(300);
    assert_eq!(runs_of(&env, &b).len(), 1, "B no admite un segundo run");
    assert!(
        tui.pty.output_contains("ya tiene un run"),
        "rechazo de B visible"
    );

    // Error de D (repo inexistente): nota de error atribuida a D, sin run.
    tui.settle(400);
    tui.start_task(&d, "D");
    assert!(
        tui.wait_db(10_000, || {
            let env = &env;
            runs_of(env, &d).is_empty()
        }),
        "D no debe crear runs"
    );
    tui.settle(300);
    assert!(
        tui.pty.output_contains("error:"),
        "el error de D debe verse en la TUI: {}",
        tui.pty.output_lossy()
    );
    assert!(tui.pty.alive(), "la TUI sobrevive al error de D");
    // El error de D no liberó slots ajenos: B sigue rechazando duplicados.
    tui.start_task(&b, "B3");
    tui.settle(300);
    assert_eq!(
        runs_of(&env, &b).len(),
        1,
        "slot de B intacto tras el error de D"
    );

    // El slot de A sí quedó libre exactamente una vez: A2 arranca con B activo.
    tui.start_task(&a, "A2");
    assert!(tui.wait_run_active(&a, 15_000), "A2 debía arrancar");
    assert_eq!(runs_of(&env, &a).len(), 2);

    // Sin contaminación de timelines: B no tiene run.ended ni checkpoints de A.
    assert_eq!(
        event_count(&env, &b, "run.ended"),
        0,
        "B sigue sin terminar"
    );
    assert_eq!(event_count(&env, &b, "checkpoint.ready"), 0);
    assert_eq!(event_count(&env, &a, "checkpoint.ready"), 0, "A cancelado");

    // Limpieza: cancelar A2 y B.
    tui.cancel(&a);
    assert!(tui.wait_run_terminal(&a, 20_000));
    tui.cancel(&b);
    assert!(tui.wait_run_terminal(&b, 20_000));
    assert_eq!(runs_of(&env, &b).last().unwrap().state, "cancelled");
}

// ── T7: navegación y revisión de ambos checkpoints ──────────────────────────

#[test]
fn t7_navegacion_y_revision_de_snapshots() {
    let env = TestEnv::new("s3-t7");
    let b = thread_new_spaced(&env, "t7-b");
    let a = thread_new_spaced(&env, "t7-a");
    let script = env.script("t7", script_delay(5_000, "# t7 revisado\n"));
    let backend = format!("mock:{}", script.display());

    let mut tui = Tui::launch(&env, &backend, &[]);
    tui.start_task(&a, "A");
    assert!(tui.wait_run_active(&a, 15_000));
    tui.start_task(&b, "B");
    assert!(tui.wait_run_active(&b, 15_000));
    assert!(tui.wait_session(&a, 15_000));
    assert!(tui.wait_session(&b, 15_000));
    tui.settle(500);

    // Navegar A → B → A mientras ambos corren: sin cambios de estado.
    tui.open(&a);
    tui.settle(250);
    tui.open(&b);
    tui.settle(250);
    tui.open(&a);
    tui.settle(250);
    assert!(tui.pty.alive(), "la TUI sigue viva tras navegar");
    assert_eq!(runs_of(&env, &a)[0].state, "running", "navegar no muta A");
    assert_eq!(runs_of(&env, &b)[0].state, "running", "navegar no muta B");

    assert!(tui.wait_run_terminal(&a, 25_000));
    assert!(tui.wait_run_terminal(&b, 25_000));
    let head_before = env.head_oid();

    // Revisar cada snapshot de forma independiente (sin tocar workspaces).
    let ca = env.ready_snapshots(&a);
    let cb = env.ready_snapshots(&b);
    assert_eq!(ca.len(), 1);
    assert_eq!(cb.len(), 1);
    assert_ne!(ca[0].0, cb[0].0, "snapshots independientes");
    assert_ne!(ca[0].1, cb[0].1, "oids independientes");
    for (thread, snap) in [(&a, &ca[0].0), (&b, &cb[0].0)] {
        let show = env.aeril_ok(&["checkpoint", "show", "--thread", thread, "--snapshot", snap]);
        let patch = show["patch"].as_str().unwrap_or_default();
        assert!(
            patch.contains("# t7 revisado"),
            "el patch del snapshot debe ser revisable: {patch}"
        );
    }
    // La revisión es de solo lectura: HEAD del repo intacto.
    assert_eq!(env.head_oid(), head_before);
    assert!(tui.pty.alive());
}

// ── T8: salida segura (q bloqueado con runs vivos) ──────────────────────────

#[test]
fn t8_salida_segura_con_runs_vivos() {
    let env = TestEnv::new("s3-t8");
    let b = thread_new_spaced(&env, "t8-b");
    let a = thread_new_spaced(&env, "t8-a");
    let script = env.script("t8", script_delay(12_000, "# t8\n"));
    let backend = format!("mock:{}", script.display());

    let mut tui = Tui::launch(&env, &backend, &[]);
    tui.start_task(&a, "A");
    assert!(tui.wait_run_active(&a, 15_000));
    tui.start_task(&b, "B");
    assert!(tui.wait_run_active(&b, 15_000));
    tui.settle(400);

    // `q` con runs vivos: rechazado con mensaje, proceso sigue vivo.
    tui.pty.send(b"q");
    tui.pty.pump(600);
    assert!(tui.pty.alive(), "q NO debe salir con runs vivos");
    assert!(
        tui.pty.output_contains("salida bloqueada"),
        "mensaje de salida bloqueada: {}",
        tui.pty.output_lossy()
    );

    // Cancelar ambos; tras los acks, la salida normal funciona.
    tui.cancel(&a);
    assert!(tui.wait_run_terminal(&a, 20_000));
    tui.cancel(&b);
    assert!(tui.wait_run_terminal(&b, 20_000));
    assert!(
        active_runs_cli(&env).is_empty(),
        "sin runs activos antes de salir"
    );

    let mut status = None;
    for _ in 0..20 {
        tui.pty.send(b"q");
        if let Some(st) = tui.pty.wait_exit(500) {
            status = Some(st);
            break;
        }
    }
    let status = status.expect("q debía salir tras los ack terminales");
    assert!(status.success(), "salida normal con código 0: {status}");
    assert_eq!(runs_of(&env, &a).last().unwrap().state, "cancelled");
    assert_eq!(runs_of(&env, &b).last().unwrap().state, "cancelled");
}

// ── T9: restricción Hermes real (1 run por vez) ─────────────────────────────

#[test]
fn t9_hermes_real_un_run_por_vez() {
    let env = TestEnv::new("s3-t9");
    let b = thread_new_spaced(&env, "t9-b");
    let a = thread_new_spaced(&env, "t9-a");

    // Shim determinista que ocupa el lugar del binario `hermes`: el backend
    // resuelve "hermes" por PATH, pero NUNCA se lanza Hermes real en tests.
    let hold = env.script("t9", script_delay(15_000, "# t9\n"));
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
    assert!(
        tui.wait_db(15_000, || {
            let env = &env;
            runs_of(env, &a).iter().any(|r| r.state == "running")
        }),
        "el primer run Hermes no arrancó"
    );
    // Es el camino Hermes (backend persistido), no mock.
    assert_eq!(runs_of(&env, &a)[0].backend, "hermes");

    // Segundo Hermes simultáneo: rechazado sin crear run.
    tui.start_task(&b, "Hermes B");
    tui.settle(300);
    assert!(
        runs_of(&env, &b).is_empty(),
        "no debe lanzarse un segundo Hermes concurrente"
    );
    assert!(
        tui.pty.output_contains("Hermes real: un run por vez"),
        "limitación visible: {}",
        tui.pty.output_lossy()
    );
    assert!(tui.pty.alive());

    // Limpieza.
    tui.cancel(&a);
    assert!(tui.wait_run_terminal(&a, 20_000));
    assert_eq!(runs_of(&env, &a).last().unwrap().state, "cancelled");
}
