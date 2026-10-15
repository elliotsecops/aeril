//! Regresión de la auditoría (Wave B: B1–B6 + leases + reconciliación).

mod common;
use common::*;
use serde_json::Value;
use std::process::Command;
use std::time::{Duration, Instant};

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

fn snapshot_of(run: &Value) -> String {
    run["snapshot"]["snapshot_id"].as_str().unwrap().to_string()
}

// ── B1: entrada ACP acotada ──────────────────────────────────────────────────

#[test]
fn b1_linea_gigante_se_rechaza_antes_de_acumularla() {
    let env = TestEnv::new("rc1-b1-line");
    let thread = env.thread_new("b1");
    let big = "x".repeat(10_000);
    let script = env.script(
        "big",
        serde_json::json!({"message": big, "final_message": "fin", "edits": []}),
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
        &[("AERIL_ACP_MAX_LINE_BYTES", "4096")],
    );
    let summary = parse_json(&out.stdout);
    assert_eq!(summary["state"], "failed", "summary: {summary}");
    let detail = summary["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("más de 4096 bytes") || detail.contains("rechazada"),
        "detalle: {detail}"
    );
}

#[test]
fn b1_presupuesto_del_turno_es_acumulado() {
    let env = TestEnv::new("rc1-b1-turn");
    let thread = env.thread_new("b1t");
    let chunk = "y".repeat(4_000);
    let script = env.script(
        "two",
        serde_json::json!({"message": chunk, "final_message": chunk, "edits": []}),
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
        // Cada línea entra sola (< límite), pero dos juntas exceden el turno.
        &[
            ("AERIL_ACP_MAX_LINE_BYTES", "8192"),
            ("AERIL_ACP_MAX_TURN_BYTES", "6000"),
        ],
    );
    let summary = parse_json(&out.stdout);
    assert_eq!(summary["state"], "failed", "summary: {summary}");
    assert!(
        summary["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("acumulada"),
        "detalle: {}",
        summary["detail"]
    );
}

// ── B2: memoria de revisión acotada ──────────────────────────────────────────

#[test]
fn b2_patch_demasiado_grande_se_rechaza_explicitamente() {
    let env = TestEnv::new("rc1-b2-patch");
    let thread = env.thread_new("b2");
    let big_content = format!("# grande\n\n{}\n", "línea larga de contenido. ".repeat(80));
    let script = env.script("big", mock_edit("NOTES.md", &big_content, "x"));
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
        &[("AERIL_MAX_PATCH_BYTES", "200")],
    );
    let summary = parse_json(&out.stdout);
    assert_eq!(summary["state"], "failed", "summary: {summary}");
    assert!(
        summary["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("patch revisado"),
        "detalle: {}",
        summary["detail"]
    );
    assert!(env.checkpoint_list(&thread).is_empty(), "nada publicado");
}

#[test]
fn b2_comentarios_paginados_sin_cargar_patches() {
    let env = TestEnv::new("rc1-b2-comments");
    let thread = env.thread_new("b2c");
    let script = env.script("s", mock_edit("NOTES.md", "# v2\n\ncomentable\n", "x"));
    let run = env.run_start(&thread, "x", &script);
    let sid = snapshot_of(&run);
    let mut ids = Vec::new();
    for i in 0..5 {
        let c = env.aeril_ok(&[
            "comment",
            "add",
            "--thread",
            &thread,
            "--snapshot",
            &sid,
            "--path",
            "NOTES.md",
            "--start",
            "3",
            "--end",
            "3",
            "--text",
            &format!("nota {i}"),
        ]);
        ids.push(c["comment_id"].as_str().unwrap().to_string());
    }
    let page1 = env.aeril_ok(&["comment", "list", "--thread", &thread, "--limit", "2"]);
    assert_eq!(page1.as_array().unwrap().len(), 2);
    assert_eq!(page1[0]["comment_id"], ids[0].as_str());
    // Resumen liviano: no incluye el patch original.
    assert!(page1[0].get("original_patch").is_none());
    let page2 = env.aeril_ok(&[
        "comment", "list", "--thread", &thread, "--limit", "2", "--offset", "2",
    ]);
    assert_eq!(page2[0]["comment_id"], ids[2].as_str());
    let empty = env.aeril_ok(&[
        "comment", "list", "--thread", &thread, "--limit", "2", "--offset", "10",
    ]);
    assert!(empty.as_array().unwrap().is_empty());
}

// ── B3: carreras de publicación y recuperación ───────────────────────────────

#[test]
fn b3_creacion_concurrente_del_mismo_oid_es_idempotente() {
    let env = TestEnv::new("rc1-b3-same");
    let repo = aeril::git::open(&env.repo).unwrap();
    let base = env.head_oid();
    let source = {
        let tree = env.rev_parse(&format!("{base}^{{tree}}"));
        git_out(
            &env.repo,
            &["commit-tree", &tree, "-p", &base, "-m", "origen"],
        )
    };
    let refname = "refs/aeril/checkpoints/race-same";
    let repo_path = env.repo.clone();
    let source_clone = source.clone();
    let hook = move || {
        // Otro proceso crea la MISMA ref con el MISMO OID justo antes.
        git(&repo_path, &["update-ref", refname, &source_clone]);
    };
    let outcome =
        aeril::git::snapshot::publish_ref_cas_with_hook(&repo, refname, &source, Some(&hook))
            .expect("mismo OID ⇒ éxito idempotente");
    assert_eq!(outcome, aeril::git::snapshot::PublishOutcome::AlreadyExists);
    assert_eq!(env.rev_parse(refname), source);
}

#[test]
fn b3_creacion_concurrente_de_otro_oid_es_conflicto() {
    let env = TestEnv::new("rc1-b3-conflict");
    let repo = aeril::git::open(&env.repo).unwrap();
    let base = env.head_oid();
    let source = {
        let tree = env.rev_parse(&format!("{base}^{{tree}}"));
        git_out(
            &env.repo,
            &["commit-tree", &tree, "-p", &base, "-m", "origen"],
        )
    };
    let other = {
        let tree = env.rev_parse("HEAD^{tree}");
        git_out(
            &env.repo,
            &["commit-tree", &tree, "-p", &base, "-m", "ajeno"],
        )
    };
    let refname = "refs/aeril/checkpoints/race-diff";
    let repo_path = env.repo.clone();
    let other_clone = other.clone();
    let hook = move || {
        git(&repo_path, &["update-ref", refname, &other_clone]);
    };
    let err = aeril::git::snapshot::publish_ref_cas_with_hook(&repo, refname, &source, Some(&hook))
        .expect_err("otro OID ⇒ conflicto");
    assert!(err.to_string().contains("conflicto"), "{err}");
    assert_eq!(
        env.rev_parse(refname),
        other,
        "la ref del otro proceso queda intacta"
    );
}

#[test]
fn b3_recuperacion_concurrente_no_duplica_ready() {
    let env = TestEnv::new("rc1-b3-recover");
    let thread = env.thread_new("b3r");
    let script = env.script("s", mock_edit("NOTES.md", "# v2\n\nrecuperable\n", "x"));
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
        &[("AERIL_CRASH_AFTER", "prepared")],
    );
    assert_eq!(signal_of(&out.status), Some(9));

    // Dos procesos de recuperación a la vez sobre el mismo store.
    let mk = |env: &TestEnv| {
        Command::new(BIN)
            .args(["recover"])
            .env("AERIL_DATA_DIR", &env.data)
            .current_dir(&env.root)
            .spawn()
            .expect("recover")
    };
    let mut a = mk(&env);
    let mut b = mk(&env);
    let a_st = a.wait().unwrap();
    let b_st = b.wait().unwrap();
    assert!(a_st.success() && b_st.success(), "recover exit");

    let ready = events_of(&env, &thread, "checkpoint.ready");
    assert_eq!(
        ready.len(),
        1,
        "un único ready pese a dos recuperadores: {ready:?}"
    );
    assert_eq!(ready[0]["payload"]["recovered"], true);

    let again = env.aeril_ok(&["recover"]);
    assert_eq!(again.as_array().unwrap().len(), 0, "nada pendiente después");
}

// ── B4: intención durable de integración ─────────────────────────────────────

#[test]
fn b4_crash_antes_del_cas_deja_intento_abandonado_no_fabricado() {
    let env = TestEnv::new("rc1-b4-intent");
    let thread = env.thread_new("b4i");
    let script = env.script("s", mock_edit("NOTES.md", "# v2\n\nintegrable\n", "x"));
    let run = env.run_start(&thread, "x", &script);
    let sid = snapshot_of(&run);
    let source_oid = run["snapshot"]["oid"].as_str().unwrap().to_string();
    let base = env.head_oid();
    git(&env.repo, &["branch", "destino", &base]);

    let out = env.aeril_env(
        &[
            "integrate",
            "--thread",
            &thread,
            "--snapshot",
            &sid,
            "--to",
            "destino",
        ],
        &[("AERIL_CRASH_AFTER", "integrate_intent")],
    );
    assert_eq!(signal_of(&out.status), Some(9), "muere tras la intención");
    assert_eq!(env.rev_parse("destino"), base, "la ref no se tocó");

    let rec = env.aeril_ok(&["recover"]);
    let entries = rec.as_array().unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e["action"] == "integration_abandoned"),
        "recover reporta el abandono: {rec}"
    );
    let blocked = events_of(&env, &thread, "integration.blocked");
    assert_eq!(blocked.len(), 1);
    assert!(
        blocked[0]["payload"]["reason"]
            .as_str()
            .unwrap()
            .contains("abandonado"),
        "razón: {}",
        blocked[0]["payload"]["reason"]
    );
    assert!(events_of(&env, &thread, "integration.completed").is_empty());

    // Reintento explícito: ahora integra de verdad.
    let ok = env.aeril_ok(&[
        "integrate",
        "--thread",
        &thread,
        "--snapshot",
        &sid,
        "--to",
        "destino",
    ]);
    assert_eq!(ok["target_oid_after"].as_str().unwrap(), source_oid);
    assert_eq!(env.rev_parse("destino"), source_oid);
}

#[test]
fn b4_crash_despues_del_cas_se_reconcilia_con_evidencia() {
    let env = TestEnv::new("rc1-b4-after");
    let thread = env.thread_new("b4a");
    let script = env.script("s", mock_edit("NOTES.md", "# v2\n\nintegrada\n", "x"));
    let run = env.run_start(&thread, "x", &script);
    let sid = snapshot_of(&run);
    let source_oid = run["snapshot"]["oid"].as_str().unwrap().to_string();
    let base = env.head_oid();
    git(&env.repo, &["branch", "destino2", &base]);

    let out = env.aeril_env(
        &[
            "integrate",
            "--thread",
            &thread,
            "--snapshot",
            &sid,
            "--to",
            "destino2",
        ],
        &[("AERIL_CRASH_AFTER", "integrate_committed")],
    );
    assert_eq!(signal_of(&out.status), Some(9));
    assert_eq!(env.rev_parse("destino2"), source_oid, "la ref SÍ se movió");

    let rec = env.aeril_ok(&["recover"]);
    assert!(
        rec.as_array()
            .unwrap()
            .iter()
            .any(|e| e["action"] == "integration_completed_recovered"),
        "recover: {rec}"
    );
    let completed = events_of(&env, &thread, "integration.completed");
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0]["payload"]["recovered"], true);

    // Idempotente: una segunda recuperación no agrega nada.
    let rec2 = env.aeril_ok(&["recover"]);
    assert!(
        !rec2
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["action"].as_str().unwrap().contains("integration")),
        "sin pendientes: {rec2}"
    );
    assert_eq!(events_of(&env, &thread, "integration.completed").len(), 1);
}

// ── B5: leases de workspace ──────────────────────────────────────────────────

fn spawn_slow_run(env: &TestEnv, thread: &str, delay_ms: u64) -> std::process::Child {
    let script = env.script(
        "slow-lease",
        serde_json::json!({"message": "trabajando", "delay_ms": delay_ms, "edits": []}),
    );
    Command::new(BIN)
        .args([
            "run",
            "start",
            "--thread",
            thread,
            "--prompt",
            "x",
            "--backend",
            &format!("mock:{}", script.display()),
        ])
        .env("AERIL_DATA_DIR", &env.data)
        .current_dir(&env.root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn run lento")
}

fn wait_until_session(env: &TestEnv, thread: &str, timeout_s: u64) {
    let deadline = Instant::now() + Duration::from_secs(timeout_s);
    while Instant::now() < deadline {
        if !events_of(env, thread, "engine.session").is_empty() {
            return;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    panic!("el run lento no llegó a establecer sesión");
}

#[test]
fn b5_run_concurrente_y_borrado_bloqueados_por_lease() {
    let env = TestEnv::new("rc1-b5-busy");
    let thread = env.thread_new("b5");
    let mut slow = spawn_slow_run(&env, &thread, 15_000);
    wait_until_session(&env, &thread, 20);

    // Segundo run sobre el mismo workspace: rechazado mientras el primero vive.
    let script2 = env.script("s2", mock_edit("NOTES.md", "# otro\n", "x"));
    let out = env.aeril_env(
        &[
            "run",
            "start",
            "--thread",
            &thread,
            "--prompt",
            "x",
            "--backend",
            &format!("mock:{}", script2.display()),
        ],
        &[],
    );
    assert!(!out.status.success(), "el segundo run debe fallar");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("en uso"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Borrar el worktree con un run activo: bloqueado.
    let rm = env.aeril(&["worktree", "rm", "--thread", &thread]);
    assert!(!rm.status.success());
    assert!(
        String::from_utf8_lossy(&rm.stderr).contains("lease con dueño VIVO"),
        "stderr: {}",
        String::from_utf8_lossy(&rm.stderr)
    );

    // Caída abrupta: el lease queda de un PID muerto y el próximo run lo toma.
    let _ = slow.kill();
    let _ = slow.wait();
    let script3 = env.script("s3", mock_edit("NOTES.md", "# toma de control\n", "x"));
    let run = env.run_start(&thread, "x", &script3);
    assert_eq!(run["state"], "completed", "run: {run}");
    let takeover = events_of(&env, &thread, "workspace.lease_taken_over");
    assert_eq!(
        takeover.len(),
        1,
        "toma de control registrada: {takeover:?}"
    );
    assert_eq!(takeover[0]["payload"]["reason"], "pid muerto");
}

#[test]
fn b5_lease_de_dueno_muerto_se_toma_y_se_registra() {
    // RC2-B5: un TTL vencido con dueño VIVO ya NO transfiere ownership (eso se
    // prueba en tests/rc2_b5.rs). La transferencia automática queda solo para
    // dueños PROBADAMENTE muertos.
    let env = TestEnv::new("rc1-b5-dead");
    let thread = env.thread_new("b5d");
    let script = env.script("s", mock_edit("NOTES.md", "# v2\n", "x"));
    let _ = env.run_start(&thread, "x", &script);

    // PID inexistente (proceso creado y reapeado): dueño probadamente muerto.
    let mut victim = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let dead_pid = victim.id() as i64;
    victim.kill().unwrap();
    victim.wait().unwrap();
    let store = aeril::store::Store::open(&env.data.join("aeril.db")).unwrap();
    let ws: String = store
        .conn
        .query_row(
            "SELECT workspace_id FROM runs WHERE workspace_id IS NOT NULL ORDER BY started_ms DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    store
        .conn
        .execute(
            &format!(
                "INSERT INTO leases (workspace_id, run_id, pid, pid_start, acquired_ms, renewed_ms, ttl_ms)
                 VALUES ('{ws}', 'run-ajeno', {dead_pid}, 0, 0, 0, 60000)"
            ),
            [],
        )
        .unwrap();

    let script2 = env.script("s2", mock_edit("NOTES.md", "# v3\n", "x"));
    let run = env.run_start(&thread, "x", &script2);
    assert_eq!(run["state"], "completed", "run: {run}");
    let takeover = events_of(&env, &thread, "workspace.lease_taken_over");
    assert_eq!(takeover.last().unwrap()["payload"]["reason"], "pid muerto");
}

// ── B6: plazo de cancelación ─────────────────────────────────────────────────

#[test]
fn b6_backend_que_ignora_la_cancelacion_termina_por_plazo() {
    let env = TestEnv::new("rc1-b6");
    let thread = env.thread_new("b6");
    let script = env.script(
        "stubborn",
        serde_json::json!({"message": "trabajando", "delay_ms": 30000, "edits": [], "ignore_cancel": true}),
    );
    let started = Instant::now();
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
            "--cancel-after-ms",
            "500",
        ],
        &[("AERIL_CANCEL_GRACE_SECS", "2")],
    );
    let elapsed = started.elapsed();
    let summary = parse_json(&out.stdout);
    assert_eq!(summary["state"], "cancelled", "summary: {summary}");
    assert_eq!(summary["stop_reason"], "cancelled");
    assert!(
        summary["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("no confirmada"),
        "detalle: {}",
        summary["detail"]
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "el plazo debe ser fijo (~2 s), no el delay del mock: {elapsed:?}"
    );
}
