//! Exportación sin pérdida a texto plano (invariante 5): JSONL y Markdown.

use crate::error::{Error, Result};
use crate::store::events;
use crate::store::runs::{self, RunRow};
use crate::store::{Store, threads};
use crate::timeutil::iso8601_utc;
use serde_json::json;
use std::io::Write;

pub fn jsonl(store: &Store, thread_id: &str, out: &mut impl Write) -> Result<()> {
    let thread = threads::get(&store.conn, thread_id)?
        .ok_or_else(|| Error::not_found(format!("thread {thread_id}")))?;
    writeln!(
        out,
        "{}",
        json!({
            "type": "thread",
            "id": thread.id,
            "title": thread.title,
            "repo_id": thread.repo_id,
            "repo_path": thread.repo_path,
            "state": thread.state,
            "created_ms": thread.created_ms,
            "created_at": iso8601_utc(thread.created_ms),
        })
    )?;
    for run in runs::list(&store.conn, thread_id)? {
        writeln!(
            out,
            "{}",
            json!({
                "type": "run",
                "id": run.id,
                "backend": run.backend,
                "backend_version": run.backend_version,
                "workspace_id": run.workspace_id,
                "base_oid": run.base_oid,
                "engine_session_id": run.engine_session_id,
                "state": run.state,
                "started_ms": run.started_ms,
                "ended_ms": run.ended_ms,
            })
        )?;
    }
    let mut after = -1i64;
    loop {
        let page = events::fetch_after(&store.conn, thread_id, after, 500)?;
        if page.is_empty() {
            break;
        }
        for ev in &page {
            after = ev.seq;
            writeln!(
                out,
                "{}",
                json!({
                    "type": "event",
                    "id": ev.id,
                    "seq": ev.seq,
                    "run_id": ev.run_id,
                    "kind": ev.kind,
                    "actor": ev.actor,
                    "payload_version": ev.payload_version,
                    "payload": ev.payload,
                    "created_ms": ev.created_ms,
                    "created_at": iso8601_utc(ev.created_ms),
                })
            )?;
        }
    }
    Ok(())
}

pub fn markdown(store: &Store, thread_id: &str) -> Result<String> {
    let thread = threads::get(&store.conn, thread_id)?
        .ok_or_else(|| Error::not_found(format!("thread {thread_id}")))?;
    let mut s = String::new();
    s.push_str(&format!("# Aeril — {}\n\n", thread.title));
    s.push_str(&format!(
        "- repo: `{}`\n- thread: `{}`\n- creado: {}\n\n",
        thread.repo_path,
        thread.id,
        iso8601_utc(thread.created_ms)
    ));
    for run in runs::list(&store.conn, thread_id)? {
        s.push_str(&run_section(&run));
    }
    s.push_str("## Eventos\n\n");
    let mut after = -1i64;
    loop {
        let page = events::fetch_after(&store.conn, thread_id, after, 500)?;
        if page.is_empty() {
            break;
        }
        for ev in &page {
            after = ev.seq;
            match ev.kind.as_str() {
                events::K_THREAD_CREATED => {}
                events::K_RUN_STARTED => {
                    let p: events::payload::RunStarted = ev.parse()?;
                    s.push_str(&format!(
                        "### [{:>5}] run `{}` — objetivo\n\n{}\n\n",
                        ev.seq,
                        run_short(&run_id_of(ev)),
                        p.goal
                    ));
                }
                events::K_AGENT_MESSAGE => {
                    let p: events::payload::AgentMessage = ev.parse()?;
                    s.push_str(&format!("**[{:>5}] agente:**\n\n{}\n\n", ev.seq, p.text));
                }
                events::K_AGENT_TOOL_CALL => {
                    let p: events::payload::AgentToolCall = ev.parse()?;
                    s.push_str(&format!(
                        "- [{:>5}] tool `{}` {} {}\n",
                        ev.seq,
                        p.tool_call_id,
                        p.title,
                        p.status.unwrap_or_default()
                    ));
                }
                events::K_PERMISSION_REQUESTED => {
                    let p: events::payload::PermissionRequested = ev.parse()?;
                    s.push_str(&format!(
                        "- [{:>5}] permiso pedido: {} (opciones: {})\n",
                        ev.seq,
                        p.title,
                        p.options.join(", ")
                    ));
                }
                events::K_PERMISSION_RESPONDED => {
                    let p: events::payload::PermissionResponded = ev.parse()?;
                    s.push_str(&format!(
                        "- [{:>5}] permiso respondido: {} ({})\n",
                        ev.seq, p.option_id, p.policy
                    ));
                }
                events::K_CHECKPOINT_READY => {
                    let p: events::payload::CheckpointReady = ev.parse()?;
                    s.push_str(&format!(
                        "### [{:>5}] checkpoint `{}` — {}\n\n**Archivos:**\n\n",
                        ev.seq,
                        &p.snapshot_id[..8.min(p.snapshot_id.len())],
                        if p.recovered {
                            "recuperado al reabrir"
                        } else {
                            "capturado"
                        }
                    ));
                    for f in &p.files {
                        s.push_str(&format!(
                            "- `{}` {} +{} −{}\n",
                            f.path, f.status, f.additions, f.deletions
                        ));
                    }
                    s.push_str(&format!("\n```diff\n{}```\n\n", p.patch));
                }
                events::K_CHECKPOINT_PREPARED => {
                    let p: events::payload::CheckpointPrepared = ev.parse()?;
                    s.push_str(&format!(
                        "- [{:>5}] checkpoint prepared `{}` → {}\n",
                        ev.seq,
                        &p.snapshot_id[..8.min(p.snapshot_id.len())],
                        p.expected_oid
                    ));
                }
                events::K_CHECKPOINT_FAILED => {
                    let p: events::payload::CheckpointFailed = ev.parse()?;
                    s.push_str(&format!(
                        "- [{:>5}] checkpoint FALLÓ (`{}`): {}\n",
                        ev.seq, p.reason, p.snapshot_id
                    ));
                }
                events::K_COMMENT_ADDED => {
                    let p: events::payload::CommentAdded = ev.parse()?;
                    s.push_str(&format!(
                        "> **[{:>5}] comentario** — `{}` líneas {}-{} (snapshot {}):\n> {}\n\n",
                        ev.seq,
                        p.anchor.path,
                        p.anchor.start_line,
                        p.anchor.end_line,
                        &p.anchor.snapshot_id[..8.min(p.anchor.snapshot_id.len())],
                        p.text.replace('\n', "\n> ")
                    ));
                }
                events::K_FEEDBACK_SENT => {
                    let p: events::payload::FeedbackSent = ev.parse()?;
                    s.push_str(&format!(
                        "- [{:>5}] feedback enviado ({} comentarios) en run `{}`\n",
                        ev.seq,
                        p.comment_ids.len(),
                        run_short(&p.run_id)
                    ));
                }
                events::K_FEEDBACK_DELIVERED => {
                    let p: events::payload::FeedbackDelivered = ev.parse()?;
                    s.push_str(&format!(
                        "- [{:>5}] feedback ENTREGADO al motor ({} comentarios)\n",
                        ev.seq,
                        p.comment_ids.len()
                    ));
                }
                events::K_FEEDBACK_FAILED => {
                    let p: events::payload::FeedbackFailed = ev.parse()?;
                    s.push_str(&format!(
                        "- [{:>5}] feedback NO entregado: {}\n",
                        ev.seq, p.reason
                    ));
                }
                events::K_COMMENT_RESOLVED => {
                    let p: events::payload::CommentResolved = ev.parse()?;
                    s.push_str(&format!(
                        "- [{:>5}] comentario resuelto: {}\n",
                        ev.seq, p.comment_id
                    ));
                }
                events::K_RUN_ENDED => {
                    let p: events::payload::RunEnded = ev.parse()?;
                    s.push_str(&format!(
                        "### [{:>5}] run `{}` terminó: {}{}\n\n",
                        ev.seq,
                        run_short(&run_id_of(ev)),
                        p.state,
                        p.reason.map(|r| format!(" ({r})")).unwrap_or_default()
                    ));
                }
                events::K_WORKSPACE_RECREATED => {
                    let p: events::payload::WorkspaceRecreated = ev.parse()?;
                    s.push_str(&format!(
                        "- [{:>5}] workspace recreado ({}) desde {}\n",
                        ev.seq, p.reason, p.from_oid
                    ));
                }
                events::K_INTEGRATION_COMPLETED => {
                    let p: events::payload::IntegrationCompleted = ev.parse()?;
                    s.push_str(&format!(
                        "- [{:>5}] integrado fast-forward a {}: {} → {}\n",
                        ev.seq, p.target_branch, p.target_oid_before, p.target_oid_after
                    ));
                }
                events::K_INTEGRATION_BLOCKED => {
                    let p: events::payload::IntegrationBlocked = ev.parse()?;
                    s.push_str(&format!(
                        "- [{:>5}] integración BLOQUEADA a {}: {}\n",
                        ev.seq, p.target_branch, p.reason
                    ));
                }
                events::K_VALIDATION_RECORDED => {
                    let p: events::payload::ValidationRecorded = ev.parse()?;
                    s.push_str(&format!(
                        "- [{:>5}] validación `{}` → exit {} ({} ms)\n",
                        ev.seq, p.command, p.exit_code, p.duration_ms
                    ));
                }
                events::K_ENGINE_SESSION => {
                    let p: events::payload::EngineSession = ev.parse()?;
                    s.push_str(&format!(
                        "- [{:>5}] sesión del motor {}: {}\n",
                        ev.seq,
                        if p.resumed { "reanudada" } else { "nueva" },
                        p.engine_session_id
                    ));
                }
                other => {
                    s.push_str(&format!("- [{:>5}] {}\n", ev.seq, other));
                }
            }
        }
    }
    Ok(s)
}

fn run_section(run: &RunRow) -> String {
    format!(
        "## Run `{}`\n\n- backend: {} ({})\n- base OID: {}\n- workspace: {}\n- estado: {}\n- sesión del motor: {}\n\n",
        &run.id[..8.min(run.id.len())],
        run.backend,
        run.backend_version,
        run.base_oid.clone().unwrap_or_default(),
        run.workspace_id.clone().unwrap_or_default(),
        run.state,
        run.engine_session_id.clone().unwrap_or_default(),
    )
}

fn run_id_of(ev: &events::EventRecord) -> String {
    ev.run_id.clone().unwrap_or_default()
}

fn run_short(id: &str) -> String {
    id[..8.min(id.len())].to_string()
}
