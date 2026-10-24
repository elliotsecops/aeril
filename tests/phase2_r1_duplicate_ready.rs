//! Capture-vs-recover duplicate-ready race.
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

fn ready_snapshots(env: &TestEnv, thread: &str) -> Vec<(String, String)> {
    env.ready_snapshots(thread)
}

#[test]
fn capture_vs_recover_does_not_duplicate_ready() {
    for attempt in 0..20 {
        let env = TestEnv::new(&format!("r1-race-dup-{attempt}"));
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
            .current_dir(&env.root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn run");

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
        let rec = env.aeril(&["recover"]);
        let rec_json: Value = serde_json::from_slice(&rec.stdout).unwrap_or(Value::Null);
        let completed = rec_json
            .as_array()
            .map(|a| a.iter().any(|e| e["action"] == "completed_ready"))
            .unwrap_or(false);
        let ready_before = ready_snapshots(&env, &thread);
        unsafe {
            libc::kill(child.id() as i32, libc::SIGCONT);
        }
        let out = child.wait_with_output().unwrap();
        if !completed || !out.status.success() {
            eprintln!(
                "attempt {attempt}: completed={completed} status={:?}",
                out.status
            );
            continue;
        }
        let run: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
        let after = ready_snapshots(&env, &thread);
        eprintln!(
            "attempt {attempt}: run state={} ready_before={} after={}",
            run["state"],
            ready_before.len(),
            after.len()
        );
        if !ready_before.is_empty() && after.len() > 1 {
            panic!(
                "DUPLICATE READY: {} ready events for one capture (run {run})",
                after.len()
            );
        }
        if !ready_before.is_empty() && after.len() == 1 && run["state"] == "completed" {
            assert_eq!(
                run["snapshot"]["snapshot_id"],
                ready_before[0].0.as_str(),
                "el run debe reportar el snapshot verificado"
            );
            assert_eq!(run["snapshot"]["oid"], ready_before[0].1.as_str());
            return;
        }
    }
    panic!("could not hit capture/recover window in 20 attempts");
}
