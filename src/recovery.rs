//! S0 — Reconciliación durable de runs interrumpidos.
//!
//! Cuando Aeril reabre, un run en `starting`/`running` pudo quedar huérfano por
//! la muerte de su SUPERVISOR (el proceso de Aeril que lo lanzaba). Reglas:
//!
//! - **Nunca** se declara interrumpido un run mientras su supervisor esté vivo
//!   (evidencia: pid+starttime en `run.started`, con fallback al lease).
//! - Si el supervisor está probadamente muerto y quedó un backend vivo, se
//!   termina (grupo de procesos, starttime verificado) y recién entonces se
//!   marca `run.ended failed/interrupted`.
//! - Si la terminación del backend NO puede establecerse, se persiste
//!   `workspace.recovery_unsafe` y el workspace queda bloqueado para reuso
//!   automático (nunca se reusa un workspace que un proceso ajeno podría
//!   seguir modificando).
//! - Nunca se fabrica éxito, checkpoint ni contexto del motor: solo se
//!   reconcilia el run y se preservan los hechos existentes. Los checkpoints
//!   `prepared` legítimos se recuperan por el protocolo de Fase 0 (Gate B).
//! - Transaccional por run, repetible e idempotente: la verificación y el
//!   append ocurren en la MISMA transacción IMMEDIATE; si otro proceso ya
//!   resolvió el run, este no duplica el hecho.

use crate::error::Result;
use crate::store::Store;
use crate::store::events::{self, payload};
use crate::store::leases::{pid_alive, process_start_time};
use crate::store::runs::RunRow;
use crate::timeutil::now_ms;
use rusqlite::{Connection, OptionalExtension, params};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub enum RunOutcome {
    /// Supervisor muerto: run marcado `failed/interrupted` de forma durable.
    Interrupted {
        supervisor: String,
        backend: String,
        unsafe_state: bool,
    },
    /// El supervisor sigue vivo: no se toca nada.
    SupervisorAlive { supervisor: String },
    /// No hay evidencia duradera para decidir: se reporta y NO se reconcilia
    /// (conservador: mejor dejar el run intacto que declararlo muerto).
    SupervisorUnknown,
    /// Otro proceso ya resolvió el run (transacción perdedora).
    AlreadyResolved,
}

#[derive(Debug, Clone)]
pub struct RunRecovery {
    pub run_id: String,
    pub thread_id: String,
    pub workspace_id: Option<String>,
    pub outcome: RunOutcome,
}

/// Veredicto de reuso de un workspace (gate del Runner).
#[derive(Debug, Clone)]
pub enum ReuseVerdict {
    Safe,
    Blocked(String),
}

enum Supervisor {
    Alive(String),
    Dead(String),
    Unknown,
}

enum Backend {
    Dead(String),
    Terminated(String),
    Unsafe(String),
}

fn events_by_run_kind(
    conn: &Connection,
    thread_id: &str,
    run_id: &str,
    kind: &str,
) -> Result<Option<events::EventRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, thread_id, seq, run_id, kind, actor, payload_version, payload, created_ms
         FROM events WHERE thread_id = ?1 AND run_id = ?2 AND kind = ?3
         ORDER BY seq DESC LIMIT 1",
    )?;
    let mut rows = stmt.query_map(params![thread_id, run_id, kind], |row| {
        let payload_text: String = row.get(7)?;
        Ok(events::EventRecord {
            id: row.get(0)?,
            thread_id: row.get(1)?,
            seq: row.get(2)?,
            run_id: row.get(3)?,
            kind: row.get(4)?,
            actor: row.get(5)?,
            payload_version: row.get(6)?,
            payload: serde_json::from_str(&payload_text).unwrap_or(serde_json::Value::Null),
            created_ms: row.get(8)?,
        })
    })?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// Estado del proceso desde /proc (primer campo tras `comm`): `Z` = zombie,
/// `X` = muerto. Un zombie ya no puede ejecutar: cuenta como muerto.
pub fn process_dead_or_zombie(pid: i64) -> bool {
    if pid <= 0 || !pid_alive(pid) {
        return true;
    }
    matches!(process_state(pid), Some('Z') | Some('X'))
}

fn process_state(pid: i64) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    after_comm.split_whitespace().next()?.chars().next()
}

fn classify_process(pid: i64, pid_start: u64) -> (bool, bool) {
    // (alive, matches_identity) — zombie/muerto ⇒ no vivo.
    if process_dead_or_zombie(pid) {
        return (false, false);
    }
    match process_start_time(pid) {
        Some(st) if pid_start != 0 => (true, st == pid_start),
        _ => (true, true), // sin starttime: conservador, vivo
    }
}

fn supervisor_state(store: &Store, run: &RunRow) -> Result<Supervisor> {
    supervisor_state_conn(&store.conn, run)
}

fn supervisor_state_conn(conn: &Connection, run: &RunRow) -> Result<Supervisor> {
    // 1) Evidencia duradera del supervisor en `run.started`.
    if let Some(ev) = events_by_run_kind(conn, &run.thread_id, &run.id, events::K_RUN_STARTED)? {
        let p: payload::RunStarted = ev.parse()?;
        if let Some(pid) = p.supervisor_pid {
            let start = p.supervisor_pid_start.unwrap_or(0);
            let (alive, matches) = classify_process(pid, start);
            return Ok(if !alive {
                Supervisor::Dead(format!("supervisor muerto (pid {pid})"))
            } else if !matches {
                Supervisor::Dead(format!("supervisor: pid {pid} reutilizado"))
            } else {
                Supervisor::Alive(format!("supervisor vivo (pid {pid})"))
            });
        }
    }
    // 2) Fallback: el lease del run (si existe y es suyo).
    if let Some(ws) = &run.workspace_id
        && let Some(info) = conn
            .query_row(
                "SELECT run_id, pid, pid_start, renewed_ms, ttl_ms FROM leases WHERE workspace_id = ?1",
                params![ws],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)? as u64)),
            )
            .optional()?
            .filter(|(owner, _, _)| owner == &run.id)
        {
            let (pid, pid_start) = (info.1, info.2);
            let (alive, matches) = classify_process(pid, pid_start);
            return Ok(if !alive {
                Supervisor::Dead(format!("supervisor muerto (pid {pid}, evidencia del lease)"))
            } else if !matches {
                Supervisor::Dead(format!("supervisor: pid {pid} reutilizado (lease)"))
            } else {
                Supervisor::Alive(format!("supervisor vivo (pid {pid}, lease)"))
            });
        }
    Ok(Supervisor::Unknown)
}

/// Muerte del backend: muerto / terminado por la recuperación / inseguro.
fn backend_state(store: &Store, run: &RunRow) -> Result<Backend> {
    let Some(ev) = events_by_run_kind(
        &store.conn,
        &run.thread_id,
        &run.id,
        events::K_BACKEND_SPAWNED,
    )?
    else {
        return Ok(Backend::Unsafe(
            "sin evidencia del backend (no se puede verificar terminación)".into(),
        ));
    };
    let b: payload::BackendSpawned = ev.parse()?;
    let (alive, matches) = classify_process(b.pid, b.pid_start);
    if !alive {
        return Ok(Backend::Dead(format!("backend muerto (pid {})", b.pid)));
    }
    if !matches {
        return Ok(Backend::Dead(format!(
            "backend: pid {} reutilizado (no es nuestro proceso)",
            b.pid
        )));
    }
    // Nuestro backend sobrevive al supervisor: terminarlo y verificar.
    let pgid = process_pgrp(b.pid).unwrap_or(b.pid);
    unsafe {
        if pgid == b.pid {
            libc::kill(-(b.pid as i32), libc::SIGKILL); // grupo completo
        } else {
            libc::kill(b.pid as i32, libc::SIGKILL);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if process_dead_or_zombie(b.pid) {
            return Ok(Backend::Terminated(format!(
                "backend terminado durante la recuperación (pid {})",
                b.pid
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(Backend::Unsafe(format!(
        "backend sigue vivo tras SIGKILL (pid {}); terminación no establecida",
        b.pid
    )))
}

fn process_pgrp(pid: i64) -> Option<i64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // fields[0] = state → pgrp (campo 5) = fields[2].
    fields.get(2).and_then(|v| v.parse().ok())
}

/// Marca el run como interrumpido de forma atómica e idempotente.
/// Devuelve `true` si ESTE proceso persistió el hecho.
fn append_interruption_tx(
    store: &mut Store,
    run: &RunRow,
    supervisor: &str,
    backend: &Backend,
) -> Result<bool> {
    let backend_desc = match backend {
        Backend::Dead(d) | Backend::Terminated(d) | Backend::Unsafe(d) => d.clone(),
    };
    let tx = store
        .conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    // Re-verificación dentro de la transacción: otro proceso pudo resolverlo.
    let state: Option<String> = tx
        .query_row(
            "SELECT state FROM runs WHERE id = ?1",
            params![run.id],
            |r| r.get(0),
        )
        .optional()?;
    match state.as_deref() {
        Some("starting") | Some("running") => {}
        _ => {
            tx.rollback()?;
            return Ok(false);
        }
    }
    let ended_exists: i64 = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM events WHERE thread_id = ?1 AND run_id = ?2 AND kind = ?3)",
        params![run.thread_id, run.id, events::K_RUN_ENDED],
        |r| r.get(0),
    )?;
    if ended_exists == 1 {
        tx.rollback()?;
        return Ok(false);
    }
    let reason = format!("interrumpido: {supervisor}; {backend_desc}");
    tx.execute(
        "UPDATE runs SET state = 'failed', ended_ms = ?2 WHERE id = ?1",
        params![run.id, now_ms()],
    )?;
    events::append_tx(
        &tx,
        &run.thread_id,
        Some(&run.id),
        events::K_RUN_ENDED,
        "system",
        &serde_json::to_value(payload::RunEnded {
            state: "failed".into(),
            reason: Some(reason),
            stop_reason: None,
        })?,
        now_ms(),
    )?;
    if let (Backend::Unsafe(detail), Some(ws)) = (backend, &run.workspace_id) {
        // Estado inseguro/desconocido explícito y durable.
        let unsafe_ev = payload::WorkspaceRecoveryUnsafe {
            workspace_id: ws.clone(),
            run_id: run.id.clone(),
            backend_state: "unknown_or_alive".into(),
            detail: detail.clone(),
        };
        events::append_tx(
            &tx,
            &run.thread_id,
            Some(&run.id),
            events::K_WORKSPACE_RECOVERY_UNSAFE,
            "system",
            &serde_json::to_value(unsafe_ev)?,
            now_ms(),
        )?;
    }
    tx.commit()?;
    Ok(true)
}

/// Reconcilia todos los runs en `starting`/`running`. Repetible: cada llamada
/// re-evalúa la evidencia y solo persiste lo que aún no está resuelto.
pub fn reconcile_interrupted_runs(store: &mut Store) -> Result<Vec<RunRecovery>> {
    let stale: Vec<RunRow> = {
        let mut stmt = store.conn.prepare(
            "SELECT id, thread_id, workspace_id, repo_path, worktree_path, branch, base_oid,
                    backend, backend_version, engine_session_id, state, started_ms, ended_ms
             FROM runs WHERE state IN ('starting','running') ORDER BY started_ms ASC",
        )?;
        let rows = stmt.query_map([], |row| {
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
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };

    let mut out = Vec::new();
    for run in stale {
        let outcome = match supervisor_state(store, &run)? {
            Supervisor::Alive(desc) => RunOutcome::SupervisorAlive { supervisor: desc },
            Supervisor::Unknown => RunOutcome::SupervisorUnknown,
            Supervisor::Dead(desc) => {
                let backend = backend_state(store, &run)?;
                let unsafe_state = matches!(backend, Backend::Unsafe(_));
                let backend_desc = match &backend {
                    Backend::Dead(d) | Backend::Terminated(d) | Backend::Unsafe(d) => d.clone(),
                };
                if append_interruption_tx(store, &run, &desc, &backend)? {
                    RunOutcome::Interrupted {
                        supervisor: desc,
                        backend: backend_desc,
                        unsafe_state,
                    }
                } else {
                    RunOutcome::AlreadyResolved
                }
            }
        };
        out.push(RunRecovery {
            run_id: run.id.clone(),
            thread_id: run.thread_id.clone(),
            workspace_id: run.workspace_id.clone(),
            outcome,
        });
    }
    Ok(out)
}

/// Registro durable (una vez por workspace) de un reuso bloqueado por el gate.
pub fn note_reuse_blocked(store: &mut Store, workspace_id: &str, reason: &str) -> Result<()> {
    let exists: i64 = store.conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM events WHERE kind = ?1
              AND json_extract(payload, '$.workspace_id') = ?2
        )",
        params![events::K_WORKSPACE_REUSE_BLOCKED, workspace_id],
        |r| r.get(0),
    )?;
    if exists == 1 {
        return Ok(());
    }
    // El evento es de nivel workspace (no hay thread único natural): se ancla
    // al thread del último run del workspace, si existe.
    let thread_id: Option<String> = store
        .conn
        .query_row(
            "SELECT thread_id FROM runs WHERE workspace_id = ?1 ORDER BY started_ms DESC LIMIT 1",
            params![workspace_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(thread_id) = thread_id {
        events::append_typed(
            &mut store.conn,
            &thread_id,
            None,
            events::K_WORKSPACE_REUSE_BLOCKED,
            "system",
            &payload::WorkspaceReuseBlocked {
                workspace_id: workspace_id.to_string(),
                reason: reason.to_string(),
            },
        )?;
    }
    Ok(())
}

/// Gate de reuso (Runner): ¿es seguro reutilizar este workspace?
///
/// - sin runs → Safe;
/// - último run sin terminar (`starting`/`running`) → Blocked;
/// - último run `completed`/`cancelled` → Safe (el Runner terminó su backend
///   con kill de grupo verificado en el camino normal);
/// - último run `failed` → solo Safe con evidencia de backend muerto/reutilizado;
///   sin evidencia o con backend vivo → Blocked (conservador).
pub fn workspace_reuse_verdict(conn: &Connection, workspace_id: &str) -> Result<ReuseVerdict> {
    let run: Option<RunRow> = {
        let mut stmt = conn.prepare(
            "SELECT id, thread_id, workspace_id, repo_path, worktree_path, branch, base_oid,
                    backend, backend_version, engine_session_id, state, started_ms, ended_ms
             FROM runs WHERE workspace_id = ?1 ORDER BY started_ms DESC, id DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![workspace_id], |row| {
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
        })?;
        match rows.next() {
            Some(r) => Some(r?),
            None => None,
        }
    };
    let Some(run) = run else {
        return Ok(ReuseVerdict::Safe);
    };
    match run.state.as_str() {
        // Un run sin terminar con SUPERVISOR VIVO no se resuelve acá: el lease
        // del workspace produce el conflicto explícito (Busy). Sin supervisor
        // vivo (stale sin reconciliar o desconocido) no se reutiliza.
        "starting" | "running" => match supervisor_state_conn(conn, &run)? {
            Supervisor::Alive(_) => Ok(ReuseVerdict::Safe),
            Supervisor::Dead(desc) => Ok(ReuseVerdict::Blocked(format!(
                "run {} stale sin reconciliar ({desc})",
                run.id
            ))),
            Supervisor::Unknown => Ok(ReuseVerdict::Blocked(format!(
                "run {} sin evidencia de supervisor (conservador)",
                run.id
            ))),
        },
        "completed" | "cancelled" => Ok(ReuseVerdict::Safe),
        _ => {
            let ev = events_by_run_kind(conn, &run.thread_id, &run.id, events::K_BACKEND_SPAWNED)?;
            let Some(ev) = ev else {
                return Ok(ReuseVerdict::Blocked(
                    "sin evidencia de backend del último run (conservador)".into(),
                ));
            };
            let b: payload::BackendSpawned = ev.parse()?;
            let (alive, matches) = classify_process(b.pid, b.pid_start);
            if alive && matches {
                Ok(ReuseVerdict::Blocked(format!(
                    "el backend del último run sigue vivo (pid {})",
                    b.pid
                )))
            } else {
                Ok(ReuseVerdict::Safe)
            }
        }
    }
}
