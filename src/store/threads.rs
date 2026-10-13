//! Threads: conversaciones durables. Sobreviven a la muerte del proceso y a la
//! eliminación del worktree (no referencian rutas de trabajo, solo el repo).

use crate::error::{Error, Result};
use crate::ids::new_id;
use crate::store::events::{self, payload};
use crate::timeutil::now_ms;
use rusqlite::{Connection, TransactionBehavior, params};

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
