//! PA0-C: manejo contextual y verificable de integraciones bloqueadas (C1–C6).

mod common;

use aeril::integration::{self, IntegrationStatus, ReviewState, ValidationState};
use aeril::store::{Store, events};
use common::pty::TuiPty;
use common::*;
use serde_json::Value;
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
        let thread = env.thread_new("pa0-c blocked integration");
        let script = env.script(
            "s",
            mock_edit("NOTES.md", "# v2\n\ncontenido pa0-c\n", "edit"),
        );
        let run = env.run_start(&thread, "actualiza NOTES", &script);
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

    fn preview_json(&self, to: &str) -> Value {
        self.env.aeril_ok(&[
            "integrate",
            "preview",
            "--thread",
            &self.thread,
            "--snapshot",
            &self.snapshot,
            "--to",
            to,
        ])
    }

    fn preview_text(&self, to: &str) -> String {
        let out = self.env.aeril(&[
            "integrate",
            "preview",
            "--thread",
            &self.thread,
            "--snapshot",
            &self.snapshot,
            "--to",
            to,
            "--text",
        ]);
        assert!(
            out.status.success(),
            "preview --text falló: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn validate_ok(&self) {
        self.env.aeril_ok(&[
            "validate",
            "--thread",
            &self.thread,
            "--snapshot",
            &self.snapshot,
            "--",
            "true",
        ]);
    }

    fn ack(&self) {
        self.env.aeril_ok(&[
            "review",
            "ack",
            "--thread",
            &self.thread,
            "--snapshot",
            &self.snapshot,
        ]);
    }

    fn validate_and_ack(&self) {
        self.validate_ok();
        self.ack();
    }
}

fn pty_contains_wrapped(pty: &TuiPty, needle: &str) -> bool {
    let is_border = |c: char| matches!(c, '│' | '─' | '╭' | '╮' | '╰' | '╯' | '┃');
    let flat: String = pty
        .output_flat()
        .chars()
        .filter(|c| !is_border(*c))
        .collect();
    let want: String = needle
        .chars()
        .filter(|c| !c.is_whitespace() && !is_border(*c))
        .collect();
    flat.contains(&want)
}

fn spawn_and_open_preview(env: &TestEnv, branch: &str, rows: u16, cols: u16) -> TuiPty {
    let mut pty = TuiPty::spawn_sized(env, &[], &[], rows, cols);
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
    pty.type_line(branch);
    let p_deadline = Instant::now() + Duration::from_secs(5);
    while !pty.output_contains("Integration preview") {
        assert!(
            Instant::now() < p_deadline,
            "preview no abrió en {cols}x{rows}: {}",
            pty.output_lossy()
        );
        pty.pump(80);
    }
    pty.pump(150);
    pty
}

fn advance_branch(env: &TestEnv, branch: &str, file: &str) -> String {
    let main = env.rev_parse("main");
    git(&env.repo, &["checkout", "-q", "-b", "tmp-advance"]);
    std::fs::write(env.repo.join(file), "cambio divergente\n").unwrap();
    git(&env.repo, &["add", "-A"]);
    git(&env.repo, &["commit", "-q", "-m", "advance target"]);
    let oid = env.rev_parse("HEAD");
    git(&env.repo, &["checkout", "-q", "main"]);
    assert_eq!(env.rev_parse("main"), main);
    env.update_ref(&format!("refs/heads/{branch}"), &oid);
    git(&env.repo, &["branch", "-D", "tmp-advance"]);
    oid
}

fn assert_no_destructive_advice(preview: &Value, extra_text: &str) {
    let cmds = preview["manual_commands"]
        .as_array()
        .expect("manual_commands array");
    for cmd in cmds {
        let s = cmd.as_str().unwrap_or_default();
        assert!(
            !s.contains("--force")
                && !s.contains("reset --hard")
                && !s.contains("clean -f")
                && !s.contains("checkout -- "),
            "comando destructivo prohibido: {s}"
        );
    }
    let notes = preview["manual_notes"]
        .as_array()
        .expect("manual_notes array");
    for n in notes {
        let s = n.as_str().unwrap_or_default();
        assert!(
            !s.contains(" --force ") || s.contains("nunca") || s.contains("No uses"),
            "nota riesgosa ambigua: {s}"
        );
    }
    assert!(
        !extra_text.contains("reset --hard") && !extra_text.contains("clean -f"),
        "texto contiene recomendación destructiva: {extra_text}"
    );
}

// ── C1: main checked out, checkout limpio ────────────────────────────────────

#[test]
fn c1_main_checked_out_clean_guides_manual_ff_and_reverify_returns_already_integrated() {
    let fx = Fx::new("pa0c-c1");
    fx.validate_and_ack();

    let repo_str = fx.env.repo.to_string_lossy().into_owned();
    let p = fx.preview_json("main");
    assert_eq!(p["status"], "target_checked_out");
    assert_eq!(p["eligible"], false);
    assert_eq!(p["checked_out_path"], repo_str.as_str());
    assert_eq!(p["target_dirty"], false);
    assert_eq!(p["source_oid"], fx.source);
    assert_eq!(p["target_oid"], fx.base);
    let cmds: Vec<&str> = p["manual_commands"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let ff_cmd = format!("git -C {repo_str} merge --ff-only {}", fx.source);
    assert!(cmds.contains(&ff_cmd.as_str()), "falta ff manual: {cmds:?}");
    let text = fx.preview_text("main");
    assert!(
        text.contains(&ff_cmd),
        "preview --text sin comando C1: {text}"
    );
    assert_no_destructive_advice(&p, &text);

    // TUI muestra estado checked out + Refresh live con [r] tras ff-only manual.
    let mut pty = spawn_and_open_preview(&fx.env, "main", 40, 120);
    assert!(pty.output_contains("target checked out"));
    assert!(pty_contains_wrapped(&pty, &fx.source));
    assert!(pty_contains_wrapped(&pty, &fx.base));
    assert!(pty_contains_wrapped(&pty, &repo_str));
    assert!(
        pty_contains_wrapped(&pty, &ff_cmd),
        "falta ff_cmd={ff_cmd:?}\nflat={}",
        pty.output_flat()
    );

    // Ejecutar exactamente el procedimiento manual indicado sobre el checkout principal.
    git(&fx.env.repo, &["merge", "--ff-only", &fx.source]);
    assert_eq!(fx.env.rev_parse("main"), fx.source);

    // En CLI, re-preview sobre `main` (todavía checked out) debe confirmar already_integrated.
    let re_p = fx.preview_json("main");
    assert_eq!(re_p["status"], "already_integrated");
    assert_eq!(re_p["eligible"], false);
    assert_eq!(re_p["target_oid"], fx.source);

    // En TUI, [r] refresca a `already integrated` sin cerrar el modal.
    pty.send(b"r");
    let r_deadline = Instant::now() + Duration::from_secs(5);
    while !pty.output_contains("already integrated") {
        assert!(
            Instant::now() < r_deadline,
            "[r] no actualizó a already integrated: {}",
            pty.output_lossy()
        );
        pty.pump(80);
    }
    pty.esc();
    pty.pump(100);
    pty.kill();
}

// ── C2: checkout sucio en target checked out ────────────────────────────────

#[test]
fn c2_dirty_checked_out_target_and_worktree_warn_safely() {
    let fx = Fx::new("pa0c-c2");
    fx.validate_and_ack();

    // Modificación sin commitear y archivo untracked en el checkout principal (`main`).
    std::fs::write(fx.env.repo.join("NOTES.md"), "# local dirty edit\n").unwrap();
    std::fs::write(fx.env.repo.join("scratch.untracked"), "wip\n").unwrap();

    let repo_str = fx.env.repo.to_string_lossy().into_owned();
    let p = fx.preview_json("main");
    assert_eq!(p["status"], "target_checked_out");
    assert_eq!(p["eligible"], false);
    assert_eq!(p["checked_out_path"], repo_str.as_str());
    assert_eq!(p["target_dirty"], true);
    let reason = p["reason"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        reason.contains("dirty checkout") || reason.contains("uncommitted"),
        "falta advertencia dirty: {reason}"
    );
    let cmds: Vec<&str> = p["manual_commands"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(cmds.iter().any(|c| c.contains(" status")), "{cmds:?}");
    assert!(cmds.iter().any(|c| c.contains(" stash")), "{cmds:?}");
    assert!(
        cmds.iter()
            .any(|c| c.contains(&format!("merge --ff-only {}", fx.source))),
        "{cmds:?}"
    );
    let text = fx.preview_text("main");
    assert_no_destructive_advice(&p, &text);

    // Verificar en TUI que se advierte DIRTY y no toca archivos locales.
    let mut pty = spawn_and_open_preview(&fx.env, "main", 40, 120);
    assert!(pty.output_contains("DIRTY"), "{}", pty.output_lossy());
    assert!(pty.output_contains(&repo_str));
    pty.send(b"y");
    pty.pump(250);
    pty.kill();
    assert_eq!(fx.env.rev_parse("main"), fx.base);
    assert_eq!(
        std::fs::read_to_string(fx.env.repo.join("NOTES.md")).unwrap(),
        "# local dirty edit\n"
    );
    assert!(fx.env.repo.join("scratch.untracked").exists());

    // Caso 2b: destino checked out sucio en un worktree secundario.
    let wt = fx.env.root.join("wt-destino");
    git(
        &fx.env.repo,
        &["worktree", "add", "-q", wt.to_str().unwrap(), "destino"],
    );
    std::fs::write(wt.join("WORKTREE_DIRTY.txt"), "local\n").unwrap();
    let wt_p = fx.preview_json("destino");
    assert_eq!(wt_p["status"], "target_checked_out");
    assert_eq!(wt_p["checked_out_path"], wt.to_string_lossy().as_ref());
    assert_eq!(wt_p["target_dirty"], true);
    git(
        &fx.env.repo,
        &["worktree", "remove", "--force", wt.to_str().unwrap()],
    );
}

// ── C3: target movido entre preview y confirmación ──────────────────────────

#[test]
fn c3_stale_target_between_preview_and_confirm_stays_blocked_with_context() {
    let fx = Fx::new("pa0c-c3");
    fx.validate_and_ack();

    // 1. CLI apply con expect obsoleto devuelve error contextual C3 sin mover refs.
    let mut pty = spawn_and_open_preview(&fx.env, "destino", 40, 120);
    assert!(pty.output_contains("fast-forward eligible"));

    let advanced = advance_branch(&fx.env, "destino", "ADVANCED.md");
    assert_ne!(advanced, fx.base);
    assert_ne!(advanced, fx.source);

    let cli_err = fx.env.aeril(&[
        "integrate",
        "apply",
        "--thread",
        &fx.thread,
        "--snapshot",
        &fx.snapshot,
        "--to",
        "destino",
        "--source",
        &fx.source,
        "--expect",
        &fx.base,
        "--confirm",
    ]);
    assert!(!cli_err.status.success(), "apply stale debe fallar");
    let stderr = String::from_utf8_lossy(&cli_err.stderr);
    assert!(
        stderr.contains(&fx.base)
            && stderr.contains(&advanced)
            && stderr.contains(&format!("log --oneline {}..{advanced}", fx.base)),
        "stderr C3 incompleto: {stderr}"
    );
    assert_eq!(fx.env.rev_parse("destino"), advanced);

    // 2. En TUI: pulsar [y] detecta stale target, mantiene modal abierto con C3 exacto y no mueve destino.
    pty.send(b"y");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !pty.output_contains("stale target") {
        assert!(
            Instant::now() < deadline,
            "TUI no mostró stale target al confirmar: {}",
            pty.output_lossy()
        );
        pty.pump(80);
    }
    assert!(pty_contains_wrapped(&pty, &fx.base), "falta expected OID");
    assert!(pty_contains_wrapped(&pty, &advanced), "falta actual OID");
    assert!(
        pty_contains_wrapped(&pty, &format!("log --oneline {}..{advanced}", fx.base)),
        "falta comando log C3; flat={}",
        pty.output_flat()
    );
    assert_eq!(fx.env.rev_parse("destino"), advanced);

    // Refrescar con [r] re-evalúa explícitamente (ahora diverged, nunca auto-aplica).
    pty.send(b"r");
    let r_deadline = Instant::now() + Duration::from_secs(5);
    while !pty.output_contains("diverged") {
        assert!(
            Instant::now() < r_deadline,
            "[r] no refrescó estado diverged: {}",
            pty.output_lossy()
        );
        pty.pump(80);
    }
    pty.kill();
    assert_eq!(fx.env.rev_parse("destino"), advanced);
}

// ── C4: divergencia en rama destino ──────────────────────────────────────────

#[test]
fn c4_diverged_target_exposes_oids_inspection_and_revalidation_requirement() {
    let fx = Fx::new("pa0c-c4");
    fx.validate_and_ack();
    let advanced = advance_branch(&fx.env, "destino", "DIV.md");

    let p = fx.preview_json("destino");
    assert_eq!(p["status"], "diverged");
    assert_eq!(p["eligible"], false);
    assert_eq!(p["source_oid"], fx.source);
    assert_eq!(p["target_oid"], advanced);
    let cmds: Vec<&str> = p["manual_commands"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        cmds.iter()
            .any(|c| c.contains(&format!("log --oneline {advanced}..{}", fx.source))),
        "{cmds:?}"
    );
    assert!(
        cmds.iter()
            .any(|c| c.contains(&format!("log --oneline {}..{advanced}", fx.source))),
        "{cmds:?}"
    );
    assert!(
        cmds.iter()
            .any(|c| c.contains("cherry-pick") || c.contains("checkout -b")),
        "{cmds:?}"
    );
    assert!(
        !cmds.iter().any(|c| c.contains("--ff-only")),
        "nunca sugerir --ff-only cuando hay divergencia: {cmds:?}"
    );
    let notes_joined = p["manual_notes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        notes_joined.contains("validación") && notes_joined.contains("review"),
        "falta advertencia de nueva validación y review: {notes_joined}"
    );
    assert_no_destructive_advice(&p, &fx.preview_text("destino"));

    // Si `main` checked out también divergió respecto del snapshot, debe informar
    // `diverged` (NO sugerir `merge --ff-only` imposible).
    std::fs::write(fx.env.repo.join("MAIN_ADV.md"), "main avanzó\n").unwrap();
    git(&fx.env.repo, &["add", "-A"]);
    git(&fx.env.repo, &["commit", "-q", "-m", "main diverged"]);
    let p_main = fx.preview_json("main");
    assert_eq!(p_main["status"], "diverged");
    assert_no_destructive_advice(&p_main, "");
}

// ── C5: precedencia estricta de gates antes de sugerencias git manuales ──────

#[test]
fn c5_validation_and_review_gates_always_precede_checked_out_manual_ff() {
    let fx = Fx::new("pa0c-c5");
    let store = fx.store();

    // (a) Sin validación ni review aunque `main` esté checked out -> ValidationRequired.
    let p1 = integration::preview(&store, &fx.thread, &fx.snapshot, "main").unwrap();
    assert_eq!(p1.status, IntegrationStatus::ValidationRequired);
    assert_eq!(p1.validation, ValidationState::Missing);
    assert!(!p1.manual_commands.iter().any(|c| c.contains("--ff-only")));

    // (b) Validación fallida aunque `main` esté checked out -> ValidationFailed.
    let fail = fx.env.aeril(&[
        "validate",
        "--thread",
        &fx.thread,
        "--snapshot",
        &fx.snapshot,
        "--",
        "false",
    ]);
    assert!(!fail.status.success());
    assert_eq!(parse_json(&fail.stdout)["exit_code"], 1);
    let p2 = integration::preview(&store, &fx.thread, &fx.snapshot, "main").unwrap();
    assert_eq!(p2.status, IntegrationStatus::ValidationFailed);
    assert!(!p2.manual_commands.iter().any(|c| c.contains("--ff-only")));

    // (c) Validación OK + comentario sin resolver -> ReviewPending.
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
        "bloqueante",
    ]);
    let p3 = integration::preview(&store, &fx.thread, &fx.snapshot, "main").unwrap();
    assert_eq!(p3.status, IntegrationStatus::ReviewPending);
    assert_eq!(p3.review, ReviewState::Pending);
    assert!(!p3.manual_commands.iter().any(|c| c.contains("--ff-only")));

    // (d) Comentario resuelto pero falta review ack -> ReviewPending (Required).
    fx.env.aeril_ok(&[
        "comment",
        "resolve",
        "--thread",
        &fx.thread,
        "--comment",
        comment["comment_id"].as_str().unwrap(),
    ]);
    let p4 = integration::preview(&store, &fx.thread, &fx.snapshot, "main").unwrap();
    assert_eq!(p4.status, IntegrationStatus::ReviewPending);
    assert_eq!(p4.review, ReviewState::Required);
    assert!(!p4.manual_commands.iter().any(|c| c.contains("--ff-only")));

    // (e) Recién después de validation PASS + review ack aparece TargetCheckedOut con C1.
    fx.ack();
    let p5 = integration::preview(&store, &fx.thread, &fx.snapshot, "main").unwrap();
    assert_eq!(p5.status, IntegrationStatus::TargetCheckedOut);
    assert!(p5.manual_commands.iter().any(|c| c.contains("--ff-only")));
}

// ── C6: legibilidad 80x24 / 120x40 / 180x50 y contrato durable intacto ───────

#[test]
fn c6_tui_legible_across_supported_sizes_and_zero_ref_movement() {
    let fx = Fx::new("pa0c-c6");
    fx.validate_and_ack();
    std::fs::write(fx.env.repo.join("NOTES.md"), "# dirty c6\n").unwrap();
    let main_before = fx.env.rev_parse("main");

    for (rows, cols) in [(24u16, 80u16), (40u16, 120u16), (50u16, 180u16)] {
        let mut pty = spawn_and_open_preview(&fx.env, "main", rows, cols);
        assert!(
            pty.output_contains("Integration preview"),
            "{cols}x{rows} sin título"
        );
        assert!(
            pty.output_contains("target checked out"),
            "{cols}x{rows} sin status"
        );
        assert!(
            pty.output_contains(&fx.source),
            "{cols}x{rows} sin source_oid completo"
        );
        assert!(
            pty.output_contains(&main_before),
            "{cols}x{rows} sin target_oid completo"
        );
        assert!(
            pty.output_contains("merge --ff-only"),
            "{cols}x{rows} sin comando ff-only"
        );
        assert!(
            pty.output_contains("[r] Refresh preview"),
            "{cols}x{rows} sin barra inferior [r]"
        );
        pty.esc();
        pty.pump(100);
        pty.kill();
    }

    assert_eq!(fx.env.rev_parse("main"), main_before);
    assert_eq!(
        events::fetch_by_kind(
            &fx.store().conn,
            &fx.thread,
            events::K_INTEGRATION_COMPLETED
        )
        .unwrap()
        .len(),
        0
    );
    let att = fx.env.aeril_ok(&["attention", "--json"]);
    assert_eq!(att.as_array().unwrap().len(), 1);
}
