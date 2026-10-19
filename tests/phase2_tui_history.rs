//! TUI — verificación de historias largas (aceptación TUI).
//!
//! Matriz de escenarios de historial largo contra la API pública del store:
//! 150 y 1.000 eventos en un run, dos runs secuenciales con checkpoint
//! verificado histórico, ventana reciente sin `run.started`, run activo sin
//! checkpoint, runs terminal (completed/failed) y selección de checkpoint
//! histórico durante un run más nuevo.
//!
//! Requisito: la verificación NO puede resolverse cargando el historial
//! completo en memoria. Por eso se asserta que la ventana de eventos es
//! acotada (100) y que el camino de refresh del TUI no materializa el
//! historial (proyección SQL + memoria estable).

use aeril::checkpoint;
use aeril::store::events::payload;
use aeril::store::{Store, events, runs, threads};
use serde_json::{Value, json};

const BASE_OID: &str = "0000000000000000000000000000000000000000";
const TAIL_LIMIT: i64 = 100;

fn store_with_thread(title: &str) -> (Store, threads::ThreadRow) {
    let mut store = Store::open_in_memory().expect("store en memoria");
    let thread = threads::create(&mut store.conn, title, "/tmp/repo", "r1").expect("thread");
    (store, thread)
}

fn start_run(store: &mut Store, thread_id: &str, id: &str, goal: &str, started_ms: i64) -> String {
    let ws = format!("ws-{id}");
    let wt = format!("/tmp/wt-{id}");
    let branch = format!("branch-{id}");
    let spec = runs::StartRunSpec {
        id: Some(id),
        thread_id,
        backend: "mock",
        backend_version: "test",
        goal,
        workspace_id: &ws,
        repo_path: "/tmp/repo",
        worktree_path: &wt,
        branch: &branch,
        base_oid: BASE_OID,
        resumed_session: None,
        profile: None,
    };
    let run = runs::start(&mut store.conn, &spec).expect("run creado");
    // Orden temporal determinista (now_ms() colapsa varios runs en el mismo ms).
    store
        .conn
        .execute(
            "UPDATE runs SET started_ms = ?2 WHERE id = ?1",
            rusqlite::params![id, started_ms],
        )
        .expect("started_ms del run");
    run.id
}

fn set_state(store: &mut Store, run_id: &str, state: &str) {
    store
        .conn
        .execute(
            "UPDATE runs SET state = ?2 WHERE id = ?1",
            rusqlite::params![run_id, state],
        )
        .expect("estado del run");
}

fn run_started_payload(goal: &str) -> Value {
    json!({
        "backend": "mock",
        "goal": goal,
        "workspace_id": "ws",
        "repo_path": "/tmp/repo",
        "worktree_path": "/tmp/wt",
        "branch": "branch",
        "base_oid": BASE_OID
    })
}

fn checkpoint_payload(snapshot_id: &str) -> Value {
    json!({
        "snapshot_id": snapshot_id,
        "git_ref": format!("refs/heads/{snapshot_id}"),
        "oid": BASE_OID,
        "base_oid": BASE_OID,
        "patch": "",
        "files": [],
        "recovered": false
    })
}

/// Agrega `n` eventos del kind indicado en una sola transacción.
fn append_n(
    store: &mut Store,
    thread_id: &str,
    run_id: &str,
    kind: &str,
    n: usize,
    payload: Value,
) {
    let tx = store
        .conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .expect("tx");
    for _ in 0..n {
        events::append_tx(&tx, thread_id, Some(run_id), kind, "system", &payload, 0)
            .expect("evento");
    }
    tx.commit().expect("commit");
}

fn append_one(store: &mut Store, thread_id: &str, run_id: &str, kind: &str, payload: Value) {
    let tx = store
        .conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .expect("tx");
    events::append_tx(&tx, thread_id, Some(run_id), kind, "system", &payload, 0).expect("evento");
    tx.commit().expect("commit");
}

fn rss_kb() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        }
    }
    0
}

// ── 1 y 2: 150 y 1.000 eventos en un run ────────────────────────────────────

fn one_run_with_many_events(total: usize) {
    let (mut store, thread) = store_with_thread("long");
    let run = start_run(&mut store, &thread.id, "run-a", "objetivo largo", 1);
    append_one(
        &mut store,
        &thread.id,
        &run,
        events::K_RUN_STARTED,
        run_started_payload("objetivo largo"),
    );
    append_one(
        &mut store,
        &thread.id,
        &run,
        events::K_ENGINE_SESSION,
        json!({}),
    );
    append_one(
        &mut store,
        &thread.id,
        &run,
        events::K_AGENT_TOOL_CALL,
        json!({}),
    );
    append_one(
        &mut store,
        &thread.id,
        &run,
        events::K_CHECKPOINT_READY,
        checkpoint_payload("snap-a"),
    );
    append_n(
        &mut store,
        &thread.id,
        &run,
        events::K_AGENT_MESSAGE,
        total - 4,
        json!({}),
    );
    set_state(&mut store, &run, "running");

    // La ventana del TUI es ACOTADA (nunca el historial completo).
    let tail = events::fetch_tail(&store.conn, &thread.id, TAIL_LIMIT).expect("tail");
    assert_eq!(
        tail.len(),
        TAIL_LIMIT as usize,
        "la ventana debe estar acotada"
    );
    assert!(
        !tail.iter().any(
            |ev| ev.kind == events::K_RUN_STARTED && ev.run_id.as_deref() == Some(run.as_str())
        ),
        "con {total} eventos, run.started queda fuera de la ventana"
    );

    // El progreso NO depende de la ventana: se proyecta en SQL.
    let progress = events::run_progress(&store.conn, &thread.id, &run).expect("progress");
    assert_eq!(progress.goal.as_deref(), Some("objetivo largo"));
    assert!(progress.has_session, "sesión de motor durable");
    assert!(progress.has_activity, "actividad del agente durable");
    assert!(progress.has_checkpoint, "checkpoint durable");
}

#[test]
fn one_run_with_150_events_keeps_progress_and_bounded_window() {
    one_run_with_many_events(150);
}

#[test]
fn one_run_with_1000_events_keeps_progress_and_bounded_window() {
    one_run_with_many_events(1000);
}

// ── 3: dos runs secuenciales con checkpoint verificado histórico ────────────

#[test]
fn two_sequential_runs_keep_historical_checkpoint_selection() {
    let (mut store, thread) = store_with_thread("two runs");
    let old = start_run(&mut store, &thread.id, "run-old", "objetivo viejo", 10);
    append_one(
        &mut store,
        &thread.id,
        &old,
        events::K_RUN_STARTED,
        run_started_payload("objetivo viejo"),
    );
    append_one(
        &mut store,
        &thread.id,
        &old,
        events::K_ENGINE_SESSION,
        json!({}),
    );
    append_one(
        &mut store,
        &thread.id,
        &old,
        events::K_AGENT_TOOL_CALL,
        json!({}),
    );
    append_one(
        &mut store,
        &thread.id,
        &old,
        events::K_CHECKPOINT_READY,
        checkpoint_payload("snap-old"),
    );
    set_state(&mut store, &old, "completed");

    let new = start_run(&mut store, &thread.id, "run-new", "objetivo nuevo", 20);
    append_one(
        &mut store,
        &thread.id,
        &new,
        events::K_RUN_STARTED,
        run_started_payload("objetivo nuevo"),
    );
    append_one(
        &mut store,
        &thread.id,
        &new,
        events::K_ENGINE_SESSION,
        json!({}),
    );
    append_one(
        &mut store,
        &thread.id,
        &new,
        events::K_AGENT_TOOL_CALL,
        json!({}),
    );
    set_state(&mut store, &new, "running");

    // Progreso aislado por run.
    let old_progress = events::run_progress(&store.conn, &thread.id, &old).expect("progress viejo");
    assert!(old_progress.has_checkpoint);
    let new_progress = events::run_progress(&store.conn, &thread.id, &new).expect("progress nuevo");
    assert_eq!(new_progress.goal.as_deref(), Some("objetivo nuevo"));
    assert!(new_progress.has_activity);
    assert!(
        !new_progress.has_checkpoint,
        "el run nuevo no tiene checkpoint propio"
    );

    // Selección HISTÓRICA del checkpoint verificado mientras corre el nuevo.
    let latest = checkpoint::latest_ready(&store, &thread.id).expect("latest ready");
    let (ev, ready): (events::EventRecord, payload::CheckpointReady) =
        latest.expect("debe haber un checkpoint verificado");
    assert_eq!(ready.snapshot_id, "snap-old");
    assert_eq!(ev.run_id.as_deref(), Some(old.as_str()));

    // Búsqueda por snapshot (proyección acotada, sin materializar historial).
    let by_snapshot =
        checkpoint::ready_for_snapshot(&store, &thread.id, "snap-old").expect("ready_for_snapshot");
    assert_eq!(by_snapshot.expect("snap-old").1.snapshot_id, "snap-old");
    assert!(
        checkpoint::ready_for_snapshot(&store, &thread.id, "snap-inexistente")
            .expect("ready_for_snapshot")
            .is_none(),
        "un snapshot inexistente no inventa checkpoint"
    );

    // El resumen de runs refleja el más nuevo.
    let summaries = threads::run_summaries(&store.conn, 10).expect("summaries");
    let summary = summaries.get(&thread.id).expect("resumen del thread");
    assert_eq!(summary.run_count, 2);
    assert_eq!(summary.latest_run_id.as_deref(), Some("run-new"));
    assert_eq!(summary.latest_state.as_deref(), Some("running"));
}

// ── 4: eventos recientes que ya no incluyen run.started ─────────────────────

#[test]
fn recent_window_without_run_started_keeps_durable_goal() {
    let (mut store, thread) = store_with_thread("window");
    let run = start_run(&mut store, &thread.id, "run-a", "objetivo durable", 1);
    append_one(
        &mut store,
        &thread.id,
        &run,
        events::K_RUN_STARTED,
        run_started_payload("objetivo durable"),
    );
    append_n(
        &mut store,
        &thread.id,
        &run,
        events::K_AGENT_MESSAGE,
        200,
        json!({}),
    );
    set_state(&mut store, &run, "running");

    let tail = events::fetch_tail(&store.conn, &thread.id, TAIL_LIMIT).expect("tail");
    assert_eq!(tail.len(), TAIL_LIMIT as usize);
    assert!(
        !tail.iter().any(|ev| ev.kind == events::K_RUN_STARTED),
        "run.started no debe estar en la ventana reciente"
    );
    // El goal sigue disponible: es un hecho durable, no de la ventana.
    let progress = events::run_progress(&store.conn, &thread.id, &run).expect("progress");
    assert_eq!(progress.goal.as_deref(), Some("objetivo durable"));
}

// ── 5: run activo sin checkpoint ────────────────────────────────────────────

#[test]
fn active_run_without_checkpoint_does_not_invent_one() {
    let (mut store, thread) = store_with_thread("sin checkpoint");
    let run = start_run(&mut store, &thread.id, "run-a", "sin checkpoint", 1);
    append_one(
        &mut store,
        &thread.id,
        &run,
        events::K_RUN_STARTED,
        run_started_payload("sin checkpoint"),
    );
    append_one(
        &mut store,
        &thread.id,
        &run,
        events::K_ENGINE_SESSION,
        json!({}),
    );
    set_state(&mut store, &run, "running");

    let progress = events::run_progress(&store.conn, &thread.id, &run).expect("progress");
    assert!(progress.has_session);
    assert!(!progress.has_activity);
    assert!(!progress.has_checkpoint, "no hay checkpoint que reportar");
    assert!(
        checkpoint::latest_ready(&store, &thread.id)
            .expect("latest ready")
            .is_none(),
        "sin checkpoints no se inventa ninguno"
    );
    let summaries = threads::run_summaries(&store.conn, 10).expect("summaries");
    assert_eq!(
        summaries
            .get(&thread.id)
            .expect("resumen")
            .latest_state
            .as_deref(),
        Some("running")
    );
}

// ── 6: run completado y run fallido ────────────────────────────────────────

#[test]
fn completed_and_failed_runs_expose_terminal_state() {
    let (mut store, thread) = store_with_thread("terminales");
    let done = start_run(&mut store, &thread.id, "run-ok", "completado", 10);
    append_one(
        &mut store,
        &thread.id,
        &done,
        events::K_RUN_STARTED,
        run_started_payload("completado"),
    );
    append_one(
        &mut store,
        &thread.id,
        &done,
        events::K_ENGINE_SESSION,
        json!({}),
    );
    append_one(
        &mut store,
        &thread.id,
        &done,
        events::K_CHECKPOINT_READY,
        checkpoint_payload("snap-ok"),
    );
    set_state(&mut store, &done, "completed");

    let bad = start_run(&mut store, &thread.id, "run-ko", "fallido", 20);
    append_one(
        &mut store,
        &thread.id,
        &bad,
        events::K_RUN_STARTED,
        run_started_payload("fallido"),
    );
    append_one(
        &mut store,
        &thread.id,
        &bad,
        events::K_ENGINE_SESSION,
        json!({}),
    );
    set_state(&mut store, &bad, "failed");

    // Cada run conserva sus propios hitos.
    let ok_progress = events::run_progress(&store.conn, &thread.id, &done).expect("progress ok");
    assert!(ok_progress.has_checkpoint);
    let ko_progress = events::run_progress(&store.conn, &thread.id, &bad).expect("progress ko");
    assert!(ko_progress.has_session);
    assert!(!ko_progress.has_checkpoint);

    // El resumen expone el estado terminal del más reciente.
    let summaries = threads::run_summaries(&store.conn, 10).expect("summaries");
    let summary = summaries.get(&thread.id).expect("resumen");
    assert_eq!(summary.latest_run_id.as_deref(), Some("run-ko"));
    assert_eq!(summary.latest_state.as_deref(), Some("failed"));

    // El checkpoint verificado sigue siendo el del run completado.
    let latest = checkpoint::latest_ready(&store, &thread.id).expect("latest ready");
    assert_eq!(latest.expect("checkpoint").1.snapshot_id, "snap-ok");
}

// ── 7: el refresh del TUI no materializa el historial completo ──────────────

#[test]
fn tui_refresh_path_does_not_materialize_full_history() {
    let (mut store, thread) = store_with_thread("memoria");
    let run = start_run(&mut store, &thread.id, "run-a", "memoria", 1);
    append_one(
        &mut store,
        &thread.id,
        &run,
        events::K_RUN_STARTED,
        run_started_payload("memoria"),
    );
    append_n(
        &mut store,
        &thread.id,
        &run,
        events::K_AGENT_MESSAGE,
        1000,
        json!({}),
    );
    // Patológico: muchos checkpoints (el camino que antes cargaba todos).
    append_n(
        &mut store,
        &thread.id,
        &run,
        events::K_CHECKPOINT_READY,
        500,
        checkpoint_payload("snap-x"),
    );
    set_state(&mut store, &run, "running");

    let before = rss_kb();
    // Camino de refresh del TUI: ventana acotada + proyección SQL + checkpoint.
    for _ in 0..20 {
        let tail = events::fetch_tail(&store.conn, &thread.id, TAIL_LIMIT).expect("tail");
        assert_eq!(tail.len(), TAIL_LIMIT as usize);
        let _ = events::run_progress(&store.conn, &thread.id, &run).expect("progress");
        let _ = checkpoint::latest_ready(&store, &thread.id).expect("latest ready");
        let _ = checkpoint::ready_for_snapshot(&store, &thread.id, "snap-x").expect("snapshot");
    }
    let after = rss_kb();
    let delta_mb = after.saturating_sub(before) as f64 / 1024.0;
    assert!(
        delta_mb < 24.0,
        "el refresh no debe materializar el historial (delta RSS {delta_mb:.1} MB con 1.500 eventos)"
    );
}
