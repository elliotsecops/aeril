//! Workspaces verificados: worktrees ligados a un `workspace_id` estable.
//!
//! Reglas:
//! - Un worktree NUNCA es un sandbox (invariante 8); separa archivos, no permisos.
//! - Aeril crea y valida el workspace antes de que el agente escriba.
//! - Un workspace inválido o con base distinta NO se reutiliza en silencio.
//! - La recreación (tras borrar el worktree) es explícita y se registra.

use crate::error::{Error, Result};
use crate::ids::{new_id, workspace_branch};
use git2::{Oid, Repository, WorktreeAddOptions};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Workspace {
    pub workspace_id: String,
    pub path: PathBuf,
    pub branch: String,
    pub base_oid: String,
}

/// Resultado de verificar un workspace existente.
#[derive(Debug)]
pub enum VerifyOutcome {
    /// Válido y con la base esperada.
    Ok,
    /// No está registrado como worktree.
    Missing,
    /// Registrado pero inservible (directorio borrado, gitdir roto).
    Invalid(String),
    /// Existe pero no coincide (base o path distintos): no se reutiliza.
    Mismatched(String),
}

fn worktree_dir(data_dir: &Path, workspace_id: &str) -> PathBuf {
    data_dir.join("worktrees").join(workspace_id)
}

/// Path del worktree de un workspace (identidad estable → atributo derivable).
pub fn path_for(data_dir: &Path, workspace_id: &str) -> PathBuf {
    worktree_dir(data_dir, workspace_id)
}

/// Crea un workspace verificado: rama propia en `base_oid` + worktree ligado.
pub fn create(
    repo: &Repository,
    data_dir: &Path,
    base_oid: &str,
    workspace_id: Option<&str>,
) -> Result<Workspace> {
    let workspace_id = workspace_id.map(str::to_string).unwrap_or_else(new_id);
    let base = Oid::from_str(base_oid)?;
    let base_commit = repo.find_commit(base)?;

    let path = worktree_dir(data_dir, &workspace_id);
    if path.exists() {
        return Err(Error::conflict(format!(
            "el path del workspace ya existe: {} (no se reutiliza a ciegas)",
            path.display()
        )));
    }
    std::fs::create_dir_all(path.parent().expect("worktrees/ siempre tiene padre"))?;

    let branch = workspace_branch(&workspace_id);
    if repo.find_branch(&branch, git2::BranchType::Local).is_ok() {
        return Err(Error::conflict(format!(
            "la rama {branch} ya existe: workspace inconsistente"
        )));
    }
    repo.branch(&branch, &base_commit, false)?;
    let branch_ref = repo.find_reference(&format!("refs/heads/{branch}"))?;

    let mut opts = WorktreeAddOptions::new();
    opts.reference(Some(&branch_ref));
    let wt_name = format!("aeril-{workspace_id}");
    repo.worktree(&wt_name, &path, Some(&mut opts))?;

    // Verificación inmediata: el worktree existe, abre, y su HEAD es la base.
    let wt = repo.find_worktree(&wt_name)?;
    let wt_repo = Repository::open_from_worktree(&wt)?;
    let head = wt_repo.head()?.peel_to_commit()?.id();
    if head != base {
        return Err(Error::integrity(format!(
            "worktree recién creado con HEAD {head}, se esperaba {base}"
        )));
    }
    Ok(Workspace {
        workspace_id,
        path,
        branch,
        base_oid: base_oid.to_string(),
    })
}

/// Nombre interno del worktree git para un workspace.
fn wt_name(workspace_id: &str) -> String {
    format!("aeril-{workspace_id}")
}

/// Verifica un workspace existente contra la base y el path esperados.
pub fn verify(
    repo: &Repository,
    data_dir: &Path,
    workspace_id: &str,
    expected_base: &str,
) -> VerifyOutcome {
    let expected_path = worktree_dir(data_dir, workspace_id);
    let wt = match repo.find_worktree(&wt_name(workspace_id)) {
        Ok(wt) => wt,
        Err(_) => return VerifyOutcome::Missing,
    };
    // El directorio admin registra el path original; si el trabajo se borró, open falla.
    let wt_repo = match Repository::open_from_worktree(&wt) {
        Ok(r) => r,
        Err(e) => return VerifyOutcome::Invalid(format!("worktree no abrible: {e}")),
    };
    let head = match wt_repo.head().and_then(|h| h.peel_to_commit()) {
        Ok(c) => c.id().to_string(),
        Err(e) => return VerifyOutcome::Invalid(format!("HEAD ilegible: {e}")),
    };
    if head != expected_base {
        return VerifyOutcome::Mismatched(format!("HEAD {head} ≠ base {expected_base}"));
    }
    match wt_repo.workdir() {
        Some(dir) if dir == expected_path.as_path() => VerifyOutcome::Ok,
        Some(dir) => VerifyOutcome::Mismatched(format!(
            "worktree en {} ≠ {}",
            dir.display(),
            expected_path.display()
        )),
        None => VerifyOutcome::Invalid("worktree sin working dir".into()),
    }
}

/// Elimina el worktree del filesystem y su registro admin. No toca la rama.
pub fn remove(repo: &Repository, data_dir: &Path, workspace_id: &str) -> Result<()> {
    let path = worktree_dir(data_dir, workspace_id);
    if path.exists() {
        std::fs::remove_dir_all(&path)?;
    }
    prune_admin(repo, workspace_id)?;
    Ok(())
}

/// Borra el directorio administrativo del worktree (equivalente a
/// `git worktree prune` para este nombre; la rama no se toca).
fn prune_admin(repo: &Repository, workspace_id: &str) -> Result<()> {
    let admin = repo
        .commondir()
        .join("worktrees")
        .join(wt_name(workspace_id));
    if admin.exists() {
        std::fs::remove_dir_all(&admin)?;
    }
    Ok(())
}

/// Recrea un workspace explícitamente (tras borrado externo) desde `from_oid`.
/// La rama previa del workspace se re-crea en esa base: es decisión de Aeril.
pub fn recreate(
    repo: &Repository,
    data_dir: &Path,
    workspace_id: &str,
    from_oid: &str,
) -> Result<Workspace> {
    prune_admin(repo, workspace_id)?;
    let path = worktree_dir(data_dir, workspace_id);
    if path.exists() {
        std::fs::remove_dir_all(&path)?;
    }
    let branch = workspace_branch(workspace_id);
    if let Ok(mut b) = repo.find_branch(&branch, git2::BranchType::Local) {
        // La rama es de Aeril (refs/heads/aeril/ws/<id>): se puede re-crear.
        b.delete()?;
    }
    create(repo, data_dir, from_oid, Some(workspace_id))
}

/// Abre el repositorio del worktree (para statuses y captura).
pub fn open_worktree(repo: &Repository, workspace_id: &str) -> Result<Repository> {
    let wt = repo.find_worktree(&wt_name(workspace_id))?;
    Ok(Repository::open_from_worktree(&wt)?)
}
