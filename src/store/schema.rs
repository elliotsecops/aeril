//! Esquema SQLite v1 — tres tablas (`threads`/`runs`/`events`), por decisión v0.4.
//!
//! Una cuarta tabla de checkpoints NO se agrega en el ensayo: la recuperación se
//! resuelve con un anti-join sobre `events` usando el índice `(kind, thread_id)`
//! sobre el payload JSON. Si la limpieza de refs vuelve difícil esa consulta, la
//! cuarta tabla llega después con los mismos IDs (migración mecánica).

use crate::error::Result;
use rusqlite::Connection;

pub const SCHEMA_V1: &str = r#"
CREATE TABLE threads (
  id TEXT PRIMARY KEY NOT NULL,
  title TEXT NOT NULL DEFAULT '',
  repo_id TEXT NOT NULL,
  repo_path TEXT NOT NULL,
  state TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open','archived')),
  next_seq INTEGER NOT NULL DEFAULT 0 CHECK (next_seq >= 0),
  created_ms INTEGER NOT NULL,
  updated_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE runs (
  id TEXT PRIMARY KEY NOT NULL,
  thread_id TEXT NOT NULL REFERENCES threads(id),
  workspace_id TEXT,
  repo_path TEXT,
  worktree_path TEXT,
  branch TEXT,
  base_oid TEXT,
  backend TEXT NOT NULL,
  backend_version TEXT NOT NULL DEFAULT '',
  engine_session_id TEXT,
  state TEXT NOT NULL CHECK (state IN ('starting','running','completed','failed','cancelled')),
  started_ms INTEGER NOT NULL,
  ended_ms INTEGER,
  UNIQUE (thread_id, id)
) STRICT;

CREATE TABLE events (
  id TEXT PRIMARY KEY NOT NULL,
  thread_id TEXT NOT NULL REFERENCES threads(id),
  seq INTEGER NOT NULL CHECK (seq >= 0),
  run_id TEXT,
  kind TEXT NOT NULL,
  actor TEXT NOT NULL CHECK (actor IN ('human','agent','system')),
  payload_version INTEGER NOT NULL DEFAULT 1 CHECK (payload_version >= 1),
  payload TEXT NOT NULL CHECK (json_valid(payload)),
  created_ms INTEGER NOT NULL,
  UNIQUE (thread_id, seq),
  UNIQUE (thread_id, id),
  FOREIGN KEY (thread_id, run_id) REFERENCES runs(thread_id, id)
) STRICT;

CREATE INDEX idx_events_thread_seq ON events(thread_id, seq);
CREATE INDEX idx_events_kind_thread ON events(kind, thread_id);
CREATE INDEX idx_events_run ON events(run_id) WHERE run_id IS NOT NULL;
CREATE INDEX idx_runs_thread ON runs(thread_id, started_ms);
"#;

/// v4 (P2-S1 R1): vínculo durable run→perfil gestionado. Sin esta columna,
/// `unassign`/`--clear-threads` podían borrar la única evidencia de propiedad
/// de un run no-terminal (P1-1). NULL = run sin perfil gestionado (flag OFF).
pub const SCHEMA_V4: &str = r#"
ALTER TABLE runs ADD COLUMN profile TEXT;
"#;

pub const SCHEMA_VERSION: i64 = 4;

/// v2: lease de workspace activo (exclusión entre procesos de Aeril).
/// Tabla pequeña y dedicada: el `INSERT ... ON CONFLICT` dentro de una
/// transacción IMMEDIATE da la exclusión atómica; no hay lógica en JSON.
pub const SCHEMA_V2: &str = r#"
CREATE TABLE leases (
  workspace_id TEXT PRIMARY KEY NOT NULL,
  run_id TEXT NOT NULL,
  pid INTEGER NOT NULL,
  acquired_ms INTEGER NOT NULL,
  renewed_ms INTEGER NOT NULL,
  ttl_ms INTEGER NOT NULL
) STRICT;
"#;

/// v3: `pid_start` (starttime del proceso dueño): distingue un PID vivo real
/// de un PID reutilizado por otro proceso. Sin esto, un TTL vencido podía
/// transferir el workspace mientras el dueño seguía trabajando (RC2-B5).
pub const SCHEMA_V3: &str = r#"
ALTER TABLE leases ADD COLUMN pid_start INTEGER NOT NULL DEFAULT 0;
"#;

pub fn migrate(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if current >= SCHEMA_VERSION {
        return Ok(());
    }
    let tx = conn.transaction()?;
    if current < 1 {
        tx.execute_batch(SCHEMA_V1)?;
    }
    if current < 2 {
        tx.execute_batch(SCHEMA_V2)?;
    }
    if current < 3 {
        tx.execute_batch(SCHEMA_V3)?;
    }
    if current < 4 {
        tx.execute_batch(SCHEMA_V4)?;
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}
