//! Revisión: comentarios anclados a snapshots inmutables.
//!
//! Un comentario se ancla a `snapshot_id` + path + lado + rango + blob OIDs +
//! hash del fragmento. NUNCA a una línea "actual". Mostrar el comentario
//! reabre el patch ORIGINAL aunque el archivo haya cambiado y el worktree se
//! haya borrado. Remapear a un snapshot nuevo será una operación aparte que no
//! está implementada en v0.

use crate::error::{Error, Result};
use crate::git;
use crate::store::Store;
use crate::store::events::{self, EventRecord, payload};
use crate::util::fnv1a_hex;
use git2::Repository;

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
fn states_for(store: &Store, thread_id: &str) -> Result<Vec<(String, CommentState)>> {
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
    for ev in events::fetch_by_kind(&store.conn, thread_id, events::K_FEEDBACK_SENT)? {
        let p: payload::FeedbackSent = ev.parse()?;
        for cid in p.comment_ids {
            set_state(&cid, CommentState::Sent);
        }
    }
    for ev in events::fetch_by_kind(&store.conn, thread_id, events::K_FEEDBACK_DELIVERED)? {
        let p: payload::FeedbackDelivered = ev.parse()?;
        for cid in p.comment_ids {
            set_state(&cid, CommentState::Delivered);
        }
    }
    for ev in events::fetch_by_kind(&store.conn, thread_id, events::K_COMMENT_RESOLVED)? {
        let p: payload::CommentResolved = ev.parse()?;
        set_state(&p.comment_id, CommentState::Resolved);
    }
    Ok(out)
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
    if let Some(repo) = repo {
        if let Some((oid, bytes)) = git::diff::file_at(repo, &ready.oid, &added.anchor.path)? {
            let content = String::from_utf8_lossy(&bytes).into_owned();
            if let Some(expected) = &added.anchor.after_blob_oid {
                blob_verified = oid == *expected;
            }
            anchored_text = lines_range(&content, added.anchor.start_line, added.anchor.end_line);
        }
    }

    let state = states_for(store, thread_id)?
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

pub fn list_comments(
    store: &Store,
    thread_id: &str,
    repo: Option<&Repository>,
) -> Result<Vec<CommentView>> {
    let mut out = Vec::new();
    for ev in events::fetch_by_kind(&store.conn, thread_id, events::K_COMMENT_ADDED)? {
        out.push(view_comment(store, thread_id, &ev.id, repo)?);
    }
    Ok(out)
}

/// IDs de comentarios sin resolver (para el envío de feedback).
pub fn open_comment_ids(store: &Store, thread_id: &str) -> Result<Vec<String>> {
    let states = states_for(store, thread_id)?;
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
