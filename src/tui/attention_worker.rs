//! Evaluación progresiva de Attention para la TUI (`PA0-PERF-R`).
//!
//! Mantiene separación estricta entre:
//! - `AttentionStatus`: estado derivado de evidencia durable;
//! - `EvaluationState`: si esa evidencia fue evaluada y sigue vigente.
//!
//! Un único worker dentro del proceso TUI evalúa en lotes pequeños usando una
//! conexión SQLite de solo lectura independiente y valida watermarks
//! (`threads.next_seq`) antes de publicar resultados. Las acciones sensibles
//! siguen ejecutando sus preflights autoritativos propios.

use crate::attention::{self, TaskAttention};
use crate::error::Result;
use crate::store::Store;
use crate::store::threads;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const WORKER_BATCH_SIZE: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvaluationState {
    Pending,
    Verified { through_seq: i64 },
    Error { detail: String },
}

#[derive(Debug, Clone)]
pub struct AttentionView {
    pub state: EvaluationState,
    pub attention: Option<TaskAttention>,
}

impl AttentionView {
    pub fn pending() -> Self {
        AttentionView {
            state: EvaluationState::Pending,
            attention: None,
        }
    }

    pub fn verified(attention: TaskAttention, through_seq: i64) -> Self {
        AttentionView {
            state: EvaluationState::Verified { through_seq },
            attention: Some(attention),
        }
    }

    pub fn error(detail: impl Into<String>) -> Self {
        AttentionView {
            state: EvaluationState::Error {
                detail: detail.into(),
            },
            attention: None,
        }
    }

    pub fn is_verified(&self) -> bool {
        matches!(self.state, EvaluationState::Verified { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AttentionCoverage {
    pub total: usize,
    pub verified: usize,
    pub pending: usize,
    pub errors: usize,
}

#[derive(Debug)]
pub enum WorkerOutcome {
    Verified {
        attention: Box<TaskAttention>,
        through_seq: i64,
    },
    Stale,
    Error {
        detail: String,
    },
}

#[derive(Debug)]
pub enum WorkerEvent {
    Result {
        thread_id: String,
        generation: u64,
        attempts: u8,
        outcome: WorkerOutcome,
    },
    Fatal {
        detail: String,
    },
}

#[derive(Debug)]
pub struct WorkerJob {
    pub thread_id: String,
    pub priority: u8,
    pub generation: u64,
    pub attempts: u8,
    seq: u64,
}

impl PartialEq for WorkerJob {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.seq == other.seq
    }
}
impl Eq for WorkerJob {}
impl PartialOrd for WorkerJob {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for WorkerJob {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap es max-heap: menor prioridad numérica primero.
        other
            .priority
            .cmp(&self.priority)
            .then_with(|| other.seq.cmp(&self.seq))
            .then_with(|| self.thread_id.cmp(&other.thread_id))
    }
}

pub struct AttentionWorker {
    tx: Option<mpsc::Sender<WorkerJob>>,
    cancel: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    generations: Arc<Mutex<HashMap<String, u64>>>,
}

impl AttentionWorker {
    pub fn spawn(
        db_path: PathBuf,
        generations: Arc<Mutex<HashMap<String, u64>>>,
        tx: mpsc::Sender<WorkerJob>,
        rx: mpsc::Receiver<WorkerJob>,
        events: mpsc::Sender<WorkerEvent>,
    ) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(true));
        let cancel_worker = cancel.clone();
        let paused_worker = paused.clone();
        let generations_worker = generations.clone();
        let handle = thread::spawn(move || {
            worker_loop(
                db_path,
                generations_worker,
                rx,
                events,
                cancel_worker,
                paused_worker,
            );
        });
        AttentionWorker {
            tx: Some(tx),
            cancel,
            paused,
            handle: Some(handle),
            generations,
        }
    }

    pub fn submit(&self, thread_id: String, priority: u8, generation: u64, attempts: u8) {
        let Some(tx) = self.tx.as_ref() else {
            return;
        };
        let _ = tx.send(WorkerJob {
            thread_id,
            priority,
            generation,
            attempts,
            seq: job_seq(),
        });
    }

    pub fn resume(&self) {
        self.paused.store(false, AtomicOrdering::Relaxed);
    }

    pub fn generation(&self, thread_id: &str) -> u64 {
        self.generations
            .lock()
            .ok()
            .and_then(|m| m.get(thread_id).copied())
            .unwrap_or(0)
    }

    pub fn invalidate(&self, thread_id: &str) -> u64 {
        let mut guard = match self.generations.lock() {
            Ok(g) => g,
            Err(_) => return 0,
        };
        let next = guard.get(thread_id).copied().unwrap_or(0) + 1;
        guard.insert(thread_id.to_string(), next);
        next
    }
}

impl Drop for AttentionWorker {
    fn drop(&mut self) {
        self.cancel.store(true, AtomicOrdering::Relaxed);
        self.tx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn job_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

fn drain_incoming(rx: &mpsc::Receiver<WorkerJob>, heap: &mut BinaryHeap<WorkerJob>) -> bool {
    loop {
        match rx.try_recv() {
            Ok(job) => heap.push(job),
            Err(mpsc::TryRecvError::Empty) => return true,
            Err(mpsc::TryRecvError::Disconnected) => return false,
        }
    }
}

fn worker_loop(
    db_path: PathBuf,
    generations: Arc<Mutex<HashMap<String, u64>>>,
    rx: mpsc::Receiver<WorkerJob>,
    events: mpsc::Sender<WorkerEvent>,
    cancel: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
) {
    let store = match Store::open_read_only(&db_path) {
        Ok(s) => s,
        Err(e) => {
            let _ = events.send(WorkerEvent::Fatal {
                detail: format!("no se pudo abrir la base para Attention: {e}"),
            });
            return;
        }
    };
    let mut heap: BinaryHeap<WorkerJob> = BinaryHeap::new();
    let mut batch_count: usize = 0;
    let mut batch_had_background = false;
    loop {
        if cancel.load(AtomicOrdering::Relaxed) {
            break;
        }
        if !drain_incoming(&rx, &mut heap) && heap.is_empty() {
            break;
        }
        let Some(job) = heap.pop() else {
            batch_count = 0;
            batch_had_background = false;
            match rx.recv() {
                Ok(job) => {
                    heap.push(job);
                    continue;
                }
                Err(_) => break,
            }
        };
        while paused.load(AtomicOrdering::Relaxed) && !cancel.load(AtomicOrdering::Relaxed) {
            thread::sleep(Duration::from_millis(2));
        }
        if cancel.load(AtomicOrdering::Relaxed) {
            break;
        }
        let current = generations
            .lock()
            .ok()
            .and_then(|m| m.get(&job.thread_id).copied());
        if current != Some(job.generation) {
            continue;
        }
        let outcome = evaluate(&store, &job.thread_id);
        if cancel.load(AtomicOrdering::Relaxed) {
            break;
        }
        if events
            .send(WorkerEvent::Result {
                thread_id: job.thread_id.clone(),
                generation: job.generation,
                attempts: job.attempts,
                outcome,
            })
            .is_err()
        {
            break;
        }
        if job.priority > 1 {
            batch_had_background = true;
        }
        batch_count += 1;
        if batch_count >= WORKER_BATCH_SIZE {
            batch_count = 0;
            if batch_had_background {
                batch_had_background = false;
                thread::yield_now();
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

fn evaluate(store: &Store, thread_id: &str) -> WorkerOutcome {
    let before = match threads::next_seq(&store.conn, thread_id) {
        Ok(seq) => seq,
        Err(e) => {
            return WorkerOutcome::Error {
                detail: e.to_string(),
            };
        }
    };
    let attention = match attention::project_thread(store, thread_id) {
        Ok(a) => a,
        Err(e) => {
            return WorkerOutcome::Error {
                detail: e.to_string(),
            };
        }
    };
    let after = match threads::next_seq(&store.conn, thread_id) {
        Ok(seq) => seq,
        Err(e) => {
            return WorkerOutcome::Error {
                detail: e.to_string(),
            };
        }
    };
    if before != after {
        WorkerOutcome::Stale
    } else {
        WorkerOutcome::Verified {
            attention: Box::new(attention),
            through_seq: after,
        }
    }
}

/// Evalúa sincrónicamente una tarea y solo la publica si el watermark no
/// cambió durante la lectura. Si hay mutación concurrente devuelve `Pending`
/// para que el worker la re-evalúe.
pub fn evaluate_stable(store: &Store, thread_id: &str) -> Result<AttentionView> {
    let before = threads::next_seq(&store.conn, thread_id)?;
    let attention = attention::project_thread(store, thread_id)?;
    let after = threads::next_seq(&store.conn, thread_id)?;
    if before != after {
        return Ok(AttentionView::pending());
    }
    Ok(AttentionView::verified(attention, after))
}

/// Capacidad de filas de tarea visibles (incluyendo fila parcial al pie) para
/// el layout real de `render_tasks`. Garantiza que toda fila visible en el
/// arranque pueda quedar verificada antes de presentarse como válida.
pub fn visible_task_capacity(width: u16, height: u16) -> usize {
    if width == 0 || height == 0 {
        return 1;
    }
    let body_height = height.saturating_sub(3 + 2);
    if body_height <= 5 {
        return 1;
    }
    let list_width = if width < 110 {
        width.saturating_mul(36) / 100
    } else {
        width.saturating_mul(28) / 100
    };
    let margin_h = if list_width < 38 { 1 } else { 2 };
    let inner_width = list_width.saturating_sub(2 + margin_h * 2);
    let list_height = body_height.saturating_sub(5);
    let compact = inner_width < 30;
    let show_branch_inline = inner_width >= 36;
    let lines_per_task = if compact {
        3
    } else if show_branch_inline {
        4
    } else {
        5
    };
    (list_height as usize).div_ceil(lines_per_task).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::events;
    use crate::store::{Store, threads};
    use serde_json::json;

    fn thread(store: &mut Store) -> String {
        threads::create(&mut store.conn, "progressive", "/tmp/repo", "r1")
            .expect("thread")
            .id
    }

    #[test]
    fn visible_capacity_is_bounded_and_derived_from_size() {
        assert_eq!(visible_task_capacity(0, 0), 1);
        for (w, h) in [(80, 24), (120, 40), (180, 50)] {
            let capacity = visible_task_capacity(w, h);
            assert!(capacity >= 1, "{w}x{h}");
            assert!(capacity <= 20, "{w}x{h}");
        }
    }

    #[test]
    fn evaluate_stable_tracks_watermark_and_ignores_no_run() {
        let mut store = Store::open_in_memory().expect("store");
        let id = thread(&mut store);
        let first = evaluate_stable(&store, &id).expect("first");
        assert!(matches!(
            first.state,
            EvaluationState::Verified { through_seq: 1 }
        ));
        assert!(first.attention.is_some());
        events::append(
            &mut store.conn,
            &id,
            None,
            events::K_AGENT_MESSAGE,
            "system",
            &json!({"text": "nuevo"}),
        )
        .expect("event");
        let second = evaluate_stable(&store, &id).expect("second");
        assert!(matches!(
            second.state,
            EvaluationState::Verified { through_seq: 2 }
        ));
    }

    #[test]
    fn evaluate_stable_propagates_projection_errors() {
        let mut store = Store::open_in_memory().expect("store");
        let id = thread(&mut store);
        events::append(
            &mut store.conn,
            &id,
            None,
            events::K_INTEGRATION_BLOCKED,
            "system",
            &json!({}),
        )
        .expect("event");
        assert!(evaluate_stable(&store, &id).is_err());
    }

    #[test]
    fn pending_and_error_do_not_carry_attention_results() {
        assert!(AttentionView::pending().attention.is_none());
        assert!(AttentionView::error("worker").attention.is_none());
    }

    #[test]
    fn read_only_worker_connection_rejects_writes() {
        let dir = std::env::temp_dir().join(format!("aeril-ro-{}", crate::ids::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("aeril.db");
        {
            let mut rw = Store::open(&db).expect("open rw");
            let _ = thread(&mut rw);
        }
        let ro = Store::open_read_only(&db).expect("open ro");
        let write_attempt = ro.conn.execute(
            "INSERT INTO threads (id, title, repo_id, repo_path, state, next_seq, created_ms, updated_ms)
             VALUES ('x', 'y', 'r', '/tmp', 'open', 0, 1, 1)",
            [],
        );
        assert!(
            write_attempt.is_err(),
            "read-only connection must reject writes"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
