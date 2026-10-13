//! Runs: intentos de ejecución. El run fija la base OID exacta, el workspace,
//! la rama y la sesión externa del motor; reanudar no cambia esa base.

use crate::error::{Error, Result};
use crate::ids::new_id;
use crate::store::events::{self, payload};
use crate::timeutil::now_ms;
use rusqlite::{Connection, TransactionBehavior, params};

#[derive(Debug, Clone)]
pub struct RunRow {
    pub id: String,
    pub thread_id: String,
    pub workspace_id: Option<String>,
    pub repo_path: Option<String>,
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
    pub base_oid: Option<String>,
    pub backend: String,
    pub backend_version: String,
    pub engine_session_id: Option<String>,
    pub state: String,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
}

const COLS: &str = "id, thread_id, workspace_id, repo_path, worktree_path, branch, base_oid, \
                    backend, backend_version, engine_session_id, state, started_ms, ended_ms";

fn row_to_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunRow> {
    Ok(RunRow {
        id: row.get(0)?,
        thread_id: row.get(1)?,
        workspace_id: row.get(2)?,
        repo_path: row.get(3)?,
        worktree_path: row.get(4)?,
        branch: row.get(5)?,
        base_oid: row.get(6)?,
        backend: row.get(7)?,
        backend_version: row.get(8)?,
        engine_session_id: row.get(9)?,
        state: row.get(10)?,
        started_ms: row.get(11)?,
        ended_ms: row.get(12)?,
    })
}

pub struct StartRunSpec<'a> {
    /// ID explícito del run (el lease de workspace se toma antes de crear la
    /// fila y debe referirse al mismo run).
    pub id: Option<&'a str>,
    pub thread_id: &'a str,
    pub backend: &'a str,
    pub backend_version: &'a str,
    pub goal: &'a str,
    pub workspace_id: &'a str,
    pub repo_path: &'a str,
    pub worktree_path: &'a str,
    pub branch: &'a str,
    pub base_oid: &'a str,
    pub resumed_session: Option<&'a str>,
}

/// Crea el run y su evento `run.started` atómicamente.
pub fn start(conn: &mut Connection, spec: &StartRunSpec<'_>) -> Result<RunRow> {
    let id = spec.id.map(str::to_string).unwrap_or_else(new_id);
    let now = now_ms();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO runs (id, thread_id, workspace_id, repo_path, worktree_path, branch, base_oid,
                           backend, backend_version, state, started_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'starting', ?10)",
        params![
            id,
            spec.thread_id,
            spec.workspace_id,
            spec.repo_path,
            spec.worktree_path,
            spec.branch,
            spec.base_oid,
            spec.backend,
            spec.backend_version,
            now
        ],
    )?;
    let payload = payload::RunStarted {
        backend: spec.backend.to_string(),
        goal: spec.goal.to_string(),
        workspace_id: spec.workspace_id.to_string(),
        repo_path: spec.repo_path.to_string(),
        worktree_path: spec.worktree_path.to_string(),
        branch: spec.branch.to_string(),
        base_oid: spec.base_oid.to_string(),
        resumed_session: spec.resumed_session.map(str::to_string),
    };
    events::append_tx(
        &tx,
        spec.thread_id,
        Some(&id),
        events::K_RUN_STARTED,
        "system",
        &serde_json::to_value(payload)?,
        now,
    )?;
    tx.commit()?;
    get(conn, spec.thread_id, &id)?
        .ok_or_else(|| Error::Other("run recién creado no legible".into()))
}

pub fn get(conn: &Connection, thread_id: &str, run_id: &str) -> Result<Option<RunRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLS} FROM runs WHERE thread_id = ?1 AND id = ?2"
    ))?;
    let mut rows = stmt.query_map(params![thread_id, run_id], row_to_run)?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

pub fn latest(conn: &Connection, thread_id: &str) -> Result<Option<RunRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLS} FROM runs WHERE thread_id = ?1 ORDER BY started_ms DESC, id DESC LIMIT 1"
    ))?;
    let mut rows = stmt.query_map(params![thread_id], row_to_run)?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

pub fn list(conn: &Connection, thread_id: &str) -> Result<Vec<RunRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLS} FROM runs WHERE thread_id = ?1 ORDER BY started_ms ASC, id ASC"
    ))?;
    let rows = stmt.query_map(params![thread_id], row_to_run)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn set_state(conn: &Connection, run_id: &str, state: &str) -> Result<()> {
    conn.execute(
        "UPDATE runs SET state = ?2 WHERE id = ?1",
        params![run_id, state],
    )?;
    Ok(())
}

/// Persiste la sesión externa del motor (identidad durable, no "contexto").
/// Registra la operación intentada y si el protocolo la aceptó; la
/// restauración efectiva del contexto previo NO es verificable con Hermes
/// (load de sesión desconocida responde OK sin historial) → `verified=false`.
pub fn set_engine_session(
    conn: &mut Connection,
    thread_id: &str,
    run_id: &str,
    engine_session_id: &str,
    resumed: bool,
    attempted: &str,
    accepted: bool,
) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "UPDATE runs SET engine_session_id = ?2 WHERE id = ?1",
        params![run_id, engine_session_id],
    )?;
    let payload = payload::EngineSession {
        engine_session_id: engine_session_id.to_string(),
        resumed,
        attempted: Some(attempted.to_string()),
        accepted: Some(accepted),
        verified: false,
    };
    events::append_tx(
        &tx,
        thread_id,
        Some(run_id),
        events::K_ENGINE_SESSION,
        "system",
        &serde_json::to_value(payload)?,
        now_ms(),
    )?;
    tx.commit()?;
    Ok(())
}

/// Cierra el run y registra `run.ended` en la misma transacción.
pub fn end(
    conn: &mut Connection,
    thread_id: &str,
    run_id: &str,
    state: &str,
    reason: Option<&str>,
    stop_reason: Option<&str>,
) -> Result<()> {
    let now = now_ms();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let changed = tx.execute(
        "UPDATE runs SET state = ?2, ended_ms = ?3 WHERE id = ?1",
        params![run_id, state, now],
    )?;
    if changed == 0 {
        return Err(Error::not_found(format!("run {run_id}")));
    }
    let payload = payload::RunEnded {
        state: state.to_string(),
        reason: reason.map(str::to_string),
        stop_reason: stop_reason.map(str::to_string),
    };
    events::append_tx(
        &tx,
        thread_id,
        Some(run_id),
        events::K_RUN_ENDED,
        "system",
        &serde_json::to_value(payload)?,
        now,
    )?;
    tx.commit()?;
    Ok(())
}
