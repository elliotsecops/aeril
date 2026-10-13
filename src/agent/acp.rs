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
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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

/// Observador del turno: persistencia y política viven fuera del protocolo.
pub trait TurnObserver {
    /// Notificación `session/update`.
    fn on_update(&mut self, update: &Value) -> Result<()>;
    /// Request `session/request_permission`: decide y registra.
    fn on_permission(&mut self, req: &PermissionRequest) -> PermissionDecision;
}

/// Observador vacío para llamadas de control (initialize/session/new).
pub struct NoopObserver;

impl TurnObserver for NoopObserver {
    fn on_update(&mut self, _update: &Value) -> Result<()> {
        Ok(())
    }
    fn on_permission(&mut self, _req: &PermissionRequest) -> PermissionDecision {
        PermissionDecision::Cancelled {
            policy: "control_phase_default_deny".into(),
        }
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
    Closed(String),
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
    rx: mpsc::Receiver<ServerMsg>,
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
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
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
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let mut v = tail.lock().unwrap();
                    if v.len() >= 200 {
                        v.remove(0);
                    }
                    v.push(l);
                }
            });
        }

        let (tx, rx) = mpsc::channel(1024);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let msg = match classify(&line) {
                            Ok(m) => m,
                            Err(e) => {
                                let _ = tx.send(ServerMsg::Closed(e.to_string())).await;
                                break;
                            }
                        };
                        if tx.send(msg).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        let _ = tx.send(ServerMsg::Closed("EOF del backend".into())).await;
                        break;
                    }
                    Err(e) => {
                        let _ = tx
                            .send(ServerMsg::Closed(format!("error leyendo del backend: {e}")))
                            .await;
                        break;
                    }
                }
            }
        });

        Ok(AcpClient {
            stdin,
            child,
            rx,
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
        loop {
            let msg = match timeout(self.control_timeout, self.rx.recv()).await {
                Ok(Some(m)) => m,
                Ok(None) => return Err(self.closed_err("el canal del backend se cerró")),
                Err(_) => {
                    return Err(Error::backend(format!(
                        "timeout de control esperando `{method}` ({:?})",
                        self.control_timeout
                    )));
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
                ServerMsg::Closed(reason) => return Err(self.closed_err(&reason)),
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
                let decision = obs.on_permission(&req);
                let result = match decision {
                    PermissionDecision::Selected { option_id, .. } => {
                        json!({"outcome": {"outcome": "selected", "optionId": option_id}})
                    }
                    PermissionDecision::Cancelled { .. } => {
                        json!({"outcome": {"outcome": "cancelled"}})
                    }
                };
                self.respond(id, result).await
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
    pub async fn initialize(&mut self, client_name: &str, client_version: &str) -> Result<Value> {
        let params = json!({
            "protocolVersion": 1,
            "clientCapabilities": {
                "fs": {"readTextFile": false, "writeTextFile": false},
                "terminal": false
            },
            "clientInfo": {"name": client_name, "version": client_version}
        });
        let mut obs = NoopObserver;
        self.request("initialize", params, &mut obs).await
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

    /// Envía un prompt y bombea el turno hasta la respuesta final.
    /// `cancel` permite pedir `session/cancel` desde otro thread.
    pub async fn prompt(
        &mut self,
        text: &str,
        obs: &mut dyn TurnObserver,
        mut cancel: Option<oneshot::Receiver<()>>,
    ) -> Result<String> {
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

        let mut cancel_sent = false;
        loop {
            let recv = self.rx.recv();
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
                    let _ = self.send(&json!({
                        "jsonrpc": "2.0",
                        "method": "session/cancel",
                        "params": {"sessionId": session_id}
                    })).await;
                }
                msg = timeout(self.turn_timeout, recv) => {
                    let msg = match msg {
                        Ok(Some(m)) => m,
                        Ok(None) => return Err(self.closed_err("el canal del backend se cerró durante el turno")),
                        Err(_) => {
                            let _ = self.kill().await;
                            return Err(Error::backend(format!(
                                "turno sin actividad durante {:?}; backend terminado",
                                self.turn_timeout
                            )));
                        }
                    };
                    match msg {
                        ServerMsg::Response { id: rid, result } if rid == id => {
                            let v = result.map_err(|e| Error::backend(format!("prompt falló: {e}")))?;
                            let stop = v.get("stopReason").and_then(Value::as_str).unwrap_or("end_turn");
                            return Ok(stop.to_string());
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
                        ServerMsg::Closed(reason) => return Err(self.closed_err(&reason)),
                    }
                }
            }
        }
    }

    /// Termina el proceso del backend.
    pub async fn kill(&mut self) -> Result<()> {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
        Ok(())
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
