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
    /// `true` solo si ESTA llamada movió la ref. Un no-op idempotente
    /// (target ya en source, o otro writer ganó) no es un éxito nuevo.
    pub performed: bool,
}

/// Fast-forward de `target_branch` (debe estar exactamente en `expect_oid`)
/// al commit `source_oid`. Verifica ancestría y pertenencia de checkout.
pub fn fast_forward(
    repo: &Repository,
    target_branch: &str,
    source_oid: &str,
    expect_oid: &str,
) -> std::result::Result<IntegrationOutcome, IntegrationBlocked> {
    fast_forward_with_hook(repo, target_branch, source_oid, expect_oid, None)
}

/// Igual que [`fast_forward`] con un `hook` que corre DESPUÉS de la
/// verificación de ancestría y ANTES del CAS. Solo para tests deterministas
/// de concurrencia (simula a otro escritor avanzando la rama justo ahí).
pub fn fast_forward_with_hook(
    repo: &Repository,
    target_branch: &str,
    source_oid: &str,
    expect_oid: &str,
    hook: Option<&dyn Fn()>,
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
            performed: false,
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

    if let Some(h) = hook {
        h();
    }

    // CAS real: transacción de refs de libgit2. `lock_ref` fija el valor
    // observado; `commit` falla con GIT_EMODIFIED si otro escritor movió la
    // ref entretanto. Un avance concurrente nunca se sobrescribe: el commit
    // ajeno queda y este intento se reporta como stale.
    let mut tx = repo.transaction().map_err(|e| {
        IntegrationBlocked::Git(format!("no se pudo abrir la transacción de refs: {e}"))
    })?;
    tx.lock_ref(&full_ref)
        .map_err(|e| IntegrationBlocked::Git(format!("no se pudo bloquear {full_ref}: {e}")))?;
    let locked = repo
        .find_reference(&full_ref)
        .ok()
        .and_then(|r| r.target())
        .map(|o| o.to_string());
    match locked.as_deref() {
        Some(cur) if cur == source_oid => {
            // Otro escritor completó la misma integración: idempotente.
            return Ok(IntegrationOutcome {
                target_oid_before: target_s,
                target_oid_after: source_oid.to_string(),
                performed: false,
            });
        }
        Some(cur) if cur == expect_oid => {}
        Some(cur) => {
            return Err(IntegrationBlocked::Stale {
                expected: expect_oid.to_string(),
                actual: cur.to_string(),
            });
        }
        None => {
            return Err(IntegrationBlocked::Git(format!(
                "{full_ref} desapareció durante la integración"
            )));
        }
    }
    let sig = git2::Signature::now("Aeril", "aeril@localhost")
        .map_err(|e| IntegrationBlocked::Git(e.to_string()))?;
    tx.set_target(
        &full_ref,
        source,
        Some(&sig),
        "aeril: integración fast-forward",
    )
    .map_err(|e| IntegrationBlocked::Git(e.to_string()))?;
    match tx.commit() {
        Ok(()) => {}
        Err(e) if e.code() == git2::ErrorCode::Modified => {
            let actual = repo
                .find_reference(&full_ref)
                .ok()
                .and_then(|r| r.target())
                .map(|o| o.to_string())
                .unwrap_or_default();
            return Err(IntegrationBlocked::Stale {
                expected: expect_oid.to_string(),
                actual,
            });
        }
        Err(e) => return Err(IntegrationBlocked::Git(e.to_string())),
    }

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
        performed: true,
    })
}

/// Devuelve una descripción del worktree que tiene la rama chequeada, si hay.
pub fn checked_out_worktree(repo: &Repository, branch: &str) -> Option<String> {
    if let Ok(head) = repo.head()
        && head.is_branch()
        && head.shorthand().ok() == Some(branch)
    {
        return Some("checkout principal".to_string());
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
            if let Ok(head) = wr.head()
                && head.is_branch()
                && head.shorthand().ok() == Some(branch)
            {
                return Some(format!("worktree {name}"));
            }
        }
    }
    None
}
