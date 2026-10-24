//! PA0 Attention: proyección read-only del estado accionable de cada tarea.
//!
//! No es un segundo motor de tareas: deriva estado/acción de runs,
//! checkpoints, validación, review e integración ya durables. Nunca escribe
//! eventos, no mueve refs y no ejecuta comandos.
//!
//! Precedencia (de mayor a menor):
//! integridad → permiso → recuperación → integración bloqueada → revisión
//! requerida → validación fallida → validación requerida → listo para
//! integrar → en curso → fallido → terminado → sin run.

use crate::checkpoint;
use crate::error::{Error, Result};
use crate::integration::ValidationState;
use crate::review;
use crate::store::Store;
use crate::store::events::{self, payload};
use crate::store::{leases, runs, threads};
use rusqlite::OptionalExtension;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionStatus {
    IntegrityHold,
    PermissionPending,
    RecoveryRequired,
    IntegrationBlocked,
    ReviewRequired,
    ValidationFailed,
    ValidationRequired,
    ReadyToIntegrate,
    InProgress,
    Failed,
    Done,
    NoRun,
}

impl AttentionStatus {
    pub fn priority(self) -> u8 {
        match self {
            AttentionStatus::IntegrityHold => 0,
            AttentionStatus::PermissionPending => 1,
            AttentionStatus::RecoveryRequired => 2,
            AttentionStatus::IntegrationBlocked => 3,
            AttentionStatus::ReviewRequired => 4,
            AttentionStatus::ValidationFailed => 5,
            AttentionStatus::ValidationRequired => 6,
            AttentionStatus::ReadyToIntegrate => 7,
            AttentionStatus::InProgress => 8,
            AttentionStatus::Failed => 9,
            AttentionStatus::Done => 10,
            AttentionStatus::NoRun => 11,
        }
    }

    /// Describe el estado durable, no la acción. La acción vive en
    /// `next_action` para que ambos puedan mostrarse sin contradecirse.
    pub fn label(self) -> &'static str {
        match self {
            AttentionStatus::IntegrityHold => "estado inconsistente",
            AttentionStatus::PermissionPending => "permiso pendiente",
            AttentionStatus::RecoveryRequired => "recuperación pendiente",
            AttentionStatus::IntegrationBlocked => "integración bloqueada",
            AttentionStatus::ReviewRequired => "checkpoint listo",
            AttentionStatus::ValidationFailed => "validación fallida",
            AttentionStatus::ValidationRequired => "checkpoint revisado",
            AttentionStatus::ReadyToIntegrate => "validación aprobada",
            AttentionStatus::InProgress => "agente trabajando",
            AttentionStatus::Failed => "run fallido",
            AttentionStatus::Done => "sin trabajo pendiente",
            AttentionStatus::NoRun => "sin run",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskAttention {
    pub thread_id: String,
    pub thread_title: String,
    pub status: AttentionStatus,
    pub priority: u8,
    pub status_label: String,
    pub next_action: String,
    pub reason: String,
    pub run_state: Option<String>,
    pub run_id: Option<String>,
    pub snapshot_id: Option<String>,
    pub source_oid: Option<String>,
    /// Run que produjo el checkpoint mostrado. Permite distinguir el último
    /// resultado revisable de un run posterior que todavía trabaja.
    pub checkpoint_run_id: Option<String>,
    /// `true` si el checkpoint mostrado pertenece a un run anterior al último.
    pub historical_checkpoint: bool,
    pub validation: Option<ValidationState>,
    /// `pending` (comentarios abiertos), `required` (sin acknowledgement) o
    /// `clear`. Se calcula solo para el snapshot/OID exacto.
    pub review: Option<String>,
    pub integration: Option<String>,
}

impl TaskAttention {
    fn new(
        thread: &threads::ThreadRow,
        status: AttentionStatus,
        action: &str,
        reason: impl Into<String>,
    ) -> Self {
        TaskAttention {
            thread_id: thread.id.clone(),
            thread_title: thread.title.clone(),
            status,
            priority: status.priority(),
            status_label: status.label().to_string(),
            next_action: action.to_string(),
            reason: reason.into(),
            run_state: None,
            run_id: None,
            snapshot_id: None,
            source_oid: None,
            checkpoint_run_id: None,
            historical_checkpoint: false,
            validation: None,
            review: None,
            integration: None,
        }
    }

    fn with_run(mut self, run_state: Option<String>, run_id: Option<String>) -> Self {
        self.run_state = run_state;
        self.run_id = run_id;
        self
    }

    fn set(&mut self, status: AttentionStatus, action: &str, reason: impl Into<String>) {
        self.status = status;
        self.priority = status.priority();
        self.status_label = status.label().to_string();
        self.next_action = action.to_string();
        self.reason = reason.into();
        if self.historical_checkpoint {
            if !self.reason.is_empty() {
                self.reason.push_str(" · checkpoint de un run anterior");
            } else {
                self.reason = "checkpoint de un run anterior".into();
            }
        }
    }

    /// `true` si la tarea requiere una acción del humano para avanzar. Un run
    /// en curso y los estados terminales son observación, no acción.
    pub fn is_actionable(&self) -> bool {
        self.status.priority() <= AttentionStatus::ReadyToIntegrate.priority()
            && self.status != AttentionStatus::InProgress
    }
}

enum IntegrationEvent {
    Requested {
        seq: i64,
        source_oid: String,
    },
    Completed {
        seq: i64,
        source_oid: String,
    },
    Blocked {
        seq: i64,
        source_oid: String,
        reason: String,
    },
}

impl IntegrationEvent {
    fn seq(&self) -> i64 {
        match self {
            IntegrationEvent::Requested { seq, .. }
            | IntegrationEvent::Completed { seq, .. }
            | IntegrationEvent::Blocked { seq, .. } => *seq,
        }
    }

    fn source_oid(&self) -> &str {
        match self {
            IntegrationEvent::Requested { source_oid, .. }
            | IntegrationEvent::Completed { source_oid, .. }
            | IntegrationEvent::Blocked { source_oid, .. } => source_oid,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            IntegrationEvent::Requested { .. } => events::K_INTEGRATION_REQUESTED,
            IntegrationEvent::Completed { .. } => events::K_INTEGRATION_COMPLETED,
            IntegrationEvent::Blocked { .. } => events::K_INTEGRATION_BLOCKED,
        }
    }
}

fn latest_integration(store: &Store, thread_id: &str) -> Result<Option<IntegrationEvent>> {
    let row: Option<(i64, String, String)> = store
        .conn
        .query_row(
            "SELECT seq, kind, payload FROM events
              WHERE thread_id = ?1 AND kind IN (?2, ?3, ?4)
              ORDER BY seq DESC LIMIT 1",
            rusqlite::params![
                thread_id,
                events::K_INTEGRATION_REQUESTED,
                events::K_INTEGRATION_COMPLETED,
                events::K_INTEGRATION_BLOCKED
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((seq, kind, text)) = row else {
        return Ok(None);
    };
    let invalid = |e: serde_json::Error| {
        Error::integrity(format!(
            "evento {kind} ilegible en el thread {thread_id}: {e}"
        ))
    };
    let event = match kind.as_str() {
        events::K_INTEGRATION_REQUESTED => {
            let p: payload::IntegrationRequested = serde_json::from_str(&text).map_err(invalid)?;
            if p.source_oid.is_empty() || p.target_branch.is_empty() {
                return Err(Error::integrity(format!(
                    "evento {kind} sin identidad de integración en el thread {thread_id}"
                )));
            }
            IntegrationEvent::Requested {
                seq,
                source_oid: p.source_oid,
            }
        }
        events::K_INTEGRATION_COMPLETED => {
            let p: payload::IntegrationCompleted = serde_json::from_str(&text).map_err(invalid)?;
            if p.source_oid.is_empty() || p.target_branch.is_empty() {
                return Err(Error::integrity(format!(
                    "evento {kind} sin identidad de integración en el thread {thread_id}"
                )));
            }
            IntegrationEvent::Completed {
                seq,
                source_oid: p.source_oid,
            }
        }
        events::K_INTEGRATION_BLOCKED => {
            let p: payload::IntegrationBlocked = serde_json::from_str(&text).map_err(invalid)?;
            if p.source_oid.is_empty() || p.target_branch.is_empty() {
                return Err(Error::integrity(format!(
                    "evento {kind} sin identidad de integración en el thread {thread_id}"
                )));
            }
            IntegrationEvent::Blocked {
                seq,
                source_oid: p.source_oid,
                reason: if p.reason.trim().is_empty() {
                    "integración bloqueada sin detalle".into()
                } else {
                    p.reason
                },
            }
        }
        other => {
            return Err(Error::integrity(format!(
                "evento de integración desconocido {other:?} en el thread {thread_id}"
            )));
        }
    };
    Ok(Some(event))
}

/// Último `checkpoint.prepared` sin `ready` ni `failed` para su snapshot.
/// Es la misma condición que usa la recuperación durable.
fn latest_pending_prepared(store: &Store, thread_id: &str) -> Result<Option<i64>> {
    Ok(store
        .conn
        .query_row(
            "SELECT p.seq FROM events p
              WHERE p.thread_id = ?1 AND p.kind = ?2
                AND NOT EXISTS (
                    SELECT 1 FROM events d
                    WHERE d.thread_id = p.thread_id
                      AND d.kind IN (?3, ?4)
                      AND json_extract(d.payload, '$.snapshot_id') =
                          json_extract(p.payload, '$.snapshot_id')
                )
              ORDER BY p.seq DESC LIMIT 1",
            rusqlite::params![
                thread_id,
                events::K_CHECKPOINT_PREPARED,
                events::K_CHECKPOINT_READY,
                events::K_CHECKPOINT_FAILED
            ],
            |row| row.get(0),
        )
        .optional()?)
}

/// Pedido de permiso sin respuesta durable, acotado al run que lo emitió.
/// No importa si el run sigue activo: un pedido sin responder al cierre es
/// evidencia inconsistente, nunca un run limpio.
fn pending_permission(store: &Store, thread_id: &str, run_id: &str) -> Result<Option<String>> {
    let row: Option<(String,)> = store
        .conn
        .query_row(
            "SELECT p.payload FROM events p
              WHERE p.thread_id = ?1 AND p.run_id = ?2 AND p.kind = ?3
                AND NOT EXISTS (
                    SELECT 1 FROM events r
                    WHERE r.thread_id = p.thread_id AND r.kind = ?4
                      AND json_extract(r.payload, '$.request_id') =
                          json_extract(p.payload, '$.request_id')
                      AND r.seq > p.seq
                )
              ORDER BY p.seq DESC LIMIT 1",
            rusqlite::params![
                thread_id,
                run_id,
                events::K_PERMISSION_REQUESTED,
                events::K_PERMISSION_RESPONDED
            ],
            |row| Ok((row.get(0)?,)),
        )
        .optional()?;
    let Some((text,)) = row else {
        return Ok(None);
    };
    let p: payload::PermissionRequested = serde_json::from_str(&text).map_err(|e| {
        Error::integrity(format!(
            "permission.requested ilegible en el thread {thread_id}: {e}"
        ))
    })?;
    if p.request_id.is_empty() {
        return Err(Error::integrity(format!(
            "permission.requested sin request_id en el thread {thread_id}"
        )));
    }
    Ok(Some(if p.title.trim().is_empty() {
        p.request_id
    } else {
        p.title
    }))
}

/// Fallo durable al auditar una autorización. Sin auditoría no hay
/// autorización: la tarea no puede presentarse como limpia.
fn permission_audit_failure(
    store: &Store,
    thread_id: &str,
    run_id: &str,
) -> Result<Option<String>> {
    let text: Option<String> = store
        .conn
        .query_row(
            "SELECT payload FROM events
              WHERE thread_id = ?1 AND run_id = ?2 AND kind = ?3
              ORDER BY seq DESC LIMIT 1",
            rusqlite::params![thread_id, run_id, events::K_PERMISSION_AUDIT_FAILED],
            |row| row.get(0),
        )
        .optional()?;
    let Some(text) = text else {
        return Ok(None);
    };
    let p: payload::PermissionAuditFailed = serde_json::from_str(&text).map_err(|e| {
        Error::integrity(format!(
            "permission.audit_failed ilegible en el thread {thread_id}: {e}"
        ))
    })?;
    if p.stage.trim().is_empty() || p.detail.trim().is_empty() {
        return Err(Error::integrity(format!(
            "permission.audit_failed incompleto en el thread {thread_id}"
        )));
    }
    Ok(Some(format!("{}: {}", p.stage, p.detail)))
}

/// Estado de la última validación registrada para el snapshot exacto.
/// La identidad que importa es snapshot + OID: una validación de otro OID
/// nunca habilita la integración.
fn validation_for_snapshot(
    store: &Store,
    thread_id: &str,
    snapshot_id: &str,
    oid: &str,
) -> Result<(ValidationState, Option<String>)> {
    let text: Option<String> = store
        .conn
        .query_row(
            "SELECT payload FROM events
              WHERE thread_id = ?1 AND kind = ?2
                AND json_extract(payload, '$.snapshot_id') = ?3
              ORDER BY seq DESC LIMIT 1",
            rusqlite::params![thread_id, events::K_VALIDATION_RECORDED, snapshot_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(text) = text else {
        return Ok((ValidationState::Missing, None));
    };
    let record: payload::ValidationRecorded = match serde_json::from_str(&text) {
        Ok(r) => r,
        Err(e) => {
            return Ok((
                ValidationState::Invalid,
                Some(format!("validation.recorded ilegible: {e}")),
            ));
        }
    };
    if record.snapshot_id != snapshot_id
        || record.oid.is_empty()
        || record.command.trim().is_empty()
        || record.output_hash.is_empty()
    {
        return Ok((
            ValidationState::Invalid,
            Some("validation.recorded estructuralmente inválida".into()),
        ));
    }
    if record.oid != oid {
        return Ok((
            ValidationState::WrongOid,
            Some(format!(
                "la validación corresponde al OID {}, no al {}",
                record.oid, oid
            )),
        ));
    }
    if record.exit_code != 0 {
        return Ok((
            ValidationState::Failed,
            Some(format!(
                "la validación terminó con exit {}",
                record.exit_code
            )),
        ));
    }
    if !record.tree_before_ok {
        return Ok((
            ValidationState::Failed,
            Some("el checkout no reprodujo el tree del snapshot".into()),
        ));
    }
    if !record.tree_after_unchanged {
        return Ok((
            ValidationState::Failed,
            Some("el checkout cambió durante la validación".into()),
        ));
    }
    Ok((ValidationState::Passed, None))
}

fn integration_action(reason: &str) -> &'static str {
    if reason.contains("checkout") {
        "integrar desde un checkout limpio"
    } else if reason.contains("se movió") {
        "repetir preview"
    } else if reason.contains("no desciende") {
        "resolver divergencia"
    } else if reason.contains("no existe") {
        "crear rama destino"
    } else {
        "resolver bloqueo"
    }
}

/// Proyección de una tarea. Es read-only: no ejecuta comandos ni escribe.
pub fn project_thread(store: &Store, thread_id: &str) -> Result<TaskAttention> {
    let thread = threads::get(&store.conn, thread_id)?
        .ok_or_else(|| Error::not_found(format!("thread {thread_id}")))?;
    let run = runs::latest(&store.conn, thread_id)?;
    let run_state = run.as_ref().map(|r| r.state.clone());
    let run_id = run.as_ref().map(|r| r.id.clone());
    let run_active = matches!(run_state.as_deref(), Some("starting") | Some("running"));
    let run_live = match run.as_ref() {
        Some(run) if run_active => match run.workspace_id.as_deref() {
            Some(workspace_id) => matches!(
                leases::active(&store.conn, workspace_id)?,
                Some(lease) if lease.run_id == run.id
            ),
            // Sin workspace no hay lease que consultar (filas históricas).
            None => true,
        },
        _ => false,
    };

    let ready = checkpoint::latest_ready(store, thread_id)?;
    if let Some((_, ready)) = ready.as_ref() {
        let prepared = checkpoint::prepared_for_snapshot(store, thread_id, &ready.snapshot_id)?;
        let Some(prepared) = prepared else {
            let mut a = TaskAttention::new(
                &thread,
                AttentionStatus::IntegrityHold,
                "revisar estado",
                "checkpoint ready sin checkpoint.prepared verificable",
            )
            .with_run(run_state, run_id.clone());
            a.snapshot_id = Some(ready.snapshot_id.clone());
            a.source_oid = Some(ready.oid.clone());
            return Ok(a);
        };
        if prepared.snapshot_id != ready.snapshot_id
            || prepared.expected_oid != ready.oid
            || prepared.git_ref != ready.git_ref
        {
            let mut a = TaskAttention::new(
                &thread,
                AttentionStatus::IntegrityHold,
                "revisar estado",
                "checkpoint.prepared y checkpoint.ready se contradicen",
            )
            .with_run(run_state, run_id.clone());
            a.snapshot_id = Some(ready.snapshot_id.clone());
            a.source_oid = Some(ready.oid.clone());
            return Ok(a);
        }
        if checkpoint::has_failed_marker(store, thread_id, &ready.snapshot_id)? {
            let mut a = TaskAttention::new(
                &thread,
                AttentionStatus::IntegrityHold,
                "revisar estado",
                "checkpoint.ready y checkpoint.failed coexisten para el mismo snapshot",
            )
            .with_run(run_state, run_id.clone());
            a.snapshot_id = Some(ready.snapshot_id.clone());
            a.source_oid = Some(ready.oid.clone());
            return Ok(a);
        }
    }

    let pending_prepared = latest_pending_prepared(store, thread_id)?;
    let integration = latest_integration(store, thread_id)?;

    // 1) Integridad de permisos: un fallo de auditoría o un pedido sin
    //    respuesta al cierre no puede convertirse en un run limpio.
    if let Some(detail) = run_id
        .as_deref()
        .map(|rid| permission_audit_failure(store, thread_id, rid))
        .transpose()?
        .flatten()
    {
        let mut a = TaskAttention::new(
            &thread,
            AttentionStatus::IntegrityHold,
            "revisar estado",
            format!("fallo de auditoría de permiso: {detail}"),
        )
        .with_run(run_state, run_id.clone());
        if let Some((_, ready)) = ready.as_ref() {
            a.snapshot_id = Some(ready.snapshot_id.clone());
            a.source_oid = Some(ready.oid.clone());
        }
        return Ok(a);
    }

    let permission = match run_id.as_deref() {
        Some(rid) => pending_permission(store, thread_id, rid)?,
        None => None,
    };
    if let Some(title) = permission {
        let (status, action, reason) = if run_active && run_live {
            (
                AttentionStatus::PermissionPending,
                "responder permiso",
                format!("el agente espera autorización: {title}"),
            )
        } else if run_active {
            (
                AttentionStatus::RecoveryRequired,
                "aeril recover",
                format!("hay un permiso sin responder y el supervisor no está vivo: {title}"),
            )
        } else {
            (
                AttentionStatus::IntegrityHold,
                "revisar estado",
                format!("permission.requested sin respuesta durable al cierre del run: {title}"),
            )
        };
        let mut a =
            TaskAttention::new(&thread, status, action, reason).with_run(run_state, run_id.clone());
        if let Some((_, ready)) = ready.as_ref() {
            a.snapshot_id = Some(ready.snapshot_id.clone());
            a.source_oid = Some(ready.oid.clone());
        }
        return Ok(a);
    }

    // 2) Recuperación: prepared sin resolución y sin supervisor vivo. Un
    //    prepared durante un run vivo es una captura en curso, no un residuo.
    if pending_prepared.is_some() && !run_live {
        let mut a = TaskAttention::new(
            &thread,
            AttentionStatus::RecoveryRequired,
            "aeril recover",
            "hay un checkpoint preparado sin resolver",
        )
        .with_run(run_state, run_id.clone());
        if let Some((_, ready)) = ready.as_ref() {
            a.snapshot_id = Some(ready.snapshot_id.clone());
            a.source_oid = Some(ready.oid.clone());
        }
        return Ok(a);
    }

    // 3) Run activo sin supervisor vivo: la reconciliación al reabrir puede
    //    cerrarlo, pero la proyección debe señalar recuperación, no "en curso".
    if run_active && !run_live {
        return Ok(TaskAttention::new(
            &thread,
            AttentionStatus::RecoveryRequired,
            "aeril recover",
            "el run figura activo pero no tiene un supervisor vivo",
        )
        .with_run(run_state, run_id.clone()));
    }

    // 4) Checkpoint listo: integración, revisión y validación exactas.
    if let Some((ready_ev, ready)) = ready {
        let mut a = TaskAttention::new(
            &thread,
            AttentionStatus::ReviewRequired,
            "revisar cambios",
            String::new(),
        )
        .with_run(run_state.clone(), run_id.clone());
        a.snapshot_id = Some(ready.snapshot_id.clone());
        a.source_oid = Some(ready.oid.clone());
        a.checkpoint_run_id = ready_ev.run_id.clone();
        a.historical_checkpoint = match (&a.checkpoint_run_id, &run_id) {
            (Some(checkpoint_run), Some(latest_run)) => checkpoint_run != latest_run,
            (None, Some(_)) => true,
            _ => false,
        };

        if let Some(event) = integration.as_ref()
            && event.seq() > ready_ev.seq
            && event.source_oid() == ready.oid
        {
            a.integration = Some(event.kind().to_string());
            match event {
                IntegrationEvent::Completed { .. } => {
                    // El checkpoint está integrado, pero un run posterior
                    // puede seguir trabajando o haber fallado. "Integrado" no
                    // debe ocultar el estado más reciente de la tarea.
                    match run_state.as_deref() {
                        Some("failed") => a.set(
                            AttentionStatus::Failed,
                            "revisar fallo",
                            "el checkpoint anterior está integrado, pero el último run falló",
                        ),
                        Some("cancelled") => a.set(
                            AttentionStatus::Failed,
                            "revisar cancelación",
                            "el checkpoint anterior está integrado, pero el último run fue cancelado",
                        ),
                        Some("starting") | Some("running") => a.set(
                            AttentionStatus::InProgress,
                            "observar progreso",
                            "el checkpoint anterior está integrado; hay un run nuevo en curso",
                        ),
                        _ => {
                            a.set(
                                AttentionStatus::Done,
                                "ninguna",
                                "el checkpoint ya está integrado en la rama destino",
                            );
                            a.status_label = "integrado".into();
                        }
                    }
                    return Ok(a);
                }
                IntegrationEvent::Requested { .. } => {
                    a.set(
                        AttentionStatus::RecoveryRequired,
                        "aeril recover",
                        "integración solicitada sin resolución durable",
                    );
                    return Ok(a);
                }
                IntegrationEvent::Blocked { reason, .. } => {
                    a.set(
                        AttentionStatus::IntegrationBlocked,
                        integration_action(reason),
                        format!("integración bloqueada: {reason}"),
                    );
                    return Ok(a);
                }
            }
        }

        let comments = review::open_comment_ids_for_snapshot(
            store,
            thread_id,
            &ready.snapshot_id,
            &ready.oid,
        )?;
        let ack = review::acknowledged(store, thread_id, &ready.snapshot_id, &ready.oid)?;
        let (validation, validation_reason) =
            validation_for_snapshot(store, thread_id, &ready.snapshot_id, &ready.oid)?;
        a.validation = Some(validation);

        if !comments.is_empty() {
            a.review = Some("pending".into());
            a.set(
                AttentionStatus::ReviewRequired,
                "resolver comentarios",
                format!(
                    "{} comentario(s) sin resolver en el snapshot",
                    comments.len()
                ),
            );
            return Ok(a);
        }
        if !ack {
            a.review = Some("required".into());
            a.set(
                AttentionStatus::ReviewRequired,
                "marcar revisado",
                "revisión explícita pendiente: los comentarios resueltos no equivalen a revisado",
            );
            return Ok(a);
        }
        a.review = Some("clear".into());

        match validation {
            ValidationState::Missing => {
                a.set(
                    AttentionStatus::ValidationRequired,
                    "ejecutar validación",
                    "no hay validación para el snapshot y OID exactos",
                );
            }
            ValidationState::Failed | ValidationState::WrongOid | ValidationState::Invalid => {
                a.validation = Some(validation);
                a.set(
                    AttentionStatus::ValidationFailed,
                    "corregir y revalidar",
                    validation_reason.unwrap_or_else(|| "validación no utilizable".into()),
                );
            }
            ValidationState::Passed => {
                a.set(
                    AttentionStatus::ReadyToIntegrate,
                    "abrir preview de integración",
                    "revisión y validación verificadas; la elegibilidad Git la decide el preview",
                );
            }
        }
        return Ok(a);
    }

    // 5) Sin checkpoint: el estado accionable lo define el último run.
    match run_state.as_deref() {
        Some("failed") => Ok(TaskAttention::new(
            &thread,
            AttentionStatus::Failed,
            "revisar fallo",
            "el último run falló",
        )
        .with_run(run_state, run_id.clone())),
        Some("cancelled") => Ok(TaskAttention::new(
            &thread,
            AttentionStatus::Failed,
            "revisar cancelación",
            "el último run fue cancelado",
        )
        .with_run(run_state, run_id.clone())),
        Some("completed") => Ok(TaskAttention::new(
            &thread,
            AttentionStatus::Done,
            "ninguna",
            "el run terminó sin un checkpoint pendiente",
        )
        .with_run(run_state, run_id.clone())),
        Some(_) if run_active => Ok(TaskAttention::new(
            &thread,
            AttentionStatus::InProgress,
            "observar progreso",
            "el agente está trabajando; todavía no hay checkpoint",
        )
        .with_run(run_state, run_id.clone())),
        Some(_) => Ok(TaskAttention::new(
            &thread,
            AttentionStatus::NoRun,
            "iniciar run",
            "todavía no hay trabajo revisable",
        )
        .with_run(run_state, run_id.clone())),
        None => Ok(TaskAttention::new(
            &thread,
            AttentionStatus::NoRun,
            "iniciar run",
            "la tarea todavía no tiene runs",
        )),
    }
}

/// Proyecta todas las tareas visibles. Un error de evidencia no oculta la
/// tarea: se muestra como `integrity_hold` para que el humano investigue.
pub fn project_all(store: &Store, limit: i64) -> Result<Vec<TaskAttention>> {
    let mut out = Vec::new();
    for thread in threads::list_projection(&store.conn, limit)? {
        match project_thread(store, &thread.id) {
            Ok(a) => out.push(a),
            Err(e) => out.push(TaskAttention::new(
                &thread,
                AttentionStatus::IntegrityHold,
                "revisar estado",
                format!("no se pudo proyectar la tarea: {e}"),
            )),
        }
    }
    Ok(out)
}
