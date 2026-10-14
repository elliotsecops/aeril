//! Orquestación de runs: thread → workspace verificado → backend externo →
//! eventos normalizados → checkpoint → feedback/validación/integración.
//!
//! Reglas: nunca se clasifica trabajo incompleto como exitoso; un prompt cuyo
//! resultado quedó incierto no se repite automáticamente.

use crate::agent::acp::{
    self as acpmod, AcpClient, PermissionDecision, PermissionRequest, TurnObserver,
};
use crate::agent::{BackendSpec, resolve_backend};
use crate::checkpoint;
use crate::crash;
use crate::error::{Error, Result};
use crate::git;
use crate::ids::new_id;
use crate::store::events::{self, payload};
use crate::store::runs::{self, RunRow, StartRunSpec};
use crate::store::{Store, leases, threads};
use crate::timeutil::now_ms;
use crate::util::{fnv1a_hex, truncate};
use git2::Repository;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use tokio::sync::oneshot;

/// Notas para la UI (la TUI relee del store; esto es para reaccionar).
#[derive(Debug, Clone)]
pub enum UiNote {
    Event {
        thread_id: String,
        kind: String,
    },
    RunDone {
        thread_id: String,
        run_id: String,
        state: String,
    },
    Error {
        thread_id: String,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionPolicySetting {
    Auto,
    Deny,
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub backend: String,
    pub goal: String,
    pub permissions: PermissionPolicySetting,
    /// Si viene `Some`, es un run de feedback sobre estos comentarios.
    pub feedback_comment_ids: Option<Vec<String>>,
    /// Intentar `session/load` de la sesión previa del motor.
    pub resume_session: bool,
    /// Modelo a fijar con `session/set_model` tras abrir la sesión.
    pub model: Option<String>,
}

#[derive(Debug)]
pub struct RunSummary {
    pub thread_id: String,
    pub run_id: String,
    pub state: String,
    pub stop_reason: Option<String>,
    pub engine_session_id: Option<String>,
    pub snapshot: Option<checkpoint::CaptureOutcome>,
    pub detail: Option<String>,
}

pub struct Runner {
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub ui: Option<std_mpsc::Sender<UiNote>>,
    pub cancel: Option<oneshot::Receiver<()>>,
}

struct WorkTarget {
    workspace_id: String,
    path: PathBuf,
    branch: String,
}

impl Runner {
    fn note(&self, note: UiNote) {
        if let Some(tx) = &self.ui {
            let _ = tx.send(note);
        }
    }

    pub fn start(
        &mut self,
        store: &mut Store,
        thread_id: &str,
        opts: &RunOptions,
    ) -> Result<RunSummary> {
        let thread = threads::get(&store.conn, thread_id)?
            .ok_or_else(|| Error::not_found(format!("thread {thread_id}")))?;
        let repo = git::open(Path::new(&thread.repo_path))?;
        let latest = runs::latest(&store.conn, thread_id)?;
        let prev_session = latest.as_ref().and_then(|r| r.engine_session_id.clone());

        // ── base de la tarea y workspace candidato ───────────────────────────
        let feedback = opts.feedback_comment_ids.is_some();
        let base_oid: String = if feedback {
            let (_, ready) = checkpoint::latest_ready(store, thread_id)?.ok_or_else(|| {
                Error::invalid("feedback requiere al menos un checkpoint en estado ready")
            })?;
            ready.oid
        } else {
            git::head_oid(&repo)?
        };
        let candidate_ws: Option<String> = if feedback {
            latest.as_ref().and_then(|r| r.workspace_id.clone())
        } else {
            match latest.as_ref() {
                Some(r) if r.base_oid.as_deref() == Some(base_oid.as_str()) => {
                    r.workspace_id.clone()
                }
                _ => None,
            }
        };

        // ── lease del workspace existente ANTES de verificarlo o tocarlo ────
        // Dos procesos de Aeril no pueden editar el mismo checkout a la vez;
        // un lease vencido o de un proceso muerto se toma y se registra.
        let pre_run_id = new_id();
        let mut lease_guard: Option<LeaseGuard> = None;
        // Señal de pérdida de ownership: si el guard la emite, el turno se
        // cancela y el run se marca fallido (nunca completitud normal).
        let mut lease_lost_rx: Option<oneshot::Receiver<()>> = None;
        if let Some(ws) = &candidate_ws {
            match leases::try_acquire(
                &mut store.conn,
                ws,
                &pre_run_id,
                std::process::id() as i64,
                leases::ttl_ms(),
            )? {
                leases::AcquireOutcome::Busy { info, age_ms } => {
                    return Err(Error::conflict(format!(
                        "workspace {ws} en uso: el proceso dueño sigue vivo (run {}, pid {}, heartbeat hace {age_ms} ms); un TTL vencido no transfiere ownership; recuperación explícita: `aeril lease release {ws} [--force]`",
                        info.run_id, info.pid
                    )));
                }
                leases::AcquireOutcome::Acquired { took_over } => {
                    if let Some(t) = took_over {
                        events::append_typed(
                            &mut store.conn,
                            thread_id,
                            None,
                            events::K_WORKSPACE_LEASE_TAKEN_OVER,
                            "system",
                            &payload::WorkspaceLeaseTakenOver {
                                workspace_id: ws.clone(),
                                previous_run_id: t.previous_run_id,
                                previous_pid: t.previous_pid,
                                age_ms: t.age_ms,
                                reason: t.reason.to_string(),
                            },
                        )?;
                    }
                    let (lost_tx, lost_rx) = oneshot::channel();
                    lease_guard = Some(LeaseGuard::start(
                        self.db_path.clone(),
                        ws.clone(),
                        pre_run_id.clone(),
                        lost_tx,
                    ));
                    lease_lost_rx = Some(lost_rx);
                }
            }
        }

        // ── verificación / recreación del workspace ──────────────────────────
        let target: WorkTarget = if feedback {
            let (_, ready) = checkpoint::latest_ready(store, thread_id)?
                .ok_or_else(|| Error::invalid("feedback sin checkpoint listo"))?;
            match candidate_ws.clone() {
                Some(ws) => match git::worktree::verify(&repo, &self.data_dir, &ws, &ready.oid) {
                    git::worktree::VerifyOutcome::Ok => WorkTarget {
                        workspace_id: ws.clone(),
                        path: git::worktree::path_for(&self.data_dir, &ws),
                        branch: crate::ids::workspace_branch(&ws),
                    },
                    other => {
                        let ws = git::worktree::recreate(&repo, &self.data_dir, &ws, &ready.oid)?;
                        persist_recreation(
                            store,
                            thread_id,
                            &ws,
                            &ready.oid,
                            &format!("{other:?}"),
                        )?;
                        WorkTarget {
                            workspace_id: ws.workspace_id,
                            path: ws.path,
                            branch: ws.branch,
                        }
                    }
                },
                None => {
                    let ws = git::worktree::create(&repo, &self.data_dir, &ready.oid, None)?;
                    WorkTarget {
                        workspace_id: ws.workspace_id,
                        path: ws.path,
                        branch: ws.branch,
                    }
                }
            }
        } else {
            match candidate_ws.clone() {
                Some(ws)
                    if matches!(
                        git::worktree::verify(&repo, &self.data_dir, &ws, &base_oid),
                        git::worktree::VerifyOutcome::Ok
                    ) =>
                {
                    WorkTarget {
                        workspace_id: ws.clone(),
                        path: git::worktree::path_for(&self.data_dir, &ws),
                        branch: crate::ids::workspace_branch(&ws),
                    }
                }
                _ => {
                    // Workspace nuevo (id fresco): privado del run, sin lease
                    // necesario (ningún otro proceso conoce su identidad).
                    let ws = git::worktree::create(&repo, &self.data_dir, &base_oid, None)?;
                    WorkTarget {
                        workspace_id: ws.workspace_id,
                        path: ws.path,
                        branch: ws.branch,
                    }
                }
            }
        };

        // Todo run adquiere el lease de SU workspace (también los recién
        // creados: otro proceso podría reutilizar el mismo workspace después).
        let needs_lease = lease_guard
            .as_ref()
            .map(|g| g.workspace_id != target.workspace_id)
            .unwrap_or(true);
        if needs_lease {
            // El guard viejo (y su receiver) se descartan juntos: un sender
            // huérfano dispararía la cancelación del turno nuevo.
            drop(lease_guard.take());
            match leases::try_acquire(
                &mut store.conn,
                &target.workspace_id,
                &pre_run_id,
                std::process::id() as i64,
                leases::ttl_ms(),
            )? {
                leases::AcquireOutcome::Busy { info, age_ms } => {
                    return Err(Error::conflict(format!(
                        "workspace {} en uso: el proceso dueño sigue vivo (run {}, pid {}, heartbeat hace {age_ms} ms); un TTL vencido no transfiere ownership; recuperación explícita: `aeril lease release {} [--force]`",
                        target.workspace_id, info.run_id, info.pid, target.workspace_id
                    )));
                }
                leases::AcquireOutcome::Acquired { took_over } => {
                    if let Some(t) = took_over {
                        events::append_typed(
                            &mut store.conn,
                            thread_id,
                            None,
                            events::K_WORKSPACE_LEASE_TAKEN_OVER,
                            "system",
                            &payload::WorkspaceLeaseTakenOver {
                                workspace_id: target.workspace_id.clone(),
                                previous_run_id: t.previous_run_id,
                                previous_pid: t.previous_pid,
                                age_ms: t.age_ms,
                                reason: t.reason.to_string(),
                            },
                        )?;
                    }
                    let (lost_tx, lost_rx) = oneshot::channel();
                    lease_guard = Some(LeaseGuard::start(
                        self.db_path.clone(),
                        target.workspace_id.clone(),
                        pre_run_id.clone(),
                        lost_tx,
                    ));
                    lease_lost_rx = Some(lost_rx);
                }
            }
        }

        // ── backend y run ────────────────────────────────────────────────────
        let spec = resolve_backend(&opts.backend)?;
        let run = runs::start(
            &mut store.conn,
            &StartRunSpec {
                id: Some(&pre_run_id),
                thread_id,
                backend: &spec.name,
                backend_version: &spec.version,
                goal: &opts.goal,
                workspace_id: &target.workspace_id,
                repo_path: &thread.repo_path,
                worktree_path: &target.path.to_string_lossy(),
                branch: &target.branch,
                base_oid: &base_oid,
                resumed_session: if opts.resume_session {
                    prev_session.as_deref()
                } else {
                    None
                },
            },
        )?;

        // Intención durable del feedback ANTES de lanzar el backend: si algo
        // falla después, el fallo es registrable y auditable.
        if let Some(ids) = &opts.feedback_comment_ids {
            let sent = payload::FeedbackSent {
                comment_ids: ids.clone(),
                run_id: run.id.clone(),
            };
            events::append_typed(
                &mut store.conn,
                thread_id,
                Some(&run.id),
                events::K_FEEDBACK_SENT,
                "human",
                &sent,
            )?;
        }

        let prompt = match (&opts.feedback_comment_ids, feedback) {
            (Some(ids), true) => compose_feedback_prompt(store, thread_id, ids)?,
            _ => opts.goal.clone(),
        };

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| Error::backend(format!("no se pudo crear el runtime: {e}")))?;

        let turn = rt.block_on(self.run_turn(
            store,
            &repo,
            &run,
            &spec,
            &target.path,
            &prompt,
            opts,
            prev_session.as_deref(),
            lease_lost_rx,
        ));
        drop(rt);

        // ── cierre del run ───────────────────────────────────────────────────
        // Pérdida de ownership: el run NUNCA reporta completitud normal y no
        // captura checkpoint (deja de ser dueño del workspace).
        let lease_lost = lease_guard.as_ref().map(|g| g.lost()).unwrap_or(false);
        let delivered = turn.feedback_delivered;
        let (mut state, mut detail, stop_reason) = match turn {
            TurnResult {
                stop_reason: Some(s),
                detail,
                ..
            } if s == "end_turn" => ("completed", detail, Some(s)),
            TurnResult {
                stop_reason: Some(s),
                detail,
                ..
            } if s == "cancelled" => ("cancelled", detail, Some(s)),
            TurnResult {
                stop_reason: Some(s),
                detail,
                ..
            } => (
                "failed",
                detail.or_else(|| Some(format!("stopReason: {s}"))),
                Some(s),
            ),
            TurnResult {
                stop_reason: None,
                detail,
                ..
            } => ("failed", detail, None),
        };
        if lease_lost {
            let ws = lease_guard
                .as_ref()
                .map(|g| g.workspace_id.clone())
                .unwrap_or_default();
            let _ = events::append_typed(
                &mut store.conn,
                thread_id,
                Some(&run.id),
                events::K_WORKSPACE_LEASE_LOST,
                "system",
                &payload::WorkspaceLeaseLost {
                    workspace_id: ws.clone(),
                    run_id: run.id.clone(),
                    reason: "ownership transferido a otro proceso durante el turno".into(),
                },
            );
            state = "failed";
            detail = Some(format!(
                "lease del workspace {ws} perdido durante el turno; run abortado sin completar (detalle: {})",
                detail.unwrap_or_else(|| "sin detalle".into())
            ));
        }

        // Feedback sin evidencia de procesamiento = fallo explícito (jamás se
        // declara entregado sin ack del backend).
        if delivered == Some(false)
            && let Some(ids) = &opts.feedback_comment_ids
        {
            let reason = detail
                .clone()
                .unwrap_or_else(|| "el turno no dio evidencia de procesamiento".into());
            let _ = events::append_typed(
                &mut store.conn,
                thread_id,
                Some(&run.id),
                events::K_FEEDBACK_FAILED,
                "system",
                &payload::FeedbackFailed {
                    comment_ids: ids.clone(),
                    run_id: Some(run.id.clone()),
                    reason,
                },
            );
        }

        let mut snapshot: Option<checkpoint::CaptureOutcome> = None;
        if state == "completed" {
            match checkpoint::capture(store, &repo, &run, "run completado") {
                Ok(Some(c)) => {
                    self.note(UiNote::Event {
                        thread_id: thread_id.to_string(),
                        kind: events::K_CHECKPOINT_READY.into(),
                    });
                    snapshot = Some(c);
                }
                Ok(None) => {}
                Err(e) => {
                    state = "failed";
                    detail = Some(format!("captura de checkpoint falló: {e}"));
                }
            }
        }

        runs::end(
            &mut store.conn,
            thread_id,
            &run.id,
            state,
            detail.as_deref(),
            stop_reason.as_deref(),
        )?;
        // Libera el lease explícitamente (además del Drop por early-returns).
        drop(lease_guard);
        self.note(UiNote::RunDone {
            thread_id: thread_id.to_string(),
            run_id: run.id.clone(),
            state: state.to_string(),
        });

        let engine_session_id =
            runs::get(&store.conn, thread_id, &run.id)?.and_then(|r| r.engine_session_id);
        Ok(RunSummary {
            thread_id: thread_id.to_string(),
            run_id: run.id,
            state: state.to_string(),
            stop_reason,
            engine_session_id,
            snapshot,
            detail,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_turn(
        &mut self,
        store: &mut Store,
        repo: &Repository,
        run: &RunRow,
        spec: &BackendSpec,
        worktree: &Path,
        prompt: &str,
        opts: &RunOptions,
        prev_session: Option<&str>,
        lease_lost_rx: Option<oneshot::Receiver<()>>,
    ) -> TurnResult {
        let _ = repo;
        let mut client = match AcpClient::spawn(spec, worktree).await {
            Ok(c) => c,
            Err(e) => {
                return TurnResult {
                    stop_reason: None,
                    detail: Some(format!("no se pudo lanzar el backend: {e}")),
                    feedback_delivered: opts.feedback_comment_ids.as_ref().map(|_| false),
                };
            }
        };
        if let Err(e) = client.initialize("aeril", env!("CARGO_PKG_VERSION")).await {
            let _ = client.kill().await;
            return TurnResult {
                stop_reason: None,
                detail: Some(format!("initialize falló: {e}")),
                feedback_delivered: opts.feedback_comment_ids.as_ref().map(|_| false),
            };
        }

        // Sesión del motor: el ID previo viene del RUN ANTERIOR (parámetro
        // explícito), no de la fila del run actual (que nace sin sesión).
        // Se registra qué se intentó y si el protocolo lo aceptó; la
        // restauración real del contexto no es verificable con Hermes hoy.
        let mut resumed = false;
        let mut attempted: &str = "new";
        let mut accepted = true;
        let session_id = match (opts.resume_session, prev_session) {
            (true, Some(sid)) => {
                attempted = "load";
                let mut obs = acpmod::NoopObserver;
                match client.session_load(sid, worktree, &mut obs).await {
                    Ok(true) => {
                        resumed = true;
                        sid.to_string()
                    }
                    accepted_load => {
                        accepted = false;
                        let _ = accepted_load;
                        match client
                            .session_new(worktree, &mut acpmod::NoopObserver)
                            .await
                        {
                            Ok(s) => s,
                            Err(e) => {
                                let _ = client.kill().await;
                                return TurnResult {
                                    stop_reason: None,
                                    detail: Some(format!("session/new falló: {e}")),
                                    feedback_delivered: opts
                                        .feedback_comment_ids
                                        .as_ref()
                                        .map(|_| false),
                                };
                            }
                        }
                    }
                }
            }
            _ => match client
                .session_new(worktree, &mut acpmod::NoopObserver)
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    let _ = client.kill().await;
                    return TurnResult {
                        stop_reason: None,
                        detail: Some(format!("session/new falló: {e}")),
                        feedback_delivered: opts.feedback_comment_ids.as_ref().map(|_| false),
                    };
                }
            },
        };
        if let Err(e) = runs::set_engine_session(
            &mut store.conn,
            &run.thread_id,
            &run.id,
            &session_id,
            resumed,
            attempted,
            accepted,
        ) {
            let _ = client.kill().await;
            return TurnResult {
                stop_reason: None,
                detail: Some(format!("no se pudo persistir la sesión del motor: {e}")),
                feedback_delivered: opts.feedback_comment_ids.as_ref().map(|_| false),
            };
        }
        // La sesión quedó establecida: el run está corriendo (si el proceso
        // muere a partir de aquí, la fila queda `running` = interrumpido).
        if let Err(e) = runs::set_state(&store.conn, &run.id, "running") {
            let _ = client.kill().await;
            return TurnResult {
                stop_reason: None,
                detail: Some(format!("no se pudo marcar el run como running: {e}")),
                feedback_delivered: opts.feedback_comment_ids.as_ref().map(|_| false),
            };
        }

        // Modelo explícito del run (si el usuario lo pidió).
        if let Some(model) = &opts.model
            && let Err(e) = client.session_set_model(model).await
        {
            let _ = client.kill().await;
            return TurnResult {
                stop_reason: None,
                detail: Some(format!("no se pudo fijar el modelo `{model}`: {e}")),
                feedback_delivered: opts.feedback_comment_ids.as_ref().map(|_| false),
            };
        }

        // Cancelación efectiva: la del usuario O la pérdida de lease.
        let effective_cancel = merge_cancels(self.cancel.take(), lease_lost_rx);
        let stop = {
            let mut observer = PersistingObserver {
                store,
                thread_id: run.thread_id.clone(),
                run_id: run.id.clone(),
                permissions: opts.permissions,
                ui: self.ui.clone(),
                current_message: None,
                feedback_pending: opts.feedback_comment_ids.clone().map(|ids| (ids, false)),
                turn_started: false,
                tools: HashMap::new(),
            };
            let res = client.prompt(prompt, &mut observer, effective_cancel).await;
            // El último mensaje acumulado del turno se persiste SIEMPRE,
            // aunque el turno haya fallado (es evidencia de lo que dijo el motor).
            let flush = observer.flush_message();
            // La entrega (o su ausencia) se decide aquí; el evento de fallo lo
            // escribe el llamador con el resultado final del run.
            let delivered = observer
                .feedback_pending
                .as_ref()
                .map(|(_, delivered)| *delivered);
            drop(observer);
            if let Err(e) = flush {
                return TurnResult {
                    stop_reason: None,
                    detail: Some(format!("no se pudo persistir el mensaje final: {e}")),
                    feedback_delivered: delivered,
                };
            }
            match res {
                Ok(c) if c.cancel_unconfirmed => TurnResult {
                    stop_reason: Some("cancelled".into()),
                    detail: Some(
                        "cancelación no confirmada por el backend; proceso terminado tras el plazo"
                            .into(),
                    ),
                    feedback_delivered: delivered,
                },
                Ok(c) => TurnResult {
                    stop_reason: Some(c.stop_reason),
                    detail: None,
                    feedback_delivered: delivered,
                },
                Err(e) => TurnResult {
                    stop_reason: None,
                    detail: Some(e.to_string()),
                    feedback_delivered: delivered,
                },
            }
        };
        let _ = client.kill().await;
        stop
    }
}

struct TurnResult {
    stop_reason: Option<String>,
    detail: Option<String>,
    /// `None` = este run no envía feedback; `Some(true)` = el motor dio
    /// evidencia de procesarlo; `Some(false)` = quedó sin ack.
    feedback_delivered: Option<bool>,
}

/// Guard del lease de workspace: renueva en un thread propio, ABORTA si
/// pierde ownership (señal al turno) y libera al soltarse (Drop).
///
/// Invariante RC2-B5: un fallo de renovación (0 filas o error del store) NO
/// permite seguir con ownership desconocido: se marca `lost`, se señala el
/// turno (que cancela el backend) y el run termina fallido.
struct LeaseGuard {
    db_path: PathBuf,
    workspace_id: String,
    run_id: String,
    lost: std::sync::Arc<std::sync::atomic::AtomicBool>,
    renewer: Option<std::thread::JoinHandle<()>>,
}

impl LeaseGuard {
    fn start(
        db_path: PathBuf,
        workspace_id: String,
        run_id: String,
        lost_tx: oneshot::Sender<()>,
    ) -> Self {
        use std::sync::atomic::{AtomicBool, Ordering};
        let lost = std::sync::Arc::new(AtomicBool::new(false));
        let lost_thread = lost.clone();
        let db = db_path.clone();
        let ws = workspace_id.clone();
        let rid = run_id.clone();
        let interval = crate::store::leases::renew_interval_secs();
        let renewer = std::thread::spawn(move || {
            let mut lost_tx = Some(lost_tx);
            let mut ticks: u64 = 0;
            loop {
                if lost_thread.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
                ticks += 1;
                if !ticks.is_multiple_of(interval) {
                    continue;
                }
                let outcome = match Store::open(&db) {
                    Ok(store) => crate::store::leases::renew(&store.conn, &ws, &rid),
                    Err(e) => Err(e),
                };
                match outcome {
                    Ok(true) => {}
                    Ok(false) => {
                        lost_thread.store(true, Ordering::SeqCst);
                        if let Some(tx) = lost_tx.take() {
                            let _ = tx.send(());
                        }
                        return;
                    }
                    Err(e) => {
                        // Error de renovación = ownership desconocido: abortar,
                        // nunca seguir a ciegas.
                        eprintln!(
                            "aeril: renovación de lease falló ({e}); abortando por ownership desconocido"
                        );
                        lost_thread.store(true, Ordering::SeqCst);
                        if let Some(tx) = lost_tx.take() {
                            let _ = tx.send(());
                        }
                        return;
                    }
                }
            }
        });
        LeaseGuard {
            db_path,
            workspace_id,
            run_id,
            lost,
            renewer: Some(renewer),
        }
    }

    fn lost(&self) -> bool {
        self.lost.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Combina hasta dos señales de cancelación en una sola (la primera que llegue).
fn merge_cancels(
    a: Option<oneshot::Receiver<()>>,
    b: Option<oneshot::Receiver<()>>,
) -> Option<oneshot::Receiver<()>> {
    match (a, b) {
        (None, None) => None,
        (Some(r), None) | (None, Some(r)) => Some(r),
        (Some(ra), Some(rb)) => {
            let (tx, rx) = oneshot::channel();
            tokio::spawn(async move {
                tokio::select! {
                    _ = ra => {}
                    _ = rb => {}
                }
                let _ = tx.send(());
            });
            Some(rx)
        }
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.lost.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Ok(store) = Store::open(&self.db_path) {
            let _ = leases::release(&store.conn, &self.workspace_id, &self.run_id);
        }
        if let Some(t) = self.renewer.take() {
            let _ = t.join();
        }
    }
}

fn persist_recreation(
    store: &mut Store,
    thread_id: &str,
    ws: &git::worktree::Workspace,
    from_oid: &str,
    reason: &str,
) -> Result<()> {
    let payload = payload::WorkspaceRecreated {
        workspace_id: ws.workspace_id.clone(),
        worktree_path: ws.path.to_string_lossy().into_owned(),
        from_oid: from_oid.to_string(),
        reason: reason.to_string(),
    };
    events::append_typed(
        &mut store.conn,
        thread_id,
        None,
        events::K_WORKSPACE_RECREATED,
        "system",
        &payload,
    )?;
    Ok(())
}

fn compose_feedback_prompt(
    store: &Store,
    thread_id: &str,
    comment_ids: &[String],
) -> Result<String> {
    let mut out = String::from(
        "Feedback del revisor (Aeril) sobre tu trabajo anterior en este directorio.\n\nComentarios:\n",
    );
    let repo_path = threads::get(&store.conn, thread_id)?
        .map(|t| t.repo_path)
        .unwrap_or_default();
    let repo = git::open(Path::new(&repo_path)).ok();
    for (i, cid) in comment_ids.iter().enumerate() {
        let view = crate::review::view_comment(store, thread_id, cid, repo.as_ref())?;
        out.push_str(&format!(
            "{}) {} líneas {}-{} (snapshot {}):\n   \"{}\"\n",
            i + 1,
            view.anchor.path,
            view.anchor.start_line,
            view.anchor.end_line,
            &view.anchor.snapshot_id[..8.min(view.anchor.snapshot_id.len())],
            view.text.replace('\n', " ")
        ));
    }
    // Contexto: objetivo original del run anterior.
    if let Ok(events) = events::fetch_by_kind(&store.conn, thread_id, events::K_RUN_STARTED)
        && let Some(goal) = events
            .last()
            .and_then(|ev| ev.parse::<payload::RunStarted>().ok())
            .map(|p| p.goal)
        && !goal.trim().is_empty()
    {
        out.push_str(&format!("\nObjetivo original del trabajo: {goal}\n"));
    }
    out.push_str("\nAplicá el feedback al código del directorio de trabajo y dejalo en un estado coherente. No deshagas cambios fuera de lo señalado.\n");
    Ok(out)
}

/// Observador que persiste eventos normalizados y aplica la política de permisos.
struct PersistingObserver<'a> {
    store: &'a mut Store,
    thread_id: String,
    run_id: String,
    permissions: PermissionPolicySetting,
    ui: Option<std_mpsc::Sender<UiNote>>,
    current_message: Option<(Option<String>, String)>,
    /// `(comment_ids, delivered)`: `delivered=true` SOLO con evidencia
    /// persistida de procesamiento del turno (nunca por updates de setup).
    feedback_pending: Option<(Vec<String>, bool)>,
    /// `true` una vez enviado el prompt: las notificaciones previas (restos de
    /// session/new o load) no cuentan como evidencia de este turno.
    turn_started: bool,
    /// `toolCallId → (título, último estado visto)`: los updates de Hermes no
    /// repiten el título, así que se arrastra desde el `tool_call` inicial.
    tools: HashMap<String, (String, String)>,
}

impl PersistingObserver<'_> {
    fn flush_message(&mut self) -> Result<()> {
        if let Some((mid, text)) = self.current_message.take()
            && !text.trim().is_empty()
        {
            let msg = payload::AgentMessage {
                text: truncate(&text, 200_000),
                message_id: mid,
            };
            events::append_typed(
                &mut self.store.conn,
                &self.thread_id,
                Some(&self.run_id),
                events::K_AGENT_MESSAGE,
                "agent",
                &msg,
            )?;
            if let Some(tx) = &self.ui {
                let _ = tx.send(UiNote::Event {
                    thread_id: self.thread_id.clone(),
                    kind: events::K_AGENT_MESSAGE.into(),
                });
            }
        }
        Ok(())
    }

    fn mark_delivered(&mut self) -> Result<()> {
        if let Some((ids, delivered)) = self.feedback_pending.as_mut()
            && !*delivered
        {
            let payload = payload::FeedbackDelivered {
                comment_ids: ids.clone(),
                run_id: self.run_id.clone(),
            };
            // Persistir PRIMERO; el flag en memoria se actualiza recién
            // cuando el evento durable existe.
            events::append_typed(
                &mut self.store.conn,
                &self.thread_id,
                Some(&self.run_id),
                events::K_FEEDBACK_DELIVERED,
                "system",
                &payload,
            )?;
            *delivered = true;
        }
        Ok(())
    }
}

impl TurnObserver for PersistingObserver<'_> {
    fn on_turn_started(&mut self) {
        self.turn_started = true;
    }

    fn on_update(&mut self, update: &Value) -> Result<()> {
        // Evidencia de entrega: SOLO notificaciones asociadas al turno
        // (mensajes/tool calls/plan) y SOLO después de enviar el prompt.
        // usage_update, available_commands_update y session_info_update
        // pueden llegar demoradas y no prueban procesamiento.
        if self.turn_started && acpmod::is_turn_evidence(update) {
            self.mark_delivered()?;
        }
        if let Some((label, text, mid)) = acpmod::chunk_text(update) {
            if label == "agent_message_chunk" {
                match &mut self.current_message {
                    Some((existing_mid, buf)) if *existing_mid == mid => buf.push_str(&text),
                    Some((_, _)) => {
                        self.flush_message()?;
                        self.current_message = Some((mid, text));
                    }
                    None => self.current_message = Some((mid, text)),
                }
            }
            return Ok(());
        }
        let kind = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or("");
        match kind {
            "tool_call" | "tool_call_update" => {
                self.flush_message()?;
                let tool_call_id = update
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let status = update
                    .get("status")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if kind == "tool_call_update" {
                    // Solo se registra el cierre (completed/failed) una vez.
                    let is_final = status.as_deref() == Some("completed")
                        || status.as_deref() == Some("failed");
                    if !is_final {
                        return Ok(());
                    }
                    if let Some((_, last)) = self.tools.get(&tool_call_id)
                        && Some(last) == status.as_ref()
                    {
                        return Ok(());
                    }
                }
                let title = update
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| self.tools.get(&tool_call_id).map(|(t, _)| t.clone()))
                    .unwrap_or_else(|| tool_call_id.clone());
                if let Some(s) = &status {
                    self.tools
                        .insert(tool_call_id.clone(), (title.clone(), s.clone()));
                }
                let tc = payload::AgentToolCall {
                    tool_call_id: tool_call_id.clone(),
                    title: truncate(&title, 300),
                    kind: update
                        .get("kind")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    status,
                };
                events::append_typed(
                    &mut self.store.conn,
                    &self.thread_id,
                    Some(&self.run_id),
                    events::K_AGENT_TOOL_CALL,
                    "agent",
                    &tc,
                )?;
                if let Some(tx) = &self.ui {
                    let _ = tx.send(UiNote::Event {
                        thread_id: self.thread_id.clone(),
                        kind: events::K_AGENT_TOOL_CALL.into(),
                    });
                }
                Ok(())
            }
            // plan / usage / session_info / available_commands: no persistidos en v0.
            _ => Ok(()),
        }
    }

    fn on_permission(&mut self, req: &PermissionRequest) -> Result<PermissionDecision> {
        let request_label = req.request_id.to_string();
        let requested = payload::PermissionRequested {
            request_id: request_label.clone(),
            tool_call_id: req.tool_call_id.clone(),
            title: truncate(&req.title, 300),
            options: req.options.iter().map(|o| o.option_id.clone()).collect(),
            diff_path: req.diff_path.clone(),
        };
        // 1. Persistir el pedido ANTES de evaluarlo. Sin auditoría durable no
        //    se decide (fail closed).
        if let Err(e) = events::append_typed(
            &mut self.store.conn,
            &self.thread_id,
            Some(&self.run_id),
            events::K_PERMISSION_REQUESTED,
            "agent",
            &requested,
        ) {
            self.record_audit_failure(&request_label, "requested", &e.to_string());
            return Err(Error::Other(format!(
                "no se pudo auditar permission.requested: {e}"
            )));
        }

        // 2. Evaluar la política.
        let decision = match self.permissions {
            PermissionPolicySetting::Auto => {
                let chosen = req
                    .options
                    .iter()
                    .find(|o| o.kind == "allow_once")
                    .or_else(|| req.options.iter().find(|o| o.kind.starts_with("allow")));
                match chosen {
                    Some(opt) => PermissionDecision::Selected {
                        option_id: opt.option_id.clone(),
                        policy: "auto_allow_once".into(),
                    },
                    None => PermissionDecision::Cancelled {
                        policy: "auto_sin_opcion_permitida".into(),
                    },
                }
            }
            PermissionPolicySetting::Deny => PermissionDecision::Cancelled {
                policy: "deny_policy".into(),
            },
        };

        // 3. Persistir la DECISIÓN durable ANTES de responderla. Si esto
        //    falla, la decisión no se transmite (fail closed).
        let (option_or_cancel, policy) = match &decision {
            PermissionDecision::Selected { option_id, policy } => {
                (option_id.clone(), policy.clone())
            }
            PermissionDecision::Cancelled { policy } => ("cancelled".to_string(), policy.clone()),
        };
        let responded = payload::PermissionResponded {
            request_id: request_label.clone(),
            option_id: option_or_cancel,
            policy,
        };
        if let Err(e) = events::append_typed(
            &mut self.store.conn,
            &self.thread_id,
            Some(&self.run_id),
            events::K_PERMISSION_RESPONDED,
            "system",
            &responded,
        ) {
            self.record_audit_failure(&request_label, "responded", &e.to_string());
            return Err(Error::Other(format!(
                "no se pudo auditar permission.responded: {e}"
            )));
        }
        Ok(decision)
    }

    fn on_permission_sent(
        &mut self,
        req: &PermissionRequest,
        decision: &PermissionDecision,
        ok: bool,
    ) {
        let option_id = match decision {
            PermissionDecision::Selected { option_id, .. } => option_id.clone(),
            PermissionDecision::Cancelled { .. } => "cancelled".to_string(),
        };
        let transmitted = payload::PermissionTransmitted {
            request_id: req.request_id.to_string(),
            option_id,
            ok,
        };
        if let Err(e) = events::append_typed(
            &mut self.store.conn,
            &self.thread_id,
            Some(&self.run_id),
            events::K_PERMISSION_TRANSMITTED,
            "system",
            &transmitted,
        ) {
            // El resultado de la transmisión es best-effort: si no se puede
            // registrar, la decisión durable sigue siendo la fuente de verdad
            // y la falla de envío en sí ya habría terminado el turno.
            eprintln!("aeril: no se pudo registrar permission.transmitted: {e}");
        }
    }

    fn on_permission_failed(&mut self, req: &PermissionRequest, detail: &str) {
        // El evento durable de fallo de auditoría lo escribe `on_permission`
        // con la etapa precisa (requested/responded); este hook solo deja
        // rastro local del denegado fail-closed.
        eprintln!(
            "aeril: permiso {} denegado fail-closed: {detail}",
            req.request_id
        );
    }
}

impl PersistingObserver<'_> {
    /// Best-effort: puede fallar también (es el mismo store que acaba de
    /// fallar); el error del turno es la señal principal.
    fn record_audit_failure(&mut self, request_id: &str, stage: &str, detail: &str) {
        let failed = payload::PermissionAuditFailed {
            request_id: request_id.to_string(),
            stage: stage.to_string(),
            detail: truncate(detail, 500),
        };
        let _ = events::append_typed(
            &mut self.store.conn,
            &self.thread_id,
            Some(&self.run_id),
            events::K_PERMISSION_AUDIT_FAILED,
            "system",
            &failed,
        );
    }
}

// ── integración fast-forward ─────────────────────────────────────────────────

pub fn integrate(
    store: &mut Store,
    thread_id: &str,
    snapshot_id: &str,
    target_branch: &str,
    expect_oid: Option<&str>,
) -> Result<git::integrate::IntegrationOutcome> {
    // Antes de una operación nueva, reconciliar intentos interrumpidos de
    // este u otros threads (nunca se fabrica éxito sin evidencia de la ref).
    reconcile_integrations(store)?;
    let thread = threads::get(&store.conn, thread_id)?
        .ok_or_else(|| Error::not_found(format!("thread {thread_id}")))?;
    let (_, ready) = checkpoint::ready_for_snapshot(store, thread_id, snapshot_id)?
        .ok_or_else(|| Error::not_ready(format!("no hay checkpoint ready {snapshot_id}")))?;
    let repo = git::open(Path::new(&thread.repo_path))?;
    let expect = expect_oid.unwrap_or(&ready.base_oid).to_string();

    // Intención durable ANTES de tocar la ref: si el proceso muere entre
    // medio, la reconciliación al reabrir decide con evidencia.
    events::append_typed(
        &mut store.conn,
        thread_id,
        None,
        events::K_INTEGRATION_REQUESTED,
        "system",
        &payload::IntegrationRequested {
            target_branch: target_branch.to_string(),
            source_oid: ready.oid.clone(),
            target_oid_expected: expect.clone(),
        },
    )?;
    crash::point("integrate_intent");

    match git::integrate::fast_forward(&repo, target_branch, &ready.oid, &expect) {
        Ok(outcome) => {
            crash::point("integrate_committed");
            events::append_typed(
                &mut store.conn,
                thread_id,
                None,
                events::K_INTEGRATION_COMPLETED,
                "system",
                &payload::IntegrationCompleted {
                    target_branch: target_branch.to_string(),
                    source_oid: ready.oid.clone(),
                    target_oid_before: outcome.target_oid_before.clone(),
                    target_oid_after: outcome.target_oid_after.clone(),
                    recovered: false,
                },
            )?;
            Ok(outcome)
        }
        Err(blocked) => {
            let current = match &blocked {
                git::integrate::IntegrationBlocked::Stale { actual, .. } => Some(actual.clone()),
                _ => None,
            };
            events::append_typed(
                &mut store.conn,
                thread_id,
                None,
                events::K_INTEGRATION_BLOCKED,
                "system",
                &payload::IntegrationBlocked {
                    target_branch: target_branch.to_string(),
                    source_oid: ready.oid.clone(),
                    target_oid_current: current,
                    reason: blocked.describe(),
                },
            )?;
            Err(Error::conflict(blocked.describe()))
        }
    }
}

/// Resultado de reconciliar un intento de integración interrumpido.
#[derive(Debug)]
pub enum IntegrationReconcile {
    /// La ref quedó en la fuente: la integración SÍ ocurrió (evidencia).
    CompletedRecovered {
        target_branch: String,
        source_oid: String,
    },
    /// La ref quedó en la base esperada: el intento no se aplicó (abandonado).
    Abandoned {
        target_branch: String,
        source_oid: String,
        reason: String,
    },
}

/// Reconcilia `integration.requested` sin resolución. Idempotente frente a
/// reconciliaciones concurrentes (el append re-chequea dentro de la tx).
/// Nunca mueve refs: solo registra lo que la ref demuestra.
pub fn reconcile_integrations(store: &mut Store) -> Result<Vec<IntegrationReconcile>> {
    #[derive(Debug)]
    struct Pending {
        seq: i64,
        thread_id: String,
        repo_path: String,
        requested: payload::IntegrationRequested,
    }
    let pending: Vec<Pending> = {
        let mut stmt = store.conn.prepare(
        "SELECT r.seq, r.thread_id, r.payload, t.repo_path
         FROM events r JOIN threads t ON t.id = r.thread_id
         WHERE r.kind = ?1
           AND NOT EXISTS (
             SELECT 1 FROM events s
             WHERE s.thread_id = r.thread_id AND s.seq > r.seq
               AND s.kind IN (?2, ?3)
               AND json_extract(s.payload, '$.target_branch') = json_extract(r.payload, '$.target_branch')
               AND json_extract(s.payload, '$.source_oid') = json_extract(r.payload, '$.source_oid')
           )
         ORDER BY r.seq ASC",
    )?;
        let rows = stmt.query_map(
            rusqlite::params![
                events::K_INTEGRATION_REQUESTED,
                events::K_INTEGRATION_COMPLETED,
                events::K_INTEGRATION_BLOCKED
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )?;
        let mut pending = Vec::new();
        for r in rows {
            let (seq, thread_id, payload_text, repo_path) = r?;
            let requested: payload::IntegrationRequested = serde_json::from_str(&payload_text)
                .map_err(|e| Error::integrity(format!("integration.requested ilegible: {e}")))?;
            pending.push(Pending {
                seq,
                thread_id,
                repo_path,
                requested,
            });
        }
        pending
    };

    let mut outcomes = Vec::new();
    for p in pending {
        let Some(repo) = git::open(Path::new(&p.repo_path)).ok() else {
            continue; // repo no disponible: sigue pendiente, sin inventar nada
        };
        let full_ref = format!("refs/heads/{}", p.requested.target_branch);
        let current = repo
            .find_reference(&full_ref)
            .ok()
            .and_then(|r| r.target())
            .map(|o| o.to_string());

        let (kind, value, outcome) = match current.as_deref() {
            Some(cur) if cur == p.requested.source_oid => {
                let ev = payload::IntegrationCompleted {
                    target_branch: p.requested.target_branch.clone(),
                    source_oid: p.requested.source_oid.clone(),
                    target_oid_before: p.requested.target_oid_expected.clone(),
                    target_oid_after: p.requested.source_oid.clone(),
                    recovered: true,
                };
                let outcome = IntegrationReconcile::CompletedRecovered {
                    target_branch: p.requested.target_branch.clone(),
                    source_oid: p.requested.source_oid.clone(),
                };
                (
                    events::K_INTEGRATION_COMPLETED,
                    serde_json::to_value(ev)?,
                    outcome,
                )
            }
            Some(cur) if cur == p.requested.target_oid_expected => {
                let reason =
                    "intento abandonado: la ref no cambió (crash antes de aplicar)".to_string();
                let ev = payload::IntegrationBlocked {
                    target_branch: p.requested.target_branch.clone(),
                    source_oid: p.requested.source_oid.clone(),
                    target_oid_current: Some(cur.to_string()),
                    reason: reason.clone(),
                };
                let outcome = IntegrationReconcile::Abandoned {
                    target_branch: p.requested.target_branch.clone(),
                    source_oid: p.requested.source_oid.clone(),
                    reason,
                };
                (
                    events::K_INTEGRATION_BLOCKED,
                    serde_json::to_value(ev)?,
                    outcome,
                )
            }
            other => {
                let reason = format!(
                    "la ref fue movida por otro escritor tras la interrupción (actual: {})",
                    other.unwrap_or("ausente")
                );
                let ev = payload::IntegrationBlocked {
                    target_branch: p.requested.target_branch.clone(),
                    source_oid: p.requested.source_oid.clone(),
                    target_oid_current: other.map(str::to_string),
                    reason: reason.clone(),
                };
                let outcome = IntegrationReconcile::Abandoned {
                    target_branch: p.requested.target_branch.clone(),
                    source_oid: p.requested.source_oid.clone(),
                    reason,
                };
                (
                    events::K_INTEGRATION_BLOCKED,
                    serde_json::to_value(ev)?,
                    outcome,
                )
            }
        };

        // Idempotente: re-chequear dentro de la transacción.
        let tx = store
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let already: i64 = tx.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM events s
                WHERE s.thread_id = ?1 AND s.seq > ?2
                  AND s.kind IN (?3, ?4)
                  AND json_extract(s.payload, '$.target_branch') = ?5
                  AND json_extract(s.payload, '$.source_oid') = ?6
            )",
            rusqlite::params![
                p.thread_id,
                p.seq,
                events::K_INTEGRATION_COMPLETED,
                events::K_INTEGRATION_BLOCKED,
                p.requested.target_branch,
                p.requested.source_oid
            ],
            |row| row.get(0),
        )?;
        if already == 1 {
            tx.rollback()?;
            continue;
        }
        events::append_tx(&tx, &p.thread_id, None, kind, "system", &value, now_ms())?;
        tx.commit()?;
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

// ── validación sobre un checkout aislado del snapshot ───────────────────────

/// Checkout efímero de validación: rama + worktree propios de Aeril, creados
/// desde el OID EXACTO del snapshot. Se limpia siempre (Drop, best-effort).
///
/// NO es un sandbox de seguridad (MANIFIESTO §8): aísla el CONTENIDO que se
/// valida de escritores coordinados por Aeril (el worktree del agente), pero
/// no restringe procesos externos con los permisos del usuario.
struct ValidationCheckout<'a> {
    repo: &'a Repository,
    name: String,
    path: PathBuf,
    branch: String,
}

impl Drop for ValidationCheckout<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        let admin = self.repo.commondir().join("worktrees").join(&self.name);
        let _ = std::fs::remove_dir_all(&admin);
        if let Ok(mut b) = self.repo.find_branch(&self.branch, git2::BranchType::Local) {
            let _ = b.delete();
        }
    }
}

struct StreamCapture {
    tail: Vec<u8>,
    bytes: u64,
    hasher: crate::util::Fnv1a,
}

impl StreamCapture {
    fn new() -> Self {
        StreamCapture {
            tail: Vec::new(),
            bytes: 0,
            hasher: crate::util::Fnv1a::new(),
        }
    }
}

/// Hash de TODOS los bytes a medida que llegan; en memoria solo una cola acotada.
fn read_stream(mut r: impl std::io::Read) -> StreamCapture {
    let mut cap = StreamCapture::new();
    let mut chunk = [0u8; 16384];
    loop {
        match r.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let b = &chunk[..n];
                cap.bytes += n as u64;
                cap.hasher.update(b);
                cap.tail.extend_from_slice(b);
                if cap.tail.len() > 8192 {
                    let cut = cap.tail.len() - 8192;
                    cap.tail.drain(..cut);
                }
            }
        }
    }
    cap
}

/// Valida un snapshot en un checkout aislado creado desde el OID exacto.
/// El worktree del agente no participa: puede estar sucio, modificado en
/// paralelo o borrado, y la validación sigue siendo del contenido revisado.
pub fn validate(
    store: &mut Store,
    data_dir: &Path,
    thread_id: &str,
    snapshot_id: &str,
    command: &[String],
) -> Result<payload::ValidationRecorded> {
    if command.is_empty() {
        return Err(Error::invalid("validación sin comando"));
    }
    let thread = threads::get(&store.conn, thread_id)?
        .ok_or_else(|| Error::not_found(format!("thread {thread_id}")))?;
    let (_, ready) = checkpoint::ready_for_snapshot(store, thread_id, snapshot_id)?
        .ok_or_else(|| Error::not_ready(format!("no hay checkpoint ready {snapshot_id}")))?;
    let repo = git::open(Path::new(&thread.repo_path))?;
    let snap_commit = repo.find_commit(git2::Oid::from_str(&ready.oid)?)?;
    let snap_tree = snap_commit.tree()?.id().to_string();

    let validation_id = new_id();
    let wt_id = format!("val-{}", &validation_id[..12]);
    let wt_name = format!("aeril-{wt_id}");
    let branch = format!("aeril/validation/{validation_id}");
    let path = data_dir.join("validations").join(&validation_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = repo.branch(&branch, &snap_commit, false)?;
    let branch_ref = repo.find_reference(&format!("refs/heads/{branch}"))?;
    let mut add_opts = git2::WorktreeAddOptions::new();
    add_opts.reference(Some(&branch_ref));
    #[allow(clippy::unnecessary_mut_passed)] // la API exige Option<&mut _>
    repo.worktree(&wt_name, &path, Some(&mut add_opts))?;
    let checkout = ValidationCheckout {
        repo: &repo,
        name: wt_name,
        path: path.clone(),
        branch,
    };

    // Contenido verificado ANTES de ejecutar: el checkout debe ser el árbol
    // exacto del snapshot (si no, es integridad, no validación).
    let wt_repo = git::worktree::open_worktree(&repo, &wt_id)?;
    let tree_before = git::snapshot::current_tree(&wt_repo, &ready.oid)?;
    let tree_before_ok = tree_before == snap_tree;
    if !tree_before_ok {
        return Err(Error::integrity(format!(
            "el checkout de validación no reproduce el tree del snapshot ({tree_before} ≠ {snap_tree})"
        )));
    }

    let started = std::time::Instant::now();
    let mut child = std::process::Command::new(&command[0])
        .args(&command[1..])
        .current_dir(&checkout.path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| Error::invalid(format!("no se pudo ejecutar `{}`: {e}", command[0])))?;
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let out_handle = std::thread::spawn(move || read_stream(stdout));
    let err_handle = std::thread::spawn(move || read_stream(stderr));
    let status = child.wait()?;
    let out = out_handle.join().unwrap_or_else(|_| StreamCapture::new());
    let err = err_handle.join().unwrap_or_else(|_| StreamCapture::new());
    let duration_ms = started.elapsed().as_millis() as i64;

    let stdout_hash = out.hasher.hex();
    let stderr_hash = err.hasher.hex();
    let output_hash = fnv1a_hex(format!("{stdout_hash}:{stderr_hash}").as_bytes());
    let combined_tail = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.tail),
        String::from_utf8_lossy(&err.tail)
    );

    // Contenido verificado DESPUÉS: ¿el comando modificó el contenido
    // trackeado del snapshot? Se registra como observación de integridad.
    let tree_after = git::snapshot::current_tree(&wt_repo, &ready.oid).unwrap_or_default();
    let tree_after_unchanged = tree_after == snap_tree;

    let record = payload::ValidationRecorded {
        snapshot_id: snapshot_id.to_string(),
        oid: ready.oid.clone(),
        command: truncate(&command.join(" "), 500),
        exit_code: status.code().unwrap_or(-1),
        duration_ms,
        output_hash,
        output_tail: truncate(&combined_tail, 8000),
        stdout_bytes: out.bytes,
        stderr_bytes: err.bytes,
        stdout_hash,
        stderr_hash,
        tree_before_ok,
        tree_after_unchanged,
        checkout_id: validation_id.clone(),
    };
    events::append_typed(
        &mut store.conn,
        thread_id,
        None,
        events::K_VALIDATION_RECORDED,
        "system",
        &record,
    )?;
    drop(checkout); // limpieza explícita (además del Drop)
    Ok(record)
}
