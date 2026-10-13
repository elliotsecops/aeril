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

#[derive(Debug)]
pub struct CaptureOutcome {
    pub snapshot_id: String,
    pub event_id: String,
    pub oid: String,
    pub files: Vec<payload::FileSummary>,
    pub patch_len: usize,
    pub had_changes: bool,
}

/// Captura y publica un checkpoint del estado actual del worktree del run.
pub fn capture(
    store: &mut Store,
    repo: &Repository,
    run: &RunRow,
    reason: &str,
) -> Result<CaptureOutcome> {
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
    let captured = git::snapshot::capture(&wt_repo, base_oid, &message)?;
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
    let (prepared_event_id, _) = events::append_typed(
        &mut store.conn,
        &run.thread_id,
        Some(&run.id),
        events::K_CHECKPOINT_PREPARED,
        "system",
        &prepared,
    )?;
    crash::point("prepared");

    // 3. Publicar la ref con estado esperado.
    if let Err(e) = git::snapshot::publish_ref_cas(repo, &ref_name, &captured.commit_oid) {
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
    let _ = prepared_event_id;
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
        git_ref: ref_name,
        oid: captured.commit_oid.clone(),
        base_oid: base_oid.to_string(),
        patch: captured.patch.clone(),
        files: captured.files.clone(),
        recovered: false,
    };
    let (ready_event_id, _) = events::append_typed(
        &mut store.conn,
        &run.thread_id,
        Some(&run.id),
        events::K_CHECKPOINT_READY,
        "system",
        &ready,
    )?;

    Ok(CaptureOutcome {
        snapshot_id,
        event_id: ready_event_id,
        oid: captured.commit_oid,
        files: captured.files,
        patch_len: captured.patch.len(),
        had_changes: captured.had_changes,
    })
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
    /// La ref apunta a otro OID: conflicto de integridad, no se toca.
    RefConflict { snapshot_id: String, detail: String },
    /// Faltan objetos o la ref no puede verificarse: terminal, se reporta.
    ObjectsMissing { snapshot_id: String, detail: String },
    /// El repositorio del thread no está disponible: sigue pendiente.
    RepoUnavailable {
        snapshot_id: String,
        repo_path: String,
    },
    /// Otro fallo de integridad reportado como terminal.
    Failed { snapshot_id: String, detail: String },
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
                    let tx = store
                        .conn
                        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                    events::append_tx(
                        &tx,
                        &p.thread_id,
                        prepared.run_id.as_deref(),
                        events::K_CHECKPOINT_READY,
                        "system",
                        &serde_json::to_value(ready)?,
                        now_ms(),
                    )?;
                    tx.commit()?;
                    outcomes.push(RecoveryOutcome::Completed {
                        snapshot_id: prepared.snapshot_id.clone(),
                        ref_recreated,
                    });
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
    events::append_typed(
        &mut store.conn,
        thread_id,
        prepared.run_id.as_deref(),
        events::K_CHECKPOINT_FAILED,
        "system",
        &failed,
    )?;
    Ok(())
}

// ── consultas para revisión ──────────────────────────────────────────────────

/// Último checkpoint `ready` de un thread.
pub fn latest_ready(
    store: &Store,
    thread_id: &str,
) -> Result<Option<(EventRecord, payload::CheckpointReady)>> {
    let mut all = events::fetch_by_kind(&store.conn, thread_id, events::K_CHECKPOINT_READY)?;
    let last = all.pop();
    match last {
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
    let all = events::fetch_by_kind(&store.conn, thread_id, events::K_CHECKPOINT_READY)?;
    for ev in all.into_iter().rev() {
        let parsed: payload::CheckpointReady = ev.parse()?;
        if parsed.snapshot_id == snapshot_id {
            return Ok(Some((ev, parsed)));
        }
    }
    Ok(None)
}
