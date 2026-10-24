//! Preflight de integración asistida: observación read-only + apply guardado.
//!
//! El motor Git sigue siendo `git::integrate::fast_forward` y la operación
//! durable sigue siendo `session::integrate`. Este módulo solo decide si la
//! operación puede ofrecerse, con identidades exactas y evidencia verificable.

use crate::checkpoint;
use crate::error::{Error, Result};
use crate::git;
use crate::ids::checkpoint_ref;
use crate::review;
use crate::session;
use crate::store::Store;
use crate::store::events::{self, payload};
use crate::store::threads;
use git2::Oid;
use rusqlite::OptionalExtension;
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationStatus {
    EligibleFastForward,
    AlreadyIntegrated,
    TargetMissing,
    TargetInvalid,
    TargetCheckedOut,
    Diverged,
    SnapshotNotReady,
    SnapshotInvalid,
    ValidationRequired,
    ValidationFailed,
    ReviewPending,
    IntegrityError,
    StaleTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationState {
    Passed,
    Missing,
    Failed,
    WrongOid,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Clear,
    Required,
    Pending,
    Invalid,
}

#[derive(Debug, Clone, Serialize)]
pub struct IntegrationPreview {
    pub thread_id: String,
    pub thread_title: String,
    pub snapshot_id: String,
    pub source_oid: Option<String>,
    pub target_branch: String,
    pub target_oid: Option<String>,
    pub operation: String,
    pub eligible: bool,
    pub status: IntegrationStatus,
    pub reason: Option<String>,
    pub validation: ValidationState,
    pub validation_command: Option<String>,
    pub validation_oid: Option<String>,
    pub validation_exit_code: Option<i32>,
    pub validation_instruction: String,
    pub review: ReviewState,
    pub unresolved_comments: usize,
    pub files_changed: usize,
    pub additions: usize,
    pub deletions: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct AppliedIntegration {
    pub target_branch: String,
    pub source_oid: String,
    pub target_oid_before: String,
    pub target_oid_after: String,
    pub noop: bool,
}

struct ValidationLookup {
    state: ValidationState,
    command: Option<String>,
    oid: Option<String>,
    exit_code: Option<i32>,
    reason: Option<String>,
}

fn new_preview(thread: &threads::ThreadRow, snapshot_id: &str, target: &str) -> IntegrationPreview {
    IntegrationPreview {
        thread_id: thread.id.clone(),
        thread_title: thread.title.clone(),
        snapshot_id: snapshot_id.to_string(),
        source_oid: None,
        target_branch: target.to_string(),
        target_oid: None,
        operation: "blocked".into(),
        eligible: false,
        status: IntegrationStatus::SnapshotNotReady,
        reason: None,
        validation: ValidationState::Missing,
        validation_command: None,
        validation_oid: None,
        validation_exit_code: None,
        validation_instruction: format!(
            "aeril validate --thread {} --snapshot {} -- <validation-command>",
            thread.id, snapshot_id
        ),
        review: ReviewState::Clear,
        unresolved_comments: 0,
        files_changed: 0,
        additions: 0,
        deletions: 0,
    }
}

impl IntegrationPreview {
    pub fn status_label(&self) -> &'static str {
        match self.status {
            IntegrationStatus::EligibleFastForward => "fast-forward eligible",
            IntegrationStatus::AlreadyIntegrated => "already integrated",
            IntegrationStatus::TargetMissing => "target missing",
            IntegrationStatus::TargetInvalid => "target invalid",
            IntegrationStatus::TargetCheckedOut => "target checked out",
            IntegrationStatus::Diverged => "diverged",
            IntegrationStatus::SnapshotNotReady => "snapshot not ready",
            IntegrationStatus::SnapshotInvalid => "snapshot invalid",
            IntegrationStatus::ValidationRequired => "validation required",
            IntegrationStatus::ValidationFailed => "validation failed",
            IntegrationStatus::ReviewPending => "review required",
            IntegrationStatus::IntegrityError => "integrity error",
            IntegrationStatus::StaleTarget => "stale target",
        }
    }

    fn blocked(mut self, status: IntegrationStatus, reason: impl Into<String>) -> Self {
        self.status = status;
        self.operation = "blocked".into();
        self.eligible = false;
        self.reason = Some(reason.into());
        self
    }

    fn already(mut self, reason: impl Into<String>) -> Self {
        self.status = IntegrationStatus::AlreadyIntegrated;
        self.operation = "noop".into();
        self.eligible = false;
        self.reason = Some(reason.into());
        self
    }
}

fn latest_validation(
    store: &Store,
    thread_id: &str,
    snapshot_id: &str,
    expected_oid: &str,
) -> Result<ValidationLookup> {
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
        return Ok(ValidationLookup {
            state: ValidationState::Missing,
            command: None,
            oid: None,
            exit_code: None,
            reason: Some("no hay validación registrada para este snapshot".into()),
        });
    };
    let record: payload::ValidationRecorded = match serde_json::from_str(&text) {
        Ok(r) => r,
        Err(e) => {
            return Ok(ValidationLookup {
                state: ValidationState::Invalid,
                command: None,
                oid: None,
                exit_code: None,
                reason: Some(format!("validation.recorded ilegible: {e}")),
            });
        }
    };
    let command = Some(record.command.clone());
    let oid = Some(record.oid.clone());
    let exit_code = Some(record.exit_code);
    if record.snapshot_id != snapshot_id
        || record.command.trim().is_empty()
        || record.oid.is_empty()
        || record.output_hash.is_empty()
    {
        return Ok(ValidationLookup {
            state: ValidationState::Invalid,
            command,
            oid,
            exit_code,
            reason: Some("validation.recorded estructuralmente inválida".into()),
        });
    }
    if record.oid != expected_oid {
        return Ok(ValidationLookup {
            state: ValidationState::WrongOid,
            command,
            oid,
            exit_code,
            reason: Some(format!(
                "la validación es para el OID {} y el snapshot es {}",
                record.oid, expected_oid
            )),
        });
    }
    if record.exit_code != 0 {
        return Ok(ValidationLookup {
            state: ValidationState::Failed,
            command,
            oid,
            exit_code,
            reason: Some(format!(
                "la validación terminó con exit {}",
                record.exit_code
            )),
        });
    }
    if !record.tree_before_ok {
        return Ok(ValidationLookup {
            state: ValidationState::Failed,
            command,
            oid,
            exit_code,
            reason: Some("el checkout de validación no reprodujo el tree del snapshot".into()),
        });
    }
    if !record.tree_after_unchanged {
        return Ok(ValidationLookup {
            state: ValidationState::Failed,
            command,
            oid,
            exit_code,
            reason: Some("el checkout cambió durante la validación".into()),
        });
    }
    Ok(ValidationLookup {
        state: ValidationState::Passed,
        command,
        oid,
        exit_code,
        reason: None,
    })
}

/// Preflight read-only. No escribe eventos, refs, worktrees ni ejecuta comandos.
pub fn preview(
    store: &Store,
    thread_id: &str,
    snapshot_id: &str,
    target_branch: &str,
) -> Result<IntegrationPreview> {
    let thread = threads::get(&store.conn, thread_id)?
        .ok_or_else(|| Error::not_found(format!("thread {thread_id}")))?;
    let repo = git::open(Path::new(&thread.repo_path))?;
    let info = git::info(&repo)?;
    if info.repo_id != thread.repo_id {
        return Err(Error::integrity(format!(
            "el repositorio {} no coincide con la identidad registrada del thread",
            thread.repo_path
        )));
    }
    let mut p = new_preview(&thread, snapshot_id, target_branch);

    let ready = match checkpoint::ready_for_snapshot(store, thread_id, snapshot_id) {
        Ok(Some((_, ready))) => ready,
        Ok(None) => {
            return Ok(p.blocked(
                IntegrationStatus::SnapshotNotReady,
                format!("no hay checkpoint ready {snapshot_id}"),
            ));
        }
        Err(e) => {
            return Ok(p.blocked(
                IntegrationStatus::SnapshotInvalid,
                format!("checkpoint.ready ilegible: {e}"),
            ));
        }
    };
    p.source_oid = Some(ready.oid.clone());

    let prepared = match checkpoint::prepared_for_snapshot(store, thread_id, snapshot_id) {
        Ok(Some(prepared)) => prepared,
        Ok(None) => {
            return Ok(p.blocked(
                IntegrationStatus::SnapshotInvalid,
                format!("falta checkpoint.prepared para {snapshot_id}"),
            ));
        }
        Err(e) => {
            return Ok(p.blocked(
                IntegrationStatus::SnapshotInvalid,
                format!("checkpoint.prepared ilegible: {e}"),
            ));
        }
    };
    if prepared.snapshot_id != snapshot_id
        || prepared.expected_oid != ready.oid
        || prepared.git_ref != ready.git_ref
    {
        return Ok(p.blocked(
            IntegrationStatus::SnapshotInvalid,
            "prepared y ready tienen identidades contradictorias",
        ));
    }
    if ready.git_ref != checkpoint_ref(snapshot_id) {
        return Ok(p.blocked(
            IntegrationStatus::SnapshotInvalid,
            format!("ref de checkpoint inesperada: {}", ready.git_ref),
        ));
    }
    if let Err(e) = git::snapshot::verify_snapshot(&repo, &ready.git_ref, &ready.oid, &ready.files)
    {
        return Ok(p.blocked(
            IntegrationStatus::SnapshotInvalid,
            format!("objetos/ref del snapshot no verifican: {e}"),
        ));
    }
    let base = match Oid::from_str(&ready.base_oid) {
        Ok(base) => base,
        Err(e) => {
            return Ok(p.blocked(
                IntegrationStatus::SnapshotInvalid,
                format!("base_oid inválido {}: {e}", ready.base_oid),
            ));
        }
    };
    if repo.find_commit(base).is_err() {
        return Ok(p.blocked(
            IntegrationStatus::SnapshotInvalid,
            format!("commit base {} ausente", ready.base_oid),
        ));
    }

    let validation = latest_validation(store, thread_id, snapshot_id, &ready.oid)?;
    p.validation = validation.state;
    p.validation_command = validation.command.clone();
    p.validation_oid = validation.oid.clone();
    p.validation_exit_code = validation.exit_code;
    let validation_reason = validation.reason.clone();

    let unresolved =
        match review::open_comment_ids_for_snapshot(store, thread_id, snapshot_id, &ready.oid) {
            Ok(ids) => ids,
            Err(e) => {
                p.review = ReviewState::Invalid;
                return Ok(p.blocked(
                    IntegrationStatus::IntegrityError,
                    format!("review ilegible: {e}"),
                ));
            }
        };
    p.unresolved_comments = unresolved.len();
    p.review = if !unresolved.is_empty() {
        ReviewState::Pending
    } else {
        match review::acknowledged(store, thread_id, snapshot_id, &ready.oid) {
            Ok(true) => ReviewState::Clear,
            Ok(false) => ReviewState::Required,
            Err(e) => {
                p.review = ReviewState::Invalid;
                return Ok(p.blocked(
                    IntegrationStatus::IntegrityError,
                    format!("review acknowledgement ilegible: {e}"),
                ));
            }
        }
    };

    if target_branch.is_empty() || target_branch.trim() != target_branch {
        return Ok(p.blocked(
            IntegrationStatus::TargetInvalid,
            "la rama destino está vacía o tiene espacios externos",
        ));
    }
    let full_ref = format!("refs/heads/{target_branch}");
    if !git2::Reference::is_valid_name(&full_ref) {
        return Ok(p.blocked(
            IntegrationStatus::TargetInvalid,
            format!("nombre de rama inválido: {target_branch}"),
        ));
    }
    let reference = match repo.find_reference(&full_ref) {
        Ok(r) => r,
        Err(e) if e.code() == git2::ErrorCode::NotFound => {
            return Ok(p.blocked(
                IntegrationStatus::TargetMissing,
                format!("la rama destino {target_branch} no existe"),
            ));
        }
        Err(e) => return Err(e.into()),
    };
    let target = match reference.target() {
        Some(t) => t,
        None => {
            return Ok(p.blocked(
                IntegrationStatus::TargetInvalid,
                format!("{target_branch} no es una ref directa"),
            ));
        }
    };
    let target_s = target.to_string();
    p.target_oid = Some(target_s.clone());
    if let Some(worktree) = git::integrate::checked_out_worktree(&repo, target_branch) {
        return Ok(p.blocked(
            IntegrationStatus::TargetCheckedOut,
            format!("la rama destino está siendo usada por {worktree}"),
        ));
    }
    if target_s == ready.oid {
        return Ok(p.already(format!(
            "{target_branch} ya apunta al checkpoint {snapshot_id}"
        )));
    }
    let source = Oid::from_str(&ready.oid)?;
    if !repo
        .graph_descendant_of(source, base)
        .map_err(|e| Error::integrity(format!("no se pudo comprobar la base del snapshot: {e}")))?
    {
        return Ok(p.blocked(
            IntegrationStatus::SnapshotInvalid,
            format!(
                "{ready_oid} no desciende de su base {base}",
                ready_oid = ready.oid,
                base = base
            ),
        ));
    }
    match repo.graph_descendant_of(source, target) {
        Ok(true) => {}
        Ok(false) => {
            return Ok(p.blocked(
                IntegrationStatus::Diverged,
                format!(
                    "{ready_oid} no desciende de {target_s}; se requiere merge (no soportado)",
                    ready_oid = ready.oid
                ),
            ));
        }
        Err(e) => {
            return Ok(p.blocked(
                IntegrationStatus::IntegrityError,
                format!("no se pudo comprobar ancestría: {e}"),
            ));
        }
    }

    match p.validation {
        ValidationState::Passed => {}
        ValidationState::Missing => {
            let reason = format!(
                "falta validación para {snapshot_id}; ejecutá: {}",
                p.validation_instruction
            );
            return Ok(p.blocked(IntegrationStatus::ValidationRequired, reason));
        }
        ValidationState::Failed | ValidationState::WrongOid | ValidationState::Invalid => {
            return Ok(p.blocked(
                IntegrationStatus::ValidationFailed,
                validation_reason.unwrap_or_else(|| "validación no utilizable".into()),
            ));
        }
    }
    if p.review == ReviewState::Pending {
        let reason = format!(
            "hay {} comentario(s) sin resolver en este snapshot",
            p.unresolved_comments
        );
        return Ok(p.blocked(IntegrationStatus::ReviewPending, reason));
    }
    if p.review == ReviewState::Required {
        return Ok(p.blocked(
            IntegrationStatus::ReviewPending,
            format!(
                "revisión explícita pendiente; ejecutá: aeril review ack --thread {thread_id} --snapshot {snapshot_id}"
            ),
        ));
    }

    let files = match git::diff::files_between(&repo, &target_s, &ready.oid) {
        Ok(files) => files,
        Err(e) => {
            return Ok(p.blocked(
                IntegrationStatus::IntegrityError,
                format!("no se pudo calcular el cambio: {e}"),
            ));
        }
    };
    p.files_changed = files.len();
    p.additions = files.iter().map(|f| f.additions).sum();
    p.deletions = files.iter().map(|f| f.deletions).sum();
    p.status = IntegrationStatus::EligibleFastForward;
    p.operation = "fast_forward".into();
    p.eligible = true;
    p.reason = None;
    Ok(p)
}

/// Apply asistido. Re-ejecuta el preflight y exige que la fuente y el target
/// esperados sigan siendo exactamente los del preview confirmado. Después
/// delega en `session::integrate` (intención durable + CAS).
pub fn apply(
    store: &mut Store,
    thread_id: &str,
    snapshot_id: &str,
    target_branch: &str,
    expected_source_oid: &str,
    expected_target_oid: &str,
) -> Result<AppliedIntegration> {
    let p = preview(store, thread_id, snapshot_id, target_branch)?;
    let source = p
        .source_oid
        .as_deref()
        .ok_or_else(|| Error::not_ready(format!("no hay checkpoint ready {snapshot_id}")))?;
    let target = p.target_oid.as_deref().ok_or_else(|| {
        Error::conflict(
            p.reason
                .clone()
                .unwrap_or_else(|| "la rama destino no está disponible".into()),
        )
    })?;
    if source != expected_source_oid {
        return Err(Error::integrity(format!(
            "la fuente cambió: preview {expected_source_oid}, ahora {source}"
        )));
    }
    if target != expected_target_oid {
        return Err(Error::conflict(format!(
            "la rama destino se movió: esperaba {expected_target_oid}, ahora {target}; generá un preview nuevo"
        )));
    }
    if p.status == IntegrationStatus::AlreadyIntegrated {
        return Ok(AppliedIntegration {
            target_branch: target_branch.to_string(),
            source_oid: source.to_string(),
            target_oid_before: target.to_string(),
            target_oid_after: source.to_string(),
            noop: true,
        });
    }
    if p.status != IntegrationStatus::EligibleFastForward || !p.eligible {
        return Err(Error::conflict(format!(
            "integración asistida bloqueada: {}",
            p.reason.unwrap_or_else(|| "no elegible".into())
        )));
    }
    // `session::integrate` repite la resolución del checkpoint, escribe la
    // intención durable ANTES de tocar la ref y usa el CAS existente. El
    // `expect` es el OID pinned por el preview, nunca uno refrescado.
    let outcome = session::integrate_pinned(
        store,
        thread_id,
        snapshot_id,
        target_branch,
        expected_target_oid,
        expected_source_oid,
    )?;
    if outcome.target_oid_after != source {
        return Err(Error::integrity(format!(
            "la integración terminó en {}, se esperaba {source}",
            outcome.target_oid_after
        )));
    }
    let performed = outcome.performed;
    Ok(AppliedIntegration {
        target_branch: target_branch.to_string(),
        source_oid: source.to_string(),
        // Un no-op concurrente no debe reportar un "before" obsoleto como si
        // esta llamada hubiera movido la ref.
        target_oid_before: if performed {
            outcome.target_oid_before
        } else {
            source.to_string()
        },
        target_oid_after: outcome.target_oid_after,
        noop: !performed,
    })
}
