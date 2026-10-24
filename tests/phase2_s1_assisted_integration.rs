//! P2-S1: integración asistida (preview → confirmación → CAS).

mod common;

use aeril::integration::{self, IntegrationStatus, ValidationState};
use aeril::session;
use aeril::store::{Store, events};
use common::pty::TuiPty;
use common::*;
use serde_json::{Value, json};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

struct Fx {
    env: TestEnv,
    thread: String,
    snapshot: String,
    source: String,
    base: String,
}

impl Fx {
    fn new(name: &str) -> Fx {
        let env = TestEnv::new(name);
        let base = env.head_oid();
        env.update_ref("refs/heads/destino", &base);
        let thread = env.thread_new("assisted integration");
        let script = env.script(
            "s",
            mock_edit("NOTES.md", "# v2\n\ncontenido asistido\n", "edit"),
        );
        let run = env.run_start(&thread, "cambiá NOTES", &script);
        assert_eq!(run["state"], "completed", "run: {run}");
        let snapshots = env.ready_snapshots(&thread);
        assert_eq!(snapshots.len(), 1, "un checkpoint ready: {snapshots:?}");
        Fx {
            env,
            thread,
            snapshot: snapshots[0].0.clone(),
            source: snapshots[0].1.clone(),
            base,
        }
    }

    fn store(&self) -> Store {
        Store::open(&self.env.data.join("aeril.db")).expect("store")
    }

    fn preview(&self, target: &str) -> integration::IntegrationPreview {
        integration::preview(&self.store(), &self.thread, &self.snapshot, target).expect("preview")
    }

    fn validate_ok(&self) {
        let out = self.env.aeril(&[
            "validate",
            "--thread",
            &self.thread,
            "--snapshot",
            &self.snapshot,
            "--",
            "true",
        ]);
        assert!(
            out.status.success(),
            "validate true falló: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn validate_cmd(&self, cmd: &[&str]) -> Value {
        let mut args = vec![
            "validate",
            "--thread",
            self.thread.as_str(),
            "--snapshot",
            self.snapshot.as_str(),
            "--",
        ];
        args.extend(cmd);
        let out = self.env.aeril(&args);
        parse_json(&out.stdout)
    }

    fn apply_to(
        &self,
        target: &str,
        expected_target: &str,
    ) -> Result<integration::AppliedIntegration, String> {
        let mut store = self.store();
        integration::apply(
            &mut store,
            &self.thread,
            &self.snapshot,
            target,
            &self.source,
            expected_target,
        )
        .map_err(|e| e.to_string())
    }

    fn apply(&self, expected_target: &str) -> Result<integration::AppliedIntegration, String> {
        self.apply_to("destino", expected_target)
    }
}

fn count_kind(env: &TestEnv, thread: &str, kind: &str) -> usize {
    let store = Store::open(&env.data.join("aeril.db")).expect("store");
    events::fetch_by_kind(&store.conn, thread, kind)
        .expect("events")
        .len()
}

fn advance_target(env: &TestEnv, target: &str) -> String {
    let main = env.rev_parse("main");
    git(&env.repo, &["checkout", "-q", "-b", "tmp-advance"]);
    std::fs::write(env.repo.join("ADVANCE.md"), "avance\n").unwrap();
    git(&env.repo, &["add", "-A"]);
    git(&env.repo, &["commit", "-q", "-m", "advance target"]);
    let oid = env.rev_parse("HEAD");
    git(&env.repo, &["checkout", "-q", "main"]);
    assert_eq!(env.rev_parse("main"), main);
    env.update_ref(&format!("refs/heads/{target}"), &oid);
    git(&env.repo, &["branch", "-D", "tmp-advance"]);
    oid
}

fn apply_cli(fx: &Fx, expect: &str, confirm: bool) -> Output {
    let mut args = vec![
        "integrate",
        "apply",
        "--thread",
        fx.thread.as_str(),
        "--snapshot",
        fx.snapshot.as_str(),
        "--to",
        "destino",
        "--source",
        fx.source.as_str(),
        "--expect",
        expect,
    ];
    if confirm {
        args.push("--confirm");
    }
    fx.env.aeril(&args)
}

// ── I1: successful assisted integration ─────────────────────────────────────

#[test]
fn i1_successful_assisted_integration() {
    let fx = Fx::new("p2s1-i1");
    fx.validate_ok();
    let before = fx.env.rev_parse("destino");
    assert_eq!(before, fx.base);

    let p = fx.preview("destino");
    assert!(p.eligible, "preview: {p:?}");
    assert_eq!(p.status, IntegrationStatus::EligibleFastForward);
    assert_eq!(p.source_oid.as_deref(), Some(fx.source.as_str()));
    assert_eq!(p.target_oid.as_deref(), Some(fx.base.as_str()));
    assert_eq!(p.validation, ValidationState::Passed);
    assert!(p.files_changed >= 1);
    assert!(p.additions >= 1);
    // Preview es observación: no movió la ref ni escribió intención.
    assert_eq!(fx.env.rev_parse("destino"), before);
    assert_eq!(
        count_kind(&fx.env, &fx.thread, events::K_INTEGRATION_REQUESTED),
        0
    );

    let applied = fx.apply(&fx.base).expect("apply");
    assert!(!applied.noop);
    assert_eq!(applied.target_oid_after, fx.source);
    assert_eq!(fx.env.rev_parse("destino"), fx.source);
    assert_eq!(
        count_kind(&fx.env, &fx.thread, events::K_INTEGRATION_COMPLETED),
        1
    );
    let again = fx.preview("destino");
    assert_eq!(again.status, IntegrationStatus::AlreadyIntegrated);
    assert_eq!(again.operation, "noop");
    assert!(!again.eligible);
}

// ── I2 / I15: TUI preview, cancellation and explicit y ──────────────────────

#[test]
fn i2_tui_cancel_and_confirm_require_explicit_second_action() {
    let fx = Fx::new("p2s1-i2");
    fx.validate_ok();
    let before = fx.env.rev_parse("destino");
    let mut pty = TuiPty::spawn(&fx.env, &[], &[]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !pty.output_contains("Aeril") {
        assert!(
            Instant::now() < deadline,
            "la TUI no dibujó el primer frame"
        );
        pty.pump(100);
    }
    pty.enter(); // abrir el único thread
    pty.pump(300);
    pty.send(b"i");
    pty.pump(150);
    // El default es "main": borrar 4 chars y escribir destino.
    pty.send(b"\x7f\x7f\x7f\x7f");
    pty.type_line("destino");
    pty.pump(500);
    assert!(
        pty.output_contains("Integration preview"),
        "falta preview: {}",
        pty.output_lossy()
    );
    assert!(
        pty.output_contains("fast-forward eligible"),
        "falta elegibilidad: {}",
        pty.output_lossy()
    );
    assert_eq!(
        fx.env.rev_parse("destino"),
        before,
        "preview no debe escribir"
    );
    // Enter dentro del preview NO ejecuta.
    pty.enter();
    pty.pump(200);
    assert_eq!(fx.env.rev_parse("destino"), before);
    // Esc cancela sin evento.
    pty.esc();
    pty.pump(200);
    assert_eq!(fx.env.rev_parse("destino"), before);
    assert_eq!(
        count_kind(&fx.env, &fx.thread, events::K_INTEGRATION_REQUESTED),
        0
    );
    assert_eq!(
        count_kind(&fx.env, &fx.thread, events::K_INTEGRATION_BLOCKED),
        0
    );
    // Reabrir y confirmar con y.
    pty.send(b"i");
    pty.pump(150);
    pty.send(b"\x7f\x7f\x7f\x7f");
    pty.type_line("destino");
    pty.pump(500);
    pty.send(b"y");
    pty.pump(700);
    assert_eq!(fx.env.rev_parse("destino"), fx.source, "y debe integrar");
    assert_eq!(
        count_kind(&fx.env, &fx.thread, events::K_INTEGRATION_COMPLETED),
        1
    );
    pty.kill();
}

// ── I3: missing validation blocks assisted, legacy CLI still works ──────────

#[test]
fn i3_missing_validation_blocks_assisted_and_legacy_remains_compatible() {
    let fx = Fx::new("p2s1-i3");
    let before = fx.env.rev_parse("destino");
    let p = fx.preview("destino");
    assert_eq!(p.status, IntegrationStatus::ValidationRequired);
    assert!(!p.eligible);
    let out = apply_cli(&fx, &fx.base, true);
    assert!(!out.status.success(), "apply sin validación debe fallar");
    assert_eq!(fx.env.rev_parse("destino"), before);
    assert_eq!(
        count_kind(&fx.env, &fx.thread, events::K_INTEGRATION_COMPLETED),
        0
    );

    // La ruta legacy NO cambia de contrato ni exige validación.
    let legacy = fx.env.aeril_ok(&[
        "integrate",
        "--thread",
        &fx.thread,
        "--snapshot",
        &fx.snapshot,
        "--to",
        "destino",
        "--expect",
        &fx.base,
    ]);
    assert_eq!(legacy["target_oid_after"], fx.source.as_str());
    assert_eq!(fx.env.rev_parse("destino"), fx.source);
}

// ── I4: failed validation ───────────────────────────────────────────────────

#[test]
fn i4_failed_validation_blocks() {
    let fx = Fx::new("p2s1-i4");
    let record = fx.validate_cmd(&["sh", "-c", "exit 7"]);
    assert_eq!(record["exit_code"], 7);
    let p = fx.preview("destino");
    assert_eq!(p.status, IntegrationStatus::ValidationFailed);
    assert_eq!(p.validation, ValidationState::Failed);
    assert!(fx.apply(&fx.base).is_err());
    assert_eq!(fx.env.rev_parse("destino"), fx.base);
}

// ── I5: validation is snapshot-scoped ───────────────────────────────────────

#[test]
fn i5_validation_of_a_does_not_validate_b() {
    let fx = Fx::new("p2s1-i5");
    fx.validate_ok();
    let script = fx.env.script(
        "s2",
        mock_edit("NOTES.md", "# v2\n\nsegundo snapshot\n", "edit2"),
    );
    let run = fx.env.run_start(&fx.thread, "segundo", &script);
    assert_eq!(run["state"], "completed");
    let ready = fx.env.ready_snapshots(&fx.thread);
    assert_eq!(ready.len(), 2, "dos checkpoints: {ready:?}");
    let other = ready
        .iter()
        .find(|(sid, _)| *sid != fx.snapshot)
        .expect("snapshot B")
        .0
        .clone();
    let store = fx.store();
    let p = integration::preview(&store, &fx.thread, &other, "destino").expect("preview B");
    assert_eq!(p.status, IntegrationStatus::ValidationRequired);
    assert_eq!(p.validation, ValidationState::Missing);
}

// ── I6: checkout modified during validation ─────────────────────────────────

#[test]
fn i6_modified_validation_checkout_blocks() {
    let fx = Fx::new("p2s1-i6");
    let record = fx.validate_cmd(&["sh", "-c", "echo changed >> NOTES.md"]);
    assert_eq!(record["exit_code"], 0);
    assert_eq!(record["tree_after_unchanged"], false);
    let p = fx.preview("destino");
    assert_eq!(p.status, IntegrationStatus::ValidationFailed);
    assert_eq!(p.validation, ValidationState::Failed);
    assert!(fx.apply(&fx.base).is_err());
}

// ── I7: target advanced after preview is stale, never adopted ───────────────

#[test]
fn i7_target_advanced_after_preview_is_stale() {
    let fx = Fx::new("p2s1-i7");
    fx.validate_ok();
    let p = fx.preview("destino");
    assert_eq!(p.target_oid.as_deref(), Some(fx.base.as_str()));
    let advanced = advance_target(&fx.env, "destino");
    let err = fx.apply(&fx.base).expect_err("target moved must fail");
    assert!(
        err.contains("se movió") || err.contains("rama destino"),
        "error: {err}"
    );
    assert_eq!(fx.env.rev_parse("destino"), advanced, "no adopta B");
    assert_ne!(advanced, fx.source);
    assert_eq!(
        count_kind(&fx.env, &fx.thread, events::K_INTEGRATION_COMPLETED),
        0
    );
    assert_eq!(
        count_kind(&fx.env, &fx.thread, events::K_INTEGRATION_REQUESTED),
        0
    );
}

// ── I8: concurrent assisted apply: one CAS winner ───────────────────────────

#[test]
fn i8_concurrent_assisted_apply_has_one_winner() {
    let env = TestEnv::new("p2s1-i8");
    let base = env.head_oid();
    env.update_ref("refs/heads/destino", &base);
    let mut threads = Vec::new();
    for n in 0..2 {
        let t = env.thread_new(&format!("concurrent {n}"));
        let script = env.script(
            &format!("s{n}"),
            mock_edit("NOTES.md", &format!("# v2\n\nconcurrent {n}\n"), "edit"),
        );
        assert_eq!(env.run_start(&t, "x", &script)["state"], "completed");
        let snap = env.ready_snapshots(&t)[0].clone();
        let v = env.aeril_ok(&[
            "validate",
            "--thread",
            &t,
            "--snapshot",
            &snap.0,
            "--",
            "true",
        ]);
        assert_eq!(v["exit_code"], 0);
        threads.push((t, snap.0, snap.1));
    }
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let db = env.data.join("aeril.db");
    let results: Vec<(String, Result<integration::AppliedIntegration, String>)> =
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for (t, s, oid) in &threads {
                let barrier = barrier.clone();
                let db = db.clone();
                let t = t.clone();
                let s = s.clone();
                let oid = oid.clone();
                let base = base.clone();
                handles.push(scope.spawn(move || {
                    barrier.wait();
                    let mut store = Store::open(&db).expect("store");
                    let r = integration::apply(&mut store, &t, &s, "destino", &oid, &base)
                        .map_err(|e| e.to_string());
                    (oid, r)
                }));
            }
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
    let winners: Vec<_> = results.iter().filter(|(_, r)| r.is_ok()).collect();
    assert_eq!(winners.len(), 1, "resultados: {results:?}");
    let winner_oid = &winners[0].0;
    assert_eq!(env.rev_parse("destino"), *winner_oid);
    let store = Store::open(&db).expect("store");
    let completed =
        events::fetch_by_kind(&store.conn, &threads[0].0, events::K_INTEGRATION_COMPLETED)
            .unwrap()
            .len()
            + events::fetch_by_kind(&store.conn, &threads[1].0, events::K_INTEGRATION_COMPLETED)
                .unwrap()
                .len();
    assert_eq!(completed, 1, "un solo completed durable");
}

// ── I9: diverged branches ───────────────────────────────────────────────────

#[test]
fn i9_diverged_branches_are_blocked_without_merge() {
    let fx = Fx::new("p2s1-i9");
    fx.validate_ok();
    let advanced = advance_target(&fx.env, "destino");
    assert_ne!(advanced, fx.source);
    let p = fx.preview("destino");
    assert_eq!(p.status, IntegrationStatus::Diverged);
    assert!(!p.eligible);
    assert!(fx.apply(&advanced).is_err());
    assert_eq!(fx.env.rev_parse("destino"), advanced);
    assert_eq!(
        count_kind(&fx.env, &fx.thread, events::K_INTEGRATION_COMPLETED),
        0
    );
}

// ── I10: checked-out target ─────────────────────────────────────────────────

#[test]
fn i10_checked_out_target_is_blocked() {
    let fx = Fx::new("p2s1-i10");
    fx.validate_ok();
    let main_before = fx.env.rev_parse("main");
    let p = fx.preview("main");
    assert_eq!(p.status, IntegrationStatus::TargetCheckedOut);
    assert!(!p.eligible);
    assert_eq!(fx.env.rev_parse("main"), main_before);
    assert!(fx.apply_to("main", &main_before).is_err());
    assert_eq!(fx.env.rev_parse("main"), main_before);
    assert_eq!(
        count_kind(&fx.env, &fx.thread, events::K_INTEGRATION_REQUESTED),
        0
    );
}

// ── I11: pending checkpoint is not ready ────────────────────────────────────

#[test]
fn i11_pending_checkpoint_is_not_eligible() {
    let env = TestEnv::new("p2s1-i11");
    let base = env.head_oid();
    env.update_ref("refs/heads/destino", &base);
    let thread = env.thread_new("pending");
    let script = env.script("s", mock_edit("NOTES.md", "# v2\n\npending\n", "edit"));
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
    let run = parse_json(&out.stdout);
    assert_eq!(run["state"], "completed");
    assert!(run["snapshot"].is_null());
    let snaps = env.checkpoint_list(&thread);
    let prepared = snaps
        .iter()
        .find(|c| c["state"] == "prepared")
        .expect("prepared durable");
    let snap = prepared["snapshot_id"].as_str().unwrap();
    let store = Store::open(&env.data.join("aeril.db")).unwrap();
    let p = integration::preview(&store, &thread, snap, "destino").unwrap();
    assert_eq!(p.status, IntegrationStatus::SnapshotNotReady);
    assert!(!p.eligible);
    let mut store = Store::open(&env.data.join("aeril.db")).unwrap();
    assert!(integration::apply(&mut store, &thread, snap, "destino", "0", &base).is_err());
    assert_eq!(env.rev_parse("destino"), base);
}

// ── I12: corrupt/contradictory checkpoint fail-closed ──────────────────────

#[test]
fn i12_contradictory_checkpoint_fails_closed() {
    let fx = Fx::new("p2s1-i12");
    fx.validate_ok();
    let mut store = fx.store();
    let bad = json!({
        "snapshot_id": fx.snapshot,
        "git_ref": aeril::ids::checkpoint_ref(&fx.snapshot),
        "oid": "f".repeat(40),
        "base_oid": fx.base,
        "patch": "",
        "files": [],
        "recovered": true,
    });
    events::append_typed(
        &mut store.conn,
        &fx.thread,
        None,
        events::K_CHECKPOINT_READY,
        "system",
        &bad,
    )
    .unwrap();
    let p = integration::preview(&store, &fx.thread, &fx.snapshot, "destino").unwrap();
    assert!(
        matches!(
            p.status,
            IntegrationStatus::SnapshotInvalid | IntegrationStatus::IntegrityError
        ),
        "preview: {p:?}"
    );
    assert!(!p.eligible);
}

// ── I17: review is snapshot-scoped ──────────────────────────────────────────

#[test]
fn i17_unresolved_review_blocks_only_its_snapshot() {
    let fx = Fx::new("p2s1-i17");
    fx.validate_ok();
    let comment = fx.env.aeril_ok(&[
        "comment",
        "add",
        "--thread",
        &fx.thread,
        "--snapshot",
        &fx.snapshot,
        "--path",
        "NOTES.md",
        "--start",
        "1",
        "--end",
        "1",
        "--text",
        "revisar",
    ]);
    let cid = comment["comment_id"].as_str().unwrap().to_string();
    let p = fx.preview("destino");
    assert_eq!(p.status, IntegrationStatus::ReviewPending);
    assert_eq!(p.unresolved_comments, 1);
    assert!(fx.apply(&fx.base).is_err());

    fx.env.aeril_ok(&[
        "comment",
        "resolve",
        "--thread",
        &fx.thread,
        "--comment",
        &cid,
    ]);
    assert!(fx.preview("destino").eligible);

    // Historial no relacionado: segundo checkpoint con comentario abierto.
    let script = fx.env.script(
        "s2",
        mock_edit("NOTES.md", "# v2\n\nsegundo review\n", "edit2"),
    );
    fx.env.run_start(&fx.thread, "segundo", &script);
    let ready = fx.env.ready_snapshots(&fx.thread);
    let other = ready
        .iter()
        .find(|(sid, _)| *sid != fx.snapshot)
        .expect("snapshot B")
        .0
        .clone();
    fx.env.aeril_ok(&[
        "comment",
        "add",
        "--thread",
        &fx.thread,
        "--snapshot",
        &other,
        "--path",
        "NOTES.md",
        "--start",
        "1",
        "--end",
        "1",
        "--text",
        "histórico",
    ]);
    let p = fx.preview("destino");
    assert!(
        p.eligible,
        "un comentario de otro snapshot no debe bloquear: {p:?}"
    );
    assert_eq!(p.unresolved_comments, 0);
}

// ── CLI contract: preview JSON, --confirm required, no implicit apply ───────

#[test]
fn cli_preview_json_and_confirm_are_stable() {
    let fx = Fx::new("p2s1-cli");
    fx.validate_ok();
    let p = fx.env.aeril_ok(&[
        "integrate",
        "preview",
        "--thread",
        &fx.thread,
        "--snapshot",
        &fx.snapshot,
        "--to",
        "destino",
    ]);
    assert_eq!(p["eligible"], true);
    assert_eq!(p["status"], "eligible_fast_forward");
    assert_eq!(p["operation"], "fast_forward");
    assert_eq!(p["validation"], "passed");
    assert_eq!(p["review"], "clear");
    assert_eq!(p["source_oid"], fx.source.as_str());
    assert_eq!(p["target_oid"], fx.base.as_str());
    assert_eq!(fx.env.rev_parse("destino"), fx.base);

    let no_confirm = apply_cli(&fx, &fx.base, false);
    assert!(!no_confirm.status.success());
    assert_eq!(fx.env.rev_parse("destino"), fx.base);

    let wrong = apply_cli(&fx, "0".repeat(40).as_str(), true);
    assert!(!wrong.status.success());
    assert_eq!(fx.env.rev_parse("destino"), fx.base);

    let applied = apply_cli(&fx, &fx.base, true);
    assert!(
        applied.status.success(),
        "apply: {}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert_eq!(fx.env.rev_parse("destino"), fx.source);
}

#[test]
fn invalid_branch_names_are_rejected_by_git_rules() {
    let fx = Fx::new("p2s1-branch");
    fx.validate_ok();
    for bad in ["bad..name", "bad name", "-leading", "refs/heads/"] {
        let p = fx.preview(bad);
        assert!(
            matches!(
                p.status,
                IntegrationStatus::TargetInvalid | IntegrationStatus::TargetMissing
            ),
            "{bad:?}: {p:?}"
        );
        assert!(!p.eligible);
    }
}

fn apply_cli_env(fx: &Fx, expect: &str, confirm: bool, extra: &[(&str, &str)]) -> Output {
    let mut args = vec![
        "integrate",
        "apply",
        "--thread",
        fx.thread.as_str(),
        "--snapshot",
        fx.snapshot.as_str(),
        "--to",
        "destino",
        "--source",
        fx.source.as_str(),
        "--expect",
        expect,
    ];
    if confirm {
        args.push("--confirm");
    }
    fx.env.aeril_env(&args, extra)
}

fn append_validation(fx: &Fx, record: &events::payload::ValidationRecorded) {
    let mut store = fx.store();
    events::append_typed(
        &mut store.conn,
        &fx.thread,
        None,
        events::K_VALIDATION_RECORDED,
        "system",
        record,
    )
    .unwrap();
}

// ── I5b: validation identity/OID is strict ─────────────────────────────────

#[test]
fn i5b_validation_with_wrong_oid_or_malformed_payload_blocks() {
    let fx = Fx::new("p2s1-i5b");
    append_validation(
        &fx,
        &events::payload::ValidationRecorded {
            snapshot_id: fx.snapshot.clone(),
            oid: fx.base.clone(),
            command: "true".into(),
            exit_code: 0,
            duration_ms: 1,
            output_hash: "hash".into(),
            output_tail: String::new(),
            stdout_bytes: 0,
            stderr_bytes: 0,
            stdout_hash: "s".into(),
            stderr_hash: "e".into(),
            tree_before_ok: true,
            tree_after_unchanged: true,
            checkout_id: "wrong-oid".into(),
        },
    );
    let p = fx.preview("destino");
    assert_eq!(p.status, IntegrationStatus::ValidationFailed);
    assert_eq!(p.validation, ValidationState::WrongOid);

    let mut store = fx.store();
    events::append_typed(
        &mut store.conn,
        &fx.thread,
        None,
        events::K_VALIDATION_RECORDED,
        "system",
        &json!({"snapshot_id": fx.snapshot}),
    )
    .unwrap();
    let p = fx.preview("destino");
    assert_eq!(p.status, IntegrationStatus::ValidationFailed);
    assert_eq!(p.validation, ValidationState::Invalid);
}

// ── I7b: a failed validation appended after preview blocks apply ───────────

#[test]
fn i7b_validation_appended_after_preview_blocks_apply() {
    let fx = Fx::new("p2s1-i7b");
    fx.validate_ok();
    assert!(fx.preview("destino").eligible);
    append_validation(
        &fx,
        &events::payload::ValidationRecorded {
            snapshot_id: fx.snapshot.clone(),
            oid: fx.source.clone(),
            command: "false".into(),
            exit_code: 1,
            duration_ms: 1,
            output_hash: "hash".into(),
            output_tail: String::new(),
            stdout_bytes: 0,
            stderr_bytes: 0,
            stdout_hash: "s".into(),
            stderr_hash: "e".into(),
            tree_before_ok: true,
            tree_after_unchanged: true,
            checkout_id: "late-fail".into(),
        },
    );
    assert!(fx.apply(&fx.base).is_err());
    assert_eq!(fx.env.rev_parse("destino"), fx.base);
    assert_eq!(
        count_kind(&fx.env, &fx.thread, events::K_INTEGRATION_REQUESTED),
        0
    );
}

// ── I7c: a comment appended after preview blocks apply ─────────────────────

#[test]
fn i7c_review_appended_after_preview_blocks_apply() {
    let fx = Fx::new("p2s1-i7c");
    fx.validate_ok();
    assert!(fx.preview("destino").eligible);
    fx.env.aeril_ok(&[
        "comment",
        "add",
        "--thread",
        &fx.thread,
        "--snapshot",
        &fx.snapshot,
        "--path",
        "NOTES.md",
        "--start",
        "1",
        "--end",
        "1",
        "--text",
        "late comment",
    ]);
    assert!(fx.apply(&fx.base).is_err());
    assert_eq!(fx.env.rev_parse("destino"), fx.base);
}

// ── I11b: recover first, then fresh preview is eligible ────────────────────

#[test]
fn i11b_recover_pending_then_fresh_preview_is_eligible() {
    let env = TestEnv::new("p2s1-i11b");
    let base = env.head_oid();
    env.update_ref("refs/heads/destino", &base);
    let thread = env.thread_new("pending recover");
    let script = env.script("s", mock_edit("NOTES.md", "# v2\n\npending\n", "x"));
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
    assert_eq!(parse_json(&out.stdout)["state"], "completed");
    let prepared = env
        .checkpoint_list(&thread)
        .into_iter()
        .find(|c| c["state"] == "prepared")
        .expect("prepared");
    let snap = prepared["snapshot_id"].as_str().unwrap().to_string();
    env.aeril_ok(&["recover"]);
    let ready = env.ready_snapshots(&thread);
    assert_eq!(ready.len(), 1, "recover debe completar ready: {ready:?}");
    let v = env.aeril_ok(&[
        "validate",
        "--thread",
        &thread,
        "--snapshot",
        &snap,
        "--",
        "true",
    ]);
    assert_eq!(v["exit_code"], 0);
    let store = Store::open(&env.data.join("aeril.db")).unwrap();
    let p = integration::preview(&store, &thread, &snap, "destino").unwrap();
    assert!(p.eligible, "preview tras recover: {p:?}");
}

// ── I10b: target checked out in another worktree ───────────────────────────

#[test]
fn i10b_target_checked_out_in_other_worktree_is_blocked() {
    let fx = Fx::new("p2s1-i10b");
    fx.validate_ok();
    let wt = fx.env.root.join("wt-destino");
    git(
        &fx.env.repo,
        &["worktree", "add", "-q", wt.to_str().unwrap(), "destino"],
    );
    let p = fx.preview("destino");
    assert_eq!(p.status, IntegrationStatus::TargetCheckedOut);
    assert!(!p.eligible);
    git(
        &fx.env.repo,
        &["worktree", "remove", "--force", wt.to_str().unwrap()],
    );
}

// ── I15b: blocked preview cannot be confirmed ──────────────────────────────

#[test]
fn i15_blocked_preview_cannot_be_confirmed_by_y() {
    let fx = Fx::new("p2s1-i15b");
    let before = fx.env.rev_parse("destino");
    let mut pty = TuiPty::spawn(&fx.env, &[], &[]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !pty.output_contains("Aeril") {
        assert!(Instant::now() < deadline, "TUI sin primer frame");
        pty.pump(100);
    }
    pty.enter();
    pty.pump(300);
    pty.send(b"i");
    pty.pump(150);
    pty.send(b"\x7f\x7f\x7f\x7f");
    pty.type_line("destino");
    pty.pump(400);
    assert!(pty.output_contains("validation required"));
    assert_eq!(fx.env.rev_parse("destino"), before);
    pty.send(b"y");
    pty.pump(300);
    assert_eq!(fx.env.rev_parse("destino"), before);
    assert_eq!(
        count_kind(&fx.env, &fx.thread, events::K_INTEGRATION_COMPLETED),
        0
    );
    pty.kill();
}

// ── I13: assisted crash boundaries and recovery ─────────────────────────────

#[test]
fn i13_crash_after_intent_is_recovered_as_abandoned() {
    let fx = Fx::new("p2s1-i13a");
    fx.validate_ok();
    let out = apply_cli_env(
        &fx,
        &fx.base,
        true,
        &[("AERIL_CRASH_AFTER", "integrate_intent")],
    );
    assert_eq!(signal_of(&out.status), Some(9));
    assert_eq!(fx.env.rev_parse("destino"), fx.base);
    let rec = fx.env.aeril_ok(&["recover"]);
    assert!(
        rec.as_array()
            .unwrap()
            .iter()
            .any(|e| e["action"] == "integration_abandoned"),
        "recover: {rec}"
    );
    assert_eq!(
        count_kind(&fx.env, &fx.thread, events::K_INTEGRATION_BLOCKED),
        1
    );
    assert_eq!(
        count_kind(&fx.env, &fx.thread, events::K_INTEGRATION_COMPLETED),
        0
    );
    // Reintento explícito asistido.
    let applied = fx.apply(&fx.base).expect("retry");
    assert_eq!(applied.target_oid_after, fx.source);
    assert_eq!(
        count_kind(&fx.env, &fx.thread, events::K_INTEGRATION_COMPLETED),
        1
    );
}

#[test]
fn i13_crash_after_cas_is_recovered_completed_once() {
    let fx = Fx::new("p2s1-i13b");
    fx.validate_ok();
    let out = apply_cli_env(
        &fx,
        &fx.base,
        true,
        &[("AERIL_CRASH_AFTER", "integrate_committed")],
    );
    assert_eq!(signal_of(&out.status), Some(9));
    assert_eq!(fx.env.rev_parse("destino"), fx.source);
    let rec = fx.env.aeril_ok(&["recover"]);
    assert!(
        rec.as_array()
            .unwrap()
            .iter()
            .any(|e| e["action"] == "integration_completed_recovered"),
        "recover: {rec}"
    );
    assert_eq!(
        count_kind(&fx.env, &fx.thread, events::K_INTEGRATION_COMPLETED),
        1
    );
    // Idempotente.
    let rec2 = fx.env.aeril_ok(&["recover"]);
    assert!(
        !rec2
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["action"].as_str().unwrap().contains("integration")),
        "sin duplicados: {rec2}"
    );
    assert_eq!(
        count_kind(&fx.env, &fx.thread, events::K_INTEGRATION_COMPLETED),
        1
    );
}

// ── I14: large history preview stays bounded ───────────────────────────────

#[test]
fn i14_large_history_preview_is_bounded() {
    let fx = Fx::new("p2s1-i14");
    fx.validate_ok();
    // Dos checkpoints extra en el mismo thread (ready_for_snapshot no debe
    // sustituir el snapshot elegido).
    for n in 0..2 {
        let script = fx.env.script(
            &format!("extra{n}"),
            mock_edit("NOTES.md", &format!("# v2\n\nextra {n}\n"), "edit"),
        );
        assert_eq!(
            fx.env.run_start(&fx.thread, "extra", &script)["state"],
            "completed"
        );
    }
    // 100 threads × 1.000 eventos sintéticos en el mismo store.
    let mut store = fx.store();
    {
        let tx = store
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        for t in 0..100 {
            let tid = format!("bulk-{t:03}");
            tx.execute(
                "INSERT INTO threads
                   (id, title, repo_id, repo_path, state, next_seq, created_ms, updated_ms)
                 VALUES (?1, ?2, 'bulk', '/tmp/bulk', 'open', 1000, 1, 1)",
                rusqlite::params![tid, format!("bulk {t}")],
            )
            .unwrap();
            for j in 0..1000 {
                tx.execute(
                    "INSERT INTO events
                       (id, thread_id, seq, run_id, kind, actor, payload_version, payload, created_ms)
                     VALUES (?1, ?2, ?3, NULL, 'agent.message', 'system', 1, ?4, 1)",
                    rusqlite::params![
                        format!("{tid}-e{j}"),
                        tid,
                        j,
                        json!({"text": "synthetic"}).to_string()
                    ],
                )
                .unwrap();
            }
        }
        tx.commit().unwrap();
    }
    let started = Instant::now();
    let p = integration::preview(&store, &fx.thread, &fx.snapshot, "destino").unwrap();
    let elapsed = started.elapsed();
    assert!(p.eligible, "preview: {p:?}");
    assert_eq!(p.source_oid.as_deref(), Some(fx.source.as_str()));
    assert!(
        elapsed < Duration::from_secs(5),
        "preview demasiado lento: {elapsed:?}"
    );
    eprintln!("I14 preview latency with 100k events: {elapsed:?}");
}

// ── I15c: preview legible en 80x24 y 180x50 ────────────────────────────────

#[test]
fn i15c_preview_is_legible_at_supported_terminal_sizes() {
    for (rows, cols) in [(24u16, 80u16), (50u16, 180u16)] {
        let fx = Fx::new(&format!("p2s1-size-{rows}x{cols}"));
        fx.validate_ok();
        let mut pty = TuiPty::spawn_sized(&fx.env, &[], &[], rows, cols);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !pty.output_contains("Aeril") {
            assert!(Instant::now() < deadline, "TUI sin frame a {cols}x{rows}");
            pty.pump(100);
        }
        pty.enter();
        pty.pump(250);
        pty.send(b"i");
        pty.pump(120);
        pty.send(b"\x7f\x7f\x7f\x7f");
        pty.type_line("destino");
        pty.pump(350);
        assert!(
            pty.output_contains("Integration preview"),
            "preview ilegible a {cols}x{rows}: {}",
            pty.output_lossy()
        );
        assert!(
            pty.output_contains("fast-forward eligible"),
            "estado ilegible a {cols}x{rows}"
        );
        pty.esc();
        pty.pump(100);
        pty.kill();
    }
}

// ── I13c: reconciliation must not finalize a live owner ────────────────────

#[test]
fn i13c_reconcile_skips_live_owner_and_recovers_dead_owner() {
    let env = TestEnv::new("p2s1-i13c");
    let base = env.head_oid();
    env.update_ref("refs/heads/destino", &base);
    let live = env.thread_new("live owner");
    let dead = env.thread_new("dead owner");
    let mut store = Store::open(&env.data.join("aeril.db")).unwrap();
    let append = |store: &mut Store, thread: &str, pid: i64, start: Option<u64>| {
        events::append_typed(
            &mut store.conn,
            thread,
            None,
            events::K_INTEGRATION_REQUESTED,
            "system",
            &events::payload::IntegrationRequested {
                target_branch: "destino".into(),
                source_oid: "1".repeat(40),
                target_oid_expected: base.clone(),
                owner_pid: Some(pid),
                owner_pid_start: start,
            },
        )
        .unwrap();
    };
    let live_pid = std::process::id() as i64;
    append(
        &mut store,
        &live,
        live_pid,
        aeril::store::leases::process_start_time(live_pid),
    );
    let mut child = Command::new("true").spawn().unwrap();
    let dead_pid = child.id() as i64;
    child.wait().unwrap();
    append(&mut store, &dead, dead_pid, Some(1));
    let outcomes = session::reconcile_integrations(&mut store).unwrap();
    assert_eq!(outcomes.len(), 1, "solo el dueño muerto se reconcilia");
    assert!(matches!(
        outcomes[0],
        session::IntegrationReconcile::Abandoned { .. }
    ));
    let blocked_live =
        events::fetch_by_kind(&store.conn, &live, events::K_INTEGRATION_BLOCKED).unwrap();
    let blocked_dead =
        events::fetch_by_kind(&store.conn, &dead, events::K_INTEGRATION_BLOCKED).unwrap();
    assert!(blocked_live.is_empty(), "el dueño vivo no se finaliza");
    assert_eq!(blocked_dead.len(), 1);
}

// ── Pinned source is enforced inside the authoritative integrate path ───────

#[test]
fn i1b_pinned_source_mismatch_writes_nothing() {
    let fx = Fx::new("p2s1-i1b");
    fx.validate_ok();
    let mut store = fx.store();
    let err = session::integrate_pinned(
        &mut store,
        &fx.thread,
        &fx.snapshot,
        "destino",
        &fx.base,
        &"f".repeat(40),
    )
    .expect_err("source distinta debe fallar antes de escribir");
    assert!(err.to_string().contains("fuente"), "{err}");
    assert_eq!(fx.env.rev_parse("destino"), fx.base);
    assert_eq!(
        count_kind(&fx.env, &fx.thread, events::K_INTEGRATION_REQUESTED),
        0
    );
    assert_eq!(
        count_kind(&fx.env, &fx.thread, events::K_INTEGRATION_COMPLETED),
        0
    );
}

// ── Reconciliation never duplicates an existing completed success ───────────

#[test]
fn i13d_reconcile_does_not_duplicate_existing_completed() {
    // El ref ya apunta al source: sin dedupe, recovery agregaría un segundo
    // completed para el request muerto.
    let fx = Fx::new("p2s1-i13d");
    fx.env.update_ref("refs/heads/destino", &fx.source);
    let mut store = fx.store();
    events::append_typed(
        &mut store.conn,
        &fx.thread,
        None,
        events::K_INTEGRATION_COMPLETED,
        "system",
        &events::payload::IntegrationCompleted {
            target_branch: "destino".into(),
            source_oid: fx.source.clone(),
            target_oid_before: fx.base.clone(),
            target_oid_after: fx.source.clone(),
            recovered: false,
        },
    )
    .unwrap();
    let mut child = Command::new("true").spawn().unwrap();
    let dead_pid = child.id() as i64;
    child.wait().unwrap();
    events::append_typed(
        &mut store.conn,
        &fx.thread,
        None,
        events::K_INTEGRATION_REQUESTED,
        "system",
        &events::payload::IntegrationRequested {
            target_branch: "destino".into(),
            source_oid: fx.source.clone(),
            target_oid_expected: fx.base.clone(),
            owner_pid: Some(dead_pid),
            owner_pid_start: Some(1),
        },
    )
    .unwrap();
    let outcomes = session::reconcile_integrations(&mut store).unwrap();
    assert!(outcomes.is_empty(), "ya hay un completed: {outcomes:?}");
    assert_eq!(
        events::fetch_by_kind(&store.conn, &fx.thread, events::K_INTEGRATION_COMPLETED)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn i13e_prior_completed_does_not_suppress_blocked_outcome() {
    // El ref volvió a la base: el request muerto debe registrar blocked aunque
    // exista un completed histórico para el mismo target/source.
    let env = TestEnv::new("p2s1-i13e");
    let base = env.head_oid();
    env.update_ref("refs/heads/destino", &base);
    let thread = env.thread_new("blocked after completed");
    let source = "1".repeat(40);
    let mut store = Store::open(&env.data.join("aeril.db")).unwrap();
    events::append_typed(
        &mut store.conn,
        &thread,
        None,
        events::K_INTEGRATION_COMPLETED,
        "system",
        &events::payload::IntegrationCompleted {
            target_branch: "destino".into(),
            source_oid: source.clone(),
            target_oid_before: base.clone(),
            target_oid_after: source.clone(),
            recovered: false,
        },
    )
    .unwrap();
    let mut child = Command::new("true").spawn().unwrap();
    let dead_pid = child.id() as i64;
    child.wait().unwrap();
    events::append_typed(
        &mut store.conn,
        &thread,
        None,
        events::K_INTEGRATION_REQUESTED,
        "system",
        &events::payload::IntegrationRequested {
            target_branch: "destino".into(),
            source_oid: source.clone(),
            target_oid_expected: base.clone(),
            owner_pid: Some(dead_pid),
            owner_pid_start: Some(1),
        },
    )
    .unwrap();
    let outcomes = session::reconcile_integrations(&mut store).unwrap();
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert!(matches!(
        outcomes[0],
        session::IntegrationReconcile::Abandoned { .. }
    ));
    assert_eq!(
        events::fetch_by_kind(&store.conn, &thread, events::K_INTEGRATION_BLOCKED)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        events::fetch_by_kind(&store.conn, &thread, events::K_INTEGRATION_COMPLETED)
            .unwrap()
            .len(),
        1
    );
}
