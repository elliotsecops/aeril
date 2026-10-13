//! Capa Git de Aeril: repositorios validados, workspaces verificados,
//! snapshots inmutables, diffs e integración fast-forward.

pub mod diff;
pub mod integrate;
pub mod snapshot;
pub mod worktree;

use crate::error::{Error, Result};
use git2::{Repository, RepositoryState};
use std::path::{Path, PathBuf};

/// Abre un repositorio y rechaza explícitamente los estados no soportados.
pub fn open(path: &Path) -> Result<Repository> {
    let repo = Repository::open(path).map_err(|e| {
        Error::invalid(format!(
            "no se pudo abrir el repositorio {}: {e}",
            path.display()
        ))
    })?;
    validate(&repo)?;
    Ok(repo)
}

pub fn validate(repo: &Repository) -> Result<()> {
    if repo.is_bare() {
        return Err(Error::unsupported(
            "repositorio bare: se requiere working tree para el ensayo",
        ));
    }
    if repo.workdir().is_none() {
        return Err(Error::unsupported("repositorio sin working directory"));
    }
    match repo.state() {
        RepositoryState::Clean => {}
        other => {
            return Err(Error::unsupported(format!(
                "estado de Git no soportado ({other:?}): merge/rebase/cherry-pick/bisect en curso"
            )));
        }
    }
    if repo.is_empty()? {
        return Err(Error::unsupported(
            "repositorio sin commits: creá un commit base antes de usar Aeril",
        ));
    }
    Ok(())
}

/// Identidad y estado del repositorio registrado por un thread.
#[derive(Debug, Clone)]
pub struct RepoInfo {
    pub repo_id: String,
    pub path: PathBuf,
    pub head_oid: String,
}

pub fn info(repo: &Repository) -> Result<RepoInfo> {
    let head = repo.head()?.peel_to_commit()?.id();
    let path = repo
        .workdir()
        .ok_or_else(|| Error::unsupported("sin working tree"))?
        .to_path_buf();
    Ok(RepoInfo {
        repo_id: crate::ids::repo_id_for(repo.commondir()),
        path,
        head_oid: head.to_string(),
    })
}

/// OID del commit de HEAD (base por defecto de una tarea fresca).
pub fn head_oid(repo: &Repository) -> Result<String> {
    Ok(repo.head()?.peel_to_commit()?.id().to_string())
}
