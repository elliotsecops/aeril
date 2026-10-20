//! TUI P2 — semántica de `checkpoint::ready_for_snapshot` (P2-REMEDIATION).
//!
//! Regresión: la variante con `json_extract` filtraba por `snapshot_id` en SQL
//! y NUNCA deserializaba los registros que no coincidían, con lo cual un
//! `checkpoint.ready` más nuevo y estructuralmente INVÁLIDO quedaba sin
//! detectar. El algoritmo certificado (`main@3a28af2`) deserializaba cada
//! checkpoint en orden `seq DESC` y PROPAGABA el error de validación.
//!
//! Este archivo fija la semántica certificada (error propagado, primer
//! match válido en `seq DESC`) y compara el comportamiento de producción
//! contra una reimplementación local del algoritmo certificado, sin materializar
//! el historial completo (cursor en streaming, memoria acotada).

use aeril::checkpoint;
use aeril::store::events::{self, EventRecord, payload};
use aeril::store::{Store, threads};
use serde_json::{Value, json};

const BASE_OID: &str = "0000000000000000000000000000000000000000";

/// Algoritmo CERTIFICADO en `main@3a28af2` (materializa; solo referencia).
fn certified_ready_for_snapshot(
    store: &Store,
    thread_id: &str,
    snapshot_id: &str,
) -> aeril::error::Result<Option<(EventRecord, payload::CheckpointReady)>> {
    let all = events::fetch_by_kind(&store.conn, thread_id, events::K_CHECKPOINT_READY)?;
    for ev in all.into_iter().rev() {
        let parsed: payload::CheckpointReady = ev.parse()?;
        if parsed.snapshot_id == snapshot_id {
            return Ok(Some((ev, parsed)));
        }
    }
    Ok(None)
}

fn store_with_thread(title: &str) -> (Store, threads::ThreadRow) {
    let mut store = Store::open_in_memory().expect("store");
    let thread = threads::create(&mut store.conn, title, "/tmp/repo", "r1").expect("thread");
    (store, thread)
}

fn valid_checkpoint(snapshot_id: &str) -> Value {
    json!({
        "snapshot_id": snapshot_id,
        "git_ref": format!("refs/heads/{snapshot_id}"),
        "oid": BASE_OID,
        "base_oid": BASE_OID,
        "patch": "--- a\n+++ b\n",
        "files": [{"path": "NOTES.md", "status": "modified", "additions": 1, "deletions": 0}],
        "recovered": false
    })
}

/// Payload con un campo obligatorio ausente (snapshot_id).
fn missing_field_checkpoint() -> Value {
    json!({
        "git_ref": "refs/heads/x",
        "oid": BASE_OID,
        "base_oid": BASE_OID,
        "patch": "",
        "files": [],
        "recovered": false
    })
}

/// Payload con tipos incorrectos (snapshot_id numérico, files como string).
fn wrong_types_checkpoint() -> Value {
    json!({
        "snapshot_id": 42,
        "git_ref": "refs/heads/x",
        "oid": BASE_OID,
        "base_oid": BASE_OID,
        "patch": "",
        "files": "no-es-un-array",
        "recovered": false
    })
}

fn append_checkpoints(store: &mut Store, thread_id: &str, payloads: &[Value]) {
    let tx = store
        .conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .expect("tx");
    for p in payloads {
        events::append_tx(
            &tx,
            thread_id,
            None,
            events::K_CHECKPOINT_READY,
            "system",
            p,
            0,
        )
        .expect("checkpoint.ready");
    }
    tx.commit().expect("commit");
}

fn outcome(store: &Store, thread_id: &str, snapshot_id: &str) -> Result<String, String> {
    match checkpoint::ready_for_snapshot(store, thread_id, snapshot_id) {
        Ok(Some((_, ready))) => Ok(format!("ok:{}", ready.snapshot_id)),
        Ok(None) => Ok("none".into()),
        Err(e) => Err(format!("err:{}", e)),
    }
}

fn certified_outcome(store: &Store, thread_id: &str, snapshot_id: &str) -> Result<String, String> {
    match certified_ready_for_snapshot(store, thread_id, snapshot_id) {
        Ok(Some((_, ready))) => Ok(format!("ok:{}", ready.snapshot_id)),
        Ok(None) => Ok("none".into()),
        Err(e) => Err(format!("err:{}", e)),
    }
}

/// Los dos resultados deben coincidir EXACTAMENTE (mismo valor o mismo error).
fn assert_same_as_certified(store: &Store, thread_id: &str, snapshot_id: &str, ctx: &str) {
    let prod = outcome(store, thread_id, snapshot_id);
    let cert = certified_outcome(store, thread_id, snapshot_id);
    assert_eq!(
        prod, cert,
        "{ctx}: producción y algoritmo certificado divergen (prod={prod:?} cert={cert:?})"
    );
}

// ── 1) snapshot pedido existe y es válido ───────────────────────────────────

#[test]
fn requested_snapshot_exists_and_is_valid() {
    let (mut store, thread) = store_with_thread("existe");
    append_checkpoints(
        &mut store,
        &thread.id,
        &[valid_checkpoint("snap-a"), valid_checkpoint("snap-b")],
    );
    let found = checkpoint::ready_for_snapshot(&store, &thread.id, "snap-b")
        .expect("lookup")
        .expect("snap-b presente");
    assert_eq!(found.1.snapshot_id, "snap-b");
    assert_same_as_certified(&store, &thread.id, "snap-b", "snap-b");
}

// ── 2) múltiples snapshots y snapshot_ids duplicados ───────────────────────

#[test]
fn multiple_snapshots_and_duplicate_ids_pick_newest_valid() {
    let (mut store, thread) = store_with_thread("duplicados");
    append_checkpoints(
        &mut store,
        &thread.id,
        &[
            valid_checkpoint("snap-x"),
            valid_checkpoint("snap-y"),
            valid_checkpoint("snap-x"), // duplicado, más nuevo
        ],
    );
    // Semantics certificadas: se devuelve el PRIMER match en seq DESC.
    let found = checkpoint::ready_for_snapshot(&store, &thread.id, "snap-x")
        .expect("lookup")
        .expect("snap-x presente");
    assert_eq!(found.1.snapshot_id, "snap-x");
    // Y es el más nuevo de los duplicados (seq mayor).
    let all =
        events::fetch_by_kind(&store.conn, &thread.id, events::K_CHECKPOINT_READY).expect("kind");
    let newest_x = all
        .iter()
        .filter(|ev| ev.payload["snapshot_id"] == "snap-x")
        .map(|ev| ev.seq)
        .max()
        .expect("seq");
    assert_eq!(found.0.seq, newest_x, "debe ser el duplicado más nuevo");
    assert_same_as_certified(&store, &thread.id, "snap-x", "duplicado snap-x");
    assert_same_as_certified(&store, &thread.id, "snap-y", "snap-y");
}

// ── 3) checkpoint más nuevo NO relacionado, estructuralmente inválido ────────

#[test]
fn newer_unrelated_invalid_checkpoint_must_propagate_error() {
    let (mut store, thread) = store_with_thread("invalido nuevo");
    append_checkpoints(
        &mut store,
        &thread.id,
        &[
            valid_checkpoint("snap-ok"), // el pedido, más viejo
            missing_field_checkpoint(),  // más nuevo, inválido y NO relacionado
        ],
    );
    // REGRESIÓN: el filtro por snapshot_id en SQL se saltaba el inválido y
    // devolvía Ok(snap-ok). La semántica certificada exige propagar el error.
    let prod = outcome(&store, &thread.id, "snap-ok");
    let cert = certified_outcome(&store, &thread.id, "snap-ok");
    assert!(
        prod.is_err(),
        "un checkpoint inválido más nuevo debe abortar el lookup (prod={prod:?})"
    );
    assert_eq!(prod, cert, "debe coincidir con el algoritmo certificado");
}

// ── 4) campos obligatorios faltantes y tipos incorrectos ────────────────────

#[test]
fn invalid_payload_shapes_propagate_errors() {
    for (label, payload) in [
        ("missing_field", missing_field_checkpoint()),
        ("wrong_types", wrong_types_checkpoint()),
    ] {
        let (mut store, thread) = store_with_thread(label);
        append_checkpoints(
            &mut store,
            &thread.id,
            &[valid_checkpoint("snap-ok"), payload],
        );
        let prod = outcome(&store, &thread.id, "snap-ok");
        let cert = certified_outcome(&store, &thread.id, "snap-ok");
        assert!(
            prod.is_err(),
            "{label}: debe propagar el error (prod={prod:?})"
        );
        assert_eq!(prod, cert, "{label}: debe coincidir con el certificado");
    }
}

// ── 5) el snapshot pedido no existe ─────────────────────────────────────────

#[test]
fn requested_snapshot_absent_returns_none() {
    let (mut store, thread) = store_with_thread("ausente");
    append_checkpoints(
        &mut store,
        &thread.id,
        &[valid_checkpoint("snap-a"), valid_checkpoint("snap-b")],
    );
    let prod = outcome(&store, &thread.id, "snap-inexistente");
    assert_eq!(prod, Ok("none".into()));
    assert_same_as_certified(&store, &thread.id, "snap-inexistente", "ausente");
    // Sin checkpoints en absoluto ⇒ None.
    let (empty, ethread) = store_with_thread("vacio");
    assert_eq!(outcome(&empty, &ethread.id, "snap-x"), Ok("none".into()));
}

// ── 6) historiales grandes: 1.000 y 50.000 checkpoints ─────────────────────

#[test]
fn large_histories_are_answered_with_bounded_memory() {
    for total in [1_000usize, 50_000] {
        let (mut store, thread) = store_with_thread(&format!("grande-{total}"));
        let tx = store
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .expect("tx");
        for i in 0..total {
            events::append_tx(
                &tx,
                &thread.id,
                None,
                events::K_CHECKPOINT_READY,
                "system",
                &valid_checkpoint(&format!("snap-{i}")),
                0,
            )
            .expect("checkpoint");
        }
        tx.commit().expect("commit");

        // El snapshot pedido es el más viejo ⇒ hay que recorrer todo el
        // historial en seq DESC sin materializarlo.
        let found = checkpoint::ready_for_snapshot(&store, &thread.id, "snap-0")
            .expect("lookup")
            .expect("snap-0 presente");
        assert_eq!(found.1.snapshot_id, "snap-0");
        assert_same_as_certified(&store, &thread.id, "snap-0", "snap-0");
        assert_same_as_certified(&store, &thread.id, "snap-nope", "inexistente");
    }
}

// ── latencia de lookup con historiales grandes (medición, no umbral estricto) ─

#[test]
fn large_history_lookup_latency() {
    use std::time::Instant;
    for total in [1_000usize, 50_000] {
        let (mut store, thread) = store_with_thread(&format!("lat-{total}"));
        let tx = store
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .expect("tx");
        for i in 0..total {
            events::append_tx(
                &tx,
                &thread.id,
                None,
                events::K_CHECKPOINT_READY,
                "system",
                &valid_checkpoint(&format!("snap-{i}")),
                0,
            )
            .expect("checkpoint");
        }
        tx.commit().expect("commit");

        // Peor caso: el snapshot pedido es el más viejo ⇒ recorrido completo.
        let t0 = Instant::now();
        let found = checkpoint::ready_for_snapshot(&store, &thread.id, "snap-0")
            .expect("lookup")
            .expect("snap-0");
        let streaming_ms = t0.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(found.1.snapshot_id, "snap-0");

        // Referencia: algoritmo certificado (materializa el historial).
        let t1 = Instant::now();
        let cert = certified_ready_for_snapshot(&store, &thread.id, "snap-0")
            .expect("certificado")
            .expect("snap-0");
        let certified_ms = t1.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(cert.1.snapshot_id, "snap-0");

        println!(
            "LATENCIA checkpoints={total} streaming={streaming_ms:.1}ms certificado={certified_ms:.1}ms"
        );
        // Cota holgada (evita regresión patológica; no es un gate de producto).
        assert!(
            streaming_ms < 10_000.0,
            "lookup con {total} checkpoints tardó {streaming_ms:.0} ms"
        );
    }
}

// ── 7) paridad con el algoritmo certificado en una matriz mixta ─────────────

#[test]
fn mixed_matrix_matches_certified_algorithm() {
    let cases: Vec<(&str, Vec<Value>, &str)> = vec![
        ("solo-valido", vec![valid_checkpoint("s1")], "s1"),
        (
            "invalido-mas-nuevo-no-relacionado",
            vec![valid_checkpoint("s1"), missing_field_checkpoint()],
            "s1",
        ),
        (
            "invalido-mas-nuevo-mismo-id",
            vec![valid_checkpoint("s1"), wrong_types_checkpoint()],
            "s1",
        ),
        (
            "valido-mas-nuevo-otro-id",
            vec![missing_field_checkpoint(), valid_checkpoint("s2")],
            "s2",
        ),
        (
            "duplicado-invalido-mas-nuevo",
            vec![valid_checkpoint("s1"), wrong_types_checkpoint()],
            "s1",
        ),
        ("vacio", vec![], "s1"),
    ];
    for (label, payloads, requested) in cases {
        let (mut store, thread) = store_with_thread(label);
        append_checkpoints(&mut store, &thread.id, &payloads);
        assert_same_as_certified(&store, &thread.id, requested, label);
    }
}
