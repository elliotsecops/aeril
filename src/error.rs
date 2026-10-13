//! Errores estructurados de Aeril.
//!
//! Regla: cada fallo tiene una categoría estable; el CLI decide el exit code y
//! la UI muestra mensaje. Nunca se clasifica trabajo incompleto como exitoso.

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("store: {0}")]
    Store(#[from] rusqlite::Error),

    #[error("git: {0}")]
    Git(#[from] git2::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// Uso incorrecto de la API o entrada inválida del usuario.
    #[error("entrada inválida: {0}")]
    Invalid(String),

    #[error("no encontrado: {0}")]
    NotFound(String),

    /// Conflicto con estado existente (refs divergentes, secuencia, etc.).
    #[error("conflicto: {0}")]
    Conflict(String),

    /// Violación de integridad detectable (objetos faltantes, OID inesperado).
    #[error("integridad: {0}")]
    Integrity(String),

    /// Estado de Git no soportado explícitamente rechazado.
    #[error("no soportado: {0}")]
    Unsupported(String),

    /// La operación requiere un checkpoint `ready` que no existe.
    #[error("checkpoint no listo: {0}")]
    NotReady(String),

    /// Fallo del backend de agentes (proceso, protocolo, cancelación).
    #[error("backend: {0}")]
    Backend(String),

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn invalid(msg: impl Into<String>) -> Self {
        Error::Invalid(msg.into())
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Error::Conflict(msg.into())
    }
    pub fn integrity(msg: impl Into<String>) -> Self {
        Error::Integrity(msg.into())
    }
    pub fn unsupported(msg: impl Into<String>) -> Self {
        Error::Unsupported(msg.into())
    }
    pub fn backend(msg: impl Into<String>) -> Self {
        Error::Backend(msg.into())
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Error::NotFound(msg.into())
    }
    pub fn not_ready(msg: impl Into<String>) -> Self {
        Error::NotReady(msg.into())
    }
}
