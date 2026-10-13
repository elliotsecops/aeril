//! Integración acotada de v0: SOLO fast-forward a una rama destino explícita.
//! Nunca force-update; divergencia, destino sucio o ref movida = bloqueo
//! comprensible que conserva todo el trabajo.

use git2::Repository;

#[derive(Debug)]
pub enum IntegrationBlocked {
    TargetMissing(String),
    TargetCheckedOut(String),
    Stale { expected: String, actual: String },
    Diverged { target: String, source: String },
    Git(String),
}

impl IntegrationBlocked {
    pub fn code(&self) -> &'static str {
        match self {
            IntegrationBlocked::TargetMissing(_) => "target_missing",
            IntegrationBlocked::TargetCheckedOut(_) => "target_checked_out",
            IntegrationBlocked::Stale { .. } => "stale_target",
            IntegrationBlocked::Diverged { .. } => "diverged",
            IntegrationBlocked::Git(_) => "git_error",
        }
    }

    pub fn describe(&self) -> String {
        match self {
            IntegrationBlocked::TargetMissing(b) => format!("la rama destino {b} no existe"),
            IntegrationBlocked::TargetCheckedOut(b) => format!(
                "la rama destino {b} está siendo usada por un checkout: integrá desde un checkout limpio"
            ),
            IntegrationBlocked::Stale { expected, actual } => format!(
                "la rama destino se movió (esperaba {expected}, ahora {actual}): revisá de nuevo antes de integrar"
            ),
            IntegrationBlocked::Diverged { target, source } => format!(
                "la base cambió: {source} no desciende de {target}; se requiere merge (no soportado en v0)"
            ),
            IntegrationBlocked::Git(e) => format!("error de git: {e}"),
        }
    }
}

#[derive(Debug)]
pub struct IntegrationOutcome {
    pub target_oid_before: String,
    pub target_oid_after: String,
}

/// Fast-forward de `target_branch` (debe estar exactamente en `expect_oid`)
/// al commit `source_oid`. Verifica ancestría y pertenencia de checkout.
pub fn fast_forward(
    repo: &Repository,
    target_branch: &str,
    source_oid: &str,
    expect_oid: &str,
) -> std::result::Result<IntegrationOutcome, IntegrationBlocked> {
    let full_ref = format!("refs/heads/{target_branch}");

    if let Some(worktree) = checked_out_worktree(repo, target_branch) {
        return Err(IntegrationBlocked::TargetCheckedOut(worktree));
    }

    let reference = repo
        .find_reference(&full_ref)
        .map_err(|_| IntegrationBlocked::TargetMissing(target_branch.to_string()))?;
    let target = reference.target().ok_or_else(|| {
        IntegrationBlocked::TargetMissing(format!("{target_branch} (ref no directa)"))
    })?;
    let target_s = target.to_string();
    if target_s != expect_oid {
        return Err(IntegrationBlocked::Stale {
            expected: expect_oid.to_string(),
            actual: target_s,
        });
    }
    if target_s == source_oid {
        // No-op: ya está integrado.
        return Ok(IntegrationOutcome {
            target_oid_before: target_s.clone(),
            target_oid_after: target_s,
        });
    }

    let source =
        git2::Oid::from_str(source_oid).map_err(|e| IntegrationBlocked::Git(e.to_string()))?;
    if repo.find_commit(source).is_err() {
        return Err(IntegrationBlocked::Git(format!(
            "commit fuente {source_oid} ausente"
        )));
    }
    let is_descendant = repo
        .graph_descendant_of(source, target)
        .map_err(|e| IntegrationBlocked::Git(e.to_string()))?;
    if !is_descendant {
        return Err(IntegrationBlocked::Diverged {
            target: target_s,
            source: source_oid.to_string(),
        });
    }

    // Movimiento de ref sin force: es fast-forward por construcción.
    let mut r = repo
        .find_reference(&full_ref)
        .map_err(|e| IntegrationBlocked::Git(e.to_string()))?;
    r.set_target(source, "aeril: integración fast-forward")
        .map_err(|e| IntegrationBlocked::Git(e.to_string()))?;

    let after = repo
        .find_reference(&full_ref)
        .ok()
        .and_then(|r| r.target())
        .map(|o| o.to_string())
        .unwrap_or_default();
    if after != source_oid {
        return Err(IntegrationBlocked::Git(format!(
            "la ref quedó en {after}, se esperaba {source_oid}"
        )));
    }
    Ok(IntegrationOutcome {
        target_oid_before: target_s,
        target_oid_after: source_oid.to_string(),
    })
}

/// Devuelve una descripción del worktree que tiene la rama chequeada, si hay.
fn checked_out_worktree(repo: &Repository, branch: &str) -> Option<String> {
    if let Ok(head) = repo.head() {
        if head.is_branch() && head.shorthand().ok() == Some(branch) {
            return Some("checkout principal".to_string());
        }
    }
    if let Ok(names) = repo.worktrees() {
        for name in names.iter() {
            let Ok(Some(name)) = name else { continue };
            let Ok(wt) = repo.find_worktree(name) else {
                continue;
            };
            let Ok(wr) = Repository::open_from_worktree(&wt) else {
                continue;
            };
            if let Ok(head) = wr.head() {
                if head.is_branch() && head.shorthand().ok() == Some(branch) {
                    return Some(format!("worktree {name}"));
                }
            }
        }
    }
    None
}
