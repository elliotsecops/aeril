//! Snapshots inmutables: objetos Git construidos desde el working tree REAL
//! (incluye cambios sin commitear), sin tocar el índice ni la rama del humano.
//!
//! Protocolo (v0.4): construir objetos → `checkpoint.prepared` → publicar ref
//! `refs/aeril/checkpoints/<snapshot_id>` (CAS por OID esperado) → verificar →
//! `checkpoint.ready`. La orquestación vive en `crate::checkpoint`.

use crate::error::{Error, Result};
use crate::store::events::payload::FileSummary;
use git2::{Oid, Repository, Signature, StatusOptions, Statuses};
use std::path::Path;
use std::time::{Duration, Instant};

/// Límite defensivo de archivo individual en v0 (textual): 16 MiB.
pub const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;

/// Una publicación CAS compite con otros procesos por el mismo `ref.lock`.
/// El deadline es monotónico y compartido por todos los reintentos.
const REF_LOCK_RETRY_DEADLINE: Duration = Duration::from_secs(5);
const REF_LOCK_RETRY_INITIAL_BACKOFF: Duration = Duration::from_millis(1);
const REF_LOCK_RETRY_MAX_BACKOFF: Duration = Duration::from_millis(25);

/// Contención transitoria del lock de una ref. No incluye conflictos de OID.
pub fn is_transient_ref_lock(error: &Error) -> bool {
    matches!(
        error,
        Error::Git(git_error) if git_error.code() == git2::ErrorCode::Locked
    )
}

#[derive(Debug, Clone)]
pub struct Captured {
    pub commit_oid: String,
    pub tree_oid: String,
    pub files: Vec<FileSummary>,
    pub patch: String,
}

/// ¿La entrada de status es un cambio NO soportado en v0 (submódulo/gitlink
/// o directorio en disco)? Se rechaza explícitamente: jamás se omite en
/// silencio del snapshot.
fn unsupported_entry(entry: &git2::StatusEntry<'_>, abs: &std::path::Path) -> Option<String> {
    let gitlink = |d: Option<git2::DiffDelta<'_>>| {
        d.map(|d| {
            d.old_file().mode() == git2::FileMode::Commit
                || d.new_file().mode() == git2::FileMode::Commit
        })
        .unwrap_or(false)
    };
    if gitlink(entry.head_to_index()) || gitlink(entry.index_to_workdir()) {
        return Some("submódulo (gitlink) modificado".to_string());
    }
    if let Ok(meta) = std::fs::symlink_metadata(abs)
        && meta.is_dir()
    {
        return Some("directorio/submódulo no soportado".to_string());
    }
    None
}

/// Caminos con gitlink (submódulo) según el ÍNDICE del worktree: la
/// inspección es independiente del status, porque el status puede omitir
/// escrituras bajo un gitlink no inicializado (RC2-A5).
fn gitlink_paths(repo: &Repository) -> Result<Vec<String>> {
    let index = repo.index()?;
    let mut out = Vec::new();
    for entry in index.iter() {
        // 0o160000 = gitlink (submódulo), en el modo crudo del índice.
        if entry.mode == 0o160000 {
            out.push(String::from_utf8_lossy(&entry.path).into_owned());
        }
    }
    Ok(out)
}

/// Guard RC2-A5: ninguna escritura bajo un gitlink puede quedar fuera del
/// snapshot. Inicializado → el puntero se captura desde el árbol base y los
/// cambios se rechazan vía status (RC1). No inicializado → si hay CUALQUIER
/// contenido en el directorio, no se puede garantizar captura completa:
/// rechazo explícito (jamás tratarlo como archivos comunes).
fn check_gitlink_workdir(workdir: &std::path::Path, path: &str) -> Result<()> {
    let dir = workdir.join(path);
    if !dir.exists() {
        return Ok(());
    }
    if dir.join(".git").exists() {
        return Ok(()); // inicializado: puntero en árbol base + status (RC1)
    }
    let mut entries = std::fs::read_dir(&dir)?;
    if entries.next().is_some() {
        return Err(Error::unsupported(format!(
            "submódulo no inicializado con contenido bajo su path: `{path}` — capturar esto no está soportado en v0 \
             (inicializá el submódulo con `git submodule update --init` o quitá esos archivos)"
        )));
    }
    Ok(())
}

fn guard_gitlinks(repo: &Repository, workdir: &std::path::Path) -> Result<()> {
    for path in gitlink_paths(repo)? {
        check_gitlink_workdir(workdir, &path)?;
    }
    Ok(())
}

/// Captura el estado del WORKING TREE como objetos inmutables.
///
/// Devuelve `Ok(None)` si el contenido es idéntico a la base: no hay nada
/// que revisar y no se publica ni un evento `prepared` ni una ref.
///
/// `worktree_repo` debe ser el repositorio ligado al worktree de la tarea (su
/// índice es el que refleja el checkout). El commit resultante tiene como
/// único padre el commit base de la tarea (sin encadenar checkpoints).
pub fn capture(
    worktree_repo: &Repository,
    base_oid: &str,
    message: &str,
) -> Result<Option<Captured>> {
    match worktree_repo.state() {
        git2::RepositoryState::Clean => {}
        other => {
            return Err(Error::unsupported(format!(
                "estado de Git no soportado en el worktree ({other:?})"
            )));
        }
    }
    let base = Oid::from_str(base_oid)?;
    let base_commit = worktree_repo.find_commit(base)?;
    let base_tree = base_commit.tree()?;
    let workdir = worktree_repo
        .workdir()
        .ok_or_else(|| Error::unsupported("worktree sin working dir"))?
        .to_path_buf();
    // RC2-A5: gitlinks inspeccionados por índice, no por status.
    guard_gitlinks(worktree_repo, &workdir)?;

    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false);

    let statuses: Statuses = worktree_repo.statuses(Some(&mut opts))?;

    let mut builder = git2::build::TreeUpdateBuilder::new();
    for entry in statuses.iter() {
        let Ok(rel) = entry.path() else { continue };
        let abs = workdir.join(rel);
        // Gitlink/módulo o directorio: rechazo explícito ANTES de mirar disco.
        if let Some(why) = unsupported_entry(&entry, &abs) {
            return Err(Error::unsupported(format!(
                "cambio no soportado en snapshot v0 ({why}): {rel}"
            )));
        }
        let meta = std::fs::symlink_metadata(&abs);
        let Ok(meta) = meta else {
            // No está en disco: borrado respecto del índice.
            builder.remove(rel);
            continue;
        };
        let (content, mode) = if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&abs)?;
            (
                target.to_string_lossy().into_owned().into_bytes(),
                git2::FileMode::Link,
            )
        } else {
            let bytes = std::fs::read(&abs)?;
            if bytes.len() > MAX_FILE_BYTES {
                return Err(Error::unsupported(format!(
                    "archivo demasiado grande para snapshot v0 ({} bytes): {rel}",
                    bytes.len()
                )));
            }
            if bytes.contains(&0u8) {
                return Err(Error::unsupported(format!(
                    "archivo binario no soportado en snapshot v0: {rel}"
                )));
            }
            let exec = exec_bit(&meta);
            (
                bytes,
                if exec {
                    git2::FileMode::BlobExecutable
                } else {
                    git2::FileMode::Blob
                },
            )
        };
        let blob_oid = worktree_repo.blob(&content)?;
        builder.upsert(rel, blob_oid, mode);
    }
    let tree_oid = builder.create_updated(worktree_repo, &base_tree)?;
    let tree = worktree_repo.find_tree(tree_oid)?;

    let sig = Signature::now("Aeril", "aeril@localhost")?;
    let commit_oid = worktree_repo.commit(None, &sig, &sig, message, &tree, &[&base_commit])?;

    let files = crate::git::diff::files_between(worktree_repo, base_oid, &commit_oid.to_string())?;
    if files.is_empty() {
        // Sin cambios reales: el commit queda huérfano (inofensivo) y NO se
        // publica ref ni evento. Un "checkpoint vacío" no es una revisión.
        return Ok(None);
    }
    let patch = crate::git::diff::patch_between(worktree_repo, base_oid, &commit_oid.to_string())?;
    Ok(Some(Captured {
        commit_oid: commit_oid.to_string(),
        tree_oid: tree.id().to_string(),
        files,
        patch,
    }))
}

fn exec_bit(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

/// OID del tree que representa el estado actual del worktree (sin crear commit).
/// Se usa para validar que un workspace coincide con un snapshot exacto.
pub fn current_tree(worktree_repo: &Repository, base_oid: &str) -> Result<String> {
    let base = Oid::from_str(base_oid)?;
    let base_tree = worktree_repo.find_commit(base)?.tree()?;
    let workdir = worktree_repo
        .workdir()
        .ok_or_else(|| Error::unsupported("worktree sin working dir"))?
        .to_path_buf();
    guard_gitlinks(worktree_repo, &workdir)?;
    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false);

    let statuses = worktree_repo.statuses(Some(&mut opts))?;
    let mut builder = git2::build::TreeUpdateBuilder::new();
    for entry in statuses.iter() {
        let Ok(rel) = entry.path() else { continue };
        let abs = workdir.join(rel);
        if let Some(why) = unsupported_entry(&entry, &abs) {
            return Err(Error::unsupported(format!(
                "cambio no soportado ({why}): {rel}"
            )));
        }
        let Ok(meta) = std::fs::symlink_metadata(&abs) else {
            builder.remove(rel);
            continue;
        };
        let (content, mode) = if meta.file_type().is_symlink() {
            (
                std::fs::read_link(&abs)?
                    .to_string_lossy()
                    .into_owned()
                    .into_bytes(),
                git2::FileMode::Link,
            )
        } else {
            let bytes = std::fs::read(&abs)?;
            if bytes.contains(&0u8) {
                return Err(Error::unsupported(format!("archivo binario: {rel}")));
            }
            (
                bytes,
                if exec_bit(&meta) {
                    git2::FileMode::BlobExecutable
                } else {
                    git2::FileMode::Blob
                },
            )
        };
        let blob_oid = worktree_repo.blob(&content)?;
        builder.upsert(rel, blob_oid, mode);
    }
    let tree_oid = builder.create_updated(worktree_repo, &base_tree)?;
    Ok(tree_oid.to_string())
}

#[derive(Debug, PartialEq, Eq)]
pub enum PublishOutcome {
    Created,
    AlreadyExists,
}

/// Publica la ref del checkpoint con semántica de estado esperado:
/// - no existe → crear;
/// - existe con el MISMO OID → retry idempotente, ok (aunque la haya creado
///   otro proceso en paralelo: la creación falla y se re-verifica);
/// - existe con OTRO OID → conflicto de integridad, nunca se sobrescribe.
pub fn publish_ref_cas(
    repo: &Repository,
    ref_name: &str,
    expected_oid: &str,
) -> Result<PublishOutcome> {
    publish_ref_cas_with_hook(repo, ref_name, expected_oid, None)
}

/// Inspecciona la ref sin modificarla. `Ok(None)` significa ausente.
fn inspect_ref(repo: &Repository, ref_name: &str, expected: Oid) -> Result<Option<PublishOutcome>> {
    match repo.find_reference(ref_name) {
        Ok(existing) => {
            let target = existing.target().ok_or_else(|| {
                Error::integrity(format!("{ref_name} es simbólica; se esperaba directa"))
            })?;
            if target == expected {
                Ok(Some(PublishOutcome::AlreadyExists))
            } else {
                Err(Error::conflict(format!(
                    "{ref_name} ya apunta a {target} y se esperaba {expected}: conflicto de integridad; no se sobrescribe"
                )))
            }
        }
        Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Igual que [`publish_ref_cas`] con un `hook` que corre tras observar que la
/// ref no existe y antes de crearla. Solo para tests deterministas de carrera.
///
/// Si libgit2 devuelve `Locked` (otro proceso tiene el `ref.lock`), se
/// reinspecciona la ref y se reintenta hasta un deadline monotónico de cinco
/// segundos. Un lock agotado se devuelve como error transitorio para que el
/// llamador no lo convierta en un fallo terminal: el checkpoint sigue
/// pendiente y puede recuperarse después.
pub fn publish_ref_cas_with_hook(
    repo: &Repository,
    ref_name: &str,
    expected_oid: &str,
    hook: Option<&dyn Fn()>,
) -> Result<PublishOutcome> {
    let expected = Oid::from_str(expected_oid)?;
    let deadline = Instant::now() + REF_LOCK_RETRY_DEADLINE;
    let mut backoff = REF_LOCK_RETRY_INITIAL_BACKOFF;
    let mut last_lock_error: Option<Error> = None;
    let mut hook_called = false;

    loop {
        match inspect_ref(repo, ref_name, expected) {
            Ok(Some(outcome)) => return Ok(outcome),
            Ok(None) => {}
            Err(error) if is_transient_ref_lock(&error) => {
                last_lock_error = Some(error);
            }
            Err(error) => return Err(error),
        }

        if deadline.saturating_duration_since(Instant::now()).is_zero()
            && let Some(error) = last_lock_error.take()
        {
            return Err(error);
        }

        if !hook_called {
            if let Some(h) = hook {
                h();
            }
            hook_called = true;
        }

        match repo.reference(ref_name, expected, false, "aeril: checkpoint") {
            Ok(_) => {
                let check = repo.find_reference(ref_name)?;
                if check.target() != Some(expected) {
                    return Err(Error::integrity(format!(
                        "{ref_name} no quedó en {expected} tras publicarla"
                    )));
                }
                return Ok(PublishOutcome::Created);
            }
            Err(create_error) if create_error.code() == git2::ErrorCode::Locked => {
                last_lock_error = Some(create_error.into());
            }
            Err(create_error) => {
                // Otro proceso pudo crear la ref en paralelo: misma
                // (name, oid) = éxito idempotente; otro OID = conflicto.
                return match inspect_ref(repo, ref_name, expected)? {
                    Some(outcome) => Ok(outcome),
                    None => Err(create_error.into()),
                };
            }
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(last_lock_error.unwrap_or_else(|| {
                Error::Other(format!("deadline agotado esperando el lock de {ref_name}"))
            }));
        }
        std::thread::sleep(backoff.min(remaining));
        backoff = (backoff * 2).min(REF_LOCK_RETRY_MAX_BACKOFF);
    }
}

/// Verifica ref + commit + tree + blobs de los archivos cambiados contra lo
/// que el snapshot declara. Un fallo aquí es condición de integridad, no un
/// resultado "recuperado".
pub fn verify_snapshot(
    repo: &Repository,
    ref_name: &str,
    expected_oid: &str,
    files: &[FileSummary],
) -> Result<()> {
    let expected = Oid::from_str(expected_oid)?;
    let reference = repo
        .find_reference(ref_name)
        .map_err(|e| Error::integrity(format!("ref {ref_name} ausente: {e}")))?;
    let target = reference
        .target()
        .ok_or_else(|| Error::integrity(format!("{ref_name} simbólica")))?;
    if target != expected {
        return Err(Error::integrity(format!(
            "{ref_name} apunta a {target}, se esperaba {expected}"
        )));
    }
    let commit = repo
        .find_commit(expected)
        .map_err(|e| Error::integrity(format!("commit {expected} ausente: {e}")))?;
    let tree = commit.tree()?;
    for f in files {
        if let Some(after) = &f.after_blob_oid {
            let oid = Oid::from_str(after)?;
            repo.find_blob(oid)
                .map_err(|e| Error::integrity(format!("blob {oid} ({}) ausente: {e}", f.path)))?;
            let entry = tree
                .get_path(Path::new(&f.path))
                .map_err(|e| Error::integrity(format!("{} no está en el tree: {e}", f.path)))?;
            if entry.id() != oid {
                return Err(Error::integrity(format!(
                    "{}: el tree apunta a {} y el snapshot declara {oid}",
                    f.path,
                    entry.id()
                )));
            }
        }
    }
    Ok(())
}

/// Lista las refs de checkpoint del repo (el recolector de v0 solo las lista).
pub fn list_checkpoint_refs(repo: &Repository) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    let glob = repo.references_glob("refs/aeril/checkpoints/*")?;
    for r in glob {
        let r = r?;
        if let (Ok(name), Some(target)) = (r.name(), r.target()) {
            out.push((name.to_string(), target.to_string()));
        }
    }
    out.sort();
    Ok(out)
}
