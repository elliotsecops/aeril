//! Backend ACP simulado (`aeril __mock-agent`): transporte determinista para
//! tests. Habla el mismo subconjunto de ACP que Hermes (ndjson, JSON-RPC 2.0),
//! pide permisos como un agente real y aplica ediciones al cwd de la sesión.
//! NO es un agente: ejecuta un guion fijo.

use crate::error::{Error, Result};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

#[derive(serde::Deserialize, Default)]
struct MockScript {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    final_message: Option<String>,
    #[serde(default)]
    delay_ms: u64,
    #[serde(default)]
    edits: Vec<MockEdit>,
    /// Emite `usage_update` + `available_commands_update` (notificaciones de
    /// setup, NO evidencia de procesamiento del turno) tras session/new|load
    /// y al inicio de cada turno.
    #[serde(default)]
    notify_setup: bool,
    /// `session/load` responde error (fallback del cliente a sesión nueva).
    #[serde(default)]
    fail_load: bool,
    /// El proceso muere a mitad del turno (conexión perdida) tras N ms.
    #[serde(default)]
    exit_after_ms: u64,
    /// Ignora `session/cancel` (backend que no coopera; B6).
    #[serde(default)]
    ignore_cancel: bool,
}

#[derive(serde::Deserialize)]
struct MockEdit {
    op: String,
    path: String,
    #[serde(default)]
    content: String,
}

fn send(v: &Value) -> Result<()> {
    let mut out = std::io::stdout().lock();
    let line = serde_json::to_string(v)?;
    writeln!(out, "{line}")?;
    out.flush()?;
    Ok(())
}

fn safe_join(cwd: &Path, rel: &str) -> Result<PathBuf> {
    if rel.is_empty() || Path::new(rel).is_absolute() || rel.split('/').any(|c| c == "..") {
        return Err(Error::invalid(format!(
            "path inseguro en el guion mock: {rel}"
        )));
    }
    Ok(cwd.join(rel))
}

#[allow(unused_assignments)]
pub fn run(script_path: &str) -> Result<()> {
    let raw = std::fs::read_to_string(script_path)?;
    let script: MockScript = serde_json::from_str(&raw)?;

    let (tx, rx): (Sender<String>, Receiver<String>) = channel();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(l) => {
                    if tx.send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut session_id = String::new();
    let mut cwd = std::env::current_dir()?;
    let mut cancelled = false;

    while let Ok(line) = rx.recv() {
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let method = v
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let id = v.get("id").cloned();
        let params = v.get("params").cloned().unwrap_or(Value::Null);

        match (method.as_str(), id) {
            ("initialize", Some(id)) => {
                send(&json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {
                        "protocolVersion": 1,
                        "agentInfo": {"name": "aeril-mock", "version": "0"},
                        "agentCapabilities": {"loadSession": true},
                        "authMethods": []
                    }
                }))?;
            }
            ("session/new", Some(id)) => {
                session_id = crate::ids::new_id();
                if let Some(c) = params.get("cwd").and_then(Value::as_str) {
                    cwd = PathBuf::from(c);
                }
                send(&json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {
                        "sessionId": session_id,
                        "models": {"availableModels": [], "currentModelId": "mock"},
                        "modes": {"currentModeId": "default", "availableModes": []}
                    }
                }))?;
                if script.notify_setup {
                    setup_notifications()?;
                }
            }
            ("session/load", Some(id)) => {
                if script.fail_load {
                    send(&json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": -32603, "message": "load de sesión no disponible (mock)"}
                    }))?;
                    continue;
                }
                if let Some(c) = params.get("cwd").and_then(Value::as_str) {
                    cwd = PathBuf::from(c);
                }
                if let Some(s) = params.get("sessionId").and_then(Value::as_str) {
                    session_id = s.to_string();
                }
                send(&json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {
                        "models": {"availableModels": [], "currentModelId": "mock"},
                        "modes": {"currentModeId": "default", "availableModes": []}
                    }
                }))?;
                if script.notify_setup {
                    setup_notifications()?;
                }
            }
            ("session/prompt", Some(id)) => {
                if params.get("sessionId").and_then(Value::as_str) != Some(session_id.as_str()) {
                    send(
                        &json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "refusal"}}),
                    )?;
                    continue;
                }
                cancelled = false;
                let stop = run_turn(&script, &cwd, &session_id, &rx, &mut cancelled)?;
                send(&json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": stop}}))?;
            }
            ("session/cancel", None) => {
                cancelled = true;
            }
            (_, Some(id)) => {
                send(&json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": format!("método no soportado por el mock: {method}")}
                }))?;
            }
            (_, None) => {}
        }
    }
    Ok(())
}

fn run_turn(
    script: &MockScript,
    cwd: &Path,
    session_id: &str,
    rx: &Receiver<String>,
    cancelled: &mut bool,
) -> Result<String> {
    if script.notify_setup {
        setup_notifications()?;
    }
    if script.exit_after_ms > 0 {
        // Conexión perdida a mitad del turno, sin contenido real antes.
        if sleep_polling(script.exit_after_ms, rx, cancelled, true) {
            return Ok("cancelled".to_string());
        }
        std::process::exit(9);
    }
    if let Some(msg) = &script.message {
        chunk_out(msg)?;
    }
    if sleep_polling(script.delay_ms, rx, cancelled, script.ignore_cancel) {
        return Ok("cancelled".to_string());
    }

    for (i, edit) in script.edits.iter().enumerate() {
        let tc_id = format!("tc-{:012x}", i + 1);
        send(&json!({
            "jsonrpc": "2.0", "method": "session/update",
            "params": {"sessionId": session_id, "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": tc_id,
                "title": format!("write_file: {}", edit.path),
                "kind": "edit",
                "content": [{"type": "content", "content": {"type": "text", "text": format!("preparando edit de {}", edit.path)}}],
                "locations": [{"path": edit.path}]
            }}
        }))?;

        let old_text = std::fs::read_to_string(safe_join(cwd, &edit.path)?).ok();
        let req_id = 1000u64 + i as u64;
        send(&json!({
            "jsonrpc": "2.0", "id": req_id, "method": "session/request_permission",
            "params": {
                "sessionId": session_id,
                "toolCall": {
                    "toolCallId": tc_id,
                    "title": format!("Approve edit: {}", edit.path),
                    "kind": "edit",
                    "status": "pending",
                    "content": [{"type": "diff", "path": edit.path, "newText": edit.content, "oldText": old_text}]
                },
                "options": [
                    {"optionId": "allow_once", "kind": "allow_once", "name": "Allow edit"},
                    {"optionId": "deny", "kind": "reject_once", "name": "Deny"}
                ]
            }
        }))?;

        // Espera la respuesta del permiso, escuchando cancel.
        let deadline = Instant::now() + Duration::from_secs(60);
        let allowed = loop {
            if Instant::now() > deadline {
                break false;
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => {
                    let Ok(v) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if v.get("method").and_then(Value::as_str) == Some("session/cancel") {
                        if !script.ignore_cancel {
                            *cancelled = true;
                        }
                        continue;
                    }
                    if v.get("id").and_then(Value::as_u64) == Some(req_id) {
                        let option = v
                            .pointer("/result/outcome/optionId")
                            .and_then(Value::as_str)
                            .unwrap_or("deny");
                        break option.starts_with("allow");
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if *cancelled {
                        break false;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break false,
            }
        };

        if *cancelled {
            send(&json!({
                "jsonrpc": "2.0", "method": "session/update",
                "params": {"sessionId": session_id, "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": tc_id, "kind": "edit", "status": "failed",
                    "content": [{"type": "content", "content": {"type": "text", "text": "cancelado"}}]
                }}
            }))?;
            return Ok("cancelled".to_string());
        }

        if allowed {
            match edit.op.as_str() {
                "write" => {
                    let path = safe_join(cwd, &edit.path)?;
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&path, &edit.content)?;
                }
                "append" => {
                    let path = safe_join(cwd, &edit.path)?;
                    let mut f = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)?;
                    f.write_all(edit.content.as_bytes())?;
                }
                other => {
                    return Err(Error::invalid(format!(
                        "operación mock desconocida: {other}"
                    )));
                }
            }
            send(&json!({
                "jsonrpc": "2.0", "method": "session/update",
                "params": {"sessionId": session_id, "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": tc_id, "kind": "edit", "status": "completed",
                    "content": [{"type": "content", "content": {"type": "text", "text": format!("ok: {}", edit.path)}}]
                }}
            }))?;
        } else {
            send(&json!({
                "jsonrpc": "2.0", "method": "session/update",
                "params": {"sessionId": session_id, "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": tc_id, "kind": "edit", "status": "failed",
                    "content": [{"type": "content", "content": {"type": "text", "text": "denegado"}}]
                }}
            }))?;
        }
    }
    if let Some(fm) = &script.final_message {
        chunk_out(fm)?;
    }
    Ok("end_turn".to_string())
}

fn chunk_out(text: &str) -> Result<()> {
    send(&json!({
        "jsonrpc": "2.0", "method": "session/update",
        "params": {"update": {
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": text},
            "messageId": crate::ids::new_id()
        }}
    }))
}

/// Duerme `ms` en tramos cortos, detectando `session/cancel` en el canal.
/// Devuelve `true` si se canceló.
/// Notificaciones de setup (usage/commands): NO son evidencia de un turno.
fn setup_notifications() -> Result<()> {
    send(&json!({
        "jsonrpc": "2.0", "method": "session/update",
        "params": {"update": {"sessionUpdate": "usage_update", "size": 200000, "used": 1234}}
    }))?;
    send(&json!({
        "jsonrpc": "2.0", "method": "session/update",
        "params": {"update": {"sessionUpdate": "available_commands_update", "availableCommands": []}}
    }))
}

fn sleep_polling(
    ms: u64,
    rx: &Receiver<String>,
    cancelled: &mut bool,
    ignore_cancel: bool,
) -> bool {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(line) => {
                if !ignore_cancel
                    && let Ok(v) = serde_json::from_str::<Value>(&line)
                    && v.get("method").and_then(Value::as_str) == Some("session/cancel")
                {
                    *cancelled = true;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return *cancelled && !ignore_cancel,
        }
        if *cancelled && !ignore_cancel {
            return true;
        }
    }
    *cancelled && !ignore_cancel
}
