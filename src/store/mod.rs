//! Store transaccional de Aeril (SQLite, WAL, synchronous=FULL).
//!
//! Un store de Aeril, no de los backends. Conexión por hilo/proceso: SQLite en
//! modo WAL permite lectores concurrentes mientras el agente escribe.

pub mod events;
pub mod leases;
pub mod runs;
pub mod schema;
pub mod threads;

use crate::error::Result;
use rusqlite::Connection;
use std::path::Path;
use std::time::{Duration, Instant};

const STORE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const WAL_BUSY_RETRY_SLICE: Duration = Duration::from_millis(25);
const WAL_BUSY_RETRY_INITIAL_BACKOFF: Duration = Duration::from_millis(1);
const WAL_BUSY_RETRY_MAX_BACKOFF: Duration = Duration::from_millis(25);

fn is_retryable_sqlite_lock(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy) | Some(rusqlite::ErrorCode::DatabaseLocked)
    )
}

/// Switch this connection to WAL, retrying only transient lock errors.
/// SQLite may skip its busy handler when a lock upgrade could deadlock; a
/// failed journal_mode change is retry-safe and leaves the prior mode intact.
/// The retries share the existing five-second open deadline and use short
/// busy-handler slices so one SQLITE_BUSY does not consume a fresh timeout.
fn journal_mode_wal(conn: &Connection) -> rusqlite::Result<String> {
    let deadline = Instant::now() + STORE_BUSY_TIMEOUT;
    let mut backoff = WAL_BUSY_RETRY_INITIAL_BACKOFF;
    let mut last_busy: Option<rusqlite::Error> = None;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(last_busy.unwrap_or_else(|| {
                rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                    Some("journal_mode=WAL retry deadline elapsed".into()),
                )
            }));
        }

        conn.busy_timeout(remaining.min(WAL_BUSY_RETRY_SLICE))?;
        match conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0)) {
            Ok(mode) => {
                conn.busy_timeout(STORE_BUSY_TIMEOUT)?;
                return Ok(mode);
            }
            Err(error) if is_retryable_sqlite_lock(&error) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(error);
                }
                last_busy = Some(error);
                std::thread::sleep(backoff.min(remaining));
                backoff = (backoff * 2).min(WAL_BUSY_RETRY_MAX_BACKOFF);
            }
            Err(error) => return Err(error),
        }
    }
}

pub struct Store {
    pub conn: Connection,
}

impl Store {
    /// Abre (o crea) el store y aplica las migraciones pendientes.
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(STORE_BUSY_TIMEOUT)?;
        // journal_mode devuelve una fila ("wal"): query_row, no pragma_update.
        let mode = journal_mode_wal(&conn)?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(crate::error::Error::unsupported(format!(
                "SQLite journal_mode=WAL returned {mode:?}; expected `wal`"
            )));
        }
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Caché de páginas acotada (2 MiB) — parte del presupuesto de RSS idle.
        conn.pragma_update(None, "cache_size", -2048)?;
        schema::migrate(&mut conn)?;
        Ok(Store { conn })
    }

    pub fn open_in_memory() -> Result<Store> {
        let mut conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        schema::migrate(&mut conn)?;
        Ok(Store { conn })
    }
}

#[cfg(test)]
mod tests {
    /// Migración v2→vN: un store de la era RC2 (sin `pid_start`, sin
    /// `runs.profile`) se actualiza sin perder datos y las columnas quedan
    /// disponibles (R1: v4 agrega el vínculo durable run→perfil).
    #[test]
    fn migracion_v2_a_v3_conserva_datos() {
        let dir = std::env::temp_dir().join(format!("aeril-mig-{}", crate::ids::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("aeril.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(super::schema::SCHEMA_V1).unwrap();
            conn.execute_batch(super::schema::SCHEMA_V2).unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
            conn.execute(
                "INSERT INTO threads (id, title, repo_id, repo_path, state, next_seq, created_ms, updated_ms)
                 VALUES ('t1', 'pre-v3', 'r1', '/tmp', 'open', 0, 1, 1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO leases (workspace_id, run_id, pid, acquired_ms, renewed_ms, ttl_ms)
                 VALUES ('ws1', 'run1', 123, 1, 1, 60000)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO runs (id, thread_id, backend, state, started_ms)
                 VALUES ('r-old', 't1', 'mock', 'completed', 1)",
                [],
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let v: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, super::schema::SCHEMA_VERSION);
        let title: String = store
            .conn
            .query_row("SELECT title FROM threads WHERE id = 't1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(title, "pre-v3");
        let pid_start: i64 = store
            .conn
            .query_row(
                "SELECT pid_start FROM leases WHERE workspace_id = 'ws1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pid_start, 0, "fila legacy queda en 0 (liveness por PID)");
        // v4: la columna profile existe y las filas legacy quedan NULL.
        let legacy_profile: Option<String> = store
            .conn
            .query_row("SELECT profile FROM runs WHERE id = 'r-old'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(legacy_profile, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    #[test]
    fn run_summaries_returns_latest_run_without_n_plus_one() {
        let mut store = Store::open_in_memory().unwrap();
        let idle = threads::create(&mut store.conn, "idle", "/tmp/repo", "r1").unwrap();
        let busy = threads::create(&mut store.conn, "busy", "/tmp/repo", "r1").unwrap();
        store
            .conn
            .execute(
                "INSERT INTO runs (id, thread_id, branch, backend, state, started_ms, ended_ms)
                 VALUES ('run-old', ?1, 'old', 'mock', 'failed', 10, 20)",
                rusqlite::params![busy.id],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO runs (id, thread_id, branch, backend, state, started_ms, ended_ms)
                 VALUES ('run-new', ?1, 'new', 'mock', 'completed', 30, 45)",
                rusqlite::params![busy.id],
            )
            .unwrap();

        let summaries = threads::run_summaries(&store.conn, 10).unwrap();
        let idle_summary = summaries.get(&idle.id).unwrap();
        assert_eq!(idle_summary.run_count, 0);
        assert_eq!(idle_summary.latest_run_id, None);
        assert_eq!(idle_summary.latest_state, None);
        let busy_summary = summaries.get(&busy.id).unwrap();
        assert_eq!(busy_summary.run_count, 2);
        assert_eq!(busy_summary.latest_run_id.as_deref(), Some("run-new"));
        assert_eq!(busy_summary.latest_state.as_deref(), Some("completed"));
        assert_eq!(busy_summary.latest_branch.as_deref(), Some("new"));
        assert_eq!(busy_summary.started_ms, Some(30));
        assert_eq!(busy_summary.ended_ms, Some(45));
    }

    #[test]
    fn fk_habilitadas_y_esquema() {
        let store = Store::open_in_memory().unwrap();
        // FK activas: insertar un evento de un thread inexistente debe fallar.
        let payload = serde_json::json!({});
        let mut conn = store.conn;
        let err = events::append(
            &mut conn,
            "thread-inexistente",
            None,
            events::K_THREAD_CREATED,
            "system",
            &payload,
        );
        assert!(err.is_err(), "la FK debe impedir eventos huérfanos");
    }
}
