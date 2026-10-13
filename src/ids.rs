//! Identidades estables de Aeril.
//!
//! `repo_id`, `workspace_id` y `snapshot_id` son identidades; las rutas son
//! atributos actualizables y las refs Git son locators de contenido.

use uuid::Uuid;

/// ID nuevo aleatorio (32 hex, sin guiones).
pub fn new_id() -> String {
    Uuid::new_v4().simple().to_string()
}

/// Identidad estable de un repositorio derivada del path canónico de su
/// directorio común (`.git`). Si el repo se mueve, la identidad cambia: es una
/// limitación documentada del ensayo (una tabla `repos` la resolverá después).
pub fn repo_id_for(repo_common_dir: &std::path::Path) -> String {
    let canonical = repo_common_dir
        .canonicalize()
        .unwrap_or_else(|_| repo_common_dir.to_path_buf());
    Uuid::new_v5(&Uuid::NAMESPACE_URL, canonical.to_string_lossy().as_bytes())
        .simple()
        .to_string()
}

/// Nombre de ref de checkpoint para un snapshot.
pub fn checkpoint_ref(snapshot_id: &str) -> String {
    format!("refs/aeril/checkpoints/{snapshot_id}")
}

/// Nombre de ref de cuarentena (el ensayo solo la reserva; no libera nada).
pub fn quarantine_ref(snapshot_id: &str) -> String {
    format!("refs/aeril/quarantine/{snapshot_id}")
}

/// Nombre de rama de workspace.
pub fn workspace_branch(workspace_id: &str) -> String {
    format!("aeril/ws/{workspace_id}")
}
