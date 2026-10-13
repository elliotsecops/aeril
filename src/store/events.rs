//! Eventos: hechos inmutables, ordenados por `(thread_id, seq)`.
//!
//! Reglas v0.4:
//! - `UNIQUE(thread_id, seq)`; la secuencia se asigna SOLO dentro de la
//!   transacción de append (`UPDATE ... RETURNING next_seq - 1`), nunca con
//!   `MAX(seq)+1` fuera de transacción.
//! - Payloads versionados (`payload_version`) con forma por `kind`; los
//!   productores usan los structs tipados de [`payload`].

use crate::error::Result;
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
        /// `true` si el motor aceptó reanudar una sesión existente
        /// (`session/load`); `false` si se abrió una sesión nueva con contexto
        /// restaurado por Aeril. No son equivalentes y se registran distinto.
        pub resumed: bool,
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

    #[derive(Debug, Serialize, Deserialize, Default)]
    pub struct IntegrationCompleted {
        pub target_branch: String,
        pub source_oid: String,
        pub target_oid_before: String,
        pub target_oid_after: String,
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
        pub output_hash: String,
        pub output_tail: String,
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
