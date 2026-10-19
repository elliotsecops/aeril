//! Threads: conversaciones durables. Sobreviven a la muerte del proceso y a la
//! eliminación del worktree (no referencian rutas de trabajo, solo el repo).

use crate::error::{Error, Result};
use crate::ids::new_id;
use crate::store::events::{self, payload};
use crate::timeutil::now_ms;
use rusqlite::{Connection, TransactionBehavior, params};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct ThreadRow {
    pub id: String,
    pub title: String,
    pub repo_id: String,
    pub repo_path: String,
    pub state: String,
    pub created_ms: i64,
    pub updated_ms: i64,
}

/// Proyección de lectura para la lista de tareas de la TUI.
///
/// Incluye solo metadatos del último run: no carga eventos ni payloads.
/// El orden coincide con `list_projection` para que los índices de la lista
/// sigan siendo los mismos que los del resto de la TUI.
#[derive(Debug, Clone)]
pub struct ThreadRunSummary {
    pub thread_id: String,
    pub run_count: i64,
    pub latest_state: Option<String>,
    pub latest_branch: Option<String>,
    pub started_ms: Option<i64>,
    pub ended_ms: Option<i64>,
}

fn row_to_thread(row: &rusqlite::Row<'_>) -> rusqlite::Result<ThreadRow> {
    Ok(ThreadRow {
        id: row.get(0)?,
        title: row.get(1)?,
        repo_id: row.get(2)?,
        repo_path: row.get(3)?,
        state: row.get(4)?,
        created_ms: row.get(5)?,
        updated_ms: row.get(6)?,
    })
}

const COLS: &str = "id, title, repo_id, repo_path, state, created_ms, updated_ms";

/// Crea el thread y su evento `thread.created` en la misma transacción.
pub fn create(
    conn: &mut Connection,
    title: &str,
    repo_path: &str,
    repo_id: &str,
) -> Result<ThreadRow> {
    let id = new_id();
    let now = now_ms();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO threads (id, title, repo_id, repo_path, state, next_seq, created_ms, updated_ms)
         VALUES (?1, ?2, ?3, ?4, 'open', 0, ?5, ?5)",
        params![id, title, repo_id, repo_path, now],
    )?;
    let payload = payload::ThreadCreated {
        title: title.to_string(),
        repo_path: repo_path.to_string(),
        repo_id: repo_id.to_string(),
    };
    events::append_tx(
        &tx,
        &id,
        None,
        events::K_THREAD_CREATED,
        "system",
        &serde_json::to_value(payload)?,
        now,
    )?;
    tx.commit()?;
    get(conn, &id)?.ok_or_else(|| Error::Other("thread recién creado no legible".into()))
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<ThreadRow>> {
    let mut stmt = conn.prepare(&format!("SELECT {COLS} FROM threads WHERE id = ?1"))?;
    let mut rows = stmt.query_map(params![id], row_to_thread)?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

/// Proyección de arranque: solo metadatos del thread, sin contar eventos ni
/// cargar payloads. Es la consulta del presupuesto de arranque.
pub fn list_projection(conn: &Connection, limit: i64) -> Result<Vec<ThreadRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLS} FROM threads ORDER BY updated_ms DESC LIMIT ?1"
    ))?;
    let rows = stmt.query_map(params![limit], row_to_thread)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Resumen liviano del último run por thread para la lista de tareas.
///
/// Una sola consulta evita N+1 consultas al refrescar la TUI. Los threads sin
/// runs siguen apareciendo con `latest_state = None`.
pub fn run_summaries(conn: &Connection, limit: i64) -> Result<HashMap<String, ThreadRunSummary>> {
    let mut stmt = conn.prepare(
        "SELECT t.id,
                (SELECT COUNT(*) FROM runs r WHERE r.thread_id = t.id),
                r.state, r.branch, r.started_ms, r.ended_ms
         FROM threads t
         LEFT JOIN runs r ON r.id = (
             SELECT r2.id FROM runs r2
             WHERE r2.thread_id = t.id
             ORDER BY r2.started_ms DESC, r2.id DESC
             LIMIT 1
         )
         ORDER BY t.updated_ms DESC, t.id DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit], |row| {
        Ok(ThreadRunSummary {
            thread_id: row.get(0)?,
            run_count: row.get(1)?,
            latest_state: row.get(2)?,
            latest_branch: row.get(3)?,
            started_ms: row.get(4)?,
            ended_ms: row.get(5)?,
        })
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let row = row?;
        out.insert(row.thread_id.clone(), row);
    }
    Ok(out)
}

pub fn set_repo_path(conn: &Connection, id: &str, repo_path: &str) -> Result<()> {
    let changed = conn.execute(
        "UPDATE threads SET repo_path = ?2, updated_ms = ?3 WHERE id = ?1",
        params![id, repo_path, now_ms()],
    )?;
    if changed == 0 {
        return Err(Error::not_found(format!("thread {id}")));
    }
    Ok(())
}
