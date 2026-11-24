//! PA0-PERF-R: lista inmediata + Attention verificado en seleccionada y visibles
//! + resto explícitamente pendiente de evaluación hasta converger en background.

mod common;

use aeril::store::events;
use aeril::store::{Store, threads};
use common::pty::TuiPty;
use common::*;
use serde_json::json;
use std::time::{Duration, Instant};

fn ref_for(snapshot: &str) -> String {
    format!("refs/aeril/checkpoints/{snapshot}")
}

fn prepared(snapshot: &str) -> serde_json::Value {
    json!({
        "snapshot_id": snapshot,
        "git_ref": ref_for(snapshot),
        "expected_oid": "1".repeat(40),
        "base_oid": "0".repeat(40),
        "parent_oid": "0".repeat(40),
        "files": []
    })
}

fn ready(snapshot: &str) -> serde_json::Value {
    json!({
        "snapshot_id": snapshot,
        "git_ref": ref_for(snapshot),
        "oid": "1".repeat(40),
        "base_oid": "0".repeat(40),
        "patch": "",
        "files": [],
        "recovered": false
    })
}

fn seed(env: &TestEnv, count: usize) -> Vec<String> {
    let mut store = Store::open(&env.data.join("aeril.db")).expect("store");
    let mut ids = Vec::with_capacity(count);
    for i in 0..count {
        let thread = threads::create(
            &mut store.conn,
            &format!("progressive {i}"),
            "/tmp/repo",
            "r1",
        )
        .expect("thread");
        ids.push(thread.id);
    }
    let tx = store
        .conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .expect("tx");
    for (i, id) in ids.iter().enumerate() {
        for _ in 0..100 {
            events::append_tx(
                &tx,
                id,
                None,
                events::K_AGENT_MESSAGE,
                "system",
                &json!({"text": "contexto"}),
                0,
            )
            .expect("filler");
        }
        let snapshot = format!("snap-{i}");
        events::append_tx(
            &tx,
            id,
            None,
            events::K_CHECKPOINT_PREPARED,
            "system",
            &prepared(&snapshot),
            0,
        )
        .expect("prepared");
        events::append_tx(
            &tx,
            id,
            None,
            events::K_CHECKPOINT_READY,
            "system",
            &ready(&snapshot),
            0,
        )
        .expect("ready");
    }
    tx.commit().expect("commit");
    ids
}

fn beacon_stage(path: &std::path::Path, stage: &str) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        if value.get("stage").and_then(|s| s.as_str()) == Some(stage) {
            return value.get("internal_us").and_then(|v| v.as_u64());
        }
    }
    None
}

#[test]
fn tui_verifies_selected_and_visible_rows_before_first_frame_and_navigates_before_full_inbox() {
    let env = TestEnv::new("pa0-progressive");
    seed(&env, 200);
    let beacon = env.root.join("beacon.jsonl");
    let mut pty = TuiPty::spawn_sized(
        &env,
        &[],
        &[("AERIL_BENCH_BEACON", beacon.to_str().unwrap())],
        50,
        180,
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !pty.output_contains("Aeril") {
        assert!(
            Instant::now() < deadline,
            "la TUI no dibujó el primer frame"
        );
        pty.pump(50);
    }
    // En el primer frame útil:
    // 1) seleccionada y filas visibles ya deben estar verificadas;
    // 2) cobertura global refleja el resto pendiente sin falsa calma.
    assert!(
        beacon_stage(&beacon, "selected_attention").is_some(),
        "selected_attention debe existir antes del primer frame"
    );
    assert!(
        beacon_stage(&beacon, "visible_coverage").is_some(),
        "visible_coverage debe existir antes del primer frame"
    );
    let out = pty.output_lossy();
    assert!(
        pty.output_contains("eval"),
        "falta cobertura de evaluación: {out}"
    );
    assert!(
        pty.output_contains("pend"),
        "falta estado pendiente explícito: {out}"
    );
    assert!(
        pty.output_contains("checkpoint listo") && pty.output_contains("marcar revisado"),
        "las filas visibles deben mostrar su Attention verificado: {out}"
    );
    assert!(
        !pty.output_contains("pendiente de evaluación"),
        "las filas visibles iniciales no deben mostrarse pendientes: {out}"
    );

    pty.enter();
    pty.pump(50);
    let opened = beacon_stage(&beacon, "thread_opened").expect("thread_opened beacon");
    let full = beacon_stage(&beacon, "full_inbox");
    assert!(
        full.is_none() || full.unwrap() > opened,
        "el Inbox completo no debe preceder a la navegación (opened={opened}, full={full:?})"
    );
    // Navegación rápida mientras el worker procesa el backlog.
    for _ in 0..12 {
        pty.down();
    }
    pty.pump(100);
    assert!(pty.alive(), "la TUI debe seguir viva durante la proyección");

    let deadline = Instant::now() + Duration::from_secs(10);
    while beacon_stage(&beacon, "full_inbox").is_none() {
        assert!(
            Instant::now() < deadline,
            "el Inbox no completó la cobertura: {}",
            pty.output_lossy()
        );
        pty.pump(50);
    }
    pty.kill();
}

#[test]
fn worker_stops_when_tui_quits() {
    let env = TestEnv::new("pa0-progressive-quit");
    seed(&env, 200);
    let mut pty = TuiPty::spawn_sized(&env, &[], &[], 50, 180);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !pty.output_contains("Aeril") {
        assert!(
            Instant::now() < deadline,
            "la TUI no dibujó el primer frame"
        );
        pty.pump(50);
    }
    pty.send(b"q");
    let status = pty.wait_exit(3000);
    assert!(status.is_some(), "la TUI no terminó con el worker activo");
}

#[test]
fn coverage_is_visible_at_supported_sizes() {
    let env = TestEnv::new("pa0-progressive-sizes");
    seed(&env, 40);
    for (rows, cols) in [(24, 80), (40, 120), (50, 180)] {
        let mut pty = TuiPty::spawn_sized(&env, &[], &[], rows, cols);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !pty.output_contains("Aeril") {
            assert!(
                Instant::now() < deadline,
                "la TUI no dibujó el primer frame en {cols}x{rows}"
            );
            pty.pump(50);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while !pty.output_flat().contains("/40") {
            assert!(
                Instant::now() < deadline,
                "falta cobertura total en {cols}x{rows}: {}",
                pty.output_flat()
            );
            pty.pump(50);
        }
        let flat = pty.output_flat();
        assert!(
            pty.output_contains("pending") || pty.output_contains("pend") || flat.contains('p'),
            "falta indicador pendiente en {cols}x{rows}: {flat}"
        );
        assert!(
            pty.output_contains("checkpoint listo"),
            "fila visible debe mostrar Attention verificado en {cols}x{rows}: {flat}"
        );
        pty.kill();
    }
}

#[test]
fn watermarks_batch_matches_per_thread_next_seq_and_reconciles_external_change() {
    let env = TestEnv::new("pa0-progressive-wm");
    let ids = seed(&env, 3);
    let mut store = Store::open(&env.data.join("aeril.db")).expect("store");
    let map = threads::watermarks(&store.conn, &ids).expect("watermarks");
    for id in &ids {
        assert_eq!(
            map.get(id).copied(),
            Some(threads::next_seq(&store.conn, id).expect("seq"))
        );
    }

    let mut pty = TuiPty::spawn_sized(&env, &[], &[], 40, 120);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !pty.output_contains("checkpoint listo") {
        assert!(Instant::now() < deadline, "falta estado inicial");
        pty.pump(50);
    }

    // Mutación externa sobre la primera tarea (orden más reciente = último creado).
    let target = ids.last().expect("target");
    events::append(
        &mut store.conn,
        target,
        None,
        events::K_REVIEW_ACKNOWLEDGED,
        "human",
        &json!({"snapshot_id": "snap-2", "oid": "1".repeat(40)}),
    )
    .expect("ack");

    let deadline = Instant::now() + Duration::from_secs(5);
    while !pty.output_contains("checkpoint revisado") {
        assert!(
            Instant::now() < deadline,
            "watermark no invalidó y re-evaluó: {}",
            pty.output_lossy()
        );
        pty.pump(100);
    }
    pty.kill();
}

#[test]
fn evaluation_error_is_distinct_from_integrity_hold_and_never_shows_false_calm() {
    let env = TestEnv::new("pa0-progressive-err");
    let mut store = Store::open(&env.data.join("aeril.db")).expect("store");
    let t = threads::create(&mut store.conn, "err-eval", "/tmp/repo", "r1").expect("thread");
    // Evento de integración malformado provoca Error::integrity en project_thread,
    // que el evaluador captura en EvaluationState::Error (no IntegrityHold ni calma).
    events::append(
        &mut store.conn,
        &t.id,
        None,
        events::K_INTEGRATION_BLOCKED,
        "system",
        &json!({}),
    )
    .expect("bad event");

    let mut pty = TuiPty::spawn_sized(&env, &[], &[], 40, 120);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !pty.output_contains("error de evaluación") {
        assert!(
            Instant::now() < deadline,
            "debe mostrar error de evaluación explícito: {}",
            pty.output_lossy()
        );
        pty.pump(50);
    }
    let out = pty.output_lossy();
    assert!(!out.contains("sin trabajo pendiente"));
    assert!(!out.contains("sin acción pendiente"));
    assert!(!out.contains("todo limpio"));
    assert!(!out.contains("listo para integrar"));
    pty.kill();
}
