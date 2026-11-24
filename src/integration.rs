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
    pub expected_target_oid: Option<String>,
    pub checked_out_path: Option<String>,
    pub target_dirty: Option<bool>,
    pub operation: String,
    pub eligible: bool,
    pub status: IntegrationStatus,
    pub reason: Option<String>,
    pub manual_summary: Option<String>,
    pub manual_commands: Vec<String>,
    pub manual_notes: Vec<String>,
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
        expected_target_oid: None,
        checked_out_path: None,
        target_dirty: None,
        operation: "blocked".into(),
        eligible: false,
        status: IntegrationStatus::SnapshotNotReady,
        reason: None,
        manual_summary: None,
        manual_commands: Vec::new(),
        manual_notes: Vec::new(),
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

fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b'@' | b':' | b'=')
        })
    {
        arg.to_string()
    } else {
        let escaped = arg.replace('\'', "'\"'\"'");
        format!("'{escaped}'")
    }
}

fn short_id(s: &str) -> &str {
    &s[..12.min(s.len())]
}

fn clean_dir_str(path: &str) -> &str {
    if path.len() > 1 {
        path.trim_end_matches('/')
    } else {
        path
    }
}

fn populate_guidance(p: &mut IntegrationPreview, repo_path: &str) {
    let repo_clean = clean_dir_str(repo_path);
    let repo_q = shell_quote(repo_clean);
    let thread_q = shell_quote(&p.thread_id);
    let snap_q = shell_quote(&p.snapshot_id);
    let branch_q = shell_quote(&p.target_branch);
    let preview_cmd =
        format!("aeril integrate preview --thread {thread_q} --snapshot {snap_q} --to {branch_q}");
    let src_oid = p.source_oid.as_deref().unwrap_or("<source_oid>");
    let tgt_oid = p.target_oid.as_deref().unwrap_or("<target_oid>");
    let snap_short: String = p
        .snapshot_id
        .chars()
        .take(8)
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();

    p.manual_commands.clear();
    p.manual_notes.clear();

    match p.status {
        IntegrationStatus::ValidationRequired => {
            p.manual_summary = Some(
                "Ejecutá la validación del snapshot antes de integrar o usar git manual.".into(),
            );
            p.manual_commands.push(p.validation_instruction.clone());
            p.manual_notes
                .push("Aeril nunca sugiere git merge --ff-only sin validación verificada.".into());
            if p.review == ReviewState::Pending {
                p.manual_notes.push(format!(
                    "Además hay {} comentario(s) sin resolver en este snapshot.",
                    p.unresolved_comments
                ));
            } else if p.review == ReviewState::Required {
                p.manual_notes
                    .push("Luego confirmá revisión (`a` en TUI o `aeril review ack ...`).".into());
            }
        }
        IntegrationStatus::ValidationFailed => {
            p.manual_summary = Some(
                "La validación falló o no corresponde al OID exacto de este checkpoint.".into(),
            );
            p.manual_commands.push(p.validation_instruction.clone());
            p.manual_notes.push(
                "Corregí el fallo y volvé a validar antes de intentar cualquier integración."
                    .into(),
            );
        }
        IntegrationStatus::ReviewPending => {
            if p.review == ReviewState::Pending {
                p.manual_summary = Some(format!(
                    "Hay {} comentario(s) abierto(s); resolvelos antes de integrar o usar git manual.",
                    p.unresolved_comments
                ));
                p.manual_commands
                    .push(format!("aeril comment list --thread {thread_q}"));
                p.manual_commands.push(format!(
                    "aeril comment resolve --thread {thread_q} --comment <comment-id>"
                ));
                p.manual_notes.push(
                    "Aeril nunca sugiere git merge --ff-only con comentarios abiertos.".into(),
                );
            } else {
                p.manual_summary =
                    Some("Falta confirmación explícita de revisión del snapshot en Aeril.".into());
                p.manual_commands.push(format!(
                    "aeril review ack --thread {thread_q} --snapshot {snap_q}"
                ));
                p.manual_notes.push(
                    "En TUI: cerrá preview (Esc) y presioná `a`; nunca se sugiere git merge --ff-only sin review ack."
                        .into(),
                );
            }
        }
        IntegrationStatus::TargetCheckedOut => {
            let co_path = p
                .checked_out_path
                .as_deref()
                .map(clean_dir_str)
                .unwrap_or(repo_clean);
            let co_q = shell_quote(co_path);
            if p.target_dirty == Some(true) {
                p.manual_summary = Some(format!(
                    "Dirty checkout en {co_path}: cambios sin commitear (uncommitted changes)."
                ));
                p.manual_commands.push(format!("git -C {co_q} status"));
                p.manual_commands.push(format!(
                    "git -C {co_q} stash push -u -m \"aeril backup before ff\""
                ));
                p.manual_commands
                    .push(format!("git -C {co_q} merge --ff-only {src_oid}"));
                p.manual_commands.push(preview_cmd);
                p.manual_notes
                    .push(format!("Identidades: source {src_oid} · target {tgt_oid}."));
                p.manual_notes.push(
                    "Evitá comandos destructivos o reescrituras forzadas; commiteá o stasheá antes de --ff-only."
                        .into(),
                );
            } else {
                p.manual_summary = Some(format!(
                    "Rama '{}' activa en {co_path}. Aplicá fast-forward en ese checkout y re-abrí preview.",
                    p.target_branch
                ));
                p.manual_commands
                    .push(format!("git -C {co_q} merge --ff-only {src_oid}"));
                p.manual_commands.push(preview_cmd);
                p.manual_notes
                    .push(format!("Identidades: source {src_oid} · target {tgt_oid}."));
                p.manual_notes.push(
                    "Tras ejecutar --ff-only, re-ejecutá preview para confirmar `already integrated`."
                        .into(),
                );
            }
        }
        IntegrationStatus::Diverged => {
            p.manual_summary = Some(format!(
                "'{}' divergió del checkpoint; inspeccioná commits y resolvé en una rama temporal.",
                p.target_branch
            ));
            p.manual_commands.push(format!(
                "git -C {repo_q} log --oneline {tgt_oid}..{src_oid}"
            ));
            p.manual_commands.push(format!(
                "git -C {repo_q} log --oneline {src_oid}..{tgt_oid}"
            ));
            p.manual_commands.push(format!(
                "git -C {repo_q} checkout -b integrate-{snap_short} {branch_q}"
            ));
            p.manual_commands
                .push(format!("git -C {repo_q} cherry-pick {src_oid}"));
            p.manual_notes
                .push(format!("Identidades: source {src_oid} · target {tgt_oid}."));
            p.manual_notes.push(
                "Todo commit nuevo de merge/cherry-pick/rebase requiere validación y review nuevos (evitá reescrituras forzadas)."
                    .into(),
            );
        }
        IntegrationStatus::StaleTarget => {
            let expected = p
                .expected_target_oid
                .as_deref()
                .unwrap_or("<expected_target_oid>");
            if let Some(actual) = p.target_oid.as_deref() {
                p.manual_summary = Some(format!(
                    "'{}' se movió entre preview y confirmación ({} → {}); ref intacta.",
                    p.target_branch,
                    short_id(expected),
                    short_id(actual)
                ));
                p.manual_commands.push(format!(
                    "git -C {repo_q} log --oneline {expected}..{actual}"
                ));
                p.manual_commands.push(preview_cmd);
                p.manual_notes.push(format!(
                    "Esperado: {expected} · Actual: {actual} · Source: {src_oid}."
                ));
            } else {
                p.manual_summary = Some(format!(
                    "'{}' cambió o desapareció tras abrir preview (esperaba {}); ref intacta.",
                    p.target_branch,
                    short_id(expected)
                ));
                p.manual_commands.push(preview_cmd);
                p.manual_notes
                    .push(format!("Esperado: {expected} · Source: {src_oid}."));
            }
            p.manual_notes.push(
                "Refrescá el preview ([r] en TUI) para confirmar explícitamente sobre el nuevo target."
                    .into(),
            );
        }
        IntegrationStatus::TargetMissing => {
            p.manual_summary = Some(format!(
                "La rama destino '{}' no existe en {repo_clean}.",
                p.target_branch
            ));
            p.manual_commands
                .push(format!("git -C {repo_q} branch {branch_q} <base-oid>"));
            p.manual_commands.push(preview_cmd);
            p.manual_notes.push(format!(
                "Source listo: {src_oid}. Creá la rama o elegí una existente."
            ));
        }
        IntegrationStatus::TargetInvalid => {
            p.manual_summary = Some(format!(
                "Nombre o referencia destino inválida: '{}'.",
                p.target_branch
            ));
            p.manual_notes
                .push("Ingresá un nombre de rama Git válido al abrir `i`.".into());
        }
        IntegrationStatus::SnapshotNotReady | IntegrationStatus::SnapshotInvalid => {
            p.manual_summary =
                Some("El snapshot no está listo o no verifica en almacenamiento durable.".into());
            p.manual_commands
                .push(format!("aeril checkpoint list --thread {thread_q}"));
            p.manual_commands.push("aeril recover".into());
            p.manual_notes
                .push("Verificá o recuperá el checkpoint antes de intentar integrar.".into());
        }
        IntegrationStatus::IntegrityError => {
            p.manual_summary = Some(
                "Error de integridad al leer la evidencia del snapshot o del repositorio.".into(),
            );
            p.manual_commands.push("aeril recover".into());
            p.manual_notes
                .push("No se moverá ninguna ref hasta que la evidencia vuelva a verificar.".into());
        }
        IntegrationStatus::AlreadyIntegrated => {
            p.manual_summary = Some(format!(
                "'{}' ya apunta a {src_oid}. No se requiere ninguna acción.",
                p.target_branch
            ));
        }
        IntegrationStatus::EligibleFastForward => {
            p.manual_summary = Some(format!(
                "Fast-forward limpio listo: {} → {}. Solo moverá '{}' al confirmar.",
                short_id(tgt_oid),
                short_id(src_oid),
                p.target_branch
            ));
        }
    }
}

fn finish_preview(mut p: IntegrationPreview, repo_path: &str) -> IntegrationPreview {
    populate_guidance(&mut p, repo_path);
    p
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
    let p = preview_inner(store, &thread, snapshot_id, target_branch)?;
    Ok(finish_preview(p, &thread.repo_path))
}

/// Re-evalúa el preview cuando una confirmación falla o puede estar obsoleta.
/// Si el target cambió respecto de `expected_target_oid`, devuelve estado
/// explícito `StaleTarget` con instrucciones C3 y sin adoptar el nuevo OID.
pub fn stale_preview(
    store: &Store,
    thread_id: &str,
    snapshot_id: &str,
    target_branch: &str,
    expected_target_oid: &str,
) -> Result<IntegrationPreview> {
    let thread = threads::get(&store.conn, thread_id)?
        .ok_or_else(|| Error::not_found(format!("thread {thread_id}")))?;
    let mut p = preview_inner(store, &thread, snapshot_id, target_branch)?;
    if p.target_oid.as_deref() != Some(expected_target_oid) {
        p.expected_target_oid = Some(expected_target_oid.to_string());
        p.checked_out_path = None;
        p.target_dirty = None;
        let reason = match p.target_oid.as_deref() {
            Some(actual) => format!(
                "la rama destino {target_branch} se movió: esperaba {expected_target_oid}, ahora {actual}; generá un preview nuevo"
            ),
            None => format!(
                "la rama destino {target_branch} cambió o desapareció: esperaba {expected_target_oid}; generá un preview nuevo"
            ),
        };
        p = p.blocked(IntegrationStatus::StaleTarget, reason);
    }
    Ok(finish_preview(p, &thread.repo_path))
}

fn preview_inner(
    store: &Store,
    thread: &threads::ThreadRow,
    snapshot_id: &str,
    target_branch: &str,
) -> Result<IntegrationPreview> {
    let thread_id = thread.id.as_str();
    let repo = git::open(Path::new(&thread.repo_path))?;
    let info = git::info(&repo)?;
    if info.repo_id != thread.repo_id {
        return Err(Error::integrity(format!(
            "el repositorio {} no coincide con la identidad registrada del thread",
            thread.repo_path
        )));
    }
    let mut p = new_preview(thread, snapshot_id, target_branch);

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
    match checkpoint::has_failed_marker(store, thread_id, snapshot_id) {
        Ok(true) => {
            return Ok(p.blocked(
                IntegrationStatus::SnapshotInvalid,
                format!("el snapshot {snapshot_id} tiene un marcador checkpoint.failed"),
            ));
        }
        Ok(false) => {}
        Err(e) => {
            return Ok(p.blocked(
                IntegrationStatus::IntegrityError,
                format!("no se pudo verificar checkpoint.failed: {e}"),
            ));
        }
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

    // C5: validación y review siempre preceden cualquier consejo Git manual
    // o inspección de worktree sobre la rama destino.
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

    // C1 paso 3: si la rama destino (aun checked out) ya apunta al source,
    // el re-preview confirma AlreadyIntegrated.
    if target_s == ready.oid {
        return Ok(p.already(format!(
            "{target_branch} ya apunta al checkpoint {snapshot_id}"
        )));
    }

    // Comprobar divergencia antes de TargetCheckedOut evita sugerir
    // `git merge --ff-only` sobre ramas divergidas.
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

    if let Some(co) = git::integrate::checked_out_target(&repo, target_branch) {
        p.checked_out_path = Some(co.path.clone());
        p.target_dirty = Some(co.dirty);
        let reason = if co.dirty {
            format!(
                "la rama destino {target_branch} está en {} ({}) con dirty checkout (uncommitted changes)",
                co.path, co.label
            )
        } else {
            format!(
                "la rama destino {target_branch} está checked out en {} ({})",
                co.path, co.label
            )
        };
        return Ok(p.blocked(IntegrationStatus::TargetCheckedOut, reason));
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
        let log_cmd = threads::get(&store.conn, thread_id)?
            .map(|t| {
                format!(
                    "git -C {} log --oneline {expected_target_oid}..{target}",
                    shell_quote(clean_dir_str(&t.repo_path))
                )
            })
            .unwrap_or_else(|| format!("git log --oneline {expected_target_oid}..{target}"));
        let preview_cmd = format!(
            "aeril integrate preview --thread {} --snapshot {} --to {}",
            shell_quote(thread_id),
            shell_quote(snapshot_id),
            shell_quote(target_branch),
        );
        return Err(Error::conflict(format!(
            "la rama destino {target_branch} se movió: esperaba {expected_target_oid}, ahora {target} (source {source}); revisá `{log_cmd}` y regenerá preview con `{preview_cmd}`"
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
