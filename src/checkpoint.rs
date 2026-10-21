//! Protocolo de checkpoints y recuperación (v0.4).
//!
//! Captura: objetos → `prepared` → publicar ref (CAS por OID) → verificar →
//! `ready`. Comentarios solo tras `ready`.
//!
//! Recuperación al reabrir: un `prepared` sin `ready`/`failed` se reconcilia
//! SOLO contra los objetos Git originales. Jamás se reconstruye desde el
//! working tree actual y se llama el mismo snapshot.

use crate::crash;
use crate::error::{Error, Result};
use crate::git;
use crate::ids::{checkpoint_ref, new_id};
use crate::store::events::{self, EventRecord, payload};
use crate::store::{Store, runs::RunRow};
use crate::timeutil::now_ms;
use git2::Repository;
use rusqlite::OptionalExtension;

#[derive(Debug)]
pub struct CaptureOutcome {
    pub snapshot_id: String,
    pub event_id: String,
    pub oid: String,
    pub files: Vec<payload::FileSummary>,
    pub patch_len: usize,
}

/// Captura y publica un checkpoint del estado actual del worktree del run.
/// Devuelve `Ok(None)` si no hay cambios respecto de la base del run.
pub fn capture(
    store: &mut Store,
    repo: &Repository,
    run: &RunRow,
    reason: &str,
) -> Result<Option<CaptureOutcome>> {
    let workspace_id = run
        .workspace_id
        .as_deref()
        .ok_or_else(|| Error::invalid("el run no tiene workspace_id"))?;
    let base_oid = run
        .base_oid
        .as_deref()
        .ok_or_else(|| Error::invalid("el run no tiene base_oid"))?;
    let wt_repo = git::worktree::open_worktree(repo, workspace_id)?;

    let snapshot_id = new_id();
    let ref_name = checkpoint_ref(&snapshot_id);
    let message = format!("checkpoint {snapshot_id} ({reason}; run {})", run.id);

    // 1. Construir objetos inmutables (working tree real, sin tocar index/rama).
    let Some(captured) = git::snapshot::capture(&wt_repo, base_oid, &message)? else {
        // Sin cambios: no hay nada que revisar; no se publica nada.
        return Ok(None);
    };
    // Límites agregados de la memoria de revisión (B2): rechazo explícito,
    // nunca truncado silencioso ni publicación de un patch incompleto.
    let max_patch = crate::util::env_usize("AERIL_MAX_PATCH_BYTES", 4 * 1024 * 1024);
    let max_files = crate::util::env_usize("AERIL_MAX_FILES", 500);
    if captured.patch.len() > max_patch {
        return Err(Error::unsupported(format!(
            "checkpoint rechazado: el patch revisado mide {} bytes (límite {max_patch})",
            captured.patch.len()
        )));
    }
    if captured.files.len() > max_files {
        return Err(Error::unsupported(format!(
            "checkpoint rechazado: {} archivos revisados (límite {max_files})",
            captured.files.len()
        )));
    }
    crash::point("objects");

    // 2. Persistir la intención ANTES de publicar la ref.
    let prepared = payload::CheckpointPrepared {
        snapshot_id: snapshot_id.clone(),
        git_ref: ref_name.clone(),
        expected_oid: captured.commit_oid.clone(),
        run_id: Some(run.id.clone()),
        base_oid: base_oid.to_string(),
        parent_oid: base_oid.to_string(),
        files: captured.files.clone(),
    };
    let prepared_value = serde_json::to_value(&prepared)?;
    append_fenced(
        store,
        workspace_id,
        &run.id,
        &run.thread_id,
        Some(&run.id),
        events::K_CHECKPOINT_PREPARED,
        &prepared_value,
    )?;
    crash::point("prepared");

    // 3. Publicar la ref con estado esperado. La contención transitoria del
    // ref.lock deja el `prepared` recuperable: no se marca failed terminal.
    if let Err(e) = git::snapshot::publish_ref_cas(repo, &ref_name, &captured.commit_oid) {
        if git::snapshot::is_transient_ref_lock(&e) {
            return Err(e);
        }
        let failed = payload::CheckpointFailed {
            snapshot_id: snapshot_id.clone(),
            git_ref: Some(ref_name.clone()),
            expected_oid: Some(captured.commit_oid.clone()),
            phase: "publish".into(),
            reason: e.to_string(),
            terminal: true,
        };
        events::append_typed(
            &mut store.conn,
            &run.thread_id,
            Some(&run.id),
            events::K_CHECKPOINT_FAILED,
            "system",
            &failed,
        )?;
        return Err(e);
    }
    crash::point("ref");

    // 4. Verificar ref + objetos contra lo declarado.
    if let Err(e) =
        git::snapshot::verify_snapshot(repo, &ref_name, &captured.commit_oid, &captured.files)
    {
        let failed = payload::CheckpointFailed {
            snapshot_id: snapshot_id.clone(),
            git_ref: Some(ref_name.clone()),
            expected_oid: Some(captured.commit_oid.clone()),
            phase: "verify".into(),
            reason: e.to_string(),
            terminal: true,
        };
        events::append_typed(
            &mut store.conn,
            &run.thread_id,
            Some(&run.id),
            events::K_CHECKPOINT_FAILED,
            "system",
            &failed,
        )?;
        return Err(e);
    }
    crash::point("verify");

    // 5. Confirmar. Desde aquí se puede revisar/comentar.
    let ready = payload::CheckpointReady {
        snapshot_id: snapshot_id.clone(),
        git_ref: ref_name.clone(),
        oid: captured.commit_oid.clone(),
        base_oid: base_oid.to_string(),
        patch: captured.patch.clone(),
        files: captured.files.clone(),
        recovered: false,
    };
    let ready_value = serde_json::to_value(&ready)?;
    let ready_event_id = match append_fenced(
        store,
        workspace_id,
        &run.id,
        &run.thread_id,
        Some(&run.id),
        events::K_CHECKPOINT_READY,
        &ready_value,
    ) {
        Ok(id) => id,
        Err(e) if matches!(e, Error::LeaseLost(_)) => {
            // La ref ya está publicada (objetos inmutables, jamás se borra),
            // pero este checkpoint NO puede volverse ready. Marcador terminal
            // para que la recuperación no lo complete: la distinción es
            // crash legítimo (sin marcador → recovery completa) vs pérdida
            // de lease (marcador → terminado).
            let failed = payload::CheckpointFailed {
                snapshot_id: snapshot_id.clone(),
                git_ref: Some(ref_name.clone()),
                expected_oid: Some(captured.commit_oid.clone()),
                phase: "lease_fence".into(),
                reason: e.to_string(),
                terminal: true,
            };
            append_resolution(
                store,
                &run.thread_id,
                Some(&run.id),
                &snapshot_id,
                events::K_CHECKPOINT_FAILED,
                &serde_json::to_value(failed)?,
            )?;
            return Err(e);
        }
        Err(e) => return Err(e),
    };

    Ok(Some(CaptureOutcome {
        snapshot_id: snapshot_id.clone(),
        event_id: ready_event_id,
        oid: captured.commit_oid,
        files: captured.files,
        patch_len: captured.patch.len(),
    }))
}

// ── recuperación ─────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct Pending {
    pub event_id: String,
    pub thread_id: String,
    pub repo_path: String,
    pub seq: i64,
    pub prepared: payload::CheckpointPrepared,
}

/// `prepared` sin `ready` ni `failed` (anti-join por snapshot_id).
pub fn pending_checkpoints(store: &Store) -> Result<Vec<Pending>> {
    let mut stmt = store.conn.prepare(
        "SELECT p.id, p.thread_id, p.payload, t.repo_path, p.seq
         FROM events p JOIN threads t ON t.id = p.thread_id
         WHERE p.kind = ?1
           AND NOT EXISTS (
             SELECT 1 FROM events d
             WHERE d.thread_id = p.thread_id
               AND d.kind IN (?2, ?3)
               AND json_extract(d.payload, '$.snapshot_id') = json_extract(p.payload, '$.snapshot_id')
           )
         ORDER BY p.seq ASC",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![
            events::K_CHECKPOINT_PREPARED,
            events::K_CHECKPOINT_READY,
            events::K_CHECKPOINT_FAILED
        ],
        |row| {
            let payload_text: String = row.get(2)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                payload_text,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        },
    )?;
    let mut out = Vec::new();
    for r in rows {
        let (event_id, thread_id, payload_text, repo_path, seq) = r?;
        let prepared: payload::CheckpointPrepared = serde_json::from_str(&payload_text)
            .map_err(|e| Error::integrity(format!("prepared ilegible ({event_id}): {e}")))?;
        out.push(Pending {
            event_id,
            thread_id,
            repo_path,
            seq,
            prepared,
        });
    }
    Ok(out)
}

#[derive(Debug)]
pub enum RecoveryOutcome {
    /// Ref y objetos verificados (re-creando la ref si hacía falta).
    Completed {
        snapshot_id: String,
        ref_recreated: bool,
    },
    /// Otro proceso de Aeril resolvió el mismo snapshot mientras tanto:
    /// no se duplica la resolución.
    AlreadyResolved { snapshot_id: String },
    /// La ref apunta a otro OID: conflicto de integridad, no se toca.
    RefConflict { snapshot_id: String, detail: String },
    /// Faltan objetos o la ref no puede verificarse: terminal, se reporta.
    ObjectsMissing { snapshot_id: String, detail: String },
    /// El repositorio del thread no está disponible: sigue pendiente.
    RepoUnavailable {
        snapshot_id: String,
        repo_path: String,
    },
    /// Contención transitoria del lock de la ref: sigue pendiente y se
    /// reintenta en la próxima recuperación, sin `checkpoint.failed`.
    Deferred { snapshot_id: String, detail: String },
    /// Otro fallo de integridad reportado como terminal.
    Failed { snapshot_id: String, detail: String },
}

/// Fence de lease (RC2.1): verifica el ownership del workspace y persiste el
/// evento DENTRO DE LA MISMA transacción IMMEDIATE — no hay ventana entre la
/// verificación y la escritura. Un run sin el lease no puede publicar hechos
/// de checkpoint (ni `prepared` ni `ready`).
fn append_fenced(
    store: &mut Store,
    workspace_id: &str,
    owner_run_id: &str,
    thread_id: &str,
    run_id: Option<&str>,
    kind: &str,
    payload: &serde_json::Value,
) -> Result<String> {
    let tx = store
        .conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let holder: Option<String> = tx
        .query_row(
            "SELECT run_id FROM leases WHERE workspace_id = ?1",
            rusqlite::params![workspace_id],
            |row| row.get(0),
        )
        .optional()?;
    if holder.as_deref() != Some(owner_run_id) {
        tx.rollback()?;
        return Err(Error::LeaseLost(format!(
            "el workspace {workspace_id} ya no pertenece al run {owner_run_id} (holder: {}); \"{kind}\" no se persiste",
            holder.unwrap_or_else(|| "ninguno".into())
        )));
    }
    let (id, _seq) = events::append_tx(&tx, thread_id, run_id, kind, "system", payload, now_ms())?;
    tx.commit()?;
    Ok(id)
}

/// ¿Existe ya una resolución (`ready`/`failed`) para este snapshot?
fn resolution_exists(
    conn: &rusqlite::Connection,
    thread_id: &str,
    snapshot_id: &str,
) -> Result<bool> {
    let exists: i64 = conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM events
            WHERE thread_id = ?1 AND kind IN (?2, ?3)
              AND json_extract(payload, '$.snapshot_id') = ?4
        )",
        rusqlite::params![
            thread_id,
            events::K_CHECKPOINT_READY,
            events::K_CHECKPOINT_FAILED,
            snapshot_id
        ],
        |row| row.get(0),
    )?;
    Ok(exists == 1)
}

/// Append idempotente de una resolución: si otro proceso ya resolvió el
/// snapshot, no se duplica (dos `recover` concurrentes → un solo ready).
fn append_resolution(
    store: &mut Store,
    thread_id: &str,
    run_id: Option<&str>,
    snapshot_id: &str,
    kind: &str,
    payload: &serde_json::Value,
) -> Result<bool> {
    let tx = store
        .conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if resolution_exists(&tx, thread_id, snapshot_id)? {
        tx.rollback()?;
        return Ok(false);
    }
    events::append_tx(&tx, thread_id, run_id, kind, "system", payload, now_ms())?;
    tx.commit()?;
    Ok(true)
}

/// Reconcilia checkpoints interrumpidos. Idempotente: los que ya tienen
/// `ready`/`failed` quedan fuera de la consulta.
pub fn recover_pending(
    store: &mut Store,
    opener: &dyn Fn(&str) -> Option<Repository>,
) -> Result<Vec<RecoveryOutcome>> {
    let pending = pending_checkpoints(store)?;
    let mut outcomes = Vec::new();
    for p in pending {
        let prepared = &p.prepared;
        let Some(repo) = opener(&p.repo_path) else {
            // No se borra ni se marca: puede ser un path temporalmente ausente.
            outcomes.push(RecoveryOutcome::RepoUnavailable {
                snapshot_id: prepared.snapshot_id.clone(),
                repo_path: p.repo_path.clone(),
            });
            continue;
        };
        let expected = prepared.expected_oid.clone();
        let existing = repo.find_reference(&prepared.git_ref).ok();
        let mut ref_recreated = false;

        let verification_target = match existing {
            Some(reference) => {
                let target = reference
                    .target()
                    .map(|o| o.to_string())
                    .unwrap_or_default();
                if target != expected {
                    // Conflicto: jamás se sobrescribe una ref divergente.
                    let detail = format!(
                        "{} apunta a {target} y el prepared esperaba {expected}",
                        prepared.git_ref
                    );
                    append_failed(store, &p.thread_id, prepared, "ref_conflict", &detail, true)?;
                    outcomes.push(RecoveryOutcome::RefConflict {
                        snapshot_id: prepared.snapshot_id.clone(),
                        detail,
                    });
                    continue;
                }
                true
            }
            None => {
                // La ref no está: solo se recrea si el OBJETO declarado existe.
                let commit_present = git2::Oid::from_str(&expected)
                    .ok()
                    .and_then(|oid| repo.find_commit(oid).ok())
                    .is_some();
                if commit_present {
                    match git::snapshot::publish_ref_cas(&repo, &prepared.git_ref, &expected) {
                        Ok(_) => {
                            ref_recreated = true;
                            true
                        }
                        Err(e) if git::snapshot::is_transient_ref_lock(&e) => {
                            // El lock sigue tomado por otro proceso. No hay
                            // conflicto demostrado ni fallo irreversible: se
                            // deja pendiente para la próxima recuperación.
                            outcomes.push(RecoveryOutcome::Deferred {
                                snapshot_id: prepared.snapshot_id.clone(),
                                detail: e.to_string(),
                            });
                            continue;
                        }
                        Err(e) if matches!(e, Error::Conflict(_)) => {
                            let detail = e.to_string();
                            append_failed(
                                store,
                                &p.thread_id,
                                prepared,
                                "ref_conflict",
                                &detail,
                                true,
                            )?;
                            outcomes.push(RecoveryOutcome::RefConflict {
                                snapshot_id: prepared.snapshot_id.clone(),
                                detail,
                            });
                            continue;
                        }
                        Err(e) => {
                            append_failed(
                                store,
                                &p.thread_id,
                                prepared,
                                "publish_failed",
                                &e.to_string(),
                                true,
                            )?;
                            outcomes.push(RecoveryOutcome::Failed {
                                snapshot_id: prepared.snapshot_id.clone(),
                                detail: e.to_string(),
                            });
                            continue;
                        }
                    }
                } else {
                    let detail = format!(
                        "el commit esperado {expected} no existe en el repositorio; no se reconstruye desde el working tree"
                    );
                    append_failed(
                        store,
                        &p.thread_id,
                        prepared,
                        "objects_missing",
                        &detail,
                        true,
                    )?;
                    outcomes.push(RecoveryOutcome::ObjectsMissing {
                        snapshot_id: prepared.snapshot_id.clone(),
                        detail,
                    });
                    continue;
                }
            }
        };
        debug_assert!(verification_target);

        // Verificar objetos y recomponer patch/resumen desde Git (nunca del working tree).
        match git::snapshot::verify_snapshot(&repo, &prepared.git_ref, &expected, &prepared.files) {
            Ok(()) => match (
                git::diff::patch_between(&repo, &prepared.base_oid, &expected),
                git::diff::files_between(&repo, &prepared.base_oid, &expected),
            ) {
                (Ok(patch), Ok(files)) => {
                    let ready = payload::CheckpointReady {
                        snapshot_id: prepared.snapshot_id.clone(),
                        git_ref: prepared.git_ref.clone(),
                        oid: expected.clone(),
                        base_oid: prepared.base_oid.clone(),
                        patch,
                        files,
                        recovered: true,
                    };
                    // Idempotente ante recuperación concurrente: si otro
                    // proceso ya resolvió, no se duplica el evento.
                    let appended = append_resolution(
                        store,
                        &p.thread_id,
                        prepared.run_id.as_deref(),
                        &prepared.snapshot_id,
                        events::K_CHECKPOINT_READY,
                        &serde_json::to_value(ready)?,
                    )?;
                    if appended {
                        outcomes.push(RecoveryOutcome::Completed {
                            snapshot_id: prepared.snapshot_id.clone(),
                            ref_recreated,
                        });
                    } else {
                        outcomes.push(RecoveryOutcome::AlreadyResolved {
                            snapshot_id: prepared.snapshot_id.clone(),
                        });
                    }
                }
                (Err(e), _) | (_, Err(e)) => {
                    let detail = format!("no se pudo recomponer el patch desde Git: {e}");
                    append_failed(
                        store,
                        &p.thread_id,
                        prepared,
                        "diff_unavailable",
                        &detail,
                        true,
                    )?;
                    outcomes.push(RecoveryOutcome::Failed {
                        snapshot_id: prepared.snapshot_id.clone(),
                        detail,
                    });
                }
            },
            Err(e) => {
                let detail = e.to_string();
                append_failed(
                    store,
                    &p.thread_id,
                    prepared,
                    "objects_missing",
                    &detail,
                    true,
                )?;
                outcomes.push(RecoveryOutcome::ObjectsMissing {
                    snapshot_id: prepared.snapshot_id.clone(),
                    detail,
                });
            }
        }
    }
    Ok(outcomes)
}

fn append_failed(
    store: &mut Store,
    thread_id: &str,
    prepared: &payload::CheckpointPrepared,
    reason: &str,
    detail: &str,
    terminal: bool,
) -> Result<()> {
    let failed = payload::CheckpointFailed {
        snapshot_id: prepared.snapshot_id.clone(),
        git_ref: Some(prepared.git_ref.clone()),
        expected_oid: Some(prepared.expected_oid.clone()),
        phase: "recovery".into(),
        reason: format!("{reason}: {detail}"),
        terminal,
    };
    append_resolution(
        store,
        thread_id,
        prepared.run_id.as_deref(),
        &prepared.snapshot_id,
        events::K_CHECKPOINT_FAILED,
        &serde_json::to_value(failed)?,
    )?;
    Ok(())
}

// ── consultas para revisión ──────────────────────────────────────────────────

/// Último checkpoint `ready` de un thread.
pub fn latest_ready(
    store: &Store,
    thread_id: &str,
) -> Result<Option<(EventRecord, payload::CheckpointReady)>> {
    // Proyección acotada: el ÚLTIMO `checkpoint.ready` por seq, sin materializar
    // el historial de checkpoints del thread (mismo resultado que fetch_by_kind
    // + pop, con memoria constante).
    match events::fetch_latest_by_kind(&store.conn, thread_id, events::K_CHECKPOINT_READY)? {
        Some(ev) => {
            let parsed: payload::CheckpointReady = ev.parse()?;
            Ok(Some((ev, parsed)))
        }
        None => Ok(None),
    }
}

pub fn ready_for_snapshot(
    store: &Store,
    thread_id: &str,
    snapshot_id: &str,
) -> Result<Option<(EventRecord, payload::CheckpointReady)>> {
    // Cursor en streaming (seq DESC): NO se materializa el historial.
    //
    // Semántica CERTIFICADA (`main@3a28af2`): se deserializa y valida CADA
    // `checkpoint.ready` ANTES de comparar su snapshot_id, y los errores de
    // validación se PROPAGAN. Filtrar por `json_extract` en SQL saltaría los
    // registros inválidos y admitiría una operación que antes fallaba
    // (P2-REMEDIATION: regresión semántica detectada en auditoría).
    let mut stmt = store.conn.prepare(
        "SELECT id, thread_id, seq, run_id, kind, actor, payload_version, payload, created_ms
           FROM events
          WHERE thread_id = ?1 AND kind = 'checkpoint.ready'
          ORDER BY seq DESC",
    )?;
    let mut rows = stmt.query(rusqlite::params![thread_id])?;
    while let Some(row) = rows.next()? {
        let payload_text: String = row.get(7)?;
        let ev = EventRecord {
            id: row.get(0)?,
            thread_id: row.get(1)?,
            seq: row.get(2)?,
            run_id: row.get(3)?,
            kind: row.get(4)?,
            actor: row.get(5)?,
            payload_version: row.get(6)?,
            payload: serde_json::from_str(&payload_text).unwrap_or(serde_json::Value::Null),
            created_ms: row.get(8)?,
        };
        let parsed: payload::CheckpointReady = ev.parse()?;
        if parsed.snapshot_id == snapshot_id {
            return Ok(Some((ev, parsed)));
        }
    }
    Ok(None)
}
