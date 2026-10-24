//! PA0-B: proyección de Attention y próxima acción.
//!
//! Los tests sintéticos fijan la precedencia y las distinciones que no deben
//! confundirse: ready vs prepared recuperable, revisión vs "sin comentarios",
//! validación de otro OID, integración bloqueada vs terminada y eventos viejos.

mod common;

use aeril::attention::{self, AttentionStatus};
use aeril::integration::ValidationState;
use aeril::review;
use aeril::store::events;
use aeril::store::{Store, threads};
use common::pty::TuiPty;
use common::*;
use serde_json::json;
use std::time::{Duration, Instant};

const BASE: &str = "0000000000000000000000000000000000000000";
const OID: &str = "1111111111111111111111111111111111111111";
const OTHER_OID: &str = "2222222222222222222222222222222222222222";
const SNAP: &str = "snap-pa0";

fn repo_path() -> String {
    "/tmp/aeril-pa0-attention".into()
}

fn checkpoint_ref(snapshot: &str) -> String {
    format!("refs/aeril/checkpoints/{snapshot}")
}

fn new_thread(name: &str) -> (Store, String, String) {
    let mut store = Store::open_in_memory().expect("store");
    let thread = threads::create(&mut store.conn, name, &repo_path(), "r1").expect("thread");
    (store, thread.id, "run-1".into())
}

fn add_run_at(store: &mut Store, thread: &str, run: &str, state: &str, started_ms: i64) {
    store
        .conn
        .execute(
            "INSERT INTO runs
                (id, thread_id, backend, backend_version, state, started_ms, ended_ms)
             VALUES (?1, ?2, 'mock', '0', ?3, ?4, ?5)",
            rusqlite::params![
                run,
                thread,
                state,
                started_ms,
                matches!(state, "completed" | "failed" | "cancelled").then_some(started_ms + 1)
            ],
        )
        .expect("run");
}

fn add_run(store: &mut Store, thread: &str, run: &str, state: &str) {
    add_run_at(store, thread, run, state, 1);
}

fn set_run_state(store: &Store, run: &str, state: &str) {
    store
        .conn
        .execute(
            "UPDATE runs SET state = ?2, ended_ms = 2 WHERE id = ?1",
            rusqlite::params![run, state],
        )
        .expect("update run");
}

fn append(
    store: &mut Store,
    thread: &str,
    run: Option<&str>,
    kind: &str,
    payload: serde_json::Value,
) -> String {
    events::append(&mut store.conn, thread, run, kind, "system", &payload)
        .expect("event")
        .0
}

fn add_prepared(store: &mut Store, thread: &str, run: &str, snapshot: &str, oid: &str) -> String {
    append(
        store,
        thread,
        Some(run),
        events::K_CHECKPOINT_PREPARED,
        json!({
            "snapshot_id": snapshot,
            "git_ref": checkpoint_ref(snapshot),
            "expected_oid": oid,
            "run_id": run,
            "base_oid": BASE,
            "parent_oid": BASE,
            "files": [{"path": "NOTES.md", "status": "modified", "additions": 1, "deletions": 0}]
        }),
    )
}

fn add_ready(store: &mut Store, thread: &str, run: &str, snapshot: &str, oid: &str) -> String {
    append(
        store,
        thread,
        Some(run),
        events::K_CHECKPOINT_READY,
        json!({
            "snapshot_id": snapshot,
            "git_ref": checkpoint_ref(snapshot),
            "oid": oid,
            "base_oid": BASE,
            "patch": "--- a/NOTES.md\n+++ b/NOTES.md\n",
            "files": [{"path": "NOTES.md", "status": "modified", "additions": 1, "deletions": 0}],
            "recovered": false
        }),
    )
}

fn add_checkpoint(store: &mut Store, thread: &str, run: &str, snapshot: &str, oid: &str) -> String {
    add_prepared(store, thread, run, snapshot, oid);
    add_ready(store, thread, run, snapshot, oid)
}

fn add_comment(store: &mut Store, thread: &str, run: &str, ready_id: &str, text: &str) -> String {
    append(
        store,
        thread,
        Some(run),
        events::K_COMMENT_ADDED,
        json!({
            "checkpoint_event_id": ready_id,
            "oid": OID,
            "anchor": {
                "snapshot_id": SNAP,
                "path": "NOTES.md",
                "side": "new",
                "start_line": 1,
                "end_line": 1,
                "context_hash": "hash"
            },
            "text": text
        }),
    )
}

fn add_validation(
    store: &mut Store,
    thread: &str,
    run: &str,
    snapshot: &str,
    oid: &str,
    exit_code: i32,
    tree_ok: bool,
) -> String {
    append(
        store,
        thread,
        Some(run),
        events::K_VALIDATION_RECORDED,
        json!({
            "snapshot_id": snapshot,
            "oid": oid,
            "command": "true",
            "exit_code": exit_code,
            "duration_ms": 1,
            "output_hash": "fnv",
            "output_tail": "",
            "tree_before_ok": tree_ok,
            "tree_after_unchanged": tree_ok
        }),
    )
}

fn add_integration(
    store: &mut Store,
    thread: &str,
    kind: &str,
    source_oid: &str,
    reason: &str,
) -> String {
    let payload = match kind {
        events::K_INTEGRATION_REQUESTED => json!({
            "target_branch": "main",
            "source_oid": source_oid,
            "target_oid_expected": BASE
        }),
        events::K_INTEGRATION_COMPLETED => json!({
            "target_branch": "main",
            "source_oid": source_oid,
            "target_oid_before": BASE,
            "target_oid_after": source_oid,
            "recovered": false
        }),
        events::K_INTEGRATION_BLOCKED => json!({
            "target_branch": "main",
            "source_oid": source_oid,
            "target_oid_current": BASE,
            "reason": reason
        }),
        other => panic!("kind de integración inesperado: {other}"),
    };
    append(store, thread, None, kind, payload)
}

fn project(store: &Store, thread: &str) -> attention::TaskAttention {
    attention::project_thread(store, thread).expect("proyección")
}

fn ready_with_review(store: &mut Store, thread: &str, run: &str) -> String {
    let ready = add_checkpoint(store, thread, run, SNAP, OID);
    review::acknowledge(store, thread, SNAP, OID).expect("ack");
    ready
}

// ── estados base ────────────────────────────────────────────────────────────

#[test]
fn no_run_needs_a_run() {
    let (store, thread, _) = new_thread("sin run");
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::NoRun);
    assert_eq!(a.next_action, "iniciar run");
    assert!(a.run_state.is_none());
    assert!(!a.is_actionable());
}

#[test]
fn active_run_is_in_progress_not_actionable() {
    let (mut store, thread, run) = new_thread("en curso");
    add_run(&mut store, &thread, &run, "running");
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::InProgress);
    assert_eq!(a.run_state.as_deref(), Some("running"));
    assert_eq!(a.next_action, "observar progreso");
    assert!(!a.is_actionable());
}

#[test]
fn active_run_without_live_supervisor_requires_recovery() {
    let (mut store, thread, run) = new_thread("supervisor muerto");
    add_run(&mut store, &thread, &run, "running");
    store
        .conn
        .execute(
            "UPDATE runs SET workspace_id = 'ws-pa0' WHERE id = ?1",
            rusqlite::params![run],
        )
        .expect("workspace");
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::RecoveryRequired);
    assert_eq!(a.next_action, "aeril recover");
    assert!(a.reason.contains("supervisor"));
}

#[test]
fn failed_run_keeps_the_failure_visible() {
    let (mut store, thread, run) = new_thread("fallido");
    add_run(&mut store, &thread, &run, "failed");
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::Failed);
    assert_eq!(a.next_action, "revisar fallo");
}

// ── review: "sin comentarios" no significa revisado ─────────────────────────

#[test]
fn ready_without_acknowledgement_requires_review() {
    let (mut store, thread, run) = new_thread("sin ack");
    add_run(&mut store, &thread, &run, "completed");
    add_checkpoint(&mut store, &thread, &run, SNAP, OID);
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::ReviewRequired);
    assert_eq!(a.review.as_deref(), Some("required"));
    assert_eq!(a.next_action, "marcar revisado");
    assert_eq!(a.snapshot_id.as_deref(), Some(SNAP));
    assert_eq!(a.source_oid.as_deref(), Some(OID));
    assert_eq!(a.validation, Some(ValidationState::Missing));
}

#[test]
fn acknowledgement_is_exact_to_snapshot_and_oid() {
    let (mut store, thread, run) = new_thread("ack exacto");
    add_run(&mut store, &thread, &run, "completed");
    add_checkpoint(&mut store, &thread, &run, SNAP, OID);
    assert!(
        review::acknowledge(&mut store, &thread, SNAP, OTHER_OID).is_err(),
        "no se puede ack un OID que no es el del checkpoint listo"
    );
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::ReviewRequired);
    assert_eq!(a.review.as_deref(), Some("required"));
    review::acknowledge(&mut store, &thread, SNAP, OID).expect("ack exacto");
    assert!(
        !review::acknowledged(&store, &thread, SNAP, OTHER_OID).expect("consulta"),
        "el acknowledgement no debe satisfacer otro OID"
    );
    let a = project(&store, &thread);
    assert_eq!(a.review.as_deref(), Some("clear"));
    assert_eq!(a.status, AttentionStatus::ValidationRequired);
}

#[test]
fn open_comments_block_ack_and_keep_review_pending() {
    let (mut store, thread, run) = new_thread("comentarios");
    add_run(&mut store, &thread, &run, "completed");
    let ready = add_checkpoint(&mut store, &thread, &run, SNAP, OID);
    let comment = add_comment(&mut store, &thread, &run, &ready, "revisar esto");
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::ReviewRequired);
    assert_eq!(a.review.as_deref(), Some("pending"));
    assert_eq!(a.next_action, "resolver comentarios");
    assert!(a.reason.contains('1'));
    assert!(
        review::acknowledge(&mut store, &thread, SNAP, OID).is_err(),
        "no se puede marcar revisado con comentarios abiertos"
    );
    review::resolve(&mut store, &thread, &comment).expect("resolve");
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::ReviewRequired);
    assert_eq!(a.review.as_deref(), Some("required"));
    review::acknowledge(&mut store, &thread, SNAP, OID).expect("ack tras resolver");
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::ValidationRequired);
}

// ── validación: snapshot + OID exactos ──────────────────────────────────────

#[test]
fn validation_of_another_oid_never_counts() {
    let (mut store, thread, run) = new_thread("otro oid");
    add_run(&mut store, &thread, &run, "completed");
    ready_with_review(&mut store, &thread, &run);
    add_validation(&mut store, &thread, &run, SNAP, OTHER_OID, 0, true);
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::ValidationFailed);
    assert_eq!(a.validation, Some(ValidationState::WrongOid));
    assert!(a.reason.contains(OTHER_OID));
}

#[test]
fn failed_validation_then_passed_reaches_ready() {
    let (mut store, thread, run) = new_thread("validación");
    add_run(&mut store, &thread, &run, "completed");
    ready_with_review(&mut store, &thread, &run);
    add_validation(&mut store, &thread, &run, SNAP, OID, 1, true);
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::ValidationFailed);
    assert_eq!(a.next_action, "corregir y revalidar");
    add_validation(&mut store, &thread, &run, SNAP, OID, 0, true);
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::ReadyToIntegrate);
    assert_eq!(a.validation, Some(ValidationState::Passed));
    assert_eq!(a.next_action, "preview e integrar");
    assert!(a.is_actionable());
}

#[test]
fn validation_that_mutated_the_checkout_is_failed() {
    let (mut store, thread, run) = new_thread("checkout sucio");
    add_run(&mut store, &thread, &run, "completed");
    ready_with_review(&mut store, &thread, &run);
    add_validation(&mut store, &thread, &run, SNAP, OID, 0, false);
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::ValidationFailed);
    assert!(a.reason.contains("cambió") || a.reason.contains("reprodujo"));
}

// ── precedencia ─────────────────────────────────────────────────────────────

#[test]
fn pending_recovery_precedes_review_of_a_ready_checkpoint() {
    let (mut store, thread, run) = new_thread("recuperación");
    add_run(&mut store, &thread, &run, "completed");
    add_checkpoint(&mut store, &thread, &run, SNAP, OID);
    add_prepared(&mut store, &thread, &run, "snap-pendiente", OTHER_OID);
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::RecoveryRequired);
    assert!(
        a.priority < AttentionStatus::ReviewRequired.priority(),
        "recuperación debe preceder a revisión"
    );
}

#[test]
fn integrity_hold_precedes_permission_and_review() {
    let (mut store, thread, run) = new_thread("integridad");
    add_run(&mut store, &thread, &run, "running");
    append(
        &mut store,
        &thread,
        Some(&run),
        events::K_PERMISSION_REQUESTED,
        json!({
            "request_id": "req-1",
            "tool_call_id": "call-1",
            "title": "editar archivo",
            "options": ["allow_once"]
        }),
    );
    add_prepared(&mut store, &thread, &run, SNAP, OTHER_OID);
    add_ready(&mut store, &thread, &run, SNAP, OID);
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::IntegrityHold);
    assert_eq!(a.priority, 0);
    assert_eq!(a.snapshot_id.as_deref(), Some(SNAP));
    assert_eq!(a.source_oid.as_deref(), Some(OID));
}

#[test]
fn blocked_integration_precedes_ready_to_integrate() {
    let (mut store, thread, run) = new_thread("bloqueada");
    add_run(&mut store, &thread, &run, "completed");
    ready_with_review(&mut store, &thread, &run);
    add_validation(&mut store, &thread, &run, SNAP, OID, 0, true);
    add_integration(
        &mut store,
        &thread,
        events::K_INTEGRATION_BLOCKED,
        OID,
        "la rama destino main está siendo usada por un checkout: integrá desde un checkout limpio",
    );
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::IntegrationBlocked);
    assert_eq!(a.next_action, "integrar desde un checkout limpio");
    assert_eq!(
        a.integration.as_deref(),
        Some(events::K_INTEGRATION_BLOCKED)
    );
    assert_eq!(a.snapshot_id.as_deref(), Some(SNAP));
    assert_eq!(a.source_oid.as_deref(), Some(OID));
}

#[test]
fn stale_integration_for_another_oid_does_not_mask_the_new_checkpoint() {
    let (mut store, thread, run) = new_thread("evento viejo");
    add_run(&mut store, &thread, &run, "completed");
    ready_with_review(&mut store, &thread, &run);
    add_validation(&mut store, &thread, &run, SNAP, OID, 0, true);
    add_integration(
        &mut store,
        &thread,
        events::K_INTEGRATION_BLOCKED,
        OTHER_OID,
        "la rama destino se movió",
    );
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::ReadyToIntegrate);
}

#[test]
fn completed_integration_closes_the_task() {
    let (mut store, thread, run) = new_thread("integrada");
    add_run(&mut store, &thread, &run, "completed");
    ready_with_review(&mut store, &thread, &run);
    add_validation(&mut store, &thread, &run, SNAP, OID, 0, true);
    add_integration(
        &mut store,
        &thread,
        events::K_INTEGRATION_COMPLETED,
        OID,
        "",
    );
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::Done);
    assert_eq!(a.next_action, "ninguna");
    assert!(!a.is_actionable());
}

#[test]
fn integrated_checkpoint_does_not_hide_a_new_active_run() {
    let (mut store, thread, run) = new_thread("integrada y seguida");
    add_run(&mut store, &thread, &run, "completed");
    ready_with_review(&mut store, &thread, &run);
    add_validation(&mut store, &thread, &run, SNAP, OID, 0, true);
    add_integration(
        &mut store,
        &thread,
        events::K_INTEGRATION_COMPLETED,
        OID,
        "",
    );
    add_run_at(&mut store, &thread, "run-2", "running", 10);
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::InProgress);
    assert_eq!(a.run_state.as_deref(), Some("running"));
    assert_eq!(a.next_action, "observar progreso");
}

#[test]
fn integrated_checkpoint_does_not_hide_a_later_failed_run() {
    let (mut store, thread, run) = new_thread("integrada y fallida");
    add_run(&mut store, &thread, &run, "completed");
    ready_with_review(&mut store, &thread, &run);
    add_validation(&mut store, &thread, &run, SNAP, OID, 0, true);
    add_integration(
        &mut store,
        &thread,
        events::K_INTEGRATION_COMPLETED,
        OID,
        "",
    );
    add_run_at(&mut store, &thread, "run-2", "failed", 10);
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::Failed);
    assert_eq!(a.run_state.as_deref(), Some("failed"));
    assert_eq!(a.next_action, "revisar fallo");
}

#[test]
fn unresolved_integration_intent_requires_recovery() {
    let (mut store, thread, run) = new_thread("intención");
    add_run(&mut store, &thread, &run, "completed");
    ready_with_review(&mut store, &thread, &run);
    add_validation(&mut store, &thread, &run, SNAP, OID, 0, true);
    add_integration(
        &mut store,
        &thread,
        events::K_INTEGRATION_REQUESTED,
        OID,
        "",
    );
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::RecoveryRequired);
}

// ── permiso ─────────────────────────────────────────────────────────────────

#[test]
fn permission_is_actionable_only_while_its_run_is_live() {
    let (mut store, thread, run) = new_thread("permiso");
    add_run(&mut store, &thread, &run, "running");
    append(
        &mut store,
        &thread,
        Some(&run),
        events::K_PERMISSION_REQUESTED,
        json!({
            "request_id": "req-1",
            "tool_call_id": "call-1",
            "title": "editar NOTES.md",
            "options": ["allow_once", "deny"]
        }),
    );
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::PermissionPending);
    assert_eq!(a.next_action, "responder permiso");
    assert!(a.reason.contains("NOTES.md"));

    append(
        &mut store,
        &thread,
        Some(&run),
        events::K_PERMISSION_RESPONDED,
        json!({"request_id": "req-1", "option_id": "allow_once", "policy": "auto"}),
    );
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::InProgress);

    append(
        &mut store,
        &thread,
        Some(&run),
        events::K_RUN_ENDED,
        json!({"state": "completed"}),
    );
    set_run_state(&store, &run, "completed");
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::Done);
}

// ── evidencia inválida no se adivina ────────────────────────────────────────

#[test]
fn malformed_integration_event_is_integrity_hold() {
    let (mut store, thread, run) = new_thread("integración ilegible");
    add_run(&mut store, &thread, &run, "completed");
    add_checkpoint(&mut store, &thread, &run, SNAP, OID);
    append(
        &mut store,
        &thread,
        None,
        events::K_INTEGRATION_BLOCKED,
        json!({}),
    );
    assert!(attention::project_thread(&store, &thread).is_err());
    let all = attention::project_all(&store, 10).expect("proyección global");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].status, AttentionStatus::IntegrityHold);
}

// ── integración real: CLI y TUI ─────────────────────────────────────────────

fn ready_env(name: &str) -> (TestEnv, String, String) {
    let env = TestEnv::new(name);
    let thread = env.thread_new("attention cli");
    let script = env.script(
        "s",
        mock_edit("NOTES.md", "# v2\n\ncontenido attention\n", "edit"),
    );
    let run = env.run_start(&thread, "cambiá NOTES", &script);
    assert_eq!(run["state"], "completed", "run: {run}");
    let snapshots = env.ready_snapshots(&thread);
    assert_eq!(snapshots.len(), 1, "checkpoint ready: {snapshots:?}");
    (env, thread, snapshots[0].0.clone())
}

fn attention_item(env: &TestEnv, thread: &str) -> serde_json::Value {
    let all = env.aeril_ok(&["attention", "--json"]);
    all.as_array()
        .expect("array de attention")
        .iter()
        .find(|v| v["thread_id"] == thread)
        .cloned()
        .unwrap_or_else(|| panic!("thread {thread} ausente de attention: {all}"))
}

#[test]
fn cli_attention_reports_exact_next_action() {
    let (env, thread, snapshot) = ready_env("pa0-attn-cli");
    let a = attention_item(&env, &thread);
    assert_eq!(a["status"], "review_required");
    assert_eq!(a["next_action"], "marcar revisado");
    assert_eq!(a["review"], "required");
    let human = env.aeril(&["attention"]);
    assert!(human.status.success());
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        text.contains("checkpoint listo · → marcar revisado"),
        "{text}"
    );

    env.aeril_ok(&[
        "review",
        "ack",
        "--thread",
        &thread,
        "--snapshot",
        &snapshot,
    ]);
    let a = attention_item(&env, &thread);
    assert_eq!(a["status"], "validation_required");
    assert_eq!(a["next_action"], "ejecutar validación");

    env.aeril_ok(&[
        "validate",
        "--thread",
        &thread,
        "--snapshot",
        &snapshot,
        "--",
        "true",
    ]);
    let a = attention_item(&env, &thread);
    assert_eq!(a["status"], "ready_to_integrate");
    assert_eq!(a["next_action"], "preview e integrar");
    assert_eq!(a["validation"], "passed");
}

#[test]
fn tui_shows_attention_and_a_marks_the_checkpoint_reviewed() {
    let (env, thread, snapshot) = ready_env("pa0-attn-tui");
    let mut pty = TuiPty::spawn(&env, &[], &[]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !pty.output_contains("Aeril") {
        assert!(
            Instant::now() < deadline,
            "la TUI no dibujó el primer frame"
        );
        pty.pump(100);
    }
    pty.enter();
    pty.pump(400);
    assert!(
        pty.output_contains("checkpoint listo"),
        "falta el estado de attention: {}",
        pty.output_lossy()
    );
    assert!(
        pty.output_contains("marcar revisado"),
        "falta la próxima acción: {}",
        pty.output_lossy()
    );
    pty.send(b"a");
    pty.pump(500);
    let store = Store::open(&env.data.join("aeril.db")).expect("store");
    assert!(
        review::acknowledged(
            &store,
            &thread,
            &snapshot,
            &env.ready_snapshots(&thread)[0].1
        )
        .expect("ack"),
        "la tecla `a` debe escribir review.acknowledged"
    );
    assert!(
        pty.output_contains("marcado revisado"),
        "falta la confirmación visible del acknowledgement: {}",
        pty.output_lossy()
    );
    pty.kill();
}
