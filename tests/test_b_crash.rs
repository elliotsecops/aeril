//! Prueba B del gate (HANDOFF §9) + pruebas obligatorias de fallo:
//! caídas inyectadas en cada frontera del protocolo de checkpoint.
//!
//! Ninguna recuperación puede fabricar éxito: si faltan objetos, se reporta.
//! Y jamás se reconstruye el snapshot desde el working tree actual.

mod common;
use common::*;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn crash_run(env: &TestEnv, point: &str) -> String {
    let thread = env.thread_new("crash");
    let script = env.script(
        &format!("s-{point}"),
        mock_edit(
            "NOTES.md",
            "# Notas v2\n\ncontenido del crash\n",
            "editando",
        ),
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
        &[("AERIL_CRASH_AFTER", point)],
    );
    assert!(!out.status.success(), "el run debe morir");
    assert_eq!(
        signal_of(&out.status),
        Some(9),
        "punto {point}: muerte abrupta por SIGKILL (no un error controlado)"
    );
    thread
}

#[test]
fn crash_en_cada_frontera_se_recupera_o_reporta() {
    for point in ["objects", "prepared", "ref", "verify"] {
        let env = TestEnv::new(&format!("crash-{point}"));
        let thread = crash_run(&env, point);

        let list = env.checkpoint_list(&thread);
        match point {
            "objects" => {
                // La caída fue antes de persistir la intención: no hay nada
                // que recuperar, y los objetos huérfanos no bloquean nada.
                assert!(list.is_empty(), "sin prepared todavía: {list:?}");
                // S0: el run cuyo supervisor murió se reconcilia (failed/
                // interrumpido) de forma durable; el checkpoint sigue sin
                // existir (nada preparado que recuperar).
                let rec = env.aeril_ok(&["recover"]);
                let arr = rec.as_array().unwrap();
                assert!(
                    !arr.iter().any(|e| e["action"] == "completed_ready"),
                    "nada que recuperar: {rec}"
                );
                assert_eq!(
                    arr.iter()
                        .filter(|e| e["action"] == "run_interrupted")
                        .count(),
                    1,
                    "el run interrumpido se reconcilia: {rec}"
                );
                let post = env.script(
                    "post",
                    mock_edit("NOTES.md", "# Notas v2\n\nrun posterior\n", "post"),
                );
                let run = env.run_start(&thread, "posterior", &post);
                assert_eq!(run["state"], "completed", "un run nuevo debe funcionar");
            }
            _ => {
                assert_eq!(list.len(), 1, "hay un prepared pendiente: {list:?}");
                assert_eq!(list[0]["state"], "prepared");
                let sid = list[0]["snapshot_id"].as_str().unwrap().to_string();
                let expected_oid = list[0]["expected_oid"].as_str().unwrap().to_string();

                // Comentario antes de readiness: debe fallar (solo tras ready).
                let c = env.aeril(&[
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
                    "todavía no",
                ]);
                assert!(!c.status.success(), "no se comenta antes de ready");
                assert!(
                    String::from_utf8_lossy(&c.stderr).contains("no listo"),
                    "el error debe ser explícito: {}",
                    String::from_utf8_lossy(&c.stderr)
                );

                // Ensuciar el working tree: la recuperación NO puede usar esto.
                let wt = env.worktree_path(&thread);
                std::fs::write(wt.join("NOTES.md"), "# CONTENIDO ACTUAL NO REVISADO\n").unwrap();

                let rec = env.aeril_ok(&["recover"]);
                let arr = rec.as_array().unwrap();
                // S0: coexisten la reconciliación del run (supervisor muerto)
                // y la recuperación del checkpoint (protocolo Phase 0).
                assert_eq!(
                    arr.iter()
                        .filter(|e| e["action"] == "completed_ready")
                        .count(),
                    1,
                    "un checkpoint pendiente recuperado: {rec}"
                );
                let completed = arr
                    .iter()
                    .find(|e| e["action"] == "completed_ready")
                    .unwrap();
                assert_eq!(
                    completed["ref_recreated"],
                    point == "prepared",
                    "la ref se re-crea solo cuando aún no existía"
                );
                assert_eq!(
                    arr.iter()
                        .filter(|e| e["action"] == "run_interrupted")
                        .count(),
                    1,
                    "el run interrumpido se reconcilia: {rec}"
                );

                // ready con recovered=true y el contenido de los OBJETOS.
                let list2 = env.checkpoint_list(&thread);
                let ready = list2
                    .iter()
                    .find(|c| c["snapshot_id"] == sid.as_str())
                    .expect("checkpoint listado");
                assert_eq!(ready["state"], "ready");
                assert_eq!(ready["recovered"], true);
                let show = env.aeril_ok(&[
                    "checkpoint",
                    "show",
                    "--thread",
                    &thread,
                    "--snapshot",
                    &sid,
                ]);
                assert_eq!(show["oid"].as_str().unwrap(), expected_oid);
                let patch = show["patch"].as_str().unwrap();
                assert!(
                    patch.contains("contenido del crash"),
                    "el patch debe venir de los objetos originales: {patch}"
                );
                assert!(
                    !patch.contains("NO REVISADO"),
                    "jamás se reconstruye desde el working tree actual"
                );

                // Idempotencia: repetir la recuperación no duplica ni cambia.
                let rec2 = env.aeril_ok(&["recover"]);
                assert_eq!(rec2.as_array().unwrap().len(), 0, "nada pendiente: {rec2}");
                let list3 = env.checkpoint_list(&thread);
                assert_eq!(list3.iter().filter(|c| c["state"] == "ready").count(), 1);

                // Comentar sobre el checkpoint recuperado funciona.
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
                    "ok",
                ]);
                assert!(c["comment_id"].as_str().is_some());
            }
        }
    }
}

/// Al menos una caída debe ser terminación abrupta EXTERNA (SIGKILL desde
/// fuera), no solo un punto interno de auto-muerte.
#[test]
fn crash_externo_con_sigkill_desde_afuera() {
    let env = TestEnv::new("crash-external");
    let thread = env.thread_new("crash externo");
    let script = env.script(
        "s",
        mock_edit("NOTES.md", "# v2 externo\n\ncontenido externo\n", "ext"),
    );
    let marker = env.root.join("marker");
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
        .env("AERIL_PAUSE_AFTER", "prepared")
        .env("AERIL_CRASH_MARK", &marker)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn aeril");

    let deadline = Instant::now() + Duration::from_secs(30);
    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "el proceso no llegó al punto de pausa"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    child.kill().expect("SIGKILL externo");
    let status = child.wait().expect("wait");
    assert_eq!(signal_of(&status), Some(9), "terminación abrupta externa");

    let rec = env.aeril_ok(&["recover"]);
    let arr = rec.as_array().unwrap();
    assert!(
        arr.iter().any(|e| e["action"] == "completed_ready"),
        "checkpoint recuperado: {rec}"
    );
    assert!(
        arr.iter().any(|e| e["action"] == "run_interrupted"),
        "run del supervisor muerto reconciliado: {rec}"
    );
    let list = env.checkpoint_list(&thread);
    assert_eq!(list[0]["state"], "ready");
    assert_eq!(list[0]["recovered"], true);
}
