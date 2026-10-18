//! Cliente ACP (Agent Client Protocol) sobre stdio, ndjson, JSON-RPC 2.0.
//!
//! Decisiones verificadas contra Hermes 0.21.5 (ver `docs/acp-hermes-spec.md`):
//! - framing: una línea JSON compacta por mensaje (sin Content-Length);
//! - stdout reservado al protocolo; stderr es log (se guarda acotado);
//! - los requests del servidor (permission) pueden tener `id` 0;
//! - la respuesta de `session/prompt` llega recién al terminar el turno;
//!   el streaming llega por notificaciones `session/update`;
//! - `session/load` de una sesión desconocida responde OK sin historial
//!   (no es distinguible de un load real: limitación documentada).

use crate::agent::BackendSpec;
use crate::error::{Error, Result};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

#[derive(Debug, Clone)]
pub struct PermissionOption {
    pub option_id: String,
    pub kind: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct PermissionRequest {
    /// `id` JSON-RPC del request servidor→cliente (puede ser 0).
    pub request_id: Value,
    pub tool_call_id: String,
    pub title: String,
    pub options: Vec<PermissionOption>,
    pub diff_path: Option<String>,
}

#[derive(Debug, Clone)]
pub enum PermissionDecision {
    Selected { option_id: String, policy: String },
    Cancelled { policy: String },
}

/// Resultado de un turno de `session/prompt`.
#[derive(Debug, Clone)]
pub struct TurnCompletion {
    pub stop_reason: String,
    /// Cancelación pedida y NO confirmada: se agotó el plazo fijo tras el
    /// `session/cancel` y el backend fue terminado.
    pub cancel_unconfirmed: bool,
}

/// Observador del turno: persistencia y política viven fuera del protocolo.
pub trait TurnObserver {
    /// Llamado inmediatamente después de enviar el prompt: delimita qué
    /// notificaciones cuentan como evidencia DE ESTE turno.
    fn on_turn_started(&mut self) {}
    /// Notificación `session/update`.
    fn on_update(&mut self, update: &Value) -> Result<()>;
    /// Request `session/request_permission`: decide y registra.
    /// `Err` ⇒ fail closed: el cliente deniega y el turno termina.
    fn on_permission(&mut self, req: &PermissionRequest) -> Result<PermissionDecision>;
    /// Resultado del intento de transmisión de la decisión al backend.
    /// La decisión durable ya existe; esto NO la reemplaza ni la afirma.
    fn on_permission_sent(
        &mut self,
        _req: &PermissionRequest,
        _decision: &PermissionDecision,
        _ok: bool,
    ) {
    }
    /// La persistencia del estado de autorización falló (fail closed).
    fn on_permission_failed(&mut self, _req: &PermissionRequest, _detail: &str) {}
}

/// Observador vacío para llamadas de control (initialize/session/new).
pub struct NoopObserver;

impl TurnObserver for NoopObserver {
    fn on_update(&mut self, _update: &Value) -> Result<()> {
        Ok(())
    }
    fn on_permission(&mut self, _req: &PermissionRequest) -> Result<PermissionDecision> {
        Ok(PermissionDecision::Cancelled {
            policy: "control_phase_default_deny".into(),
        })
    }
}

enum ServerMsg {
    Notification(Value),
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Response {
        id: u64,
        result: std::result::Result<Value, RpcError>,
    },
}

/// Presupuesto de bytes EN VUELO del pipeline ACP (RC2-B1).
///
/// El productor (lector de stdout) **reserva bytes antes de interpretar JSON**
/// y el consumidor los libera al terminar cada frame (Drop del guard). La cola
/// transporta la LÍNEA CRUDA (sin decodificar): el costo en cola es el tamaño
/// real del wire, no una representación amplificada. Un frame individual mayor
/// que el presupuesto espera a que la cola se vacíe y transita solo.
struct InflightBudget {
    sem: std::sync::Arc<tokio::sync::Semaphore>,
    current: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    peak: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    budget: usize,
}

impl InflightBudget {
    fn new(budget: usize) -> Self {
        // Clamp defensivo: `Semaphore::new` paniquea por encima de MAX_PERMITS
        // y `acquire_many` toma u32 — una config extrema por env no debe
        // tumbar el proceso.
        let max = tokio::sync::Semaphore::MAX_PERMITS.min(u32::MAX as usize);
        let budget = budget.clamp(1, max);
        InflightBudget {
            sem: std::sync::Arc::new(tokio::sync::Semaphore::new(budget)),
            current: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            peak: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            budget,
        }
    }

    /// Reserva cupo para un frame. Backpressure real: espera si la cola está
    /// llena. Devuelve los permits retenidos (a liberar por el consumidor).
    async fn reserve(&self, size: usize) -> Option<u32> {
        // Los frames mayores que el presupuesto ocupan TODO el cupo: no pueden
        // coexistir en cola con otros (garantía de cota, sin alloc extra).
        let want = if size >= self.budget {
            self.budget
        } else {
            size.max(1)
        };
        let want = want.min(u32::MAX as usize) as u32;
        let permits = tokio::sync::Semaphore::acquire_many_owned(self.sem.clone(), want)
            .await
            .ok()?;
        let now = self
            .current
            .fetch_add(want as usize, std::sync::atomic::Ordering::SeqCst)
            + want as usize;
        self.peak
            .fetch_max(now, std::sync::atomic::Ordering::SeqCst);
        std::mem::forget(permits); // se libera vía release() del consumidor
        Some(want)
    }

    fn release(&self, permits: u32) {
        self.current
            .fetch_sub(permits as usize, std::sync::atomic::Ordering::SeqCst);
        self.sem.add_permits(permits as usize);
    }
}

/// Lo que cruza el canal interno productor → consumidor.
enum Incoming {
    /// Frame crudo encolado (sin decodificar) + su reserva de presupuesto.
    Frame(QueuedFrame),
    /// Fin del canal con causa (EOF, límite, error de lectura).
    Closed(String),
}

/// Frame crudo encolado (sin decodificar) + su reserva de presupuesto.
struct QueuedFrame {
    line: String,
    size: usize,
    permits: u32,
    budget: std::sync::Arc<InflightBudget>,
}

impl QueuedFrame {
    fn decoded(&self) -> Result<ServerMsg> {
        classify(&self.line)
    }
}

/// Guard del consumidor: libera la reserva SIEMPRE (éxito, error, cancelación
/// o cierre del canal), porque se libera en Drop.
impl Drop for QueuedFrame {
    fn drop(&mut self) {
        self.budget.release(self.permits);
    }
}

/// Límites del pipeline de entrada ACP. Defaults conservadores para produccion;
/// los tests usan `spawn_with_limits` con valores chicos y deterministas.
#[derive(Debug, Clone, Copy)]
pub struct AcpLimits {
    /// Máximo por línea/mensaje: rechazo explícito antes de asignar sin cota.
    pub max_line_bytes: usize,
    /// Máximo acumulado de bytes recibidos en un turno (o fase de control).
    pub max_turn_bytes: usize,
    /// Presupuesto de bytes EN VUELO en la cola cruda productor→consumidor.
    pub inflight_bytes: usize,
}

impl AcpLimits {
    pub fn from_env() -> Self {
        AcpLimits {
            max_line_bytes: crate::util::env_usize("AERIL_ACP_MAX_LINE_BYTES", 4 * 1024 * 1024),
            max_turn_bytes: crate::util::env_usize("AERIL_ACP_MAX_TURN_BYTES", 64 * 1024 * 1024),
            inflight_bytes: crate::util::env_usize("AERIL_ACP_INFLIGHT_BYTES", 8 * 1024 * 1024),
        }
    }
}

impl Default for AcpLimits {
    fn default() -> Self {
        AcpLimits::from_env()
    }
}

/// Lee una línea con cota estricta de memoria: nunca se acumulan más de
/// `limit` bytes (más el chunk de `fill_buf`, que está acotado por el buffer).
async fn read_bounded_line<R>(reader: &mut R, limit: usize) -> std::io::Result<Option<Vec<u8>>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    let mut out: Vec<u8> = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if out.is_empty() { None } else { Some(out) });
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(pos) => {
                if out.len() + pos > limit {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("mensaje ACP de más de {limit} bytes: rechazado"),
                    ));
                }
                out.extend_from_slice(&available[..pos]);
                reader.consume(pos + 1);
                return Ok(Some(out));
            }
            None => {
                if out.len() + available.len() > limit {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("mensaje ACP de más de {limit} bytes: rechazado"),
                    ));
                }
                out.extend_from_slice(available);
                let n = available.len();
                reader.consume(n);
            }
        }
    }
}

/// Como [`read_bounded_line`] pero para stderr: conserva hasta `keep` bytes
/// por línea y descarta el resto (el log es diagnóstico, no protocolo).
async fn read_line_lossy_bounded<R>(reader: &mut R, keep: usize) -> std::io::Result<Option<String>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    let mut out: Vec<u8> = Vec::new();
    let mut truncated = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if out.is_empty() && !truncated {
                return Ok(None);
            }
            return Ok(Some(finalize_lossy(out, truncated)));
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(pos) => {
                if out.len() < keep {
                    let take = (keep - out.len()).min(pos);
                    out.extend_from_slice(&available[..take]);
                    if take < pos {
                        truncated = true;
                    }
                } else if pos > 0 {
                    truncated = true;
                }
                reader.consume(pos + 1);
                return Ok(Some(finalize_lossy(out, truncated)));
            }
            None => {
                if out.len() < keep {
                    let take = (keep - out.len()).min(available.len());
                    out.extend_from_slice(&available[..take]);
                    if take < available.len() {
                        truncated = true;
                    }
                } else if !available.is_empty() {
                    truncated = true;
                }
                let n = available.len();
                reader.consume(n);
            }
        }
    }
}

fn finalize_lossy(mut out: Vec<u8>, truncated: bool) -> String {
    let mut s = String::from_utf8_lossy(&out).into_owned();
    if truncated {
        s.push_str("…[truncado]");
    }
    out.clear();
    s
}

#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rpc {}: {}", self.code, self.message)
    }
}

fn classify(line: &str) -> Result<ServerMsg> {
    let v: Value = serde_json::from_str(line)
        .map_err(|e| Error::backend(format!("línea ACP no parseable: {e}")))?;
    if v.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(Error::backend("mensaje ACP sin jsonrpc \"2.0\""));
    }
    let has_method = v.get("method").is_some();
    match (has_method, v.get("id").cloned()) {
        (true, Some(id)) => Ok(ServerMsg::Request {
            id,
            method: v["method"].as_str().unwrap_or_default().to_string(),
            params: v.get("params").cloned().unwrap_or(Value::Null),
        }),
        (true, None) => Ok(ServerMsg::Notification(v)),
        (false, Some(id)) => {
            let idnum = id
                .as_u64()
                .ok_or_else(|| Error::backend("respuesta con id no numérico"))?;
            let result = match v.get("error") {
                Some(err) => Err(RpcError {
                    code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                }),
                None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
            };
            Ok(ServerMsg::Response { id: idnum, result })
        }
        (false, None) => Err(Error::backend("mensaje ACP sin método ni id")),
    }
}

pub struct AcpClient {
    stdin: ChildStdin,
    child: Child,
    rx: mpsc::Receiver<Incoming>,
    budget: std::sync::Arc<InflightBudget>,
    limits: AcpLimits,
    next_id: u64,
    stderr_tail: Arc<Mutex<Vec<String>>>,
    pub session_id: Option<String>,
    control_timeout: Duration,
    turn_timeout: Duration,
}

fn env_secs(name: &str, default: u64) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(default))
}

impl AcpClient {
    pub async fn spawn(spec: &BackendSpec, cwd: &Path) -> Result<AcpClient> {
        Self::spawn_with_limits(spec, cwd, AcpLimits::from_env()).await
    }

    /// Como [`AcpClient::spawn`] con límites explícitos (tests deterministas).
    pub async fn spawn_with_limits(
        spec: &BackendSpec,
        cwd: &Path,
        limits: AcpLimits,
    ) -> Result<AcpClient> {
        let mut cmd = Command::new(&spec.program);
        if !spec.env.is_empty() {
            // P2-S1: entorno sanitizado del perfil gestionado (nunca heredado
            // para operaciones autenticadas con perfiles).
            cmd.env_clear();
            cmd.envs(spec.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        }
        cmd.args(&spec.args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Grupo de procesos propio: kill() mata también a los workers
            // que el backend haya lanzado.
            .process_group(0)
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| Error::backend(format!("no se pudo lanzar `{}`: {e}", spec.program)))?;
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        let stderr_tail = Arc::new(Mutex::new(Vec::<String>::new()));
        {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                // Buffer acotado: 200 líneas × ≤4 KiB por línea = ≤800 KiB.
                while let Ok(Some(l)) = read_line_lossy_bounded(&mut reader, 4096).await {
                    let mut v = tail.lock().unwrap();
                    if v.len() >= 200 {
                        v.remove(0);
                    }
                    v.push(l);
                }
            });
        }

        let budget = std::sync::Arc::new(InflightBudget::new(limits.inflight_bytes));
        let (tx, rx) = mpsc::channel(64);
        {
            let budget = budget.clone();
            let max_line = limits.max_line_bytes;
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                loop {
                    match read_bounded_line(&mut reader, max_line).await {
                        Ok(Some(bytes)) => {
                            let size = bytes.len();
                            if bytes.iter().all(|b| b.is_ascii_whitespace()) {
                                continue;
                            }
                            // Presupuesto ANTES de interpretar/encolar: el
                            // productor se frena si la cola está llena.
                            let Some(permits) = budget.reserve(size).await else {
                                break;
                            };
                            let line = String::from_utf8_lossy(&bytes).into_owned();
                            let frame = QueuedFrame {
                                line,
                                size,
                                permits,
                                budget: budget.clone(),
                            };
                            if tx.send(Incoming::Frame(frame)).await.is_err() {
                                // Canal cerrado: el guard del frame libera la
                                // reserva en Drop.
                                break;
                            }
                        }
                        Ok(None) => {
                            let _ = tx.send(Incoming::Closed("EOF del backend".into())).await;
                            break;
                        }
                        Err(e) => {
                            let _ = tx
                                .send(Incoming::Closed(format!("entrada rechazada: {e}")))
                                .await;
                            break;
                        }
                    }
                }
            });
        }

        Ok(AcpClient {
            stdin,
            child,
            rx,
            budget,
            limits,
            next_id: 1,
            stderr_tail,
            session_id: None,
            control_timeout: env_secs("AERIL_CONTROL_TIMEOUT_SECS", 60),
            turn_timeout: env_secs("AERIL_TURN_TIMEOUT_SECS", 300),
        })
    }

    pub fn stderr_tail(&self) -> String {
        self.stderr_tail
            .lock()
            .map(|v| v.join("\n"))
            .unwrap_or_default()
    }

    async fn send(&mut self, msg: &Value) -> Result<()> {
        let mut line = serde_json::to_string(msg)?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| Error::backend(format!("no se pudo escribir al backend: {e}")))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| Error::backend(format!("flush al backend falló: {e}")))?;
        Ok(())
    }

    fn next_request_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    async fn respond(&mut self, id: &Value, result: Value) -> Result<()> {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await
    }

    async fn respond_error(&mut self, id: &Value, code: i64, message: &str) -> Result<()> {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}))
            .await
    }

    fn closed_err(&self, reason: &str) -> Error {
        let tail = self.stderr_tail();
        if tail.is_empty() {
            Error::backend(reason.to_string())
        } else {
            Error::backend(format!(
                "{reason}\nstderr del backend (últimas líneas):\n{tail}"
            ))
        }
    }

    /// Request de control: espera la respuesta con timeout; las notificaciones
    /// y requests intercalados se despachan al observador.
    pub async fn request(
        &mut self,
        method: &str,
        params: Value,
        obs: &mut dyn TurnObserver,
    ) -> Result<Value> {
        let id = self.next_request_id();
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        let mut control_bytes: usize = 0;
        loop {
            let incoming = match timeout(self.control_timeout, self.rx.recv()).await {
                Ok(Some(m)) => m,
                Ok(None) => return Err(self.closed_err("el canal del backend se cerró")),
                Err(_) => {
                    return Err(Error::backend(format!(
                        "timeout de control esperando `{method}` ({:?})",
                        self.control_timeout
                    )));
                }
            };
            let slot = match incoming {
                Incoming::Closed(reason) => return Err(self.closed_err(&reason)),
                Incoming::Frame(slot) => slot,
            };
            control_bytes += slot.size;
            if control_bytes > self.limits.max_turn_bytes {
                let _ = self.kill().await;
                return Err(Error::backend(format!(
                    "la fase de control excedió el máximo de entrada ({} bytes)",
                    self.limits.max_turn_bytes
                )));
            }
            let msg = match slot.decoded() {
                Ok(m) => m,
                Err(e) => {
                    let _ = self.kill().await;
                    return Err(e);
                }
            };
            match msg {
                ServerMsg::Response { id: rid, result } if rid == id => {
                    return result.map_err(|e| Error::backend(format!("`{method}` falló: {e}")));
                }
                ServerMsg::Response { id: rid, .. } => {
                    return Err(Error::backend(format!(
                        "respuesta inesperada (id {rid}, esperaba {id})"
                    )));
                }
                ServerMsg::Notification(v) => {
                    if v["method"].as_str() == Some("session/update") {
                        // En fases de control se registra igual (p. ej. usage_update).
                        if let Some(update) = v.pointer("/params/update") {
                            obs.on_update(update)?;
                        }
                    }
                }
                ServerMsg::Request { id, method, params } => {
                    self.dispatch_server_request(&id, &method, &params, obs)
                        .await?;
                }
            }
        }
    }

    async fn dispatch_server_request(
        &mut self,
        id: &Value,
        method: &str,
        params: &Value,
        obs: &mut dyn TurnObserver,
    ) -> Result<()> {
        match method {
            "session/request_permission" => {
                let mut req = parse_permission(params);
                req.request_id = id.clone();
                match obs.on_permission(&req) {
                    Ok(decision) => {
                        let result = match &decision {
                            PermissionDecision::Selected { option_id, .. } => {
                                json!({"outcome": {"outcome": "selected", "optionId": option_id}})
                            }
                            PermissionDecision::Cancelled { .. } => {
                                json!({"outcome": {"outcome": "cancelled"}})
                            }
                        };
                        let send_res = self.respond(id, result).await;
                        obs.on_permission_sent(&req, &decision, send_res.is_ok());
                        send_res
                    }
                    Err(e) => {
                        // Fail closed: sin auditoría durable no hay
                        // autorización. Se intenta denegar (cancelled) y el
                        // turno termina con error (nunca un allow silencioso).
                        let _ = self
                            .respond(id, json!({"outcome": {"outcome": "cancelled"}}))
                            .await;
                        obs.on_permission_failed(&req, &e.to_string());
                        Err(e)
                    }
                }
            }
            other => {
                // Métodos de cliente que Hermes no usa (fs/*, terminal/*):
                // se rechaza explícitamente en vez de ignorar en silencio.
                self.respond_error(
                    id,
                    -32601,
                    &format!("método de cliente no soportado: {other}"),
                )
                .await
            }
        }
    }

    /// Inicializa la conexión. Devuelve el `result` del agente.
    pub async fn initialize(
        &mut self,
        client_name: &str,
        client_version: &str,
        strict: bool,
    ) -> Result<Value> {
        let params = json!({
            "protocolVersion": 1,
            "clientCapabilities": {
                "fs": {"readTextFile": false, "writeTextFile": false},
                "terminal": false
            },
            "clientInfo": {"name": client_name, "version": client_version}
        });
        let mut obs = NoopObserver;
        let result = self.request("initialize", params, &mut obs).await?;
        if strict {
            crate::profile::validate_initialize_result(&result)?;
        }
        Ok(result)
    }

    /// Abre sesión nueva en `cwd`. Devuelve el `sessionId`.
    pub async fn session_new(&mut self, cwd: &Path, obs: &mut dyn TurnObserver) -> Result<String> {
        let params = json!({"cwd": cwd.to_string_lossy(), "mcpServers": []});
        let result = self.request("session/new", params, obs).await?;
        let sid = result
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::backend("session/new sin sessionId en el resultado"))?
            .to_string();
        self.session_id = Some(sid.clone());
        Ok(sid)
    }

    /// Intenta reanudar una sesión existente (`session/load`). Devuelve `true`
    /// si el motor aceptó el load. Limitación: Hermes responde OK también para
    /// sesiones desconocidas (no es distinguible), documentado.
    pub async fn session_load(
        &mut self,
        session_id: &str,
        cwd: &Path,
        obs: &mut dyn TurnObserver,
    ) -> Result<bool> {
        let params = json!({
            "sessionId": session_id,
            "cwd": cwd.to_string_lossy(),
            "mcpServers": []
        });
        match self.request("session/load", params, obs).await {
            Ok(_) => {
                self.session_id = Some(session_id.to_string());
                Ok(true)
            }
            Err(Error::Backend(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Fija el modelo de la sesión (`session/set_model`). Algunos backends no
    /// lo soportan: el error se propaga y el run no continúa a ciegas.
    pub async fn session_set_model(&mut self, model_id: &str) -> Result<()> {
        let session_id = self
            .session_id
            .clone()
            .ok_or_else(|| Error::backend("set_model sin sesión abierta"))?;
        let params = json!({"sessionId": session_id, "modelId": model_id});
        let mut obs = NoopObserver;
        self.request("session/set_model", params, &mut obs).await?;
        Ok(())
    }

    /// Resultado de un turno. `cancel_unconfirmed` marca que la cancelación
    /// fue pedida pero el backend no la confirmó: se agotó el plazo fijo y el
    /// proceso fue terminado (nunca se espera indefinidamente).
    pub async fn prompt(
        &mut self,
        text: &str,
        obs: &mut dyn TurnObserver,
        mut cancel: Option<oneshot::Receiver<()>>,
    ) -> Result<TurnCompletion> {
        let session_id = self
            .session_id
            .clone()
            .ok_or_else(|| Error::backend("prompt sin sesión abierta"))?;
        let id = self.next_request_id();
        let params = json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": text}]
        });
        self.send(
            &json!({"jsonrpc": "2.0", "id": id, "method": "session/prompt", "params": params}),
        )
        .await?;
        // A partir de aquí, las notificaciones pueden ser evidencia de ESTE
        // turno (antes, pueden ser restos de sesión/new o de un load).
        obs.on_turn_started();

        let cancel_grace = env_secs("AERIL_CANCEL_GRACE_SECS", 10);
        let mut turn_bytes: usize = 0;
        let mut cancel_sent = false;
        let mut cancel_sent_at: Option<std::time::Instant> = None;
        loop {
            let recv = self.rx.recv();
            // Tras pedir la cancelación, el plazo es fijo e independiente del
            // streaming: un backend que sigue hablando sin confirmar no
            // extiende la espera.
            let wait = match cancel_sent_at {
                None => self.turn_timeout,
                Some(t0) => cancel_grace
                    .saturating_sub(t0.elapsed())
                    .max(std::time::Duration::from_millis(1)),
            };
            tokio::select! {
                biased;
                _ = async {
                    if let Some(rx) = cancel.as_mut() {
                        let _ = rx.await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                }, if !cancel_sent => {
                    cancel_sent = true;
                    cancel_sent_at = Some(std::time::Instant::now());
                    let _ = self.send(&json!({
                        "jsonrpc": "2.0",
                        "method": "session/cancel",
                        "params": {"sessionId": session_id}
                    })).await;
                }
                msg = timeout(wait, recv) => {
                    let incoming = match msg {
                        Ok(Some(m)) => m,
                        Ok(None) => return Err(self.closed_err("el canal del backend se cerró durante el turno")),
                        Err(_) => {
                            let _ = self.kill().await;
                            if cancel_sent_at.is_some() {
                                return Ok(TurnCompletion {
                                    stop_reason: "cancelled".into(),
                                    cancel_unconfirmed: true,
                                });
                            }
                            return Err(Error::backend(format!(
                                "turno sin actividad durante {:?}; backend terminado",
                                self.turn_timeout
                            )));
                        }
                    };
                    let slot = match incoming {
                        Incoming::Closed(reason) => return Err(self.closed_err(&reason)),
                        Incoming::Frame(slot) => slot,
                    };
                    // Presupuesto acumulado del turno: el tamaño por línea ya
                    // está acotado; acá se acota el total.
                    turn_bytes += slot.size;
                    if turn_bytes > self.limits.max_turn_bytes {
                        let _ = self.kill().await;
                        return Err(Error::backend(format!(
                            "el turno excedió el máximo de entrada acumulada ({} bytes); backend terminado",
                            self.limits.max_turn_bytes
                        )));
                    }
                    let msg = match slot.decoded() {
                        Ok(m) => m,
                        Err(e) => {
                            let _ = self.kill().await;
                            return Err(e);
                        }
                    };
                    match msg {
                        ServerMsg::Response { id: rid, result } if rid == id => {
                            let v = result.map_err(|e| Error::backend(format!("prompt falló: {e}")))?;
                            let stop = v.get("stopReason").and_then(Value::as_str).unwrap_or("end_turn");
                            return Ok(TurnCompletion {
                                stop_reason: stop.to_string(),
                                cancel_unconfirmed: false,
                            });
                        }
                        ServerMsg::Response { id: rid, .. } => {
                            return Err(Error::backend(format!("respuesta inesperada durante el turno (id {rid})")));
                        }
                        ServerMsg::Notification(v) => {
                            if v["method"].as_str() == Some("session/update")
                                && let Some(update) = v.pointer("/params/update") {
                                    obs.on_update(update)?;
                                }
                        }
                        ServerMsg::Request { id, method, params } => {
                            self.dispatch_server_request(&id, &method, &params, obs).await?;
                        }
                    }
                }
            }
        }
    }

    /// PID del proceso del backend (evidencia para la reconciliación S0).
    pub fn child_pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// Bytes actualmente en vuelo (reservados en la cola cruda).
    pub fn inflight_bytes(&self) -> usize {
        self.budget
            .current
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Pico de bytes en vuelo desde el arranque del cliente (instrumentación
    /// de tests: la cota del pipeline).
    pub fn inflight_peak_bytes(&self) -> usize {
        self.budget.peak.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Presupuesto declarado de bytes en vuelo.
    pub fn inflight_budget_bytes(&self) -> usize {
        self.budget.budget
    }

    /// Termina el proceso del backend y TODO su grupo de procesos (los
    /// workers independientes no sobreviven al padre).
    pub async fn kill(&mut self) -> Result<()> {
        if let Some(pid) = self.child.id() {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
        Ok(())
    }
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        // Best-effort: sin el grupo, un worker del backend podría sobrevivir
        // al cierre inesperado del cliente.
        if let Some(pid) = self.child.id() {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
}

pub fn parse_permission(params: &Value) -> PermissionRequest {
    let tool_call = params.get("toolCall").cloned().unwrap_or(Value::Null);
    let options = params
        .get("options")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|o| PermissionOption {
                    option_id: o
                        .get("optionId")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    kind: o
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: o
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    let diff_path = tool_call
        .get("content")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter().find_map(|c| {
                if c.get("type").and_then(Value::as_str) == Some("diff") {
                    c.get("path").and_then(Value::as_str).map(str::to_string)
                } else {
                    None
                }
            })
        });
    PermissionRequest {
        request_id: params.get("requestId").cloned().unwrap_or(Value::Null),
        tool_call_id: tool_call
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        title: tool_call
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        options,
        diff_path,
    }
}

/// ¿La notificación es evidencia de procesamiento de UN turno?
/// Las notificaciones de setup (usage, comandos disponibles, info de sesión)
/// NO lo son: pueden llegar demoradas y no prueban que el prompt se procesó.
pub fn is_turn_evidence(update: &Value) -> bool {
    matches!(
        update.get("sessionUpdate").and_then(Value::as_str),
        Some(
            "agent_message_chunk"
                | "agent_thought_chunk"
                | "tool_call"
                | "tool_call_update"
                | "plan"
        )
    )
}

/// Extrae el texto de una notificación `session/update` de tipo chunk.
pub fn chunk_text(update: &Value) -> Option<(&'static str, String, Option<String>)> {
    let kind = update.get("sessionUpdate").and_then(Value::as_str)?;
    let label = match kind {
        "agent_message_chunk" => "agent_message_chunk",
        "agent_thought_chunk" => "agent_thought_chunk",
        "user_message_chunk" => "user_message_chunk",
        _ => return None,
    };
    let text = update
        .pointer("/content/text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let message_id = update
        .get("messageId")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some((label, text, message_id))
}
