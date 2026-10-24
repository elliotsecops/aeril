//! Revisión: comentarios anclados a snapshots inmutables.
//!
//! Un comentario se ancla a `snapshot_id` + path + lado + rango + blob OIDs +
//! hash del fragmento. NUNCA a una línea "actual". Mostrar el comentario
//! reabre el patch ORIGINAL aunque el archivo haya cambiado y el worktree se
//! haya borrado. Remapear a un snapshot nuevo será una operación aparte que no
//! está implementada en v0.

use crate::checkpoint;
use crate::error::{Error, Result};
use crate::git;
use crate::ids;
use crate::store::Store;
use crate::store::events::{self, EventRecord, payload};
use crate::store::threads;
use crate::util::fnv1a_hex;
use git2::Repository;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentState {
    Open,
    Sent,
    Delivered,
    Resolved,
}

impl CommentState {
    pub fn as_str(self) -> &'static str {
        match self {
            CommentState::Open => "open",
            CommentState::Sent => "sent",
            CommentState::Delivered => "delivered",
            CommentState::Resolved => "resolved",
        }
    }
}

#[derive(Debug)]
pub struct CommentView {
    pub comment_id: String,
    pub seq: i64,
    pub created_ms: i64,
    pub text: String,
    pub anchor: payload::Anchor,
    pub snapshot_oid: String,
    pub state: CommentState,
    /// Patch EXACTO del snapshot anclado (del evento `checkpoint.ready`).
    pub original_patch: String,
    /// Fragmento anclado leído de los objetos inmutables (si el repo está).
    pub anchored_text: Option<String>,
    /// `true` si el blob actual del snapshot coincide con el OID anclado.
    pub blob_verified: bool,
}

fn lines_range(content: &str, start: u64, end: u64) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let (s, e) = (start as usize, end as usize);
    if s == 0 || e < s || e > lines.len() {
        return None;
    }
    Some(lines[(s - 1)..e].join("\n"))
}

/// Agrega un comentario contra un checkpoint `ready`. Falla si el checkpoint
/// no está listo: los comentarios solo se habilitan tras `ready`.
#[allow(clippy::too_many_arguments)]
pub fn add_comment(
    store: &mut Store,
    repo: &Repository,
    thread_id: &str,
    checkpoint_event_id: &str,
    path: &str,
    start_line: u64,
    end_line: u64,
    text: &str,
) -> Result<String> {
    if text.trim().is_empty() {
        return Err(Error::invalid("el comentario no puede estar vacío"));
    }
    if path.is_empty() {
        return Err(Error::invalid("path vacío"));
    }
    if start_line == 0 || end_line < start_line {
        return Err(Error::invalid("rango de líneas inválido"));
    }
    let ev: EventRecord = events::fetch_by_id(&store.conn, thread_id, checkpoint_event_id)?
        .ok_or_else(|| Error::not_found(format!("evento {checkpoint_event_id}")))?;
    if ev.kind != events::K_CHECKPOINT_READY {
        return Err(Error::not_ready(format!(
            "el evento {} es `{}`, no `checkpoint.ready`: los comentarios solo se crean contra checkpoints listos",
            ev.id, ev.kind
        )));
    }
    let ready: payload::CheckpointReady = ev.parse()?;

    let file = ready.files.iter().find(|f| f.path == path).ok_or_else(|| {
        Error::invalid(format!(
            "el snapshot {} no incluye el path `{path}`",
            ready.snapshot_id
        ))
    })?;
    let after_oid = file.after_blob_oid.clone().ok_or_else(|| {
        Error::unsupported(format!(
            "v0 ancla solo el lado nuevo: `{path}` no tiene contenido posterior (¿archivo borrado?)"
        ))
    })?;

    let Some((oid, bytes)) = git::diff::file_at(repo, &ready.oid, path)? else {
        return Err(Error::integrity(format!(
            "`{path}` declarado por el snapshot no está en el tree de {}",
            ready.oid
        )));
    };
    if oid != after_oid {
        return Err(Error::integrity(format!(
            "`{path}`: el blob en el snapshot es {oid} y el resumen declara {after_oid}"
        )));
    }
    let content = String::from_utf8_lossy(&bytes).into_owned();
    let line_count = content.lines().count() as u64;
    if end_line > line_count {
        return Err(Error::invalid(format!(
            "rango {start_line}-{end_line} fuera del archivo en el snapshot ({line_count} líneas)"
        )));
    }
    let anchored = lines_range(&content, start_line, end_line)
        .ok_or_else(|| Error::invalid("rango inválido para el contenido anclado"))?;
    let context_hash = fnv1a_hex(anchored.as_bytes());

    let anchor = payload::Anchor {
        snapshot_id: ready.snapshot_id.clone(),
        path: path.to_string(),
        side: "new".into(),
        start_line,
        end_line,
        before_blob_oid: file.before_blob_oid.clone(),
        after_blob_oid: Some(after_oid),
        context_hash,
    };
    let added = payload::CommentAdded {
        checkpoint_event_id: ev.id.clone(),
        oid: ready.oid.clone(),
        anchor,
        text: text.to_string(),
    };
    let (id, _) = events::append_typed(
        &mut store.conn,
        thread_id,
        None,
        events::K_COMMENT_ADDED,
        "human",
        &added,
    )?;
    Ok(id)
}

/// Estados derivados de eventos de feedback/resolución (escaneo acotado).
fn states_for(conn: &rusqlite::Connection, thread_id: &str) -> Result<Vec<(String, CommentState)>> {
    let mut out: Vec<(String, CommentState)> = Vec::new();
    let mut set_state = |id: &str, state: CommentState| {
        if let Some(entry) = out.iter_mut().find(|(cid, _)| cid == id) {
            // Resolved es terminal; Delivered no pisa a Resolved.
            if entry.1 != CommentState::Resolved {
                entry.1 = state;
            }
        } else {
            out.push((id.to_string(), state));
        }
    };
    for ev in events::fetch_by_kind(conn, thread_id, events::K_FEEDBACK_SENT)? {
        let p: payload::FeedbackSent = ev.parse()?;
        for cid in p.comment_ids {
            set_state(&cid, CommentState::Sent);
        }
    }
    for ev in events::fetch_by_kind(conn, thread_id, events::K_FEEDBACK_DELIVERED)? {
        let p: payload::FeedbackDelivered = ev.parse()?;
        for cid in p.comment_ids {
            set_state(&cid, CommentState::Delivered);
        }
    }
    for ev in events::fetch_by_kind(conn, thread_id, events::K_COMMENT_RESOLVED)? {
        let p: payload::CommentResolved = ev.parse()?;
        set_state(&p.comment_id, CommentState::Resolved);
    }
    Ok(out)
}

/// Resumen liviano de un comentario: SIN patch ni contenido anclado.
/// Es lo que carga la lista/timeline; el patch se lee solo para el seleccionado
/// vía [`view_comment`] (memoria de revisión acotada, B2).
#[derive(Debug)]
pub struct CommentSummary {
    pub comment_id: String,
    pub seq: i64,
    pub created_ms: i64,
    pub text: String,
    pub anchor: payload::Anchor,
    pub snapshot_oid: String,
    pub state: CommentState,
}

/// Página de resúmenes (orden ascendente por seq).
pub fn list_comment_summaries(
    store: &Store,
    thread_id: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<CommentSummary>> {
    let states = states_for(&store.conn, thread_id)?;
    let mut out = Vec::new();
    for ev in events::fetch_by_kind(&store.conn, thread_id, events::K_COMMENT_ADDED)? {
        let added: payload::CommentAdded = ev.parse()?;
        let state = states
            .iter()
            .find(|(cid, _)| *cid == ev.id)
            .map(|(_, s)| *s)
            .unwrap_or(CommentState::Open);
        out.push(CommentSummary {
            comment_id: ev.id.clone(),
            seq: ev.seq,
            created_ms: ev.created_ms,
            text: added.text.clone(),
            anchor: added.anchor.clone(),
            snapshot_oid: added.oid.clone(),
            state,
        });
    }
    let start = offset.max(0) as usize;
    let end = (start + limit.max(0) as usize).min(out.len());
    Ok(if start >= out.len() {
        Vec::new()
    } else {
        out.drain(..).skip(start).take(end - start).collect()
    })
}

pub fn view_comment(
    store: &Store,
    thread_id: &str,
    comment_id: &str,
    repo: Option<&Repository>,
) -> Result<CommentView> {
    let ev = events::fetch_by_id(&store.conn, thread_id, comment_id)?
        .ok_or_else(|| Error::not_found(format!("comentario {comment_id}")))?;
    if ev.kind != events::K_COMMENT_ADDED {
        return Err(Error::invalid(format!(
            "el evento {} no es un comentario",
            ev.id
        )));
    }
    let added: payload::CommentAdded = ev.parse()?;
    let ready_ev = events::fetch_by_id(&store.conn, thread_id, &added.checkpoint_event_id)?
        .ok_or_else(|| Error::integrity("el evento de checkpoint del comentario no existe"))?;
    let ready: payload::CheckpointReady = ready_ev.parse()?;

    let mut anchored_text = None;
    let mut blob_verified = false;
    if let Some(repo) = repo
        && let Some((oid, bytes)) = git::diff::file_at(repo, &ready.oid, &added.anchor.path)?
    {
        let content = String::from_utf8_lossy(&bytes).into_owned();
        if let Some(expected) = &added.anchor.after_blob_oid {
            blob_verified = oid == *expected;
        }
        anchored_text = lines_range(&content, added.anchor.start_line, added.anchor.end_line);
    }

    let state = states_for(&store.conn, thread_id)?
        .into_iter()
        .find(|(cid, _)| cid == comment_id)
        .map(|(_, s)| s)
        .unwrap_or(CommentState::Open);

    Ok(CommentView {
        comment_id: ev.id.clone(),
        seq: ev.seq,
        created_ms: ev.created_ms,
        text: added.text.clone(),
        anchor: added.anchor.clone(),
        snapshot_oid: ready.oid.clone(),
        state,
        original_patch: ready.patch.clone(),
        anchored_text,
        blob_verified,
    })
}

/// IDs de comentarios sin resolver (para el envío de feedback).
pub fn open_comment_ids(store: &Store, thread_id: &str) -> Result<Vec<String>> {
    let states = states_for(&store.conn, thread_id)?;
    let mut out = Vec::new();
    for ev in events::fetch_by_kind(&store.conn, thread_id, events::K_COMMENT_ADDED)? {
        let state = states
            .iter()
            .find(|(cid, _)| *cid == ev.id)
            .map(|(_, s)| *s)
            .unwrap_or(CommentState::Open);
        if state != CommentState::Resolved {
            out.push(ev.id);
        }
    }
    Ok(out)
}

/// IDs de comentarios NO resueltos anclados EXACTAMENTE a `snapshot_id` y
/// `expected_oid`. Comentarios históricos de otros snapshots no participan.
/// Evidencia malformada o con OID contradictorio falla cerrada.
fn open_comment_ids_for_snapshot_conn(
    conn: &rusqlite::Connection,
    thread_id: &str,
    snapshot_id: &str,
    expected_oid: &str,
) -> Result<Vec<String>> {
    let states = states_for(conn, thread_id)?;
    let mut stmt = conn.prepare(
        "SELECT id, payload FROM events
          WHERE thread_id = ?1 AND kind = ?2
          ORDER BY seq ASC",
    )?;
    let mut rows = stmt.query(rusqlite::params![thread_id, events::K_COMMENT_ADDED])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let payload_text: String = row.get(1)?;
        let added: payload::CommentAdded = serde_json::from_str(&payload_text)
            .map_err(|e| Error::integrity(format!("comment.added {id} ilegible: {e}")))?;
        // Los comentarios de otros snapshots son históricos y no participan.
        // Un comentario del snapshot exacto con OID contradictorio falla
        // cerrado; un anchor ausente ya falló al deserializar.
        if added.anchor.snapshot_id != snapshot_id {
            continue;
        }
        if added.oid != expected_oid {
            return Err(Error::integrity(format!(
                "comment.added {id} no corresponde al snapshot {snapshot_id} / OID {expected_oid}"
            )));
        }
        let state = states
            .iter()
            .find(|(cid, _)| *cid == id)
            .map(|(_, s)| *s)
            .unwrap_or(CommentState::Open);
        if state != CommentState::Resolved {
            out.push(id);
        }
    }
    Ok(out)
}

/// IDs de comentarios NO resueltos anclados EXACTAMENTE a `snapshot_id` y
/// `expected_oid`. Comentarios históricos de otros snapshots no participan.
/// Evidencia malformada o con OID contradictorio falla cerrada.
pub fn open_comment_ids_for_snapshot(
    store: &Store,
    thread_id: &str,
    snapshot_id: &str,
    expected_oid: &str,
) -> Result<Vec<String>> {
    open_comment_ids_for_snapshot_conn(&store.conn, thread_id, snapshot_id, expected_oid)
}

/// Marca explícitamente un checkpoint como revisado por un humano. Es un
/// hecho durable exacto al snapshot/OID, no una inferencia por ausencia de
/// comentarios. Idempotente para el mismo snapshot/OID.
pub fn acknowledge(
    store: &mut Store,
    thread_id: &str,
    snapshot_id: &str,
    oid: &str,
) -> Result<bool> {
    // El acknowledgement solo puede referirse a un checkpoint listo, con
    // prepared consistente, sin marcador failed y con ref/objetos Git
    // verificables. Una llamada de biblioteca no puede inventar identidades.
    let (ready_ev, ready) = checkpoint::ready_for_snapshot(store, thread_id, snapshot_id)?
        .ok_or_else(|| Error::not_ready(format!("no hay checkpoint ready {snapshot_id}")))?;
    if ready.oid != oid {
        return Err(Error::integrity(format!(
            "el checkpoint {snapshot_id} tiene OID {}, no {oid}",
            ready.oid
        )));
    }
    if ready.git_ref != ids::checkpoint_ref(snapshot_id) {
        return Err(Error::integrity(format!(
            "ref de checkpoint inesperada para {snapshot_id}: {}",
            ready.git_ref
        )));
    }
    let (prepared_run_id, prepared) =
        checkpoint::prepared_for_snapshot_with_run(store, thread_id, snapshot_id)?.ok_or_else(
            || {
                Error::not_ready(format!(
                    "no hay checkpoint.prepared verificable para {snapshot_id}"
                ))
            },
        )?;
    if prepared.snapshot_id != snapshot_id
        || prepared.expected_oid != ready.oid
        || prepared.git_ref != ready.git_ref
        || prepared.base_oid != ready.base_oid
        || prepared.files != ready.files
    {
        return Err(Error::integrity(format!(
            "checkpoint.prepared y checkpoint.ready se contradicen para {snapshot_id}"
        )));
    }
    for (left, right) in [
        (ready_ev.run_id.as_deref(), prepared_run_id.as_deref()),
        (ready_ev.run_id.as_deref(), prepared.run_id.as_deref()),
        (prepared_run_id.as_deref(), prepared.run_id.as_deref()),
    ] {
        if let (Some(left), Some(right)) = (left, right)
            && left != right
        {
            return Err(Error::integrity(format!(
                "run_id contradictorio entre checkpoint.ready y checkpoint.prepared para {snapshot_id}"
            )));
        }
    }
    if checkpoint::has_failed_marker(store, thread_id, snapshot_id)? {
        return Err(Error::integrity(format!(
            "el snapshot {snapshot_id} tiene un marcador checkpoint.failed"
        )));
    }
    let thread = threads::get(&store.conn, thread_id)?
        .ok_or_else(|| Error::not_found(format!("thread {thread_id}")))?;
    let repo = git::open(Path::new(&thread.repo_path))?;
    git::snapshot::verify_snapshot(&repo, &ready.git_ref, &ready.oid, &ready.files)?;
    let source = git2::Oid::from_str(&ready.oid)?;
    let base = git2::Oid::from_str(&ready.base_oid)?;
    repo.find_commit(base)
        .map_err(|e| Error::integrity(format!("commit base {} ausente: {e}", ready.base_oid)))?;
    if !repo
        .graph_descendant_of(source, base)
        .map_err(|e| Error::integrity(format!("no se pudo comprobar la base: {e}")))?
    {
        return Err(Error::integrity(format!(
            "el checkpoint {snapshot_id} no desciende de su base {}",
            ready.base_oid
        )));
    }

    // El chequeo de comentarios y la escritura del ack comparten la misma
    // transacción IMMEDIATE: un comentario no puede colarse entre ambos.
    let tx = store
        .conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let open = open_comment_ids_for_snapshot_conn(&tx, thread_id, snapshot_id, oid)?;
    if !open.is_empty() {
        return Err(Error::conflict(format!(
            "hay {} comentario(s) sin resolver en el snapshot; resolvelos antes de marcarlo revisado",
            open.len()
        )));
    }
    let failed_now: i64 = tx.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM events
            WHERE thread_id = ?1 AND kind = ?2
              AND json_extract(payload, '$.snapshot_id') = ?3
        )",
        rusqlite::params![thread_id, events::K_CHECKPOINT_FAILED, snapshot_id],
        |row| row.get(0),
    )?;
    if failed_now == 1 {
        return Err(Error::integrity(format!(
            "el snapshot {snapshot_id} recibió un marcador checkpoint.failed durante la revisión"
        )));
    }
    let exists: i64 = tx.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM events
            WHERE thread_id = ?1 AND kind = ?2
              AND json_extract(payload, '$.snapshot_id') = ?3
              AND json_extract(payload, '$.oid') = ?4
        )",
        rusqlite::params![thread_id, events::K_REVIEW_ACKNOWLEDGED, snapshot_id, oid],
        |row| row.get(0),
    )?;
    if exists == 1 {
        tx.rollback()?;
        return Ok(false);
    }
    events::append_tx(
        &tx,
        thread_id,
        None,
        events::K_REVIEW_ACKNOWLEDGED,
        "human",
        &serde_json::to_value(payload::ReviewAcknowledged {
            snapshot_id: snapshot_id.to_string(),
            oid: oid.to_string(),
        })?,
        crate::timeutil::now_ms(),
    )?;
    tx.commit()?;
    Ok(true)
}

/// ¿El snapshot/OID exacto tiene un acknowledgement humano durable?
///
/// No depende del `seq`: el acknowledgement identifica bytes inmutables
/// (snapshot + OID). Un ready recuperado con la misma identidad no invalida
/// una revisión ya registrada.
pub fn acknowledged(store: &Store, thread_id: &str, snapshot_id: &str, oid: &str) -> Result<bool> {
    let exists: i64 = store.conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM events
            WHERE thread_id = ?1 AND kind = ?2
              AND json_extract(payload, '$.snapshot_id') = ?3
              AND json_extract(payload, '$.oid') = ?4
        )",
        rusqlite::params![thread_id, events::K_REVIEW_ACKNOWLEDGED, snapshot_id, oid],
        |row| row.get(0),
    )?;
    Ok(exists == 1)
}

pub fn resolve(store: &mut Store, thread_id: &str, comment_id: &str) -> Result<()> {
    let ev = events::fetch_by_id(&store.conn, thread_id, comment_id)?
        .ok_or_else(|| Error::not_found(format!("comentario {comment_id}")))?;
    if ev.kind != events::K_COMMENT_ADDED {
        return Err(Error::invalid(format!(
            "el evento {comment_id} no es un comentario"
        )));
    }
    let resolved = payload::CommentResolved {
        comment_id: comment_id.to_string(),
    };
    events::append_typed(
        &mut store.conn,
        thread_id,
        None,
        events::K_COMMENT_RESOLVED,
        "human",
        &resolved,
    )?;
    Ok(())
}
