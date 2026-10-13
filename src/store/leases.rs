//! Leases de workspace: exclusión entre PROCESOS DE AERIL sobre un mismo
//! workspace (dos runs no editan el mismo checkout a la vez).
//!
//! Ámbito declarado: es coordinación advisory de Aeril. No aísla de procesos
//! externos que editen los mismos archivos con los permisos del usuario
//! (MANIFIESTO §8: un worktree no es un sandbox).
//!
//! Recuperación de leases obsoletos: si el PID dueño murió o el TTL venció,
//! el siguiente run toma el control y lo registra como evento.

use crate::error::Result;
use crate::timeutil::now_ms;
use rusqlite::{Connection, TransactionBehavior, params};

/// TTL por defecto del lease (se renueva cada ~1/4 del TTL).
pub const DEFAULT_TTL_MS: i64 = 60_000;

#[derive(Debug, Clone)]
pub struct LeaseInfo {
    pub run_id: String,
    pub pid: i64,
    pub acquired_ms: i64,
    pub renewed_ms: i64,
    pub ttl_ms: i64,
}

#[derive(Debug)]
pub struct TakenOver {
    pub previous_run_id: String,
    pub previous_pid: i64,
    pub age_ms: i64,
    pub reason: &'static str,
}

#[derive(Debug)]
pub enum AcquireOutcome {
    Acquired {
        took_over: Option<TakenOver>,
    },
    Busy {
        run_id: String,
        pid: i64,
        age_ms: i64,
    },
}

/// `kill(pid, 0)`: vivo si no falla o si falla por permisos (EPERM).
pub fn pid_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn read_lease(conn: &Connection, workspace_id: &str) -> Result<Option<LeaseInfo>> {
    let mut stmt = conn.prepare(
        "SELECT run_id, pid, acquired_ms, renewed_ms, ttl_ms FROM leases WHERE workspace_id = ?1",
    )?;
    let mut rows = stmt.query_map(params![workspace_id], |row| {
        Ok(LeaseInfo {
            run_id: row.get(0)?,
            pid: row.get(1)?,
            acquired_ms: row.get(2)?,
            renewed_ms: row.get(3)?,
            ttl_ms: row.get(4)?,
        })
    })?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// ¿Hay un run ACTIVO sobre el workspace? (PID vivo o TTL sin vencer).
pub fn active(conn: &Connection, workspace_id: &str) -> Result<Option<LeaseInfo>> {
    let now = now_ms();
    match read_lease(conn, workspace_id)? {
        Some(info)
            if !info.run_id.is_empty()
                && (pid_alive(info.pid) || info.renewed_ms + info.ttl_ms > now) =>
        {
            Ok(Some(info))
        }
        _ => Ok(None),
    }
}

/// Intenta adquirir el lease de un workspace, atómicamente:
/// - sin lease o con dueño muerto/TTL vencido → adquiere (con toma de control);
/// - con dueño vivo y TTL vigente → `Busy` (nunca se pisa un run activo).
pub fn try_acquire(
    conn: &mut Connection,
    workspace_id: &str,
    run_id: &str,
    pid: i64,
    ttl_ms: i64,
) -> Result<AcquireOutcome> {
    let now = now_ms();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let existing = read_lease(&tx, workspace_id)?;
    let mut took_over = None;
    if let Some(info) = existing
        && info.run_id != run_id
    {
        let alive = pid_alive(info.pid);
        let expired = info.renewed_ms + info.ttl_ms <= now;
        if alive && !expired {
            tx.rollback()?;
            return Ok(AcquireOutcome::Busy {
                run_id: info.run_id,
                pid: info.pid,
                age_ms: now - info.renewed_ms,
            });
        }
        took_over = Some(TakenOver {
            previous_run_id: info.run_id,
            previous_pid: info.pid,
            age_ms: (now - info.renewed_ms).max(0),
            reason: if !alive { "pid muerto" } else { "ttl vencido" },
        });
    }
    tx.execute(
        "INSERT INTO leases (workspace_id, run_id, pid, acquired_ms, renewed_ms, ttl_ms)
         VALUES (?1, ?2, ?3, ?4, ?4, ?5)
         ON CONFLICT(workspace_id) DO UPDATE SET
           run_id = excluded.run_id,
           pid = excluded.pid,
           acquired_ms = excluded.acquired_ms,
           renewed_ms = excluded.renewed_ms,
           ttl_ms = excluded.ttl_ms",
        params![workspace_id, run_id, pid, now, ttl_ms],
    )?;
    tx.commit()?;
    Ok(AcquireOutcome::Acquired { took_over })
}

/// Renueva el lease propio. `false` si ya no es nuestro (perdido).
pub fn renew(conn: &Connection, workspace_id: &str, run_id: &str) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE leases SET renewed_ms = ?3 WHERE workspace_id = ?1 AND run_id = ?2",
        params![workspace_id, run_id, now_ms()],
    )?;
    Ok(changed == 1)
}

/// Libera el lease (solo si sigue siendo nuestro).
pub fn release(conn: &Connection, workspace_id: &str, run_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM leases WHERE workspace_id = ?1 AND run_id = ?2",
        params![workspace_id, run_id],
    )?;
    Ok(())
}
