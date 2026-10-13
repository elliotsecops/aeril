//! Regresión de la auditoría de confiabilidad (Wave A: A1–A6 + B7 hash).
//!
//! Cada test reproduce el fallo señalado por el audit en su forma determinista.

mod common;
use common::*;
use serde_json::Value;
use std::time::Duration;

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

fn last_engine_session(env: &TestEnv, thread: &str) -> Value {
    let evs = events_of(env, thread, "engine.session");
    evs.last().cloned().expect("hay engine.session")
}

fn snapshot_of(run: &Value) -> String {
    run["snapshot"]["snapshot_id"].as_str().unwrap().to_string()
}

fn comment_run(env: &TestEnv, thread: &str) -> String {
    let s1 = env.script(
        "s1",
        mock_edit("NOTES.md", "# v2\n\nlínea comentada\n", "uno"),
    );
    let run = env.run_start(thread, "uno", &s1);
    let sid = snapshot_of(&run);
    let c = env.aeril_ok(&[
        "comment",
        "add",
        "--thread",
        thread,
        "--snapshot",
        &sid,
        "--path",
        "NOTES.md",
        "--start",
        "3",
        "--end",
        "3",
        "--text",
        "ajustá esto",
    ]);
    c["comment_id"].as_str().unwrap().to_string()
}

// ── A2: permisos fail closed ─────────────────────────────────────────────────

#[test]
fn a2_pedido_no_persistido_no_autoriza() {
    let env = TestEnv::new("rc1-a2-requested");
    let thread = env.thread_new("a2");
    let script = env.script("s", mock_edit("NOTES.md", "# NO DEBE ESCRIBIRSE\n", "x"));
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
        &[("AERIL_FAULT_APPEND", "permission.requested")],
    );
    let summary = parse_json(&out.stdout);
    assert_eq!(
        summary["state"], "failed",
        "sin auditoría no hay run exitoso"
    );
    assert!(
        summary["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("permission.requested"),
        "el detalle explica el fallo de auditoría: {summary}"
    );
    // El archivo del WORKTREE no fue editado: el pedido no persistido no se autorizó.
    let wt = env.worktree_path(&thread);
    assert_eq!(
        std::fs::read_to_string(wt.join("NOTES.md")).unwrap(),
        "# Notas\n\nVersión original.\n"
    );
    assert!(events_of(&env, &thread, "permission.responded").is_empty());
    let audit = events_of(&env, &thread, "permission.audit_failed");
    assert_eq!(
        audit.len(),
        1,
        "un único evento de auditoría fallida: {audit:?}"
    );
    assert_eq!(audit[0]["payload"]["stage"], "requested");
}

#[test]
fn a2_decision_no_persistida_no_autoriza() {
    let env = TestEnv::new("rc1-a2-responded");
    let thread = env.thread_new("a2b");
    let script = env.script("s", mock_edit("NOTES.md", "# NO DEBE ESCRIBIRSE\n", "x"));
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
        &[("AERIL_FAULT_APPEND", "permission.responded")],
    );
    let summary = parse_json(&out.stdout);
    assert_eq!(summary["state"], "failed");
    let wt = env.worktree_path(&thread);
    assert_eq!(
        std::fs::read_to_string(wt.join("NOTES.md")).unwrap(),
        "# Notas\n\nVersión original.\n"
    );
    assert_eq!(events_of(&env, &thread, "permission.requested").len(), 1);
    assert!(events_of(&env, &thread, "permission.responded").is_empty());
    let audit = events_of(&env, &thread, "permission.audit_failed");
    assert_eq!(
        audit.len(),
        1,
        "un único evento de auditoría fallida: {audit:?}"
    );
    assert_eq!(audit[0]["payload"]["stage"], "responded");
}

#[test]
fn a2_camino_feliz_registra_decision_y_transmision() {
    let env = TestEnv::new("rc1-a2-ok");
    let thread = env.thread_new("a2c");
    let script = env.script("s", mock_edit("NOTES.md", "# escrito\n", "x"));
    let run = env.run_start(&thread, "x", &script);
    assert_eq!(run["state"], "completed");
    let wt = env.worktree_path(&thread);
    assert_eq!(
        std::fs::read_to_string(wt.join("NOTES.md")).unwrap(),
        "# escrito\n"
    );
    let transmitted = events_of(&env, &thread, "permission.transmitted");
    assert_eq!(transmitted.len(), 1);
    assert_eq!(transmitted[0]["payload"]["ok"], true);
    assert_eq!(transmitted[0]["payload"]["option_id"], "allow_once");
}

// ── A3: reanudación real de sesión ACP ───────────────────────────────────────

#[test]
fn a3_resume_usa_el_id_previo_y_lo_registra() {
    let env = TestEnv::new("rc1-a3");
    let thread = env.thread_new("a3");
    let s1 = env.script("s1", mock_edit("NOTES.md", "# v2\n", "uno"));
    let run1 = env.run_start(&thread, "uno", &s1);
    let id1 = run1["engine_session_id"].as_str().unwrap().to_string();
    let ev1 = last_engine_session(&env, &thread);
    assert_eq!(ev1["payload"]["attempted"], "new");
    assert_eq!(ev1["payload"]["accepted"], true);
    assert_eq!(ev1["payload"]["resumed"], false);

    // Segundo run con --resume: DEBE cargar la sesión previa (bug del audit:
    // registraba el id pero abría sesión nueva).
    let s2 = env.script("s2", mock_edit("NOTES.md", "# v3\n", "dos"));
    let run2 = env.aeril_ok(&[
        "run",
        "start",
        "--thread",
        &thread,
        "--prompt",
        "dos",
        "--backend",
        &format!("mock:{}", s2.display()),
        "--resume",
    ]);
    assert_eq!(run2["state"], "completed", "run2: {run2}");
    assert_eq!(
        run2["engine_session_id"].as_str().unwrap(),
        id1,
        "la sesión reanudada es la MISMA identidad"
    );
    let ev2 = last_engine_session(&env, &thread);
    assert_eq!(ev2["payload"]["attempted"], "load");
    assert_eq!(ev2["payload"]["accepted"], true);
    assert_eq!(ev2["payload"]["resumed"], true);
    assert_eq!(
        ev2["payload"]["verified"], false,
        "acceso ≠ restauración verificada"
    );
}

#[test]
fn a3_resume_con_load_fallido_cae_a_sesion_nueva_y_lo_registra() {
    let env = TestEnv::new("rc1-a3-fallback");
    let thread = env.thread_new("a3f");
    let s1 = env.script("s1", mock_edit("NOTES.md", "# v2\n", "uno"));
    let run1 = env.run_start(&thread, "uno", &s1);
    let id1 = run1["engine_session_id"].as_str().unwrap().to_string();

    let s2 = env.script(
        "s2",
        serde_json::json!({"fail_load": true, "message": "nueva sesión", "final_message": "ok", "edits": []}),
    );
    let run2 = env.aeril_ok(&[
        "run",
        "start",
        "--thread",
        &thread,
        "--prompt",
        "dos",
        "--backend",
        &format!("mock:{}", s2.display()),
        "--resume",
    ]);
    let id2 = run2["engine_session_id"].as_str().unwrap().to_string();
    assert_ne!(id2, id1, "el load falló: se usó una sesión nueva");
    let ev = last_engine_session(&env, &thread);
    assert_eq!(ev["payload"]["attempted"], "load");
    assert_eq!(ev["payload"]["accepted"], false);
    assert_eq!(ev["payload"]["resumed"], false);
}

// ── A4: semántica de entrega de feedback ─────────────────────────────────────

#[test]
fn a4_updates_de_setup_no_entregan_feedback() {
    let env = TestEnv::new("rc1-a4-setup");
    let thread = env.thread_new("a4");
    let cid = comment_run(&env, &thread);
    // Turno con SOLO notificaciones de setup y luego cancelado: no hay
    // evidencia de procesamiento → jamás "delivered".
    let script = env.script(
        "slow",
        serde_json::json!({"notify_setup": true, "delay_ms": 5000, "edits": []}),
    );
    let summary = env.aeril_ok(&[
        "feedback",
        "send",
        "--thread",
        &thread,
        "--backend",
        &format!("mock:{}", script.display()),
        "--cancel-after-ms",
        "800",
    ]);
    assert_eq!(summary["state"], "cancelled", "summary: {summary}");
    assert!(events_of(&env, &thread, "feedback.delivered").is_empty());
    assert_eq!(events_of(&env, &thread, "feedback.failed").len(), 1);
    let views = env.aeril_ok(&["comment", "list", "--thread", &thread]);
    let view = views
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["comment_id"] == cid.as_str())
        .unwrap();
    assert_eq!(view["state"], "sent");
}

#[test]
fn a4_conexion_perdida_no_entrega_feedback() {
    let env = TestEnv::new("rc1-a4-lost");
    let thread = env.thread_new("a4l");
    let cid = comment_run(&env, &thread);
    let script = env.script(
        "die",
        serde_json::json!({"notify_setup": true, "exit_after_ms": 1200, "edits": []}),
    );
    let summary = env.aeril_ok(&[
        "feedback",
        "send",
        "--thread",
        &thread,
        "--backend",
        &format!("mock:{}", script.display()),
    ]);
    assert_eq!(summary["state"], "failed", "summary: {summary}");
    assert!(events_of(&env, &thread, "feedback.delivered").is_empty());
    assert_eq!(events_of(&env, &thread, "feedback.failed").len(), 1);
    let views = env.aeril_ok(&["comment", "list", "--thread", &thread]);
    assert_eq!(
        views
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["comment_id"] == cid.as_str())
            .unwrap()["state"],
        "sent"
    );
}

#[test]
fn a4_persistencia_de_delivered_fallida_no_marca_entrega() {
    let env = TestEnv::new("rc1-a4-persist");
    let thread = env.thread_new("a4p");
    let cid = comment_run(&env, &thread);
    let script = env.script("s", mock_edit("NOTES.md", "# aplicado\n", "aplico"));
    let out = env.aeril_env(
        &[
            "feedback",
            "send",
            "--thread",
            &thread,
            "--backend",
            &format!("mock:{}", script.display()),
        ],
        &[("AERIL_FAULT_APPEND", "feedback.delivered")],
    );
    let summary = parse_json(&out.stdout);
    assert_eq!(summary["state"], "failed");
    assert!(events_of(&env, &thread, "feedback.delivered").is_empty());
    let views = env.aeril_ok(&["comment", "list", "--thread", &thread]);
    assert_eq!(
        views
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["comment_id"] == cid.as_str())
            .unwrap()["state"],
        "sent"
    );
}

#[test]
fn a4_reintento_tras_fallo_entrega_de_verdad() {
    let env = TestEnv::new("rc1-a4-retry");
    let thread = env.thread_new("a4r");
    let cid = comment_run(&env, &thread);
    // 1er intento: cancelado sin evidencia → failed.
    let slow = env.script(
        "slow",
        serde_json::json!({"notify_setup": true, "delay_ms": 5000, "edits": []}),
    );
    let first = env.aeril_ok(&[
        "feedback",
        "send",
        "--thread",
        &thread,
        "--backend",
        &format!("mock:{}", slow.display()),
        "--cancel-after-ms",
        "700",
    ]);
    assert_eq!(first["state"], "cancelled");
    // 2º intento: el mismo comentario sigue abierto y ahora sí se entrega.
    let fast = env.script(
        "fast",
        mock_edit("NOTES.md", "# aplicado de verdad\n", "aplico"),
    );
    let second = env.aeril_ok(&[
        "feedback",
        "send",
        "--thread",
        &thread,
        "--backend",
        &format!("mock:{}", fast.display()),
    ]);
    assert_eq!(second["state"], "completed", "second: {second}");
    assert_eq!(events_of(&env, &thread, "feedback.delivered").len(), 1);
    let views = env.aeril_ok(&["comment", "list", "--thread", &thread]);
    assert_eq!(
        views
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["comment_id"] == cid.as_str())
            .unwrap()["state"],
        "delivered"
    );
}

// ── A5: subconjuntos no soportados ───────────────────────────────────────────

fn add_submodule(env: &TestEnv) -> std::path::PathBuf {
    let sub = env.root.join("suborig");
    std::fs::create_dir_all(&sub).unwrap();
    git(&sub, &["init", "-q", "-b", "main"]);
    git(&sub, &["config", "user.email", "t@t"]);
    git(&sub, &["config", "user.name", "t"]);
    std::fs::write(sub.join("s.txt"), "sub v1\n").unwrap();
    git(&sub, &["add", "-A"]);
    git(&sub, &["commit", "-qm", "sub base"]);
    git(
        &env.repo,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            sub.to_str().unwrap(),
            "sub",
        ],
    );
    git(&env.repo, &["commit", "-qm", "add submodule"]);
    sub
}

#[test]
fn a5_submodulo_modificado_se_rechaza_explicitamente() {
    let env = TestEnv::new("rc1-a5-sub");
    add_submodule(&env);
    let thread = env.thread_new("a5");

    // Run 1: crea el workspace y captura S1 con un cambio normal.
    let s1 = env.script("s1", mock_edit("NOTES.md", "# v2\n\nnotas\n", "uno"));
    let run1 = env.run_start(&thread, "uno", &s1);
    assert_eq!(run1["state"], "completed");
    let ready_before = env.checkpoint_list(&thread).len();

    // El submodule avanza dentro del worktree del agente (como haría el agente).
    let wt = env.worktree_path(&thread);
    git(
        &wt,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
            "sub",
        ],
    );
    std::fs::write(wt.join("sub/s.txt"), "sub v2\n").unwrap();
    git(&wt.join("sub"), &["add", "-A"]);
    git(&wt.join("sub"), &["commit", "-qm", "avance del sub"]);

    // Run 2 (sin ediciones): SOLO el submodule cambió → rechazo explícito,
    // nunca Ok(None) silencioso ni checkpoint del submodule.
    let s2 = env.script(
        "s2",
        serde_json::json!({"message": "sin cambios", "final_message": "nada", "edits": []}),
    );
    let run2 = env.run_start(&thread, "dos", &s2);
    assert_eq!(run2["state"], "failed", "run2: {run2}");
    assert!(
        run2["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("no soportado"),
        "detalle: {}",
        run2["detail"]
    );
    assert_eq!(
        env.checkpoint_list(&thread).len(),
        ready_before,
        "no se publicó ningún checkpoint del submodule"
    );

    // Run 3 con edición normal + submodule: también rechazado (jamás parcial).
    let s3 = env.script("s3", mock_edit("NOTES.md", "# v3\n\nmás notas\n", "tres"));
    let run3 = env.run_start(&thread, "tres", &s3);
    assert_eq!(run3["state"], "failed");
    assert_eq!(env.checkpoint_list(&thread).len(), ready_before);
}

#[test]
fn a5_estados_soportados_se_capturan_y_los_ignorados_no() {
    let env = TestEnv::new("rc1-a5-states");
    // Base extra: .gitignore, EXTRA.md y REN.md versionados.
    std::fs::write(env.repo.join(".gitignore"), "ignorado.txt\n").unwrap();
    std::fs::write(env.repo.join("EXTRA.md"), "extra\n").unwrap();
    std::fs::write(env.repo.join("REN.md"), "ren\n").unwrap();
    git(&env.repo, &["add", "-A"]);
    git(&env.repo, &["commit", "-qm", "base extra"]);

    let thread = env.thread_new("a5s");
    // Run 1: crea el workspace con el checkout base.
    let s1 = env.script("s1", mock_edit("NOTES.md", "# v2 estados\n", "uno"));
    let run1 = env.run_start(&thread, "uno", &s1);
    assert_eq!(run1["state"], "completed");

    // Dentro del WORKTREE (lo que ve el agente): ignorado, untracked, borrado
    // y renombrado, todos por fuera de git.
    let wt = env.worktree_path(&thread);
    std::fs::write(wt.join("ignorado.txt"), "no entra\n").unwrap();
    std::fs::write(wt.join("untracked.txt"), "sí entra\n").unwrap();
    std::fs::remove_file(wt.join("EXTRA.md")).unwrap();
    std::fs::rename(wt.join("REN.md"), wt.join("REN2.md")).unwrap();

    // Run 2 (reutiliza el workspace): la captura debe reflejar todo lo soportado.
    let script = env.script("s", mock_edit("NOTES.md", "# v3 estados\n", "dos"));
    let run = env.run_start(&thread, "dos", &script);
    assert_eq!(run["state"], "completed", "run: {run}");
    let sid = snapshot_of(&run);
    let show = env.aeril_ok(&[
        "checkpoint",
        "show",
        "--thread",
        &thread,
        "--snapshot",
        &sid,
    ]);
    let files: Vec<(String, String)> = show["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["path"].as_str().unwrap().to_string(),
                f["status"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let has = |p: &str, s: &str| files.iter().any(|(fp, fs)| fp == p && fs == s);
    assert!(has("NOTES.md", "modified"));
    assert!(has("untracked.txt", "added"), "{files:?}");
    assert!(has("EXTRA.md", "deleted"), "{files:?}");
    assert!(has("REN.md", "deleted"), "{files:?}");
    assert!(has("REN2.md", "added"), "{files:?}");
    assert!(
        !files.iter().any(|(p, _)| p == "ignorado.txt"),
        "los ignorados no entran al snapshot"
    );
}

// ── A6: validación en checkout aislado ───────────────────────────────────────

#[test]
fn a6_validacion_usa_checkout_aislado_del_snapshot() {
    let env = TestEnv::new("rc1-a6");
    let thread = env.thread_new("a6");
    let script = env.script(
        "s",
        mock_edit("NOTES.md", "# v2 validable\n\ncontenido\n", "x"),
    );
    let run = env.run_start(&thread, "x", &script);
    let sid = snapshot_of(&run);

    // El worktree del agente queda "sucio": la validación no debe usarlo.
    let wt = env.worktree_path(&thread);
    std::fs::write(wt.join("NOTES.md"), "# basura posterior\n").unwrap();

    let v = env.aeril_ok(&[
        "validate",
        "--thread",
        &thread,
        "--snapshot",
        &sid,
        "--",
        "cat",
        "NOTES.md",
    ]);
    assert_eq!(v["exit_code"], 0);
    assert!(
        v["output_tail"]
            .as_str()
            .unwrap()
            .contains("# v2 validable")
    );
    assert_eq!(v["tree_before_ok"], true);
    assert_eq!(v["tree_after_unchanged"], true);
    // El worktree del agente no fue tocado.
    assert_eq!(env.file("NOTES.md"), "# Notas\n\nVersión original.\n");
    assert_eq!(
        std::fs::read_to_string(wt.join("NOTES.md")).unwrap(),
        "# basura posterior\n"
    );

    // Incluso con el worktree BORRADO, la validación sigue siendo posible.
    env.aeril_ok(&["worktree", "rm", "--thread", &thread]);
    let v2 = env.aeril_ok(&[
        "validate",
        "--thread",
        &thread,
        "--snapshot",
        &sid,
        "--",
        "cat",
        "NOTES.md",
    ]);
    assert_eq!(v2["exit_code"], 0);
    assert!(
        v2["output_tail"]
            .as_str()
            .unwrap()
            .contains("# v2 validable")
    );
}

#[test]
fn a6_modificacion_concurrente_del_worktree_no_afecta_la_validacion() {
    let env = TestEnv::new("rc1-a6-race");
    let thread = env.thread_new("a6r");
    let script = env.script(
        "s",
        mock_edit("NOTES.md", "# v2 exacta\n\ncontenido\n", "x"),
    );
    let run = env.run_start(&thread, "x", &script);
    let sid = snapshot_of(&run);
    let wt = env.worktree_path(&thread);

    // Validación con comando lento; mientras corre, el agente "sigue editando".
    let bin = BIN.to_string();
    let data = env.data.clone();
    let root = env.root.clone();
    let thread2 = thread.clone();
    let sid2 = sid.clone();
    let handle = std::thread::spawn(move || {
        std::process::Command::new(bin)
            .args([
                "validate",
                "--thread",
                &thread2,
                "--snapshot",
                &sid2,
                "--",
                "sh",
                "-c",
                "sleep 1; cat NOTES.md",
            ])
            .env("AERIL_DATA_DIR", &data)
            .current_dir(&root)
            .output()
            .expect("validate")
    });
    std::thread::sleep(Duration::from_millis(350));
    std::fs::write(wt.join("NOTES.md"), "# CAMBIO DURANTE LA VALIDACIÓN\n").unwrap();
    let out = handle.join().unwrap();
    assert!(
        out.status.success(),
        "validate: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = parse_json(&out.stdout);
    assert_eq!(v["exit_code"], 0);
    assert!(
        v["output_tail"].as_str().unwrap().contains("# v2 exacta"),
        "la validación usa el snapshot exacto, no el worktree en movimiento: {}",
        v["output_tail"]
    );
}

// ── A1: CAS de integración ───────────────────────────────────────────────────

fn make_commit(env: &TestEnv, parent: &str, msg: &str) -> String {
    let tree = env.rev_parse(&format!("{parent}^{{tree}}"));
    git_out(&env.repo, &["commit-tree", &tree, "-p", parent, "-m", msg])
}

#[test]
fn a1_cas_rechaza_avance_concurrente_y_preserva_el_commit_ajeno() {
    let env = TestEnv::new("rc1-a1-race");
    let repo = aeril::git::open(&env.repo).unwrap();
    let base = env.head_oid();
    git(&env.repo, &["branch", "destino", &base]);
    let source = make_commit(&env, &base, "commit fuente");
    let competitor = make_commit(&env, &base, "avance de otro escritor");

    let repo_path = env.repo.clone();
    let competitor_clone = competitor.clone();
    let hook = move || {
        // Otro escritor avanza la rama justo antes del CAS.
        git(
            &repo_path,
            &["update-ref", "refs/heads/destino", &competitor_clone],
        );
    };
    let res = aeril::git::integrate::fast_forward_with_hook(
        &repo,
        "destino",
        &source,
        &base,
        Some(&hook),
    );
    match res {
        Err(aeril::git::integrate::IntegrationBlocked::Stale { expected, actual }) => {
            assert_eq!(expected, base);
            assert_eq!(
                actual, competitor,
                "el error reporta el OID real del competidor"
            );
        }
        other => panic!("se esperaba Stale, fue: {other:?}"),
    }
    assert_eq!(
        env.rev_parse("refs/heads/destino"),
        competitor,
        "el commit del escritor concurrente queda intacto"
    );
}

#[test]
fn a1_cas_idempotente_si_otro_ya_integró_lo_mismo() {
    let env = TestEnv::new("rc1-a1-idem");
    let repo = aeril::git::open(&env.repo).unwrap();
    let base = env.head_oid();
    git(&env.repo, &["branch", "destino", &base]);
    let source = make_commit(&env, &base, "commit fuente");

    let repo_path = env.repo.clone();
    let source_clone = source.clone();
    let hook = move || {
        git(
            &repo_path,
            &["update-ref", "refs/heads/destino", &source_clone],
        );
    };
    let res = aeril::git::integrate::fast_forward_with_hook(
        &repo,
        "destino",
        &source,
        &base,
        Some(&hook),
    )
    .expect("idempotente: otro escritor ya integró lo mismo");
    assert_eq!(res.target_oid_before, base);
    assert_eq!(res.target_oid_after, source);
    assert_eq!(env.rev_parse("refs/heads/destino"), source);
}

// ── B7: hash completo de salida de validación ────────────────────────────────

#[test]
fn b7_hash_cubre_todos_los_bytes_no_solo_la_cola() {
    let env = TestEnv::new("rc1-b7");
    let thread = env.thread_new("b7");
    let script = env.script("s", mock_edit("NOTES.md", "# v2 hash\n", "x"));
    let run = env.run_start(&thread, "x", &script);
    let sid = snapshot_of(&run);

    // 2 MiB exactos de 'x' por stdout (muy por encima de la cola retenida).
    let v = env.aeril_ok(&[
        "validate",
        "--thread",
        &thread,
        "--snapshot",
        &sid,
        "--",
        "sh",
        "-c",
        "dd if=/dev/zero bs=1048576 count=2 2>/dev/null | tr '\\0' 'x'",
    ]);
    assert_eq!(v["exit_code"], 0);
    assert_eq!(v["stdout_bytes"], 2_097_152i64);
    let expected = aeril::util::fnv1a_hex(&vec![b'x'; 2_097_152]);
    assert_eq!(v["stdout_hash"], expected, "hash sobre TODOS los bytes");
    assert!(
        v["output_tail"].as_str().unwrap().len() < 9000,
        "la cola en memoria queda acotada"
    );
    let stderr_hash = v["stderr_hash"].as_str().unwrap();
    assert_eq!(v["stderr_bytes"], 0);
    assert_eq!(
        v["output_hash"],
        aeril::util::fnv1a_hex(format!("{}:{stderr_hash}", expected).as_bytes())
    );
}
