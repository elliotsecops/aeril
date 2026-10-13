//! Store transaccional de Aeril (SQLite, WAL, synchronous=FULL).
//!
//! Un store de Aeril, no de los backends. Conexión por hilo/proceso: SQLite en
//! modo WAL permite lectores concurrentes mientras el agente escribe.

pub mod events;
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
