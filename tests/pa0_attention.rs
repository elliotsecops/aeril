//! PA0-B: proyección de Attention y próxima acción.
//!
//! Los tests sintéticos fijan la precedencia y las distinciones que no deben
//! confundirse: ready vs prepared recuperable, revisión vs "sin comentarios",
//! validación de otro OID, integración bloqueada vs terminada y eventos viejos.

mod common;

use aeril::attention::{self, AttentionStatus};
use aeril::ids;
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

fn add_ack(store: &mut Store, thread: &str, snapshot: &str, oid: &str) -> String {
    append(
        store,
        thread,
        None,
        events::K_REVIEW_ACKNOWLEDGED,
        json!({"snapshot_id": snapshot, "oid": oid}),
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
    add_ack(store, thread, SNAP, OID);
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
    assert!(
        a.is_actionable(),
        "una tarea sin run requiere iniciar trabajo"
    );
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
    add_ack(&mut store, &thread, SNAP, OID);
    assert!(
        review::acknowledged(&store, &thread, SNAP, OID).expect("consulta"),
        "el acknowledgement exacto debe existir"
    );
    assert!(
        !review::acknowledged(&store, &thread, SNAP, OTHER_OID).expect("consulta"),
        "el acknowledgement no debe satisfacer otro OID"
    );
    let a = project(&store, &thread);
    assert_eq!(a.review.as_deref(), Some("clear"));
    assert_eq!(a.status, AttentionStatus::ValidationRequired);
}

#[test]
fn open_comments_keep_review_pending() {
    let (mut store, thread, run) = new_thread("comentarios");
    add_run(&mut store, &thread, &run, "completed");
    let ready = add_checkpoint(&mut store, &thread, &run, SNAP, OID);
    let comment = add_comment(&mut store, &thread, &run, &ready, "revisar esto");
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::ReviewRequired);
    assert_eq!(a.review.as_deref(), Some("pending"));
    assert_eq!(a.next_action, "resolver comentarios");
    assert!(a.reason.contains('1'));
    review::resolve(&mut store, &thread, &comment).expect("resolve");
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::ReviewRequired);
    assert_eq!(a.review.as_deref(), Some("required"));
    add_ack(&mut store, &thread, SNAP, OID);
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
    assert_eq!(a.next_action, "abrir preview de integración");
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

// ── evidencia contradictoria: integridad antes que acción ──────────────────

#[test]
fn ready_without_prepared_is_integrity_hold() {
    let (mut store, thread, run) = new_thread("ready sin prepared");
    add_run(&mut store, &thread, &run, "completed");
    add_ready(&mut store, &thread, &run, SNAP, OID);
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::IntegrityHold);
    assert_eq!(a.next_action, "revisar estado");
    assert!(a.reason.contains("prepared"));
}

#[test]
fn contradictory_prepared_is_integrity_hold() {
    let (mut store, thread, run) = new_thread("prepared contradictorio");
    add_run(&mut store, &thread, &run, "completed");
    add_prepared(&mut store, &thread, &run, SNAP, OTHER_OID);
    add_ready(&mut store, &thread, &run, SNAP, OID);
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::IntegrityHold);
    assert!(a.reason.contains("contradicen"));
}

#[test]
fn ready_with_failed_marker_is_integrity_hold() {
    let (mut store, thread, run) = new_thread("ready y failed");
    add_run(&mut store, &thread, &run, "completed");
    add_checkpoint(&mut store, &thread, &run, SNAP, OID);
    append(
        &mut store,
        &thread,
        Some(&run),
        events::K_CHECKPOINT_FAILED,
        json!({
            "snapshot_id": SNAP,
            "phase": "recovery",
            "reason": "marcador tardío",
            "terminal": true
        }),
    );
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::IntegrityHold);
    assert!(a.reason.contains("failed"));
}

// ── secuencias de eventos: stale, validación y recuperación ─────────────────

#[test]
fn blocked_then_completed_does_not_keep_advertising_the_block() {
    let (mut store, thread, run) = new_thread("bloqueo resuelto");
    add_run(&mut store, &thread, &run, "completed");
    ready_with_review(&mut store, &thread, &run);
    add_validation(&mut store, &thread, &run, SNAP, OID, 0, true);
    add_integration(
        &mut store,
        &thread,
        events::K_INTEGRATION_BLOCKED,
        OID,
        "la rama destino está siendo usada por un checkout",
    );
    assert_eq!(
        project(&store, &thread).status,
        AttentionStatus::IntegrationBlocked
    );
    add_integration(
        &mut store,
        &thread,
        events::K_INTEGRATION_COMPLETED,
        OID,
        "",
    );
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::Done);
    assert_eq!(a.status_label, "integrado");
    assert_eq!(a.next_action, "ninguna");
}

#[test]
fn validation_of_checkpoint_a_does_not_validate_checkpoint_b() {
    let (mut store, thread, run) = new_thread("validación no heredada");
    add_run(&mut store, &thread, &run, "completed");
    add_checkpoint(&mut store, &thread, &run, "snap-a", OID);
    add_validation(&mut store, &thread, &run, "snap-a", OID, 0, true);
    add_checkpoint(&mut store, &thread, &run, "snap-b", OTHER_OID);
    add_ack(&mut store, &thread, "snap-b", OTHER_OID);
    let a = project(&store, &thread);
    assert_eq!(a.snapshot_id.as_deref(), Some("snap-b"));
    assert_eq!(a.status, AttentionStatus::ValidationRequired);
    assert_eq!(a.validation, Some(ValidationState::Missing));
}

#[test]
fn late_recovered_ready_a_does_not_replace_logical_latest_b() {
    let (mut store, thread, run) = new_thread("ready recuperado tarde");
    add_run(&mut store, &thread, &run, "completed");
    add_prepared(&mut store, &thread, &run, "snap-a", OID);
    add_prepared(&mut store, &thread, &run, "snap-b", OTHER_OID);
    add_ready(&mut store, &thread, &run, "snap-b", OTHER_OID);
    add_ready(&mut store, &thread, &run, "snap-a", OID);
    let a = project(&store, &thread);
    assert_eq!(a.snapshot_id.as_deref(), Some("snap-b"));
    assert_eq!(a.status, AttentionStatus::ReviewRequired);
}

#[test]
fn completed_run_without_pending_checkpoint_is_not_called_integrated() {
    let (mut store, thread, run) = new_thread("sin checkpoint");
    add_run(&mut store, &thread, &run, "completed");
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::Done);
    assert_eq!(a.status_label, "sin trabajo pendiente");
    assert_eq!(a.next_action, "ninguna");
}

// ── permisos: ni pendientes ni auditados como limpios ───────────────────────

#[test]
fn completed_run_with_unresolved_permission_is_not_clean() {
    let (mut store, thread, run) = new_thread("permiso sin responder");
    add_run(&mut store, &thread, &run, "completed");
    append(
        &mut store,
        &thread,
        Some(&run),
        events::K_PERMISSION_REQUESTED,
        json!({
            "request_id": "req-1",
            "tool_call_id": "call-1",
            "title": "editar NOTES.md",
            "options": ["allow_once"]
        }),
    );
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::IntegrityHold);
    assert!(a.reason.contains("sin respuesta durable"));
}

#[test]
fn permission_audit_failure_is_integrity_hold() {
    let (mut store, thread, run) = new_thread("auditoría de permiso");
    add_run(&mut store, &thread, &run, "completed");
    append(
        &mut store,
        &thread,
        Some(&run),
        events::K_PERMISSION_AUDIT_FAILED,
        json!({
            "request_id": "req-1",
            "stage": "responded",
            "detail": "no se pudo escribir la decisión"
        }),
    );
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::IntegrityHold);
    assert!(a.reason.contains("auditoría"));
}

#[test]
fn malformed_comment_anchor_fails_closed() {
    let (mut store, thread, run) = new_thread("comentario sin anchor");
    add_run(&mut store, &thread, &run, "completed");
    let ready = add_checkpoint(&mut store, &thread, &run, SNAP, OID);
    append(
        &mut store,
        &thread,
        Some(&run),
        events::K_COMMENT_ADDED,
        json!({
            "checkpoint_event_id": ready,
            "oid": OID,
            "text": "comentario corrupto"
        }),
    );
    assert!(attention::project_thread(&store, &thread).is_err());
    let all = attention::project_all(&store, 10).expect("proyección global");
    assert_eq!(all[0].status, AttentionStatus::IntegrityHold);
}

#[test]
fn legacy_permission_without_run_id_is_not_clean() {
    let (mut store, thread, run) = new_thread("permiso legacy");
    add_run(&mut store, &thread, &run, "completed");
    append(
        &mut store,
        &thread,
        None,
        events::K_PERMISSION_REQUESTED,
        json!({
            "request_id": "req-legacy",
            "tool_call_id": "call-legacy",
            "title": "editar NOTES.md",
            "options": ["allow_once"]
        }),
    );
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::IntegrityHold);
    assert!(a.reason.contains("sin respuesta durable"));
}

#[test]
fn legacy_permission_audit_failure_without_run_id_is_not_clean() {
    let (mut store, thread, run) = new_thread("auditoría legacy");
    add_run(&mut store, &thread, &run, "completed");
    append(
        &mut store,
        &thread,
        None,
        events::K_PERMISSION_AUDIT_FAILED,
        json!({
            "request_id": "req-legacy",
            "stage": "responded",
            "detail": "fallo durable sin run_id"
        }),
    );
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::IntegrityHold);
    assert!(a.reason.contains("auditoría"));
}

// ── runs solapados y read-only ──────────────────────────────────────────────

#[test]
fn old_checkpoint_is_marked_as_historical_when_a_new_run_started() {
    let (mut store, thread, run) = new_thread("run solapado");
    add_run(&mut store, &thread, &run, "completed");
    add_checkpoint(&mut store, &thread, &run, SNAP, OID);
    add_run_at(&mut store, &thread, "run-2", "running", 10);
    let a = project(&store, &thread);
    assert_eq!(a.status, AttentionStatus::ReviewRequired);
    assert_eq!(a.run_id.as_deref(), Some("run-2"));
    assert_eq!(a.checkpoint_run_id.as_deref(), Some("run-1"));
    assert!(a.historical_checkpoint);
    assert!(a.reason.contains("run anterior"));
}

#[test]
fn attention_projection_is_read_only() {
    let (mut store, thread, run) = new_thread("read only");
    add_run(&mut store, &thread, &run, "completed");
    add_checkpoint(&mut store, &thread, &run, SNAP, OID);
    let count: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .expect("count");
    let max_seq: i64 = store
        .conn
        .query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |row| {
            row.get(0)
        })
        .expect("seq");
    for _ in 0..10 {
        attention::project_all(&store, 100).expect("proyección");
    }
    let count_after: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .expect("count");
    let max_seq_after: i64 = store
        .conn
        .query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |row| {
            row.get(0)
        })
        .expect("seq");
    assert_eq!(count, count_after);
    assert_eq!(max_seq, max_seq_after);
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

fn count_kind(env: &TestEnv, thread: &str, kind: &str) -> usize {
    let store = Store::open(&env.data.join("aeril.db")).expect("store");
    events::fetch_by_kind(&store.conn, thread, kind)
        .expect("events")
        .len()
}

fn ready_oid(env: &TestEnv, thread: &str) -> String {
    env.ready_snapshots(thread)[0].1.clone()
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
    assert_eq!(count_kind(&env, &thread, events::K_REVIEW_ACKNOWLEDGED), 0);
    let human = env.aeril(&["attention"]);
    assert!(human.status.success());
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        text.contains("checkpoint listo · → marcar revisado"),
        "{text}"
    );

    let first = env.aeril_ok(&[
        "review",
        "ack",
        "--thread",
        &thread,
        "--snapshot",
        &snapshot,
    ]);
    assert_eq!(first["new_event"], true);
    let second = env.aeril_ok(&[
        "review",
        "ack",
        "--thread",
        &thread,
        "--snapshot",
        &snapshot,
    ]);
    assert_eq!(
        second["new_event"], false,
        "ack repetido debe ser idempotente"
    );
    assert_eq!(count_kind(&env, &thread, events::K_REVIEW_ACKNOWLEDGED), 1);
    for _ in 0..3 {
        let _ = attention_item(&env, &thread);
    }
    assert_eq!(
        count_kind(&env, &thread, events::K_REVIEW_ACKNOWLEDGED),
        1,
        "Attention no debe escribir acknowledgements"
    );
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
    assert_eq!(a["next_action"], "abrir preview de integración");
    assert_eq!(a["validation"], "passed");
}

#[test]
fn cli_ack_rejects_unverifiable_or_contradictory_checkpoints() {
    let (env, thread, snapshot) = ready_env("pa0-attn-ack-strict");
    let mut store = Store::open(&env.data.join("aeril.db")).expect("store");
    let oid = ready_oid(&env, &thread);
    let prepared_run: String = store
        .conn
        .query_row(
            "SELECT run_id FROM events
              WHERE thread_id = ?1 AND kind = ?2
                AND json_extract(payload, '$.snapshot_id') = ?3
              ORDER BY seq DESC LIMIT 1",
            rusqlite::params![thread, events::K_CHECKPOINT_PREPARED, snapshot],
            |row| row.get(0),
        )
        .expect("run_id del prepared");

    // Identidad de run contradictoria entre prepared y ready.
    store
        .conn
        .execute(
            "UPDATE events SET payload = json_set(payload, '$.run_id', 'otro-run')
              WHERE thread_id = ?1 AND kind = ?2
                AND json_extract(payload, '$.snapshot_id') = ?3",
            rusqlite::params![thread, events::K_CHECKPOINT_PREPARED, snapshot],
        )
        .expect("mutar run_id");
    let out = env.aeril(&[
        "review",
        "ack",
        "--thread",
        &thread,
        "--snapshot",
        &snapshot,
    ]);
    assert!(
        !out.status.success(),
        "ack con run_id contradictorio debe fallar"
    );
    store
        .conn
        .execute(
            "UPDATE events SET payload = json_set(payload, '$.run_id', ?2)
              WHERE thread_id = ?1 AND kind = ?3
                AND json_extract(payload, '$.snapshot_id') = ?4",
            rusqlite::params![
                thread,
                prepared_run,
                events::K_CHECKPOINT_PREPARED,
                snapshot
            ],
        )
        .expect("restaurar run_id");

    store
        .conn
        .execute(
            "UPDATE events SET payload = json_set(payload, '$.base_oid', ?2)
              WHERE thread_id = ?1 AND kind = ?3
                AND json_extract(payload, '$.snapshot_id') = ?4",
            rusqlite::params![thread, OTHER_OID, events::K_CHECKPOINT_PREPARED, snapshot],
        )
        .expect("mutar base_oid");
    let out = env.aeril(&[
        "review",
        "ack",
        "--thread",
        &thread,
        "--snapshot",
        &snapshot,
    ]);
    assert!(
        !out.status.success(),
        "ack con base_oid contradictorio debe fallar"
    );
    store
        .conn
        .execute(
            "UPDATE events SET payload = json_set(payload, '$.base_oid', ?2)
              WHERE thread_id = ?1 AND kind = ?3
                AND json_extract(payload, '$.snapshot_id') = ?4",
            rusqlite::params![thread, BASE, events::K_CHECKPOINT_PREPARED, snapshot],
        )
        .expect("restaurar base_oid");

    store
        .conn
        .execute(
            "UPDATE events SET payload = json_set(payload, '$.expected_oid', ?2)
              WHERE thread_id = ?1 AND kind = ?3
                AND json_extract(payload, '$.snapshot_id') = ?4",
            rusqlite::params![thread, OTHER_OID, events::K_CHECKPOINT_PREPARED, snapshot],
        )
        .expect("mutar prepared");
    let out = env.aeril(&[
        "review",
        "ack",
        "--thread",
        &thread,
        "--snapshot",
        &snapshot,
    ]);
    assert!(
        !out.status.success(),
        "ack con prepared contradictorio debe fallar"
    );

    store
        .conn
        .execute(
            "UPDATE events SET payload = json_set(payload, '$.expected_oid', ?2)
              WHERE thread_id = ?1 AND kind = ?3
                AND json_extract(payload, '$.snapshot_id') = ?4",
            rusqlite::params![thread, oid, events::K_CHECKPOINT_PREPARED, snapshot],
        )
        .expect("restaurar prepared");
    events::append(
        &mut store.conn,
        &thread,
        None,
        events::K_CHECKPOINT_FAILED,
        "system",
        &json!({
            "snapshot_id": snapshot,
            "phase": "recovery",
            "reason": "marcador de auditoría",
            "terminal": true
        }),
    )
    .expect("failed marker");
    let out = env.aeril(&[
        "review",
        "ack",
        "--thread",
        &thread,
        "--snapshot",
        &snapshot,
    ]);
    assert!(
        !out.status.success(),
        "ack con checkpoint.failed debe fallar"
    );

    store
        .conn
        .execute(
            "DELETE FROM events WHERE thread_id = ?1 AND kind = ?2
              AND json_extract(payload, '$.snapshot_id') = ?3",
            rusqlite::params![thread, events::K_CHECKPOINT_FAILED, snapshot],
        )
        .expect("quitar failed");
    git(
        &env.repo,
        &["update-ref", "-d", &ids::checkpoint_ref(&snapshot)],
    );
    let out = env.aeril(&[
        "review",
        "ack",
        "--thread",
        &thread,
        "--snapshot",
        &snapshot,
    ]);
    assert!(!out.status.success(), "ack sin ref verificable debe fallar");

    env.update_ref(&ids::checkpoint_ref(&snapshot), &oid);
    store
        .conn
        .execute(
            "DELETE FROM events WHERE thread_id = ?1 AND kind = ?2
              AND json_extract(payload, '$.snapshot_id') = ?3",
            rusqlite::params![thread, events::K_CHECKPOINT_PREPARED, snapshot],
        )
        .expect("quitar prepared");
    let out = env.aeril(&[
        "review",
        "ack",
        "--thread",
        &thread,
        "--snapshot",
        &snapshot,
    ]);
    assert!(
        !out.status.success(),
        "ack sin prepared verificable debe fallar"
    );
    assert_eq!(count_kind(&env, &thread, events::K_REVIEW_ACKNOWLEDGED), 0);
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
    assert_eq!(
        count_kind(&env, &thread, events::K_REVIEW_ACKNOWLEDGED),
        0,
        "abrir y mirar la tarea no debe crear un acknowledgement"
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

#[test]
fn tui_attention_is_legible_at_supported_sizes() {
    let (env, _thread, _snapshot) = ready_env("pa0-attn-sizes");
    for (rows, cols) in [(24, 80), (40, 120), (50, 180)] {
        let mut pty = TuiPty::spawn_sized(&env, &[], &[], rows, cols);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !pty.output_contains("Aeril") {
            assert!(
                Instant::now() < deadline,
                "la TUI no dibujó el primer frame en {cols}x{rows}"
            );
            pty.pump(100);
        }
        pty.enter();
        pty.pump(400);
        assert!(
            pty.output_contains("checkpoint listo"),
            "falta el estado en {cols}x{rows}: {}",
            pty.output_lossy()
        );
        assert!(
            pty.output_contains("marcar revisado"),
            "falta la próxima acción en {cols}x{rows}: {}",
            pty.output_lossy()
        );
        pty.kill();
    }
}
