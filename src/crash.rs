//! Inyección de fallos para el ensayo (Fase 0).
//!
//! Dos mecanismos, ambos terminan el proceso como una caída real:
//! - `AERIL_CRASH_AFTER=<punto>`: escribe el marcador (si `AERIL_CRASH_MARK`)
//!   y se auto-envía SIGKILL. Determinista e inmediato.
//! - `AERIL_PAUSE_AFTER=<punto>`: escribe el marcador y se queda dormido; el
//!   test externo espera el marcador y mata el proceso con SIGKILL.
//!
//! Puntos definidos por la captura de checkpoints: `objects`, `prepared`,
//! `ref`, `verify`. Nada de esto se activa sin las env vars.

use std::time::Duration;

fn mark(point: &str) {
    if let Ok(path) = std::env::var("AERIL_CRASH_MARK") {
        let _ = std::fs::write(path, format!("{point}\n"));
    }
}

/// Punto de fallo: si el punto coincide con la env var, muere el proceso.
pub fn point(name: &str) {
    let should_kill = std::env::var("AERIL_CRASH_AFTER").ok().as_deref() == Some(name);
    let should_pause = std::env::var("AERIL_PAUSE_AFTER").ok().as_deref() == Some(name);
    if should_kill {
        mark(name);
        unsafe {
            libc::kill(libc::getpid(), libc::SIGKILL);
        }
        unreachable!("SIGKILL no debería retornar");
    }
    if should_pause {
        mark(name);
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
}
