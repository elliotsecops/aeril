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
use std::time::Duration;

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
        conn.busy_timeout(Duration::from_secs(5))?;
        // journal_mode devuelve una fila ("wal"): query_row, no pragma_update.
        let _mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
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
    /// Migración v2→v3: un store de la era RC2 (sin `pid_start`) se actualiza
    /// sin perder datos y la columna queda disponible.
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
        }
        let store = Store::open(&path).unwrap();
        let v: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 3);
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
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

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
