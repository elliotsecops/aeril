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
                format!("/tmp/aeril-bench/repo-{t:03}"),
                now_base
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
         -> Result<String> {
            let id = new_id();
            tx.execute(
                "INSERT INTO events (id, thread_id, seq, run_id, kind, actor, payload_version, payload, created_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8)",
                params![id, thread_id, seq, run_id, kind, actor, payload.to_string(), now_base + seq * 1000],
            )?;
            seq += 1;
            Ok(id)
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
        let mut ready_events: Vec<(String, String, String)> = Vec::new(); // (event_id, snapshot_id, oid)
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
            let snap_id = new_id();
            let oid = format!("{:040x}", (t * 31 + d) % 0xffff_ffff);
            let ev_id = push(
                &tx,
                Some(&run1),
                events::K_CHECKPOINT_READY,
                "system",
                json!({
                    "snapshot_id": snap_id,
                    "git_ref": format!("refs/aeril/checkpoints/{}", new_id()),
                    "oid": oid,
                    "base_oid": "0".repeat(40),
                    "patch": patch,
                    "files": [serde_json::to_value(&file).unwrap_or(serde_json::Value::Null)],
                    "recovered": false
                }),
            )?;
            ready_events.push((ev_id, snap_id, oid));
        }

        // comentarios enlazados a los checkpoints (la vista de revisión los carga)
        let comments_n: u64 = 10.min(ready_events.len() as u64);
        for c in 0..comments_n {
            let (ev_id, snap_id, oid) = &ready_events[(c as usize) % ready_events.len()];
            push(
                &tx,
                None,
                events::K_COMMENT_ADDED,
                "human",
                json!({
                    "checkpoint_event_id": ev_id,
                    "oid": oid,
                    "anchor": {
                        "snapshot_id": snap_id,
                        "path": format!("src/module_{}.rs", c % 12),
                        "side": "new",
                        "start_line": 10 + c,
                        "end_line": 12 + c,
                        "before_blob_oid": format!("{:040x}", (t * 17 + c) % 0xffff_ffff),
                        "after_blob_oid": format!("{:040x}", (t * 29 + c + 7) % 0xffff_ffff),
                        "context_hash": format!("{:016x}", (t + c) * 7919)
                    },
                    "text": format!("Comentario sintético {c}: revisar el manejo de errores y agregar validación de entrada en esta sección.")
                }),
            )?;
        }

        // conversación sintética: mensajes, tool calls, permisos, comentarios y feedback
        let mut remaining = events_per_thread
            .saturating_sub(2)
            .saturating_sub(diffs_per_thread)
            .saturating_sub(comments_n)
            .saturating_sub(2);
        let mut i = 0u64;
        while remaining > 0 {
            match i % 10 {
                0..=4 => {
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
    p.push_str("-    Ok(recibo.legacy())\n+    Ok(recibo.validado())\n");
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

// ── medición de escala (runs concurrentes con agentes mock) ──────────────────

pub struct ScaleReport {
    pub runs: u64,
    pub failures: u64,
    pub app_rss_kb: u64,
    pub agents_rss_kb: u64,
    pub total_rss_kb: u64,
    pub gate_app_mb: u64,
    pub app_ok: bool,
    pub note: String,
}

fn read_rss_kb(pid: i64) -> u64 {
    let path = if pid <= 0 {
        "/proc/self/status".to_string()
    } else {
        format!("/proc/{pid}/status")
    };
    let Ok(content) = std::fs::read_to_string(path) else {
        return 0;
    };
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        }
    }
    0
}

/// Suma RSS de los hijos directos del proceso (los agentes mock), escaneando
/// /proc/<pid>/stat por PPID (portable: no depende de CONFIG_PROC_CHILDREN).
fn children_rss_kb() -> u64 {
    let me = std::process::id() as i64;
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i64>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // "pid (comm) state ppid ..." — comm puede tener espacios: usar el
        // último ')' como ancla.
        let Some(after_comm) = stat.rsplit_once(')') else {
            continue;
        };
        let fields: Vec<&str> = after_comm.1.split_whitespace().collect();
        if fields.len() >= 2 && fields[1].parse::<i64>().ok() == Some(me) {
            total += read_rss_kb(pid);
        }
    }
    total
}

fn ensure_scale_repo(path: &Path) -> Result<()> {
    if path.join(".git").exists() {
        return Ok(());
    }
    std::fs::create_dir_all(path)?;
    let repo = git2::Repository::init(path)?;
    std::fs::write(path.join("NOTES.md"), "# Notas de escala\n\nbase\n")?;
    let mut index = repo.index()?;
    index.add_path(Path::new("NOTES.md"))?;
    index.write()?;
    let tree_oid = index.write_tree()?;
    let tree = repo.find_tree(tree_oid)?;
    let sig = git2::Signature::now("Aeril", "aeril@localhost")?;
    repo.commit(Some("HEAD"), &sig, &sig, "base de escala", &tree, &[])?;
    Ok(())
}

/// Corre `runs` runs concurrentes con el backend MOCK dentro de UN proceso de
/// Aeril y mide: RSS de la aplicación (gate ≤ 500 MB) y RSS de los agentes
/// (informativo, medido APARTE). No prueba el consumo de Hermes real: los
/// procesos de Hermes comparten perfil (`~/.hermes`) y no es medible en v0.
pub fn scale(data_dir: &Path, runs: u64, delay_ms: u64) -> Result<ScaleReport> {
    use crate::session::{PermissionPolicySetting, RunOptions, Runner};

    std::fs::create_dir_all(data_dir)?;
    let db_path = data_dir.join("aeril.db");
    let repo_path = data_dir.join("scale-repo");
    ensure_scale_repo(&repo_path)?;

    let mut store = Store::open(&db_path)?;
    let repo = crate::git::open(&repo_path)?;
    let info = crate::git::info(&repo)?;

    let mut jobs = Vec::new();
    for i in 0..runs {
        let thread = crate::store::threads::create(
            &mut store.conn,
            &format!("Escala {i}"),
            &repo_path.to_string_lossy(),
            &info.repo_id,
        )?;
        let script_path = data_dir.join(format!("scale-script-{i}.json"));
        std::fs::write(
            &script_path,
            json!({
                "message": format!("run de escala {i}"),
                "final_message": "ok",
                "delay_ms": delay_ms,
                "edits": [{"op": "write", "path": "NOTES.md", "content": format!("run {i}\n")}],
            })
            .to_string(),
        )?;
        jobs.push((thread.id, script_path));
    }

    let mut handles = Vec::new();
    for (tid, script_path) in jobs {
        let db = db_path.clone();
        let dd = data_dir.to_path_buf();
        handles.push(std::thread::spawn(move || {
            let mut store = match Store::open(&db) {
                Ok(s) => s,
                Err(e) => return Err(e),
            };
            let mut runner = Runner {
                data_dir: dd,
                db_path: db,
                ui: None,
                cancel: None,
            };
            let opts = RunOptions {
                backend: format!("mock:{}", script_path.display()),
                goal: "carga de escala".into(),
                permissions: PermissionPolicySetting::Auto,
                feedback_comment_ids: None,
                resume_session: false,
                model: None,
            };
            match runner.start(&mut store, &tid, &opts) {
                Ok(summary) if summary.state == "completed" => Ok(()),
                Ok(summary) => Err(crate::error::Error::Other(format!(
                    "run de escala no completado: {} ({:?})",
                    summary.state, summary.detail
                ))),
                Err(e) => Err(e),
            }
        }));
        // Escalonado: crea workspaces sin pelear por locks internos de git.
        std::thread::sleep(std::time::Duration::from_millis(300));
    }

    // Muestra durante la fase de agentes (todos en su delay).
    std::thread::sleep(std::time::Duration::from_millis(delay_ms / 2));
    let app_rss_kb = read_rss_kb(-1);
    let agents_rss_kb = children_rss_kb();

    let mut failures = 0u64;
    for h in handles {
        match h.join() {
            Ok(Ok(())) => {}
            _ => failures += 1,
        }
    }

    let gate_app_mb = 500u64;
    let app_ok = app_rss_kb / 1024 <= gate_app_mb;
    Ok(ScaleReport {
        runs,
        failures,
        app_rss_kb,
        agents_rss_kb,
        total_rss_kb: app_rss_kb + agents_rss_kb,
        gate_app_mb,
        app_ok,
        note: "Agentes MOCK: caracteriza el overhead de la aplicación; NO prueba el consumo de Hermes real (perfil compartido, no medible en v0)".into(),
    })
}
