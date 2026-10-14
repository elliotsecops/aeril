//! Leases de workspace: exclusión entre PROCESOS DE AERIL sobre un mismo
//! workspace (dos runs no editan el mismo checkout a la vez).
//!
//! Invariante RC2-B5: **un TTL vencido NUNCA transfiere ownership mientras el
//! proceso dueño esté vivo**. La transferencia automática solo ocurre cuando
//! el dueño está probadamente muerto: PID inexistente, o PID reutilizado por
//! otro proceso (comparando el `starttime` del kernel, `/proc/<pid>/stat`).
//! Un dueño vivo con heartbeat vencido (p. ej. detenido con SIGSTOP) queda
//! `Busy` y requiere recuperación explícita (`aeril lease release --force`).
//!
//! Ámbito declarado: es coordinación advisory de Aeril. No aísla de procesos
//! externos que editen los mismos archivos con los permisos del usuario
//! (MANIFIESTO §8: un worktree no es un sandbox).

use crate::error::{Error, Result};
use crate::timeutil::now_ms;
use rusqlite::{Connection, TransactionBehavior, params};

/// TTL por defecto del lease (se renueva cada ~1/4 del TTL).
pub const DEFAULT_TTL_MS: i64 = 60_000;

/// TTL configurable para tests (AERIL_LEASE_TTL_MS).
pub fn ttl_ms() -> i64 {
    std::env::var("AERIL_LEASE_TTL_MS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_TTL_MS)
}

/// Intervalo de renovación configurable (AERIL_LEASE_RENEW_SECS), mínimo 1 s.
pub fn renew_interval_secs() -> u64 {
    std::env::var("AERIL_LEASE_RENEW_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v >= 1)
        .unwrap_or(15)
}

#[derive(Debug, Clone)]
pub struct LeaseInfo {
    pub run_id: String,
    pub pid: i64,
    pub pid_start: u64,
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
    Acquired { took_over: Option<TakenOver> },
    Busy { info: LeaseInfo, age_ms: i64 },
}

/// `kill(pid, 0)`: existe un proceso con ese PID (vivo o zombie).
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

/// `starttime` del proceso (campo 22 de `/proc/<pid>/stat`): inmune a la
/// reutilización de PIDs dentro del entorno Linux soportado.
pub fn process_start_time(pid: i64) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // "pid (comm) state ppid ... starttime ..." — comm puede contener ')'.
    let after_comm = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // fields[0] = state (campo 3) → starttime (campo 22) = fields[19].
    fields.get(19).and_then(|v| v.parse().ok())
}

/// ¿El dueño del lease está VIVO? Un PID que existe pero cuyo starttime no
/// coincide es un PID reutilizado: el dueño original está muerto.
/// Filas legacy (pid_start = 0) caen a la liveness simple por PID.
pub fn owner_alive(info: &LeaseInfo) -> bool {
    if !pid_alive(info.pid) {
        return false;
    }
    match process_start_time(info.pid) {
        Some(start) if info.pid_start != 0 => start == info.pid_start,
        _ => true,
    }
}

fn read_lease(conn: &Connection, workspace_id: &str) -> Result<Option<LeaseInfo>> {
    let mut stmt = conn.prepare(
        "SELECT run_id, pid, pid_start, acquired_ms, renewed_ms, ttl_ms
         FROM leases WHERE workspace_id = ?1",
    )?;
    let mut rows = stmt.query_map(params![workspace_id], |row| {
        Ok(LeaseInfo {
            run_id: row.get(0)?,
            pid: row.get(1)?,
            pid_start: row.get::<_, i64>(2)? as u64,
            acquired_ms: row.get(3)?,
            renewed_ms: row.get(4)?,
            ttl_ms: row.get(5)?,
        })
    })?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// ¿Hay un dueño VIVO del workspace? (para bloquear `worktree rm`).
/// Un lease de un proceso muerto no bloquea: recuperación conservadora pero
/// la muerte debe estar probada (PID muerto o reutilizado).
pub fn active(conn: &Connection, workspace_id: &str) -> Result<Option<LeaseInfo>> {
    match read_lease(conn, workspace_id)? {
        Some(info) if owner_alive(&info) => Ok(Some(info)),
        _ => Ok(None),
    }
}

/// Intenta adquirir el lease de un workspace, atómicamente:
/// - sin lease → adquiere;
/// - con dueño VIVO → `Busy` SIEMPRE (aunque el TTL esté vencido: solo una
///   recuperación explícita puede transferirlo);
/// - con dueño probadamente muerto (PID ausente o reutilizado) → adquiere y
///   lo registra como toma de control.
pub fn try_acquire(
    conn: &mut Connection,
    workspace_id: &str,
    run_id: &str,
    pid: i64,
    ttl: i64,
) -> Result<AcquireOutcome> {
    let now = now_ms();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let existing = read_lease(&tx, workspace_id)?;
    let mut took_over = None;
    if let Some(info) = existing
        && info.run_id != run_id
    {
        if owner_alive(&info) {
            tx.rollback()?;
            return Ok(AcquireOutcome::Busy {
                age_ms: now - info.renewed_ms,
                info,
            });
        }
        let pid_exists = pid_alive(info.pid);
        took_over = Some(TakenOver {
            previous_run_id: info.run_id,
            previous_pid: info.pid,
            age_ms: (now - info.renewed_ms).max(0),
            reason: if !pid_exists {
                "pid muerto"
            } else {
                "pid reusado (starttime distinto)"
            },
        });
    }
    let pid_start = process_start_time(pid).unwrap_or(0) as i64;
    tx.execute(
        "INSERT INTO leases (workspace_id, run_id, pid, pid_start, acquired_ms, renewed_ms, ttl_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)
         ON CONFLICT(workspace_id) DO UPDATE SET
           run_id = excluded.run_id,
           pid = excluded.pid,
           pid_start = excluded.pid_start,
           acquired_ms = excluded.acquired_ms,
           renewed_ms = excluded.renewed_ms,
           ttl_ms = excluded.ttl_ms",
        params![workspace_id, run_id, pid, pid_start, now, ttl],
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

/// Lista todos los leases con su estado de liveness.
pub fn list(conn: &Connection) -> Result<Vec<(String, LeaseInfo)>> {
    let mut stmt = conn.prepare(
        "SELECT workspace_id, run_id, pid, pid_start, acquired_ms, renewed_ms, ttl_ms FROM leases ORDER BY renewed_ms DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            LeaseInfo {
                run_id: row.get(1)?,
                pid: row.get(2)?,
                pid_start: row.get::<_, i64>(3)? as u64,
                acquired_ms: row.get(4)?,
                renewed_ms: row.get(5)?,
                ttl_ms: row.get(6)?,
            },
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Recuperación EXPLÍCITA de un lease (dueno vivo bloqueando indefinidamente,
/// p. ej. SIGSTOP). Sin `force`, solo libera si el dueño está muerto.
/// Devuelve la descripción del lease liberado.
pub fn release_external(
    conn: &mut Connection,
    workspace_id: &str,
    force: bool,
) -> Result<LeaseInfo> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let info = read_lease(&tx, workspace_id)?
        .ok_or_else(|| Error::not_found(format!("no hay lease para {workspace_id}")))?;
    if !force && owner_alive(&info) {
        tx.rollback()?;
        return Err(Error::conflict(format!(
            "el lease de {workspace_id} pertenece a un proceso VIVO (run {}, pid {}); usá --force solo si confirmaste que debe detenerse",
            info.run_id, info.pid
        )));
    }
    tx.execute(
        "DELETE FROM leases WHERE workspace_id = ?1",
        params![workspace_id],
    )?;
    tx.commit()?;
    Ok(info)
}
