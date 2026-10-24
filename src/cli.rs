//! CLI de Aeril: subcomandos para el flujo completo sin TUI (tests y scripts).

use crate::agent;
use crate::bench;
use crate::checkpoint;
use crate::error::{Error, Result};
use crate::export;
use crate::git;
use crate::integration;
use crate::review;
use crate::session::{self, PermissionPolicySetting, RunOptions, Runner};
use crate::store::events::{self, payload};
use crate::store::{Store, runs, threads};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

struct Parsed {
    flags: HashMap<String, String>,
    has: HashSet<String>,
    pos: Vec<String>,
    after_dashdash: Vec<String>,
}

fn parse(args: &[String]) -> Parsed {
    let mut p = Parsed {
        flags: HashMap::new(),
        has: HashSet::new(),
        pos: Vec::new(),
        after_dashdash: Vec::new(),
    };
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        if a == "--" {
            p.after_dashdash.extend(it.cloned());
            break;
        }
        if let Some(key) = a.strip_prefix("--") {
            p.has.insert(key.to_string());
            if let Some(next) = it.peek()
                && !next.starts_with("--")
            {
                p.flags.insert(key.to_string(), (*next).clone());
                it.next();
            }
        } else {
            p.pos.push(a.clone());
        }
    }
    p
}

impl Parsed {
    fn flag(&self, name: &str) -> Option<String> {
        self.flags.get(name).cloned()
    }
    fn require(&self, name: &str) -> Result<String> {
        self.flag(name)
            .ok_or_else(|| Error::invalid(format!("falta --{name}")))
    }
    fn yes(&self, name: &str) -> bool {
        self.has.contains(name)
    }
}

fn default_data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("AERIL_DATA_DIR")
        && !d.is_empty()
    {
        return PathBuf::from(d);
    }
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".local/share")
        });
    base.join("aeril")
}

fn print_json(v: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

/// Cancelación programada para E2E/demos: pide `session/cancel` al motor tras
/// N ms de iniciado el run (`--cancel-after-ms`).
fn cancel_receiver(p: &Parsed) -> Result<Option<tokio::sync::oneshot::Receiver<()>>> {
    match p.flag("cancel-after-ms") {
        Some(ms_str) => {
            let ms: u64 = ms_str
                .parse()
                .map_err(|_| Error::invalid("--cancel-after-ms inválido"))?;
            let (tx, rx) = tokio::sync::oneshot::channel();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(ms));
                let _ = tx.send(());
            });
            Ok(Some(rx))
        }
        None => Ok(None),
    }
}

pub fn run() -> i32 {
    let t0 = Instant::now();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(&args, t0) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("aeril: error: {e}");
            1
        }
    }
}

fn dispatch(args: &[String], t0: Instant) -> Result<i32> {
    let p = parse(args);
    let data_dir = p
        .flag("data-dir")
        .map(PathBuf::from)
        .unwrap_or_else(default_data_dir);
    // Los procesos hijos (agentes) corren con cwd del worktree: todas las
    // rutas que Aeril les pasa deben ser ABSOLUTAS.
    let _ = std::fs::create_dir_all(&data_dir);
    let data_dir = data_dir.canonicalize().unwrap_or(data_dir);
    let db_path = data_dir.join("aeril.db");

    match p.pos.first().map(String::as_str) {
        None => {
            let backend = p
                .flag("backend")
                .or_else(|| std::env::var("AERIL_BACKEND").ok())
                .unwrap_or_else(|| "hermes".into());
            crate::tui::run(data_dir, db_path, backend, t0)?;
            Ok(0)
        }
        Some("version") => {
            println!("aeril {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        Some("thread") => cmd_thread(&p, &db_path),
        Some("run") => cmd_run(&p, &data_dir, &db_path),
        Some("checkpoint") => cmd_checkpoint(&p, &db_path),
        Some("comment") => cmd_comment(&p, &db_path),
        Some("feedback") => cmd_feedback(&p, &data_dir, &db_path),
        Some("recover") => cmd_recover(&p, &db_path),
        Some("lease") => cmd_lease(&p, &db_path),
        Some("worktree") => cmd_worktree(&p, &data_dir, &db_path),
        Some("workspace") => cmd_workspace(&p, &data_dir, &db_path),
        Some("profile") => cmd_profile(&p, &data_dir, &db_path),
        Some("integrate") => cmd_integrate(&p, &db_path),
        Some("validate") => cmd_validate(&p, &data_dir, &db_path),
        Some("export") => cmd_export(&p, &db_path),
        Some("bench") => cmd_bench(&p, &data_dir, &db_path),
        Some("__mock-agent") => {
            let script = p.require("script")?;
            agent::mock::run(&script)?;
            Ok(0)
        }
        Some(other) => Err(Error::invalid(format!(
            "subcomando desconocido: {other} (probá sin argumentos para la TUI)"
        ))),
    }
}

fn open_store(db_path: &Path) -> Result<Store> {
    Store::open(db_path)
}

// ── thread ───────────────────────────────────────────────────────────────────

fn cmd_thread(p: &Parsed, db_path: &Path) -> Result<i32> {
    match p.pos.get(1).map(String::as_str) {
        Some("new") => {
            let repo_path = p.require("repo")?;
            let title = p
                .flag("title")
                .unwrap_or_else(|| "Thread sin título".into());
            let repo = git::open(Path::new(&repo_path))?;
            let info = git::info(&repo)?;
            let mut store = open_store(db_path)?;
            let row = threads::create(
                &mut store.conn,
                &title,
                &info.path.to_string_lossy(),
                &info.repo_id,
            )?;
            print_json(&json!({
                "thread_id": row.id,
                "repo_id": info.repo_id,
                "repo_path": info.path.to_string_lossy(),
                "head_oid": info.head_oid,
                "title": row.title,
            }))?;
            Ok(0)
        }
        Some("list") => {
            let store = open_store(db_path)?;
            let list = threads::list_projection(&store.conn, 500)?;
            let arr: Vec<Value> = list
                .iter()
                .map(|t| {
                    json!({
                        "thread_id": t.id,
                        "title": t.title,
                        "repo_path": t.repo_path,
                        "state": t.state,
                        "updated_ms": t.updated_ms,
                    })
                })
                .collect();
            print_json(&Value::Array(arr))?;
            Ok(0)
        }
        other => Err(Error::invalid(format!("thread {other:?} no soportado"))),
    }
}

// ── run ──────────────────────────────────────────────────────────────────────

fn run_view(store: &Store, run: &runs::RunRow) -> Result<Value> {
    use crate::recovery::{LeaseView, LivenessView};
    let supervisor = crate::recovery::supervisor_view(&store.conn, run)?;
    let lease = crate::recovery::lease_view(&store.conn, run)?;
    let status = crate::recovery::effective_status(run, &supervisor);
    let sup_status = match supervisor.liveness {
        LivenessView::Alive => "alive",
        LivenessView::Dead => "dead",
        LivenessView::Unknown => "unknown",
    };
    let lease_json = match lease {
        LeaseView::Owned { pid, owner_alive } => {
            json!({"status": "owned", "pid": pid, "owner_alive": owner_alive})
        }
        LeaseView::Missing => json!({"status": "missing"}),
        LeaseView::HeldByOther {
            holder_run_id,
            pid,
            owner_alive,
        } => json!({
            "status": "held_by_other",
            "holder_run_id": holder_run_id,
            "pid": pid,
            "owner_alive": owner_alive,
        }),
    };
    Ok(json!({
        "run_id": run.id,
        "thread_id": run.thread_id,
        "workspace_id": run.workspace_id,
        "backend": run.backend,
        "profile": run.profile,
        "state": run.state,
        "supervisor": {"status": sup_status, "detail": supervisor.detail},
        "lease": lease_json,
        "started_ms": run.started_ms,
        "ended_ms": run.ended_ms,
        "status": status,
    }))
}

fn cmd_run(p: &Parsed, data_dir: &Path, db_path: &Path) -> Result<i32> {
    match p.pos.get(1).map(String::as_str) {
        Some("list") => {
            let active_only = p.yes("active");
            let limit: i64 = p
                .flag("limit")
                .map(|v| v.parse())
                .transpose()
                .map_err(|_| Error::invalid("--limit inválido"))?
                .unwrap_or(50);
            let store = open_store(db_path)?;
            // SOLO LECTURA: no se reconcilia ni se muta nada en una consulta.
            let listed = runs::list_recent(&store.conn, active_only, limit)?;
            let arr: Vec<Value> = listed
                .iter()
                .map(|run| run_view(&store, run))
                .collect::<Result<_>>()?;
            print_json(&Value::Array(arr))?;
            Ok(0)
        }
        Some("start") => {
            let thread_id = p.require("thread")?;
            let prompt = p.require("prompt")?;
            let backend = p
                .flag("backend")
                .or_else(|| std::env::var("AERIL_BACKEND").ok())
                .unwrap_or_else(|| "hermes".into());
            let permissions = match p.flag("permissions").as_deref() {
                None | Some("auto") => PermissionPolicySetting::Auto,
                Some("deny") => PermissionPolicySetting::Deny,
                Some(other) => {
                    return Err(Error::invalid(format!(
                        "--permissions {other} no soportado (auto|deny)"
                    )));
                }
            };
            let mut store = open_store(db_path)?;
            // P2-S1 R1 (P1-4): admisión temprana ANTES de la reconciliación
            // de crash. Si el perfil gestionado del thread tiene un escritor
            // sobreviviente, el run se RECHAZA sin tocar al sobreviviente
            // (la reconciliación certificada termina backends huérfanos
            // verificados; con perfiles gestionados la decisión debe ser del
            // operador: matar verificando identidad y reconciliar con
            // `aeril recover`).
            if crate::profile::enabled()
                && crate::profile::is_hermes_backend(&backend)
                && let Some(agent) = crate::profile::agent_for_thread(data_dir, &thread_id)?
            {
                crate::profile::preflight_no_live_writer(data_dir, &agent)?;
            }
            // S0: reconciliar runs interrumpidos (supervisores muertos) y
            // checkpoints pendientes antes de arrancar un run nuevo.
            crate::recovery::reconcile_interrupted_runs(&mut store)?;
            checkpoint::recover_pending(&mut store, &|path: &str| git::open(Path::new(path)).ok())?;
            // Cancelación programada (para pruebas E2E y demos): pide
            // `session/cancel` al motor tras N ms de iniciado el run.
            let cancel_rx = cancel_receiver(p)?;
            let mut runner = Runner {
                data_dir: data_dir.to_path_buf(),
                db_path: db_path.to_path_buf(),
                ui: None,
                cancel: cancel_rx,
                invocation: String::new(),
            };
            let opts = RunOptions {
                backend,
                goal: prompt,
                permissions,
                feedback_comment_ids: None,
                resume_session: p.yes("resume"),
                model: p.flag("model"),
            };
            let summary = runner.start(&mut store, &thread_id, &opts)?;
            print_run_summary(&summary)?;
            Ok(0)
        }
        other => Err(Error::invalid(format!(
            "run {other:?} no soportado (start|list [--active] [--limit N])"
        ))),
    }
}

fn print_run_summary(summary: &session::RunSummary) -> Result<()> {
    let snapshot = summary.snapshot.as_ref().map(|s| {
        json!({
            "snapshot_id": s.snapshot_id,
            "oid": s.oid,
            "event_id": s.event_id,
            "files": s.files.len(),
            "patch_len": s.patch_len,
            "had_changes": true,
        })
    });
    print_json(&json!({
        "thread_id": summary.thread_id,
        "run_id": summary.run_id,
        "state": summary.state,
        "stop_reason": summary.stop_reason,
        "engine_session_id": summary.engine_session_id,
        "profile": summary.profile,
        "detail": summary.detail,
        "snapshot": snapshot,
    }))?;
    Ok(())
}

// ── checkpoint ───────────────────────────────────────────────────────────────

fn cmd_checkpoint(p: &Parsed, db_path: &Path) -> Result<i32> {
    match p.pos.get(1).map(String::as_str) {
        Some("list") => {
            let thread_id = p.require("thread")?;
            let store = open_store(db_path)?;
            let mut map: HashMap<String, Value> = HashMap::new();
            for ev in events::fetch_by_kind(&store.conn, &thread_id, events::K_CHECKPOINT_PREPARED)?
            {
                let prep: payload::CheckpointPrepared = ev.parse()?;
                map.entry(prep.snapshot_id.clone()).or_insert_with(|| {
                    json!({
                        "snapshot_id": prep.snapshot_id,
                        "state": "prepared",
                        "expected_oid": prep.expected_oid,
                        "git_ref": prep.git_ref,
                        "seq": ev.seq,
                    })
                });
            }
            for ev in events::fetch_by_kind(&store.conn, &thread_id, events::K_CHECKPOINT_READY)? {
                let ready: payload::CheckpointReady = ev.parse()?;
                map.insert(
                    ready.snapshot_id.clone(),
                    json!({
                        "snapshot_id": ready.snapshot_id,
                        "state": "ready",
                        "oid": ready.oid,
                        "git_ref": ready.git_ref,
                        "recovered": ready.recovered,
                        "files": ready.files.len(),
                        "seq": ev.seq,
                    }),
                );
            }
            for ev in events::fetch_by_kind(&store.conn, &thread_id, events::K_CHECKPOINT_FAILED)? {
                let failed: payload::CheckpointFailed = ev.parse()?;
                map.insert(
                    failed.snapshot_id.clone(),
                    json!({
                        "snapshot_id": failed.snapshot_id,
                        "state": "failed",
                        "reason": failed.reason,
                        "terminal": failed.terminal,
                        "seq": ev.seq,
                    }),
                );
            }
            let mut arr: Vec<Value> = map.into_values().collect();
            arr.sort_by_key(|v| v.get("seq").and_then(Value::as_i64).unwrap_or(0));
            print_json(&Value::Array(arr))?;
            Ok(0)
        }
        Some("show") => {
            let thread_id = p.require("thread")?;
            let snapshot_id = p.require("snapshot")?;
            let store = open_store(db_path)?;
            let (ev, ready) = checkpoint::ready_for_snapshot(&store, &thread_id, &snapshot_id)?
                .ok_or_else(|| {
                    Error::not_ready(format!("no hay checkpoint ready {snapshot_id}"))
                })?;
            print_json(&json!({
                "snapshot_id": ready.snapshot_id,
                "event_id": ev.id,
                "oid": ready.oid,
                "base_oid": ready.base_oid,
                "git_ref": ready.git_ref,
                "recovered": ready.recovered,
                "files": ready.files,
                "patch": ready.patch,
            }))?;
            Ok(0)
        }
        other => Err(Error::invalid(format!("checkpoint {other:?} no soportado"))),
    }
}

// ── comment ──────────────────────────────────────────────────────────────────

fn comment_view_json(view: &review::CommentView) -> Value {
    json!({
        "comment_id": view.comment_id,
        "seq": view.seq,
        "text": view.text,
        "state": view.state.as_str(),
        "snapshot_id": view.anchor.snapshot_id,
        "snapshot_oid": view.snapshot_oid,
        "anchor": view.anchor,
        "blob_verified": view.blob_verified,
        "anchored_text": view.anchored_text,
        "original_patch": view.original_patch,
    })
}

fn cmd_comment(p: &Parsed, db_path: &Path) -> Result<i32> {
    match p.pos.get(1).map(String::as_str) {
        Some("add") => {
            let thread_id = p.require("thread")?;
            let snapshot_id = p.require("snapshot")?;
            let path = p.require("path")?;
            let start: u64 = p
                .require("start")?
                .parse()
                .map_err(|_| Error::invalid("--start debe ser un número"))?;
            let end: u64 = p
                .require("end")?
                .parse()
                .map_err(|_| Error::invalid("--end debe ser un número"))?;
            let text = p.require("text")?;
            let mut store = open_store(db_path)?;
            let (ev, _ready) = checkpoint::ready_for_snapshot(&store, &thread_id, &snapshot_id)?
                .ok_or_else(|| {
                    Error::not_ready(format!("no hay checkpoint ready {snapshot_id}"))
                })?;
            let row = threads::get(&store.conn, &thread_id)?
                .ok_or_else(|| Error::not_found(format!("thread {thread_id}")))?;
            let repo = git::open(Path::new(&row.repo_path))?;
            let id = review::add_comment(
                &mut store, &repo, &thread_id, &ev.id, &path, start, end, &text,
            )?;
            print_json(
                &json!({"comment_id": id, "thread_id": thread_id, "snapshot_id": snapshot_id}),
            )?;
            Ok(0)
        }
        Some("show") => {
            let thread_id = p.require("thread")?;
            let comment_id = p.require("comment")?;
            let store = open_store(db_path)?;
            let repo = threads::get(&store.conn, &thread_id)?
                .and_then(|t| git::open(Path::new(&t.repo_path)).ok());
            let view = review::view_comment(&store, &thread_id, &comment_id, repo.as_ref())?;
            print_json(&comment_view_json(&view))?;
            Ok(0)
        }
        Some("list") => {
            let thread_id = p.require("thread")?;
            let limit: i64 = p
                .flag("limit")
                .map(|v| v.parse())
                .transpose()
                .map_err(|_| Error::invalid("--limit inválido"))?
                .unwrap_or(100);
            let offset: i64 = p
                .flag("offset")
                .map(|v| v.parse())
                .transpose()
                .map_err(|_| Error::invalid("--offset inválido"))?
                .unwrap_or(0);
            let store = open_store(db_path)?;
            let summaries = review::list_comment_summaries(&store, &thread_id, limit, offset)?;
            let arr: Vec<Value> = summaries
                .iter()
                .map(|c| {
                    json!({
                        "comment_id": c.comment_id,
                        "seq": c.seq,
                        "text": c.text,
                        "state": c.state.as_str(),
                        "snapshot_id": c.anchor.snapshot_id,
                        "snapshot_oid": c.snapshot_oid,
                        "anchor": c.anchor,
                    })
                })
                .collect();
            print_json(&Value::Array(arr))?;
            Ok(0)
        }
        Some("resolve") => {
            let thread_id = p.require("thread")?;
            let comment_id = p.require("comment")?;
            let mut store = open_store(db_path)?;
            review::resolve(&mut store, &thread_id, &comment_id)?;
            print_json(&json!({"resolved": comment_id}))?;
            Ok(0)
        }
        other => Err(Error::invalid(format!("comment {other:?} no soportado"))),
    }
}

// ── feedback ─────────────────────────────────────────────────────────────────

fn cmd_feedback(p: &Parsed, data_dir: &Path, db_path: &Path) -> Result<i32> {
    match p.pos.get(1).map(String::as_str) {
        Some("send") => {
            let thread_id = p.require("thread")?;
            let backend = p
                .flag("backend")
                .or_else(|| std::env::var("AERIL_BACKEND").ok())
                .unwrap_or_else(|| "hermes".into());
            let mut store = open_store(db_path)?;
            crate::recovery::reconcile_interrupted_runs(&mut store)?;
            checkpoint::recover_pending(&mut store, &|path: &str| git::open(Path::new(path)).ok())?;
            let ids: Vec<String> = match p.flag("comments") {
                Some(csv) => csv
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
                None => review::open_comment_ids(&store, &thread_id)?,
            };
            if ids.is_empty() {
                return Err(Error::invalid("no hay comentarios para enviar"));
            }
            let cancel_rx = cancel_receiver(p)?;
            let mut runner = Runner {
                data_dir: data_dir.to_path_buf(),
                db_path: db_path.to_path_buf(),
                ui: None,
                cancel: cancel_rx,
                invocation: String::new(),
            };
            let opts = RunOptions {
                backend,
                goal: "Feedback del revisor".into(),
                permissions: PermissionPolicySetting::Auto,
                feedback_comment_ids: Some(ids),
                // El feedback busca continuar el trabajo: se intenta reanudar
                // la sesión del motor (con fallback explícito a sesión nueva).
                resume_session: true,
                model: p.flag("model"),
            };
            let summary = runner.start(&mut store, &thread_id, &opts)?;
            print_run_summary(&summary)?;
            Ok(0)
        }
        other => Err(Error::invalid(format!("feedback {other:?} no soportado"))),
    }
}

// ── lease ────────────────────────────────────────────────────────────────────

fn cmd_lease(p: &Parsed, db_path: &Path) -> Result<i32> {
    match p.pos.get(1).map(String::as_str) {
        Some("status") => {
            let store = open_store(db_path)?;
            let list = crate::store::leases::list(&store.conn)?;
            let arr: Vec<Value> = list
                .iter()
                .map(|(ws, info)| {
                    json!({
                        "workspace_id": ws,
                        "run_id": info.run_id,
                        "pid": info.pid,
                        "owner_alive": crate::store::leases::owner_alive(info),
                        "heartbeat_age_ms": crate::timeutil::now_ms() - info.renewed_ms,
                        "ttl_ms": info.ttl_ms,
                    })
                })
                .collect();
            print_json(&Value::Array(arr))?;
            Ok(0)
        }
        Some("release") => {
            let ws = p.require("workspace")?;
            let force = p.yes("force");
            let mut store = open_store(db_path)?;
            let info = crate::store::leases::release_external(&mut store.conn, &ws, force)?;
            print_json(&json!({
                "released": true,
                "workspace_id": ws,
                "previous_run_id": info.run_id,
                "previous_pid": info.pid,
                "force_flag_received": force,
                "note": "--force no puede pisar a un dueño vivo; solo se liberan dueños con muerte verificada",
            }))?;
            Ok(0)
        }
        other => Err(Error::invalid(format!(
            "lease {other:?} no soportado (status | release --workspace W [--force: protocolar, NO pisa a un dueño vivo])"
        ))),
    }
}

// ── recover ──────────────────────────────────────────────────────────────────

fn cmd_recover(_p: &Parsed, db_path: &Path) -> Result<i32> {
    let mut store = open_store(db_path)?;
    // S0: runs interrumpidos (supervisor muerto) primero.
    let run_outcomes = crate::recovery::reconcile_interrupted_runs(&mut store)?;
    let outcomes =
        checkpoint::recover_pending(&mut store, &|path: &str| git::open(Path::new(path)).ok())?;
    // Reconciliación de integraciones interrumpidas (B4): nunca mueve refs,
    // solo registra lo que la ref demuestra.
    let integrations = session::reconcile_integrations(&mut store)?;
    let mut run_arr: Vec<Value> = run_outcomes
        .iter()
        .map(|r| match &r.outcome {
            crate::recovery::RunOutcome::Interrupted {
                supervisor,
                backend,
                unsafe_state,
            } => json!({
                "action": "run_interrupted",
                "run_id": r.run_id,
                "thread_id": r.thread_id,
                "workspace_id": r.workspace_id,
                "supervisor": supervisor,
                "backend": backend,
                "unsafe_state": unsafe_state,
            }),
            crate::recovery::RunOutcome::SupervisorAlive { supervisor } => json!({
                "action": "run_supervisor_alive",
                "run_id": r.run_id,
                "thread_id": r.thread_id,
                "supervisor": supervisor,
            }),
            crate::recovery::RunOutcome::SupervisorUnknown => json!({
                "action": "run_supervisor_unknown",
                "run_id": r.run_id,
                "thread_id": r.thread_id,
                "detail": "sin evidencia de supervisor; no se reconcilia (conservador)",
            }),
            crate::recovery::RunOutcome::AlreadyResolved => json!({
                "action": "run_already_resolved",
                "run_id": r.run_id,
                "thread_id": r.thread_id,
            }),
        })
        .collect();
    let arr: Vec<Value> = outcomes
        .iter()
        .map(|o| match o {
            checkpoint::RecoveryOutcome::Completed {
                snapshot_id,
                ref_recreated,
            } => json!({"action": "completed_ready", "snapshot_id": snapshot_id, "ref_recreated": ref_recreated}),
            checkpoint::RecoveryOutcome::AlreadyResolved { snapshot_id } => {
                json!({"action": "already_resolved", "snapshot_id": snapshot_id})
            }
            checkpoint::RecoveryOutcome::RefConflict { snapshot_id, detail } => {
                json!({"action": "ref_conflict", "snapshot_id": snapshot_id, "detail": detail})
            }
            checkpoint::RecoveryOutcome::ObjectsMissing { snapshot_id, detail } => {
                json!({"action": "objects_missing", "snapshot_id": snapshot_id, "detail": detail})
            }
            checkpoint::RecoveryOutcome::RepoUnavailable { snapshot_id, repo_path } => {
                json!({"action": "repo_unavailable", "snapshot_id": snapshot_id, "repo_path": repo_path})
            }
            checkpoint::RecoveryOutcome::Deferred { snapshot_id, detail } => {
                json!({"action": "deferred", "snapshot_id": snapshot_id, "detail": detail})
            }
            checkpoint::RecoveryOutcome::Failed { snapshot_id, detail } => {
                json!({"action": "failed", "snapshot_id": snapshot_id, "detail": detail})
            }
        })
        .collect();
    run_arr.extend(arr);
    let mut arr = run_arr;
    for i in integrations {
        arr.push(match i {
            session::IntegrationReconcile::CompletedRecovered {
                target_branch,
                source_oid,
            } => json!({"action": "integration_completed_recovered", "target_branch": target_branch, "source_oid": source_oid}),
            session::IntegrationReconcile::Abandoned {
                target_branch,
                source_oid,
                reason,
            } => json!({"action": "integration_abandoned", "target_branch": target_branch, "source_oid": source_oid, "reason": reason}),
        });
    }
    print_json(&Value::Array(arr))?;
    Ok(0)
}

// ── worktree ─────────────────────────────────────────────────────────────────

// ── profile (P2-S1) ──────────────────────────────────────────────────────────

fn cmd_profile(p: &Parsed, data_dir: &Path, db_path: &Path) -> Result<i32> {
    if !crate::profile::enabled() {
        return Err(Error::invalid(format!(
            "perfiles gestionados deshabilitados; exportá {}=1 para habilitar (default OFF)",
            crate::profile::FLAG_ENV
        )));
    }
    let json_view = |v: &crate::profile::ProfileView| -> Value {
        json!({
            "agent_id": v.agent_id,
            "registered": v.registered,
            "dir_state": v.dir_state,
            "auth_status": v.auth_status,
            "auth_checked_ms": v.auth_checked_ms,
            "thread_ids": v.thread_ids,
            "lock_held": v.lock_held,
            "lock_state": v.lock_state,
            "warmed_ms": v.warmed_ms,
            "disk_kb": v.disk_kb,
            "removing": v.removing,
        })
    };
    match p.pos.get(1).map(String::as_str) {
        Some("create") => {
            let agent_id = p.require("agent")?;
            let provider = p.flag("provider").unwrap_or_else(|| "nous".into());
            let model = p.flag("model");
            let warm = p.yes("warm");
            let v = crate::profile::create(data_dir, &agent_id, &provider, model.as_deref(), warm)?;
            print_json(&json_view(&v))?;
            Ok(0)
        }
        Some("list") => {
            let list = crate::profile::list(data_dir)?;
            let arr: Vec<Value> = list.iter().map(&json_view).collect();
            print_json(&Value::Array(arr))?;
            Ok(0)
        }
        Some("status") => {
            let agent_id = p.require("agent")?;
            let v = crate::profile::status(data_dir, &agent_id)?;
            print_json(&json_view(&v))?;
            Ok(0)
        }
        Some("warm") => {
            let agent_id = p.require("agent")?;
            let warmed_ms = crate::profile::warm_profile(data_dir, &agent_id)?;
            print_json(&json!({"agent_id": agent_id, "warmed_ms": warmed_ms}))?;
            Ok(0)
        }
        Some("assign") => {
            let agent_id = p.require("agent")?;
            let thread_id = p.require("thread")?;
            crate::profile::assign(data_dir, &agent_id, &thread_id, db_path)?;
            print_json(&json!({"assigned": true, "agent_id": agent_id, "thread_id": thread_id}))?;
            Ok(0)
        }
        Some("unassign") => {
            let thread_id = p.require("thread")?;
            let removed = crate::profile::unassign(data_dir, &thread_id, db_path)?;
            print_json(&json!({"unassigned": removed, "thread_id": thread_id}))?;
            Ok(0)
        }
        Some("recover") => {
            let agent_id = p.require("agent")?;
            if !p.yes("confirm") {
                return Err(Error::invalid(
                    "profile recover requiere --confirm (reconciliación explícita)",
                ));
            }
            let verdict = crate::profile::recover(data_dir, &agent_id)?;
            print_json(&json!({"agent_id": agent_id, "recovery": verdict}))?;
            Ok(0)
        }
        Some("remove") => {
            let agent_id = p.require("agent")?;
            let confirm = p.yes("confirm");
            let clear_threads = p.yes("clear-threads");
            crate::profile::remove(data_dir, &agent_id, confirm, clear_threads, db_path)?;
            print_json(&json!({"removed": true, "agent_id": agent_id}))?;
            Ok(0)
        }
        other => Err(Error::invalid(format!(
            "profile {other:?} no soportado (create|list|status|warm|assign|unassign|remove|recover)"
        ))),
    }
}

// ── workspace ────────────────────────────────────────────────────────────────

/// Vista de solo lectura de un workspace (S4). Reutiliza los clasificadores
/// existentes: verificación de worktree (git), supervisor/liveness (S0/S2) y
/// el veredicto de reuso (S0). Nunca muta: ni DB, ni filesystem, ni leases.
fn workspace_view(store: &Store, data_dir: &Path, w: &runs::WorkspaceRow) -> Result<Value> {
    use crate::recovery::{LivenessView, ReuseVerdict};
    let ws = &w.workspace_id;

    let run = runs::get(&store.conn, &w.thread_id, &w.latest_run_id)?
        .ok_or_else(|| Error::integrity(format!("run {} no encontrado", w.latest_run_id)))?;
    let supervisor = crate::recovery::supervisor_view(&store.conn, &run)?;
    let sup_status = match supervisor.liveness {
        LivenessView::Alive => "alive",
        LivenessView::Dead => "dead",
        LivenessView::Unknown => "unknown",
    };

    let lease = crate::store::leases::get(&store.conn, ws)?;
    let lease_live = lease
        .as_ref()
        .map(crate::store::leases::owner_alive)
        .unwrap_or(false);
    let lease_json = match &lease {
        Some(info) => json!({
            "run_id": info.run_id,
            "pid": info.pid,
            "owner_alive": crate::store::leases::owner_alive(info),
        }),
        None => Value::Null,
    };

    let unsafe_note = crate::recovery::workspace_recovery_unsafe(&store.conn, ws)?;

    // Verificación del worktree. Si el repo no abre: diagnóstico por workspace
    // (jamás se declara válido en silencio).
    let repo_path = w.repo_path.clone().unwrap_or_default();
    let expected_path = git::worktree::path_for(data_dir, ws);
    let (wt_status, wt_detail) = match git::open(Path::new(&repo_path)) {
        Err(e) => (
            "repo_unavailable".to_string(),
            format!("no se pudo abrir el repo {repo_path}: {e}"),
        ),
        Ok(repo) => match &w.base_oid {
            None => (
                "invalid".to_string(),
                "el run no registra base_oid".to_string(),
            ),
            Some(base) => match git::worktree::verify(&repo, data_dir, ws, base) {
                git::worktree::VerifyOutcome::Ok => ("ok".to_string(), String::new()),
                git::worktree::VerifyOutcome::Missing => (
                    "missing".to_string(),
                    "worktree no registrado (¿borrado o nunca creado?)".to_string(),
                ),
                git::worktree::VerifyOutcome::Invalid(d) => ("invalid".to_string(), d),
                git::worktree::VerifyOutcome::Mismatched(d) => ("mismatched".to_string(), d),
            },
        },
    };

    // Elegibilidad de reuso = veredicto S0 + worktree utilizable + sin lease vivo.
    let verdict = crate::recovery::workspace_reuse_verdict(&store.conn, ws)?;
    let (mut eligible, mut reuse_reason) = match (&wt_status[..], verdict) {
        ("ok", ReuseVerdict::Safe) => (true, String::new()),
        ("ok", ReuseVerdict::Blocked(reason)) => (false, reason),
        (status, _) => (
            false,
            format!("worktree no utilizable ({status}): {wt_detail}"),
        ),
    };
    if eligible
        && lease_live
        && let Some(info) = &lease
    {
        eligible = false;
        reuse_reason = format!(
            "lease vivo (run {}, pid {}): no se reutiliza mientras el dueño siga vivo",
            info.run_id, info.pid
        );
    }

    // Estado operativo (precedencia: seguridad > uso > presencia > disponibilidad).
    let wt_label = match wt_status.as_str() {
        "missing" => Some("missing_worktree"),
        "invalid" | "mismatched" => Some("invalid_worktree"),
        _ => None,
    };
    let unresolved = run.state == "starting" || run.state == "running";
    let status: &str = if wt_status == "repo_unavailable" {
        "repo_unavailable"
    } else if unsafe_note.is_some() {
        "unsafe_for_reuse"
    } else if lease_live && wt_status == "ok" {
        "verified_leased"
    } else if unresolved && !lease_live {
        "stale_or_uncertain"
    } else if let Some(label) = wt_label {
        label
    } else if !eligible {
        "unsafe_for_reuse"
    } else {
        "verified_available"
    };

    Ok(json!({
        "workspace_id": ws,
        "thread_id": w.thread_id,
        "latest_run_id": w.latest_run_id,
        "latest_run_state": run.state,
        "repo_path": repo_path,
        "worktree_path": w
            .worktree_path
            .clone()
            .unwrap_or_else(|| expected_path.to_string_lossy().into_owned()),
        "expected_worktree_path": expected_path.to_string_lossy(),
        "branch": w.branch,
        "base_oid": w.base_oid,
        "worktree": {"status": wt_status, "detail": wt_detail},
        "lease": lease_json,
        "supervisor": {"status": sup_status, "detail": supervisor.detail},
        "recovery_unsafe": unsafe_note.is_some(),
        "recovery_unsafe_detail": unsafe_note.as_ref().map(|n| n.detail.clone()),
        "reuse": {"eligible": eligible, "reason": reuse_reason},
        "status": status,
        "started_ms": w.started_ms,
        "ended_ms": w.ended_ms,
    }))
}

fn cmd_workspace(p: &Parsed, data_dir: &Path, db_path: &Path) -> Result<i32> {
    match p.pos.get(1).map(String::as_str) {
        Some("list") => {
            let thread_filter = p.flag("thread");
            let limit: i64 = p
                .flag("limit")
                .map(|v| v.parse())
                .transpose()
                .map_err(|_| Error::invalid("--limit inválido"))?
                .unwrap_or(100);
            let store = open_store(db_path)?;
            // SOLO LECTURA: no se reconcilia, no se toca el filesystem y no se
            // crean worktrees durante una consulta.
            let rows = runs::workspaces_latest(&store.conn, thread_filter.as_deref(), limit)?;
            let arr: Vec<Value> = rows
                .iter()
                .map(|w| workspace_view(&store, data_dir, w))
                .collect::<Result<_>>()?;
            print_json(&Value::Array(arr))?;
            Ok(0)
        }
        other => Err(Error::invalid(format!(
            "workspace {other:?} no soportado (list [--thread T] [--limit N])"
        ))),
    }
}

/// Barrera de sincronización SOLO PARA TESTS (no existe en release: se
/// compila fuera con `cfg(not(debug_assertions))`). Si la env
/// `AERIL_TEST_RM_BARRIER_*` apunta a un directorio, escribe `<dir>/ready` y
/// espera `<dir>/go` (máx. 30 s). Sin la env no hace absolutamente nada.
fn test_rm_barrier(name: &str) {
    #[cfg(not(debug_assertions))]
    {
        let _ = name;
    }
    #[cfg(debug_assertions)]
    {
        if let Ok(dir) = std::env::var(name) {
            let dir = std::path::PathBuf::from(dir);
            let _ = std::fs::write(dir.join("ready"), b"1");
            let go = dir.join("go");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !go.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
}

/// Motivo de rechazo por lease para `worktree rm` (S4-R1): el mismo mensaje se
/// usa en el chequeo barato y en el fence final transaccional.
fn rm_lease_refusal(ws: &str, info: &crate::store::leases::LeaseInfo) -> Error {
    if crate::store::leases::owner_alive(info) {
        Error::conflict(format!(
            "el workspace {ws} tiene un lease con dueño VIVO (run {}, pid {}): su lease NO se libera, ni con `--force` (RC2.1) y el borrado se rechaza. Terminá el proceso dueño (cierre normal, SIGTERM o SIGKILL), verificá su muerte (p. ej. `ps -p {}`) y reintentá",
            info.run_id, info.pid, info.pid
        ))
    } else {
        Error::conflict(format!(
            "el workspace {ws} tiene un lease registrado de un dueño sin vida verificada (run {}, pid {}): el borrado se rechaza — cualquier lease presente bloquea en el fence final. Resolvelo por su ciclo de vida: verificá la muerte del proceso y liberá con `aeril lease release --workspace {ws}` (que rechaza dueños vivos), y reintentá",
            info.run_id, info.pid
        ))
    }
}

fn cmd_worktree(p: &Parsed, data_dir: &Path, db_path: &Path) -> Result<i32> {
    match p.pos.get(1).map(String::as_str) {
        Some("rm") => {
            let thread_id = p.require("thread")?;
            let unsafe_ok = p.yes("unsafe-ok");
            let mut store = open_store(db_path)?;
            let row = threads::get(&store.conn, &thread_id)?
                .ok_or_else(|| Error::not_found(format!("thread {thread_id}")))?;
            let run = runs::latest(&store.conn, &thread_id)?
                .ok_or_else(|| Error::invalid("el thread no tiene runs"))?;
            let ws = run
                .workspace_id
                .ok_or_else(|| Error::invalid("el run no tiene workspace"))?;

            // 1) Chequeo barato (sin lock): un dueño VIVO se rechaza temprano.
            if let Some(info) = crate::store::leases::active(&store.conn, &ws)? {
                return Err(rm_lease_refusal(&ws, &info));
            }

            // 2) Preparación (repo abrible) FUERA de la transacción final.
            let repo = git::open(Path::new(&row.repo_path))?;
            test_rm_barrier("AERIL_TEST_RM_BARRIER_PRE_TX");

            // 3) Decisión final serializada con `leases::try_acquire`: ambos usan
            //    TransactionBehavior::Immediate, así que una adquisición no
            //    puede colarse entre el chequeo y el borrado. La transacción
            //    serializa contra adquisiciones de lease de Aeril; NO es una
            //    transacción atómica DB+filesystem (el rollback no deshace el
            //    borrado de archivos).
            let tx = store
                .conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

            // 3a) La identidad del target no cambió desde la selección.
            let current_ws = runs::latest(&tx, &thread_id)?
                .and_then(|r| r.workspace_id)
                .ok_or_else(|| {
                    Error::conflict(
                        "el thread quedó sin run durante la operación; borrado abortado",
                    )
                })?;
            if current_ws != ws {
                return Err(Error::conflict(format!(
                    "el thread cambió de workspace durante la operación ({ws} → {current_ws}); borrado abortado; reintentá"
                )));
            }

            // 3b) Fence final de lease: CUALQUIER fila presente bloquea (un
            //     lease stale se resuelve por el ciclo de vida existente).
            if let Some(info) = crate::store::leases::get(&tx, &ws)? {
                return Err(rm_lease_refusal(&ws, &info));
            }

            // 3c) Guard específico de borrado (D1): un run sin resolver con
            //     supervisor VIVO o DESCONOCIDO se rechaza por defecto aunque no
            //     haya lease; `--unsafe-ok` es un reconocimiento explícito de
            //     riesgo, no una prueba de que el workspace dejó de modificarse.
            if let Some(latest) = runs::latest_of_workspace(&tx, &ws)?
                && (latest.state == "starting" || latest.state == "running")
            {
                let view = crate::recovery::supervisor_view(&tx, &latest)?;
                let blocked = matches!(
                    view.liveness,
                    crate::recovery::LivenessView::Alive | crate::recovery::LivenessView::Unknown
                );
                if blocked && !unsafe_ok {
                    return Err(Error::conflict(format!(
                        "el workspace {ws} tiene un run sin resolver (run {}, estado {}) con supervisor {} ({}): el borrado por defecto se rechaza. `--unsafe-ok` NO termina backends ni prueba que el workspace haya dejado de modificarse; si verificaste manualmente que no queda ningún proceso escribiendo, repetí con `--unsafe-ok`",
                        latest.id,
                        latest.state,
                        if matches!(view.liveness, crate::recovery::LivenessView::Alive) {
                            "vivo"
                        } else {
                            "desconocido"
                        },
                        view.detail
                    )));
                }
            }

            // 3d) Evidencia S0 de backend/recovery (sin cambios de semántica):
            //     bloqueado ⇒ exige acción explícita del usuario.
            if !unsafe_ok
                && let crate::recovery::ReuseVerdict::Blocked(reason) =
                    crate::recovery::workspace_reuse_verdict(&tx, &ws)?
            {
                return Err(Error::conflict(format!(
                    "el workspace {ws} no es seguro para borrar: {reason}. Aeril solo verifica procesos del grupo que él creó; si comprobaste manualmente que no queda ningún backend escribiendo en el worktree, repetí con `--unsafe-ok` (acción explícita; no se tocan checkpoints, refs ni comentarios)"
                )));
            }

            test_rm_barrier("AERIL_TEST_RM_BARRIER_IN_TX");

            // 4) Borrado con el lock de escritura todavía tomado; commit después.
            git::worktree::remove(&repo, data_dir, &ws)?;
            tx.commit()?;
            print_json(&json!({"removed": true, "workspace_id": ws, "unsafe_ok": unsafe_ok}))?;
            Ok(0)
        }
        other => Err(Error::invalid(format!(
            "worktree {other:?} no soportado (rm --thread T [--unsafe-ok])"
        ))),
    }
}

// ── integrate ────────────────────────────────────────────────────────────────

fn cmd_integrate(p: &Parsed, db_path: &Path) -> Result<i32> {
    // `--confirm` es booleano para apply; si aparece antes del subcomando el
    // parser podría consumir `apply` como su valor y caer en la ruta legacy.
    // Rechazar esa forma evita un bypass silencioso del gate asistido.
    if p.pos.get(1).is_none() && p.yes("confirm") {
        return Err(Error::invalid(
            "--confirm solo es válido con `integrate apply`; no se ejecuta la ruta legacy",
        ));
    }
    match p.pos.get(1).map(String::as_str) {
        None => cmd_integrate_legacy(p, db_path),
        Some("preview") => cmd_integrate_preview(p, db_path),
        Some("apply") => cmd_integrate_apply(p, db_path),
        Some(other) => Err(Error::invalid(format!(
            "integrate {other:?} no soportado (preview|apply)"
        ))),
    }
}

/// Compatibilidad P2-R1: fast-forward directo, sin gate de validación/review.
fn cmd_integrate_legacy(p: &Parsed, db_path: &Path) -> Result<i32> {
    let thread_id = p.require("thread")?;
    let snapshot_id = p.require("snapshot")?;
    let to = p.flag("to").unwrap_or_else(|| "main".into());
    let expect = p.flag("expect");
    let mut store = open_store(db_path)?;
    let outcome = session::integrate(&mut store, &thread_id, &snapshot_id, &to, expect.as_deref())?;
    print_json(&json!({
        "target_branch": to,
        "target_oid_before": outcome.target_oid_before,
        "target_oid_after": outcome.target_oid_after,
    }))?;
    Ok(0)
}

fn cmd_integrate_preview(p: &Parsed, db_path: &Path) -> Result<i32> {
    let thread_id = p.require("thread")?;
    let snapshot_id = p.require("snapshot")?;
    let to = p.require("to")?;
    let store = open_store(db_path)?;
    let preview = integration::preview(&store, &thread_id, &snapshot_id, &to)?;
    print_json(&serde_json::to_value(preview)?)?;
    Ok(0)
}

fn cmd_integrate_apply(p: &Parsed, db_path: &Path) -> Result<i32> {
    if !p.yes("confirm") {
        return Err(Error::invalid(
            "integrate apply requiere --confirm (no hay confirmación implícita)",
        ));
    }
    let thread_id = p.require("thread")?;
    let snapshot_id = p.require("snapshot")?;
    let to = p.require("to")?;
    let source = p.require("source")?;
    let expect = p.require("expect")?;
    let mut store = open_store(db_path)?;
    let applied = integration::apply(&mut store, &thread_id, &snapshot_id, &to, &source, &expect)?;
    print_json(&serde_json::to_value(applied)?)?;
    Ok(0)
}

// ── validate ─────────────────────────────────────────────────────────────────

fn cmd_validate(p: &Parsed, data_dir: &Path, db_path: &Path) -> Result<i32> {
    let thread_id = p.require("thread")?;
    let snapshot_id = p.require("snapshot")?;
    if p.after_dashdash.is_empty() {
        return Err(Error::invalid(
            "falta el comando: aeril validate --thread T --snapshot S -- cmd args…",
        ));
    }
    let mut store = open_store(db_path)?;
    let record = session::validate(
        &mut store,
        data_dir,
        &thread_id,
        &snapshot_id,
        &p.after_dashdash,
    )?;
    print_json(&json!({
        "snapshot_id": record.snapshot_id,
        "oid": record.oid,
        "command": record.command,
        "exit_code": record.exit_code,
        "duration_ms": record.duration_ms,
        "output_hash": record.output_hash,
        "output_tail": record.output_tail,
        "stdout_bytes": record.stdout_bytes,
        "stderr_bytes": record.stderr_bytes,
        "stdout_hash": record.stdout_hash,
        "stderr_hash": record.stderr_hash,
        "tree_before_ok": record.tree_before_ok,
        "tree_after_unchanged": record.tree_after_unchanged,
        "checkout_id": record.checkout_id,
    }))?;
    Ok(if record.exit_code == 0 { 0 } else { 3 })
}

// ── export ───────────────────────────────────────────────────────────────────

fn cmd_export(p: &Parsed, db_path: &Path) -> Result<i32> {
    let thread_id = p.require("thread")?;
    let format = p.flag("format").unwrap_or_else(|| "jsonl".into());
    let store = open_store(db_path)?;
    match format.as_str() {
        "jsonl" => {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            export::jsonl(&store, &thread_id, &mut lock)?;
        }
        "md" | "markdown" => {
            print!("{}", export::markdown(&store, &thread_id)?);
        }
        other => {
            return Err(Error::invalid(format!(
                "--format {other} no soportado (jsonl|md)"
            )));
        }
    }
    Ok(0)
}

// ── bench ────────────────────────────────────────────────────────────────────

fn cmd_bench(p: &Parsed, data_dir: &Path, db_path: &Path) -> Result<i32> {
    match p.pos.get(1).map(String::as_str) {
        Some("seed") => {
            let dir = p
                .flag("dir")
                .map(PathBuf::from)
                .unwrap_or_else(|| data_dir.join("bench"));
            let threads_n: u64 = p
                .flag("threads")
                .map(|v| v.parse())
                .transpose()
                .map_err(|_| Error::invalid("--threads inválido"))?
                .unwrap_or(100);
            let events_n: u64 = p
                .flag("events")
                .map(|v| v.parse())
                .transpose()
                .map_err(|_| Error::invalid("--events inválido"))?
                .unwrap_or(1000);
            let diffs_n: u64 = p
                .flag("diffs")
                .map(|v| v.parse())
                .transpose()
                .map_err(|_| Error::invalid("--diffs inválido"))?
                .unwrap_or(10);
            let report = bench::seed(&dir, threads_n, events_n, diffs_n)?;
            print_json(&json!({
                "dir": dir.to_string_lossy(),
                "threads": report.threads,
                "events": report.events,
                "elapsed_ms": report.elapsed_ms,
                "db_bytes": report.db_bytes,
            }))?;
            Ok(0)
        }
        Some("scale") => {
            let runs: u64 = p
                .flag("runs")
                .map(|v| v.parse())
                .transpose()
                .map_err(|_| Error::invalid("--runs inválido"))?
                .unwrap_or(5);
            let delay_ms: u64 = p
                .flag("delay-ms")
                .map(|v| v.parse())
                .transpose()
                .map_err(|_| Error::invalid("--delay-ms inválido"))?
                .unwrap_or(6000);
            let dir = data_dir.join("scale");
            let _ = db_path;
            let report = bench::scale(&dir, runs, delay_ms)?;
            print_json(&json!({
                "runs": report.runs,
                "failures": report.failures,
                "app_rss_mb": report.app_rss_kb as f64 / 1024.0,
                "agents_rss_mb": report.agents_rss_kb as f64 / 1024.0,
                "total_rss_mb": report.total_rss_kb as f64 / 1024.0,
                "gate_app_mb": report.gate_app_mb,
                "app_ok": report.app_ok,
                "note": report.note,
            }))?;
            Ok(if report.app_ok && report.failures == 0 {
                0
            } else {
                4
            })
        }
        other => Err(Error::invalid(format!("bench {other:?} no soportado"))),
    }
}
