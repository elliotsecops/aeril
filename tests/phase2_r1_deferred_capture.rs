//! P2-R1-C: a completed agent turn with a recoverable prepared checkpoint must
//! not be reported as a failed turn. Terminal capture failures without durable
//! recovery evidence remain failed.

mod common;

use common::*;
use serde_json::Value;

fn events_of(env: &TestEnv, thread: &str, kind: &str) -> Vec<Value> {
    let out = env.aeril(&["export", "--thread", thread, "--format", "jsonl"]);
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            v.get("type").and_then(Value::as_str) == Some("event")
                && v.get("kind").and_then(Value::as_str) == Some(kind)
        })
        .collect()
}

#[test]
fn r1_c_deferred_capture_preserves_completed_turn_and_recovers_ready() {
    let env = TestEnv::new("p2r1-c-deferred");
    let thread = env.thread_new("deferred capture");
    let script = env.script(
        "s",
        mock_edit("NOTES.md", "# v2\n\ncaptura diferida\n", "x"),
    );

    let out = env.aeril_env(
        &[
            "run",
            "start",
            "--thread",
            &thread,
            "--prompt",
            "x",
            "--backend",
            &format!("mock:{}", script.display()),
        ],
        &[("AERIL_FAULT_APPEND", "checkpoint.ready")],
    );
    assert!(
        out.status.success(),
        "el turno completado no debe abortar por un checkpoint diferido: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run: Value = parse_json(&out.stdout);
    assert_eq!(run["state"], "completed", "run: {run}");
    assert!(run["snapshot"].is_null(), "no se fabrica snapshot: {run}");
    assert!(
        run["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("checkpoint pendiente de recuperación"),
        "el detalle debe declarar el checkpoint pendiente: {run}"
    );
    assert!(
        events_of(&env, &thread, "checkpoint.failed").is_empty(),
        "un pending recuperable no es un failed terminal"
    );
    let prepared = env
        .checkpoint_list(&thread)
        .into_iter()
        .find(|c| c["state"] == "prepared")
        .expect("prepared durable");
    let sid = prepared["snapshot_id"].as_str().unwrap().to_string();
    assert!(
        env.ready_snapshots(&thread).is_empty(),
        "un ready no puede existir antes de recuperar"
    );

    let rec = env.aeril_ok(&["recover"]);
    assert!(
        rec.as_array()
            .unwrap()
            .iter()
            .any(|e| e["action"] == "completed_ready" && e["snapshot_id"] == sid.as_str()),
        "recover debe completar el pending: {rec}"
    );
    let ready = env.ready_snapshots(&thread);
    assert_eq!(ready.len(), 1, "exactamente un ready: {ready:?}");
    assert_eq!(ready[0].0, sid);
    assert_eq!(events_of(&env, &thread, "checkpoint.ready").len(), 1);
    assert_eq!(events_of(&env, &thread, "checkpoint.failed").len(), 0);

    let again = env.aeril_ok(&["recover"]);
    assert!(
        !again
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["action"] == "completed_ready" || e["action"] == "deferred"),
        "recovery idempotente: {again}"
    );
    assert_eq!(events_of(&env, &thread, "checkpoint.ready").len(), 1);
}

#[test]
fn r1_c_failure_before_prepared_remains_failed() {
    let env = TestEnv::new("p2r1-c-before");
    let thread = env.thread_new("before prepared");
    let script = env.script("s", mock_edit("NOTES.md", "# v2\n\nsin prepared\n", "x"));
    let out = env.aeril_env(
        &[
            "run",
            "start",
            "--thread",
            &thread,
            "--prompt",
            "x",
            "--backend",
            &format!("mock:{}", script.display()),
        ],
        &[("AERIL_FAULT_APPEND", "checkpoint.prepared")],
    );
    assert!(out.status.success(), "el CLI reporta el estado, no aborta");
    let run: Value = parse_json(&out.stdout);
    assert_eq!(run["state"], "failed", "run: {run}");
    assert!(run["snapshot"].is_null());
    assert!(
        env.checkpoint_list(&thread).is_empty(),
        "sin prepared no hay checkpoint recuperable"
    );
    assert!(events_of(&env, &thread, "checkpoint.failed").is_empty());
}
