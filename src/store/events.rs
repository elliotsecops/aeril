//! Eventos: hechos inmutables, ordenados por `(thread_id, seq)`.
//!
//! Reglas v0.4:
//! - `UNIQUE(thread_id, seq)`; la secuencia se asigna SOLO dentro de la
//!   transacción de append (`UPDATE ... RETURNING next_seq - 1`), nunca con
//!   `MAX(seq)+1` fuera de transacción.
//! - Payloads versionados (`payload_version`) con forma por `kind`; los
//!   productores usan los structs tipados de [`payload`].

use crate::error::{Error, Result};
use crate::timeutil::now_ms;
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

// ── kinds ────────────────────────────────────────────────────────────────────
pub const K_THREAD_CREATED: &str = "thread.created";
pub const K_RUN_STARTED: &str = "run.started";
pub const K_ENGINE_SESSION: &str = "engine.session";
pub const K_AGENT_MESSAGE: &str = "agent.message";
pub const K_AGENT_TOOL_CALL: &str = "agent.tool_call";
pub const K_PERMISSION_REQUESTED: &str = "permission.requested";
pub const K_PERMISSION_RESPONDED: &str = "permission.responded";
pub const K_PERMISSION_TRANSMITTED: &str = "permission.transmitted";
pub const K_PERMISSION_AUDIT_FAILED: &str = "permission.audit_failed";
pub const K_RUN_ENDED: &str = "run.ended";
pub const K_CHECKPOINT_PREPARED: &str = "checkpoint.prepared";
pub const K_CHECKPOINT_READY: &str = "checkpoint.ready";
pub const K_CHECKPOINT_FAILED: &str = "checkpoint.failed";
pub const K_COMMENT_ADDED: &str = "comment.added";
pub const K_COMMENT_RESOLVED: &str = "comment.resolved";
pub const K_FEEDBACK_SENT: &str = "feedback.sent";
pub const K_FEEDBACK_DELIVERED: &str = "feedback.delivered";
pub const K_FEEDBACK_FAILED: &str = "feedback.failed";
pub const K_WORKSPACE_RECREATED: &str = "workspace.recreated";
pub const K_WORKSPACE_LEASE_TAKEN_OVER: &str = "workspace.lease_taken_over";
pub const K_WORKSPACE_LEASE_LOST: &str = "workspace.lease_lost";
pub const K_INTEGRATION_REQUESTED: &str = "integration.requested";
pub const K_INTEGRATION_COMPLETED: &str = "integration.completed";
pub const K_INTEGRATION_BLOCKED: &str = "integration.blocked";
pub const K_VALIDATION_RECORDED: &str = "validation.recorded";

// ── payloads tipados (v1) ────────────────────────────────────────────────────
pub mod payload {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, Default)]
    pub struct ThreadCreated {
        pub title: String,
        pub repo_path: String,
        pub repo_id: String,
    }

    #[derive(Debug, Serialize, Deserialize, Default)]
    pub struct RunStarted {
        pub backend: String,
        pub goal: String,
        pub workspace_id: String,
        pub repo_path: String,
        pub worktree_path: String,
        pub branch: String,
        pub base_oid: String,
        #[serde(default)]
        pub resumed_session: Option<String>,
    }

    #[derive(Debug, Serialize, Deserialize, Default)]
    pub struct EngineSession {
        pub engine_session_id: String,
        /// `true` si la sesión final en uso es la misma que la previa.
        pub resumed: bool,
        /// Operación intentada primero: `"load"` o `"new"`.
        #[serde(default)]
        pub attempted: Option<String>,
        /// El motor aceptó el intento (protocolo). No implica restauración.
        #[serde(default)]
        pub accepted: Option<bool>,
        /// Restauración del contexto previo VERIFICADA. Con Hermes 0.21.5
        /// `session/load` de una sesión desconocida responde OK sin historial:
        /// no es verificable → siempre `false` en v0.
        #[serde(default)]
        pub verified: bool,
    }

    #[derive(Debug, Serialize, Deserialize, Default)]
    pub struct AgentMessage {
        pub text: String,
        #[serde(default)]
        pub message_id: Option<String>,
    }

    #[derive(Debug, Serialize, Deserialize, Default)]
    pub struct AgentToolCall {
        pub tool_call_id: String,
        pub title: String,
        #[serde(default)]
        pub kind: Option<String>,
        #[serde(default)]
        pub status: Option<String>,
    }

    #[derive(Debug, Serialize, Deserialize, Default)]
    pub struct PermissionRequested {
        pub request_id: String,
        pub tool_call_id: String,
        pub title: String,
        pub options: Vec<String>,
        #[serde(default)]
        pub diff_path: Option<String>,
    }

    #[derive(Debug, Serialize, Deserialize, Default)]
    pub struct PermissionResponded {
        pub request_id: String,
        pub option_id: String,
        pub policy: String,
    }

    /// La decisión durable (`permission.responded`) NO afirma entrega: este
    /// evento registra el intento de transmisión al backend y su resultado.
    #[derive(Debug, Serialize, Deserialize, Default)]
    pub struct PermissionTransmitted {
        pub request_id: String,
        pub option_id: String,
        pub ok: bool,
    }

    /// Fallo al persistir estado de autorización: sin auditoría durable no
    /// hay autorización (fail closed). Best-effort; puede fallar también.
    #[derive(Debug, Serialize, Deserialize, Default)]
    pub struct PermissionAuditFailed {
        pub request_id: String,
        pub stage: String,
        pub detail: String,
    }

    #[derive(Debug, Serialize, Deserialize, Default)]
    pub struct RunEnded {
        pub state: String,
        #[serde(default)]
        pub reason: Option<String>,
        #[serde(default)]
        pub stop_reason: Option<String>,
    }

    #[derive(Debug, Serialize, Deserialize, Clone, Default)]
    pub struct FileSummary {
        pub path: String,
        pub status: String,
        pub additions: usize,
        pub deletions: usize,
        #[serde(default)]
        pub before_blob_oid: Option<String>,
        #[serde(default)]
        pub after_blob_oid: Option<String>,
    }

    #[derive(Debug, Serialize, Deserialize, Default)]
    pub struct CheckpointPrepared {
        pub snapshot_id: String,
        pub git_ref: String,
        pub expected_oid: String,
        #[serde(default)]
        pub run_id: Option<String>,
        pub base_oid: String,
        pub parent_oid: String,
        pub files: Vec<FileSummary>,
    }

    #[derive(Debug, Serialize, Deserialize, Default)]
    pub struct CheckpointReady {
        pub snapshot_id: String,
        pub git_ref: String,
        pub oid: String,
        pub base_oid: String,
        /// Patch exacto revisado (formato git diff), almacenado con el evento.
        pub patch: String,
        pub files: Vec<FileSummary>,
        /// `true` si lo completó la recuperación al reabrir, no la captura.
        pub recovered: bool,
    }

    #[derive(Debug, Serialize, Deserialize, Default)]
    pub struct CheckpointFailed {
        pub snapshot_id: String,
        #[serde(default)]
        pub git_ref: Option<String>,
        #[serde(default)]
        pub expected_oid: Option<String>,
        pub phase: String,
        pub reason: String,
        pub terminal: bool,
    }

    /// Ancla de un comentario: referencia a contenido inmutable, nunca a una
    /// línea "actual". `side` es `new` (lado posterior) en v0.
    #[derive(Debug, Serialize, Deserialize, Clone, Default)]
    pub struct Anchor {
        pub snapshot_id: String,
        pub path: String,
        pub side: String,
        pub start_line: u64,
        pub end_line: u64,
        #[serde(default)]
        pub before_blob_oid: Option<String>,
        #[serde(default)]
        pub after_blob_oid: Option<String>,
        /// Hash del fragmento anclado dentro de la versión inmutable.
        pub context_hash: String,
    }

    #[derive(Debug, Serialize, Deserialize, Default)]
    pub struct CommentAdded {
        /// Evento `checkpoint.ready` del snapshot anclado (mismo thread).
        pub checkpoint_event_id: String,
        pub oid: String,
        pub anchor: Anchor,
        pub text: String,
    }

    #[derive(Debug, Serialize, Deserialize, Default)]
    pub struct FeedbackSent {
        pub comment_ids: Vec<String>,
        pub run_id: String,
    }

    #[derive(Debug, Serialize, Deserialize, Default)]
    pub struct FeedbackDelivered {
        pub comment_ids: Vec<String>,
        pub run_id: String,
    }

    #[derive(Debug, Serialize, Deserialize, Default)]
    pub struct FeedbackFailed {
        pub comment_ids: Vec<String>,
        #[serde(default)]
        pub run_id: Option<String>,
        pub reason: String,
    }

    #[derive(Debug, Serialize, Deserialize, Default)]
    pub struct CommentResolved {
        pub comment_id: String,
    }

    #[derive(Debug, Serialize, Deserialize, Default)]
    pub struct WorkspaceRecreated {
        pub workspace_id: String,
        pub worktree_path: String,
        pub from_oid: String,
        pub reason: String,
    }

    /// Pérdida de ownership durante un run: el run se aborta, no completa.
    #[derive(Debug, Serialize, Deserialize, Default)]
    pub struct WorkspaceLeaseLost {
        pub workspace_id: String,
        pub run_id: String,
        pub reason: String,
    }

    /// Toma de control de un lease de un proceso PROBADAMENTE muerto
    /// (pid ausente o reutilizado). Advisory: coordina Aeril, no aísla de
    /// procesos externos.
    #[derive(Debug, Serialize, Deserialize, Default)]
    pub struct WorkspaceLeaseTakenOver {
        pub workspace_id: String,
        pub previous_run_id: String,
        pub previous_pid: i64,
        pub age_ms: i64,
        pub reason: String,
    }

    /// Intención durable de integración, escrita ANTES de tocar la ref.
    #[derive(Debug, Serialize, Deserialize, Default)]
    pub struct IntegrationRequested {
        pub target_branch: String,
        pub source_oid: String,
        pub target_oid_expected: String,
    }

    #[derive(Debug, Serialize, Deserialize, Default)]
    pub struct IntegrationCompleted {
        pub target_branch: String,
        pub source_oid: String,
        pub target_oid_before: String,
        pub target_oid_after: String,
        /// `true` si lo resolvió la reconciliación al reabrir, no la operación.
        #[serde(default)]
        pub recovered: bool,
    }

    #[derive(Debug, Serialize, Deserialize, Default)]
    pub struct IntegrationBlocked {
        pub target_branch: String,
        pub source_oid: String,
        #[serde(default)]
        pub target_oid_current: Option<String>,
        pub reason: String,
    }

    #[derive(Debug, Serialize, Deserialize, Default)]
    pub struct ValidationRecorded {
        pub snapshot_id: String,
        pub oid: String,
        pub command: String,
        pub exit_code: i32,
        pub duration_ms: i64,
        /// Identidad estable de los bytes de stdout+stderr (FNV no criptográfico
        /// en v0), calculada incrementalmente sobre TODOS los bytes.
        pub output_hash: String,
        pub output_tail: String,
        #[serde(default)]
        pub stdout_bytes: u64,
        #[serde(default)]
        pub stderr_bytes: u64,
        #[serde(default)]
        pub stdout_hash: String,
        #[serde(default)]
        pub stderr_hash: String,
        /// El checkout aislado reproducía el tree exacto del snapshot antes de correr.
        #[serde(default)]
        pub tree_before_ok: bool,
        /// El contenido trackeado no cambió durante la validación.
        #[serde(default)]
        pub tree_after_unchanged: bool,
        #[serde(default)]
        pub checkout_id: String,
    }
}

// ── registros ────────────────────────────────────────────────────────────────
#[derive(Debug, Clone)]
pub struct EventRecord {
    pub id: String,
    pub thread_id: String,
    pub seq: i64,
    pub run_id: Option<String>,
    pub kind: String,
    pub actor: String,
    pub payload_version: i64,
    pub payload: serde_json::Value,
    pub created_ms: i64,
}

impl EventRecord {
    pub fn parse<T: DeserializeOwned>(&self) -> Result<T> {
        Ok(serde_json::from_value(self.payload.clone())?)
    }
}

fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventRecord> {
    let payload_text: String = row.get(7)?;
    let payload = serde_json::from_str(&payload_text).unwrap_or(serde_json::Value::Null);
    Ok(EventRecord {
        id: row.get(0)?,
        thread_id: row.get(1)?,
        seq: row.get(2)?,
        run_id: row.get(3)?,
        kind: row.get(4)?,
        actor: row.get(5)?,
        payload_version: row.get(6)?,
        payload,
        created_ms: row.get(8)?,
    })
}

const SELECT_COLS: &str = "SELECT id, thread_id, seq, run_id, kind, actor, payload_version, payload, created_ms FROM events";

// ── append ───────────────────────────────────────────────────────────────────
/// Punto de fallo determinista para tests de robustez (mismo espíritu que la
/// inyección de crashes): `AERIL_FAULT_APPEND=<kind>` (o `all`) hace fallar el
/// append de ese kind. NUNCA se activa sin la variable de entorno.
fn fault_injected(kind: &str) -> bool {
    match std::env::var("AERIL_FAULT_APPEND") {
        Ok(v) => v == "all" || v == kind,
        Err(_) => false,
    }
}

/// Append dentro de una transacción existente. Asigna `seq` con el contador
/// del thread; si el INSERT falla, la transacción completa se revierte.
pub fn append_tx(
    tx: &Transaction<'_>,
    thread_id: &str,
    run_id: Option<&str>,
    kind: &str,
    actor: &str,
    payload: &serde_json::Value,
    now: i64,
) -> Result<(String, i64)> {
    if fault_injected(kind) {
        return Err(Error::Other(format!(
            "escritura fallida inyectada para `{kind}` (AERIL_FAULT_APPEND)"
        )));
    }
    let seq: i64 = tx.query_row(
        "UPDATE threads SET next_seq = next_seq + 1, updated_ms = ?1 WHERE id = ?2 RETURNING next_seq - 1",
        params![now, thread_id],
        |row| row.get(0),
    )?;
    let id = crate::ids::new_id();
    tx.execute(
        "INSERT INTO events (id, thread_id, seq, run_id, kind, actor, payload_version, payload, created_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8)",
        params![id, thread_id, seq, run_id, kind, actor, payload.to_string(), now],
    )?;
    Ok((id, seq))
}

/// Append atómico en su propia transacción IMMEDIATE.
pub fn append(
    conn: &mut Connection,
    thread_id: &str,
    run_id: Option<&str>,
    kind: &str,
    actor: &str,
    payload: &serde_json::Value,
) -> Result<(String, i64)> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let out = append_tx(&tx, thread_id, run_id, kind, actor, payload, now_ms())?;
    tx.commit()?;
    Ok(out)
}

/// Variante tipada: serializa el struct y delega en [`append`].
pub fn append_typed<T: Serialize>(
    conn: &mut Connection,
    thread_id: &str,
    run_id: Option<&str>,
    kind: &str,
    actor: &str,
    payload: &T,
) -> Result<(String, i64)> {
    append(
        conn,
        thread_id,
        run_id,
        kind,
        actor,
        &serde_json::to_value(payload)?,
    )
}

// ── lecturas paginadas ───────────────────────────────────────────────────────
/// Últimos `limit` eventos de un thread, en orden ascendente de `seq`.
pub fn fetch_tail(conn: &Connection, thread_id: &str, limit: i64) -> Result<Vec<EventRecord>> {
    let mut stmt = conn.prepare(&format!(
        "{SELECT_COLS} WHERE thread_id = ?1 ORDER BY seq DESC LIMIT ?2"
    ))?;
    let rows = stmt.query_map(params![thread_id, limit], row_to_event)?;
    let mut out: Vec<EventRecord> = rows.collect::<rusqlite::Result<_>>()?;
    out.reverse();
    Ok(out)
}

/// Página hacia adelante desde `after_seq` (exclusivo), orden ascendente.
pub fn fetch_after(
    conn: &Connection,
    thread_id: &str,
    after_seq: i64,
    limit: i64,
) -> Result<Vec<EventRecord>> {
    let mut stmt = conn.prepare(&format!(
        "{SELECT_COLS} WHERE thread_id = ?1 AND seq > ?2 ORDER BY seq ASC LIMIT ?3"
    ))?;
    let rows = stmt.query_map(params![thread_id, after_seq, limit], row_to_event)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn fetch_by_id(
    conn: &Connection,
    thread_id: &str,
    event_id: &str,
) -> Result<Option<EventRecord>> {
    let mut stmt = conn.prepare(&format!("{SELECT_COLS} WHERE thread_id = ?1 AND id = ?2"))?;
    let mut rows = stmt.query_map(params![thread_id, event_id], row_to_event)?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

/// Eventos de un kind en un thread (orden ascendente). Acotado por el llamador.
pub fn fetch_by_kind(conn: &Connection, thread_id: &str, kind: &str) -> Result<Vec<EventRecord>> {
    let mut stmt = conn.prepare(&format!(
        "{SELECT_COLS} WHERE thread_id = ?1 AND kind = ?2 ORDER BY seq ASC"
    ))?;
    let rows = stmt.query_map(params![thread_id, kind], row_to_event)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn count(conn: &Connection, thread_id: &str) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM events WHERE thread_id = ?1",
        params![thread_id],
        |row| row.get(0),
    )?)
}
