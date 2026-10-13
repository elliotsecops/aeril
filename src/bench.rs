//! Fixtures sintéticas y helpers de medición del arranque.
//!
//! Perfil de referencia (v0.4): 100 conversaciones, 1.000 eventos por
//! conversación, 10 diffs por conversación.

use crate::error::Result;
use crate::ids::new_id;
use crate::store::{Store, events};
use crate::timeutil::now_ms;
use rusqlite::params;
use serde_json::json;
use std::path::Path;
use std::time::Instant;

pub struct SeedReport {
    pub threads: u64,
    pub events: u64,
    pub elapsed_ms: u64,
    pub db_bytes: u64,
}

pub fn seed(
    dir: &Path,
    threads_n: u64,
    events_per_thread: u64,
    diffs_per_thread: u64,
) -> Result<SeedReport> {
    std::fs::create_dir_all(dir)?;
    let db_path = dir.join("aeril.db");
    let mut store = Store::open(&db_path)?;
    let started = Instant::now();
    let mut total_events = 0u64;
    for t in 0..threads_n {
        let thread_id = new_id();
        let now_base = now_ms() - (t as i64) * 3_600_000;
        let run1 = new_id();
        let run2 = new_id();
        let tx = store
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO threads (id, title, repo_id, repo_path, state, next_seq, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, 'open', 0, ?5, ?5)",
            params![
                thread_id,
                format!("Conversación sintética {t:03}: refactor del módulo de pagos + revisión de seguridad"),
                format!("repoid{t:04}"),
                format!("/tmp/aeril-bench/repo-{t:03}")
            ],
        )?;
        for (run_id, state) in [(&run1, "completed"), (&run2, "completed")] {
            tx.execute(
                "INSERT INTO runs (id, thread_id, workspace_id, repo_path, worktree_path, branch, base_oid,
                                   backend, backend_version, state, started_ms, ended_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'hermes', '0.x', ?8, ?9, ?10)",
                params![
                    run_id,
                    thread_id,
                    format!("ws{t:04}{}", &run_id[..4]),
                    format!("/tmp/aeril-bench/repo-{t:03}"),
                    format!("/tmp/aeril-bench/wt-{t:03}-{}", &run_id[..4]),
                    format!("aeril/ws/{}", &run_id[..8]),
                    "0".repeat(40),
                    state,
                    now_base,
                    now_base + 60_000
                ],
            )?;
        }

        let mut seq = 0i64;
        let mut push = |tx: &rusqlite::Transaction<'_>,
                        run_id: Option<&str>,
                        kind: &str,
                        actor: &str,
                        payload: serde_json::Value|
         -> Result<()> {
            tx.execute(
                "INSERT INTO events (id, thread_id, seq, run_id, kind, actor, payload_version, payload, created_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8)",
                params![new_id(), thread_id, seq, run_id, kind, actor, payload.to_string(), now_base + seq * 1000],
            )?;
            seq += 1;
            Ok(())
        };

        push(
            &tx,
            None,
            events::K_THREAD_CREATED,
            "system",
            json!({"title": "Conversación sintética", "repo_path": format!("/tmp/aeril-bench/repo-{t:03}"), "repo_id": format!("repoid{t:04}")}),
        )?;
        push(
            &tx,
            Some(&run1),
            events::K_RUN_STARTED,
            "system",
            json!({"backend": "hermes", "goal": "Refactorizá el módulo de pagos y agregá validaciones", "workspace_id": format!("ws{t:04}"), "repo_path": format!("/tmp/aeril-bench/repo-{t:03}"), "worktree_path": format!("/tmp/aeril-bench/wt-{t:03}"), "branch": format!("aeril/ws/{run1}"), "base_oid": "0".repeat(40)}),
        )?;

        // diffs: checkpoint.ready con patch y archivos
        for d in 0..diffs_per_thread {
            let patch = fake_patch(t, d);
            let file = events::payload::FileSummary {
                path: format!("src/module_{d}.rs"),
                status: "modified".into(),
                additions: 12 + d as usize,
                deletions: 3 + d as usize,
                before_blob_oid: Some(format!("{:040x}", (t * 17 + d) % 0xffff_ffff)),
                after_blob_oid: Some(format!("{:040x}", (t * 29 + d + 7) % 0xffff_ffff)),
            };
            push(
                &tx,
                Some(&run1),
                events::K_CHECKPOINT_READY,
                "system",
                json!({
                    "snapshot_id": new_id(),
                    "git_ref": format!("refs/aeril/checkpoints/{}", new_id()),
                    "oid": format!("{:040x}", (t * 31 + d) % 0xffff_ffff),
                    "base_oid": "0".repeat(40),
                    "patch": patch,
                    "files": [serde_json::to_value(&file).unwrap_or(serde_json::Value::Null)],
                    "recovered": false
                }),
            )?;
        }

        // conversación sintética: mensajes, tool calls, permisos, comentarios y feedback
        let mut remaining = events_per_thread
            .saturating_sub(2)
            .saturating_sub(diffs_per_thread)
            .saturating_sub(2);
        let mut i = 0u64;
        while remaining > 0 {
            match i % 10 {
                0 | 1 | 2 | 3 | 4 => {
                    push(
                        &tx,
                        Some(if i % 20 < 10 { &run1 } else { &run2 }),
                        events::K_AGENT_MESSAGE,
                        "agent",
                        json!({"text": format!(
                            "Analicé el archivo src/module_{}.rs: encontré {} referencias al servicio de pagos y propongo unificar el manejo de errores con un tipo Result común. Detalle del cambio: {}\n",
                            i % 12, i % 7 + 1,
                            "línea reemplazada por una versión con validación de entrada y mejor mensaje de error; ".repeat((i % 4 + 1) as usize)
                        ), "message_id": new_id()}),
                    )?;
                }
                5 | 6 => {
                    push(
                        &tx,
                        Some(&run1),
                        events::K_AGENT_TOOL_CALL,
                        "agent",
                        json!({"tool_call_id": format!("tc-{:012x}", i), "title": format!("write_file: src/module_{}.rs", i % 12), "kind": "edit", "status": "completed"}),
                    )?;
                }
                7 => {
                    push(
                        &tx,
                        Some(&run1),
                        events::K_PERMISSION_REQUESTED,
                        "agent",
                        json!({"request_id": format!("perm-check-{i}"), "tool_call_id": format!("perm-check-{i}"), "title": format!("command: cargo test -p module_{}", i % 12), "options": ["allow_once", "allow_session", "deny"], "diff_path": null}),
                    )?;
                }
                8 => {
                    push(
                        &tx,
                        Some(&run1),
                        events::K_PERMISSION_RESPONDED,
                        "system",
                        json!({"request_id": format!("perm-check-{i}"), "option_id": "allow_once", "policy": "auto_allow_once"}),
                    )?;
                }
                _ => {
                    push(
                        &tx,
                        Some(&run2),
                        events::K_AGENT_MESSAGE,
                        "agent",
                        json!({"text": format!("Continuando con el feedback del revisor: aplicado el comentario sobre el manejo de errores en el módulo {}.", i % 12), "message_id": new_id()}),
                    )?;
                }
            }
            i += 1;
            remaining -= 1;
        }
        push(
            &tx,
            Some(&run1),
            events::K_RUN_ENDED,
            "system",
            json!({"state": "completed", "reason": null, "stop_reason": "end_turn"}),
        )?;
        push(
            &tx,
            Some(&run2),
            events::K_RUN_ENDED,
            "system",
            json!({"state": "completed", "reason": null, "stop_reason": "end_turn"}),
        )?;
        tx.execute(
            "UPDATE threads SET next_seq = ?2, updated_ms = ?3 WHERE id = ?1",
            params![thread_id, seq, now_base + seq * 1000],
        )?;
        tx.commit()?;
        total_events += seq as u64;
    }
    let db_bytes = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
    Ok(SeedReport {
        threads: threads_n,
        events: total_events,
        elapsed_ms: started.elapsed().as_millis() as u64,
        db_bytes,
    })
}

fn fake_patch(t: u64, d: u64) -> String {
    let mut p = format!(
        "diff --git a/src/module_{d}.rs b/src/module_{d}.rs\n--- a/src/module_{d}.rs\n+++ b/src/module_{d}.rs\n@@ -{},14 +{},16 @@ pub fn procesar_pago(monto: Decimal) -> Result<Recibo> {{\n",
        10 + d,
        10 + d
    );
    for l in 0..12 {
        p.push_str(&format!(
            "-    let valor_{l} = calcular_{l}(monto, {t}, {d});\n"
        ));
        p.push_str(&format!(
            "+    let valor_{l} = calcular_{l}(monto, {t}, {d})?; // validado\n"
        ));
    }
    p.push_str("     let recibo = emitir(&valor_0, &valor_1, &valor_2)?;\n");
    p.push_str(&format!(
        "-    Ok(recibo.legacy())\n+    Ok(recibo.validado())\n"
    ));
    p
}

/// Escribe una línea de beacon de arranque si `AERIL_BENCH_BEACON` está seteado.
/// `t0` es el `Instant` capturado al inicio de `main`.
pub fn beacon(stage: &str, t0: Instant) {
    let Ok(path) = std::env::var("AERIL_BENCH_BEACON") else {
        return;
    };
    let line = json!({
        "stage": stage,
        "internal_us": t0.elapsed().as_micros() as u64,
        "unix_ms": now_ms(),
    })
    .to_string();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{line}");
    }
}
