//! P2-S1 — Perfiles Hermes gestionados por Aeril (detrás de feature flag).
//! P2-S1 R1 — endurecimiento tras auditoría independiente (4 P1 + H1/H2).
//!
//! Un perfil vive en `<AERIL_DATA_DIR>/hermes/<agent_id>/`:
//!
//! ```text
//! <agent_id>/
//!   home/         HERMES_HOME (estado Hermes completo)
//!   san-home/     HOME sanitizado (XDG_* y GH_CONFIG_DIR derivan de acá)
//!   profile.json  metadata de Aeril (0600; NUNCA credenciales)
//!   aeril.lock    flock de escritor único (retenido durante el writer ACP;
//!                 el fd se hereda al backend ⇒ sobrevive la muerte del supervisor)
//!   .meta.lock    flock que serializa read-modify-write de profile.json
//!   owner.json    evidencia (pid+starttime+pgid) del último warm/escritor
//! agents.json     registro agent_id→perfil y thread→perfil (0600)
//! .registry.lock  flock del registro
//! ```
//!
//! Invariantes (P2-S1 R1):
//! - **P1-1**: ningún perfil se borra ni muta su registro si hay runs
//!   asociados no-terminales (por perfil durable `runs.profile` o por thread);
//!   la DB inaccesible o la consulta fallida ⇒ HOLD fail-closed (jamás
//!   `unwrap_or(0)`); `--clear-threads` solo cubre asociaciones históricas.
//! - **P1-2**: `logged_in` solo tras respuesta inequívoca del CLI de Hermes
//!   (exit 0, provider esperado, estado positivo exacto, sin contradicción);
//!   timeout/exit≠0/salida ambigua ⇒ verdicto fresco `unknown` persistido.
//! - **P1-3**: el pre-warm acepta solo una respuesta `initialize` JSON-RPC 2.0
//!   con id coincidente, result válido para ACP v1, frame ≤ 1 MiB y deadline
//!   fijo; notificaciones no pueden suplantar la respuesta.
//! - **P1-4**: exclusión de escritor por HERMES_HOME aun si el supervisor muere:
//!   flock heredado por el child + verificación fail-closed de procesos
//!   sobrevivientes (pid+starttime) antes de admitir un escritor nuevo.
//! - **H1**: mountinfo/locks/procesos ilegibles ⇒ fail-closed en operaciones
//!   destructivas; lock "ausente" se distingue de lock "no verificable".
//! - **H2**: temporales únicos O_EXCL/O_NOFOLLOW, fsync de archivo y directorio
//!   propagados, RMW de metadata serializado por `.meta.lock`.
//! - Aeril **nunca** abre/parsea/copia `auth.json`; el estado de auth se lee
//!   solo por la CLI de Hermes (`auth status`) — metadata-only, sin secretos.

use crate::error::{Error, Result};
use crate::store::leases::process_start_time;
use crate::timeutil::now_ms;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const FLAG_ENV: &str = "AERIL_HERMES_PROFILES";
pub const WARM_TIMEOUT_ENV: &str = "AERIL_PROFILE_WARM_TIMEOUT_SECS";
pub const AUTH_TIMEOUT_ENV: &str = "AERIL_PROFILE_AUTH_TIMEOUT_SECS";
pub const HERMES_BIN_ENV: &str = "AERIL_HERMES_BIN";

/// Cota dura de un frame JSON-RPC de initialize (P1-3). Hermes real emite una
/// sola línea pequeña; 1 MiB es holgado y evita memoria no acotada.
const FRAME_MAX_BYTES: usize = 1024 * 1024;
/// Cota de bytes totales leídos durante el handshake de warm.
const STREAM_MAX_BYTES: usize = 8 * 1024 * 1024;
/// Cota de salida aceptada del CLI de auth (P1-2).
const AUTH_OUTPUT_MAX_BYTES: usize = 64 * 1024;

/// ¿Está habilitado el subsistema de perfiles? (default OFF)
pub fn enabled() -> bool {
    matches!(
        std::env::var(FLAG_ENV).ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

pub fn is_hermes_backend(spec_name: &str) -> bool {
    spec_name == "hermes"
}

// ── paths y validación ───────────────────────────────────────────────────────

fn root(data_dir: &Path) -> PathBuf {
    data_dir.join("hermes")
}

fn agent_dir(data_dir: &Path, agent_id: &str) -> PathBuf {
    root(data_dir).join(agent_id)
}

pub fn home_dir(data_dir: &Path, agent_id: &str) -> PathBuf {
    agent_dir(data_dir, agent_id).join("home")
}

pub fn san_home_dir(data_dir: &Path, agent_id: &str) -> PathBuf {
    agent_dir(data_dir, agent_id).join("san-home")
}

fn meta_path(data_dir: &Path, agent_id: &str) -> PathBuf {
    agent_dir(data_dir, agent_id).join("profile.json")
}

fn meta_lock_path(data_dir: &Path, agent_id: &str) -> PathBuf {
    agent_dir(data_dir, agent_id).join(".meta.lock")
}

fn owner_path(data_dir: &Path, agent_id: &str) -> PathBuf {
    agent_dir(data_dir, agent_id).join("owner.json")
}

/// Abre un archivo sin seguir el componente final. `O_NOFOLLOW` protege el
/// último componente; los llamadores validan el directorio padre.
fn open_nofollow(path: &Path, create: bool, write: bool) -> Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    if write {
        opts.write(true);
    }
    if create {
        opts.create(true).truncate(false).mode(0o600);
    }
    opts.custom_flags(libc::O_NOFOLLOW).open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::NotFound(format!("{} no existe", path.display()))
        } else {
            Error::conflict(format!(
                "no se pudo abrir {} sin seguir symlinks ({e}) — fail-closed",
                path.display()
            ))
        }
    })
}

fn read_file_nofollow(path: &Path) -> Result<Vec<u8>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::not_found(path.display().to_string()));
        }
        Err(e) => return Err(e.into()),
    };
    if meta.file_type().is_symlink() {
        return Err(Error::conflict(format!(
            "{} es un symlink — fail-closed",
            path.display()
        )));
    }
    if !meta.is_file() {
        return Err(Error::conflict(format!(
            "{} no es un archivo regular — fail-closed",
            path.display()
        )));
    }
    let mut f = open_nofollow(path, false, false)?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn require_real_dir(path: &Path, label: &str) -> Result<()> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| {
        Error::conflict(format!(
            "{label} {} no inspeccionable ({e}) — fail-closed",
            path.display()
        ))
    })?;
    if meta.file_type().is_symlink() {
        return Err(Error::conflict(format!(
            "{label} {} es un symlink — fail-closed",
            path.display()
        )));
    }
    if !meta.is_dir() {
        return Err(Error::conflict(format!(
            "{label} {} no es un directorio — fail-closed",
            path.display()
        )));
    }
    Ok(())
}

/// Root canónico del store de perfiles. En modo creación exige que `hermes/`
/// sea un directorio real; jamás atraviesa un symlink de root.
fn safe_root(data_dir: &Path, create: bool) -> Result<PathBuf> {
    let data = std::fs::canonicalize(data_dir).map_err(|e| {
        Error::conflict(format!(
            "data dir {} no inspeccionable ({e}) — fail-closed",
            data_dir.display()
        ))
    })?;
    let candidate = data.join("hermes");
    match std::fs::symlink_metadata(&candidate) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(Error::conflict(format!(
                "el root de perfiles {} es un symlink — fail-closed",
                candidate.display()
            )));
        }
        Ok(meta) if !meta.is_dir() => {
            return Err(Error::conflict(format!(
                "el root de perfiles {} no es un directorio — fail-closed",
                candidate.display()
            )));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
            let mut b = std::fs::DirBuilder::new();
            b.mode(0o700);
            b.create(&candidate).map_err(|e| {
                Error::conflict(format!(
                    "no se pudo crear el root de perfiles {} ({e}) — fail-closed",
                    candidate.display()
                ))
            })?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(candidate),
        Err(e) => return Err(e.into()),
    }
    let real = std::fs::canonicalize(&candidate)?;
    if real.parent() != Some(data.as_path()) {
        return Err(Error::conflict(format!(
            "el root de perfiles {} resuelve fuera del data dir — fail-closed",
            candidate.display()
        )));
    }
    Ok(real)
}

/// Crea un directorio real (no symlink) exigiendo un padre ya verificado.
fn ensure_real_dir(path: &Path, label: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(Error::conflict(format!(
                "{label} {} es un symlink — fail-closed",
                path.display()
            )));
        }
        Ok(meta) if !meta.is_dir() => {
            return Err(Error::conflict(format!(
                "{label} {} no es un directorio — fail-closed",
                path.display()
            )));
        }
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let parent = path
        .parent()
        .ok_or_else(|| Error::integrity(format!("{} sin padre", path.display())))?;
    require_real_dir(parent, "directorio padre")?;
    let mut b = std::fs::DirBuilder::new();
    b.mode(0o700);
    b.create(path)?;
    require_real_dir(path, label)
}

fn assert_profile_components_safe(dir: &Path) -> Result<()> {
    for (name, want_dir) in [("home", true), ("san-home", true)] {
        let p = dir.join(name);
        match std::fs::symlink_metadata(&p) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(Error::conflict(format!(
                    "{} es un symlink — fail-closed",
                    p.display()
                )));
            }
            Ok(meta) if want_dir && !meta.is_dir() => {
                return Err(Error::conflict(format!(
                    "{} no es un directorio — fail-closed",
                    p.display()
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    for name in ["profile.json", "owner.json", "aeril.lock", ".meta.lock"] {
        let p = dir.join(name);
        match std::fs::symlink_metadata(&p) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(Error::conflict(format!(
                    "{} es un symlink — fail-closed",
                    p.display()
                )));
            }
            Ok(meta) if !meta.is_file() => {
                return Err(Error::conflict(format!(
                    "{} no es un archivo regular — fail-closed",
                    p.display()
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// `agent_id` seguro: minúsculas/dígitos/`_`/`-`, 1..=32, sin empezar con `-`.
pub fn validate_agent_id(agent_id: &str) -> Result<()> {
    let ok_len = !agent_id.is_empty() && agent_id.len() <= 32;
    let ok_chars = agent_id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if ok_len && ok_chars && !agent_id.starts_with('-') {
        Ok(())
    } else {
        Err(Error::invalid(format!(
            "agent_id inválido: {agent_id:?} (usar [a-z0-9_-]{{1,32}} sin '-' inicial)"
        )))
    }
}

/// Comprueba que el path no es symlink, que su padre canónico es el root de
/// perfiles y que no hay mountpoints dentro (fail-closed: H1 — si mountinfo no
/// puede leerse, la operación se rechaza en vez de asumir "sin mounts").
fn assert_safe_agent_path(data_dir: &Path, agent_id: &str) -> Result<PathBuf> {
    validate_agent_id(agent_id)?;
    let root_real = safe_root(data_dir, false)?;
    let dir = root_real.join(agent_id);
    if dir.exists() || std::fs::symlink_metadata(&dir).is_ok() {
        let sym = std::fs::symlink_metadata(&dir)?;
        if sym.file_type().is_symlink() {
            return Err(Error::conflict(format!(
                "el path del perfil es un symlink: {} (fail-closed)",
                dir.display()
            )));
        }
        if !sym.is_dir() {
            return Err(Error::conflict(format!(
                "el path del perfil no es un directorio: {}",
                dir.display()
            )));
        }
        let real = std::fs::canonicalize(&dir)?;
        if real.parent() != Some(root_real.as_path()) {
            return Err(Error::conflict(format!(
                "el perfil resuelve fuera del root de perfiles: {} → {}",
                dir.display(),
                real.display()
            )));
        }
        if has_mountpoint_inside(&real)? {
            return Err(Error::conflict(format!(
                "hay un mountpoint dentro del perfil: {} (fail-closed)",
                real.display()
            )));
        }
        assert_profile_components_safe(&real)?;
        Ok(real)
    } else {
        Ok(dir)
    }
}

/// ¿Algún mountpoint del sistema está dentro de `path`?
///
/// H1: un error de lectura de mountinfo NO se interpreta como "no hay
/// mounts": devuelve `Err` y el llamador destructivo debe rechazar.
fn has_mountpoint_inside(path: &Path) -> Result<bool> {
    has_mountpoint_inside_in(path, Path::new("/proc/self/mountinfo"))
}

fn has_mountpoint_inside_in(path: &Path, mountinfo: &Path) -> Result<bool> {
    let info = std::fs::read_to_string(mountinfo).map_err(|e| {
        Error::conflict(format!(
            "no se pudo leer {} ({e}) — fail-closed",
            mountinfo.display()
        ))
    })?;
    let path_s = path.to_string_lossy().to_string();
    for line in info.lines() {
        let Some(mp) = line.split_whitespace().nth(4) else {
            continue;
        };
        let mp = mp
            .replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\012", "\n")
            .replace("\\134", "\\");
        if mp == path_s || mp.starts_with(&(path_s.clone() + "/")) {
            return Ok(true);
        }
    }
    Ok(false)
}

// ── procesos (P1-4 / H1) ─────────────────────────────────────────────────────

/// Identidad de un PID frente a evidencia durable `(pid, starttime)`.
/// Nunca confunde "sin evidencia" con "evidencia que coincide".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PidId {
    /// El PID no existe (muerto y reapeado; o PID inválido).
    Absent,
    /// El proceso existe y su starttime coincide.
    Ours { zombie: bool },
    /// El proceso existe pero con OTRO starttime: PID reutilizado.
    Reused,
    /// Existe pero la identidad no puede establecerse (stat ilegible, sin
    /// starttime registrado). NUNCA autoriza a ignorarlo.
    Unknown,
}

fn pid_identity(pid: i64, recorded_start: u64) -> PidId {
    if pid <= 0 {
        return PidId::Absent;
    }
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return PidId::Absent,
        Err(_) => return PidId::Unknown,
    };
    let Some(after_comm) = stat.rsplit_once(')').map(|(_, a)| a) else {
        return PidId::Unknown;
    };
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    let Some(state) = fields.first().and_then(|s| s.chars().next()) else {
        return PidId::Unknown;
    };
    let Some(actual) = fields.get(19).and_then(|v| v.parse::<u64>().ok()) else {
        return PidId::Unknown;
    };
    if recorded_start == 0 {
        return PidId::Unknown;
    }
    if actual == recorded_start {
        PidId::Ours {
            zombie: matches!(state, 'Z' | 'X'),
        }
    } else {
        PidId::Reused
    }
}

/// Procesos (mismo euid) asociados a `HERMES_HOME=<home>` o a archivos del
/// perfil. Señales, en orden:
///
/// 1. `environ` legible → `HERMES_HOME=<home>` (definitivo).
/// 2. `environ` con EACCES/EPERM: condición ESPERADA en hosts normales
///    (`PR_SET_DUMPABLE=0`: systemd --user, ssh-agent, kwin, …). No es
///    evidencia de propiedad; se buscan señales alternativas legibles
///    (`cmdline` y `fd` apuntando al perfil). Sin señales ⇒ se omite
///    (documentado: los escritores normales de Hermes son inspeccionables;
///    un proceso same-uid malicioso que se oculte queda fuera del contrato
///    de amenaza — ver doc R1 §6).
/// 3. Otros errores de inspección (I/O, /proc raíz ilegible) ⇒ `Err`
///    fail-closed para operaciones destructivas.
///
/// Los procesos de OTRO uid no pueden ser dueños de un perfil 0700 (se
/// omiten; no se rompe el uso multi-usuario normal).
fn processes_using_home(home: &Path) -> Result<Vec<i64>> {
    processes_using_home_in(home, Path::new("/proc"))
}

/// ¿El proceso es un *auth probe* del propio subsistema
/// (`hermes auth status …`)? Los probes usan el entorno sanitizado (llevan
/// HERMES_HOME) pero NO son escritores ACP: no deben bloquear admisiones.
/// `None` = cmdline ilegible ⇒ se trata como posible escritor (conservador).
fn is_auth_probe(dir: &Path) -> Option<bool> {
    let buf = std::fs::read(dir.join("cmdline")).ok()?;
    let args: Vec<&[u8]> = buf.split(|b| *b == 0).filter(|a| !a.is_empty()).collect();
    let has = |needle: &[u8]| args.contains(&needle);
    Some(has(b"auth") && has(b"status"))
}

fn cmdline_mentions(dir: &Path, home: &Path) -> bool {
    match std::fs::read(dir.join("cmdline")) {
        Ok(buf) => {
            let home_s = home.to_string_lossy().to_string();
            buf.split(|b| *b == 0).any(|c| c == home_s.as_bytes())
        }
        Err(_) => false,
    }
}

/// ¿Algún fd abierto del proceso apunta dentro del perfil? (señal
/// alternativa para procesos con environ restringido; best-effort).
fn fds_point_into(dir: &Path, home: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir.join("fd")) else {
        return false;
    };
    for e in entries.flatten() {
        if let Ok(target) = std::fs::read_link(e.path()) {
            let t = target.to_string_lossy();
            let h = home.to_string_lossy();
            if t == h || t.starts_with(&format!("{h}/")) {
                return true;
            }
        }
    }
    false
}

fn processes_using_home_in(home: &Path, proc_root: &Path) -> Result<Vec<i64>> {
    let needle = format!("HERMES_HOME={}", home.display());
    let euid = unsafe { libc::geteuid() };
    let entries = std::fs::read_dir(proc_root).map_err(|e| {
        Error::conflict(format!(
            "no se pudo inspeccionar {} ({e}) — fail-closed",
            proc_root.display()
        ))
    })?;
    let mut pids = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<i64>().ok()) else {
            continue;
        };
        let dir = e.path();
        let meta = match std::fs::symlink_metadata(&dir) {
            Ok(m) => m,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(Error::conflict(format!(
                    "no se pudo inspeccionar {} ({err}) — fail-closed",
                    dir.display()
                )));
            }
        };
        if meta.uid() != euid {
            // Otro usuario no puede ser dueño de este perfil (dirs 0700).
            continue;
        }
        match std::fs::read(dir.join("environ")) {
            Ok(buf) => {
                if buf.split(|b| *b == 0).any(|c| c == needle.as_bytes())
                    && is_auth_probe(&dir) != Some(true)
                {
                    pids.push(pid);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err)
                if err.kind() == std::io::ErrorKind::PermissionDenied
                    || err.raw_os_error() == Some(libc::EPERM) =>
            {
                // dumpable=0 / restringido: no concluyente por environ;
                // señales alternativas legibles antes de omitir.
                if (cmdline_mentions(&dir, home) || fds_point_into(&dir, home))
                    && is_auth_probe(&dir) != Some(true)
                {
                    pids.push(pid);
                }
            }
            Err(err) => {
                return Err(Error::conflict(format!(
                    "inspección de procesos falló (pid {pid}: {err}) — fail-closed"
                )));
            }
        }
    }
    Ok(pids)
}

// ── escrituras atómicas (H2) ─────────────────────────────────────────────────

/// Escritura atómica: temporal ÚNICO (O_EXCL, O_NOFOLLOW), fsync del archivo,
/// rename y fsync del directorio. Todos los errores se propagan.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| Error::integrity(format!("sin directorio padre: {}", path.display())))?;
    require_real_dir(dir, "directorio destino")?;
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && meta.file_type().is_symlink()
    {
        return Err(Error::conflict(format!(
            "{} es un symlink — fail-closed",
            path.display()
        )));
    }
    let base = path.file_name().and_then(|s| s.to_str()).unwrap_or("meta");
    let mut last_err: Option<std::io::Error> = None;
    for _ in 0..8 {
        let tmp = dir.join(format!(".{}.tmp-{}", base, crate::ids::new_id()));
        let opened = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp);
        match opened {
            Ok(mut f) => {
                let res = (|| -> Result<()> {
                    f.write_all(bytes)?;
                    f.sync_all()?;
                    drop(f);
                    std::fs::rename(&tmp, path)?;
                    let d = File::open(dir)?;
                    d.sync_all()?;
                    Ok(())
                })();
                if let Err(e) = res {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last_err = Some(e);
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(Error::integrity(format!(
        "no se pudo crear un temporal único para {}: {last_err:?}",
        path.display()
    )))
}

// ── metadata + meta lock (H2) ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProfileMeta {
    pub agent_id: String,
    pub created_ms: i64,
    pub hermes_version: String,
    pub provider: String,
    pub model: Option<String>,
    #[serde(default)]
    pub last_auth_check_ms: Option<i64>,
    #[serde(default)]
    pub auth_status: String,
    #[serde(default)]
    pub warmed_ms: Option<i64>,
}

fn read_meta(data_dir: &Path, agent_id: &str) -> Result<Option<ProfileMeta>> {
    let _ = assert_safe_agent_path(data_dir, agent_id)?;
    match read_file_nofollow(&meta_path(data_dir, agent_id)) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).map_err(|e| {
            Error::integrity(format!("profile.json ilegible ({agent_id}): {e}"))
        })?)),
        Err(Error::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Serializa los read-modify-write de `profile.json` entre procesos.
/// Bloqueante: flock se libera al morir el proceso (sin locks stale).
fn lock_meta(data_dir: &Path, agent_id: &str) -> Result<File> {
    let path = meta_lock_path(data_dir, agent_id);
    let Some(parent) = path.parent() else {
        return Err(Error::not_found(format!("perfil `{agent_id}` no existe")));
    };
    if !parent.exists() {
        return Err(Error::not_found(format!("perfil `{agent_id}` no existe")));
    }
    require_real_dir(parent, "directorio del perfil")?;
    let f = open_nofollow(&path, true, true).map_err(|e| {
        Error::conflict(format!(
            "no se pudo abrir el meta-lock del perfil `{agent_id}` ({e}) — fail-closed"
        ))
    })?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        return Err(Error::conflict(format!(
            "no se pudo tomar el meta-lock del perfil `{agent_id}`: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(f)
}

/// Escribe metadata asumiendo que el caller retiene el meta-lock.
fn write_meta_locked(data_dir: &Path, agent_id: &str, meta: &ProfileMeta) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(meta)?;
    write_atomic(&meta_path(data_dir, agent_id), &bytes)
}

/// RMW serializado de metadata: lee, muta y escribe bajo `.meta.lock`.
fn update_meta<F: FnOnce(&mut ProfileMeta)>(
    data_dir: &Path,
    agent_id: &str,
    f: F,
) -> Result<ProfileMeta> {
    let _ml = lock_meta(data_dir, agent_id)?;
    let mut meta = read_meta(data_dir, agent_id)?
        .ok_or_else(|| Error::not_found(format!("perfil `{agent_id}` sin metadata")))?;
    f(&mut meta);
    write_meta_locked(data_dir, agent_id, &meta)?;
    Ok(meta)
}

// ── registro ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RegistryEntry {
    pub thread_ids: Vec<String>,
    pub created_ms: i64,
    /// Tombstone durable de un borrado iniciado. Un perfil con este bit no se
    /// adopta/reasigna; solo puede completarse con `profile remove`.
    #[serde(default)]
    pub removing: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registry {
    pub version: u32,
    pub agents: BTreeMap<String, RegistryEntry>,
    pub updated_ms: i64,
}

impl Default for Registry {
    fn default() -> Self {
        Registry {
            version: 1,
            agents: BTreeMap::new(),
            updated_ms: 0,
        }
    }
}

#[derive(Debug)]
enum RegistryState {
    Present(Registry),
    MissingEmpty,
    MissingUnknown,
}

/// Distingue un store nuevo/vacío de una pérdida de evidencia.
fn load_registry_state(data_dir: &Path) -> Result<RegistryState> {
    let root_real = match safe_root(data_dir, false) {
        Ok(r) => r,
        Err(e) => {
            // A completely new data dir has no `hermes/` root. `safe_root`
            // canonicalizes the data dir; a missing root is only a problem
            // when the data dir itself is unavailable.
            return Err(e);
        }
    };
    let root_exists = std::fs::symlink_metadata(&root_real).is_ok();
    if !root_exists {
        return Ok(RegistryState::MissingEmpty);
    }
    let path = root_real.join("agents.json");
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(Error::conflict(format!(
            "{} es un symlink — fail-closed",
            path.display()
        ))),
        Ok(_) => {
            let bytes = read_file_nofollow(&path)?;
            let reg = serde_json::from_slice(&bytes)
                .map_err(|e| Error::integrity(format!("agents.json ilegible: {e}")))?;
            Ok(RegistryState::Present(reg))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut has_state = false;
            for entry in std::fs::read_dir(&root_real)? {
                let entry = entry?;
                if entry.file_name() == ".registry.lock" {
                    continue;
                }
                has_state = true;
                break;
            }
            if has_state {
                Ok(RegistryState::MissingUnknown)
            } else {
                Ok(RegistryState::MissingEmpty)
            }
        }
        Err(e) => Err(e.into()),
    }
}

/// Lectura para diagnóstico/listado: nunca inventa entradas, pero permite
/// enumerar directorios huérfanos cuando falta el registro.
fn load_registry(data_dir: &Path) -> Result<Registry> {
    match load_registry_state(data_dir)? {
        RegistryState::Present(reg) => Ok(reg),
        RegistryState::MissingEmpty | RegistryState::MissingUnknown => Ok(Registry::default()),
    }
}

/// Lectura para decisiones de ownership: un registro ausente en un store con
/// estado previo es evidencia incompleta, no un registro vacío.
fn load_registry_strict(data_dir: &Path) -> Result<Registry> {
    match load_registry_state(data_dir)? {
        RegistryState::Present(reg) => Ok(reg),
        RegistryState::MissingEmpty => Ok(Registry::default()),
        RegistryState::MissingUnknown => Err(Error::conflict(
            "el registro agents.json falta pero el store de perfiles contiene estado: \
             ownership no demostrable — fail-closed; restaurá el registro desde evidencia \
             durable (runs.profile/backup) antes de borrar o reasignar",
        )),
    }
}

fn save_registry(data_dir: &Path, reg: &Registry) -> Result<()> {
    let root_real = safe_root(data_dir, true)?;
    require_real_dir(&root_real, "root de perfiles")?;
    let mut reg = reg.clone();
    reg.updated_ms = now_ms();
    let bytes = serde_json::to_vec_pretty(&reg)?;
    write_atomic(&root_real.join("agents.json"), &bytes)
}

/// Toma el flock del registro (bloqueante; se libera al morir el proceso).
pub(crate) fn lock_registry(data_dir: &Path) -> Result<File> {
    let root_real = safe_root(data_dir, true)?;
    let path = root_real.join(".registry.lock");
    let f = open_nofollow(&path, true, true)?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        return Err(Error::conflict(format!(
            "no se pudo tomar el lock del registro: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(f)
}

// ── lock de escritor único (P1-4 / H1) ───────────────────────────────────────

/// Estado del lock de escritor. H1: "ausente" es definitivo; un error de
/// apertura/lectura es INCERTIDUMBRE y nunca se interpreta como libre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockState {
    Free,
    Held,
    Unknown(String),
}

/// Guard del flock exclusivo del perfil. Se libera al hacer drop (o al morir el
/// proceso). El fd puede hacerse heredable para que el backend lo retenga y el
/// lock sobreviva a la muerte del supervisor (P1-4).
pub struct ProfileLock {
    file: File,
}

impl ProfileLock {
    /// Adquiere el lock. NO crea directorios: si el perfil no existe, falla
    /// (evita recrear un perfil borrado durante una carrera de admisión).
    pub fn acquire(data_dir: &Path, agent_id: &str) -> Result<ProfileLock> {
        validate_agent_id(agent_id)?;
        let root_real = safe_root(data_dir, false)?;
        let dir = root_real.join(agent_id);
        require_real_dir(&dir, "directorio del perfil")?;
        let path = dir.join("aeril.lock");
        let f = match open_nofollow(&path, true, true) {
            Ok(f) => f,
            Err(Error::NotFound(_)) => {
                return Err(Error::not_found(format!(
                    "perfil `{agent_id}` no existe (dir ausente) — fail-closed"
                )));
            }
            Err(e) => {
                return Err(Error::conflict(format!(
                    "no se pudo abrir el lock del perfil `{agent_id}` ({e}) — fail-closed"
                )));
            }
        };
        let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            return Err(Error::conflict(format!(
                "el perfil `{agent_id}` ya tiene un escritor ACP activo (aeril.lock ocupado)"
            )));
        }
        Ok(ProfileLock { file: f })
    }

    /// Hace el fd heredable por el backend (limpia FD_CLOEXEC). Sin esto, el
    /// flock muere con el supervisor y un child sobreviviente quedaría sin
    /// protección de exclusión (P1-4).
    pub fn make_inheritable(&self) -> Result<()> {
        let fd = self.file.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 {
            return Err(Error::integrity(format!(
                "no se pudo leer FD_CLOEXEC del lock: {}",
                std::io::Error::last_os_error()
            )));
        }
        if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
            return Err(Error::integrity(format!(
                "no se pudo hacer heredable el lock: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }

    /// Estado del lock sin efectos colaterales: si el archivo no existe es
    /// definitivamente libre; errores de apertura/flock ⇒ Unknown.
    pub fn lock_state(data_dir: &Path, agent_id: &str) -> LockState {
        let root_real = match safe_root(data_dir, false) {
            Ok(r) => r,
            Err(e) => return LockState::Unknown(e.to_string()),
        };
        let path = root_real.join(agent_id).join("aeril.lock");
        let f = match OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return LockState::Free,
            Err(e) => return LockState::Unknown(e.to_string()),
        };
        let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            LockState::Free
        } else {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                LockState::Held
            } else {
                LockState::Unknown(err.to_string())
            }
        }
    }

    /// Compat: `true` solo si el lock está definitivamente tomado.
    pub fn is_held(data_dir: &Path, agent_id: &str) -> bool {
        matches!(Self::lock_state(data_dir, agent_id), LockState::Held)
    }
}

// ── evidencia de dueño + verificación de sobrevivientes (P1-4) ───────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OwnerRecord {
    supervisor_pid: i64,
    supervisor_start: u64,
    backend_pid: i64,
    backend_start: u64,
    pgid: i64,
    started_ms: i64,
}

fn write_owner(data_dir: &Path, agent_id: &str, rec: &OwnerRecord) -> Result<()> {
    #[cfg(debug_assertions)]
    if std::env::var("AERIL_TEST_FAIL_OWNER_WRITE").ok().as_deref() == Some("1") {
        return Err(Error::integrity(
            "fallo de escritura de owner.json inyectado por test",
        ));
    }
    let bytes = serde_json::to_vec_pretty(rec)?;
    write_atomic(&owner_path(data_dir, agent_id), &bytes)
}

fn read_owner(data_dir: &Path, agent_id: &str) -> Result<Option<OwnerRecord>> {
    let _ = assert_safe_agent_path(data_dir, agent_id)?;
    match read_file_nofollow(&owner_path(data_dir, agent_id)) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).map_err(|e| {
            Error::integrity(format!("owner.json ilegible ({agent_id}): {e}"))
        })?)),
        Err(Error::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

fn clear_owner(data_dir: &Path, agent_id: &str) -> Result<()> {
    let path = owner_path(data_dir, agent_id);
    if let Ok(meta) = std::fs::symlink_metadata(&path)
        && meta.file_type().is_symlink()
    {
        return Err(Error::conflict(format!(
            "{} es un symlink — fail-closed",
            path.display()
        )));
    }
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Verifica que no haya un escritor sobreviviente asociado al perfil. Debe
/// llamarse con el flock del perfil ya tomado.
///
/// - `owner.json`: si un pid registrado sigue VIVO con starttime coincidente
///   (o su identidad es desconocida) ⇒ HOLD fail-closed. Si está
///   ausente/reutilizado/zombie, el registro es stale y se limpia.
/// - Escaneo `/proc` por `HERMES_HOME` del perfil (mismo euid), fail-closed
///   ante incertidumbre.
pub fn verify_no_survivor(data_dir: &Path, agent_id: &str) -> Result<()> {
    let real = assert_safe_agent_path(data_dir, agent_id)?;
    let home = real.join("home");
    let owner = read_owner(data_dir, agent_id)?;
    if let Some(owner) = &owner {
        for (pid, start, which) in [
            (owner.supervisor_pid, owner.supervisor_start, "supervisor"),
            (owner.backend_pid, owner.backend_start, "backend"),
        ] {
            // The caller of warm/recovery may be the same supervisor that
            // wrote owner.json. It is not a surviving writer; the caller
            // already holds the exclusive lock.
            if which == "supervisor" && pid == std::process::id() as i64 {
                continue;
            }
            match pid_identity(pid, start) {
                PidId::Absent | PidId::Reused => {}
                PidId::Ours { zombie: true } => {}
                PidId::Ours { zombie: false } => {
                    return Err(Error::conflict(format!(
                        "perfil `{agent_id}`: {which} previo VIVO (pid {pid}) — HOLD fail-closed; \
                         terminá el proceso y reintentá (`aeril profile recover --agent {agent_id} --confirm`)"
                    )));
                }
                PidId::Unknown => {
                    return Err(Error::conflict(format!(
                        "perfil `{agent_id}`: identidad del pid {pid} ({which}) no verificable — HOLD fail-closed"
                    )));
                }
            }
        }
    }
    let pids = processes_using_home(&home)?;
    if !pids.is_empty() {
        return Err(Error::conflict(format!(
            "perfil `{agent_id}`: procesos vivos con HERMES_HOME del perfil (pids {pids:?}) — HOLD fail-closed"
        )));
    }
    // Only after both owner evidence and the process scan are clean may stale
    // owner evidence be removed. If a scan finds a survivor, owner.json is
    // retained for the operator.
    if owner.is_some() {
        clear_owner(data_dir, agent_id)?;
    }
    Ok(())
}

/// Read-only owner-evidence check for early admission. Never clears evidence
/// and never acquires the profile lock; the definitive admission re-checks
/// under the lock.
pub fn check_owner_evidence(data_dir: &Path, agent_id: &str) -> Result<()> {
    let Some(owner) = read_owner(data_dir, agent_id)? else {
        return Ok(());
    };
    for (pid, start, which) in [
        (owner.supervisor_pid, owner.supervisor_start, "supervisor"),
        (owner.backend_pid, owner.backend_start, "backend"),
    ] {
        if which == "supervisor" && pid == std::process::id() as i64 {
            continue;
        }
        match pid_identity(pid, start) {
            PidId::Absent | PidId::Reused | PidId::Ours { zombie: true } => {}
            PidId::Ours { zombie: false } => {
                return Err(Error::conflict(format!(
                    "perfil `{agent_id}`: {which} registrado sigue VIVO (pid {pid}) — HOLD fail-closed"
                )));
            }
            PidId::Unknown => {
                return Err(Error::conflict(format!(
                    "perfil `{agent_id}`: identidad del {which} registrado (pid {pid}) no verificable — HOLD fail-closed"
                )));
            }
        }
    }
    Ok(())
}

/// Admisión temprana (P1-4): rechaza si hay un escritor sobreviviente o si
/// el estado del lock es incertidumbre. NO adquiere el lock (lo hará la
/// admisión definitiva); se usa antes de operaciones que podrían terminar al
/// backend anterior (takeover de lease) para que la decisión sea explícita:
/// jamás se admite un run "matando primero" a un escritor vivo.
pub fn preflight_no_live_writer(data_dir: &Path, agent_id: &str) -> Result<()> {
    let real = assert_safe_agent_path(data_dir, agent_id)?;
    let home = real.join("home");
    match ProfileLock::lock_state(data_dir, agent_id) {
        LockState::Held => {
            return Err(Error::conflict(format!(
                "perfil `{agent_id}`: escritor ACP activo (lock tomado) — run rechazado;                  esperá a que termine o terminá el proceso huérfano (identidad verificada) y reconciliá con `aeril recover`"
            )));
        }
        LockState::Unknown(e) => {
            return Err(Error::conflict(format!(
                "perfil `{agent_id}`: estado del lock no verificable ({e}) — fail-closed"
            )));
        }
        LockState::Free => {}
    }
    check_owner_evidence(data_dir, agent_id)?;
    let pids = processes_using_home(&home)?;
    if !pids.is_empty() {
        return Err(Error::conflict(format!(
            "perfil `{agent_id}`: procesos vivos con HERMES_HOME del perfil (pids {pids:?}) —              run rechazado; terminá el escritor huérfano y reconciliá con `aeril recover`"
        )));
    }
    Ok(())
}

/// Reconciliación acotada (P1-4): con el lock tomado, valida que no haya
/// sobrevivientes y limpia la evidencia stale. Si hay un escritor vivo o
/// identidad no verificable, rechaza (jamás fuerza).
pub fn recover(data_dir: &Path, agent_id: &str) -> Result<String> {
    let _lock = ProfileLock::acquire(data_dir, agent_id)?;
    verify_no_survivor(data_dir, agent_id)?;
    Ok("reconciled".into())
}

// ── binario hermes + entorno sanitizado ─────────────────────────────────────

pub fn hermes_bin() -> Result<PathBuf> {
    if let Ok(p) = std::env::var(HERMES_BIN_ENV)
        && !p.is_empty()
    {
        let raw = PathBuf::from(p);
        let absolute = if raw.is_absolute() {
            raw
        } else {
            std::env::current_dir()
                .map_err(|e| Error::not_found(format!("cwd no disponible: {e}")))?
                .join(raw)
        };
        let real = std::fs::canonicalize(&absolute).map_err(|e| {
            Error::not_found(format!(
                "AERIL_HERMES_BIN {} no resuelve a un archivo ({e})",
                absolute.display()
            ))
        })?;
        if !real.is_file() {
            return Err(Error::not_found(format!(
                "AERIL_HERMES_BIN {} no es un archivo regular",
                real.display()
            )));
        }
        return Ok(real);
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let cand = dir.join("hermes");
        if cand.is_file() {
            if let Ok(real) = std::fs::canonicalize(&cand) {
                return Ok(real);
            }
            return Ok(cand);
        }
    }
    Err(Error::not_found(
        "binario `hermes` no encontrado en PATH (o definí AERIL_HERMES_BIN)",
    ))
}

/// Entorno sanitizado del perfil (validado en P2-S0): HOME/XDG/GH_CONFIG_DIR
/// propios, HERMES_HOME explícito, PATH mínimo + dir del binario hermes.
/// Jamás el entorno heredado.
pub fn env_for_profile(data_dir: &Path, agent_id: &str) -> Result<Vec<(String, String)>> {
    let dir = assert_safe_agent_path(data_dir, agent_id)?;
    require_real_dir(&dir, "directorio del perfil")?;
    let san = dir.join("san-home");
    let home = dir.join("home");
    let mut path = String::from("/usr/bin:/bin");
    if let Ok(bin) = hermes_bin()
        && let Some(dir) = bin.parent()
    {
        path = format!("{}:{path}", dir.display());
    }
    Ok(vec![
        ("HERMES_HOME".into(), home.to_string_lossy().into_owned()),
        ("HOME".into(), san.to_string_lossy().into_owned()),
        (
            "XDG_CONFIG_HOME".into(),
            san.join(".config").to_string_lossy().into_owned(),
        ),
        (
            "XDG_CACHE_HOME".into(),
            san.join(".cache").to_string_lossy().into_owned(),
        ),
        (
            "XDG_DATA_HOME".into(),
            san.join(".local/share").to_string_lossy().into_owned(),
        ),
        (
            "XDG_STATE_HOME".into(),
            san.join(".local/state").to_string_lossy().into_owned(),
        ),
        (
            "GH_CONFIG_DIR".into(),
            san.join(".config/gh").to_string_lossy().into_owned(),
        ),
        ("PATH".into(), path),
        ("TERM".into(), "dumb".into()),
        ("LANG".into(), "C.UTF-8".into()),
    ])
}

fn env_secs(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v >= 1)
        .unwrap_or(default)
}

// ── auth status (P1-2) ───────────────────────────────────────────────────────

/// Clasifica la salida de `hermes auth status <provider>` (contrato del Hermes
/// instalado, v0.21.x: `"<provider>: logged in"` / `"<provider>: logged out"` /
/// `"<provider>: Nous · free tier"`). Función pura, testeable.
///
/// Estricto: exige UNA única línea de estado del provider esperado (sin
/// contradicciones), estado positivo EXACTO `"logged in"` para `logged_in`.
/// Cualquier otra cosa ⇒ `unknown` (fail-closed).
fn classify_auth_output(stdout: &str, provider: &str) -> String {
    let expected = provider.to_lowercase();
    let mut statuses: Vec<&str> = Vec::new();
    for line in stdout.lines() {
        // Las líneas de continuación del CLI van indentadas; no son estado.
        if line.chars().next().is_some_and(char::is_whitespace) {
            continue;
        }
        let Some((tok, rest)) = line.split_once(':') else {
            continue;
        };
        if tok.trim().to_lowercase() != expected {
            continue;
        }
        statuses.push(rest.trim());
    }
    if statuses.len() != 1 {
        // 0 = provider no mencionado (ambiguo); >1 = contradictorio.
        return "unknown".into();
    }
    let rest = statuses[0];
    if rest == "logged in" {
        return "logged_in".into();
    }
    let lower = rest.to_lowercase();
    if lower.starts_with("logged out") {
        return "absent".into();
    }
    for neg in [
        "not logged in",
        "not signed in",
        "not currently logged in",
        "no credentials",
    ] {
        if lower == neg
            || lower.starts_with(&format!("{neg} ("))
            || lower.starts_with(&format!("{neg}:"))
        {
            return "absent".into();
        }
    }
    // Estado guest reconocido del Hermes soportado: NO es un login de cuenta.
    if lower.contains("free tier") {
        return "absent".into();
    }
    "unknown".into()
}

/// Estado de auth por perfil, **sin leer credenciales**: ejecuta
/// `hermes auth status <provider>` con el entorno sanitizado. Exige exit 0 y
/// salida inequívoca. Timeout/exit≠0/salida ambigua ⇒ `unknown` FRESCO
/// persistido (nunca se deja un `logged_in` stale como si fuera actual).
pub fn auth_status(data_dir: &Path, agent_id: &str) -> Result<String> {
    let _ = assert_safe_agent_path(data_dir, agent_id)?;
    let meta = read_meta(data_dir, agent_id)?
        .ok_or_else(|| Error::not_found(format!("perfil `{agent_id}` sin metadata")))?;
    let provider = if meta.provider.is_empty() {
        "nous".to_string()
    } else {
        meta.provider.clone()
    };
    let verdict = probe_auth(data_dir, agent_id, &provider);
    update_meta(data_dir, agent_id, |m| {
        m.auth_status = verdict.clone();
        m.last_auth_check_ms = Some(now_ms());
    })?;
    Ok(verdict)
}

fn probe_auth(data_dir: &Path, agent_id: &str, provider: &str) -> String {
    let bin = match hermes_bin() {
        Ok(b) => b,
        Err(_) => return "unknown".into(),
    };
    let env = match env_for_profile(data_dir, agent_id) {
        Ok(e) => e,
        Err(_) => return "unknown".into(),
    };
    let timeout = Duration::from_secs(env_secs(AUTH_TIMEOUT_ENV, 20));
    let mut child = match std::process::Command::new(&bin)
        .args(["auth", "status", provider])
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return "unknown".into(),
    };
    let capture = capture_auth_output(&mut child, timeout);
    if capture.truncated || capture.timed_out {
        kill_auth_group(&mut child);
        return "unknown".into();
    }
    if !capture.status.as_ref().is_some_and(|st| st.success()) {
        kill_auth_group(&mut child);
        return "unknown".into();
    }
    let stdout = String::from_utf8_lossy(&capture.stdout);
    let stderr = String::from_utf8_lossy(&capture.stderr);
    if stderr_contradicts(&stderr, provider) {
        return "unknown".into();
    }
    classify_auth_output(&stdout, provider)
}

struct AuthCapture {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
    timed_out: bool,
    status: Option<std::process::ExitStatus>,
}

fn stderr_contradicts(stderr: &str, provider: &str) -> bool {
    let expected = provider.to_lowercase();
    for line in stderr.lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_lowercase();
        if lower.starts_with(&format!("{expected}:"))
            || lower.contains("logged in")
            || lower.contains("logged out")
            || lower.contains("not signed in")
            || lower.contains("no credentials")
        {
            return true;
        }
    }
    false
}

fn kill_auth_group(child: &mut std::process::Child) {
    if let Ok(pid) = i32::try_from(child.id()) {
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.wait();
}

fn capture_auth_output(child: &mut std::process::Child, timeout: Duration) -> AuthCapture {
    let deadline = Instant::now() + timeout;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let out_fd = stdout.as_ref().map(|s| s.as_raw_fd()).unwrap_or(-1);
    let err_fd = stderr.as_ref().map(|s| s.as_raw_fd()).unwrap_or(-1);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut out_closed = out_fd < 0;
    let mut err_closed = err_fd < 0;
    let mut truncated = false;
    let mut timed_out = false;
    let mut status = None;

    while !(out_closed && err_closed) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            timed_out = true;
            break;
        }
        let mut fds = [
            libc::pollfd {
                fd: if out_closed { -1 } else { out_fd },
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            },
            libc::pollfd {
                fd: if err_closed { -1 } else { err_fd },
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            },
        ];
        let ms = remaining.as_millis().clamp(1, 50) as i32;
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, ms) };
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EINTR) {
                timed_out = true;
                break;
            }
            continue;
        }
        if rc == 0 {
            continue;
        }
        for (idx, slot) in fds.iter().enumerate() {
            if slot.revents == 0 {
                continue;
            }
            let fd = slot.fd;
            if fd < 0 {
                continue;
            }
            let mut chunk = [0u8; 8192];
            let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EINTR) || e.raw_os_error() == Some(libc::EAGAIN) {
                    continue;
                }
                timed_out = true;
                break;
            }
            if n == 0 {
                if idx == 0 {
                    out_closed = true;
                } else {
                    err_closed = true;
                }
                continue;
            }
            let data = &chunk[..n as usize];
            let target = if idx == 0 { &mut out } else { &mut err };
            if target.len() + data.len() > AUTH_OUTPUT_MAX_BYTES {
                truncated = true;
                break;
            }
            target.extend_from_slice(data);
        }
        if truncated {
            break;
        }
    }

    if !truncated && !timed_out {
        loop {
            match child.try_wait() {
                Ok(Some(st)) => {
                    status = Some(st);
                    break;
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => {
                    timed_out = true;
                    break;
                }
            }
        }
    }
    drop(stdout.take());
    drop(stderr.take());
    AuthCapture {
        stdout: out,
        stderr: err,
        truncated,
        timed_out,
        status,
    }
}

// ── ciclo de vida ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ProfileView {
    pub agent_id: String,
    pub registered: bool,
    pub dir_state: String, // ok | incomplete | missing | orphan
    pub auth_status: String,
    pub auth_checked_ms: Option<i64>,
    pub thread_ids: Vec<String>,
    pub lock_held: bool,
    pub lock_state: String, // free | held | unknown
    pub warmed_ms: Option<i64>,
    pub disk_kb: u64,
    pub removing: bool,
}

fn dir_state(data_dir: &Path, agent_id: &str, registered: bool) -> String {
    let dir = agent_dir(data_dir, agent_id);
    let meta = meta_path(data_dir, agent_id);
    match (dir.exists(), meta.exists(), registered) {
        (false, _, _) => "missing".into(),
        (true, true, true) => "ok".into(),
        (true, false, true) => "incomplete".into(),
        (true, _, false) => "orphan".into(),
    }
}

fn disk_kb(path: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for e in entries.flatten() {
        let Ok(m) = std::fs::symlink_metadata(e.path()) else {
            continue;
        };
        if m.file_type().is_symlink() {
            continue;
        }
        if m.is_dir() {
            total += disk_kb(&e.path());
        } else {
            total += m.len() / 1024;
        }
    }
    total
}

fn ensure_dirs(data_dir: &Path, agent_id: &str) -> Result<()> {
    let root_real = safe_root(data_dir, true)?;
    let dirs = [
        (root_real.join(agent_id), "directorio del perfil"),
        (root_real.join(agent_id).join("home"), "home del perfil"),
        (
            root_real.join(agent_id).join("san-home"),
            "san-home del perfil",
        ),
    ];
    for (p, label) in dirs {
        ensure_real_dir(&p, label)?;
        require_real_dir(&p, label)?;
        std::fs::set_permissions(&p, PermissionsExt::from_mode(0o700))?;
    }
    Ok(())
}

/// Crea (idempotente) un perfil. Repara un create interrumpido SOLO si el
/// directorio está registrado, vacío o tiene metadata propia; nunca adopta
/// directorios ajenos.
pub fn create(
    data_dir: &Path,
    agent_id: &str,
    provider: &str,
    model: Option<&str>,
    warm: bool,
) -> Result<ProfileView> {
    validate_agent_id(agent_id)?;
    let _reg_lock = lock_registry(data_dir)?;
    let mut reg = load_registry_strict(data_dir)?;
    let registered = reg.agents.contains_key(agent_id);
    if reg.agents.get(agent_id).is_some_and(|e| e.removing) {
        return Err(Error::conflict(format!(
            "el perfil `{agent_id}` tiene un borrado incompleto registrado; \
             completá `profile remove --agent {agent_id} --confirm` antes de recrearlo"
        )));
    }
    let dir = agent_dir(data_dir, agent_id);
    if dir.exists() {
        // Fail-closed: nunca operar a través de symlinks ni fuera del root.
        let _ = assert_safe_agent_path(data_dir, agent_id)?;
        if !registered {
            let empty = std::fs::read_dir(&dir)
                .map(|mut it| it.next().is_none())
                .unwrap_or(false);
            if !empty {
                return Err(Error::conflict(format!(
                    "directorio existente no reconocido: el perfil `{agent_id}` existe y no está registrado: \
                     ownership no demostrable — no se adopta; restaurá el registro o eliminá \
                     el directorio con una operación explícita autorizada"
                )));
            }
        }
        let meta = read_meta(data_dir, agent_id)?;
        match meta {
            Some(m) if m.agent_id == agent_id => {
                // Idempotente: completar piezas faltantes sin tocar nada más.
                ensure_dirs(data_dir, agent_id)?;
            }
            Some(m) => {
                return Err(Error::conflict(format!(
                    "el directorio del perfil pertenece a otro agent_id ({}) — fail-closed",
                    m.agent_id
                )));
            }
            None => {
                // Create interrumpido: reparar solo si está registrado o vacío.
                let empty = std::fs::read_dir(&dir)
                    .map(|mut it| it.next().is_none())
                    .unwrap_or(false);
                if !registered && !empty {
                    return Err(Error::conflict(format!(
                        "directorio existente no reconocido en {} — fail-closed (no se adopta)",
                        dir.display()
                    )));
                }
                ensure_dirs(data_dir, agent_id)?;
                let meta = ProfileMeta {
                    agent_id: agent_id.to_string(),
                    created_ms: now_ms(),
                    hermes_version: String::new(),
                    provider: provider.to_string(),
                    model: model.map(str::to_string),
                    last_auth_check_ms: None,
                    auth_status: "unknown".into(),
                    warmed_ms: None,
                };
                let _ml = lock_meta(data_dir, agent_id)?;
                write_meta_locked(data_dir, agent_id, &meta)?;
            }
        }
    } else {
        ensure_dirs(data_dir, agent_id)?;
        let meta = ProfileMeta {
            agent_id: agent_id.to_string(),
            created_ms: now_ms(),
            hermes_version: String::new(),
            provider: provider.to_string(),
            model: model.map(str::to_string),
            last_auth_check_ms: None,
            auth_status: "unknown".into(),
            warmed_ms: None,
        };
        let _ml = lock_meta(data_dir, agent_id)?;
        write_meta_locked(data_dir, agent_id, &meta)?;
    }
    reg.agents
        .entry(agent_id.to_string())
        .or_insert_with(|| RegistryEntry {
            thread_ids: Vec::new(),
            created_ms: now_ms(),
            removing: false,
        });
    save_registry(data_dir, &reg)?;
    drop(_reg_lock);
    if warm {
        warm_profile(data_dir, agent_id)?;
    }
    status_view(data_dir, agent_id, true)
}

pub fn list(data_dir: &Path) -> Result<Vec<ProfileView>> {
    let reg = load_registry(data_dir)?;
    let mut ids: Vec<String> = reg.agents.keys().cloned().collect();
    // Orphan dirs (sin registro): se reportan, jamás se adoptan.
    if let Ok(entries) = std::fs::read_dir(root(data_dir)) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name == "agents.json" {
                continue;
            }
            if e.path().is_dir()
                && validate_agent_id(&name).is_ok()
                && !reg.agents.contains_key(&name)
            {
                ids.push(name);
            }
        }
    }
    ids.sort();
    let mut out = Vec::new();
    for id in ids {
        let registered = reg.agents.contains_key(&id);
        out.push(status_view(data_dir, &id, registered)?);
    }
    Ok(out)
}

fn status_view(data_dir: &Path, agent_id: &str, registered: bool) -> Result<ProfileView> {
    let meta = read_meta(data_dir, agent_id)?;
    let registry = load_registry(data_dir)?;
    let auth_status = meta
        .as_ref()
        .map(|m| m.auth_status.clone())
        .unwrap_or_else(|| "unknown".into());
    let auth_checked_ms = meta.as_ref().and_then(|m| m.last_auth_check_ms);
    let thread_ids = registry
        .agents
        .get(agent_id)
        .map(|e| e.thread_ids.clone())
        .unwrap_or_default();
    let removing = registry
        .agents
        .get(agent_id)
        .map(|e| e.removing)
        .unwrap_or(false);
    let mut state = dir_state(data_dir, agent_id, registered);
    if removing {
        state = "removing".into();
    }
    let lock_state = match ProfileLock::lock_state(data_dir, agent_id) {
        LockState::Free => "free",
        LockState::Held => "held",
        LockState::Unknown(_) => "unknown",
    };
    Ok(ProfileView {
        agent_id: agent_id.to_string(),
        registered,
        dir_state: state,
        auth_status,
        auth_checked_ms,
        thread_ids,
        lock_held: lock_state == "held",
        lock_state: lock_state.to_string(),
        warmed_ms: meta.and_then(|m| m.warmed_ms),
        disk_kb: disk_kb(&agent_dir(data_dir, agent_id)),
        removing,
    })
}

/// Estado del perfil con verificación de auth fresca (no lee credenciales).
pub fn status(data_dir: &Path, agent_id: &str) -> Result<ProfileView> {
    let registered = load_registry(data_dir)?.agents.contains_key(agent_id);
    let _ = auth_status(data_dir, agent_id)?; // refresca metadata (metadata-only)
    status_view(data_dir, agent_id, registered)
}

// ── pre-warm estricto (P1-3) ─────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum Frame {
    Accept,
    Ignore,
    Reject(String),
}

/// Valida un frame de respuesta `initialize` (P1-3). Contrato del Hermes
/// instalado (ACP v1): result con `protocolVersion=1`, `agentCapabilities`
/// objeto, `authMethods` no vacío con ids string; `agentInfo` objeto si está.
/// Las notificaciones se ignoran SOLO si no traen result/error; cualquier
/// frame que no sea JSON válido, o respuesta con id/forma incorrecta, rechaza.
fn validate_initialize_frame(line: &[u8]) -> Frame {
    let v: serde_json::Value = match serde_json::from_slice(line) {
        Ok(v) => v,
        Err(_) => return Frame::Reject("frame no es JSON válido".into()),
    };
    let Some(obj) = v.as_object() else {
        return Frame::Reject("frame no es un objeto JSON-RPC".into());
    };
    if obj.get("jsonrpc").and_then(|j| j.as_str()) != Some("2.0") {
        return Frame::Reject("jsonrpc distinto de \"2.0\"".into());
    }
    let has_method = obj.contains_key("method");
    let has_result = obj.contains_key("result");
    let has_error = obj.contains_key("error");
    if has_method {
        // Notificación legítima: sin id ni campos de respuesta.
        if obj.contains_key("id") || has_result || has_error {
            return Frame::Reject(
                "notificación con id/result/error (no puede suplantar initialize)".into(),
            );
        }
        return Frame::Ignore;
    }
    match obj.get("id").and_then(|i| i.as_u64()) {
        Some(1) => {}
        Some(_) => {
            return Frame::Reject("id de respuesta no coincide con initialize (id=1)".into());
        }
        None => return Frame::Reject("respuesta sin id numérico".into()),
    }
    if let Some(err) = obj.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("sin message");
        return Frame::Reject(format!("error JSON-RPC en initialize: {msg}"));
    }
    let Some(result) = obj.get("result") else {
        return Frame::Reject("respuesta sin result".into());
    };
    match validate_initialize_result(result) {
        Ok(()) => Frame::Accept,
        Err(e) => Frame::Reject(e.to_string()),
    }
}

/// Shared ACP initialize-result contract for profile warm and real managed
/// runs. The Hermes/ACP v1 result must carry a supported protocol version,
/// an `agentCapabilities` object and a non-empty `authMethods` array.
pub(crate) fn validate_initialize_result(result: &serde_json::Value) -> Result<()> {
    let Some(ro) = result.as_object() else {
        return Err(Error::backend("result de initialize no es objeto"));
    };
    match ro.get("protocolVersion").and_then(|p| p.as_i64()) {
        Some(1) => {}
        Some(other) => {
            return Err(Error::backend(format!(
                "protocolVersion {other} no soportado (esperado 1)"
            )));
        }
        None => return Err(Error::backend("sin protocolVersion entero")),
    }
    match ro.get("agentCapabilities") {
        Some(c) if c.is_object() => {}
        Some(_) => return Err(Error::backend("agentCapabilities no es objeto")),
        None => return Err(Error::backend("sin agentCapabilities")),
    }
    if let Some(i) = ro.get("agentInfo")
        && !i.is_object()
    {
        return Err(Error::backend("agentInfo no es objeto"));
    }
    match ro.get("authMethods") {
        Some(m) => {
            let Some(arr) = m.as_array() else {
                return Err(Error::backend("authMethods no es array"));
            };
            if arr.is_empty() {
                return Err(Error::backend(
                    "authMethods vacío (el Hermes soportado siempre anuncia ≥1 método)",
                ));
            }
            for e in arr {
                if e.get("id").and_then(|i| i.as_str()).is_none() {
                    return Err(Error::backend("authMethod sin id string"));
                }
            }
        }
        None => return Err(Error::backend("sin authMethods")),
    }
    Ok(())
}

/// Termina el grupo de procesos `pgid` con escalada acotada y verificación
/// (SIGTERM → plazo → SIGKILL → verificación). No declara éxito si algún
/// miembro vivo sobrevive o si el grupo no puede inspeccionarse.
fn terminate_group(pgid: i64, grace: Duration) -> Result<()> {
    let pgid32 = pgid as i32;
    unsafe {
        libc::kill(-pgid32, libc::SIGTERM);
    }
    let deadline = Instant::now() + grace;
    loop {
        match crate::recovery::live_group_members(pgid) {
            Some(v) if v.is_empty() => return Ok(()),
            _ => {}
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    unsafe {
        libc::kill(-pgid32, libc::SIGKILL);
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match crate::recovery::live_group_members(pgid) {
            Some(v) if v.is_empty() => return Ok(()),
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(Error::backend(format!(
                "no se pudo verificar la terminación del grupo de procesos {pgid} — fail-closed"
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Pre-warm acotado: `hermes acp` + initialize ESTRICTO con deadline fijo,
/// cotas de frame/stream y terminación verificada del grupo antes de liberar
/// el lock de escritor. Retiene el lock (heredable) durante el handshake.
pub fn warm_profile(data_dir: &Path, agent_id: &str) -> Result<i64> {
    let _ = assert_safe_agent_path(data_dir, agent_id)?;
    let lock = ProfileLock::acquire(data_dir, agent_id)?;
    lock.make_inheritable()?;
    verify_no_survivor(data_dir, agent_id)?;
    let bin = hermes_bin()?;
    let env = env_for_profile(data_dir, agent_id)?;
    let timeout = Duration::from_secs(env_secs(WARM_TIMEOUT_ENV, 120));
    eprintln!("[profile] warm {agent_id}: spawn `hermes acp`…");
    let mut child = std::process::Command::new(&bin)
        .arg("acp")
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| Error::backend(format!("no se pudo lanzar `hermes acp`: {e}")))?;
    let pid = child.id() as i64;
    let started = Instant::now();
    // Evidencia de dueño (pid+starttime+pgid) para verificación posterior.
    let owner = OwnerRecord {
        supervisor_pid: std::process::id() as i64,
        supervisor_start: process_start_time(std::process::id() as i64).unwrap_or(0),
        backend_pid: pid,
        backend_start: process_start_time(pid).unwrap_or(0),
        pgid: pid,
        started_ms: now_ms(),
    };
    if let Err(e) = write_owner(data_dir, agent_id, &owner) {
        let _ = terminate_group(pid, Duration::from_secs(2));
        let _ = child.wait();
        let home = home_dir(data_dir, agent_id);
        let survivors = processes_using_home(&home).unwrap_or_default();
        return Err(Error::backend(format!(
            "no se pudo persistir owner.json ({e}); backend terminado/verificado, \
             sobrevivientes={survivors:?} — warm falla fail-closed"
        )));
    }
    let init = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": 1, "clientCapabilities": {}, "clientInfo": {"name": "aeril-profile-warm", "version": env!("CARGO_PKG_VERSION")}}
    });
    let send_res = (|| -> Result<()> {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| Error::backend("stdin no disponible"))?;
        writeln!(stdin, "{init}")?;
        stdin.flush()?;
        Ok(())
    })();
    let result = match send_res {
        Ok(()) => {
            eprintln!(
                "[profile] warm {agent_id}: initialize enviado; esperando respuesta estricta…"
            );
            read_initialize_response(&mut child, timeout)
        }
        Err(e) => Err(e),
    };
    let elapsed_ms = started.elapsed().as_millis() as i64;
    // Terminación verificada ANTES de liberar el lock (P1-3).
    let terminated = terminate_group(pid, Duration::from_secs(2));
    let _ = child.wait();
    match terminated {
        Ok(()) => {
            // A descendant may have escaped the original PGID (setsid). The
            // profile-wide process scan is the authority; only a clean scan
            // permits clearing owner.json.
            verify_no_survivor(data_dir, agent_id)?;
        }
        Err(e) => {
            // Evidencia retenida: un próximo admission verá al sobreviviente.
            return Err(e);
        }
    }
    result?;
    update_meta(data_dir, agent_id, |m| {
        m.warmed_ms = Some(elapsed_ms);
    })?;
    eprintln!("[profile] warm {agent_id}: listo en {elapsed_ms} ms");
    Ok(elapsed_ms)
}

fn read_initialize_response(child: &mut std::process::Child, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut buf: Vec<u8> = Vec::new();
    let mut total = 0usize;
    let mut byte = [0u8; 1];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Error::backend(
                "initialize excedió el plazo fijo (fail-closed)",
            ));
        }
        let stdout = child
            .stdout
            .as_mut()
            .ok_or_else(|| Error::backend("stdout no disponible"))?;
        let mut pfd = libc::pollfd {
            fd: stdout.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = remaining.as_millis().clamp(1, 200) as i32;
        let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
        if rc < 0 {
            return Err(Error::backend(format!(
                "poll sobre el backend falló: {}",
                std::io::Error::last_os_error()
            )));
        }
        if rc == 0 {
            continue;
        }
        match stdout.read(&mut byte) {
            Ok(0) => {
                return Err(Error::backend(
                    "EOF del backend antes de una respuesta initialize válida (fail-closed)",
                ));
            }
            Ok(_) => {
                total += 1;
                if total > STREAM_MAX_BYTES {
                    return Err(Error::backend(
                        "stream de initialize excedió la cota total — fail-closed",
                    ));
                }
                buf.push(byte[0]);
                if byte[0] == b'\n' {
                    if buf.len() > FRAME_MAX_BYTES {
                        return Err(Error::backend(
                            "frame de initialize excede 1 MiB — fail-closed",
                        ));
                    }
                    let line: Vec<u8> = buf
                        .iter()
                        .copied()
                        .filter(|b| *b != b'\n' && *b != b'\r')
                        .collect();
                    if line.is_empty() {
                        buf.clear();
                        continue;
                    }
                    match validate_initialize_frame(&line) {
                        Frame::Accept => return Ok(()),
                        Frame::Ignore => {
                            buf.clear();
                            continue;
                        }
                        Frame::Reject(reason) => {
                            return Err(Error::backend(format!(
                                "respuesta initialize inválida: {reason} — fail-closed"
                            )));
                        }
                    }
                } else if buf.len() > FRAME_MAX_BYTES {
                    return Err(Error::backend(
                        "frame de initialize excede 1 MiB sin terminador — fail-closed",
                    ));
                }
            }
            Err(e) => {
                return Err(Error::backend(format!(
                    "lectura del backend falló: {e} — fail-closed"
                )));
            }
        }
    }
}

// ── mapeo thread→perfil ──────────────────────────────────────────────────────

/// Mapeo thread→perfil (asignación explícita; única por thread).
pub fn agent_for_thread(data_dir: &Path, thread_id: &str) -> Result<Option<String>> {
    let reg = load_registry_strict(data_dir)?;
    Ok(reg
        .agents
        .iter()
        .find(|(_, e)| e.thread_ids.iter().any(|t| t == thread_id))
        .map(|(id, _)| id.clone()))
}

/// Runs no-terminales asociados a un perfil: por vínculo durable
/// (`runs.profile`, sobrevive a unassign/reassign) o por thread (legacy).
/// Cualquier error de DB ⇒ `Err` (el llamador decide fail-closed).
fn count_non_terminal_runs(
    conn: &rusqlite::Connection,
    profile: &str,
    threads: &[String],
) -> Result<i64> {
    // Un único COUNT(DISTINCT): las condiciones (perfil durable y threads
    // legacy) pueden solaparse sobre el mismo run; el número no debe inflarse.
    let mut sql = String::from(
        "SELECT COUNT(DISTINCT id) FROM runs WHERE state IN ('starting','running') AND (profile = ?1",
    );
    for i in 0..threads.len() {
        sql.push_str(&format!(" OR thread_id = ?{}", i + 2));
    }
    sql.push(')');
    let mut stmt = conn.prepare(&sql)?;
    let mut binds: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(1 + threads.len());
    binds.push(&profile);
    for t in threads {
        binds.push(t);
    }
    let n: i64 = stmt.query_row(rusqlite::params_from_iter(binds.iter()), |r| r.get(0))?;
    Ok(n)
}

/// A run with `runs.profile = NULL` predates durable profile ownership. If it
/// is non-terminal, no profile deletion can prove it is unrelated to that
/// run; refuse until it is reconciled.
fn count_legacy_non_terminal_runs(conn: &rusqlite::Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM runs
         WHERE state IN ('starting','running') AND profile IS NULL",
        [],
        |r| r.get(0),
    )?)
}

/// Abre el store para chequeos de seguridad: cualquier fallo es fail-closed.
fn open_store_for_guard(db_path: &Path) -> Result<crate::store::Store> {
    crate::store::Store::open(db_path).map_err(|e| {
        Error::conflict(format!(
            "no se pudo abrir la base para verificar runs activos ({e}) — fail-closed"
        ))
    })
}

/// Rechaza si el thread tiene runs no-terminales (HOLD + reconciliación).
fn deny_if_thread_active(conn: &rusqlite::Connection, thread_id: &str, action: &str) -> Result<()> {
    let active: i64 = conn.query_row(
        "SELECT COUNT(*) FROM runs WHERE thread_id = ?1 AND state IN ('starting','running')",
        [thread_id],
        |r| r.get(0),
    )?;
    if active > 0 {
        return Err(Error::conflict(format!(
            "el thread {thread_id} tiene {active} run(s) no-terminal(es): {action} rechazado — \
             HOLD fail-closed; reconciliá con `aeril recover` o esperá a que terminen"
        )));
    }
    Ok(())
}

pub fn assign(data_dir: &Path, agent_id: &str, thread_id: &str, db_path: &Path) -> Result<()> {
    validate_agent_id(agent_id)?;
    let _reg_lock = lock_registry(data_dir)?;
    let mut reg = load_registry_strict(data_dir)?;
    if !reg.agents.contains_key(agent_id) {
        return Err(Error::not_found(format!(
            "perfil `{agent_id}` no registrado (creálo con `aeril profile create`)"
        )));
    }
    if reg.agents.get(agent_id).is_some_and(|e| e.removing)
        || reg
            .agents
            .values()
            .any(|e| e.removing && e.thread_ids.iter().any(|t| t == thread_id))
    {
        return Err(Error::conflict(format!(
            "hay un borrado de perfil incompleto que afecta a `{agent_id}` o al thread {thread_id}; \
             completá el remove antes de reasignar"
        )));
    }
    if !meta_path(data_dir, agent_id).exists() {
        return Err(Error::conflict(format!(
            "perfil `{agent_id}` incompleto (sin profile.json) — repará con `profile create`"
        )));
    }
    // P1-1: no quitar el vínculo de un thread con runs no-terminales.
    let store = open_store_for_guard(db_path)?;
    deny_if_thread_active(&store.conn, thread_id, "reasignación")?;
    // Unicidad: quitar el thread de cualquier otro perfil (explícito, atómico).
    for (_, e) in reg.agents.iter_mut() {
        e.thread_ids.retain(|t| t != thread_id);
    }
    reg.agents
        .get_mut(agent_id)
        .expect("existe")
        .thread_ids
        .push(thread_id.to_string());
    save_registry(data_dir, &reg)
}

pub fn unassign(data_dir: &Path, thread_id: &str, db_path: &Path) -> Result<bool> {
    let _reg_lock = lock_registry(data_dir)?;
    let mut reg = load_registry_strict(data_dir)?;
    if reg
        .agents
        .values()
        .any(|e| e.removing && e.thread_ids.iter().any(|t| t == thread_id))
    {
        return Err(Error::conflict(format!(
            "el thread {thread_id} pertenece a un perfil con borrado incompleto; \
             completá el remove antes de desasignar"
        )));
    }
    let assigned = reg
        .agents
        .values()
        .any(|e| e.thread_ids.iter().any(|t| t == thread_id));
    if !assigned {
        return Ok(false);
    }
    // P1-1: no borrar el vínculo durable de un thread con runs no-terminales.
    let store = open_store_for_guard(db_path)?;
    deny_if_thread_active(&store.conn, thread_id, "desasignación")?;
    let mut removed = false;
    for (_, e) in reg.agents.iter_mut() {
        let before = e.thread_ids.len();
        e.thread_ids.retain(|t| t != thread_id);
        removed |= e.thread_ids.len() != before;
    }
    if removed {
        save_registry(data_dir, &reg)?;
    }
    Ok(removed)
}

// ── borrado (P1-1 / C4 / H1) ─────────────────────────────────────────────────

/// Borrado explícito y verificado. Fail-closed ante cualquier duda.
///
/// Orden de seguridad (R1):
/// 1. registry lock + validación de ownership/path (sin mutar nada);
/// 2. guard de runs no-terminales SIEMPRE (por perfil durable o thread) con
///    DB inaccesible/consulta fallida ⇒ HOLD (jamás `unwrap_or(0)`);
/// 3. guard de asociaciones históricas (override explícito `--clear-threads`);
/// 4. adquisición del flock de escritor (no solo chequeo) retenido durante el
///    borrado;
/// 5. procesos sobrevivientes + mountpoints/symlinks (fail-closed);
/// 6. borrado + actualización atómica del registro.
pub fn remove(
    data_dir: &Path,
    agent_id: &str,
    confirm: bool,
    clear_threads: bool,
    db_path: &Path,
) -> Result<()> {
    if !confirm {
        return Err(Error::invalid(
            "borrado de perfil requiere --confirm (acción explícita)",
        ));
    }
    validate_agent_id(agent_id)?;
    let raw_dir = agent_dir(data_dir, agent_id);
    let dir_exists = match std::fs::symlink_metadata(&raw_dir) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e.into()),
    };

    // Lock order: profile writer → registry → SQLite. The profile lock is
    // taken first so an in-flight admission cannot lose its profile, and so
    // the writer exclusion is already held when the registry is inspected.
    let _profile_lock = if dir_exists {
        let _ = assert_safe_agent_path(data_dir, agent_id)?;
        Some(ProfileLock::acquire(data_dir, agent_id)?)
    } else {
        None
    };
    let _reg_lock = lock_registry(data_dir)?;
    let mut reg = load_registry_strict(data_dir)?;
    let entry = reg.agents.get(agent_id).cloned();
    let threads = entry
        .as_ref()
        .map(|e| e.thread_ids.clone())
        .unwrap_or_default();
    let removing = entry.as_ref().is_some_and(|e| e.removing);

    // (1) Ownership y seguridad de path ANTES de cualquier mutación.
    if dir_exists {
        if entry.is_none() {
            return Err(Error::conflict(format!(
                "el directorio del perfil `{agent_id}` existe pero no está registrado: \
                 ownership no demostrable — fail-closed (no se adopta ni se borra)"
            )));
        }
        if !removing {
            match read_meta(data_dir, agent_id)? {
                Some(m) if m.agent_id == agent_id => {}
                Some(m) => {
                    return Err(Error::conflict(format!(
                        "el perfil declara agent_id={} — fail-closed",
                        m.agent_id
                    )));
                }
                None => {
                    return Err(Error::conflict(format!(
                        "perfil `{agent_id}` sin profile.json (incompleto) — repará con `profile create`"
                    )));
                }
            }
        }
    }

    // (2) Guard de runs no-terminales — SIEMPRE, incluso con --clear-threads.
    //     Sin evidencia de terminalidad ⇒ HOLD. DB ausente ⇒ HOLD (no hay
    //     evidencia que demuestre ausencia de runs no-terminales). Errores de
    //     apertura/consulta ⇒ HOLD.
    if !db_path.exists() {
        return Err(Error::conflict(format!(
            "no existe la base de evidencia de runs ({}) — no se puede demostrar que no haya runs no-terminales; fail-closed (creala con cualquier comando que use el store, p. ej. `thread new`)",
            db_path.display()
        )));
    }
    let store = open_store_for_guard(db_path)?;
    let active = count_non_terminal_runs(&store.conn, agent_id, &threads).map_err(|e| {
        Error::conflict(format!(
            "no se pudo verificar runs activos del perfil `{agent_id}` ({e}) — fail-closed"
        ))
    })?;
    if active > 0 {
        return Err(Error::conflict(format!(
            "perfil `{agent_id}`: {active} run(s) no-terminal(es) asociados — HOLD fail-closed; \
             reconciliá con `aeril recover` o esperá a que terminen"
        )));
    }
    let legacy = count_legacy_non_terminal_runs(&store.conn).map_err(|e| {
        Error::conflict(format!(
            "no se pudo verificar runs legacy sin perfil ({e}) — fail-closed"
        ))
    })?;
    if legacy > 0 {
        return Err(Error::conflict(format!(
            "hay {legacy} run(s) no-terminal(es) con runs.profile=NULL: \
             ownership legacy no demostrable — no se borra ningún perfil; \
             reconciliá esos runs y reintentá"
        )));
    }

    // (3) Asociaciones históricas del registro (override explícito).
    if !removing && !threads.is_empty() && !clear_threads {
        return Err(Error::conflict(format!(
            "el perfil tiene {} thread(s) asignados; desasigná (`profile unassign --thread T`) o usá --clear-threads",
            threads.len()
        )));
    }

    // Idempotente: registro sin directorio ⇒ solo limpiar registro (ya
    // verificado que no hay runs no-terminales asociados).
    if !dir_exists {
        if entry.is_some() {
            reg.agents.remove(agent_id);
            save_registry(data_dir, &reg)?;
            return Ok(());
        }
        return Err(Error::not_found(format!("perfil `{agent_id}` no existe")));
    }

    // (4) Owner/procesos y path, todavía bajo profile lock + registry lock.
    verify_no_survivor(data_dir, agent_id)?;
    let real = assert_safe_agent_path(data_dir, agent_id)?;
    let before = std::fs::symlink_metadata(&real)?;
    if before.file_type().is_symlink() {
        return Err(Error::conflict(
            "symlink swap detectado — fail-closed".to_string(),
        ));
    }
    let (dev, ino) = (before.dev(), before.ino());
    let again = std::fs::symlink_metadata(&real)?;
    if again.dev() != dev || again.ino() != ino || again.file_type().is_symlink() {
        return Err(Error::conflict(
            "el path cambió entre verificación y borrado — fail-closed".to_string(),
        ));
    }

    // (5) Tombstone durable antes de tocar el filesystem. Un crash a partir
    // de aquí deja un estado reanudable; `create`/assign rechazan el perfil.
    if !removing {
        let mut entry = entry.expect("dir exists and entry checked");
        entry.removing = true;
        reg.agents.insert(agent_id.to_string(), entry);
        save_registry(data_dir, &reg)?;
    }

    // (6) Borrado + registro atómico (idempotente si falla a mitad).
    std::fs::remove_dir_all(&real)?;
    reg.agents.remove(agent_id);
    save_registry(data_dir, &reg)?;
    Ok(())
}

// ── unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_id_validation() {
        for ok in ["a", "agent-1", "a_b-c9", "x".repeat(32).as_str()] {
            assert!(validate_agent_id(ok).is_ok(), "{ok} debe ser válido");
        }
        for bad in [
            "",
            "A",
            "a/b",
            "..",
            "../x",
            "-a",
            "a b",
            "á",
            &"x".repeat(33),
        ] {
            assert!(validate_agent_id(bad).is_err(), "{bad:?} debe ser inválido");
        }
    }

    #[test]
    fn registry_roundtrip() {
        let reg = Registry {
            version: 1,
            agents: [(
                "a".to_string(),
                RegistryEntry {
                    thread_ids: vec!["t1".into()],
                    created_ms: 7,
                    removing: false,
                },
            )]
            .into_iter()
            .collect(),
            updated_ms: 9,
        };
        let bytes = serde_json::to_vec(&reg).unwrap();
        let back: Registry = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.agents["a"].thread_ids, vec!["t1".to_string()]);
        assert_eq!(back.version, 1);
    }

    #[test]
    fn enabled_flag_parsing() {
        let prev = std::env::var(FLAG_ENV).ok();
        unsafe { std::env::remove_var(FLAG_ENV) };
        assert!(!enabled());
        unsafe { std::env::set_var(FLAG_ENV, "1") };
        assert!(enabled());
        match prev {
            Some(v) => unsafe { std::env::set_var(FLAG_ENV, v) },
            None => unsafe { std::env::remove_var(FLAG_ENV) },
        }
    }

    // ── P1-2: clasificación estricta de auth ────────────────────────────────

    #[test]
    fn auth_classifier_adversarial_matrix() {
        let cases: &[(&str, &str, &str)] = &[
            ("nous: logged in\n", "nous", "logged_in"),
            (
                "nous: logged in\n  auth_type: oauth\n  client_id: abc\n",
                "nous",
                "logged_in",
            ),
            ("nous: logged out\n", "nous", "absent"),
            (
                "nous: logged out (Hermes is not logged into Nous Portal.)\n",
                "nous",
                "absent",
            ),
            ("nous: not logged in\n", "nous", "absent"),
            ("nous: not signed in\n", "nous", "absent"),
            ("nous: not currently logged in\n", "nous", "absent"),
            ("nous: no credentials\n", "nous", "absent"),
            (
                "nous: Nous · free tier\n  Run `hermes auth upgrade` ...\n",
                "nous",
                "absent",
            ),
            ("github: logged in\n", "nous", "unknown"),
            ("nous: logged in\nnous: logged out\n", "nous", "unknown"),
            ("note: something happened\n", "nous", "unknown"),
            ("", "nous", "unknown"),
            (";;; not json\n", "nous", "unknown"),
            ("nous: weird status\n", "nous", "unknown"),
            ("nous:logged in\n", "nous", "logged_in"),
            ("NOUS: logged in\n", "nous", "logged_in"),
        ];
        for (stdout, provider, expected) in cases {
            let got = classify_auth_output(stdout, provider);
            assert_eq!(got, *expected, "stdout={stdout:?} provider={provider}");
        }
    }

    // ── P1-3: validador estricto de initialize ──────────────────────────────

    fn valid_init() -> String {
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "result": {
                "protocolVersion": 1,
                "agentInfo": {"name": "fake", "version": "0"},
                "agentCapabilities": {"loadSession": true},
                "authMethods": [{"id": "terminal", "name": "setup"}]
            }
        })
        .to_string()
    }

    #[test]
    fn init_validator_adversarial_matrix() {
        // Aceptar
        assert_eq!(
            validate_initialize_frame(valid_init().as_bytes()),
            Frame::Accept
        );
        // Notificación legítima → ignorar
        assert_eq!(
            validate_initialize_frame(br#"{"jsonrpc":"2.0","method":"note","params":{"a":1}}"#),
            Frame::Ignore
        );
        // Rechazos
        let rejects: Vec<String> = vec![
            // notificación con result top-level (suplantación)
            r#"{"jsonrpc":"2.0","method":"note","result":{"x":1}}"#.into(),
            // result string
            r#"{"jsonrpc":"2.0","id":1,"result":"ok"}"#.into(),
            // id incorrecto
            r#"{"jsonrpc":"2.0","id":99,"result":{"protocolVersion":1,"authMethods":[{"id":"t"}],"agentCapabilities":{}}}"#.into(),
            // result null
            r#"{"jsonrpc":"2.0","id":1,"result":null}"#.into(),
            // error JSON-RPC
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"boom"}}"#.into(),
            // sin jsonrpc
            r#"{"result":{"protocolVersion":1}}"#.into(),
            // jsonrpc distinto
            r#"{"jsonrpc":"1.0","id":1,"result":{"protocolVersion":1,"authMethods":[{"id":"t"}],"agentCapabilities":{}}}"#.into(),
            // sin protocolVersion
            r#"{"jsonrpc":"2.0","id":1,"result":{"authMethods":[{"id":"t"}],"agentCapabilities":{}}}"#.into(),
            // sin agentCapabilities
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"authMethods":[{"id":"t"}]}}"#.into(),
            // protocolVersion distinto
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":2,"authMethods":[{"id":"t"}],"agentCapabilities":{}}}"#.into(),
            // authMethods vacío
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"authMethods":[],"agentCapabilities":{}}}"#.into(),
            // authMethods no array
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"authMethods":{},"agentCapabilities":{}}}"#.into(),
            // authMethod sin id
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"authMethods":[{"name":"x"}],"agentCapabilities":{}}}"#.into(),
            // sin authMethods
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#.into(),
            // agentCapabilities no objeto
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"authMethods":[{"id":"t"}],"agentCapabilities":[]}}"#.into(),
            // id string
            r#"{"jsonrpc":"2.0","id":"1","result":{"protocolVersion":1,"authMethods":[{"id":"t"}],"agentCapabilities":{}}}"#.into(),
            // JSON inválido
            "{ nope".into(),
            // no objeto
            "[1,2,3]".into(),
        ];
        for line in rejects {
            match validate_initialize_frame(line.as_bytes()) {
                Frame::Reject(_) => {}
                other => panic!("debía rechazar {line:?}, obtuvo {other:?}"),
            }
        }
    }

    // ── H1: mountinfo fail-closed + inyección de fallos ─────────────────────

    #[test]
    fn mountinfo_fault_injection() {
        let dir = std::env::temp_dir().join(format!("aeril-r1-mnt-{}", crate::ids::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p");
        std::fs::create_dir_all(&path).unwrap();

        // mountinfo inaccesible ⇒ Err (fail-closed, nunca "sin mounts").
        let missing = dir.join("no-existe");
        assert!(has_mountpoint_inside_in(&path, &missing).is_err());

        // sin mounts dentro ⇒ Ok(false)
        let ok = dir.join("mountinfo-ok");
        std::fs::write(&ok, "36 35 98:0 /mnt / rw,relatime - ext4 /dev/root rw\n").unwrap();
        assert!(!has_mountpoint_inside_in(&path, &ok).unwrap());

        // mount dentro del path (con escape \040) ⇒ Ok(true)
        let inside = dir.join("mountinfo-inside");
        std::fs::write(
            &inside,
            format!(
                "36 35 98:0 /mnt {}/sub\\040dir rw,relatime - tmpfs tmpfs rw\n",
                path.display()
            ),
        )
        .unwrap();
        assert!(has_mountpoint_inside_in(&path, &inside).unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── H1: /proc fail-closed + inyección de fallos ─────────────────────────

    #[test]
    fn proc_scan_fault_injection() {
        let dir = std::env::temp_dir().join(format!("aeril-r1-proc-{}", crate::ids::new_id()));
        let proc = dir.join("proc");
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(proc.join("1234")).unwrap();
        let needle = format!("PATH=/usr/bin\0HERMES_HOME={}\0", home.display());
        std::fs::write(proc.join("1234/environ"), needle.as_bytes()).unwrap();

        // Encuentra el pid con HERMES_HOME del perfil.
        assert_eq!(processes_using_home_in(&home, &proc).unwrap(), vec![1234]);

        // Un auth probe del propio subsistema NO es escritor (se omite).
        std::fs::create_dir_all(proc.join("4321")).unwrap();
        std::fs::write(proc.join("4321/environ"), needle.as_bytes()).unwrap();
        std::fs::write(proc.join("4321/cmdline"), b"hermes\0auth\0status\0nous\0").unwrap();
        let found = processes_using_home_in(&home, &proc).unwrap();
        assert!(found.contains(&1234) && !found.contains(&4321));

        // Otro HERMES_HOME ⇒ no lo cuenta.
        let other = dir.join("other-home");
        std::fs::create_dir_all(&other).unwrap();
        assert!(processes_using_home_in(&other, &proc).unwrap().is_empty());

        // proc root inexistente ⇒ Err (fail-closed).
        assert!(processes_using_home_in(&home, &dir.join("nope")).is_err());

        // environ con EACCES (dumpable=0) sin señales alternativas ⇒ se omite
        // (host normal usable; NO se interpreta como dueño).
        std::fs::remove_dir_all(proc.join("1234")).unwrap();
        std::fs::create_dir_all(proc.join("5678")).unwrap();
        std::fs::write(proc.join("5678/environ"), b"HERMES_HOME=x\0").unwrap();
        std::fs::set_permissions(proc.join("5678/environ"), PermissionsExt::from_mode(0o000))
            .unwrap();
        assert!(processes_using_home_in(&home, &proc).unwrap().is_empty());

        // …pero si el cmdline (legible) menciona el perfil ⇒ se detecta.
        let home_s = home.to_string_lossy().to_string();
        std::fs::write(
            proc.join("5678/cmdline"),
            format!("hermes\0acp\0{home_s}\0"),
        )
        .unwrap();
        assert_eq!(processes_using_home_in(&home, &proc).unwrap(), vec![5678]);

        // fds apuntando dentro del perfil ⇒ también se detecta.
        std::fs::remove_file(proc.join("5678/cmdline")).unwrap();
        std::fs::create_dir_all(proc.join("5678/fd")).unwrap();
        let f = home.join("some-file");
        std::fs::write(&f, b"x").unwrap();
        std::os::unix::fs::symlink(&f, proc.join("5678/fd/3")).unwrap();
        assert_eq!(processes_using_home_in(&home, &proc).unwrap(), vec![5678]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── P1-1: el guard cuenta 'starting' y 'running' (perfil y thread) ─────

    #[test]
    fn non_terminal_run_guard_covers_starting_and_running() {
        let store = crate::store::Store::open_in_memory().unwrap();
        store
            .conn
            .execute(
                "INSERT INTO threads (id, title, repo_id, repo_path, state, next_seq, created_ms, updated_ms)
                 VALUES ('t1', 'x', 'r1', '/tmp', 'open', 0, 1, 1)",
                [],
            )
            .unwrap();
        let insert = |id: &str, state: &str, profile: Option<&str>| {
            store
                .conn
                .execute(
                    "INSERT INTO runs (id, thread_id, backend, state, started_ms, profile)
                     VALUES (?1, 't1', 'hermes', ?2, 1, ?3)",
                    rusqlite::params![id, state, profile],
                )
                .unwrap();
        };
        // 'starting' con perfil ⇒ cuenta (ruta durable).
        insert("r-start", "starting", Some("p1"));
        assert_eq!(count_non_terminal_runs(&store.conn, "p1", &[]).unwrap(), 1);
        // 'running' por thread (legacy sin profile) ⇒ cuenta.
        insert("r-run-legacy", "running", None);
        assert_eq!(
            count_non_terminal_runs(&store.conn, "p1", &["t1".to_string()]).unwrap(),
            2
        );
        // Terminales no cuentan.
        insert("r-done", "completed", Some("p1"));
        insert("r-failed", "failed", Some("p1"));
        assert_eq!(
            count_non_terminal_runs(&store.conn, "p1", &["t1".to_string()]).unwrap(),
            2
        );
        // Otro perfil no cuenta.
        assert_eq!(count_non_terminal_runs(&store.conn, "p2", &[]).unwrap(), 0);
    }

    // ── H2: temporales únicos + RMW de metadata ─────────────────────────────

    #[test]
    fn atomic_write_concurrent_unique_tmp() {
        let dir = std::env::temp_dir().join(format!("aeril-r1-atomic-{}", crate::ids::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("meta.json");
        let mut handles = Vec::new();
        for i in 0..8 {
            let t = target.clone();
            handles.push(std::thread::spawn(move || {
                let payload = format!("{{\"n\":{i}}}");
                write_atomic(&t, payload.as_bytes()).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // El archivo final es JSON válido y no quedan temporales.
        let text = std::fs::read_to_string(&target).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(v["n"].is_u64());
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temporales residuales: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
