//! P2-S1 R2 — cross-component adversarial regression suite.
//!
//! All fixtures are disposable and use the fake Hermes binary. No real
//! credentials or P2-S0 profiles are touched.

mod common;

use common::pty::TuiPty;
use common::*;
use serde_json::Value;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

const FAKE_HERMES: &str = include_str!("common/fake_hermes.py");

struct R2 {
    env: TestEnv,
    bin: PathBuf,
    path_orig: String,
}

impl R2 {
    fn new(name: &str) -> R2 {
        let env = TestEnv::new(name);
        let bin = env.root.join("fakebin");
        std::fs::create_dir_all(&bin).unwrap();
        let hermes = bin.join("hermes");
        std::fs::write(&hermes, FAKE_HERMES).unwrap();
        std::fs::set_permissions(&hermes, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path_orig = std::env::var("PATH").unwrap_or_default();
        R2 {
            env,
            bin,
            path_orig,
        }
    }

    fn path_env(&self) -> String {
        format!("{}:{}", self.bin.display(), self.path_orig)
    }

    fn env_pairs(&self, on: bool) -> Vec<(String, String)> {
        let mut out = vec![
            ("PATH".into(), self.path_env()),
            (
                "AERIL_HERMES_BIN".into(),
                self.bin.join("hermes").to_string_lossy().into_owned(),
            ),
        ];
        if on {
            out.push(("AERIL_HERMES_PROFILES".into(), "1".into()));
        }
        out
    }

    fn cmd(&self, on: bool, args: &[&str], extra: &[(&str, String)]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .env("AERIL_DATA_DIR", &self.env.data)
            .current_dir(&self.env.root);
        for (k, v) in self.env_pairs(on) {
            cmd.env(k, v);
        }
        for (k, v) in extra {
            cmd.env(k, v);
        }
        cmd
    }

    fn out(&self, on: bool, args: &[&str]) -> Output {
        self.cmd(on, args, &[]).output().expect("aeril")
    }

    fn ok(&self, on: bool, args: &[&str]) -> Value {
        let out = self.out(on, args);
        assert!(
            out.status.success(),
            "aeril {args:?} falló:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        parse_json(&out.stdout)
    }

    fn err(&self, on: bool, args: &[&str]) -> String {
        let out = self.out(on, args);
        assert!(
            !out.status.success(),
            "aeril {args:?} debía fallar y salió 0:\nstdout: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    }

    fn create(&self, agent: &str) {
        self.ok(true, &["profile", "create", "--agent", agent]);
    }

    fn assign(&self, agent: &str, thread: &str) {
        self.ok(
            true,
            &["profile", "assign", "--agent", agent, "--thread", thread],
        );
    }

    fn auth_mode(&self, agent: &str, mode: &str) {
        std::fs::write(self.home(agent).join("fake-auth-mode"), mode).unwrap();
    }

    fn init_mode(&self, agent: &str, mode: &str) {
        std::fs::write(self.home(agent).join("fake-init-mode"), mode).unwrap();
    }

    fn agent_dir(&self, agent: &str) -> PathBuf {
        self.env.data.join("hermes").join(agent)
    }

    fn home(&self, agent: &str) -> PathBuf {
        self.agent_dir(agent).join("home")
    }

    fn san_home(&self, agent: &str) -> PathBuf {
        self.agent_dir(agent).join("san-home")
    }

    fn thread_new(&self) -> String {
        let v = self.ok(
            false,
            &[
                "thread",
                "new",
                "--repo",
                self.env.repo.to_str().unwrap(),
                "--title",
                "r2",
            ],
        );
        v["thread_id"].as_str().unwrap().to_string()
    }

    fn spawn_run(&self, thread: &str, extra: &[(&str, String)]) -> Child {
        self.cmd(
            true,
            &[
                "run",
                "start",
                "--thread",
                thread,
                "--prompt",
                "x",
                "--backend",
                "hermes",
            ],
            extra,
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
    }

    fn wait_for(&self, path: &Path, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if path.exists() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        path.exists()
    }

    fn fake_identity(&self, agent: &str) -> (i32, u64) {
        let pid: i32 = std::fs::read_to_string(self.home(agent).join("fake-pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let start = std::fs::read_to_string(self.home(agent).join("fake-pid-start"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or_else(|| proc_start(pid).unwrap_or(0));
        (pid, start)
    }

    fn latest_run(&self, thread: &str) -> Option<aeril::store::runs::RunRow> {
        let store = aeril::store::Store::open(&self.env.data.join("aeril.db")).ok()?;
        aeril::store::runs::latest(&store.conn, thread)
            .ok()
            .flatten()
    }

    fn registry_agent_for(&self, thread: &str) -> Option<String> {
        let text = std::fs::read_to_string(self.env.data.join("hermes/agents.json")).ok()?;
        let v: Value = serde_json::from_str(&text).ok()?;
        v["agents"].as_object()?.iter().find_map(|(agent, e)| {
            e["thread_ids"]
                .as_array()
                .map(|ids| ids.iter().any(|t| t.as_str() == Some(thread)))
                .unwrap_or(false)
                .then(|| agent.clone())
        })
    }

    fn kill_verified(&self, pid: i32, start: u64) {
        if pid <= 0 {
            return;
        }
        let actual = proc_start(pid);
        if actual != Some(start) {
            return;
        }
        let Ok(cmdline) = std::fs::read_to_string(format!("/proc/{pid}/cmdline")) else {
            return;
        };
        if !cmdline.contains("hermes") {
            return;
        }
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

impl Drop for R2 {
    fn drop(&mut self) {
        if std::env::var("AERIL_KEEP_TESTS").is_ok() {
            return;
        }
        for name in ["fake-pid", "fake-escaped-pid", "fake-auth-descendant-pid"] {
            let path = self.env.data.join("hermes");
            if !path.exists() {
                continue;
            }
            for entry in walk_files(&path) {
                if entry.file_name().and_then(|s| s.to_str()) != Some(name) {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&entry) else {
                    continue;
                };
                let mut parts = text.split_whitespace();
                let Some(pid) = parts.next().and_then(|s| s.parse::<i32>().ok()) else {
                    continue;
                };
                let start = parts
                    .next()
                    .and_then(|s| s.parse::<u64>().ok())
                    .or_else(|| proc_start(pid))
                    .unwrap_or(0);
                self.kill_verified(pid, start);
            }
        }
        // Descendant files may contain multiple "pid start" pairs.
        let root = self.env.data.join("hermes");
        for entry in walk_files(&root) {
            if entry.file_name().and_then(|s| s.to_str()) != Some("fake-descendant-pid") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&entry) else {
                continue;
            };
            let nums: Vec<u64> = text
                .split_whitespace()
                .filter_map(|s| s.parse::<u64>().ok())
                .collect();
            for pair in nums.chunks(2) {
                if pair.len() == 2 {
                    self.kill_verified(pair[0] as i32, pair[1]);
                }
            }
        }
    }
}

fn proc_start(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = stat.rsplit_once(')')?.1;
    tail.split_whitespace().nth(19)?.parse().ok()
}

fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            out.extend(walk_files(&path));
        } else {
            out.push(path);
        }
    }
    out
}

fn leak_process(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

// ── P1-A: admission linearization ───────────────────────────────────────────

#[test]
fn r2_p1a_reassignment_during_final_mapping_check_is_consistent() {
    let r2 = R2::new("r2-p1a");
    r2.create("a");
    r2.create("b");
    r2.auth_mode("a", "positive");
    r2.auth_mode("b", "positive");
    let t = r2.thread_new();
    r2.assign("a", &t);
    let barrier = r2.env.root.join("barrier-reassign");
    let run = r2.spawn_run(
        &t,
        &[(
            "AERIL_TEST_PROFILE_ADMISSION_BARRIER",
            barrier.to_string_lossy().into_owned(),
        )],
    );
    assert!(
        r2.wait_for(&barrier.join("ready"), 15),
        "admission no llegó al barrier"
    );
    let reassign = r2
        .cmd(
            true,
            &["profile", "assign", "--agent", "b", "--thread", &t],
            &[],
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    std::fs::write(barrier.join("go"), b"1").unwrap();
    let run_out = run.wait_with_output().unwrap();
    let reassign_out = reassign.wait_with_output().unwrap();
    let run_profile = r2.latest_run(&t).and_then(|r| r.profile);
    let registry = r2.registry_agent_for(&t);
    assert_eq!(
        run_profile,
        registry,
        "run profile and registry diverged; run status={} stderr={} reassign_rc={}",
        run_out.status,
        String::from_utf8_lossy(&run_out.stderr),
        reassign_out.status
    );
    assert!(
        run_profile.is_some(),
        "the admitted run must carry durable profile ownership"
    );
}

#[test]
fn r2_p1a_profile_deletion_during_admission_is_blocked() {
    let r2 = R2::new("r2-p1a-del");
    r2.create("a");
    r2.auth_mode("a", "positive");
    let t = r2.thread_new();
    r2.assign("a", &t);
    let barrier = r2.env.root.join("barrier-delete");
    let run = r2.spawn_run(
        &t,
        &[(
            "AERIL_TEST_PROFILE_ADMISSION_BARRIER",
            barrier.to_string_lossy().into_owned(),
        )],
    );
    assert!(r2.wait_for(&barrier.join("ready"), 15));
    let remove = r2
        .cmd(
            true,
            &[
                "profile",
                "remove",
                "--agent",
                "a",
                "--confirm",
                "--clear-threads",
            ],
            &[],
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    std::fs::write(barrier.join("go"), b"1").unwrap();
    let _ = run.wait_with_output().unwrap();
    let remove_out = remove.wait_with_output().unwrap();
    assert!(
        !remove_out.status.success(),
        "profile removal succeeded during admission: {}",
        String::from_utf8_lossy(&remove_out.stderr)
    );
    assert!(r2.agent_dir("a").exists(), "profile dir was deleted");
}

// ── P1-B: missing registry and legacy ownership ─────────────────────────────

#[test]
fn r2_p1b_missing_registry_blocks_destructive_removal() {
    let r2 = R2::new("r2-p1b-missing");
    r2.create("a");
    let _ = r2.thread_new();
    std::fs::remove_file(r2.env.data.join("hermes/agents.json")).unwrap();
    let err = r2.err(
        true,
        &[
            "profile",
            "remove",
            "--agent",
            "a",
            "--confirm",
            "--clear-threads",
        ],
    );
    assert!(
        err.contains("registro") || err.contains("ownership") || err.contains("fail-closed"),
        "mensaje inesperado: {err}"
    );
    assert!(r2.agent_dir("a").exists());
}

#[test]
fn r2_p1b_legacy_null_run_blocks_profile_deletion() {
    let r2 = R2::new("r2-p1b-legacy");
    r2.create("a");
    let t = r2.thread_new();
    let legacy_home = r2.env.root.join("legacy-home");
    std::fs::create_dir_all(&legacy_home).unwrap();
    std::fs::write(legacy_home.join("fake-init-mode"), "linger").unwrap();
    let mut run = r2
        .cmd(
            false,
            &[
                "run",
                "start",
                "--thread",
                &t,
                "--prompt",
                "legacy",
                "--backend",
                "hermes",
            ],
            &[
                ("HERMES_HOME", legacy_home.to_string_lossy().into_owned()),
                (
                    "AERIL_HERMES_BIN",
                    r2.bin.join("hermes").to_string_lossy().into_owned(),
                ),
                ("AERIL_HERMES_PROFILES", "0".into()),
            ],
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    assert!(
        r2.wait_for(&legacy_home.join("fake-lingering"), 15),
        "legacy fake backend did not reach the turn"
    );
    let fake_pid: i32 = std::fs::read_to_string(legacy_home.join("fake-pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let fake_start = std::fs::read_to_string(legacy_home.join("fake-pid-start"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .or_else(|| proc_start(fake_pid))
        .unwrap_or(0);
    unsafe {
        libc::kill(run.id() as i32, libc::SIGKILL);
    }
    let _ = run.wait();
    r2.kill_verified(fake_pid, fake_start);
    std::thread::sleep(Duration::from_millis(300));
    let latest = r2.latest_run(&t).expect("legacy run row");
    assert_eq!(latest.profile, None, "legacy run must be NULL-profile");
    assert!(
        matches!(latest.state.as_str(), "starting" | "running"),
        "legacy run must remain unresolved for this test: {}",
        latest.state
    );
    let err = r2.err(
        true,
        &[
            "profile",
            "remove",
            "--agent",
            "a",
            "--confirm",
            "--clear-threads",
        ],
    );
    assert!(
        err.contains("legacy") || err.contains("no-terminal") || err.contains("ownership"),
        "mensaje inesperado: {err}"
    );
    assert!(r2.agent_dir("a").exists());
}

#[test]
fn r2_p1b_corrupt_registry_is_fail_closed() {
    let r2 = R2::new("r2-p1b-corrupt");
    r2.create("a");
    let _ = r2.thread_new();
    std::fs::write(r2.env.data.join("hermes/agents.json"), b"{not json").unwrap();
    let err = r2.err(true, &["profile", "remove", "--agent", "a", "--confirm"]);
    assert!(
        err.contains("agents.json") || err.contains("ilegible"),
        "{err}"
    );
    assert!(r2.agent_dir("a").exists());
}

// ── P1-C: recovery follows durable ownership ────────────────────────────────

#[test]
fn r2_p1c_flag_off_recovery_does_not_kill_managed_survivor() {
    let r2 = R2::new("r2-p1c");
    r2.create("a");
    r2.auth_mode("a", "positive");
    r2.init_mode("a", "linger");
    let t = r2.thread_new();
    r2.assign("a", &t);
    let mut run = r2.spawn_run(&t, &[]);
    assert!(r2.wait_for(&r2.home("a").join("fake-pid"), 15));
    assert!(r2.wait_for(&r2.home("a").join("fake-lingering"), 15));
    let (fake, start) = r2.fake_identity("a");
    unsafe {
        libc::kill(run.id() as i32, libc::SIGKILL);
    }
    let _ = run.wait();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        proc_start(fake),
        Some(start),
        "fake survivor identity changed"
    );
    let out = r2.out(false, &["recover"]);
    assert!(
        out.status.success(),
        "recover failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        proc_start(fake).is_some(),
        "flag-OFF recovery killed a managed survivor"
    );
    r2.kill_verified(fake, start);
}

// ── P1-D: filesystem containment ────────────────────────────────────────────

#[test]
fn r2_p1d_symlinked_root_is_rejected() {
    let r2 = R2::new("r2-p1d-root");
    let external = r2.env.root.join("external-root");
    std::fs::create_dir_all(&external).unwrap();
    std::fs::write(external.join("sentinel"), b"unchanged").unwrap();
    std::os::unix::fs::symlink(&external, r2.env.data.join("hermes")).unwrap();
    let out = r2.out(true, &["profile", "create", "--agent", "a"]);
    assert!(!out.status.success(), "symlinked root was accepted");
    assert_eq!(
        std::fs::read(external.join("sentinel")).unwrap(),
        b"unchanged"
    );
    assert!(
        !external.join("a").exists(),
        "profile was created outside root"
    );
}

#[test]
fn r2_p1d_symlinked_home_and_san_home_are_rejected() {
    let r2 = R2::new("r2-p1d-home");
    r2.create("a");
    let external_home = r2.env.root.join("external-home");
    std::fs::create_dir_all(&external_home).unwrap();
    std::fs::write(external_home.join("sentinel"), b"unchanged").unwrap();
    std::fs::remove_dir_all(r2.home("a")).unwrap();
    std::os::unix::fs::symlink(&external_home, r2.home("a")).unwrap();
    let out = r2.out(true, &["profile", "warm", "--agent", "a"]);
    assert!(!out.status.success(), "symlinked home was accepted");
    assert_eq!(
        std::fs::read(external_home.join("sentinel")).unwrap(),
        b"unchanged"
    );
    assert!(!external_home.join("fake-pid").exists());

    let r2b = R2::new("r2-p1d-san");
    r2b.create("a");
    let external_san = r2b.env.root.join("external-san");
    std::fs::create_dir_all(&external_san).unwrap();
    std::fs::set_permissions(&external_san, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::remove_dir_all(r2b.san_home("a")).unwrap();
    std::os::unix::fs::symlink(&external_san, r2b.san_home("a")).unwrap();
    let out = r2b.out(true, &["profile", "create", "--agent", "a"]);
    assert!(!out.status.success(), "symlinked san-home was accepted");
    let mode = std::fs::symlink_metadata(&external_san)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o755, "external san-home permissions were changed");
}

#[test]
fn r2_p1d_symlinked_lock_files_are_rejected() {
    let r2 = R2::new("r2-p1d-lock");
    r2.create("a");
    let _ = r2.thread_new();
    let external = r2.env.root.join("external-lock");
    std::fs::write(&external, b"unchanged").unwrap();
    std::fs::write(r2.agent_dir("a").join("aeril.lock"), b"").unwrap();
    std::fs::remove_file(r2.agent_dir("a").join("aeril.lock")).unwrap();
    std::os::unix::fs::symlink(&external, r2.agent_dir("a").join("aeril.lock")).unwrap();
    let out = r2.out(true, &["profile", "warm", "--agent", "a"]);
    assert!(!out.status.success(), "symlinked aeril.lock was accepted");
    assert_eq!(std::fs::read(&external).unwrap(), b"unchanged");
}

#[test]
fn r2_p1d_symlinked_metadata_is_rejected() {
    let r2 = R2::new("r2-p1d-meta");
    r2.create("a");
    let external = r2.env.root.join("external-meta");
    std::fs::write(
        &external,
        br#"{"agent_id":"a","created_ms":1,"hermes_version":"","provider":"nous","model":null,"last_auth_check_ms":1,"auth_status":"logged_in","warmed_ms":null}"#,
    )
    .unwrap();
    std::fs::remove_file(r2.agent_dir("a").join("profile.json")).unwrap();
    std::os::unix::fs::symlink(&external, r2.agent_dir("a").join("profile.json")).unwrap();
    let out = r2.out(true, &["profile", "status", "--agent", "a"]);
    assert!(!out.status.success(), "symlinked profile.json was accepted");
    assert_eq!(
        std::fs::read(&external).unwrap(),
        br#"{"agent_id":"a","created_ms":1,"hermes_version":"","provider":"nous","model":null,"last_auth_check_ms":1,"auth_status":"logged_in","warmed_ms":null}"#
    );
}

// ── P1-E: auth proof and bounded pipes ──────────────────────────────────────

#[test]
fn r2_p1e_truncated_or_contradictory_auth_is_unknown() {
    let r2 = R2::new("r2-p1e-auth");
    r2.create("a");
    for mode in [
        "truncated-positive",
        "positive-flood",
        "stderr-contradictory",
        "stderr-flood",
    ] {
        r2.auth_mode("a", mode);
        let out = r2
            .cmd(
                true,
                &["profile", "status", "--agent", "a"],
                &[("AERIL_PROFILE_AUTH_TIMEOUT_SECS", "1".into())],
            )
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "status failed for {mode}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: Value = parse_json(&out.stdout);
        assert_ne!(
            v["auth_status"], "logged_in",
            "mode {mode} produced a false positive"
        );
    }
}

#[test]
fn r2_p1e_descendant_holding_stdout_cannot_extend_auth_timeout() {
    let r2 = R2::new("r2-p1e-desc");
    r2.create("a");
    r2.auth_mode("a", "descendant-stdout");
    let mut status = r2
        .cmd(
            true,
            &["profile", "status", "--agent", "a"],
            &[("AERIL_PROFILE_AUTH_TIMEOUT_SECS", "1".into())],
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut finished = None;
    while Instant::now() < deadline {
        if let Some(st) = status.try_wait().unwrap() {
            finished = Some(st);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if finished.is_none() {
        leak_process(&mut status);
        panic!("auth probe hung on a descendant-held stdout");
    }
    let out = status.wait_with_output().unwrap();
    assert!(
        finished.unwrap().success(),
        "status failed: {:?}",
        out.stderr
    );
    let v: Value = parse_json(&out.stdout);
    assert_ne!(v["auth_status"], "logged_in");
    if let Ok(text) = std::fs::read_to_string(r2.home("a").join("fake-descendant-pid")) {
        let nums: Vec<u64> = text
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        for pair in nums.chunks(2) {
            if pair.len() == 2 {
                r2.kill_verified(pair[0] as i32, pair[1]);
            }
        }
    }
}

#[test]
fn r2_p1a_reassignment_during_slow_auth_fails_closed() {
    let r2 = R2::new("r2-p1a-slow");
    r2.create("a");
    r2.create("b");
    r2.auth_mode("a", "slow:2");
    r2.auth_mode("b", "positive");
    let t = r2.thread_new();
    r2.assign("a", &t);
    let run = r2.spawn_run(&t, &[]);
    assert!(
        r2.wait_for(&r2.home("a").join("fake-auth-started"), 10),
        "slow auth probe did not start"
    );
    let assign = r2.out(true, &["profile", "assign", "--agent", "b", "--thread", &t]);
    assert!(
        assign.status.success(),
        "reassignment itself should be allowed while auth runs"
    );
    let out = run.wait_with_output().unwrap();
    assert!(
        !out.status.success(),
        "admission must fail closed after a concurrent reassignment"
    );
    assert!(
        r2.latest_run(&t).is_none(),
        "no run may be created with the old profile after reassignment"
    );
    assert_eq!(r2.registry_agent_for(&t).as_deref(), Some("b"));
}

#[test]
fn r2_p1d_registry_lock_symlink_is_rejected() {
    let r2 = R2::new("r2-p1d-reglock");
    r2.create("a");
    let t = r2.thread_new();
    let external = r2.env.root.join("external-reglock");
    std::fs::write(&external, b"unchanged").unwrap();
    let _ = std::fs::remove_file(r2.env.data.join("hermes/.registry.lock"));
    std::os::unix::fs::symlink(&external, r2.env.data.join("hermes/.registry.lock")).unwrap();
    let out = r2.out(true, &["profile", "assign", "--agent", "a", "--thread", &t]);
    assert!(
        !out.status.success(),
        "symlinked registry lock was accepted"
    );
    assert_eq!(std::fs::read(&external).unwrap(), b"unchanged");
}

#[test]
fn r2_p1d_lock_inode_replacement_is_detected() {
    let r2 = R2::new("r2-p1d-lockinode");
    r2.create("a");
    r2.auth_mode("a", "positive");
    let t = r2.thread_new();
    r2.assign("a", &t);
    let barrier = r2.env.root.join("barrier-lock-inode");
    let run = r2.spawn_run(
        &t,
        &[(
            "AERIL_TEST_PROFILE_ADMISSION_BARRIER",
            barrier.to_string_lossy().into_owned(),
        )],
    );
    assert!(r2.wait_for(&barrier.join("ready"), 15));
    // Replace the lock path while the admission still holds the old inode.
    std::fs::remove_file(r2.agent_dir("a").join("aeril.lock")).unwrap();
    std::fs::write(r2.agent_dir("a").join("aeril.lock"), b"").unwrap();
    // A second writer must not admit through the fresh inode.
    let warm = r2.out(true, &["profile", "warm", "--agent", "a"]);
    assert!(
        !warm.status.success(),
        "second writer admitted through a replaced lock inode"
    );
    std::fs::write(barrier.join("go"), b"1").unwrap();
    let out = run.wait_with_output().unwrap();
    assert!(
        !out.status.success(),
        "admission did not detect its lock inode being replaced"
    );
    assert!(r2.latest_run(&t).is_none());
}

#[test]
fn r2_p1d_meta_lock_symlink_is_rejected() {
    let r2 = R2::new("r2-p1d-metalock");
    r2.create("a");
    let external = r2.env.root.join("external-metalock");
    std::fs::write(&external, b"unchanged").unwrap();
    let _ = std::fs::remove_file(r2.agent_dir("a").join(".meta.lock"));
    std::os::unix::fs::symlink(&external, r2.agent_dir("a").join(".meta.lock")).unwrap();
    let out = r2.out(true, &["profile", "status", "--agent", "a"]);
    assert!(!out.status.success(), "symlinked meta lock was accepted");
    assert_eq!(std::fs::read(&external).unwrap(), b"unchanged");
}

#[test]
fn r2_p1d_owner_symlink_is_rejected() {
    let r2 = R2::new("r2-p1d-ownerlink");
    r2.create("a");
    let _ = r2.thread_new();
    let external = r2.env.root.join("external-owner");
    std::fs::write(
        &external,
        br#"{"supervisor_pid":1,"supervisor_start":1,"backend_pid":1,"backend_start":1,"pgid":1,"started_ms":1}"#,
    )
    .unwrap();
    std::os::unix::fs::symlink(&external, r2.agent_dir("a").join("owner.json")).unwrap();
    let out = r2.out(
        true,
        &[
            "profile",
            "remove",
            "--agent",
            "a",
            "--confirm",
            "--clear-threads",
        ],
    );
    assert!(!out.status.success(), "symlinked owner.json was accepted");
    assert!(r2.agent_dir("a").exists());
    assert_eq!(
        std::fs::read(&external).unwrap(),
        br#"{"supervisor_pid":1,"supervisor_start":1,"backend_pid":1,"backend_start":1,"pgid":1,"started_ms":1}"#
    );
}

#[test]
fn r2_p1d_agents_json_symlink_is_rejected() {
    let r2 = R2::new("r2-p1d-agentslink");
    r2.create("a");
    let external = r2.env.root.join("external-agents");
    std::fs::write(
        &external,
        br#"{"version":1,"agents":{"a":{"thread_ids":[],"created_ms":1}},"updated_ms":1}"#,
    )
    .unwrap();
    std::fs::remove_file(r2.env.data.join("hermes/agents.json")).unwrap();
    std::os::unix::fs::symlink(&external, r2.env.data.join("hermes/agents.json")).unwrap();
    let out = r2.out(true, &["profile", "list"]);
    assert!(!out.status.success(), "symlinked agents.json was accepted");
    assert_eq!(
        std::fs::read(&external).unwrap(),
        br#"{"version":1,"agents":{"a":{"thread_ids":[],"created_ms":1}},"updated_ms":1}"#
    );
}

// ── P2-H1/H7: shared ACP initialize contract on real runs ───────────────────

#[test]
fn r2_h1_h7_managed_runs_require_strict_initialize() {
    let r2 = R2::new("r2-h1");
    r2.create("a");
    r2.auth_mode("a", "positive");
    for mode in [
        "null-result",
        "string-result",
        "missing-capabilities",
        "missing-protocol",
    ] {
        r2.init_mode("a", mode);
        let t = r2.thread_new();
        r2.assign("a", &t);
        let v = r2.ok(
            true,
            &[
                "run",
                "start",
                "--thread",
                &t,
                "--prompt",
                "x",
                "--backend",
                "hermes",
            ],
        );
        assert_eq!(
            v["state"], "failed",
            "managed run accepted invalid initialize mode {mode}: {v}"
        );
        assert!(v["snapshot"].is_null(), "failed init must not checkpoint");
    }
    // Legitimate fixture remains accepted.
    r2.init_mode("a", "valid");
    let t = r2.thread_new();
    r2.assign("a", &t);
    let v = r2.ok(
        true,
        &[
            "run",
            "start",
            "--thread",
            &t,
            "--prompt",
            "x",
            "--backend",
            "hermes",
        ],
    );
    assert_eq!(v["state"], "completed");
}

#[test]
fn r2_h7_warm_requires_agent_capabilities() {
    let r2 = R2::new("r2-h7");
    r2.create("a");
    r2.init_mode("a", "missing-capabilities");
    let out = r2.out(true, &["profile", "warm", "--agent", "a"]);
    assert!(
        !out.status.success(),
        "warm accepted initialize without agentCapabilities"
    );
}

// ── P2-H2: escaped descendants and owner-write failure ──────────────────────

#[test]
fn r2_h2_escaped_grandchild_is_not_reported_clean() {
    let r2 = R2::new("r2-h2");
    r2.create("a");
    r2.init_mode("a", "escape-grandchild");
    let out = r2.out(true, &["profile", "warm", "--agent", "a"]);
    assert!(
        !out.status.success(),
        "warm reported success with an escaped descendant"
    );
    assert!(
        r2.agent_dir("a").join("owner.json").exists(),
        "owner evidence was cleared despite an escaped survivor"
    );
    let list = r2.ok(true, &["profile", "list"]);
    let a = list
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["agent_id"] == "a")
        .unwrap();
    assert_eq!(a["lock_state"], "held", "escaped child lost inherited lock");
}

#[test]
fn r2_h2_owner_write_failure_aborts_spawn() {
    let r2 = R2::new("r2-h2-ownerfail");
    r2.create("a");
    r2.init_mode("a", "valid");
    let out = r2
        .cmd(
            true,
            &["profile", "warm", "--agent", "a"],
            &[("AERIL_TEST_FAIL_OWNER_WRITE", "1".into())],
        )
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "warm succeeded when owner evidence could not be persisted"
    );
    let pidfile = r2.home("a").join("fake-pid");
    if let Ok(text) = std::fs::read_to_string(&pidfile)
        && let Ok(pid) = text.trim().parse::<i32>()
    {
        let start = proc_start(pid).unwrap_or(0);
        r2.kill_verified(pid, start);
    }
}

// ── P2-H3: removal honors owner.json ────────────────────────────────────────

#[test]
fn r2_h3_remove_honors_live_and_corrupt_owner_records() {
    let r2 = R2::new("r2-h3");
    r2.create("a");
    let _ = r2.thread_new();
    let mut sleeper = Command::new("sleep").arg("30").spawn().unwrap();
    let pid = sleeper.id() as i32;
    let start = proc_start(pid).unwrap();
    std::fs::write(
        r2.agent_dir("a").join("owner.json"),
        serde_json::json!({
            "supervisor_pid": pid,
            "supervisor_start": start,
            "backend_pid": pid,
            "backend_start": start,
            "pgid": pid,
            "started_ms": 1,
        })
        .to_string(),
    )
    .unwrap();
    let out = r2.out(
        true,
        &[
            "profile",
            "remove",
            "--agent",
            "a",
            "--confirm",
            "--clear-threads",
        ],
    );
    assert!(!out.status.success(), "remove ignored a live owner record");
    assert!(r2.agent_dir("a").exists());
    let _ = sleeper.kill();
    let _ = sleeper.wait();

    std::fs::write(r2.agent_dir("a").join("owner.json"), b"{not json").unwrap();
    let out = r2.out(
        true,
        &[
            "profile",
            "remove",
            "--agent",
            "a",
            "--confirm",
            "--clear-threads",
        ],
    );
    assert!(
        !out.status.success(),
        "remove ignored a corrupt owner record"
    );
    assert!(r2.agent_dir("a").exists());
}

// ── P2-H4: concurrent and interrupted schema migration ──────────────────────

fn legacy_db(path: &Path, version: i64) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(aeril::store::schema::SCHEMA_V1).unwrap();
    if version >= 2 {
        conn.execute_batch(aeril::store::schema::SCHEMA_V2).unwrap();
    }
    if version >= 3 {
        conn.execute_batch(aeril::store::schema::SCHEMA_V3).unwrap();
    }
    conn.pragma_update(None, "user_version", version).unwrap();
    conn.execute(
        "INSERT INTO threads (id, title, repo_id, repo_path, state, next_seq, created_ms, updated_ms)
         VALUES ('t-old', 'old', 'r-old', '/tmp', 'open', 0, 1, 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO runs (id, thread_id, backend, state, started_ms)
         VALUES ('run-old', 't-old', 'mock', 'completed', 1)",
        [],
    )
    .unwrap();
}

#[test]
fn r2_h4_concurrent_migration_is_serialized() {
    let dir = std::env::temp_dir().join(format!("aeril-r2-mig-{}", aeril::ids::new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("aeril.db");
    legacy_db(&path, 3);
    let barrier = Arc::new(Barrier::new(24));
    std::thread::scope(|scope| {
        for _ in 0..24 {
            let path = path.clone();
            let barrier = barrier.clone();
            scope.spawn(move || {
                barrier.wait();
                let store = aeril::store::Store::open(&path).expect("concurrent open");
                let v: i64 = store
                    .conn
                    .query_row("PRAGMA user_version", [], |r| r.get(0))
                    .unwrap();
                assert_eq!(v, aeril::store::schema::SCHEMA_VERSION);
                let profile: Option<String> = store
                    .conn
                    .query_row("SELECT profile FROM runs WHERE id='run-old'", [], |r| {
                        r.get(0)
                    })
                    .unwrap();
                assert_eq!(profile, None);
            });
        }
    });
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn r2_h4_interrupted_v4_ddl_is_idempotent() {
    for version in [1, 2, 3] {
        let dir =
            std::env::temp_dir().join(format!("aeril-r2-mig{version}-{}", aeril::ids::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("aeril.db");
        legacy_db(&path, version);
        let store = aeril::store::Store::open(&path).expect("upgrade");
        let v: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, aeril::store::schema::SCHEMA_VERSION);
    }

    let dir = std::env::temp_dir().join(format!("aeril-r2-migpart-{}", aeril::ids::new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("aeril.db");
    legacy_db(&path, 3);
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("ALTER TABLE runs ADD COLUMN profile TEXT", [])
            .unwrap();
        conn.pragma_update(None, "user_version", 3).unwrap();
    }
    let store = aeril::store::Store::open(&path).expect("partial v4 ddl");
    let v: i64 = store
        .conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, aeril::store::schema::SCHEMA_VERSION);
}

// ── P2-H5: partial removal recovery ─────────────────────────────────────────

fn mark_removing(r2: &R2, agent: &str) {
    let path = r2.env.data.join("hermes/agents.json");
    let mut v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    v["agents"][agent]["removing"] = Value::Bool(true);
    std::fs::write(&path, serde_json::to_vec_pretty(&v).unwrap()).unwrap();
}

#[test]
fn r2_h5_partial_removal_is_idempotent_and_resumable() {
    let r2 = R2::new("r2-h5");
    r2.create("a");
    let _ = r2.thread_new();
    mark_removing(&r2, "a");
    std::fs::remove_file(r2.agent_dir("a").join("profile.json")).unwrap();
    let out = r2.out(
        true,
        &[
            "profile",
            "remove",
            "--agent",
            "a",
            "--confirm",
            "--clear-threads",
        ],
    );
    assert!(
        out.status.success(),
        "partial removal was not resumable: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!r2.agent_dir("a").exists());
    let reg: Value =
        serde_json::from_slice(&std::fs::read(r2.env.data.join("hermes/agents.json")).unwrap())
            .unwrap();
    assert!(reg["agents"].get("a").is_none());
}

#[test]
fn r2_h5_removing_profile_cannot_be_recreated_or_reassigned() {
    let r2 = R2::new("r2-h5-create");
    r2.create("a");
    let t = r2.thread_new();
    mark_removing(&r2, "a");
    let out = r2.out(true, &["profile", "create", "--agent", "a"]);
    assert!(!out.status.success(), "removing profile was recreated");
    let out = r2.out(true, &["profile", "assign", "--agent", "a", "--thread", &t]);
    assert!(
        !out.status.success(),
        "removing profile accepted assignment"
    );
}

#[test]
fn r2_h5_partial_removal_resumes_after_owner_recovery() {
    let r2 = R2::new("r2-h5-owner");
    r2.create("a");
    let _ = r2.thread_new();
    mark_removing(&r2, "a");
    std::fs::remove_file(r2.agent_dir("a").join("profile.json")).unwrap();
    let mut sleeper = Command::new("sleep").arg("30").spawn().unwrap();
    let pid = sleeper.id() as i32;
    let start = proc_start(pid).unwrap();
    std::fs::write(
        r2.agent_dir("a").join("owner.json"),
        serde_json::json!({
            "supervisor_pid": pid,
            "supervisor_start": start,
            "backend_pid": pid,
            "backend_start": start,
            "pgid": pid,
            "started_ms": 1,
        })
        .to_string(),
    )
    .unwrap();
    let out = r2.out(
        true,
        &[
            "profile",
            "remove",
            "--agent",
            "a",
            "--confirm",
            "--clear-threads",
        ],
    );
    assert!(!out.status.success(), "partial removal ignored live owner");
    let _ = sleeper.kill();
    let _ = sleeper.wait();
    let out = r2.out(
        true,
        &[
            "profile",
            "remove",
            "--agent",
            "a",
            "--confirm",
            "--clear-threads",
        ],
    );
    assert!(
        out.status.success(),
        "partial removal did not resume after owner recovery: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!r2.agent_dir("a").exists());
}

// ── P2-H6: durable ownership in list/export surfaces ────────────────────────

#[test]
fn r2_h6_run_list_and_exports_include_profile() {
    let r2 = R2::new("r2-h6");
    r2.create("a");
    r2.auth_mode("a", "positive");
    r2.init_mode("a", "valid");
    let t = r2.thread_new();
    r2.assign("a", &t);
    let v = r2.ok(
        true,
        &[
            "run",
            "start",
            "--thread",
            &t,
            "--prompt",
            "x",
            "--backend",
            "hermes",
        ],
    );
    assert_eq!(v["state"], "completed");
    assert_eq!(v["profile"], "a");
    let runs = r2.ok(true, &["run", "list", "--limit", "10"]);
    let run = runs
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["thread_id"] == t.as_str())
        .expect("run listed");
    assert_eq!(run["profile"], "a");
    let jsonl = r2.out(true, &["export", "--thread", &t, "--format", "jsonl"]);
    assert!(jsonl.status.success());
    let text = String::from_utf8_lossy(&jsonl.stdout);
    assert!(
        text.contains("\"profile\":\"a\""),
        "jsonl missing profile: {text}"
    );
    assert!(
        text.contains("\"profile\":\"a\""),
        "run.started missing profile"
    );
    let md = r2.out(true, &["export", "--thread", &t, "--format", "md"]);
    assert!(md.status.success());
    let md = String::from_utf8_lossy(&md.stdout);
    assert!(
        md.contains("- profile: a"),
        "markdown missing profile: {md}"
    );
}

// ── P3: disk accounting must not follow symlinks ────────────────────────────

#[test]
fn r2_p3_disk_kb_does_not_follow_symlinks() {
    let r2 = R2::new("r2-p3-disk");
    r2.create("a");
    let external = r2.env.root.join("external-disk");
    std::fs::create_dir_all(&external).unwrap();
    std::fs::write(external.join("big"), vec![0u8; 2 * 1024 * 1024]).unwrap();
    // A nested symlink is not itself a profile component. Listing must
    // continue to work, but disk accounting must not traverse it.
    std::os::unix::fs::symlink(&external, r2.home("a").join("evil")).unwrap();
    let v = r2.ok(true, &["profile", "list"]);
    let a = v
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["agent_id"] == "a")
        .unwrap();
    assert!(
        a["disk_kb"].as_u64().unwrap() < 1024,
        "disk_kb followed an external symlink: {a}"
    );
}

// ── R3: lock inheritance, tombstones and owner lifecycle ────────────────────

fn wait_run_state(r2: &R2, thread: &str, want: &str, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if r2.latest_run(thread).is_some_and(|run| run.state == want) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn wait_lock_free(r2: &R2, agent: &str, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        let list = r2.ok(true, &["profile", "list"]);
        let state = list
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["agent_id"] == agent)
            .map(|p| p["lock_state"].as_str().unwrap_or("unknown"))
            .unwrap_or("missing");
        if state == "free" {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn r3_p1_1_successful_auth_descendant_cannot_retain_writer_lock() {
    let r2 = R2::new("r3-auth-escape-ok");
    r2.create("a");
    r2.auth_mode("a", "auth-escape-clean");
    r2.init_mode("a", "valid");
    let t = r2.thread_new();
    r2.assign("a", &t);
    let v = r2.ok(
        true,
        &[
            "run",
            "start",
            "--thread",
            &t,
            "--prompt",
            "x",
            "--backend",
            "hermes",
        ],
    );
    assert_eq!(v["state"], "completed");
    assert!(
        r2.wait_for(&r2.home("a").join("fake-auth-descendant-pid"), 5),
        "auth descendant was not created"
    );
    let list = r2.ok(true, &["profile", "list"]);
    let a = list
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["agent_id"] == "a")
        .unwrap();
    assert_eq!(
        a["lock_state"], "free",
        "auth descendant retained the profile writer lock"
    );
    assert!(
        !r2.agent_dir("a").join("owner.json").exists(),
        "completed run left owner evidence behind"
    );
}

#[test]
fn r3_p1_1_timed_out_auth_descendant_cannot_retain_writer_lock() {
    let r2 = R2::new("r3-auth-escape-timeout");
    r2.create("a");
    r2.auth_mode("a", "auth-timeout-escape");
    let t = r2.thread_new();
    r2.assign("a", &t);
    let out = r2
        .cmd(
            true,
            &[
                "run",
                "start",
                "--thread",
                &t,
                "--prompt",
                "x",
                "--backend",
                "hermes",
            ],
            &[("AERIL_PROFILE_AUTH_TIMEOUT_SECS", "1".into())],
        )
        .output()
        .unwrap();
    assert!(!out.status.success(), "timeout auth must reject admission");
    assert!(
        r2.wait_for(&r2.home("a").join("fake-auth-descendant-pid"), 5),
        "auth descendant was not created"
    );
    let list = r2.ok(true, &["profile", "list"]);
    let a = list
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["agent_id"] == "a")
        .unwrap();
    assert_eq!(a["lock_state"], "free");
}

#[test]
fn r3_p1_1_malformed_auth_descendant_cannot_retain_writer_lock() {
    let r2 = R2::new("r3-auth-escape-bad");
    r2.create("a");
    r2.auth_mode("a", "auth-malformed-escape");
    let t = r2.thread_new();
    r2.assign("a", &t);
    let out = r2
        .cmd(
            true,
            &[
                "run",
                "start",
                "--thread",
                &t,
                "--prompt",
                "x",
                "--backend",
                "hermes",
            ],
            &[("AERIL_PROFILE_AUTH_TIMEOUT_SECS", "1".into())],
        )
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "malformed auth must reject admission"
    );
    assert!(r2.wait_for(&r2.home("a").join("fake-auth-descendant-pid"), 5));
    let list = r2.ok(true, &["profile", "list"]);
    let a = list
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["agent_id"] == "a")
        .unwrap();
    assert_eq!(a["lock_state"], "free");
}

#[test]
fn r3_p1_2_tombstone_blocks_run_and_warm() {
    let r2 = R2::new("r3-tombstone-pre");
    r2.create("a");
    r2.auth_mode("a", "positive");
    let t = r2.thread_new();
    r2.assign("a", &t);
    mark_removing(&r2, "a");
    let err = r2.err(
        true,
        &[
            "run",
            "start",
            "--thread",
            &t,
            "--prompt",
            "x",
            "--backend",
            "hermes",
        ],
    );
    assert!(err.contains("removing") || err.contains("borrado"), "{err}");
    assert!(
        r2.latest_run(&t).is_none(),
        "run was admitted under tombstone"
    );
    let err = r2.err(true, &["profile", "warm", "--agent", "a"]);
    assert!(err.contains("removing") || err.contains("borrado"), "{err}");
    let list = r2.ok(true, &["profile", "list"]);
    let a = list
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["agent_id"] == "a")
        .unwrap();
    assert_eq!(a["dir_state"], "removing");
    // Diagnostic status still works and does not initiate auth.
    let status = r2.ok(true, &["profile", "status", "--agent", "a"]);
    assert_eq!(status["dir_state"], "removing");
}

#[test]
fn r3_p1_2_tombstone_during_slow_auth_rejects_admission() {
    let r2 = R2::new("r3-tombstone-race");
    r2.create("a");
    r2.auth_mode("a", "slow:2");
    r2.init_mode("a", "valid");
    let t = r2.thread_new();
    r2.assign("a", &t);
    let run = r2.spawn_run(&t, &[]);
    assert!(
        r2.wait_for(&r2.home("a").join("fake-auth-started"), 10),
        "slow auth did not start"
    );
    mark_removing(&r2, "a");
    let out = run.wait_with_output().unwrap();
    assert!(!out.status.success(), "admission completed under tombstone");
    assert!(
        r2.latest_run(&t).is_none(),
        "run row created under tombstone"
    );
}

#[test]
fn r3_p1_2_sigkill_after_tombstone_removal_resumes() {
    let r2 = R2::new("r3-tombstone-kill");
    r2.create("a");
    let _ = r2.thread_new();
    let barrier = r2.env.root.join("barrier-tombstone");
    let mut remove = r2
        .cmd(
            true,
            &["profile", "remove", "--agent", "a", "--confirm"],
            &[(
                "AERIL_TEST_REMOVE_AFTER_TOMBSTONE_BARRIER",
                barrier.to_string_lossy().into_owned(),
            )],
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    assert!(r2.wait_for(&barrier.join("ready"), 15));
    unsafe {
        libc::kill(remove.id() as i32, libc::SIGKILL);
    }
    let _ = remove.wait();
    let reg: Value =
        serde_json::from_slice(&std::fs::read(r2.env.data.join("hermes/agents.json")).unwrap())
            .unwrap();
    assert_eq!(reg["agents"]["a"]["removing"], true);
    assert!(r2.agent_dir("a").exists());
    let out = r2.out(true, &["profile", "remove", "--agent", "a", "--confirm"]);
    assert!(
        out.status.success(),
        "removal did not resume after SIGKILL: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!r2.agent_dir("a").exists());
}

#[test]
fn r3_p2_1_long_lived_supervisor_owner_is_cleared_after_run() {
    let r2 = R2::new("r3-tui-owner");
    r2.create("a");
    r2.auth_mode("a", "positive");
    r2.init_mode("a", "valid");
    let t = r2.thread_new();
    r2.assign("a", &t);
    let path = r2.path_env();
    let hermes = r2.bin.join("hermes").to_string_lossy().into_owned();
    let extra = [
        ("AERIL_HERMES_PROFILES", "1"),
        ("PATH", path.as_str()),
        ("AERIL_HERMES_BIN", hermes.as_str()),
        ("AERIL_BACKEND", "hermes"),
    ];
    let mut tui = TuiPty::spawn(&r2.env, &[], &extra);
    tui.pump(1500);
    tui.enter();
    tui.pump(500);
    tui.send(b"n");
    tui.pump(300);
    tui.type_line("tui goal");
    tui.pump(500);
    assert!(
        wait_run_state(&r2, &t, "completed", 20),
        "TUI run did not complete"
    );
    assert!(tui.alive(), "TUI exited before owner assertion");
    assert!(
        wait_lock_free(&r2, "a", 5),
        "live supervisor left a persistent writer lock"
    );
    let list = r2.ok(true, &["profile", "list"]);
    let a = list
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["agent_id"] == "a")
        .unwrap();
    assert_eq!(a["lock_state"], "free", "live supervisor left false HOLD");
    r2.ok(true, &["profile", "warm", "--agent", "a"]);
    // A second admission from another process must also work.
    let v = r2.ok(
        true,
        &[
            "run",
            "start",
            "--thread",
            &t,
            "--prompt",
            "cli goal",
            "--backend",
            "hermes",
        ],
    );
    assert_eq!(v["state"], "completed");
    tui.kill();
}

#[test]
fn r3_p2_1_escaped_backend_prevents_owner_cleanup() {
    let r2 = R2::new("r3-owner-survivor");
    r2.create("a");
    r2.auth_mode("a", "positive");
    r2.init_mode("a", "escape-on-turn");
    let t = r2.thread_new();
    r2.assign("a", &t);
    let v = r2.ok(
        true,
        &[
            "run",
            "start",
            "--thread",
            &t,
            "--prompt",
            "x",
            "--backend",
            "hermes",
        ],
    );
    assert_eq!(v["state"], "failed", "escaped backend must fail cleanup");
    assert!(r2.wait_for(&r2.home("a").join("fake-escaped-pid"), 5));
    assert!(
        r2.agent_dir("a").join("owner.json").exists(),
        "owner evidence was cleared with a surviving backend"
    );
    let list = r2.ok(true, &["profile", "list"]);
    let a = list
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["agent_id"] == "a")
        .unwrap();
    assert_eq!(a["lock_state"], "held");
}

#[test]
fn r3_p2_1_owner_clear_failure_is_not_reported_as_clean() {
    let r2 = R2::new("r3-owner-clear-fail");
    r2.create("a");
    r2.auth_mode("a", "positive");
    r2.init_mode("a", "valid");
    let t = r2.thread_new();
    r2.assign("a", &t);
    let out = r2
        .cmd(
            true,
            &[
                "run",
                "start",
                "--thread",
                &t,
                "--prompt",
                "x",
                "--backend",
                "hermes",
            ],
            &[("AERIL_TEST_FAIL_OWNER_CLEAR", "1".into())],
        )
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "run command itself should print a summary"
    );
    let v: Value = parse_json(&out.stdout);
    assert_eq!(v["state"], "failed", "owner clear failure was ignored");
    assert!(r2.agent_dir("a").join("owner.json").exists());
}

#[test]
fn r3_p2_1_reused_supervisor_pid_is_stale() {
    let r2 = R2::new("r3-owner-reused");
    r2.create("a");
    let lock = r2.agent_dir("a").join("aeril.lock");
    std::fs::write(&lock, b"").unwrap();
    let meta = std::fs::metadata(&lock).unwrap();
    let mut sleeper = Command::new("sleep").arg("30").spawn().unwrap();
    let pid = sleeper.id() as i32;
    let start = proc_start(pid).unwrap();
    std::fs::write(
        r2.agent_dir("a").join("owner.json"),
        serde_json::json!({
            "supervisor_pid": pid,
            "supervisor_start": start + 1,
            "backend_pid": 0,
            "backend_start": 0,
            "pgid": 0,
            "started_ms": 1,
            "lock_dev": meta.dev(),
            "lock_ino": meta.ino(),
        })
        .to_string(),
    )
    .unwrap();
    let out = r2.out(true, &["profile", "warm", "--agent", "a"]);
    assert!(
        out.status.success(),
        "reused supervisor PID caused a false HOLD: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = sleeper.kill();
    let _ = sleeper.wait();
}

#[test]
fn r3_p2_2_backend_owner_write_failure_aborts_run() {
    let r2 = R2::new("r3-backend-owner-fail");
    r2.create("a");
    r2.auth_mode("a", "positive");
    r2.init_mode("a", "valid");
    let t = r2.thread_new();
    r2.assign("a", &t);
    let out = r2
        .cmd(
            true,
            &[
                "run",
                "start",
                "--thread",
                &t,
                "--prompt",
                "x",
                "--backend",
                "hermes",
            ],
            &[("AERIL_TEST_FAIL_BACKEND_OWNER_WRITE", "1".into())],
        )
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: Value = parse_json(&out.stdout);
    assert_eq!(v["state"], "failed");
    assert!(
        !r2.agent_dir("a").join("owner.json").exists(),
        "clean failed spawn should leave no owner evidence"
    );
}
