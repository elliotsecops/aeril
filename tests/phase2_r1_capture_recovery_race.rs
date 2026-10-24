//! Adversarial review only: reproduce capture-vs-recover race.
mod common;

use common::*;
use serde_json::{Value, json};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn big_script(n: usize) -> Value {
    let mut edits = Vec::new();
    for i in 0..n {
        edits.push(json!({
            "op": "write",
            "path": format!("bulk/f{i:04}.txt"),
            "content": format!("file {i}\n{}\n", "x".repeat(8192)),
        }));
    }
    json!({"message": "bulk", "final_message": "listo", "edits": edits})
}

#[test]
fn capture_vs_recover_race_is_not_false_failed() {
    for attempt in 0..20 {
        let env = TestEnv::new(&format!("r1-race-{attempt}"));
        let thread = env.thread_new("race");
        let script = env.script("bulk", big_script(300));

        let mut child = Command::new(BIN)
            .args([
                "run",
                "start",
                "--thread",
                &thread,
                "--prompt",
                "x",
                "--backend",
                &format!("mock:{}", script.display()),
            ])
            .env("AERIL_DATA_DIR", &env.data)
            .env("AERIL_FAULT_APPEND", "checkpoint.ready")
            .current_dir(&env.root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn run");

        // Wait until the durable prepared exists, then freeze the supervisor
        // before it can append its own (fault-injected) ready.
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut prepared = false;
        while Instant::now() < deadline {
            let list = env.checkpoint_list(&thread);
            if list.iter().any(|c| c["state"] == "prepared") {
                prepared = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        if !prepared {
            let _ = child.kill();
            let _ = child.wait();
            continue;
        }
        unsafe {
            libc::kill(child.id() as i32, libc::SIGSTOP);
        }

        // A concurrent recovery sees the prepared and resolves it.
        let rec = env.aeril(&["recover"]);
        let rec_json: Value = serde_json::from_slice(&rec.stdout).unwrap_or(Value::Null);
        let completed = rec_json
            .as_array()
            .map(|a| a.iter().any(|e| e["action"] == "completed_ready"))
            .unwrap_or(false);
        let ready_before_continue = env.ready_snapshots(&thread);
        unsafe {
            libc::kill(child.id() as i32, libc::SIGCONT);
        }
        let out = child.wait_with_output().unwrap();
        if !completed || !out.status.success() {
            // Retry when we missed the capture window or the run itself died.
            eprintln!(
                "attempt {attempt}: completed={completed} status={:?} stderr={}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            continue;
        }
        let run: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
        eprintln!("attempt {attempt}: run={run} ready_before={ready_before_continue:?}");
        if !ready_before_continue.is_empty() && run["state"] == "failed" {
            panic!("FALSE FAILURE: run failed although recovery made the checkpoint ready: {run}");
        }
        if !ready_before_continue.is_empty() && run["state"] == "completed" {
            // The completed outcome must report the exact verified snapshot,
            // not merely some other ready of the same run.
            assert_eq!(
                run["snapshot"]["snapshot_id"],
                ready_before_continue[0].0.as_str()
            );
            assert_eq!(run["snapshot"]["oid"], ready_before_continue[0].1.as_str());
            return;
        }
    }
    panic!("could not hit capture/recover window in 20 attempts");
}

fn has_event(env: &TestEnv, thread: &str, kind: &str) -> bool {
    let Ok(conn) = rusqlite::Connection::open(env.data.join("aeril.db")) else {
        return false;
    };
    let _ = conn.busy_timeout(Duration::from_millis(50));
    conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM events WHERE thread_id = ?1 AND kind = ?2)",
        rusqlite::params![thread, kind],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
        == 1
}

fn inject_divergent_ready(env: &TestEnv, thread: &str) -> rusqlite::Result<Option<String>> {
    use rusqlite::OptionalExtension;
    let mut conn = rusqlite::Connection::open(env.data.join("aeril.db"))?;
    conn.busy_timeout(Duration::from_millis(50))?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let prepared: Option<(Option<String>, String)> = tx
        .query_row(
            "SELECT run_id, payload FROM events
              WHERE thread_id = ?1 AND kind = 'checkpoint.prepared'
              ORDER BY seq DESC LIMIT 1",
            rusqlite::params![thread],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((run_id, payload)) = prepared else {
        tx.rollback()?;
        return Ok(None);
    };
    let prepared: Value = serde_json::from_str(&payload).unwrap();
    let snapshot = prepared["snapshot_id"].as_str().unwrap().to_string();
    let already: i64 = tx.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM events
             WHERE thread_id = ?1 AND kind = 'checkpoint.ready'
               AND json_extract(payload, '$.snapshot_id') = ?2)",
        rusqlite::params![thread, snapshot],
        |r| r.get(0),
    )?;
    if already == 1 {
        tx.rollback()?;
        return Ok(None);
    }
    let base = prepared["base_oid"].as_str().unwrap().to_string();
    let seq: i64 = tx.query_row(
        "UPDATE threads SET next_seq = next_seq + 1 WHERE id = ?1
         RETURNING next_seq - 1",
        rusqlite::params![thread],
        |r| r.get(0),
    )?;
    let payload = json!({
        "snapshot_id": snapshot,
        "git_ref": format!("refs/aeril/checkpoints/{snapshot}"),
        "oid": "f".repeat(40),
        "base_oid": base,
        "patch": "",
        "files": [],
        "recovered": true,
    });
    tx.execute(
        "INSERT INTO events
           (id, thread_id, seq, run_id, kind, actor, payload_version, payload, created_ms)
         VALUES (?1, ?2, ?3, ?4, 'checkpoint.ready', 'system', 1, ?5, 1)",
        rusqlite::params![
            aeril::ids::new_id(),
            thread,
            seq,
            run_id,
            payload.to_string()
        ],
    )?;
    tx.commit()?;
    Ok(Some(snapshot))
}

#[test]
fn divergent_ready_during_capture_fails_closed() {
    for attempt in 0..10 {
        let env = TestEnv::new(&format!("r1-race-divergent-{attempt}"));
        let thread = env.thread_new("divergent");
        let script = env.script("bulk", big_script(100));

        let mut child = Command::new(BIN)
            .args([
                "run",
                "start",
                "--thread",
                &thread,
                "--prompt",
                "x",
                "--backend",
                &format!("mock:{}", script.display()),
            ])
            .env("AERIL_DATA_DIR", &env.data)
            .env("AERIL_FAULT_APPEND", "checkpoint.ready")
            .current_dir(&env.root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn run");

        let deadline = Instant::now() + Duration::from_secs(20);
        let mut prepared = false;
        while Instant::now() < deadline {
            if has_event(&env, &thread, "checkpoint.prepared") {
                prepared = true;
                break;
            }
            std::thread::sleep(Duration::from_micros(500));
        }
        if !prepared || has_event(&env, &thread, "checkpoint.ready") {
            let _ = child.kill();
            let _ = child.wait();
            continue;
        }
        // Race the injection between prepared and the capture's own ready.
        // A short IMMEDIATE transaction either wins or observes the ready and
        // returns None; a busy lock is retried.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut injected = None;
        let mut last_err = None;
        while Instant::now() < deadline {
            match inject_divergent_ready(&env, &thread) {
                Ok(Some(snapshot)) => {
                    injected = Some(snapshot);
                    break;
                }
                Ok(None) => break,
                Err(e) => {
                    last_err = Some(e);
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        let Some(snapshot) = injected else {
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("attempt {attempt}: could not inject (last_err={last_err:?})");
            continue;
        };
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "el CLI reporta estado, no aborta");
        let run: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
        if run["state"] == "completed" {
            // If the injected ready became visible only after the session
            // classified the capture, the truthful outcome is deferred with a
            // null snapshot. A completed run WITH a snapshot would mean the
            // divergent ready was accepted: that is the fail-open we forbid.
            assert!(
                run["snapshot"].is_null(),
                "un ready divergente inyectado no puede reportarse como snapshot: {run}"
            );
            continue;
        }
        assert_eq!(run["state"], "failed", "run: {run}");
        assert!(
            run["snapshot"].is_null(),
            "no se debe fabricar un snapshot desde un ready divergente: {run}"
        );
        let ready = env.ready_snapshots(&thread);
        assert_eq!(ready.len(), 1, "solo el ready inyectado: {ready:?}");
        assert_eq!(ready[0].0, snapshot);
        return;
    }
    panic!("could not inject divergent ready during capture window");
}
