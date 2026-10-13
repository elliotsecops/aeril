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
    pub comments: Vec<review::CommentView>,
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
    Help,
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
    pub running: Option<(String, oneshot::Sender<()>)>,
    pub backend: String,
    pub should_quit: bool,
    pub t0: Instant,
    pub first_draw_done: bool,
    pub input_processed: bool,
}

impl App {
    fn reload_threads(&mut self) -> Result<()> {
        self.threads = threads::list_projection(&self.store.conn, 200)?;
        if self.thread_sel >= self.threads.len() {
            self.thread_sel = self.threads.len().saturating_sub(1);
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
        let repo = git::open(Path::new(&detail.repo_path)).ok();
        detail.comments = review::list_comments(&self.store, &detail.thread_id, repo.as_ref())?;
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
            } else if ev.kind == events::K_COMMENT_ADDED {
                if let Some(c) = detail.comments.iter().find(|c| c.comment_id == ev.id) {
                    detail.patch = c.original_patch.lines().map(str::to_string).collect();
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
        if self.running.is_some() {
            self.status = "ya hay un run en curso (x para cancelar)".into();
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

    fn spawn_run(&mut self, thread_id: &str, opts: RunOptions, status: &str) {
        let db_path = self.db_path.clone();
        let data_dir = self.data_dir.clone();
        let tx = self.ui_tx.clone();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let tid = thread_id.to_string();
        let tid2 = tid.clone();
        std::thread::spawn(move || {
            let mut store = match Store::open(&db_path) {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.send(UiNote::Error {
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
            };
            if let Err(e) = runner.start(&mut store, &tid2, &opts) {
                let _ = tx.send(UiNote::Error {
                    thread_id: tid2,
                    message: e.to_string(),
                });
            } else {
                let _ = tx.send(UiNote::RunDone {
                    thread_id: tid2,
                    run_id: String::new(),
                    state: "done".into(),
                });
            }
        });
        self.running = Some((tid, cancel_tx));
        self.status = status.to_string();
    }

    fn on_note(&mut self, note: UiNote) {
        match &note {
            UiNote::RunDone { thread_id, .. } => {
                if self.running.as_ref().map(|(t, _)| t) == Some(thread_id) {
                    self.running = None;
                }
                let _ = self.reload_threads();
                let _ = self.refresh_detail();
            }
            UiNote::Event { thread_id, .. } => {
                if self
                    .detail
                    .as_ref()
                    .map(|d| &d.thread_id)
                    .is_some_and(|t| t == thread_id)
                {
                    let _ = self.refresh_detail();
                }
            }
            UiNote::Error { message, .. } => {
                self.running = None;
                self.status = format!("error: {message}");
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
            Mode::Help => {
                self.mode = Mode::Normal;
            }
            Mode::Normal => match key.code {
                KeyCode::Char('q') => self.should_quit = true,
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
                KeyCode::Char('f') => self.send_feedback()?,
                KeyCode::Char('i') => self.start_integrate()?,
                KeyCode::Char('x') => {
                    if let Some((_, cancel)) = self.running.take() {
                        let _ = cancel.send(());
                        self.status = "cancelación pedida al motor…".into();
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
    let outcomes =
        checkpoint::recover_pending(&mut store, &|path: &str| git::open(Path::new(path)).ok())?;
    let recovery_note = summarize_recovery(&outcomes);

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
        running: None,
        backend,
        should_quit: false,
        t0,
        first_draw_done: false,
        input_processed: false,
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
        if event::poll(Duration::from_millis(80))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.on_key(key)?;
                    if !app.input_processed {
                        app.input_processed = true;
                        bench::beacon("input_processed", app.t0);
                    }
                }
            }
        }
    }
    Ok(())
}

fn summarize_recovery(outcomes: &[checkpoint::RecoveryOutcome]) -> Option<String> {
    if outcomes.is_empty() {
        return None;
    }
    let mut completed = 0;
    let mut problems = Vec::new();
    for o in outcomes {
        match o {
            checkpoint::RecoveryOutcome::Completed { .. } => completed += 1,
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
    if completed > 0 {
        s.push_str(&format!(
            "recuperación: {completed} checkpoint(s) completados"
        ));
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
            " · Fase 0 · backend: {} · {}",
            app.backend,
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
        Mode::Help => footer_lines.push(Line::from(
            "↑↓ navegar · Enter abrir thread · c comentar checkpoint · f enviar feedback · i integrar · x cancelar run · PgUp/PgDn scroll del patch · r refrescar · q salir",
        )),
        Mode::Normal => {
            let mut spans = vec![Span::styled("↑↓ ", Style::default().fg(Color::Green))];
            spans.push(Span::raw("navegar · "));
            spans.push(Span::styled("Enter ", Style::default().fg(Color::Green)));
            spans.push(Span::raw("abrir · "));
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
        _ => {
            let status = if app.running.is_some() {
                format!("▶ run en curso… {} · (x cancela)", app.status)
            } else {
                app.status.clone()
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
                "  Flujo Fase 0:  editar → 📌 checkpoint → 🚩 comentar → f feedback → i integrar"
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
