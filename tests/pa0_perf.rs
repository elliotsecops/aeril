//! PA0-PERF-1: regresión del lookup `checkpoint.prepared`.
//!
//! Fija la semántica del lookup mientras el plan usa explícitamente
//! `idx_events_checkpoint_snapshot`. No mide tiempos en CI: el A/B de arranque
//! se ejecuta con el protocolo externo de `scripts/bench/startup.py`.

use aeril::attention::{self, AttentionStatus};
use aeril::checkpoint;
use aeril::store::events::{self, payload};
use aeril::store::{Store, threads};
use rusqlite::params;
use serde_json::json;

const BASE: &str = "0000000000000000000000000000000000000000";
const OID_A: &str = "1111111111111111111111111111111111111111";
const OID_B: &str = "2222222222222222222222222222222222222222";

fn ref_for(snapshot: &str) -> String {
    format!("refs/aeril/checkpoints/{snapshot}")
}

fn prepared_payload(snapshot: &str, oid: &str, run_id: Option<&str>) -> serde_json::Value {
    let mut value = json!({
        "snapshot_id": snapshot,
        "git_ref": ref_for(snapshot),
        "expected_oid": oid,
        "base_oid": BASE,
        "parent_oid": BASE,
        "files": []
    });
    if let Some(run_id) = run_id {
        value["run_id"] = json!(run_id);
    }
    value
}

fn ready_payload(snapshot: &str, oid: &str) -> serde_json::Value {
    json!({
        "snapshot_id": snapshot,
        "git_ref": ref_for(snapshot),
        "oid": oid,
        "base_oid": BASE,
        "patch": "",
        "files": [],
        "recovered": false
    })
}

fn append(
    store: &mut Store,
    thread: &str,
    kind: &str,
    payload: serde_json::Value,
) -> (String, i64) {
    events::append(&mut store.conn, thread, None, kind, "system", &payload).expect("event")
}

fn new_thread(store: &mut Store, title: &str) -> String {
    threads::create(&mut store.conn, title, "/tmp/repo", "r1")
        .expect("thread")
        .id
}

#[test]
fn prepared_lookup_is_correct_on_large_multi_thread_dataset() {
    let mut store = Store::open_in_memory().expect("store");
    let mut threads = Vec::new();
    for i in 0..100 {
        threads.push(new_thread(&mut store, &format!("thread-{i}")));
    }

    let tx = store.conn.transaction().expect("tx");
    {
        let mut fill = tx
            .prepare(
                "INSERT INTO events
                    (id, thread_id, seq, run_id, kind, actor, payload_version, payload, created_ms)
                 VALUES (?1, ?2, ?3, NULL, 'agent.message', 'system', 1, '{\"text\":\"x\"}', 0)",
            )
            .expect("prepare fill");
        let mut prepared = tx
            .prepare(
                "INSERT INTO events
                    (id, thread_id, seq, run_id, kind, actor, payload_version, payload, created_ms)
                 VALUES (?1, ?2, ?3, NULL, ?4, 'system', 1, ?5, 0)",
            )
            .expect("prepare checkpoint");
        for (i, thread) in threads.iter().enumerate() {
            for seq in 1..=1000 {
                fill.execute(params![format!("fill-{i}-{seq}"), thread, seq as i64])
                    .expect("filler");
            }
            let snapshot = format!("snap-{i}");
            let value = prepared_payload(&snapshot, OID_A, Some(&format!("run-{i}")));
            prepared
                .execute(params![
                    format!("prepared-{i}"),
                    thread,
                    1001_i64,
                    events::K_CHECKPOINT_PREPARED,
                    serde_json::to_string(&value).expect("json")
                ])
                .expect("prepared");
        }
    }
    tx.commit().expect("commit");

    for (i, thread) in threads.iter().enumerate() {
        let snapshot = format!("snap-{i}");
        let (run_id, prepared) =
            checkpoint::prepared_for_snapshot_with_run(&store, thread, &snapshot)
                .expect("lookup")
                .expect("prepared");
        assert_eq!(prepared.snapshot_id, snapshot);
        assert_eq!(prepared.expected_oid, OID_A);
        assert_eq!(prepared.run_id, Some(format!("run-{i}")));
        assert!(run_id.is_none(), "la columna run_id del evento es NULL");
    }
}

#[test]
fn duplicate_prepared_selects_latest_seq() {
    let mut store = Store::open_in_memory().expect("store");
    let thread = new_thread(&mut store, "duplicado");
    let (_, old_seq) = append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        prepared_payload("snap-x", OID_A, Some("run-old")),
    );
    let (_, new_seq) = append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        prepared_payload("snap-x", OID_A, Some("run-new")),
    );
    assert!(new_seq > old_seq);
    let prepared = checkpoint::prepared_for_snapshot(&store, &thread, "snap-x")
        .expect("lookup")
        .expect("prepared");
    assert_eq!(prepared.run_id.as_deref(), Some("run-new"));
}

#[test]
fn malformed_selected_payload_fails_explicitly() {
    let mut store = Store::open_in_memory().expect("store");
    let thread = new_thread(&mut store, "malformado");
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        prepared_payload("snap-x", OID_A, Some("run-old")),
    );
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        json!({"snapshot_id": "snap-x"}),
    );
    assert!(
        checkpoint::prepared_for_snapshot(&store, &thread, "snap-x").is_err(),
        "el payload seleccionado debe propagar el error de deserialización"
    );
}

#[test]
fn prepared_lookup_is_isolated_by_thread_and_snapshot() {
    let mut store = Store::open_in_memory().expect("store");
    let t1 = new_thread(&mut store, "uno");
    let t2 = new_thread(&mut store, "dos");
    append(
        &mut store,
        &t1,
        events::K_CHECKPOINT_PREPARED,
        prepared_payload("snap-1", OID_A, Some("run-1")),
    );
    assert!(
        checkpoint::prepared_for_snapshot(&store, &t2, "snap-1")
            .expect("lookup")
            .is_none()
    );
    assert!(
        checkpoint::prepared_for_snapshot(&store, &t1, "snap-2")
            .expect("lookup")
            .is_none()
    );
}

#[test]
fn legacy_prepared_without_run_id_remains_readable() {
    let mut store = Store::open_in_memory().expect("store");
    let thread = new_thread(&mut store, "legacy");
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        prepared_payload("snap-legacy", OID_A, None),
    );
    let prepared = checkpoint::prepared_for_snapshot(&store, &thread, "snap-legacy")
        .expect("lookup")
        .expect("prepared");
    assert_eq!(prepared.snapshot_id, "snap-legacy");
    assert!(prepared.run_id.is_none());
}

#[test]
fn latest_ready_preserves_logical_checkpoint_order() {
    let mut store = Store::open_in_memory().expect("store");
    let thread = new_thread(&mut store, "orden");
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        prepared_payload("snap-a", OID_A, None),
    );
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        prepared_payload("snap-b", OID_B, None),
    );
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_READY,
        ready_payload("snap-b", OID_B),
    );
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_READY,
        ready_payload("snap-a", OID_A),
    );
    let (_, latest) = checkpoint::latest_ready(&store, &thread)
        .expect("latest")
        .expect("ready");
    assert_eq!(latest.snapshot_id, "snap-b");
}

#[test]
fn attention_classifications_and_actions_remain_unchanged() {
    let mut store = Store::open_in_memory().expect("store");
    let thread = new_thread(&mut store, "attention");
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        prepared_payload("snap-a", OID_A, None),
    );
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_READY,
        ready_payload("snap-a", OID_A),
    );
    let a = attention::project_thread(&store, &thread).expect("project");
    assert_eq!(a.status, AttentionStatus::ReviewRequired);
    assert_eq!(a.next_action, "marcar revisado");

    append(
        &mut store,
        &thread,
        events::K_REVIEW_ACKNOWLEDGED,
        json!({"snapshot_id": "snap-a", "oid": OID_A}),
    );
    let a = attention::project_thread(&store, &thread).expect("project");
    assert_eq!(a.status, AttentionStatus::ValidationRequired);
    assert_eq!(a.next_action, "ejecutar validación");

    append(
        &mut store,
        &thread,
        events::K_VALIDATION_RECORDED,
        json!({
            "snapshot_id": "snap-a",
            "oid": OID_A,
            "command": "true",
            "exit_code": 0,
            "duration_ms": 1,
            "output_hash": "hash",
            "output_tail": "",
            "tree_before_ok": true,
            "tree_after_unchanged": true
        }),
    );
    let a = attention::project_thread(&store, &thread).expect("project");
    assert_eq!(a.status, AttentionStatus::ReadyToIntegrate);
    assert_eq!(a.next_action, "abrir preview de integración");
}

#[test]
fn prepared_identity_is_not_altered_by_the_hint() {
    let mut store = Store::open_in_memory().expect("store");
    let thread = new_thread(&mut store, "identidad");
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        prepared_payload("snap-a", OID_A, Some("run-a")),
    );
    let (event_run, prepared) =
        checkpoint::prepared_for_snapshot_with_run(&store, &thread, "snap-a")
            .expect("lookup")
            .expect("prepared");
    assert_eq!(event_run, None);
    assert_eq!(prepared.snapshot_id, "snap-a");
    assert_eq!(prepared.expected_oid, OID_A);
    assert_eq!(prepared.run_id.as_deref(), Some("run-a"));
    let via_payload = checkpoint::prepared_for_snapshot(&store, &thread, "snap-a")
        .expect("lookup")
        .expect("prepared");
    assert_eq!(via_payload.git_ref, prepared.git_ref);
    assert_eq!(via_payload.base_oid, prepared.base_oid);
    assert_eq!(via_payload.files.len(), prepared.files.len());
    let _: payload::CheckpointPrepared = prepared;
}
