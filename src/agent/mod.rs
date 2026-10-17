//! Backends de agentes. El motor es externo y se habla por contrato
//! estructurado (ACP); Aeril no embebe runtime de agentes.

pub mod acp;
pub mod mock;

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct BackendSpec {
    pub name: String,
    pub version: String,
    pub program: String,
    pub args: Vec<String>,
    /// Overrides de entorno (P2-S1: perfiles gestionados). Vacío = heredar.
    pub env: Vec<(String, String)>,
}

/// Resuelve un nombre de backend a un comando concreto.
/// - `hermes` → `hermes acp` (backend inicial opcional).
/// - `mock:<script>` → el propio binario de Aeril en modo `__mock-agent`
///   (transporte ACP simulado para tests deterministas).
pub fn resolve_backend(name: &str) -> Result<BackendSpec> {
    if let Some(script) = name.strip_prefix("mock:") {
        let exe = std::env::current_exe()
            .map_err(|e| Error::backend(format!("no se pudo resolver el binario actual: {e}")))?;
        return Ok(BackendSpec {
            name: "mock".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            program: exe.to_string_lossy().into_owned(),
            args: vec!["__mock-agent".into(), "--script".into(), script.to_string()],
            env: Vec::new(),
        });
    }
    match name {
        "hermes" => {
            let version =
                detect_version("hermes", &["acp", "--version"]).unwrap_or_else(|| "unknown".into());
            Ok(BackendSpec {
                name: "hermes".into(),
                version,
                program: "hermes".into(),
                args: vec!["acp".into()],
                env: Vec::new(),
            })
        }
        other => Err(Error::invalid(format!(
            "backend desconocido: `{other}` (v0 soporta `hermes` y `mock:<script>`)"
        ))),
    }
}

pub fn detect_version(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
}
