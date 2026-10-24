//! TUI workbench: tareas, run activo, timeline, checkpoint, diff y revisión.
//! Mantiene las acciones del corte vertical (comentar, feedback, integrar y
//! cancelar) sobre el estado durable de la Fase 0/1.

use crate::attention;
use crate::bench;
use crate::checkpoint;
use crate::error::{Error, Result};
use crate::git;
use crate::integration;
use crate::review;
use crate::session::{self, PermissionPolicySetting, RunOptions, Runner, UiNote};
use crate::store::Store;
use crate::store::events::{self, EventRecord};
use crate::store::threads::{self, ThreadRow, ThreadRunSummary};
use crate::timeutil::format_relative;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::layout::{Alignment, Constraint, Layout, Margin};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
};
use std::collections::HashMap;
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
    pub run_summary: Option<ThreadRunSummary>,
    pub progress: events::RunProgress,
    pub checkpoint: Option<CheckpointPanel>,
    /// Snapshot/OID cuyo patch está realmente visible. Es la única identidad
    /// que la tecla `a` puede marcar como revisada.
    pub displayed_snapshot: Option<(String, String)>,
}

pub struct CheckpointPanel {
    pub ready: events::payload::CheckpointReady,
    pub run_id: Option<String>,
    pub ordinal: usize,
    pub total: usize,
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
    IntegrationPreview {
        preview: Box<integration::IntegrationPreview>,
    },
    /// S3: captura del objetivo de un run nuevo en el thread abierto.
    RunInput {
        buf: String,
    },
    Help,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NarrowView {
    Run,
    Review,
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

const BG: Color = Color::Rgb(13, 16, 21);
const PANEL_BG: Color = Color::Rgb(21, 24, 31);
const PANEL_BG_ALT: Color = Color::Rgb(25, 29, 38);
const BORDER: Color = Color::Rgb(43, 49, 61);
const TEXT: Color = Color::Rgb(201, 209, 217);
const TEXT_BRIGHT: Color = Color::Rgb(235, 239, 244);
const MUTED: Color = Color::Rgb(125, 133, 144);
const DIM: Color = Color::Rgb(82, 89, 101);
const ACCENT: Color = Color::Rgb(111, 158, 232);
const ACCENT_BG: Color = Color::Rgb(38, 59, 89);
const GREEN: Color = Color::Rgb(127, 179, 107);
const RED: Color = Color::Rgb(214, 110, 110);
const YELLOW: Color = Color::Rgb(214, 179, 106);
const MAGENTA: Color = Color::Rgb(180, 142, 173);
const CYAN: Color = Color::Rgb(111, 168, 163);

fn is_hermes_backend(backend: &str) -> bool {
    backend == "hermes" || backend.starts_with("hermes:")
}

pub struct App {
    pub store: Store,
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub threads: Vec<ThreadRow>,
    pub run_summaries: HashMap<String, ThreadRunSummary>,
    pub attention: HashMap<String, attention::TaskAttention>,
    pub thread_sel: usize,
    pub detail: Option<ThreadDetail>,
    pub mode: Mode,
    pub narrow_view: NarrowView,
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

/// Identidad que la tecla `a` puede marcar: exactamente el patch visible.
fn review_target(detail: &ThreadDetail) -> Option<(String, String)> {
    detail.displayed_snapshot.clone()
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
        self.run_summaries = threads::run_summaries(&self.store.conn, 200)?;
        self.attention = attention::project_all(&self.store, 200)?
            .into_iter()
            .map(|a| (a.thread_id.clone(), a))
            .collect();
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
            run_summary: None,
            progress: events::RunProgress::default(),
            checkpoint: None,
            displayed_snapshot: None,
        };
        self.detail = Some(detail);
        self.refresh_detail()?;
        Ok(())
    }

    fn refresh_detail(&mut self) -> Result<()> {
        let run_summary = self
            .detail
            .as_ref()
            .and_then(|d| self.run_summaries.get(&d.thread_id).cloned());
        let Some(detail) = self.detail.as_mut() else {
            return Ok(());
        };
        detail.run_summary = run_summary;
        detail.events = events::fetch_tail(&self.store.conn, &detail.thread_id, 100)?;
        let current_run_id = detail
            .run_summary
            .as_ref()
            .and_then(|s| s.latest_run_id.clone());
        detail.progress = match current_run_id.as_deref() {
            Some(run_id) => events::run_progress(&self.store.conn, &detail.thread_id, run_id)?,
            None => events::RunProgress::default(),
        };
        // Resúmenes livianos (sin patches): la memoria de revisión queda
        // acotada; el patch se carga SOLO para el evento seleccionado.
        detail.comments = review::list_comment_summaries(&self.store, &detail.thread_id, 200, 0)?;
        if detail.sel >= detail.events.len() {
            detail.sel = detail.events.len().saturating_sub(1);
        }
        // Patch del evento seleccionado. Si no se seleccionó un checkpoint,
        // el panel derecho muestra el último checkpoint verificado: la
        // evidencia durable no depende de mantener una fila seleccionada.
        detail.patch.clear();
        detail.patch_scroll = 0;
        detail.checkpoint = None;
        detail.displayed_snapshot = None;
        let selected_ready = if let Some(ev) = detail
            .events
            .get(detail.sel)
            .filter(|ev| ev.kind == events::K_CHECKPOINT_READY)
            .cloned()
        {
            Some(ev)
        } else {
            checkpoint::latest_ready(&self.store, &detail.thread_id)?.map(|(ev, _)| ev)
        };
        if let Some(ev) = selected_ready
            && let Ok(ready) = ev.parse::<events::payload::CheckpointReady>()
        {
            let total = events::count_by_kind(
                &self.store.conn,
                &detail.thread_id,
                events::K_CHECKPOINT_READY,
            )? as usize;
            let ordinal = events::count_by_kind_until(
                &self.store.conn,
                &detail.thread_id,
                events::K_CHECKPOINT_READY,
                ev.seq,
            )? as usize;
            detail.patch = ready.patch.lines().map(str::to_string).collect();
            detail.displayed_snapshot = Some((ready.snapshot_id.clone(), ready.oid.clone()));
            detail.checkpoint = Some(CheckpointPanel {
                ready,
                run_id: ev.run_id.clone(),
                ordinal,
                total,
            });
        }
        if let Some(ev) = detail.events.get(detail.sel).cloned()
            && ev.kind == events::K_COMMENT_ADDED
            && detail.comments.iter().any(|c| c.comment_id == ev.id)
        {
            // El patch del comentario se carga SOLO para el seleccionado.
            let repo = git::open(Path::new(&detail.repo_path)).ok();
            if let Ok(view) =
                review::view_comment(&self.store, &detail.thread_id, &ev.id, repo.as_ref())
            {
                detail.patch = view.original_patch.lines().map(str::to_string).collect();
                // El panel derecho no debe seguir mostrando el último ready si
                // el patch visible pertenece al snapshot anclado del comentario.
                detail.checkpoint = None;
                detail.displayed_snapshot =
                    Some((view.anchor.snapshot_id.clone(), view.snapshot_oid.clone()));
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
            self.status = "seleccioná un checkpoint listo para comentar".into();
            return Ok(());
        };
        if ev.kind != events::K_CHECKPOINT_READY {
            self.status = "solo se comenta sobre eventos `checkpoint.ready`".into();
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
        let short_snapshot: String = ready.snapshot_id.chars().take(8).collect();
        let desc = format!("{path} líneas {start}-{end} · snapshot {short_snapshot}");
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
                let short_id: String = id.chars().take(8).collect();
                self.status =
                    format!("comentario {short_id} agregado (abierto, f para enviar feedback)");
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
        // Si el usuario seleccionó un checkpoint.ready en el timeline, ese es
        // el snapshot elegido. Si no, se elige latest_ready UNA vez y se pinea.
        let selected = detail
            .events
            .get(detail.sel)
            .filter(|ev| ev.kind == events::K_CHECKPOINT_READY)
            .cloned();
        let ready = match selected {
            Some(ev) => match ev.parse::<events::payload::CheckpointReady>() {
                Ok(ready) => Some(ready),
                Err(e) => {
                    self.status = format!("checkpoint seleccionado ilegible: {e}");
                    return Ok(());
                }
            },
            None => checkpoint::latest_ready(&self.store, &thread_id)?.map(|(_, ready)| ready),
        };
        match ready {
            Some(ready) => {
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

    /// Enter en BranchInput SOLO calcula el preview read-only. Nunca mueve refs.
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
        match integration::preview(&self.store, &thread_id, &snapshot_id, &target) {
            Ok(preview) => {
                self.status = if preview.eligible {
                    "preview listo: y confirma · Esc cancela".into()
                } else {
                    format!(
                        "preview: {} · {}",
                        preview.status_label(),
                        preview.reason.clone().unwrap_or_default()
                    )
                };
                self.mode = Mode::IntegrationPreview {
                    preview: Box::new(preview),
                };
            }
            Err(e) => {
                self.status = format!("preview falló: {e}");
            }
        }
        Ok(())
    }

    /// Marca explícitamente revisado el checkpoint cuyo patch está visible.
    /// La identidad sale de `displayed_snapshot`, no de "el último ready":
    /// un comentario puede mostrar el patch de un snapshot anterior.
    fn mark_reviewed(&mut self) -> Result<()> {
        let Some(detail) = self.detail.as_ref() else {
            self.status = "abrí un thread para marcar un checkpoint revisado".into();
            return Ok(());
        };
        let thread_id = detail.thread_id.clone();
        let Some((snapshot_id, oid)) = review_target(detail) else {
            self.status = "seleccioná un checkpoint listo para marcar revisado".into();
            return Ok(());
        };
        match review::acknowledge(&mut self.store, &thread_id, &snapshot_id, &oid) {
            Ok(true) => {
                let short: String = snapshot_id.chars().take(8).collect();
                self.status = format!("checkpoint {short} marcado revisado");
                self.reload_threads()?;
            }
            Ok(false) => self.status = "el checkpoint ya estaba marcado revisado".into(),
            Err(e) => self.status = format!("no se pudo marcar revisado: {e}"),
        }
        self.refresh_detail()?;
        Ok(())
    }

    /// `y` es la única tecla que ejecuta la operación. Revalida preview,
    /// snapshot, validación, review y target pinned antes del CAS.
    fn confirm_integration(&mut self) -> Result<()> {
        let Mode::IntegrationPreview { preview } = std::mem::replace(&mut self.mode, Mode::Normal)
        else {
            return Ok(());
        };
        let Some(detail) = self.detail.as_ref() else {
            self.status = "no hay thread abierto: integración cancelada".into();
            return Ok(());
        };
        if detail.thread_id != preview.thread_id {
            self.status = "el thread cambió: preview invalidado, generá uno nuevo".into();
            return Ok(());
        }
        if !preview.eligible {
            self.status = format!(
                "no confirmable: {}",
                preview.reason.clone().unwrap_or_else(|| "bloqueado".into())
            );
            return Ok(());
        }
        let (Some(source), Some(target)) = (&preview.source_oid, &preview.target_oid) else {
            self.status = "preview sin identidades completas: cancelado".into();
            return Ok(());
        };
        match integration::apply(
            &mut self.store,
            &preview.thread_id,
            &preview.snapshot_id,
            &preview.target_branch,
            source,
            target,
        ) {
            Ok(outcome) if outcome.noop => {
                self.status = format!(
                    "ya integrado: {} apunta a {}",
                    outcome.target_branch, outcome.target_oid_after
                );
            }
            Ok(outcome) => {
                self.status = format!(
                    "integrado fast-forward a {}: {} → {}",
                    outcome.target_branch,
                    &outcome.target_oid_before[..8.min(outcome.target_oid_before.len())],
                    &outcome.target_oid_after[..8.min(outcome.target_oid_after.len())]
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
        // La lista de tareas puede conservar un path que ya no es un repo
        // válido. Fallar aquí hace visible el error en esta interacción; el
        // runner vuelve a validar el repositorio antes de crear el run.
        let repo_preflight = threads::get(&self.store.conn, thread_id)
            .and_then(|row| row.ok_or_else(|| Error::not_found(format!("thread {thread_id}"))))
            .and_then(|row| git::open(Path::new(&row.repo_path)).map(|_| ()));
        if let Err(error) = repo_preflight {
            self.status = format!("error: {error}");
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
            Mode::IntegrationPreview { .. } => match key.code {
                KeyCode::Esc | KeyCode::Char('n') => {
                    self.mode = Mode::Normal;
                    self.status = "integración cancelada (sin cambios)".into();
                }
                KeyCode::Char('y') => self.confirm_integration()?,
                // Enter y cualquier otra tecla NO ejecutan: la confirmación
                // explícita es obligatoria.
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
                KeyCode::Tab => {
                    self.narrow_view = match self.narrow_view {
                        NarrowView::Run => NarrowView::Review,
                        NarrowView::Review => NarrowView::Run,
                    };
                    self.status = match self.narrow_view {
                        NarrowView::Run => "vista: run".into(),
                        NarrowView::Review => "vista: checkpoint / review".into(),
                    };
                }
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
                KeyCode::Char('a') => self.mark_reviewed()?,
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
    let run_summaries = threads::run_summaries(&store.conn, 200)?;
    let attention = attention::project_all(&store, 200)?
        .into_iter()
        .map(|a| (a.thread_id.clone(), a))
        .collect();
    let (ui_tx, ui_rx) = mpsc::channel();
    let mut app = App {
        store,
        data_dir,
        db_path: db_path.clone(),
        threads,
        run_summaries,
        attention,
        thread_sel: 0,
        detail: None,
        mode: Mode::Normal,
        narrow_view: NarrowView::Run,
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
        // Drenar notas antes del frame: un error terminal de una invocación
        // debe aparecer en la misma ventana de interacción, no un frame tarde.
        while let Ok(note) = app.ui_rx.try_recv() {
            app.on_note(note);
        }
        terminal.draw(|f| render(f, app))?;
        if !app.first_draw_done {
            app.first_draw_done = true;
            bench::beacon("first_draw", app.t0);
        }
        while let Ok(note) = app.ui_rx.try_recv() {
            app.on_note(note);
        }
        if event::poll(Duration::from_millis(50))?
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
            checkpoint::RecoveryOutcome::Deferred { snapshot_id, .. } => problems.push(format!(
                "recuperación diferida por lock de ref en snapshot {}",
                &snapshot_id[..8.min(snapshot_id.len())]
            )),
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

fn panel() -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(PANEL_BG).fg(TEXT))
}

fn label_style() -> Style {
    Style::default().fg(MUTED).add_modifier(Modifier::BOLD)
}

fn key_badge(key: &str) -> Span<'static> {
    Span::styled(
        format!(" {key} "),
        Style::default()
            .fg(TEXT_BRIGHT)
            .bg(PANEL_BG_ALT)
            .add_modifier(Modifier::BOLD),
    )
}

fn task_state(app: &App, thread_id: &str) -> (String, Color) {
    if let Some(slot) = app.runs.iter().find(|slot| slot.thread_id == thread_id) {
        return (
            if slot.cancel_requested {
                "cancelling".into()
            } else {
                "running".into()
            },
            if slot.cancel_requested {
                YELLOW
            } else {
                ACCENT
            },
        );
    }
    let state = app
        .run_summaries
        .get(thread_id)
        .and_then(|s| s.latest_state.as_deref());
    match state {
        Some("starting") => ("starting".into(), ACCENT),
        Some("running") => ("running".into(), ACCENT),
        Some("completed") => ("completed".into(), GREEN),
        Some("failed") => ("failed".into(), RED),
        Some("cancelled") => ("cancelled".into(), YELLOW),
        _ => ("idle".into(), MUTED),
    }
}

fn elapsed_for(summary: Option<&ThreadRunSummary>) -> String {
    let Some(summary) = summary else {
        return "—".into();
    };
    let Some(start) = summary.started_ms else {
        return "—".into();
    };
    let end = summary.ended_ms.unwrap_or_else(crate::timeutil::now_ms);
    let secs = ((end - start).max(0) / 1000) as u64;
    if secs == 0 {
        "<1s".into()
    } else if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}

fn short_repo(path: &str) -> String {
    let p = Path::new(path);
    let name = p
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let parent = p
        .parent()
        .and_then(Path::file_name)
        .map(|s| s.to_string_lossy().into_owned());
    match parent {
        Some(parent) if parent != "/" => format!("{parent}/{name}"),
        _ => name,
    }
}

fn display_time(ms: i64) -> String {
    let iso = crate::timeutil::iso8601_utc(ms);
    iso.get(11..19).unwrap_or("--:--:--").to_string()
}

fn now_clock() -> String {
    let iso = crate::timeutil::iso8601_utc(crate::timeutil::now_ms());
    format!(
        "{} {} UTC",
        iso.get(0..10).unwrap_or("---- -- --"),
        iso.get(11..16).unwrap_or("--:--")
    )
}

fn selected_repo(app: &App) -> String {
    app.detail
        .as_ref()
        .map(|d| d.repo_path.clone())
        .or_else(|| app.threads.get(app.thread_sel).map(|t| t.repo_path.clone()))
        .unwrap_or_default()
}

fn running_thread_count(app: &App) -> usize {
    app.threads
        .iter()
        .filter(|thread| {
            let (state, _) = task_state(app, &thread.id);
            state == "running" || state == "starting" || state == "cancelling"
        })
        .count()
}

fn render(f: &mut Frame, app: &mut App) {
    f.render_widget(Block::default().style(Style::default().bg(BG)), f.area());
    let footer_height =
        if matches!(app.mode, Mode::Normal) && app.status.is_empty() && app.recovery_note.is_none()
        {
            2
        } else {
            3
        };
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(footer_height),
    ])
    .split(f.area());

    render_header(f, app, chunks[0]);

    if f.area().width < 110 {
        // En terminales angostas, tres columnas dejan de ser legibles. La
        // lista de tareas queda fija y el panel principal alterna Run/Review.
        let body = Layout::horizontal([Constraint::Percentage(36), Constraint::Percentage(64)])
            .split(chunks[1]);
        render_tasks(f, app, body[0]);
        match app.narrow_view {
            NarrowView::Run => render_run(f, app, body[1]),
            NarrowView::Review => render_checkpoint(f, app, body[1]),
        }
    } else {
        let body = Layout::horizontal([
            Constraint::Percentage(28),
            Constraint::Percentage(44),
            Constraint::Percentage(28),
        ])
        .split(chunks[1]);
        render_tasks(f, app, body[0]);
        render_run(f, app, body[1]);
        render_checkpoint(f, app, body[2]);
    }
    render_footer(f, app, chunks[2]);
    if let Mode::IntegrationPreview { preview } = &app.mode {
        render_integration_preview(f, preview, f.area());
    }
}

fn render_integration_preview(
    f: &mut Frame,
    preview: &integration::IntegrationPreview,
    area: ratatui::layout::Rect,
) {
    let width = if area.width >= 120 {
        104
    } else {
        area.width.saturating_sub(4).max(40)
    };
    let height = if area.height >= 36 {
        22
    } else {
        area.height.saturating_sub(4).max(12)
    };
    let rect = ratatui::layout::Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    };
    f.render_widget(Clear, rect);
    let accent = if preview.eligible { GREEN } else { RED };
    let short = |s: &str| s[..12.min(s.len())].to_string();
    let validation = match preview.validation {
        integration::ValidationState::Passed => format!(
            "PASS — {}",
            preview
                .validation_command
                .as_deref()
                .unwrap_or("validation")
        ),
        integration::ValidationState::Missing => "MISSING — ejecutá la instrucción inferior".into(),
        integration::ValidationState::Failed
        | integration::ValidationState::WrongOid
        | integration::ValidationState::Invalid => format!(
            "BLOCKED — {}",
            preview
                .reason
                .clone()
                .unwrap_or_else(|| "evidencia inválida".into())
        ),
    };
    let review = match preview.review {
        integration::ReviewState::Clear => "clear".to_string(),
        integration::ReviewState::Required => "required — press Esc, then `a` in Aeril".into(),
        integration::ReviewState::Pending => {
            format!("{} unresolved comment(s)", preview.unresolved_comments)
        }
        integration::ReviewState::Invalid => "invalid evidence".into(),
    };
    let lines = vec![
        Line::from(Span::styled(
            "Integration preview",
            Style::default()
                .fg(TEXT_BRIGHT)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("Thread      ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{} ({})", preview.thread_title, short(&preview.thread_id)),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled("Snapshot    ", Style::default().fg(MUTED)),
            Span::styled(short(&preview.snapshot_id), Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled("Source      ", Style::default().fg(MUTED)),
            Span::styled(
                preview
                    .source_oid
                    .as_deref()
                    .map(short)
                    .unwrap_or_else(|| "—".into()),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled("Target      ", Style::default().fg(MUTED)),
            Span::styled(preview.target_branch.clone(), Style::default().fg(TEXT)),
            Span::styled("  @ ", Style::default().fg(MUTED)),
            Span::styled(
                preview
                    .target_oid
                    .as_deref()
                    .map(short)
                    .unwrap_or_else(|| "—".into()),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("Files       ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{} changed", preview.files_changed),
                Style::default().fg(TEXT),
            ),
            Span::styled("   Changes ", Style::default().fg(MUTED)),
            Span::styled(
                format!("+{} / -{}", preview.additions, preview.deletions),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled("Validation  ", Style::default().fg(MUTED)),
            Span::styled(validation, Style::default().fg(accent)),
        ]),
        Line::from(vec![
            Span::styled("Review      ", Style::default().fg(MUTED)),
            Span::styled(review, Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled("Git         ", Style::default().fg(MUTED)),
            Span::styled(
                preview.status_label(),
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            preview.reason.clone().unwrap_or_else(|| {
                if preview.eligible {
                    "Fast-forward verificado. La ref solo se moverá con [y].".into()
                } else {
                    "Operación bloqueada: no se moverá ninguna ref.".into()
                }
            }),
            Style::default().fg(MUTED),
        )),
        Line::from(Span::styled(
            if preview.validation == integration::ValidationState::Missing {
                preview.validation_instruction.clone()
            } else {
                String::new()
            },
            Style::default().fg(YELLOW),
        )),
        Line::from(""),
        Line::from(Span::styled(
            if preview.eligible {
                "[y] Confirm fast-forward    [Esc] Cancel"
            } else {
                "[Esc] Close"
            },
            Style::default().fg(TEXT_BRIGHT),
        )),
    ];
    let block = Block::default()
        .title(" assisted integration ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent))
        .style(Style::default().bg(PANEL_BG));
    f.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        rect,
    );
}

fn render_header(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let compact = area.width < 110;
    let title = vec![
        Span::styled(
            " Aeril",
            Style::default()
                .fg(TEXT_BRIGHT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  |  Agent workspace", Style::default().fg(MUTED)),
    ];
    let block = panel();
    f.render_widget(block, area);
    let inner = area.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    if compact {
        let cols = Layout::horizontal([Constraint::Min(0), Constraint::Length(22)]).split(inner);
        f.render_widget(
            Paragraph::new(Line::from(title)).style(Style::default().bg(PANEL_BG)),
            cols[0],
        );
        f.render_widget(
            Paragraph::new(now_clock())
                .style(Style::default().fg(MUTED))
                .alignment(Alignment::Right),
            cols[1],
        );
        return;
    }
    let workspace_width = if area.width >= 140 {
        52
    } else if area.width >= 120 {
        46
    } else {
        38
    };
    let cols = Layout::horizontal([
        Constraint::Min(34),
        Constraint::Length(workspace_width),
        Constraint::Length(22),
    ])
    .split(inner);
    f.render_widget(
        Paragraph::new(Line::from(title)).style(Style::default().bg(PANEL_BG)),
        cols[0],
    );
    let repo = selected_repo(app);
    let workspace = if repo.is_empty() {
        "workspace  no task selected".to_string()
    } else {
        format!("workspace  {}", short_repo(&repo))
    };
    f.render_widget(
        Paragraph::new(workspace).style(Style::default().fg(MUTED)),
        cols[1],
    );
    f.render_widget(
        Paragraph::new(now_clock())
            .style(Style::default().fg(MUTED))
            .alignment(Alignment::Right),
        cols[2],
    );
}

fn render_tasks(f: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let running = running_thread_count(app);
    let attention_count = app.attention.values().filter(|a| a.is_actionable()).count();
    let summary = if area.width >= 46 {
        format!(
            "{} total · {} attention · {} running ",
            app.threads.len(),
            attention_count,
            running
        )
    } else if area.width >= 32 {
        format!(
            "{} · {} attn · {} run ",
            app.threads.len(),
            attention_count,
            running
        )
    } else {
        format!("{} · {} attn ", app.threads.len(), attention_count)
    };
    let block = panel();
    let inner = block.inner(area).inner(Margin {
        horizontal: 2,
        vertical: 0,
    });
    f.render_widget(block, area);
    let header = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(inner);
    let summary_width = summary.chars().count() as u16;
    let header_cols = Layout::horizontal([Constraint::Min(0), Constraint::Length(summary_width)])
        .split(header[0]);
    f.render_widget(
        Paragraph::new("Tasks").style(
            Style::default()
                .fg(TEXT_BRIGHT)
                .bg(PANEL_BG)
                .add_modifier(Modifier::BOLD),
        ),
        header_cols[0],
    );
    f.render_widget(
        Paragraph::new(summary)
            .style(Style::default().fg(MUTED).bg(PANEL_BG))
            .alignment(Alignment::Right),
        header_cols[1],
    );
    let list_area = header[2];
    if app.threads.is_empty() {
        f.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled("No tasks yet.", Style::default().fg(MUTED))),
                Line::from(Span::styled(
                    "Create one from the CLI, then refresh.",
                    Style::default().fg(DIM),
                )),
            ])
            .style(Style::default().bg(PANEL_BG)),
            list_area,
        );
        return;
    }
    let title_width = list_area.width.saturating_sub(9) as usize;
    let show_branch_inline = list_area.width >= 36;
    let compact_items = list_area.width < 30;
    let items: Vec<ListItem> = app
        .threads
        .iter()
        .enumerate()
        .map(|(idx, thread)| {
            let (state, state_color) = task_state(app, &thread.id);
            let att = app.attention.get(&thread.id);
            let color = att
                .map(|a| match a.priority {
                    0..=3 => MAGENTA,
                    4..=6 => YELLOW,
                    7 => GREEN,
                    _ => MUTED,
                })
                .unwrap_or(state_color);
            let summary = app.run_summaries.get(&thread.id);
            let elapsed = elapsed_for(summary);
            let selected = idx == app.thread_sel;
            let branch = summary
                .and_then(|s| s.latest_branch.clone())
                .unwrap_or_else(|| "—".into());
            let status = match att {
                Some(a) if a.historical_checkpoint => "checkpoint previo".to_string(),
                Some(a) => format!("{}  ·  {elapsed}", a.status_label),
                None => format!("{state}  ·  {elapsed}"),
            };
            let mut status_line = vec![
                Span::styled("      ", Style::default()),
                Span::styled(status.clone(), Style::default().fg(color)),
            ];
            let mut lines = vec![Line::from(vec![
                Span::styled(
                    if selected { "▌" } else { " " },
                    Style::default().fg(if selected { ACCENT } else { PANEL_BG }),
                ),
                Span::styled(format!("{:>2}  ", idx + 1), Style::default().fg(DIM)),
                Span::styled("● ", Style::default().fg(color)),
                Span::styled(
                    truncate_chars(&thread.title, title_width.max(8)),
                    Style::default().fg(TEXT_BRIGHT),
                ),
            ])];
            if show_branch_inline {
                let branch_text = format!("↳ {}", truncate_chars(&branch, 14));
                let padding = list_area
                    .width
                    .saturating_sub(
                        6 + status.chars().count() as u16 + branch_text.chars().count() as u16,
                    )
                    .max(2) as usize;
                status_line.push(Span::raw(" ".repeat(padding)));
                status_line.push(Span::styled(branch_text, Style::default().fg(DIM)));
                lines.push(Line::from(status_line));
            } else {
                lines.push(Line::from(status_line));
                if !compact_items {
                    lines.push(Line::from(vec![
                        Span::styled("      ", Style::default()),
                        Span::styled(
                            format!("↳ {}", truncate_chars(&branch, title_width.max(8))),
                            Style::default().fg(DIM),
                        ),
                    ]));
                }
            }
            if let Some(att) = att {
                let action = format!("→ {}", att.next_action);
                lines.push(Line::from(vec![
                    Span::styled("      ", Style::default()),
                    Span::styled(
                        truncate_chars(&action, list_area.width.saturating_sub(6).max(8) as usize),
                        Style::default().fg(color),
                    ),
                ]));
            }
            if !compact_items {
                lines.push(Line::from(""));
                lines.push(Line::from(""));
            }
            ListItem::new(lines).style(if selected {
                Style::default().bg(ACCENT_BG)
            } else {
                Style::default().bg(PANEL_BG)
            })
        })
        .collect();
    let list = List::new(items)
        .style(Style::default().bg(PANEL_BG))
        .highlight_style(Style::default().bg(ACCENT_BG))
        .highlight_symbol("");
    let mut state = ListState::default();
    state.select(Some(app.thread_sel));
    f.render_stateful_widget(list, list_area, &mut state);
}

fn latest_goal(detail: &ThreadDetail) -> Option<String> {
    if detail
        .run_summary
        .as_ref()
        .and_then(|s| s.latest_run_id.as_ref())
        .is_some()
    {
        detail.progress.goal.clone()
    } else {
        None
    }
}

fn run_ended(detail: &ThreadDetail) -> bool {
    let state = detail
        .run_summary
        .as_ref()
        .and_then(|s| s.latest_state.as_deref());
    matches!(state, Some("completed" | "failed" | "cancelled"))
}

fn run_is_active(detail: &ThreadDetail) -> bool {
    matches!(
        detail
            .run_summary
            .as_ref()
            .and_then(|s| s.latest_state.as_deref()),
        Some("starting" | "running")
    )
}

fn checklist(detail: &ThreadDetail) -> Vec<(String, Color, String)> {
    if detail
        .run_summary
        .as_ref()
        .and_then(|s| s.latest_run_id.as_ref())
        .is_none()
    {
        return Vec::new();
    }
    let has_session = detail.progress.has_session;
    let has_activity = detail.progress.has_activity;
    let has_checkpoint = detail.progress.has_checkpoint;
    if detail
        .run_summary
        .as_ref()
        .and_then(|s| s.latest_state.as_deref())
        .is_none()
    {
        return Vec::new();
    }
    let active = run_is_active(detail);
    let ended = run_ended(detail);
    let mut active_used = false;
    let mut steps = Vec::new();
    for (name, done, failed) in [
        ("Goal recorded", true, false),
        ("Engine session opened", has_session, false),
        ("Agent activity captured", has_activity, false),
        (
            "Checkpoint verified",
            has_checkpoint,
            ended && !has_checkpoint,
        ),
        ("Run finalized", ended, false),
    ] {
        let (icon, color) = if done {
            ("✓", GREEN)
        } else if failed {
            ("✕", RED)
        } else if active && !active_used {
            active_used = true;
            ("●", ACCENT)
        } else {
            ("○", MUTED)
        };
        steps.push((icon.to_string(), color, name.to_string()));
    }
    steps
}

fn render_run(f: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let detail_meta = app
        .detail
        .as_ref()
        .map(|d| (d.thread_id.clone(), d.title.clone()));
    let Some((thread_id, thread_title)) = detail_meta else {
        let block = panel();
        let inner = block.inner(area).inner(Margin {
            horizontal: 1,
            vertical: 0,
        });
        f.render_widget(block, area);
        f.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  Select a task with ↑↓ and press Enter.",
                    Style::default().fg(MUTED),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  n starts a run · c comments on a verified checkpoint",
                    Style::default().fg(DIM),
                )),
            ])
            .style(Style::default().bg(PANEL_BG)),
            inner,
        );
        return;
    };
    let (state, state_color) = task_state(app, &thread_id);
    let run_no = app
        .run_summaries
        .get(&thread_id)
        .map(|s| s.run_count)
        .unwrap_or(0);
    let block = panel();
    let inner = block.inner(area).inner(Margin {
        horizontal: 2,
        vertical: 0,
    });
    f.render_widget(block, area);
    let header = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(inner);
    let status = format!(
        "{}  ·  {}",
        state,
        elapsed_for(app.run_summaries.get(&thread_id))
    );
    let status_width = status.chars().count() as u16;
    let header_cols =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(status_width + 2)])
            .split(header[0]);
    let title_width = header_cols[0].width.saturating_sub(16) as usize;
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(
                    "Run {}",
                    if run_no > 0 {
                        run_no.to_string()
                    } else {
                        "—".into()
                    }
                ),
                Style::default()
                    .fg(TEXT_BRIGHT)
                    .bg(PANEL_BG)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" — ", Style::default().fg(DIM).bg(PANEL_BG)),
            Span::styled(
                truncate_chars(&thread_title, title_width.max(8)),
                Style::default().fg(TEXT_BRIGHT).bg(PANEL_BG),
            ),
        ])),
        header_cols[0],
    );
    f.render_widget(
        Paragraph::new(status)
            .style(Style::default().fg(state_color).bg(PANEL_BG))
            .alignment(Alignment::Right),
        header_cols[1],
    );
    let Some(detail) = app.detail.as_mut() else {
        return;
    };
    let compact = area.width < 70;
    let sections = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(if compact { 2 } else { 7 }),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(5),
    ])
    .split(header[2]);

    let goal = latest_goal(detail)
        .unwrap_or_else(|| "No run yet. Press n to describe the next task.".to_string());
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(" Goal", label_style())),
            Line::from(Span::styled(goal, Style::default().fg(TEXT))),
        ])
        .wrap(Wrap { trim: true })
        .style(Style::default().bg(PANEL_BG)),
        sections[0],
    );

    let steps = checklist(detail);
    let checklist_lines: Vec<Line> = if steps.is_empty() {
        vec![Line::from(Span::styled(
            "  Waiting for the first run.",
            Style::default().fg(DIM),
        ))]
    } else if compact {
        let done = steps
            .iter()
            .filter(|(icon, ..)| icon.as_str() == "✓")
            .count();
        let active =
            steps
                .iter()
                .find(|(icon, ..)| icon.as_str() == "●")
                .map(|(_, color, text)| {
                    Span::styled(
                        format!("  ·  ● {}", truncate_chars(text, 16)),
                        Style::default().fg(*color),
                    )
                });
        let mut line = vec![Span::styled(
            format!("  {done}/{} steps complete", steps.len()),
            Style::default().fg(MUTED),
        )];
        if let Some(active) = active {
            line.push(active);
        }
        vec![Line::from(line)]
    } else {
        steps
            .into_iter()
            .map(|(icon, color, text)| {
                Line::from(vec![
                    Span::styled(format!("  {icon}  "), Style::default().fg(color)),
                    Span::styled(text, Style::default().fg(TEXT)),
                ])
            })
            .collect()
    };
    f.render_widget(
        Paragraph::new(checklist_lines).style(Style::default().bg(PANEL_BG)),
        sections[1],
    );
    f.render_widget(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(BORDER)),
        sections[2],
    );
    let file_count = detail
        .checkpoint
        .as_ref()
        .map(|c| c.ready.files.len())
        .unwrap_or(0);
    let comments = detail.comments.len();
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " Agent output",
                Style::default()
                    .fg(ACCENT)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            ),
            Span::styled(
                format!("   Files {file_count}   Reviews {comments}"),
                Style::default().fg(MUTED),
            ),
        ]))
        .style(Style::default().bg(PANEL_BG)),
        sections[3],
    );

    if detail.events.is_empty() {
        f.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  The timeline will appear after the first run.",
                    Style::default().fg(DIM),
                )),
            ])
            .style(Style::default().bg(PANEL_BG)),
            sections[4],
        );
        return;
    }
    let items: Vec<ListItem> = detail
        .events
        .iter()
        .map(|ev| ListItem::new(summarize_event(detail, ev)))
        .collect();
    let list = List::new(items)
        .style(Style::default().bg(PANEL_BG))
        .highlight_style(Style::default().bg(PANEL_BG_ALT))
        .highlight_symbol("");
    let mut list_state = ListState::default();
    list_state.select(Some(detail.sel));
    f.render_stateful_widget(list, sections[4], &mut list_state);
}

fn parse_hunk(line: &str) -> Option<(i64, i64)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let old_start = old.split(',').next()?.parse().ok()?;
    let new_start = rest.split(' ').next()?.split(',').next()?.parse().ok()?;
    Some((old_start, new_start))
}

fn patch_line(
    line: &str,
    old: i64,
    new: i64,
    old_available: bool,
    new_available: bool,
) -> Line<'static> {
    let numbers = Span::styled(
        format!(
            "{:>4} {:>4}  ",
            if old_available {
                old.to_string()
            } else {
                String::new()
            },
            if new_available {
                new.to_string()
            } else {
                String::new()
            }
        ),
        Style::default().fg(DIM),
    );
    let body_style = if line.starts_with('+') {
        Style::default().fg(GREEN)
    } else if line.starts_with('-') {
        Style::default().fg(RED)
    } else if line.starts_with("@@") {
        Style::default().fg(CYAN)
    } else if line.starts_with("diff --git") || line.starts_with("index ") {
        Style::default().fg(MUTED)
    } else if line.starts_with('\\') {
        Style::default().fg(DIM)
    } else {
        Style::default().fg(TEXT)
    };
    Line::from(vec![numbers, Span::styled(line.to_string(), body_style)])
}

fn diff_path(line: &str) -> Option<String> {
    let rest = line.strip_prefix("diff --git a/")?;
    let (_, path) = rest.split_once(" b/")?;
    Some(path.to_string())
}

fn is_git_metadata(line: &str) -> bool {
    line.starts_with("index ")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("new file mode ")
        || line.starts_with("deleted file mode ")
        || line.starts_with("old mode ")
        || line.starts_with("new mode ")
        || line.starts_with("similarity index ")
        || line.starts_with("rename from ")
        || line.starts_with("rename to ")
}

fn patch_display_lines(detail: &ThreadDetail) -> Vec<Line<'static>> {
    let mut old = 0i64;
    let mut new = 0i64;
    let mut out = Vec::with_capacity(detail.patch.len());
    let stats: std::collections::HashMap<&str, (usize, usize)> = detail
        .checkpoint
        .as_ref()
        .into_iter()
        .flat_map(|checkpoint| checkpoint.ready.files.iter())
        .map(|file| (file.path.as_str(), (file.additions, file.deletions)))
        .collect();
    for line in &detail.patch {
        if let Some((old_start, new_start)) = parse_hunk(line) {
            old = old_start;
            new = new_start;
            out.push(Line::from(Span::styled(
                format!("  {line}"),
                Style::default().fg(CYAN),
            )));
        } else if line.starts_with("diff --git ") {
            if !out.is_empty() {
                out.push(Line::from(""));
            }
            let path = diff_path(line).unwrap_or_else(|| line.clone());
            let (additions, deletions) = stats.get(path.as_str()).copied().unwrap_or((0, 0));
            let mut header = vec![
                Span::styled("▾ ", Style::default().fg(ACCENT)),
                Span::styled(
                    path,
                    Style::default()
                        .fg(TEXT_BRIGHT)
                        .add_modifier(Modifier::BOLD),
                ),
            ];
            if additions > 0 || deletions > 0 {
                header.push(Span::styled(
                    format!("   +{additions}"),
                    Style::default().fg(GREEN),
                ));
                header.push(Span::styled(
                    format!("  -{deletions}"),
                    Style::default().fg(RED),
                ));
            }
            out.push(Line::from(header));
        } else if is_git_metadata(line) {
            out.push(Line::from(Span::styled(
                format!("  {line}"),
                Style::default().fg(DIM),
            )));
        } else if line.starts_with('-') {
            out.push(patch_line(line, old, new, true, false));
            old += 1;
        } else if line.starts_with('+') {
            out.push(patch_line(line, old, new, false, true));
            new += 1;
        } else if line.starts_with('\\') {
            out.push(patch_line(line, old, new, false, false));
        } else {
            out.push(patch_line(line, old, new, true, true));
            if !line.is_empty() {
                old += 1;
                new += 1;
            }
        }
    }
    out
}

fn render_checkpoint(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let checkpoint = app.detail.as_ref().and_then(|d| d.checkpoint.as_ref());
    let ordinal = checkpoint
        .map(|c| format!("{} / {}", c.ordinal, c.total))
        .unwrap_or_default();
    let block = panel();
    let inner = block.inner(area).inner(Margin {
        horizontal: 2,
        vertical: 0,
    });
    f.render_widget(block, area);
    let header = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(inner);
    let ordinal_width = ordinal.chars().count() as u16;
    let header_cols = Layout::horizontal([Constraint::Min(0), Constraint::Length(ordinal_width)])
        .split(header[0]);
    f.render_widget(
        Paragraph::new("Checkpoint").style(
            Style::default()
                .fg(TEXT_BRIGHT)
                .bg(PANEL_BG)
                .add_modifier(Modifier::BOLD),
        ),
        header_cols[0],
    );
    f.render_widget(
        Paragraph::new(ordinal)
            .style(Style::default().fg(MUTED).bg(PANEL_BG))
            .alignment(Alignment::Right),
        header_cols[1],
    );
    let content = header[2];
    let Some(detail) = app.detail.as_ref() else {
        f.render_widget(
            Paragraph::new(vec![Line::from(Span::styled(
                "Select a task to inspect its checkpoints.",
                Style::default().fg(DIM),
            ))])
            .style(Style::default().bg(PANEL_BG)),
            content,
        );
        return;
    };
    let Some(checkpoint) = checkpoint else {
        f.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "No verified checkpoint yet.",
                    Style::default().fg(MUTED),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Run an agent, then select its checkpoint.",
                    Style::default().fg(DIM),
                )),
            ])
            .style(Style::default().bg(PANEL_BG)),
            content,
        );
        return;
    };
    let current_run = detail
        .run_summary
        .as_ref()
        .and_then(|s| s.latest_run_id.as_deref());
    let checkpoint_from_current =
        current_run.is_some() && checkpoint.run_id.as_deref() == current_run;
    let checkpoint_run_label = if checkpoint_from_current {
        "current run"
    } else if current_run.is_some() {
        if area.width < 46 {
            "previous run"
        } else {
            "previous run · current pending"
        }
    } else {
        "current thread"
    };
    let checkpoint_run_color = if checkpoint_from_current {
        GREEN
    } else {
        YELLOW
    };
    let compact = area.height < 30;
    let sections = Layout::vertical([
        Constraint::Length(if compact { 3 } else { 6 }),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(if compact { 5 } else { 8 }),
    ])
    .split(content);
    let additions: usize = checkpoint.ready.files.iter().map(|f| f.additions).sum();
    let deletions: usize = checkpoint.ready.files.iter().map(|f| f.deletions).sum();
    let snapshot = &checkpoint.ready.snapshot_id;
    let oid = &checkpoint.ready.oid;
    let summary_lines = if compact {
        vec![
            Line::from(vec![
                Span::styled(
                    format!("{}  ·  ", &snapshot[..8.min(snapshot.len())]),
                    Style::default().fg(TEXT_BRIGHT),
                ),
                Span::styled(
                    format!("{} files  ", checkpoint.ready.files.len()),
                    Style::default().fg(TEXT),
                ),
                Span::styled(
                    format!("+{additions}  -{deletions}"),
                    Style::default().fg(GREEN),
                ),
            ]),
            Line::from(Span::styled(
                if checkpoint.ready.recovered {
                    "recovered · verified"
                } else {
                    "verified · immutable"
                },
                Style::default().fg(GREEN),
            )),
            Line::from(Span::styled(
                checkpoint_run_label,
                Style::default().fg(checkpoint_run_color),
            )),
        ]
    } else {
        vec![
            Line::from(vec![
                Span::styled("Snapshot  ", Style::default().fg(MUTED)),
                Span::styled(
                    snapshot[..8.min(snapshot.len())].to_string(),
                    Style::default().fg(TEXT_BRIGHT),
                ),
            ]),
            Line::from(vec![
                Span::styled("Files     ", Style::default().fg(MUTED)),
                Span::styled(
                    format!("{} changed", checkpoint.ready.files.len()),
                    Style::default().fg(TEXT),
                ),
                Span::styled(
                    format!("   +{additions}  -{deletions}"),
                    Style::default().fg(GREEN),
                ),
            ]),
            Line::from(vec![
                Span::styled("OID       ", Style::default().fg(MUTED)),
                Span::styled(
                    oid[..8.min(oid.len())].to_string(),
                    Style::default().fg(DIM),
                ),
            ]),
            Line::from(vec![
                Span::styled("State     ", Style::default().fg(MUTED)),
                Span::styled(
                    if checkpoint.ready.recovered {
                        "recovered · verified"
                    } else {
                        "verified · immutable"
                    },
                    Style::default().fg(GREEN),
                ),
            ]),
            Line::from(vec![
                Span::styled("Run       ", Style::default().fg(MUTED)),
                Span::styled(
                    checkpoint_run_label,
                    Style::default().fg(checkpoint_run_color),
                ),
            ]),
        ]
    };
    f.render_widget(
        Paragraph::new(summary_lines).style(Style::default().bg(PANEL_BG)),
        sections[0],
    );
    f.render_widget(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(BORDER)),
        sections[1],
    );
    let total = detail.patch.len();
    let scroll = (detail.patch_scroll as usize).min(total.saturating_sub(1));
    let available = sections[3].height as usize;
    let visible_end = (scroll + available).min(total);
    let title = if total == 0 {
        "Patch  ·  pending".to_string()
    } else if area.width < 46 {
        format!("Patch  {}-{}/{}", scroll + 1, visible_end, total)
    } else {
        format!(
            "Patch  ·  lines {}-{} of {}  ·  PgUp/PgDn",
            scroll + 1,
            visible_end,
            total
        )
    };
    f.render_widget(
        Paragraph::new(title).style(
            Style::default()
                .fg(ACCENT)
                .bg(PANEL_BG)
                .add_modifier(Modifier::BOLD),
        ),
        sections[2],
    );
    let lines: Vec<Line> = patch_display_lines(detail)
        .into_iter()
        .skip(scroll)
        .take(available)
        .collect();
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(PANEL_BG)),
        sections[3],
    );
    f.render_widget(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(BORDER)),
        sections[4],
    );
    let notes = Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).split(sections[5]);
    let mut note_header = vec![Line::from(vec![
        Span::styled(
            "Review notes",
            Style::default()
                .fg(TEXT_BRIGHT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {} comment(s)", detail.comments.len()),
            Style::default().fg(MUTED),
        ),
    ])];
    if let Some(comment) = detail.comments.last() {
        note_header.push(Line::from(vec![
            Span::styled("● ", Style::default().fg(ACCENT)),
            Span::styled("You", Style::default().fg(TEXT_BRIGHT)),
            Span::styled(
                format!("  {}", format_relative(comment.created_ms)),
                Style::default().fg(DIM),
            ),
        ]));
        f.render_widget(
            Paragraph::new(comment.text.clone())
                .wrap(Wrap { trim: true })
                .style(Style::default().fg(TEXT).bg(PANEL_BG)),
            notes[1].inner(Margin {
                horizontal: 2,
                vertical: 0,
            }),
        );
    } else {
        note_header.push(Line::from(Span::styled(
            "Select a checkpoint and press c to add one.",
            Style::default().fg(DIM),
        )));
    }
    f.render_widget(
        Paragraph::new(note_header).style(Style::default().bg(PANEL_BG)),
        notes[0],
    );
}

fn render_footer(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let compact = area.width < 110;
    let medium = area.width < 140;
    f.render_widget(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(BG)),
        area,
    );
    let inner = ratatui::layout::Rect {
        x: area.x + 2,
        y: area.y + 1,
        width: area.width.saturating_sub(4),
        height: area.height.saturating_sub(1),
    };
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(inner);
    let active = running_thread_count(app);
    let status = if active == 0 {
        "idle".to_string()
    } else {
        format!("{active} run(s) active")
    };
    let right = if area.width < 140 {
        format!("● {status} ")
    } else {
        format!("● {status}  ·  {}", short_repo(&selected_repo(app)))
    };

    let (heading, input) = match &app.mode {
        Mode::Normal => {
            let keys = if compact {
                Line::from(vec![
                    key_badge("n"),
                    Span::raw(" run   "),
                    key_badge("r"),
                    Span::raw(" refresh   "),
                    key_badge("c"),
                    Span::raw(" comment   "),
                    key_badge("Tab"),
                    Span::raw(if app.narrow_view == NarrowView::Run {
                        " review   "
                    } else {
                        " run   "
                    }),
                    key_badge("q"),
                    Span::raw(" quit"),
                ])
            } else if medium {
                Line::from(vec![
                    key_badge("n"),
                    Span::raw(" run   "),
                    key_badge("r"),
                    Span::raw(" refresh   "),
                    key_badge("c"),
                    Span::raw(" comment   "),
                    key_badge("a"),
                    Span::raw(" reviewed   "),
                    key_badge("x"),
                    Span::raw(" cancel   "),
                    key_badge("?"),
                    Span::raw(" help   "),
                    key_badge("q"),
                    Span::raw(" quit"),
                ])
            } else {
                Line::from(vec![
                    key_badge("n"),
                    Span::raw(" new run   "),
                    key_badge("r"),
                    Span::raw(" refresh   "),
                    key_badge("c"),
                    Span::raw(" comment   "),
                    key_badge("i"),
                    Span::raw(" integrate   "),
                    key_badge("x"),
                    Span::raw(" cancel   "),
                    key_badge("?"),
                    Span::raw(" help   "),
                    key_badge("q"),
                    Span::raw(" quit"),
                ])
            };
            let status = app.status.clone();
            (
                keys,
                Line::from(Span::styled(status, Style::default().fg(MUTED))),
            )
        }
        Mode::CommentInput { anchor_desc, .. } => (
            Line::from(vec![
                Span::styled(
                    if compact {
                        "COMENTARIO ".to_string()
                    } else {
                        format!("COMENTARIO [{anchor_desc}] ")
                    },
                    Style::default().fg(MAGENTA).add_modifier(Modifier::BOLD),
                ),
                Span::styled("Enter guarda · Esc cancela", Style::default().fg(MUTED)),
            ]),
            Line::from(Span::styled(
                format!(
                    "> {}",
                    match &app.mode {
                        Mode::CommentInput { buf, .. } => buf,
                        _ => "",
                    }
                ),
                Style::default().fg(TEXT_BRIGHT),
            )),
        ),
        Mode::BranchInput { .. } => (
            Line::from(Span::styled(
                if compact {
                    "INTEGRAR · Enter inspecciona · Esc cancela"
                } else {
                    "INTEGRAR fast-forward: rama destino · Enter abre preview · Esc cancela"
                },
                Style::default().fg(MAGENTA),
            )),
            Line::from(Span::styled(
                format!(
                    "rama> {}",
                    match &app.mode {
                        Mode::BranchInput { buf, .. } => buf,
                        _ => "",
                    }
                ),
                Style::default().fg(TEXT_BRIGHT),
            )),
        ),
        Mode::IntegrationPreview { preview } => (
            Line::from(vec![
                Span::styled("INTEGRACIÓN ", Style::default().fg(MAGENTA)),
                Span::styled(
                    preview.status_label(),
                    Style::default().fg(if preview.eligible { GREEN } else { RED }),
                ),
                Span::styled(
                    if preview.eligible {
                        " · [y] confirmar   [Esc] cancelar"
                    } else {
                        " · [Esc] cerrar"
                    },
                    Style::default().fg(MUTED),
                ),
            ]),
            Line::from(Span::styled(
                preview.reason.clone().unwrap_or_else(|| {
                    format!(
                        "{} · {} archivos · +{} / -{}",
                        preview.target_branch,
                        preview.files_changed,
                        preview.additions,
                        preview.deletions
                    )
                }),
                Style::default().fg(TEXT),
            )),
        ),
        Mode::RunInput { .. } => (
            Line::from(Span::styled(
                if compact {
                    "NUEVO RUN · Enter lanza · Esc cancela"
                } else {
                    "NUEVO RUN: escribí el objetivo en el thread abierto · Enter lanza · Esc cancela"
                },
                Style::default().fg(ACCENT),
            )),
            Line::from(Span::styled(
                format!(
                    "objetivo> {}",
                    match &app.mode {
                        Mode::RunInput { buf } => buf,
                        _ => "",
                    }
                ),
                Style::default().fg(TEXT_BRIGHT),
            )),
        ),
        Mode::Help => (
            Line::from(Span::styled(
                if compact {
                    "↑↓ select · Enter open · n run · c comment · Tab view · r refresh · q quit"
                } else {
                    "↑↓ select · Enter open · n run · c comment · a reviewed · f feedback · i integrate · x cancel · PgUp/PgDn patch · r refresh · q quit"
                },
                Style::default().fg(MUTED),
            )),
            Line::from(Span::styled(
                "Press ? or Esc to return.",
                Style::default().fg(DIM),
            )),
        ),
    };
    let right_width = if compact {
        22
    } else if area.width >= 140 {
        58
    } else {
        30
    };
    let row =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(right_width)]).split(rows[0]);
    f.render_widget(
        Paragraph::new(heading).style(Style::default().bg(BG)),
        row[0],
    );
    let status_style = if active == 0 {
        Style::default().fg(MUTED)
    } else {
        Style::default().fg(ACCENT)
    };
    f.render_widget(
        Paragraph::new(right)
            .style(status_style.bg(BG))
            .alignment(Alignment::Right),
        row[1],
    );
    let mut input_line = vec![input];
    if let Some(note) = &app.recovery_note {
        input_line.push(Line::from(Span::styled(
            format!("  {note}"),
            Style::default().fg(YELLOW),
        )));
    }
    f.render_widget(
        Paragraph::new(input_line).style(Style::default().bg(BG)),
        rows[1],
    );
}

fn action(label: &str, color: Color) -> Span<'static> {
    Span::styled(
        format!("{label:<8}"),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

fn split_title(title: &str) -> (String, String) {
    match title.split_once(": ") {
        Some((verb, path)) => (verb.to_string(), path.to_string()),
        None => (title.to_string(), String::new()),
    }
}

fn summarize_event(detail: &ThreadDetail, ev: &EventRecord) -> Line<'static> {
    let time = Span::styled(
        format!("{}  ", display_time(ev.created_ms)),
        Style::default().fg(DIM),
    );
    match ev.kind.as_str() {
        events::K_RUN_STARTED => {
            let p: events::payload::RunStarted =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            Line::from(vec![
                time,
                action("run", ACCENT),
                Span::styled(truncate_chars(&p.goal, 70), Style::default().fg(TEXT)),
            ])
        }
        events::K_AGENT_MESSAGE => {
            let p: events::payload::AgentMessage =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            Line::from(vec![
                time,
                action("msg", MUTED),
                Span::styled(
                    truncate_chars(p.text.lines().next().unwrap_or_default(), 76),
                    Style::default().fg(TEXT),
                ),
            ])
        }
        events::K_AGENT_TOOL_CALL => {
            let p: events::payload::AgentToolCall =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            let (verb, path) = split_title(&p.title);
            let mut spans = vec![
                time,
                action("tool", CYAN),
                Span::styled(truncate_chars(&verb, 28), Style::default().fg(MUTED)),
            ];
            if !path.is_empty() {
                spans.push(Span::styled("  ", Style::default()));
                spans.push(Span::styled(
                    truncate_chars(&path, 52),
                    Style::default().fg(TEXT_BRIGHT),
                ));
            }
            if let Some(status) = p.status.as_deref() {
                let color = match status {
                    "completed" => GREEN,
                    "failed" => RED,
                    _ => MUTED,
                };
                spans.push(Span::styled(
                    format!("  {status}"),
                    Style::default().fg(color),
                ));
            }
            Line::from(spans)
        }
        events::K_PERMISSION_REQUESTED => {
            let p: events::payload::PermissionRequested =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            let (verb, path) = split_title(&p.title);
            let mut spans = vec![
                time,
                action("perm", YELLOW),
                Span::styled(truncate_chars(&verb, 28), Style::default().fg(MUTED)),
            ];
            if !path.is_empty() {
                spans.push(Span::styled("  ", Style::default()));
                spans.push(Span::styled(
                    truncate_chars(&path, 52),
                    Style::default().fg(TEXT_BRIGHT),
                ));
            }
            Line::from(spans)
        }
        events::K_PERMISSION_RESPONDED => {
            let p: events::payload::PermissionResponded =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            Line::from(vec![
                time,
                action("perm", YELLOW),
                Span::styled(
                    format!("{}  ({})", p.option_id, p.policy),
                    Style::default().fg(MUTED),
                ),
            ])
        }
        events::K_CHECKPOINT_PREPARED => {
            let p: events::payload::CheckpointPrepared =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            Line::from(vec![
                time,
                action("chkpt", MUTED),
                Span::styled("prepared  ", Style::default().fg(DIM)),
                Span::styled(
                    p.snapshot_id[..8.min(p.snapshot_id.len())].to_string(),
                    Style::default().fg(TEXT),
                ),
            ])
        }
        events::K_CHECKPOINT_READY => {
            let p: events::payload::CheckpointReady =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            let adds: usize = p.files.iter().map(|f| f.additions).sum();
            let dels: usize = p.files.iter().map(|f| f.deletions).sum();
            Line::from(vec![
                time,
                action("chkpt", GREEN),
                Span::styled(
                    p.snapshot_id[..8.min(p.snapshot_id.len())].to_string(),
                    Style::default().fg(TEXT_BRIGHT),
                ),
                Span::styled(
                    format!("  {} files", p.files.len()),
                    Style::default().fg(MUTED),
                ),
                Span::styled(format!("  +{adds}"), Style::default().fg(GREEN)),
                Span::styled(format!("  -{dels}"), Style::default().fg(RED)),
                Span::styled(
                    if p.recovered { "  recovered" } else { "" },
                    Style::default().fg(YELLOW),
                ),
            ])
        }
        events::K_CHECKPOINT_FAILED => {
            let p: events::payload::CheckpointFailed =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            Line::from(vec![
                time,
                action("error", RED),
                Span::styled(
                    format!("checkpoint failed  {}", truncate_chars(&p.reason, 54)),
                    Style::default().fg(RED),
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
                time,
                action("note", MAGENTA),
                Span::styled(
                    truncate_chars(&p.anchor.path, 34),
                    Style::default().fg(TEXT_BRIGHT),
                ),
                Span::styled(
                    format!(
                        ":{}-{}  [{state}]  ",
                        p.anchor.start_line, p.anchor.end_line
                    ),
                    Style::default().fg(DIM),
                ),
                Span::styled(truncate_chars(&p.text, 38), Style::default().fg(TEXT)),
            ])
        }
        events::K_COMMENT_RESOLVED => Line::from(vec![
            time,
            action("note", GREEN),
            Span::styled("resolved", Style::default().fg(GREEN)),
        ]),
        events::K_FEEDBACK_SENT => {
            let p: events::payload::FeedbackSent =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            Line::from(vec![
                time,
                action("feedback", ACCENT),
                Span::styled(
                    format!("sent  {} comments", p.comment_ids.len()),
                    Style::default().fg(MUTED),
                ),
            ])
        }
        events::K_FEEDBACK_DELIVERED => {
            let p: events::payload::FeedbackDelivered =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            Line::from(vec![
                time,
                action("feedback", GREEN),
                Span::styled(
                    format!("delivered  {} comments", p.comment_ids.len()),
                    Style::default().fg(GREEN),
                ),
            ])
        }
        events::K_FEEDBACK_FAILED => {
            let p: events::payload::FeedbackFailed =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            Line::from(vec![
                time,
                action("error", RED),
                Span::styled(
                    format!("feedback failed  {}", truncate_chars(&p.reason, 46)),
                    Style::default().fg(RED),
                ),
            ])
        }
        events::K_RUN_ENDED => {
            let p: events::payload::RunEnded = ev.parse().unwrap_or_else(|_| unreachable_payload());
            let color = match p.state.as_str() {
                "completed" => GREEN,
                "cancelled" => YELLOW,
                _ => RED,
            };
            Line::from(vec![
                time,
                action("end", color),
                Span::styled(p.state, Style::default().fg(color)),
            ])
        }
        events::K_WORKSPACE_RECREATED => Line::from(vec![
            time,
            action("worktree", YELLOW),
            Span::styled("recreated", Style::default().fg(YELLOW)),
        ]),
        events::K_ENGINE_SESSION => {
            let p: events::payload::EngineSession =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            Line::from(vec![
                time,
                action("session", MUTED),
                Span::styled(
                    if p.resumed { "resumed" } else { "new" },
                    Style::default().fg(TEXT),
                ),
                Span::styled(
                    if p.verified {
                        "  context verified"
                    } else {
                        "  context unverified"
                    },
                    Style::default().fg(DIM),
                ),
            ])
        }
        events::K_INTEGRATION_COMPLETED => Line::from(vec![
            time,
            action("merge", GREEN),
            Span::styled("fast-forward completed", Style::default().fg(GREEN)),
        ]),
        events::K_INTEGRATION_BLOCKED => {
            let p: events::payload::IntegrationBlocked =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            Line::from(vec![
                time,
                action("error", RED),
                Span::styled(
                    format!("merge blocked  {}", truncate_chars(&p.reason, 48)),
                    Style::default().fg(RED),
                ),
            ])
        }
        events::K_VALIDATION_RECORDED => {
            let p: events::payload::ValidationRecorded =
                ev.parse().unwrap_or_else(|_| unreachable_payload());
            let color = if p.exit_code == 0 { GREEN } else { RED };
            Line::from(vec![
                time,
                action("test", color),
                Span::styled(
                    format!("exit {}  ", p.exit_code),
                    Style::default().fg(color),
                ),
                Span::styled(truncate_chars(&p.command, 42), Style::default().fg(TEXT)),
            ])
        }
        other => Line::from(vec![
            time,
            action("event", DIM),
            Span::styled(truncate_chars(other, 60), Style::default().fg(MUTED)),
        ]),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn run_started(seq: i64, run_id: &str, goal: &str) -> EventRecord {
        EventRecord {
            id: format!("event-{seq}"),
            thread_id: "thread".into(),
            seq,
            run_id: Some(run_id.into()),
            kind: events::K_RUN_STARTED.into(),
            actor: "system".into(),
            payload_version: 1,
            payload: serde_json::json!({
                "backend": "mock",
                "goal": goal,
                "workspace_id": format!("ws-{run_id}"),
                "repo_path": "/tmp/repo",
                "worktree_path": format!("/tmp/worktree-{run_id}"),
                "branch": format!("branch-{run_id}"),
                "base_oid": "0000000000000000000000000000000000000000"
            }),
            created_ms: 0,
        }
    }

    fn event(seq: i64, run_id: &str, kind: &str) -> EventRecord {
        EventRecord {
            id: format!("event-{seq}"),
            thread_id: "thread".into(),
            seq,
            run_id: Some(run_id.into()),
            kind: kind.into(),
            actor: "system".into(),
            payload_version: 1,
            payload: serde_json::json!({}),
            created_ms: 0,
        }
    }

    fn run_payload(run_id: &str, goal: &str) -> serde_json::Value {
        serde_json::json!({
            "backend": "mock",
            "goal": goal,
            "workspace_id": format!("ws-{run_id}"),
            "repo_path": "/tmp/repo",
            "worktree_path": format!("/tmp/worktree-{run_id}"),
            "branch": format!("branch-{run_id}"),
            "base_oid": "0000000000000000000000000000000000000000"
        })
    }

    fn long_thread_detail(total_events: usize) -> ThreadDetail {
        let mut store = Store::open_in_memory().unwrap();
        let thread = threads::create(&mut store.conn, "long", "/tmp/repo", "r1").unwrap();
        store
            .conn
            .execute(
                "INSERT INTO runs (id, thread_id, branch, backend, state, started_ms, ended_ms)
                 VALUES ('run-old', ?1, 'branch-old', 'mock', 'completed', 1, 2)",
                rusqlite::params![thread.id],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO runs (id, thread_id, branch, backend, state, started_ms)
                 VALUES ('run-new', ?1, 'branch-new', 'mock', 'running', 30)",
                rusqlite::params![thread.id],
            )
            .unwrap();
        let tx = store
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let append = |kind: &str, run_id: &str, payload: serde_json::Value| {
            events::append_tx(&tx, &thread.id, Some(run_id), kind, "system", &payload, 0).unwrap();
        };
        append(
            events::K_RUN_STARTED,
            "run-old",
            run_payload("run-old", "old goal"),
        );
        append(events::K_ENGINE_SESSION, "run-old", serde_json::json!({}));
        append(events::K_AGENT_TOOL_CALL, "run-old", serde_json::json!({}));
        append(events::K_CHECKPOINT_READY, "run-old", serde_json::json!({}));
        append(events::K_RUN_ENDED, "run-old", serde_json::json!({}));
        append(
            events::K_RUN_STARTED,
            "run-new",
            run_payload("run-new", "new goal"),
        );
        append(events::K_ENGINE_SESSION, "run-new", serde_json::json!({}));
        for _ in 8..total_events {
            append(events::K_AGENT_MESSAGE, "run-new", serde_json::json!({}));
        }
        tx.commit().unwrap();

        let old_progress = events::run_progress(&store.conn, &thread.id, "run-old").unwrap();
        assert!(old_progress.has_checkpoint);
        let progress = events::run_progress(&store.conn, &thread.id, "run-new").unwrap();
        assert_eq!(progress.goal.as_deref(), Some("new goal"));
        assert!(progress.has_session);
        assert!(!progress.has_activity);
        assert!(!progress.has_checkpoint);
        let summaries = threads::run_summaries(&store.conn, 10).unwrap();
        let summary = summaries.get(&thread.id).unwrap().clone();
        assert_eq!(summary.latest_run_id.as_deref(), Some("run-new"));
        let events = events::fetch_tail(&store.conn, &thread.id, 100).unwrap();
        assert_eq!(events.len(), 100);
        assert!(
            events
                .iter()
                .all(|ev| ev.kind != events::K_RUN_STARTED
                    || ev.run_id.as_deref() != Some("run-new")),
            "the current run start must be outside the 100-event window"
        );
        ThreadDetail {
            thread_id: thread.id,
            title: "long".into(),
            repo_path: "/tmp/repo".into(),
            events,
            sel: 0,
            comments: Vec::new(),
            patch: Vec::new(),
            patch_scroll: 0,
            run_summary: Some(summary),
            progress,
            checkpoint: Some(CheckpointPanel {
                ready: events::payload::CheckpointReady::default(),
                run_id: Some("run-old".into()),
                ordinal: 1,
                total: 1,
            }),
            displayed_snapshot: None,
        }
    }

    fn detail_with_two_runs() -> ThreadDetail {
        ThreadDetail {
            thread_id: "thread".into(),
            title: "two runs".into(),
            repo_path: "/tmp/repo".into(),
            events: vec![
                run_started(0, "run-old", "old goal"),
                event(1, "run-old", events::K_ENGINE_SESSION),
                event(2, "run-old", events::K_AGENT_TOOL_CALL),
                event(3, "run-old", events::K_CHECKPOINT_READY),
                event(4, "run-old", events::K_RUN_ENDED),
                run_started(5, "run-new", "new goal"),
                event(6, "run-new", events::K_ENGINE_SESSION),
            ],
            sel: 6,
            comments: Vec::new(),
            patch: Vec::new(),
            patch_scroll: 0,
            run_summary: Some(ThreadRunSummary {
                thread_id: "thread".into(),
                run_count: 2,
                latest_run_id: Some("run-new".into()),
                latest_state: Some("running".into()),
                latest_branch: Some("branch-run-new".into()),
                started_ms: Some(100),
                ended_ms: None,
            }),
            progress: events::RunProgress {
                goal: Some("new goal".into()),
                has_session: true,
                has_activity: false,
                has_checkpoint: false,
            },
            checkpoint: Some(CheckpointPanel {
                ready: events::payload::CheckpointReady::default(),
                run_id: Some("run-old".into()),
                ordinal: 1,
                total: 1,
            }),
            displayed_snapshot: None,
        }
    }

    #[test]
    fn review_target_is_the_displayed_snapshot_not_the_latest_checkpoint() {
        let mut detail = detail_with_two_runs();
        detail.displayed_snapshot = Some(("snap-a".into(), "oid-a".into()));
        assert_eq!(
            review_target(&detail),
            Some(("snap-a".into(), "oid-a".into())),
            "la tecla `a` no debe caer al último ready si el patch visible es otro"
        );
        detail.displayed_snapshot = None;
        assert_eq!(review_target(&detail), None);
    }

    #[test]
    fn progress_is_scoped_to_the_latest_run() {
        let detail = detail_with_two_runs();
        let steps = checklist(&detail);
        let step = |text: &str| {
            steps
                .iter()
                .find(|(_, _, label)| label == text)
                .unwrap_or_else(|| panic!("missing step {text}: {steps:?}"))
        };

        assert_eq!(step("Goal recorded").0, "✓");
        assert_eq!(step("Engine session opened").0, "✓");
        assert_eq!(step("Agent activity captured").0, "●");
        assert_eq!(step("Checkpoint verified").0, "○");
        assert_eq!(step("Run finalized").0, "○");
        assert_eq!(latest_goal(&detail).as_deref(), Some("new goal"));
    }

    /// Detail mínimo con un run en un estado dado (para la semántica del
    /// checklist: activo vs terminal).
    fn detail_with_state(
        state: &str,
        progress: events::RunProgress,
        checkpoint: Option<CheckpointPanel>,
    ) -> ThreadDetail {
        ThreadDetail {
            thread_id: "thread".into(),
            title: "state".into(),
            repo_path: "/tmp/repo".into(),
            events: vec![run_started(0, "run-x", "goal")],
            sel: 0,
            comments: Vec::new(),
            patch: Vec::new(),
            patch_scroll: 0,
            run_summary: Some(ThreadRunSummary {
                thread_id: "thread".into(),
                run_count: 1,
                latest_run_id: Some("run-x".into()),
                latest_state: Some(state.into()),
                latest_branch: Some("branch-run-x".into()),
                started_ms: Some(100),
                ended_ms: None,
            }),
            progress,
            checkpoint,
            displayed_snapshot: None,
        }
    }

    fn step_icon(detail: &ThreadDetail, text: &str) -> String {
        let steps = checklist(detail);
        steps
            .iter()
            .find(|(_, _, label)| label == text)
            .unwrap_or_else(|| panic!("missing step {text}: {steps:?}"))
            .0
            .clone()
    }

    #[test]
    fn active_run_without_checkpoint_keeps_checkpoint_step_pending() {
        let detail = detail_with_state(
            "running",
            events::RunProgress {
                goal: Some("goal".into()),
                has_session: true,
                has_activity: true,
                has_checkpoint: false,
            },
            None,
        );
        // Run activo sin checkpoint: el hito es el PASO EN CURSO (●) y el
        // run no está finalizado (○). No se marca fallido porque el run
        // sigue activo (el ✕ es solo para runs terminales sin checkpoint).
        assert_eq!(step_icon(&detail, "Checkpoint verified"), "●");
        assert_eq!(step_icon(&detail, "Run finalized"), "○");
        assert_eq!(step_icon(&detail, "Agent activity captured"), "✓");
        assert!(detail.checkpoint.is_none(), "no hay panel de checkpoint");
    }

    #[test]
    fn completed_run_without_checkpoint_marks_checkpoint_step_failed() {
        let detail = detail_with_state(
            "completed",
            events::RunProgress {
                goal: Some("goal".into()),
                has_session: true,
                has_activity: true,
                has_checkpoint: false,
            },
            None,
        );
        // Run terminal sin checkpoint verificado: el hito se marca FALLIDO.
        assert_eq!(step_icon(&detail, "Checkpoint verified"), "✕");
        assert_eq!(step_icon(&detail, "Run finalized"), "✓");
    }

    #[test]
    fn failed_run_marks_run_finalized() {
        let detail = detail_with_state(
            "failed",
            events::RunProgress {
                goal: Some("goal".into()),
                has_session: true,
                has_activity: false,
                has_checkpoint: false,
            },
            None,
        );
        assert_eq!(step_icon(&detail, "Run finalized"), "✓");
        assert_eq!(step_icon(&detail, "Checkpoint verified"), "✕");
    }

    #[test]
    fn historical_checkpoint_is_shown_while_a_newer_run_is_active() {
        let detail = detail_with_two_runs();
        // El checkpoint verificado pertenece al run viejo; el run nuevo sigue activo.
        assert_eq!(
            detail
                .run_summary
                .as_ref()
                .unwrap()
                .latest_run_id
                .as_deref(),
            Some("run-new")
        );
        assert_eq!(
            detail.checkpoint.as_ref().and_then(|c| c.run_id.as_deref()),
            Some("run-old"),
            "la selección de checkpoint es histórica y estable"
        );
        assert_eq!(step_icon(&detail, "Checkpoint verified"), "○");
        assert_eq!(step_icon(&detail, "Run finalized"), "○");
    }

    #[test]
    fn long_history_does_not_hide_current_run_progress() {
        for total_events in [150usize, 1000] {
            let detail = long_thread_detail(total_events);
            let steps = checklist(&detail);
            let step = |text: &str| {
                steps
                    .iter()
                    .find(|(_, _, label)| label == text)
                    .unwrap_or_else(|| panic!("missing step {text} for {total_events} events"))
            };
            assert_eq!(step("Engine session opened").0, "✓");
            assert_eq!(step("Agent activity captured").0, "●");
            assert_eq!(step("Checkpoint verified").0, "○");
            assert_eq!(latest_goal(&detail).as_deref(), Some("new goal"));
            assert_eq!(
                detail
                    .checkpoint
                    .as_ref()
                    .and_then(|checkpoint| checkpoint.run_id.as_deref()),
                Some("run-old")
            );
        }
    }
}
