//! P2-R1-B: logical checkpoint ordering.
//!
//! `latest_ready` must follow the order in which checkpoints were captured
//! (`checkpoint.prepared` seq), not the order in which recovery happened to
//! write `checkpoint.ready`.

mod common;

use aeril::checkpoint;
use aeril::store::{Store, events, threads};
use serde_json::{Value, json};
use std::time::Instant;

const OID: &str = "0000000000000000000000000000000000000000";

fn store_thread() -> (Store, String) {
    let mut store = Store::open_in_memory().unwrap();
    let thread = threads::create(&mut store.conn, "checkpoint order", "/tmp/repo", "r1").unwrap();
    (store, thread.id)
}

fn prepared(snapshot_id: &str, run_id: Option<&str>) -> Value {
    json!({
        "snapshot_id": snapshot_id,
        "git_ref": format!("refs/aeril/checkpoints/{snapshot_id}"),
        "expected_oid": OID,
        "run_id": run_id,
        "base_oid": OID,
        "parent_oid": OID,
        "files": [],
    })
}

fn ready(snapshot_id: &str, run_id: Option<&str>) -> Value {
    json!({
        "snapshot_id": snapshot_id,
        "git_ref": format!("refs/aeril/checkpoints/{snapshot_id}"),
        "oid": OID,
        "base_oid": OID,
        "patch": "",
        "files": [],
        "recovered": false,
        "run_id": run_id,
    })
}

fn add_run(store: &Store, thread_id: &str, run_id: &str) {
    store
        .conn
        .execute(
            "INSERT INTO runs (id, thread_id, backend, state, started_ms)
             VALUES (?1, ?2, 'mock', 'completed', 1)",
            rusqlite::params![run_id, thread_id],
        )
        .unwrap();
}

fn append(
    store: &mut Store,
    thread_id: &str,
    kind: &str,
    run_id: Option<&str>,
    payload: &Value,
) -> i64 {
    let tx = store
        .conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let (_, seq) = events::append_tx(&tx, thread_id, run_id, kind, "system", payload, 0).unwrap();
    tx.commit().unwrap();
    seq
}

fn latest_id(store: &Store, thread_id: &str) -> String {
    checkpoint::latest_ready(store, thread_id)
        .unwrap()
        .expect("ready")
        .1
        .snapshot_id
}

#[test]
fn r1_b1_normal_order_selects_newest_capture() {
    let (mut store, thread) = store_thread();
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        None,
        &prepared("a", None),
    );
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_READY,
        None,
        &ready("a", None),
    );
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        None,
        &prepared("b", None),
    );
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_READY,
        None,
        &ready("b", None),
    );
    assert_eq!(latest_id(&store, &thread), "b");
}

#[test]
fn r1_b2_late_recovery_does_not_replace_newer_capture() {
    let (mut store, thread) = store_thread();
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        None,
        &prepared("a", None),
    );
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        None,
        &prepared("b", None),
    );
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_READY,
        None,
        &ready("b", None),
    );
    // A's recovery resolves last, but B was captured later.
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_READY,
        None,
        &ready("a", None),
    );
    assert_eq!(latest_id(&store, &thread), "b");
}

#[test]
fn r1_b3_pending_newer_snapshot_is_not_selected() {
    let (mut store, thread) = store_thread();
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        None,
        &prepared("a", None),
    );
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_READY,
        None,
        &ready("a", None),
    );
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        None,
        &prepared("b", None),
    );
    assert_eq!(latest_id(&store, &thread), "a");
}

#[test]
fn r1_b4_failed_newer_snapshot_is_not_selected() {
    let (mut store, thread) = store_thread();
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        None,
        &prepared("a", None),
    );
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_READY,
        None,
        &ready("a", None),
    );
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        None,
        &prepared("b", None),
    );
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_FAILED,
        None,
        &json!({"snapshot_id":"b","reason":"test","terminal":true}),
    );
    assert_eq!(latest_id(&store, &thread), "a");
}

#[test]
fn r1_b5a_invalid_selected_ready_propagates() {
    let (mut store, thread) = store_thread();
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        None,
        &prepared("ok", None),
    );
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_READY,
        None,
        &ready("ok", None),
    );
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        None,
        &prepared("bad", None),
    );
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_READY,
        None,
        &json!({"git_ref":"refs/aeril/checkpoints/bad"}),
    );
    assert!(checkpoint::latest_ready(&store, &thread).is_err());
}

#[test]
fn r1_b5b_contradictory_run_identity_is_rejected() {
    let (mut store, thread) = store_thread();
    add_run(&store, &thread, "run-a");
    add_run(&store, &thread, "run-b");
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        Some("run-a"),
        &prepared("a", Some("run-a")),
    );
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_READY,
        Some("run-b"),
        &ready("a", Some("run-b")),
    );
    assert!(checkpoint::latest_ready(&store, &thread).is_err());
}

#[test]
fn r1_b5c_ready_before_prepared_is_inconsistent() {
    let (mut store, thread) = store_thread();
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_READY,
        None,
        &ready("a", None),
    );
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        None,
        &prepared("a", None),
    );
    assert!(checkpoint::latest_ready(&store, &thread).is_err());
}

#[test]
fn r1_b7_large_history_keeps_logical_order() {
    const PAIRS: usize = 25_000;
    let (mut store, thread) = store_thread();
    let tx = store
        .conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let mut prepared_seqs = Vec::with_capacity(PAIRS);
    for i in 0..PAIRS {
        let sid = format!("snap-{i:05}");
        let payload = prepared(&sid, None);
        let (_, seq) = events::append_tx(
            &tx,
            &thread,
            None,
            events::K_CHECKPOINT_PREPARED,
            "system",
            &payload,
            0,
        )
        .unwrap();
        prepared_seqs.push(seq);
    }
    // Resolve every snapshot except the second-to-last one, then resolve the
    // newest, and finally resolve the second-to-last late.
    for i in (0..PAIRS).filter(|i| *i != PAIRS - 2) {
        let sid = format!("snap-{i:05}");
        let payload = ready(&sid, None);
        events::append_tx(
            &tx,
            &thread,
            None,
            events::K_CHECKPOINT_READY,
            "system",
            &payload,
            0,
        )
        .unwrap();
    }
    let late_sid = format!("snap-{:05}", PAIRS - 2);
    events::append_tx(
        &tx,
        &thread,
        None,
        events::K_CHECKPOINT_READY,
        "system",
        &ready(&late_sid, None),
        0,
    )
    .unwrap();
    tx.commit().unwrap();

    let old_start = Instant::now();
    let old = events::fetch_latest_by_kind(&store.conn, &thread, events::K_CHECKPOINT_READY)
        .unwrap()
        .expect("old ready");
    let old_elapsed = old_start.elapsed();
    let new_start = Instant::now();
    let latest = checkpoint::latest_ready(&store, &thread)
        .unwrap()
        .unwrap()
        .1;
    let new_elapsed = new_start.elapsed();
    assert_eq!(latest.snapshot_id, format!("snap-{:05}", PAIRS - 1));
    assert_eq!(old.payload["snapshot_id"], late_sid);
    println!(
        "LATENCIA latest_ready old_ready_seq={} old_ms={:.3} logical_ms={:.3} prepared_events={}",
        old.seq,
        old_elapsed.as_secs_f64() * 1000.0,
        new_elapsed.as_secs_f64() * 1000.0,
        prepared_seqs.len()
    );
    let plan = store.conn.prepare(
        "EXPLAIN QUERY PLAN
         SELECT r.id FROM events r
         LEFT JOIN events p ON p.thread_id = r.thread_id
           AND p.kind = 'checkpoint.prepared'
           AND json_extract(p.payload, '$.snapshot_id') =
               json_extract(r.payload, '$.snapshot_id')
         WHERE r.thread_id = ?1 AND r.kind = 'checkpoint.ready'
         ORDER BY COALESCE(p.seq, r.seq) DESC, r.seq DESC LIMIT 1",
    );
    if let Ok(mut stmt) = plan
        && let Ok(rows) = stmt.query_map(rusqlite::params![thread], |row| row.get::<_, String>(3))
    {
        let lines: Vec<String> = rows.filter_map(Result::ok).collect();
        println!("QUERY_PLAN {}", lines.join(" | "));
        assert!(
            lines
                .iter()
                .any(|l| l.contains("idx_events_checkpoint_snapshot")),
            "el plan debe usar el índice v5: {lines:?}"
        );
    }
}

fn run_state(
    store: &Store,
    thread_id: &str,
    run_id: &str,
    snapshot_id: &str,
) -> checkpoint::RunCheckpointState {
    checkpoint::run_checkpoint_state(store, thread_id, run_id, snapshot_id).unwrap()
}

#[test]
fn r1_c_state_ready_is_visible_and_wins_over_unrelated_pending() {
    let (mut store, thread) = store_thread();
    add_run(&store, &thread, "run-1");
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        Some("run-1"),
        &prepared("a", Some("run-1")),
    );
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_READY,
        Some("run-1"),
        &ready("a", Some("run-1")),
    );
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        Some("run-1"),
        &prepared("b", Some("run-1")),
    );
    match run_state(&store, &thread, "run-1", "a") {
        checkpoint::RunCheckpointState::Ready(ready) => {
            let (ev, ready) = *ready;
            assert_eq!(ready.snapshot_id, "a");
            assert_eq!(ev.run_id.as_deref(), Some("run-1"));
        }
        other => panic!("se esperaba Ready(a), se obtuvo {other:?}"),
    }
    // El pending de OTRO snapshot no contamina la clasificación de `a`.
    assert!(matches!(
        run_state(&store, &thread, "run-1", "b"),
        checkpoint::RunCheckpointState::Pending
    ));
}

#[test]
fn r1_c_state_pending_requires_durable_prepared() {
    let (mut store, thread) = store_thread();
    add_run(&store, &thread, "run-1");
    assert!(matches!(
        run_state(&store, &thread, "run-1", "a"),
        checkpoint::RunCheckpointState::None
    ));
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        Some("run-1"),
        &prepared("a", Some("run-1")),
    );
    assert!(matches!(
        run_state(&store, &thread, "run-1", "a"),
        checkpoint::RunCheckpointState::Pending
    ));
}

#[test]
fn r1_c_state_terminal_failure_is_none() {
    let (mut store, thread) = store_thread();
    add_run(&store, &thread, "run-1");
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        Some("run-1"),
        &prepared("a", Some("run-1")),
    );
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_FAILED,
        Some("run-1"),
        &json!({"snapshot_id":"a","reason":"test","terminal":true}),
    );
    assert!(matches!(
        run_state(&store, &thread, "run-1", "a"),
        checkpoint::RunCheckpointState::None
    ));
}

#[test]
fn r1_b_legacy_ready_without_prepared_keeps_ready_seq_fallback() {
    let (mut store, thread) = store_thread();
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_READY,
        None,
        &ready("legacy-only", None),
    );
    let (ev, ready) = checkpoint::latest_ready(&store, &thread)
        .unwrap()
        .expect("ready legacy");
    assert_eq!(ready.snapshot_id, "legacy-only");
    assert!(ev.seq >= 0, "ready legacy conserva su seq real");
}

#[test]
fn r1_c_state_handles_legacy_prepared_without_run_id() {
    let (mut store, thread) = store_thread();
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        None,
        &prepared("legacy", None),
    );
    assert!(matches!(
        run_state(&store, &thread, "run-1", "legacy"),
        checkpoint::RunCheckpointState::Pending
    ));
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_READY,
        None,
        &ready("legacy", None),
    );
    assert!(matches!(
        run_state(&store, &thread, "run-1", "legacy"),
        checkpoint::RunCheckpointState::Ready(_)
    ));
}

#[test]
fn r1_c_state_rejects_ready_from_another_run() {
    let (mut store, thread) = store_thread();
    add_run(&store, &thread, "run-1");
    add_run(&store, &thread, "run-2");
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_PREPARED,
        Some("run-1"),
        &prepared("a", Some("run-1")),
    );
    append(
        &mut store,
        &thread,
        events::K_CHECKPOINT_READY,
        Some("run-2"),
        &ready("a", Some("run-2")),
    );
    assert!(
        checkpoint::run_checkpoint_state(&store, &thread, "run-1", "a").is_err(),
        "un ready de otro run no puede resolver la captura"
    );
}

#[test]
fn r1_c_state_rejects_ready_with_divergent_oid_or_ref() {
    for (snapshot, oid, git_ref) in [
        (
            "bad-oid",
            "f".repeat(40),
            "refs/aeril/checkpoints/bad-oid".to_string(),
        ),
        (
            "bad-ref",
            OID.to_string(),
            "refs/aeril/checkpoints/other".into(),
        ),
    ] {
        let (mut store, thread) = store_thread();
        add_run(&store, &thread, "run-1");
        append(
            &mut store,
            &thread,
            events::K_CHECKPOINT_PREPARED,
            Some("run-1"),
            &prepared(snapshot, Some("run-1")),
        );
        append(
            &mut store,
            &thread,
            events::K_CHECKPOINT_READY,
            Some("run-1"),
            &json!({
                "snapshot_id": snapshot,
                "git_ref": git_ref,
                "oid": oid,
                "base_oid": OID,
                "patch": "",
                "files": [],
                "recovered": true,
            }),
        );
        assert!(
            checkpoint::run_checkpoint_state(&store, &thread, "run-1", snapshot).is_err(),
            "ready con OID/ref divergente debe fallar cerrado ({snapshot})"
        );
    }
}
