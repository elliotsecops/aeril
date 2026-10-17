//! S0 — Reconciliación durable de runs interrumpidos.
//!
//! Cuando Aeril reabre, un run en `starting`/`running` pudo quedar huérfano por
//! la muerte de su SUPERVISOR (el proceso de Aeril que lo lanzaba). Reglas:
//!
//! - **Nunca** se declara interrumpido un run mientras su supervisor esté vivo
//!   (evidencia: pid+starttime en `run.started`, con fallback al lease).
//! - Si el supervisor está probadamente muerto y quedó un backend vivo
//!   (identidad VERIFICADA por pid+starttime), se termina su grupo de procesos
//!   y se verifica que no queden miembros vivos; recién entonces se marca
//!   `run.ended failed/interrupted`.
//! - Identidad no verificable (`pid_start=0`, `/proc` ilegible), PID
//!   reutilizado, o grupo homónimo con miembros sin atribución ⇒ NUNCA se
//!   envía señal y se persiste `workspace.recovery_unsafe`: el workspace queda
//!   bloqueado para reuso automático (conservador, preferimos preservar el
//!   trabajo del usuario antes que reclamar el workspace).
//! - Un `workspace.recovery_unsafe` registrado no se olvida después: el gate de
//!   reuso lo consulta y jamás lo degrada por ver un PID ahora muerto.
//! - Nunca se fabrica éxito, checkpoint ni contexto del motor: solo se
//!   reconcilia el run y se preservan los hechos existentes. Los checkpoints
//!   `prepared` legítimos se recuperan por el protocolo de Fase 0 (Gate B).
//! - Transaccional por run, repetible e idempotente: la verificación y el
//!   append ocurren en la MISMA transacción IMMEDIATE; si otro proceso ya
//!   resolvió el run, este no duplica el hecho.
//!
//! **Frontera de soporte:** Aeril puede inspeccionar y supervisar el GRUPO DE
//! PROCESOS que él mismo creó (el backend se lanza como líder de grupo), pero
//! NO es un sandbox del sistema operativo. Procesos que escapan por su cuenta
//! del grupo/sesión registrados (p. ej. `setsid`) quedan fuera de la garantía
//! de atribución: no se reclaman, no se señalan, y su existencia no se detecta.

use crate::error::Result;
use crate::store::Store;
use crate::store::events::{self, payload};
use crate::store::leases::pid_alive;
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

/// Identidad de un PID frente a una evidencia durable `(pid, starttime)`.
/// Enum explícito para no confundir "sin evidencia" con "evidencia que
/// coincide" (D2 de la auditoría S0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PidIdentity {
    /// El PID no existe (proceso muerto y reapeado; o PID inválido).
    Absent,
    /// El proceso existe y su starttime coincide con la evidencia.
    Ours { zombie: bool },
    /// El proceso existe pero con OTRO starttime: PID reutilizado.
    Reused,
    /// El proceso existe pero la identidad no puede establecerse (sin
    /// `pid_start`, sin `/proc`, stat ilegible). NUNCA autoriza señales.
    Unknown,
}

fn identify_pid(pid: i64, recorded_start: u64) -> PidIdentity {
    if pid <= 0 || !pid_alive(pid) {
        return PidIdentity::Absent;
    }
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return PidIdentity::Unknown; // existe (kill(0)) pero no inspeccionable
    };
    let Some(after_comm) = stat.rsplit_once(')').map(|(_, a)| a) else {
        return PidIdentity::Unknown;
    };
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    let Some(state) = fields.first().and_then(|s| s.chars().next()) else {
        return PidIdentity::Unknown;
    };
    let Some(actual) = fields.get(19).and_then(|v| v.parse::<u64>().ok()) else {
        return PidIdentity::Unknown;
    };
    if recorded_start == 0 {
        return PidIdentity::Unknown; // sin evidencia de identidad
    }
    if actual == recorded_start {
        PidIdentity::Ours {
            zombie: matches!(state, 'Z' | 'X'),
        }
    } else {
        PidIdentity::Reused
    }
}

/// Miembros VIVOS (no zombie) del grupo de procesos `pgid`, según `/proc`.
///
/// `None` = no se pudo inspeccionar (desconocido: JAMÁS se interpreta como
/// grupo vacío). Un zombie no puede escribir y no cuenta como miembro vivo.
pub fn live_group_members(pgid: i64) -> Option<Vec<i64>> {
    if pgid <= 0 {
        return None;
    }
    let entries = std::fs::read_dir("/proc").ok()?;
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i64>().ok())
        else {
            continue;
        };
        let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(s) => s,
            // El proceso terminó mientras se escaneaba: no es miembro vivo.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            // No inspeccionable ⇒ desconocido (conservador).
            Err(_) => return None,
        };
        let after_comm = stat.rsplit_once(')').map(|(_, a)| a)?;
        let fields: Vec<&str> = after_comm.split_whitespace().collect();
        let state = fields.first().and_then(|s| s.chars().next())?;
        if matches!(state, 'Z' | 'X') {
            continue;
        }
        let pgrp = fields.get(2).and_then(|v| v.parse::<i64>().ok());
        if pgrp == Some(pgid) {
            out.push(pid);
        }
    }
    Some(out)
}

/// Termina el backend con identidad VERIFICADA (`PidIdentity::Ours`) y su
/// grupo de procesos, con plazo acotado. No declara éxito si algún miembro
/// vivo sobrevive o si el grupo no puede inspeccionarse.
fn terminate_verified_backend(b: &payload::BackendSpawned) -> Result<Backend> {
    let leader_group = process_pgrp(b.pid);
    let own_group = leader_group == Some(b.pid);
    unsafe {
        if own_group {
            libc::kill(-(b.pid as i32), libc::SIGKILL); // grupo completo
        } else {
            libc::kill(b.pid as i32, libc::SIGKILL);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let leader_gone = process_dead_or_zombie(b.pid);
        let members_done = match leader_group {
            Some(pgid) => match live_group_members(pgid) {
                Some(members) => members.is_empty(),
                None => false, // desconocido: no se declara terminado
            },
            None => false, // grupo no determinable: terminación no verificable
        };
        if leader_gone && members_done {
            return Ok(Backend::Terminated(format!(
                "backend terminado durante la recuperación (pid {})",
                b.pid
            )));
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(Backend::Unsafe(format!(
        "backend o miembros de su grupo siguen vivos (pid {}); terminación no establecida",
        b.pid
    )))
}

/// Mapea la identidad del supervisor a la decisión conservadora: jamás se
/// declara muerto a un supervisor sin identidad verificable.
fn supervisor_from_identity(identity: PidIdentity, pid: i64, via_lease: bool) -> Supervisor {
    let suffix = if via_lease { " (lease)" } else { "" };
    match identity {
        PidIdentity::Absent => Supervisor::Dead(format!("supervisor muerto (pid {pid}){suffix}")),
        PidIdentity::Ours { zombie: true } => {
            Supervisor::Dead(format!("supervisor muerto (zombie, pid {pid}){suffix}"))
        }
        PidIdentity::Ours { zombie: false } => {
            Supervisor::Alive(format!("supervisor vivo (pid {pid}){suffix}"))
        }
        PidIdentity::Reused => {
            Supervisor::Dead(format!("supervisor: pid {pid} reutilizado{suffix}"))
        }
        PidIdentity::Unknown => Supervisor::Alive(format!(
            "supervisor sin identidad verificable (pid {pid}){suffix}; se asume vivo (conservador)"
        )),
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
            return Ok(supervisor_from_identity(
                identify_pid(pid, start),
                pid,
                false,
            ));
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
            return Ok(supervisor_from_identity(
                identify_pid(pid, pid_start),
                pid,
                true,
            ));
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
    match identify_pid(b.pid, b.pid_start) {
        // Identidad verificada (vivo o zombie): el grupo es atribuible.
        PidIdentity::Ours { .. } => {
            // P2-S1 R1 (P1-4): con perfiles gestionados habilitados y un run
            // asociado a un perfil, la reconciliación NO termina al backend
            // sobreviviente: lo declara inseguro y la admisión de perfiles
            // rechaza hasta que el OPERADOR termine el proceso (identidad
            // verificada) y reintente. Con el flag OFF el comportamiento
            // certificado de Fase 1 queda intacto.
            if crate::profile::enabled() && run.profile.is_some() {
                return Ok(Backend::Unsafe(format!(
                    "perfil gestionado `{}`: terminación automática deshabilitada (P1-4);                      terminá el proceso huérfano verificando identidad y reconciliá",
                    run.profile.as_deref().unwrap_or("?")
                )));
            }
            terminate_verified_backend(&b)
        }
        // PID inexistente: sin miembros vivos del grupo homónimo no queda
        // escritor; con miembros, la atribución no es verificable (posible
        // reutilización de PGID) ⇒ unsafe SIN enviar señal.
        PidIdentity::Absent => match live_group_members(b.pid) {
            Some(members) if members.is_empty() => {
                Ok(Backend::Dead(format!("backend muerto (pid {})", b.pid)))
            }
            Some(members) => Ok(Backend::Unsafe(format!(
                "el pid {} no existe pero su grupo homónimo tiene {} miembro(s) vivo(s) sin atribución verificable; no se envía señal",
                b.pid,
                members.len()
            ))),
            None => Ok(Backend::Unsafe(format!(
                "no se pudo inspeccionar /proc para verificar el grupo del backend (pid {})",
                b.pid
            ))),
        },
        // PID reutilizado: JAMÁS se señala (podría ser un proceso ajeno).
        PidIdentity::Reused => match live_group_members(b.pid) {
            Some(members) if members.is_empty() => Ok(Backend::Dead(format!(
                "backend: pid {} reutilizado (no es nuestro proceso)",
                b.pid
            ))),
            _ => Ok(Backend::Unsafe(format!(
                "pid {} reutilizado y grupo homónimo no verificable; no se envía señal",
                b.pid
            ))),
        },
        // Identidad no establecida (pid_start=0 o /proc ilegible): JAMÁS se
        // señala y el resultado es explícitamente inseguro.
        PidIdentity::Unknown => Ok(Backend::Unsafe(format!(
            "identidad del backend no establecida (pid {}, pid_start {}); no se envía señal",
            b.pid, b.pid_start
        ))),
    }
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
                    backend, backend_version, engine_session_id, state, started_ms, ended_ms,
                    profile
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
                profile: row.get(13)?,
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

// ── S2: vista de estado de runs (SOLO LECTURA) ──────────────────────────────
//
// Reutiliza los clasificadores de liveness de S0/RC2 (supervisor_state_conn y
// leases::owner_alive): no hay un segundo clasificador de PIDs. Una consulta
// jamás reconcilia ni muta: solo lee.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LivenessView {
    Alive,
    Dead,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct SupervisorView {
    pub liveness: LivenessView,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub enum LeaseView {
    Owned {
        pid: i64,
        owner_alive: bool,
    },
    Missing,
    HeldByOther {
        holder_run_id: String,
        pid: i64,
        owner_alive: bool,
    },
}

/// Liveness del supervisor del run según la evidencia durable (run.started) y,
/// como fallback, el lease del propio run. Sin evidencia ⇒ Unknown (nunca se
/// declara muerto ni vivo sin prueba).
pub fn supervisor_view(conn: &Connection, run: &RunRow) -> Result<SupervisorView> {
    let (liveness, detail) = match supervisor_state_conn(conn, run)? {
        Supervisor::Alive(d) => (LivenessView::Alive, d),
        Supervisor::Dead(d) => (LivenessView::Dead, d),
        Supervisor::Unknown => (
            LivenessView::Unknown,
            "sin evidencia de supervisor (ni run.started ni lease propio)".into(),
        ),
    };
    Ok(SupervisorView { liveness, detail })
}

/// Ownership del lease del workspace del run, con liveness del dueño cuando
/// el lease pertenece a OTRO run (requisito S2: no presentarlo como válido).
pub fn lease_view(conn: &Connection, run: &RunRow) -> Result<LeaseView> {
    let Some(ws) = &run.workspace_id else {
        return Ok(LeaseView::Missing);
    };
    match crate::store::leases::get(conn, ws)? {
        None => Ok(LeaseView::Missing),
        Some(info) if info.run_id == run.id => Ok(LeaseView::Owned {
            pid: info.pid,
            owner_alive: crate::store::leases::owner_alive(&info),
        }),
        Some(info) => {
            let owner_alive = crate::store::leases::owner_alive(&info);
            Ok(LeaseView::HeldByOther {
                holder_run_id: info.run_id,
                pid: info.pid,
                owner_alive,
            })
        }
    }
}

/// Estado operativo efectivo: no se reporta `running` solo porque SQLite diga
/// `running` — se exige supervisor VIVO. Muerto ⇒ interrupted_candidate;
/// desconocido ⇒ unknown. Los estados terminales se conservan tal cual.
pub fn effective_status(run: &RunRow, supervisor: &SupervisorView) -> &'static str {
    match run.state.as_str() {
        "completed" => "completed",
        "failed" => "failed",
        "cancelled" => "cancelled",
        "starting" => match supervisor.liveness {
            LivenessView::Alive => "starting",
            LivenessView::Dead => "interrupted_candidate",
            LivenessView::Unknown => "unknown",
        },
        "running" => match supervisor.liveness {
            LivenessView::Alive => "running",
            LivenessView::Dead => "interrupted_candidate",
            LivenessView::Unknown => "unknown",
        },
        _ => "unknown",
    }
}

/// Nota durable `workspace.recovery_unsafe` más reciente de un workspace
/// (S4): si existe, el workspace NUNCA se reutiliza automáticamente (D4).
/// Devuelve el payload para poder mostrar el detalle en diagnósticos.
pub fn workspace_recovery_unsafe(
    conn: &Connection,
    workspace_id: &str,
) -> Result<Option<events::payload::WorkspaceRecoveryUnsafe>> {
    let mut stmt = conn.prepare(
        "SELECT payload FROM events WHERE kind = ?1
           AND json_extract(payload, '$.workspace_id') = ?2
         ORDER BY created_ms DESC, id DESC LIMIT 1",
    )?;
    let mut rows = stmt.query_map(
        params![events::K_WORKSPACE_RECOVERY_UNSAFE, workspace_id],
        |row| row.get::<_, String>(0),
    )?;
    match rows.next() {
        Some(payload_text) => {
            let payload_text = payload_text?;
            Ok(Some(serde_json::from_str(&payload_text).map_err(|e| {
                crate::error::Error::integrity(format!("payload de {workspace_id} ilegible: {e}"))
            })?))
        }
        None => Ok(None),
    }
}

/// Gate de reuso (Runner): ¿es seguro reutilizar este workspace?
///
/// - estado `workspace.recovery_unsafe` previo → Blocked SIEMPRE: la
///   incertidumbre registrada no se olvida por ver un PID ahora muerto (D4);
/// - sin runs → Safe;
/// - último run sin terminar (`starting`/`running`) → Blocked;
/// - último run `completed`/`cancelled` → Safe (el Runner terminó su backend
///   con kill de grupo verificado en el camino normal);
/// - último run `failed` → solo Safe con identidad verificada y sin miembros
///   vivos del grupo del backend; identidad desconocida/reutilizada con
///   miembros, o `/proc` no inspeccionable → Blocked (conservador).
pub fn workspace_reuse_verdict(conn: &Connection, workspace_id: &str) -> Result<ReuseVerdict> {
    // D4: un estado inseguro previo no se olvida jamás en la decisión de reuso.
    let unsafe_exists: i64 = conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM events WHERE kind = ?1
              AND json_extract(payload, '$.workspace_id') = ?2)",
        params![events::K_WORKSPACE_RECOVERY_UNSAFE, workspace_id],
        |r| r.get(0),
    )?;
    if unsafe_exists == 1 {
        return Ok(ReuseVerdict::Blocked(
            "recuperación previa marcó el workspace como inseguro (backend con terminación no verificada); \
             no se reutiliza automáticamente"
                .into(),
        ));
    }
    let run: Option<RunRow> = {
        let mut stmt = conn.prepare(
            "SELECT id, thread_id, workspace_id, repo_path, worktree_path, branch, base_oid,
                    backend, backend_version, engine_session_id, state, started_ms, ended_ms,
                    profile
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
                profile: row.get(13)?,
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
            match identify_pid(b.pid, b.pid_start) {
                PidIdentity::Ours { zombie: false } => Ok(ReuseVerdict::Blocked(format!(
                    "el backend del último run sigue vivo (pid {})",
                    b.pid
                ))),
                // Identidad no verificable: conservador, sin señales.
                PidIdentity::Unknown => Ok(ReuseVerdict::Blocked(format!(
                    "identidad del backend del último run no verificable (pid {}, pid_start {}); conservador",
                    b.pid, b.pid_start
                ))),
                // Líder muerto o reutilizado: solo Safe si el grupo homónimo
                // no tiene miembros vivos (posible escritor atribuible).
                PidIdentity::Ours { zombie: true } | PidIdentity::Absent | PidIdentity::Reused => {
                    match live_group_members(b.pid) {
                        Some(members) if members.is_empty() => Ok(ReuseVerdict::Safe),
                        Some(members) => Ok(ReuseVerdict::Blocked(format!(
                            "el grupo del backend del último run tiene {} miembro(s) vivo(s) (pid {}); conservador",
                            members.len(),
                            b.pid
                        ))),
                        None => Ok(ReuseVerdict::Blocked(format!(
                            "no se pudo inspeccionar /proc para el grupo del backend (pid {}); conservador",
                            b.pid
                        ))),
                    }
                }
            }
        }
    }
}
