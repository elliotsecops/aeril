//! P2-S1 — Perfiles Hermes gestionados por Aeril (detrás de feature flag).
//!
//! Un perfil vive en `<AERIL_DATA_DIR>/hermes/<agent_id>/`:
//!
//! ```text
//! <agent_id>/
//!   home/        HERMES_HOME (estado Hermes completo)
//!   san-home/    HOME sanitizado (XDG_* y GH_CONFIG_DIR derivan de acá)
//!   profile.json metadata de Aeril (0600; NUNCA credenciales)
//!   aeril.lock   flock de escritor único (retenido durante el writer ACP)
//! agents.json    registro agent_id→perfil y thread→perfil (0600)
//! .registry.lock flock del registro
//! ```
//!
//! Invariantes (P2-S1):
//! - Aeril **nunca** abre/parsea/copia `auth.json`; el estado de auth se lee
//!   solo por la CLI de Hermes (`auth status`) o el handshake ACP.
//! - Fail-closed: sin perfil asignado o sin auth verificada ⇒ el run hermes se
//!   rechaza; **sin fallback** al perfil del host ni a otro perfil.
//! - Escrituras atómicas + fsync; el registro se muta bajo `.registry.lock`;
//!   las operaciones son idempotentes y reconcilian estados parciales sin
//!   adopción silenciosa.
//! - Borrado solo explícito (`--confirm`), con checks de ownership, locks,
//!   procesos, symlinks y mountpoints.

use crate::error::{Error, Result};
use crate::timeutil::now_ms;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const FLAG_ENV: &str = "AERIL_HERMES_PROFILES";
pub const WARM_TIMEOUT_ENV: &str = "AERIL_PROFILE_WARM_TIMEOUT_SECS";
pub const AUTH_TIMEOUT_ENV: &str = "AERIL_PROFILE_AUTH_TIMEOUT_SECS";
pub const HERMES_BIN_ENV: &str = "AERIL_HERMES_BIN";

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

fn lock_path(data_dir: &Path, agent_id: &str) -> PathBuf {
    agent_dir(data_dir, agent_id).join("aeril.lock")
}

fn registry_path(data_dir: &Path) -> PathBuf {
    root(data_dir).join("agents.json")
}

fn registry_lock_path(data_dir: &Path) -> PathBuf {
    root(data_dir).join(".registry.lock")
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
/// perfiles y que no hay mountpoints dentro (fail-closed).
fn assert_safe_agent_path(data_dir: &Path, agent_id: &str) -> Result<PathBuf> {
    validate_agent_id(agent_id)?;
    let root_real =
        std::fs::canonicalize(root(data_dir)).unwrap_or_else(|_| root(data_dir).to_path_buf());
    let dir = agent_dir(data_dir, agent_id);
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
        Ok(real)
    } else {
        Ok(dir)
    }
}

/// ¿Algún mountpoint del sistema está dentro de `path`? (parse de mountinfo)
fn has_mountpoint_inside(path: &Path) -> Result<bool> {
    let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return Ok(false);
    };
    let path_s = path.to_string_lossy().to_string();
    for line in info.lines() {
        let Some(mp) = line.split_whitespace().nth(4) else {
            continue;
        };
        let mp = mp.replace("\\040", " ");
        if mp == path_s || mp.starts_with(&(path_s.clone() + "/")) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// ¿Algún proceso vivo tiene `HERMES_HOME=<home>`? (detección de usuarios)
fn processes_using_home(home: &Path) -> Result<Vec<i64>> {
    let needle = format!("HERMES_HOME={}", home.display());
    let mut pids = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Ok(pids);
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<i64>().ok()) else {
            continue;
        };
        let env_path = e.path().join("environ");
        let Ok(mut f) = File::open(&env_path) else {
            continue;
        };
        let mut buf = Vec::new();
        if f.read_to_end(&mut buf).is_err() {
            continue;
        }
        if buf
            .split(|b| *b == 0)
            .any(|chunk| chunk == needle.as_bytes())
        {
            pids.push(pid);
        }
    }
    Ok(pids)
}

// ── escrituras atómicas + registro ───────────────────────────────────────────

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| Error::integrity(format!("sin directorio padre: {}", path.display())))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("meta"),
        std::process::id()
    ));
    {
        let mut f = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RegistryEntry {
    pub thread_ids: Vec<String>,
    pub created_ms: i64,
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

fn load_registry(data_dir: &Path) -> Result<Registry> {
    let path = registry_path(data_dir);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| Error::integrity(format!("agents.json ilegible: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Registry::default()),
        Err(e) => Err(e.into()),
    }
}

fn save_registry(data_dir: &Path, reg: &Registry) -> Result<()> {
    let mut reg = reg.clone();
    reg.updated_ms = now_ms();
    let bytes = serde_json::to_vec_pretty(&reg)?;
    write_atomic(&registry_path(data_dir), &bytes)
}

/// Toma el flock del registro (bloqueante; se libera al morir el proceso).
fn lock_registry(data_dir: &Path) -> Result<File> {
    let path = registry_lock_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&path)?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        return Err(Error::conflict(format!(
            "no se pudo tomar el lock del registro: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(f)
}

// ── lock de escritor único por perfil ────────────────────────────────────────

/// Guard del flock exclusivo del perfil. Se libera al hacer drop (o al morir el
/// proceso: no quedan locks stale).
pub struct ProfileLock {
    _file: File,
}

impl ProfileLock {
    pub fn acquire(data_dir: &Path, agent_id: &str) -> Result<ProfileLock> {
        validate_agent_id(agent_id)?;
        let path = lock_path(data_dir, agent_id);
        let parent = path
            .parent()
            .ok_or_else(|| Error::integrity("lock sin padre".to_string()))?;
        std::fs::create_dir_all(parent)?;
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&path)?;
        let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            return Err(Error::conflict(format!(
                "el perfil `{agent_id}` ya tiene un escritor ACP activo (aeril.lock ocupado)"
            )));
        }
        Ok(ProfileLock { _file: f })
    }

    /// ¿El lock está tomado por alguien? Prueba **sin efectos colaterales**:
    /// si el archivo de lock no existe, no está tomado y no se crea nada
    /// (importante para `list`/`status`, que deben ser de solo lectura).
    pub fn is_held(data_dir: &Path, agent_id: &str) -> bool {
        let Ok(f) = OpenOptions::new()
            .read(true)
            .open(lock_path(data_dir, agent_id))
        else {
            return false;
        };
        let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        // rc == 0 ⇒ lo tomamos nosotros (no estaba tomado); el drop lo libera.
        rc != 0
    }
}

// ── binario hermes + entorno sanitizado ─────────────────────────────────────

pub fn hermes_bin() -> Result<PathBuf> {
    if let Ok(p) = std::env::var(HERMES_BIN_ENV)
        && !p.is_empty()
    {
        return Ok(PathBuf::from(p));
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let cand = dir.join("hermes");
        if cand.is_file() {
            return Ok(cand);
        }
    }
    Err(Error::not_found(
        "binario `hermes` no encontrado en PATH (o definí AERIL_HERMES_BIN)",
    ))
}

/// Entorno sanitizado del perfil (validado en P2-S0): HOME/XDG/GH_CONFIG_DIR
/// propios, HERMES_HOME explícito, PATH mínimo. Jamás el entorno heredado.
pub fn env_for_profile(data_dir: &Path, agent_id: &str) -> Result<Vec<(String, String)>> {
    let san = san_home_dir(data_dir, agent_id);
    let home = home_dir(data_dir, agent_id);
    // PATH mínimo + directorio del binario hermes (sus subprocesos pueden
    // necesitar encontrarlo; nunca el PATH heredado del host).
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

/// Estado de auth por perfil, **sin leer credenciales**: ejecuta
/// `hermes auth status <provider>` con el entorno sanitizado y clasifica la
/// salida (`logged_in|absent|unknown`). Nunca abre `auth.json`.
pub fn auth_status(data_dir: &Path, agent_id: &str) -> Result<String> {
    let meta = read_meta(data_dir, agent_id)?
        .ok_or_else(|| Error::not_found(format!("perfil `{agent_id}` sin metadata")))?;
    let provider = if meta.provider.is_empty() {
        "nous".to_string()
    } else {
        meta.provider.clone()
    };
    let bin = hermes_bin()?;
    let env = env_for_profile(data_dir, agent_id)?;
    let timeout = Duration::from_secs(env_secs(AUTH_TIMEOUT_ENV, 20));
    let mut child = std::process::Command::new(&bin)
        .args(["auth", "status", &provider])
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| Error::backend(format!("no se pudo ejecutar `hermes auth status`: {e}")))?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Ok("unknown".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(e.into()),
        }
    }
    let out = child.wait_with_output()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
    .to_lowercase();
    // OJO: "not logged in" contiene "logged in"; chequear primero el negativo.
    let status = if text.contains("not logged in") || text.contains("no credentials") {
        "absent"
    } else if text.contains("logged in") {
        "logged_in"
    } else {
        "unknown"
    };
    // Persistir solo metadata (sin secretos).
    if let Some(mut meta) = read_meta(data_dir, agent_id)? {
        meta.auth_status = status.to_string();
        meta.last_auth_check_ms = Some(now_ms());
        write_meta(data_dir, agent_id, &meta)?;
    }
    Ok(status.into())
}

// ── metadata ─────────────────────────────────────────────────────────────────

fn read_meta(data_dir: &Path, agent_id: &str) -> Result<Option<ProfileMeta>> {
    match std::fs::read(meta_path(data_dir, agent_id)) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).map_err(|e| {
            Error::integrity(format!("profile.json ilegible ({agent_id}): {e}"))
        })?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn write_meta(data_dir: &Path, agent_id: &str, meta: &ProfileMeta) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(meta)?;
    write_atomic(&meta_path(data_dir, agent_id), &bytes)
}

fn ensure_dirs(data_dir: &Path, agent_id: &str) -> Result<()> {
    for p in [
        root(data_dir),
        agent_dir(data_dir, agent_id),
        home_dir(data_dir, agent_id),
        san_home_dir(data_dir, agent_id),
    ] {
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true).mode(0o700);
        b.create(&p)?;
        let _ = std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o700));
    }
    Ok(())
}

// ── ciclo de vida ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ProfileView {
    pub agent_id: String,
    pub registered: bool,
    pub dir_state: String, // ok | incomplete | missing | orphan
    pub auth_status: String,
    pub thread_ids: Vec<String>,
    pub lock_held: bool,
    pub warmed_ms: Option<i64>,
    pub disk_kb: u64,
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
        if let Ok(m) = e.metadata() {
            if m.is_dir() {
                total += disk_kb(&e.path());
            } else {
                total += m.len() / 1024;
            }
        }
    }
    total
}

/// Crea (idempotente) un perfil. Repara un create interrumpido SOLO si el
/// directorio está registrado o vacío; nunca adopta directorios ajenos.
pub fn create(
    data_dir: &Path,
    agent_id: &str,
    provider: &str,
    model: Option<&str>,
    warm: bool,
) -> Result<ProfileView> {
    validate_agent_id(agent_id)?;
    let _reg_lock = lock_registry(data_dir)?;
    let mut reg = load_registry(data_dir)?;
    let registered = reg.agents.contains_key(agent_id);
    let dir = agent_dir(data_dir, agent_id);
    if dir.exists() {
        // Fail-closed: nunca operar a través de symlinks ni fuera del root.
        let _ = assert_safe_agent_path(data_dir, agent_id)?;
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
                write_meta(data_dir, agent_id, &meta)?;
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
        write_meta(data_dir, agent_id, &meta)?;
    }
    reg.agents
        .entry(agent_id.to_string())
        .or_insert_with(|| RegistryEntry {
            thread_ids: Vec::new(),
            created_ms: now_ms(),
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
    let auth_status = meta
        .as_ref()
        .map(|m| m.auth_status.clone())
        .unwrap_or_else(|| "unknown".into());
    let thread_ids = load_registry(data_dir)?
        .agents
        .get(agent_id)
        .map(|e| e.thread_ids.clone())
        .unwrap_or_default();
    Ok(ProfileView {
        agent_id: agent_id.to_string(),
        registered,
        dir_state: dir_state(data_dir, agent_id, registered),
        auth_status,
        thread_ids,
        lock_held: ProfileLock::is_held(data_dir, agent_id),
        warmed_ms: meta.and_then(|m| m.warmed_ms),
        disk_kb: disk_kb(&agent_dir(data_dir, agent_id)),
    })
}

/// Estado del perfil con verificación de auth fresca (no lee credenciales).
pub fn status(data_dir: &Path, agent_id: &str) -> Result<ProfileView> {
    let registered = load_registry(data_dir)?.agents.contains_key(agent_id);
    let _ = auth_status(data_dir, agent_id)?; // refresca metadata (metadata-only)
    status_view(data_dir, agent_id, registered)
}

/// Pre-warm acotado: `hermes acp` + initialize con timeout duro y progreso.
/// Retiene el lock de escritor durante el handshake.
pub fn warm_profile(data_dir: &Path, agent_id: &str) -> Result<i64> {
    let _lock = ProfileLock::acquire(data_dir, agent_id)?;
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
        .spawn()
        .map_err(|e| Error::backend(format!("no se pudo lanzar `hermes acp`: {e}")))?;
    let started = std::time::Instant::now();
    let init = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": 1, "clientCapabilities": {}, "clientInfo": {"name": "aeril-profile-warm", "version": env!("CARGO_PKG_VERSION")}}
    });
    {
        let stdin = child.stdin.as_mut().expect("stdin piped");
        writeln!(stdin, "{init}")?;
        stdin.flush()?;
    }
    eprintln!("[profile] warm {agent_id}: initialize enviado; esperando respuesta…");
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    let deadline = started + timeout;
    let ok = loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break false;
        }
        // poll() con timeout acotado: la lectura nunca puede bloquear más allá
        // del deadline (aunque el backend no emita nada).
        let stdout = child.stdout.as_mut().expect("stdout piped");
        let mut pfd = libc::pollfd {
            fd: stdout.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = remaining.as_millis().clamp(1, 200) as i32;
        let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
        if rc < 0 {
            break false;
        }
        if rc == 0 {
            continue;
        }
        match stdout.read(&mut byte) {
            Ok(0) => break false,
            Ok(_) => {
                buf.push(byte[0]);
                if byte[0] == b'\n' {
                    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf)
                        && v.get("result").is_some()
                    {
                        break true;
                    }
                    buf.clear();
                }
            }
            Err(_) => break false,
        }
    };
    let elapsed_ms = started.elapsed().as_millis() as i64;
    let _ = child.kill();
    let _ = child.wait();
    if !ok {
        return Err(Error::backend(format!(
            "pre-warm del perfil `{agent_id}` falló o excedió el plazo ({}s)",
            timeout.as_secs()
        )));
    }
    if let Some(mut meta) = read_meta(data_dir, agent_id)? {
        meta.warmed_ms = Some(elapsed_ms);
        write_meta(data_dir, agent_id, &meta)?;
    }
    eprintln!("[profile] warm {agent_id}: listo en {elapsed_ms} ms");
    Ok(elapsed_ms)
}

/// Mapeo thread→perfil (asignación explícita; única por thread).
pub fn agent_for_thread(data_dir: &Path, thread_id: &str) -> Result<Option<String>> {
    let reg = load_registry(data_dir)?;
    Ok(reg
        .agents
        .iter()
        .find(|(_, e)| e.thread_ids.iter().any(|t| t == thread_id))
        .map(|(id, _)| id.clone()))
}

pub fn assign(data_dir: &Path, agent_id: &str, thread_id: &str) -> Result<()> {
    validate_agent_id(agent_id)?;
    let _reg_lock = lock_registry(data_dir)?;
    let mut reg = load_registry(data_dir)?;
    if !reg.agents.contains_key(agent_id) {
        return Err(Error::not_found(format!(
            "perfil `{agent_id}` no registrado (creálo con `aeril profile create`)"
        )));
    }
    if !meta_path(data_dir, agent_id).exists() {
        return Err(Error::conflict(format!(
            "perfil `{agent_id}` incompleto (sin profile.json) — repará con `profile create`"
        )));
    }
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

pub fn unassign(data_dir: &Path, thread_id: &str) -> Result<bool> {
    let _reg_lock = lock_registry(data_dir)?;
    let mut reg = load_registry(data_dir)?;
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

/// Borrado explícito y verificado. Fail-closed ante cualquier duda.
pub fn remove(
    data_dir: &Path,
    agent_id: &str,
    confirm: bool,
    clear_threads: bool,
    db_path: Option<&Path>,
) -> Result<()> {
    if !confirm {
        return Err(Error::invalid(
            "borrado de perfil requiere --confirm (acción explícita)",
        ));
    }
    validate_agent_id(agent_id)?;
    let _reg_lock = lock_registry(data_dir)?;
    let mut reg = load_registry(data_dir)?;
    let entry = reg.agents.get(agent_id).cloned();
    let dir = agent_dir(data_dir, agent_id);
    // Idempotente: registro sin directorio ⇒ solo limpiar registro.
    if !dir.exists() {
        if entry.is_some() {
            reg.agents.remove(agent_id);
            save_registry(data_dir, &reg)?;
            return Ok(());
        }
        return Err(Error::not_found(format!("perfil `{agent_id}` no existe")));
    }
    // Ownership: el profile.json debe declarar el mismo agent_id.
    let meta = read_meta(data_dir, agent_id)?;
    match &meta {
        Some(m) if m.agent_id == agent_id => {}
        Some(m) => {
            return Err(Error::conflict(format!(
                "el perfil declara agent_id={} — fail-closed",
                m.agent_id
            )));
        }
        None => {
            return Err(Error::conflict(format!(
                "perfil `{agent_id}` sin profile.json (incompleto) — repará con `profile create` o borralo manualmente con cuidado"
            )));
        }
    }
    // Referencias históricas del registro.
    let threads = entry.map(|e| e.thread_ids).unwrap_or_default();
    if !threads.is_empty() && !clear_threads {
        return Err(Error::conflict(format!(
            "el perfil tiene {} thread(s) asignados; desasigná (`profile unassign --thread T`) o usá --clear-threads",
            threads.len()
        )));
    }
    // Runs activos (starting/running) de esos threads ⇒ fail-closed. Con
    // `--clear-threads` el operador declara que las referencias son
    // históricas; los checks de lock/procesos/symlinks siguen fail-closed.
    if !clear_threads
        && let Some(db) = db_path
        && !threads.is_empty()
        && let Ok(store) = crate::store::Store::open(db)
    {
        for t in &threads {
            let active: i64 = store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM runs WHERE thread_id = ?1 AND state IN ('starting','running')",
                    [t],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            if active > 0 {
                return Err(Error::conflict(format!(
                    "hay runs activos del thread {t} — fail-closed"
                )));
            }
        }
    }
    // Escritor ACP vivo (lock) o proceso con HERMES_HOME apuntando al perfil.
    if ProfileLock::is_held(data_dir, agent_id) {
        return Err(Error::conflict(format!(
            "el perfil `{agent_id}` tiene un escritor ACP activo (lock) — fail-closed"
        )));
    }
    let users = processes_using_home(&home_dir(data_dir, agent_id))?;
    if !users.is_empty() {
        return Err(Error::conflict(format!(
            "hay procesos vivos con HERMES_HOME del perfil (pids {:?}) — fail-closed",
            users
        )));
    }
    // Paths: symlink/mountpoint/fuera del root ⇒ fail-closed (con verificación
    // de inodo justo antes del borrado; remove_dir_all no sigue symlinks).
    let real = assert_safe_agent_path(data_dir, agent_id)?;
    let before = std::fs::symlink_metadata(&real)?;
    if before.file_type().is_symlink() {
        return Err(Error::conflict(
            "symlink swap detectado — fail-closed".to_string(),
        ));
    }
    let (dev, ino) = {
        use std::os::unix::fs::MetadataExt;
        (before.dev(), before.ino())
    };
    let again = std::fs::symlink_metadata(&real)?;
    use std::os::unix::fs::MetadataExt;
    if again.dev() != dev || again.ino() != ino || again.file_type().is_symlink() {
        return Err(Error::conflict(
            "el path cambió entre verificación y borrado — fail-closed".to_string(),
        ));
    }
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
        // No depende del entorno real: solo comprueba que la función no entra
        // en pánico y que el default sin variable es false.
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
}
