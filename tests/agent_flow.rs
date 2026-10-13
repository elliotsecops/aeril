//! Flujo del backend por ACP con el mock determinista: eventos normalizados,
//! permisos respondidos por optionId, sesión del motor persistida y entrega
//! de feedback (sent ≠ delivered; sin ack no se declara entregado).

mod common;
use common::*;

#[test]
fn flujo_agente_registra_eventos_normalizados_y_permisos() {
    let env = TestEnv::new("agent-flow");
    let thread = env.thread_new("flujo del agente");
    let script = env.script(
        "s",
        mock_edit("NOTES.md", "# v2 flujo\n\ncontenido\n", "voy a editar"),
    );
    let run = env.run_start(&thread, "Editá NOTES.md", &script);
    assert_eq!(run["state"], "completed");
    assert!(run["engine_session_id"].as_str().is_some());

    let out = env.aeril(&["export", "--thread", &thread, "--format", "jsonl"]);
    let text = String::from_utf8_lossy(&out.stdout);
    for kind in [
        "engine.session",
        "agent.message",
        "agent.tool_call",
        "permission.requested",
        "permission.responded",
        "checkpoint.prepared",
        "checkpoint.ready",
        "run.ended",
    ] {
        assert!(text.contains(kind), "falta el evento {kind}:\n{text}");
    }
    assert!(text.contains("\"option_id\":\"allow_once\""));
    assert!(text.contains("\"policy\":\"auto_allow_once\""));
    assert!(text.contains("\"stop_reason\":\"end_turn\""));

    // La sesión del motor quedó persistida en el run (identidad durable).
    let run_line = text
        .lines()
        .find(|l| l.contains("\"type\":\"run\""))
        .expect("línea de run");
    let run_json: serde_json::Value = serde_json::from_str(run_line).unwrap();
    assert_eq!(run_json["engine_session_id"], run["engine_session_id"]);
}

#[test]
fn run_sin_cambios_no_publica_checkpoint() {
    let env = TestEnv::new("agent-nochanges");
    let thread = env.thread_new("sin cambios");
    let script = env.script(
        "s",
        serde_json::json!({"message": "solo charla", "final_message": "no había nada que hacer", "edits": []}),
    );
    let run = env.run_start(&thread, "no cambies nada", &script);
    assert_eq!(run["state"], "completed");
    assert!(
        run["snapshot"].is_null(),
        "sin cambios no hay checkpoint: {run}"
    );
    assert!(
        env.checkpoint_list(&thread).is_empty(),
        "no debe existir ni prepared ni ready"
    );
}

#[test]
fn feedback_sin_backend_sano_no_se_declara_entregado() {
    let env = TestEnv::new("agent-feedback-fail");
    let thread = env.thread_new("feedback fallido");
    let s1 = env.script(
        "s1",
        mock_edit("NOTES.md", "# v2 feedback\n\nlínea tres\n", "uno"),
    );
    let run = env.run_start(&thread, "uno", &s1);
    let sid = run["snapshot"]["snapshot_id"].as_str().unwrap().to_string();
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
        "ajustá esto",
    ]);
    let cid = c["comment_id"].as_str().unwrap().to_string();

    // Backend mock con script inexistente: el proceso muere al leer el guion.
    let summary = env.aeril_ok(&[
        "feedback",
        "send",
        "--thread",
        &thread,
        "--backend",
        "mock:/ruta/que/no/existe.json",
    ]);
    assert_eq!(summary["state"], "failed", "summary: {summary}");

    // El comentario NO figura entregado; el fallo quedó registrado.
    let views = env.aeril_ok(&["comment", "list", "--thread", &thread]);
    let view = views
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["comment_id"] == cid.as_str())
        .unwrap();
    assert_eq!(view["state"], "sent", "sin ack del motor no hay delivered");

    let out = env.aeril(&["export", "--thread", &thread, "--format", "jsonl"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("feedback.sent"));
    assert!(text.contains("feedback.failed"));
    assert!(
        !text.contains("feedback.delivered"),
        "no debe existir delivered sin evidencia"
    );
}

#[test]
fn feedback_entregado_con_ack_y_nuevo_checkpoint() {
    let env = TestEnv::new("agent-feedback-ok");
    let thread = env.thread_new("feedback ok");
    let s1 = env.script(
        "s1",
        mock_edit("NOTES.md", "# v2\n\nprimera edición\n", "uno"),
    );
    let run = env.run_start(&thread, "uno", &s1);
    let sid = run["snapshot"]["snapshot_id"].as_str().unwrap().to_string();
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
        "aclará la tercera línea",
    ]);
    let cid = c["comment_id"].as_str().unwrap().to_string();

    let s2 = env.script(
        "s2",
        mock_edit("NOTES.md", "# v2\n\nprimera edición aclarada\n", "aplico"),
    );
    let summary = env.aeril_ok(&[
        "feedback",
        "send",
        "--thread",
        &thread,
        "--backend",
        &format!("mock:{}", s2.display()),
    ]);
    assert_eq!(summary["state"], "completed", "summary: {summary}");
    assert!(
        summary["snapshot"].is_object(),
        "el feedback produjo otro checkpoint"
    );

    let views = env.aeril_ok(&["comment", "list", "--thread", &thread]);
    let view = views
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["comment_id"] == cid.as_str())
        .unwrap();
    assert_eq!(view["state"], "delivered");

    // Resolver el comentario no desprotege el snapshot (sigue ready).
    env.aeril_ok(&["comment", "resolve", "--thread", &thread, "--comment", &cid]);
    let ready = env.ready_snapshots(&thread);
    assert!(ready.iter().any(|(id, _)| id == &sid));
    let views = env.aeril_ok(&["comment", "list", "--thread", &thread]);
    assert_eq!(views.as_array().unwrap()[0]["state"], "resolved");
}
