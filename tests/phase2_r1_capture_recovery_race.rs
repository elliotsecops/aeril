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
            // The outcome is truthful; stop after reproducing one.
            return;
        }
    }
    panic!("could not hit capture/recover window in 20 attempts");
}
