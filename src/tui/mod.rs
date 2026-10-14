//! TUI mínima del ensayo: lista de threads, timeline de eventos, patch del
//! checkpoint seleccionado, comentar, feedback, integración y cancelación.
//! No es el multiplexor de paneles: es la superficie para probar el corte
//! vertical (Fase 0).

use crate::bench;
use crate::checkpoint;
use crate::error::{Error, Result};
use crate::git;
use crate::review;
use crate::session::{self, PermissionPolicySetting, RunOptions, Runner, UiNote};
use crate::store::Store;
use crate::store::events::{self, EventRecord};
use crate::store::threads::{self, ThreadRow};
use crate::timeutil::format_relative;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

pub struct ThreadDetail {
    pub thread_id: String,
    pub title: String,
    pub repo_path: String,
    pub events: Vec<EventRecord>,
    pub sel: usize,
    pub comments: Vec<review::CommentSummary>,
    pub patch: Vec<String>,
    pub patch_scroll: u16,
}

pub enum Mode {
    Normal,
    CommentInput {
        buf: String,
        anchor_desc: String,
        checkpoint_event_id: String,
        path: String,
        start: u64,
        end: u64,
    },
    BranchInput {
        buf: String,
        snapshot_id: String,
    },
    /// S3: captura del objetivo de un run nuevo en el thread abierto.
    RunInput {
        buf: String,
    },
    Help,
}

/// S3: un run supervisado por esta TUI. El slot nace al lanzar y muere SOLO
/// con la nota terminal de esa invocación (`RunDone`/`Error`): pedir
/// cancelación no libera capacidad (el run puede seguir vivo hasta el ack).
pub struct RunSlot {
    pub invocation: String,
    pub thread_id: String,
    pub run_id: Option<String>,
    pub cancel_requested: bool,
    /// El envío consume el canal (oneshot): se guarda como `Option` para
    /// poder dispararlo una sola vez.
    cancel: Option<oneshot::Sender<()>>,
}

/// Máximo de runs supervisados por instancia de TUI (límite de producto N=2
/// de Fase 1; NO es un límite global del sistema operativo).
pub const MAX_SUPERVISED_RUNS: usize = 2;

fn is_hermes_backend(backend: &str) -> bool {
    backend == "hermes" || backend.starts_with("hermes:")
}

pub struct App {
    pub store: Store,
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub threads: Vec<ThreadRow>,
    pub thread_sel: usize,
    pub detail: Option<ThreadDetail>,
    pub mode: Mode,
    pub status: String,
    pub recovery_note: Option<String>,
    pub ui_rx: mpsc::Receiver<UiNote>,
    pub ui_tx: mpsc::Sender<UiNote>,
    pub runs: Vec<RunSlot>,
    pub backend: String,
    pub should_quit: bool,
    pub t0: Instant,
    pub first_draw_done: bool,
    pub input_processed: bool,
    pub thread_opened_beacon: bool,
}

impl App {
    fn reload_threads(&mut self) -> Result<()> {
        // S3: preservar la selección por IDENTIDAD. Con runs concurrentes el
        // orden de la lista cambia (updated_ms) y un índice fijo apuntaría a
        // otro thread al recargar.
        let prev_sel_id = self
            .detail
            .as_ref()
            .map(|d| d.thread_id.clone())
            .or_else(|| self.threads.get(self.thread_sel).map(|t| t.id.clone()));
        self.threads = threads::list_projection(&self.store.conn, 200)?;
        if let Some(id) = prev_sel_id
            && let Some(idx) = self.threads.iter().position(|t| t.id == id)
        {
            self.thread_sel = idx;
        }
        if self.thread_sel >= self.threads.len() {
            self.thread_sel = self.threads.len().saturating_sub(1);
        }
        Ok(())
    }

    fn slot_index_by_invocation(&self, invocation: &str) -> Option<usize> {
        self.runs.iter().position(|s| s.invocation == invocation)
    }

    fn slot_index_for_thread(&self, thread_id: &str) -> Option<usize> {
        self.runs.iter().position(|s| s.thread_id == thread_id)
    }

    /// Thread "seleccionado": el abierto en detalle o el resaltado en la lista.
    fn selected_thread_id(&self) -> Option<String> {
        self.detail
            .as_ref()
            .map(|d| d.thread_id.clone())
            .or_else(|| self.threads.get(self.thread_sel).map(|t| t.id.clone()))
    }

    /// Gate S3 de capacidad/ownership. Err trae el motivo visible al usuario.
    /// - máximo 2 runs supervisados (activos o cancelando);
    /// - máximo 1 por thread hasta el ack terminal;
    /// - Hermes real: 1 por vez (sesiones ACP concurrentes aún no certificadas).
    fn can_start(&self, thread_id: &str, backend: &str) -> std::result::Result<(), String> {
        if let Some(slot) = self.runs.iter().find(|s| s.thread_id == thread_id) {
            return Err(format!(
                "el thread ya tiene un run {} (se libera con el ack terminal)",
                if slot.cancel_requested {
                    "cancelando"
                } else {
                    "activo"
                }
            ));
        }
        if self.runs.len() >= MAX_SUPERVISED_RUNS {
            return Err(format!(
                "capacidad máxima: {MAX_SUPERVISED_RUNS} runs supervisados a la vez"
            ));
        }
        if is_hermes_backend(backend) && !self.runs.is_empty() {
            return Err(
                "Hermes real: un run por vez (sesiones ACP concurrentes sin certificar); usá mock para concurrencia"
                    .into(),
            );
        }
        Ok(())
    }

    fn open_thread(&mut self, idx: usize) -> Result<()> {
        if idx >= self.threads.len() {
            return Ok(());
        }
        let t = self.threads[idx].clone();
        let detail = ThreadDetail {
            thread_id: t.id.clone(),
            title: t.title.clone(),
            repo_path: t.repo_path.clone(),
            events: Vec::new(),
            sel: 0,
            comments: Vec::new(),
            patch: Vec::new(),
            patch_scroll: 0,
        };
        self.detail = Some(detail);
        self.refresh_detail()?;
        Ok(())
    }

    fn refresh_detail(&mut self) -> Result<()> {
        let Some(detail) = self.detail.as_mut() else {
            return Ok(());
        };
        detail.events = events::fetch_tail(&self.store.conn, &detail.thread_id, 100)?;
        // Resúmenes livianos (sin patches): la memoria de revisión queda
        // acotada; el patch se carga SOLO para el evento seleccionado.
        detail.comments = review::list_comment_summaries(&self.store, &detail.thread_id, 200, 0)?;
        if detail.sel >= detail.events.len() {
            detail.sel = detail.events.len().saturating_sub(1);
        }
        // Patch del evento seleccionado.
        detail.patch.clear();
        detail.patch_scroll = 0;
        if let Some(ev) = detail.events.get(detail.sel).cloned() {
            if ev.kind == events::K_CHECKPOINT_READY {
                if let Ok(p) = ev.parse::<events::payload::CheckpointReady>() {
                    detail.patch = p.patch.lines().map(str::to_string).collect();
                }
            } else if ev.kind == events::K_COMMENT_ADDED
                && detail.comments.iter().any(|c| c.comment_id == ev.id)
            {
                // El patch del comentario se carga SOLO para el seleccionado.
                let repo = git::open(Path::new(&detail.repo_path)).ok();
                if let Ok(view) =
                    review::view_comment(&self.store, &detail.thread_id, &ev.id, repo.as_ref())
                {
                    detail.patch = view.original_patch.lines().map(str::to_string).collect();
                }
            }
        }
        Ok(())
    }

    fn selected_event(&self) -> Option<EventRecord> {
        self.detail
            .as_ref()
            .and_then(|d| d.events.get(d.sel))
            .cloned()
    }

    fn start_comment(&mut self) -> Result<()> {
        let Some(ev) = self.selected_event() else {
            self.status = "seleccioná un checkpoint listo (📌) para comentar".into();
            return Ok(());
        };
        if ev.kind != events::K_CHECKPOINT_READY {
            self.status = "solo se comenta sobre eventos `checkpoint.ready` (📌)".into();
            return Ok(());
        }
        let ready: events::payload::CheckpointReady = ev.parse()?;
        // Ancla por defecto: primer archivo con lado nuevo y su primer hunk.
        let mut anchor: Option<(String, u64, u64)> = None;
        for f in &ready.files {
            if f.after_blob_oid.is_none() {
                continue;
            }
            let repo = git::open(Path::new(
                &self
                    .detail
                    .as_ref()
                    .map(|d| d.repo_path.clone())
                    .unwrap_or_default(),
            ))
            .ok();
            let line_count = repo
                .as_ref()
                .and_then(|r| git::diff::text_at(r, &ready.oid, &f.path).ok().flatten())
                .map(|t| t.lines().count() as u64)
                .unwrap_or(1);
            let (s, e) = git::diff::first_hunk_range(&ready.patch, &f.path)
                .unwrap_or((1, line_count.max(1)));
            anchor = Some((f.path.clone(), s, e));
            break;
        }
        let Some((path, start, end)) = anchor else {
            self.status = "el checkpoint no tiene archivos anclables (lado nuevo)".into();
            return Ok(());
        };
        let desc = format!(
            "{path} líneas {start}-{end} · snapshot {}",
            &ready.snapshot_id[..8]
        );
        self.mode = Mode::CommentInput {
            buf: String::new(),
            anchor_desc: desc,
            checkpoint_event_id: ev.id.clone(),
            path,
            start,
            end,
        };
        Ok(())
    }

    fn submit_comment(&mut self) -> Result<()> {
        let Mode::CommentInput {
            buf,
            checkpoint_event_id,
            path,
            start,
            end,
            ..
        } = std::mem::replace(&mut self.mode, Mode::Normal)
        else {
            return Ok(());
        };
        let Some(detail) = self.detail.as_ref() else {
            return Ok(());
        };
        let thread_id = detail.thread_id.clone();
        let repo_path = detail.repo_path.clone();
        if buf.trim().is_empty() {
            self.status = "comentario vacío: descartado".into();
            return Ok(());
        }
        let repo = git::open(Path::new(&repo_path))?;
        match review::add_comment(
            &mut self.store,
            &repo,
            &thread_id,
            &checkpoint_event_id,
            &path,
            start,
            end,
            buf.trim(),
        ) {
            Ok(id) => {
                self.status = format!(
                    "comentario {} agregado (🚩 abierto, f para enviar feedback)",
                    &id[..8]
                );
                self.refresh_detail()?;
            }
            Err(e) => {
                self.status = format!("no se pudo comentar: {e}");
            }
        }
        Ok(())
    }

    fn send_feedback(&mut self) -> Result<()> {
        let Some(detail) = self.detail.as_ref() else {
            return Ok(());
        };
        let thread_id = detail.thread_id.clone();
        if let Err(reason) = self.can_start(&thread_id, &self.backend) {
            self.status = format!("feedback no lanzado: {reason}");
            return Ok(());
        }
        let ids = review::open_comment_ids(&self.store, &thread_id)?;
        if ids.is_empty() {
            self.status = "no hay comentarios abiertos para enviar".into();
            return Ok(());
        }
        self.spawn_run(
            &thread_id,
            RunOptions {
                backend: self.backend.clone(),
                goal: "Feedback del revisor".into(),
                permissions: PermissionPolicySetting::Auto,
                feedback_comment_ids: Some(ids.clone()),
                resume_session: true,
                model: None,
            },
            &format!("enviando {} comentario(s) al agente…", ids.len()),
        );
        Ok(())
    }

    fn start_integrate(&mut self) -> Result<()> {
        let Some(detail) = self.detail.as_ref() else {
            return Ok(());
        };
        let thread_id = detail.thread_id.clone();
        match checkpoint::latest_ready(&self.store, &thread_id)? {
            Some((_, ready)) => {
                self.mode = Mode::BranchInput {
                    buf: "main".into(),
                    snapshot_id: ready.snapshot_id,
                };
                Ok(())
            }
            None => {
                self.status = "no hay checkpoints listos para integrar".into();
                Ok(())
            }
        }
    }

    fn submit_integrate(&mut self) -> Result<()> {
        let Mode::BranchInput { buf, snapshot_id } =
            std::mem::replace(&mut self.mode, Mode::Normal)
        else {
            return Ok(());
        };
        let Some(detail) = self.detail.as_ref() else {
            return Ok(());
        };
        let thread_id = detail.thread_id.clone();
        let target = buf.trim().to_string();
        if target.is_empty() {
            self.status = "rama destino vacía".into();
            return Ok(());
        }
        match session::integrate(&mut self.store, &thread_id, &snapshot_id, &target, None) {
            Ok(outcome) => {
                self.status = format!(
                    "integrado fast-forward a {target}: {} → {}",
                    &outcome.target_oid_before[..8],
                    &outcome.target_oid_after[..8]
                );
            }
            Err(e) => {
                self.status = format!("integración bloqueada: {e}");
            }
        }
        self.refresh_detail()?;
        Ok(())
    }

    fn start_run_prompt(&mut self) -> Result<()> {
        if self.detail.is_none() {
            self.status = "abrí un thread (Enter) para lanzar un run".into();
            return Ok(());
        }
        self.mode = Mode::RunInput { buf: String::new() };
        self.status = "nuevo run: escribí el objetivo · Enter lanza · Esc cancela".into();
        Ok(())
    }

    fn submit_run(&mut self) -> Result<()> {
        let Mode::RunInput { buf } = std::mem::replace(&mut self.mode, Mode::Normal) else {
            return Ok(());
        };
        let goal = buf.trim().to_string();
        if goal.is_empty() {
            self.status = "objetivo vacío: run no lanzado".into();
            return Ok(());
        }
        let Some(detail) = self.detail.as_ref() else {
            self.status = "abrí un thread (Enter) para lanzar un run".into();
            return Ok(());
        };
        let thread_id = detail.thread_id.clone();
        // Gate revalidado al enviar (y otra vez al lanzar): el estado pudo
        // cambiar mientras se escribía el objetivo.
        if let Err(reason) = self.can_start(&thread_id, &self.backend) {
            self.status = format!("no se lanzó: {reason}");
            return Ok(());
        }
        self.spawn_run(
            &thread_id,
            RunOptions {
                backend: self.backend.clone(),
                goal: goal.clone(),
                permissions: PermissionPolicySetting::Auto,
                feedback_comment_ids: None,
                resume_session: true,
                model: None,
            },
            &format!(
                "▶ run lanzado · {} · thread {}",
                truncate_chars(&goal, 40),
                &thread_id[..6.min(thread_id.len())]
            ),
        );
        Ok(())
    }

    /// Lanza un run supervisado. El gate se valida ANTES de crear estado; el
    /// slot queda ocupado hasta la nota terminal de ESTA invocación (S3).
    fn spawn_run(&mut self, thread_id: &str, opts: RunOptions, status: &str) -> bool {
        if let Err(reason) = self.can_start(thread_id, &opts.backend) {
            self.status = format!("no se lanzó: {reason}");
            return false;
        }
        let invocation = crate::ids::new_id();
        let db_path = self.db_path.clone();
        let data_dir = self.data_dir.clone();
        let tx = self.ui_tx.clone();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let tid = thread_id.to_string();
        let tid2 = tid.clone();
        let inv2 = invocation.clone();
        std::thread::spawn(move || {
            let mut store = match Store::open(&db_path) {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.send(UiNote::Error {
                        invocation: inv2,
                        thread_id: tid2,
                        message: e.to_string(),
                    });
                    return;
                }
            };
            let mut runner = Runner {
                data_dir,
                db_path: db_path.clone(),
                ui: Some(tx.clone()),
                cancel: Some(cancel_rx),
                invocation: inv2.clone(),
            };
            // Un panic del runner NO puede dejar el slot ocupado para siempre
            // (bloquearía hasta la salida): se convierte en nota terminal y el
            // run a medio camino queda para la reconciliación S0.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                runner.start(&mut store, &tid2, &opts)
            }));
            let terminal_error = match outcome {
                Ok(Ok(_)) => None,
                Ok(Err(e)) => Some(e.to_string()),
                Err(_) => Some("el runner terminó con un panic (el run queda para S0)".to_string()),
            };
            if let Some(message) = terminal_error {
                let _ = tx.send(UiNote::Error {
                    invocation: inv2,
                    thread_id: tid2,
                    message,
                });
            }
            // La nota RunDone terminal la emite el propio Runner: no duplicar.
        });
        self.runs.push(RunSlot {
            invocation,
            thread_id: tid,
            run_id: None,
            cancel_requested: false,
            cancel: Some(cancel_tx),
        });
        self.status = status.to_string();
        true
    }

    fn on_note(&mut self, note: UiNote) {
        match note {
            UiNote::RunStarted {
                invocation, run_id, ..
            } => {
                if let Some(slot) = self.runs.iter_mut().find(|s| s.invocation == invocation) {
                    slot.run_id = Some(run_id);
                }
                let _ = self.reload_threads();
                let _ = self.refresh_detail();
            }
            UiNote::RunDone {
                invocation, state, ..
            } => {
                // Ack terminal: libera ÚNICAMENTE el slot de esta invocación.
                // Una nota tardía de un run viejo no toca una ejecución nueva.
                if let Some(idx) = self.slot_index_by_invocation(&invocation) {
                    let slot = self.runs.remove(idx);
                    self.status = if slot.cancel_requested {
                        // Cancelación confirmada ≠ finalización normal.
                        format!("cancelación confirmada: run {state}")
                    } else {
                        match state.as_str() {
                            "completed" => "run completado".into(),
                            "cancelled" => "run cancelado".into(),
                            _ => format!("run terminó: {state}"),
                        }
                    };
                }
                let _ = self.reload_threads();
                let _ = self.refresh_detail();
            }
            UiNote::Event { thread_id, .. } => {
                // Solo refresca si el evento es del thread abierto: la
                // navegación no redirige eventos de otros threads.
                if self
                    .detail
                    .as_ref()
                    .map(|d| &d.thread_id)
                    .is_some_and(|t| t == &thread_id)
                {
                    let _ = self.refresh_detail();
                }
            }
            UiNote::Error {
                invocation,
                message,
                ..
            } => {
                // Error terminal de la invocación (falló antes de existir un
                // run, o panic del runner): libera ESE slot y se atribuye al
                // thread de esa invocación.
                if let Some(idx) = self.slot_index_by_invocation(&invocation) {
                    self.runs.remove(idx);
                }
                self.status = format!("error: {message}");
                let _ = self.reload_threads();
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Result<()> {
        match &mut self.mode {
            Mode::CommentInput { buf, .. } => match key.code {
                KeyCode::Esc => {
                    self.mode = Mode::Normal;
                    self.status = "comentario cancelado".into();
                }
                KeyCode::Enter => self.submit_comment()?,
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) => buf.push(c),
                _ => {}
            },
            Mode::BranchInput { buf, .. } => match key.code {
                KeyCode::Esc => {
                    self.mode = Mode::Normal;
                    self.status = "integración cancelada".into();
                }
                KeyCode::Enter => self.submit_integrate()?,
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) => buf.push(c),
                _ => {}
            },
            Mode::RunInput { buf } => match key.code {
                KeyCode::Esc => {
                    self.mode = Mode::Normal;
                    self.status = "nuevo run cancelado".into();
                }
                KeyCode::Enter => self.submit_run()?,
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) => buf.push(c),
                _ => {}
            },
            Mode::Help => {
                self.mode = Mode::Normal;
            }
            Mode::Normal => match key.code {
                KeyCode::Char('q') => {
                    if self.runs.is_empty() {
                        self.should_quit = true;
                    } else {
                        // Salida segura (S3): no se abandona con runs vivos.
                        let activos = self.runs.iter().filter(|s| !s.cancel_requested).count();
                        self.status = format!(
                            "salida bloqueada: {} run(s) supervisado(s) ({} activos, {} cancelando) — cancelá con x y esperá el ack terminal",
                            self.runs.len(),
                            activos,
                            self.runs.len() - activos
                        );
                    }
                }
                KeyCode::Char('?') => self.mode = Mode::Help,
                KeyCode::Char('r') => {
                    self.reload_threads()?;
                    self.refresh_detail()?;
                    self.status = "refrescado".into();
                }
                KeyCode::Up => {
                    if let Some(d) = self.detail.as_mut() {
                        d.sel = d.sel.saturating_sub(1);
                        self.refresh_detail()?;
                    } else {
                        self.thread_sel = self.thread_sel.saturating_sub(1);
                    }
                }
                KeyCode::Down => {
                    if let Some(d) = self.detail.as_mut() {
                        d.sel = (d.sel + 1).min(d.events.len().saturating_sub(1));
                        self.refresh_detail()?;
                    } else if !self.threads.is_empty() {
                        self.thread_sel = (self.thread_sel + 1).min(self.threads.len() - 1);
                    }
                }
                KeyCode::PageDown => {
                    if let Some(d) = self.detail.as_mut() {
                        d.patch_scroll = d.patch_scroll.saturating_add(10);
                    }
                }
                KeyCode::PageUp => {
                    if let Some(d) = self.detail.as_mut() {
                        d.patch_scroll = d.patch_scroll.saturating_sub(10);
                    }
                }
                KeyCode::Enter => {
                    if self.detail.is_none() {
                        self.open_thread(self.thread_sel)?;
                    }
                }
                KeyCode::Esc => {
                    self.detail = None;
                }
                KeyCode::Char('c') => self.start_comment()?,
                KeyCode::Char('n') => self.start_run_prompt()?,
                KeyCode::Char('f') => self.send_feedback()?,
                KeyCode::Char('i') => self.start_integrate()?,
                KeyCode::Char('x') => {
                    match self
                        .selected_thread_id()
                        .and_then(|tid| self.slot_index_for_thread(&tid))
                    {
                        Some(idx) => {
                            let slot = &mut self.runs[idx];
                            if slot.cancel_requested {
                                self.status =
                                    "el run ya está cancelando (slot ocupado hasta el ack)".into();
                            } else if let Some(cancel) = slot.cancel.take() {
                                // El envío consume el canal; si falla, el run
                                // ya terminó (llegará/ llegó su nota terminal).
                                let _ = cancel.send(());
                                slot.cancel_requested = true;
                                self.status =
                                    "cancelación pedida: el slot sigue ocupado hasta el ack terminal"
                                        .into();
                            }
                        }
                        None => {
                            self.status = "no hay run activo en el thread seleccionado".into();
                        }
                    }
                }
                _ => {}
            },
        }
        Ok(())
    }
}

pub fn run(data_dir: PathBuf, db_path: PathBuf, backend: String, t0: Instant) -> Result<()> {
    let mut store = Store::open(&db_path)?;

    // Recuperación al abrir: reconciliar checkpoints interrumpidos.
    // S0: runs interrumpidos (supervisores muertos) antes que los checkpoints.
    let run_outcomes = crate::recovery::reconcile_interrupted_runs(&mut store)?;
    let outcomes =
        checkpoint::recover_pending(&mut store, &|path: &str| git::open(Path::new(path)).ok())?;
    let recovery_note = summarize_recovery(&run_outcomes, &outcomes);
    // Reconciliación de integraciones interrumpidas (B4).
    let _ = session::reconcile_integrations(&mut store)?;

    let threads = threads::list_projection(&store.conn, 200)?;
    let (ui_tx, ui_rx) = mpsc::channel();
    let mut app = App {
        store,
        data_dir,
        db_path: db_path.clone(),
        threads,
        thread_sel: 0,
        detail: None,
        mode: Mode::Normal,
        status: String::new(),
        recovery_note,
        ui_rx,
        ui_tx,
        runs: Vec::new(),
        backend,
        should_quit: false,
        t0,
        first_draw_done: false,
        input_processed: false,
        thread_opened_beacon: false,
    };

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    while !app.should_quit {
        terminal.draw(|f| render(f, app))?;
        if !app.first_draw_done {
            app.first_draw_done = true;
            bench::beacon("first_draw", app.t0);
        }
        while let Ok(note) = app.ui_rx.try_recv() {
            app.on_note(note);
        }
        if event::poll(Duration::from_millis(80))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            app.on_key(key)?;
            if !app.input_processed {
                app.input_processed = true;
                bench::beacon("input_processed", app.t0);
            }
            // Punto de medición del benchmark (§4): la entrada ABRIÓ un
            // thread real (timeline + resúmenes de comentarios renderizados).
            if !app.thread_opened_beacon && app.detail.is_some() {
                app.thread_opened_beacon = true;
                bench::beacon("thread_opened", app.t0);
            }
        }
    }
    Ok(())
}

fn summarize_recovery(
    run_outcomes: &[crate::recovery::RunRecovery],
    outcomes: &[checkpoint::RecoveryOutcome],
) -> Option<String> {
    if run_outcomes.is_empty() && outcomes.is_empty() {
        return None;
    }
    let mut interrupted = 0usize;
    let mut unsafe_runs = 0usize;
    for r in run_outcomes {
        if let crate::recovery::RunOutcome::Interrupted { unsafe_state, .. } = &r.outcome {
            interrupted += 1;
            if *unsafe_state {
                unsafe_runs += 1;
            }
        }
    }
    let mut completed = 0;
    let mut problems = Vec::new();
    for o in outcomes {
        match o {
            checkpoint::RecoveryOutcome::Completed { .. } => completed += 1,
            checkpoint::RecoveryOutcome::AlreadyResolved { .. } => {}
            checkpoint::RecoveryOutcome::RefConflict { snapshot_id, .. } => problems.push(format!(
                "conflicto de ref en snapshot {}",
                &snapshot_id[..8.min(snapshot_id.len())]
            )),
            checkpoint::RecoveryOutcome::ObjectsMissing { snapshot_id, .. } => {
                problems.push(format!(
                    "objetos faltantes para snapshot {}",
                    &snapshot_id[..8.min(snapshot_id.len())]
                ))
            }
            checkpoint::RecoveryOutcome::RepoUnavailable { repo_path, .. } => {
                problems.push(format!("repo no disponible: {repo_path}"))
            }
            checkpoint::RecoveryOutcome::Failed { detail, .. } => problems.push(detail.clone()),
        }
    }
    let mut s = String::new();
    if interrupted > 0 {
        s.push_str(&format!(
            "recuperación: {interrupted} run(s) interrumpidos reconciliados{}",
            if unsafe_runs > 0 {
                format!(
                    " ({unsafe_runs} con backend no verificado: workspace bloqueado para reuso)"
                )
            } else {
                String::new()
            }
        ));
    }
    if completed > 0 {
        if !s.is_empty() {
            s.push_str(" · ");
        }
        s.push_str(&format!("{completed} checkpoint(s) completados"));
    }
    if !problems.is_empty() {
        if !s.is_empty() {
            s.push_str(" · ");
        }
        s.push_str(&format!("pendientes: {}", problems.join("; ")));
    }
    Some(s)
}

// ── render ───────────────────────────────────────────────────────────────────

fn render(f: &mut Frame, app: &mut App) {
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .split(f.area());

    // Header
    let mut header = vec![Line::from(vec![
        Span::styled(
            "🪶 Aeril",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            " · Fase 1 · backend: {} · runs {}/{} · {}",
            app.backend,
            app.runs.len(),
            MAX_SUPERVISED_RUNS,
            format_relative(crate::timeutil::now_ms())
        )),
    ])];
    if let Some(note) = &app.recovery_note {
        header.push(Line::from(Span::styled(
            note.clone(),
            Style::default().fg(Color::Yellow),
        )));
    }
    f.render_widget(
        Paragraph::new(header).block(Block::default().borders(Borders::ALL)),
        chunks[0],
    );

    // Body: threads | main
    let body = Layout::horizontal([Constraint::Length(36), Constraint::Min(0)]).split(chunks[1]);
    render_threads(f, app, body[0]);
    render_main(f, app, body[1]);

    // Footer
    let mut footer_lines = Vec::new();
    match &app.mode {
        Mode::CommentInput { anchor_desc, .. } => footer_lines.push(Line::from(vec![
            Span::styled(format!("COMENTARIO [{anchor_desc}] "), Style::default().fg(Color::Magenta)),
            Span::raw("Enter guarda · Esc cancela"),
        ])),
        Mode::BranchInput { .. } => footer_lines.push(Line::from(
            "INTEGRAR fast-forward: escribí la rama destino · Enter ejecuta · Esc cancela",
        )),
        Mode::RunInput { .. } => footer_lines.push(Line::from(
            "NUEVO RUN: escribí el objetivo en el thread abierto · Enter lanza · Esc cancela",
        )),
        Mode::Help => footer_lines.push(Line::from(
            "↑↓ navegar · Enter abrir thread · n nuevo run · c comentar checkpoint · f enviar feedback · i integrar · x cancelar run (solo el thread seleccionado) · PgUp/PgDn scroll del patch · r refrescar · q salir (bloqueado con runs vivos)",
        )),
        Mode::Normal => {
            let mut spans = vec![Span::styled("↑↓ ", Style::default().fg(Color::Green))];
            spans.push(Span::raw("navegar · "));
            spans.push(Span::styled("Enter ", Style::default().fg(Color::Green)));
            spans.push(Span::raw("abrir · "));
            spans.push(Span::styled("n ", Style::default().fg(Color::Green)));
            spans.push(Span::raw("nuevo run · "));
            spans.push(Span::styled("c ", Style::default().fg(Color::Green)));
            spans.push(Span::raw("comentar · "));
            spans.push(Span::styled("f ", Style::default().fg(Color::Green)));
            spans.push(Span::raw("feedback · "));
            spans.push(Span::styled("i ", Style::default().fg(Color::Green)));
            spans.push(Span::raw("integrar · "));
            spans.push(Span::styled("x ", Style::default().fg(Color::Green)));
            spans.push(Span::raw("cancelar · "));
            spans.push(Span::styled("? ", Style::default().fg(Color::Green)));
            spans.push(Span::raw("ayuda · "));
            spans.push(Span::styled("q ", Style::default().fg(Color::Green)));
            spans.push(Span::raw("salir"));
            footer_lines.push(Line::from(spans));
        }
    }
    // Input visual de los modos de edición.
    match &app.mode {
        Mode::CommentInput { buf, .. } => {
            footer_lines.push(Line::from(vec![
                Span::raw("> "),
                Span::styled(buf.clone(), Style::default().fg(Color::White)),
                Span::styled("▏", Style::default().fg(Color::Gray)),
            ]));
        }
        Mode::BranchInput { buf, .. } => {
            footer_lines.push(Line::from(vec![
                Span::raw("rama> "),
                Span::styled(buf.clone(), Style::default().fg(Color::White)),
                Span::styled("▏", Style::default().fg(Color::Gray)),
            ]));
        }
        Mode::RunInput { buf } => {
            footer_lines.push(Line::from(vec![
                Span::raw("objetivo> "),
                Span::styled(buf.clone(), Style::default().fg(Color::White)),
                Span::styled("▏", Style::default().fg(Color::Gray)),
            ]));
        }
        _ => {
            let status = if app.runs.is_empty() {
                app.status.clone()
            } else {
                let marca: String = app
                    .runs
                    .iter()
                    .map(|s| if s.cancel_requested { '⏹' } else { '▶' })
                    .collect();
                format!(
                    "{marca} runs {}/{} (x cancela el thread seleccionado) · {}",
                    app.runs.len(),
                    MAX_SUPERVISED_RUNS,
                    app.status
                )
            };
            footer_lines.push(Line::from(Span::styled(
                status,
                Style::default().fg(Color::Gray),
            )));
        }
    }
    f.render_widget(Paragraph::new(footer_lines), chunks[2]);
}

fn render_threads(f: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let items: Vec<ListItem> = app
        .threads
        .iter()
        .map(|t| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{} ", &t.id[..6.min(t.id.len())]),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(truncate_chars(&t.title, 26)),
            ]))
            .style(Style::default())
        })
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" threads "))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ");
    let mut state = ListState::default();
    state.select(Some(app.thread_sel));
    f.render_stateful_widget(list, area, &mut state);
}

fn render_main(f: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let Some(detail) = app.detail.as_mut() else {
        let text = vec![
            Line::from(""),
            Line::from("  Abrí un thread con Enter para ver su timeline y checkpoints."),
            Line::from(""),
            Line::from(
                "  Flujo:  n run nuevo → editar → 📌 checkpoint → 🚩 comentar → f feedback → i integrar"
                    .to_string(),
            ),
        ];
        f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), area);
        return;
    };
    let split =
        Layout::vertical([Constraint::Percentage(40), Constraint::Percentage(60)]).split(area);

    // Timeline
    let items: Vec<ListItem> = detail
        .events
        .iter()
        .map(|ev| {
            let text = summarize_event(detail, ev);
            ListItem::new(text)
        })
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(format!(
            " {} · {} eventos ",
            truncate_chars(&detail.title, 40),
            detail.events.len()
        )))
        .highlight_style(Style::default().bg(Color::DarkGray));
    let mut state = ListState::default();
    state.select(Some(detail.sel));
    f.render_stateful_widget(list, split[0], &mut state);

    // Patch
    let total = detail.patch.len();
    let visible: Vec<Line> = detail
        .patch
        .iter()
        .skip(detail.patch_scroll as usize)
        .take(split[1].height.saturating_sub(2) as usize)
        .map(|l| {
            let style = if l.starts_with('+') {
                Style::default().fg(Color::Green)
            } else if l.starts_with('-') {
                Style::default().fg(Color::Red)
            } else if l.starts_with("@@") {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            };
            Line::from(Span::styled(l.clone(), style))
        })
        .collect();
    let patch_title = if total == 0 {
        " patch (seleccioná un 📌 checkpoint o 🚩 comentario) ".to_string()
    } else {
        format!(
            " patch · líneas {}-{} de {total} (PgUp/PgDn) ",
            detail.patch_scroll + 1,
            (detail.patch_scroll as usize + visible.len()).min(total)
        )
    };
    f.render_widget(
        Paragraph::new(visible).block(Block::default().borders(Borders::ALL).title(patch_title)),
        split[1],
    );
}

fn summarize_event(detail: &ThreadDetail, ev: &EventRecord) -> Line<'static> {
    let seq = Span::styled(
        format!("{:>4} ", ev.seq),
        Style::default().fg(Color::DarkGray),
    );
    match ev.kind.as_str() {
        events::K_RUN_STARTED => {
            let p: events::payload::RunStarted =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            Line::from(vec![
                seq,
                Span::styled("▶ run ", Style::default().fg(Color::Blue)),
                Span::raw(truncate_chars(&p.goal, 70)),
            ])
        }
        events::K_AGENT_MESSAGE => {
            let p: events::payload::AgentMessage =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            Line::from(vec![
                seq,
                Span::raw("💬 "),
                Span::raw(truncate_chars(
                    p.text.lines().next().unwrap_or_default(),
                    76,
                )),
            ])
        }
        events::K_AGENT_TOOL_CALL => {
            let p: events::payload::AgentToolCall =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            Line::from(vec![
                seq,
                Span::raw("🔧 "),
                Span::raw(truncate_chars(&p.title, 60)),
                Span::raw(p.status.map(|s| format!(" [{s}]")).unwrap_or_default()),
            ])
        }
        events::K_PERMISSION_REQUESTED => {
            let p: events::payload::PermissionRequested =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            Line::from(vec![
                seq,
                Span::raw("🔐 "),
                Span::raw(truncate_chars(&p.title, 70)),
            ])
        }
        events::K_PERMISSION_RESPONDED => {
            let p: events::payload::PermissionResponded =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            Line::from(vec![
                seq,
                Span::styled(
                    format!("🔐 → {} ({}) ", p.option_id, p.policy),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        }
        events::K_CHECKPOINT_PREPARED => {
            let p: events::payload::CheckpointPrepared =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            Line::from(vec![
                seq,
                Span::styled(
                    format!(
                        "… checkpoint preparado {} ",
                        &p.snapshot_id[..8.min(p.snapshot_id.len())]
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        }
        events::K_CHECKPOINT_READY => {
            let p: events::payload::CheckpointReady =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            let adds: usize = p.files.iter().map(|f| f.additions).sum();
            let dels: usize = p.files.iter().map(|f| f.deletions).sum();
            Line::from(vec![
                seq,
                Span::styled("📌 checkpoint ", Style::default().fg(Color::Yellow)),
                Span::styled(
                    p.snapshot_id[..8.min(p.snapshot_id.len())].to_string(),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(format!(
                    " · {} archivo(s) +{adds} −{dels}{}",
                    p.files.len(),
                    if p.recovered { " · recuperado" } else { "" }
                )),
            ])
        }
        events::K_CHECKPOINT_FAILED => {
            let p: events::payload::CheckpointFailed =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            Line::from(vec![
                seq,
                Span::styled(
                    format!("✖ checkpoint falló: {} ", truncate_chars(&p.reason, 60)),
                    Style::default().fg(Color::Red),
                ),
            ])
        }
        events::K_COMMENT_ADDED => {
            let p: events::payload::CommentAdded =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            let state = detail
                .comments
                .iter()
                .find(|c| c.comment_id == ev.id)
                .map(|c| c.state.as_str())
                .unwrap_or("open");
            Line::from(vec![
                seq,
                Span::styled("🚩 ", Style::default().fg(Color::Magenta)),
                Span::raw(format!(
                    "{}:{}-{} [{state}] ",
                    p.anchor.path, p.anchor.start_line, p.anchor.end_line
                )),
                Span::raw(truncate_chars(&p.text, 40)),
            ])
        }
        events::K_COMMENT_RESOLVED => Line::from(vec![
            seq,
            Span::styled("✓ comentario resuelto ", Style::default().fg(Color::Green)),
        ]),
        events::K_FEEDBACK_SENT => {
            let p: events::payload::FeedbackSent =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            Line::from(vec![
                seq,
                Span::styled(
                    format!("📤 feedback enviado ({} comentarios) ", p.comment_ids.len()),
                    Style::default().fg(Color::Blue),
                ),
            ])
        }
        events::K_FEEDBACK_DELIVERED => {
            let p: events::payload::FeedbackDelivered =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            Line::from(vec![
                seq,
                Span::styled(
                    format!(
                        "📬 feedback ENTREGADO ({} comentarios) ",
                        p.comment_ids.len()
                    ),
                    Style::default().fg(Color::Green),
                ),
            ])
        }
        events::K_FEEDBACK_FAILED => {
            let p: events::payload::FeedbackFailed =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            Line::from(vec![
                seq,
                Span::styled(
                    format!(
                        "✖ feedback no entregado: {} ",
                        truncate_chars(&p.reason, 50)
                    ),
                    Style::default().fg(Color::Red),
                ),
            ])
        }
        events::K_RUN_ENDED => {
            let p: events::payload::RunEnded = ev.parse().unwrap_or_else(|_| unreachable_payload());
            let color = match p.state.as_str() {
                "completed" => Color::Green,
                "cancelled" => Color::Yellow,
                _ => Color::Red,
            };
            Line::from(vec![
                seq,
                Span::styled(
                    format!("⏹ run terminó: {} ", p.state),
                    Style::default().fg(color),
                ),
            ])
        }
        events::K_WORKSPACE_RECREATED => Line::from(vec![
            seq,
            Span::styled("↻ workspace recreado ", Style::default().fg(Color::Yellow)),
        ]),
        events::K_ENGINE_SESSION => {
            let p: events::payload::EngineSession =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            Line::from(vec![
                seq,
                Span::styled(
                    format!(
                        "🔌 sesión del motor {} ",
                        if p.resumed { "reanudada" } else { "nueva" }
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        }
        events::K_INTEGRATION_COMPLETED => Line::from(vec![
            seq,
            Span::styled(
                "⇥ integración fast-forward completada ",
                Style::default().fg(Color::Green),
            ),
        ]),
        events::K_INTEGRATION_BLOCKED => {
            let p: events::payload::IntegrationBlocked =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            Line::from(vec![
                seq,
                Span::styled(
                    format!(
                        "⇥ integración bloqueada: {} ",
                        truncate_chars(&p.reason, 60)
                    ),
                    Style::default().fg(Color::Red),
                ),
            ])
        }
        events::K_VALIDATION_RECORDED => {
            let p: events::payload::ValidationRecorded =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            let color = if p.exit_code == 0 {
                Color::Green
            } else {
                Color::Red
            };
            Line::from(vec![
                seq,
                Span::styled(
                    format!("🧪 validación exit {} ", p.exit_code),
                    Style::default().fg(color),
                ),
                Span::raw(truncate_chars(&p.command, 40)),
            ])
        }
        other => Line::from(vec![seq, Span::raw(truncate_chars(other, 60))]),
    }
}

/// Nunca se llama: evita unwrap en payloads corruptos (muestra línea vacía).
fn unreachable_payload<T: Default + serde::de::DeserializeOwned>() -> T {
    serde_json::from_str("null").unwrap_or_default()
}

fn truncate_chars(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

/// Firma compatible con `Result<()>` del CLI.
pub fn run_result(data_dir: PathBuf, db_path: PathBuf, backend: String, t0: Instant) -> Result<()> {
    run(data_dir, db_path, backend, t0).map_err(|e| Error::Other(format!("TUI: {e}")))
}
