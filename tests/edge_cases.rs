//! Casos borde obligatorios: refs conflictivas, objetos faltantes, destino
//! stale/divergente/ocupado, validación contra workspace que no coincide y
//! cancelación del turno.

mod common;
use common::*;

#[test]
fn ref_conflictiva_detiene_y_nunca_sobrescribe() {
    let env = TestEnv::new("edge-refconflict");
    let thread = env.thread_new("conflicto de ref");
    let script = env.script("s", mock_edit("NOTES.md", "# v2 conflicto\n", "x"));
    let out = env.aeril_env(
        &[
            "run", "start", "--thread", &thread, "--prompt", "x", "--backend",
            &format!("mock:{}", script.display()),
        ],
        &[("AERIL_CRASH_AFTER", "prepared")],
    );
    assert_eq!(signal_of(&out.status), Some(9));

    let list = env.checkpoint_list(&thread);
    let sid = list[0]["snapshot_id"].as_str().unwrap().to_string();

    // Ref ajena/divergente en el mismo nombre (p. ej. una DB restaurada).
    let other = env.head_oid();
    env.update_ref(&format!("refs/aeril/checkpoints/{sid}"), &other);

    let rec = env.aeril_ok(&["recover"]);
    assert_eq!(rec.as_array().unwrap()[0]["action"], "ref_conflict");

    // La ref NO se sobrescribió y el checkpoint queda failed terminal.
    assert_eq!(
        env.rev_parse(&format!("refs/aeril/checkpoints/{sid}")),
        other
    );
    let list2 = env.checkpoint_list(&thread);
    let st = list2
        .iter()
        .find(|c| c["snapshot_id"] == sid.as_str())
        .unwrap();
    assert_eq!(st["state"], "failed");
    assert_eq!(st["terminal"], true);

    // Y una segunda recuperación no reintenta (failed es terminal).
    let rec2 = env.aeril_ok(&["recover"]);
    assert_eq!(rec2.as_array().unwrap().len(), 0);
}

#[test]
fn objetos_faltantes_se_reportan_sin_fabricar_exito() {
    let env = TestEnv::new("edge-missingobj");
    let thread = env.thread_new("objetos faltantes");
    let script = env.script("s", mock_edit("NOTES.md", "# v2 objetos\n", "x"));
    let out = env.aeril_env(
        &[
            "run", "start", "--thread", &thread, "--prompt", "x", "--backend",
            &format!("mock:{}", script.display()),
        ],
        &[("AERIL_CRASH_AFTER", "prepared")],
    );
    assert_eq!(signal_of(&out.status), Some(9));

    let list = env.checkpoint_list(&thread);
    let sid = list[0]["snapshot_id"].as_str().unwrap().to_string();
    let expected = list[0]["expected_oid"].as_str().unwrap().to_string();

    // Borrar el objeto del commit esperado (repo temporal de test).
    let obj = env
        .repo
        .join(".git/objects")
        .join(&expected[..2])
        .join(&expected[2..]);
    assert!(obj.exists(), "el objeto debe existir antes de borrarlo");
    std::fs::remove_file(&obj).unwrap();

    let rec = env.aeril_ok(&["recover"]);
    assert_eq!(rec.as_array().unwrap()[0]["action"], "objects_missing");
    assert!(
        rec[0]["detail"]
            .as_str()
            .unwrap()
            .contains("no se reconstruye desde el working tree"),
        "el reporte debe decir qué falta y qué NO se hizo: {rec}"
    );
    let list2 = env.checkpoint_list(&thread);
    let st = list2
        .iter()
        .find(|c| c["snapshot_id"] == sid.as_str())
        .unwrap();
    assert_eq!(st["state"], "failed");

    // Comentar ese snapshot falla: nunca hay ready fabricado.
    let c = env.aeril(&[
        "comment", "add", "--thread", &thread, "--snapshot", &sid, "--path", "NOTES.md", "--start",
        "3", "--end", "3", "--text", "no",
    ]);
    assert!(!c.status.success());
}

#[test]
fn integracion_stale_y_divergente_bloquean_sin_tocar_nada() {
    let env = TestEnv::new("edge-integrate");
    let thread = env.thread_new("integración");
    let script = env.script("s", mock_edit("NOTES.md", "# v2 integración\n", "x"));
    let run = env.run_start(&thread, "x", &script);
    let sid = run["snapshot"]["snapshot_id"].as_str().unwrap().to_string();
    let s1_oid = run["snapshot"]["oid"].as_str().unwrap().to_string();
    let b0 = env.head_oid();

    // 1) stale: la rama destino se movió respecto de la base del snapshot.
    let b = "destino".to_string();
    env.update_ref(&format!("refs/heads/{b}"), &b0);
    env.aeril_ok(&["integrate", "--thread", &thread, "--snapshot", &sid, "--to", &b]);
    // ahora destino == S1; integrar de nuevo el mismo snapshot es stale.
    let out = env.aeril(&["integrate", "--thread", &thread, "--snapshot", &sid, "--to", &b]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("se movió"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(env.rev_parse(&b), s1_oid, "la rama no se movió por el bloqueo");

    // 2) divergente: rama con un commit que no es ancestro del snapshot.
    let tree = env.rev_parse("HEAD^{tree}");
    let divergent = git_out(
        &env.repo,
        &["commit-tree", &tree, "-p", &b0, "-m", "línea paralela"],
    );
    env.update_ref("refs/heads/diverge", &divergent);
    let out = env.aeril(&[
        "integrate", "--thread", &thread, "--snapshot", &sid, "--to", "diverge", "--expect",
        &divergent,
    ]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no desciende"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(env.rev_parse("refs/heads/diverge"), divergent);

    // 3) destino ocupado por un checkout: main está chequeada → bloqueo.
    let out = env.aeril(&["integrate", "--thread", &thread, "--snapshot", &sid, "--to", "main"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("checkout"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Los bloqueos quedan registrados como eventos, sin force-update.
    let out = env.aeril(&["export", "--thread", &thread, "--format", "jsonl"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        text.matches("\"kind\":\"integration.blocked\"").count(),
        3,
        "stale + divergente + checkout ocupado"
    );
    assert_eq!(text.matches("\"kind\":\"integration.completed\"").count(), 1);
}

#[test]
fn validacion_rechaza_workspace_que_no_coincide_con_el_snapshot() {
    let env = TestEnv::new("edge-validate");
    let thread = env.thread_new("validación");
    let script = env.script("s", mock_edit("NOTES.md", "# v2 validación\n", "x"));
    let run = env.run_start(&thread, "x", &script);
    let sid = run["snapshot"]["snapshot_id"].as_str().unwrap().to_string();

    // El workspace queda con contenido distinto al snapshot (edición posterior).
    let wt = env.worktree_path(&thread);
    std::fs::write(wt.join("NOTES.md"), "# contenido posterior\n").unwrap();

    let out = env.aeril(&[
        "validate", "--thread", &thread, "--snapshot", &sid, "--", "cat", "NOTES.md",
    ]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no coincide con el snapshot"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Con el workspace intacto, la validación corre y queda ligada al OID.
    let env2 = TestEnv::new("edge-validate-ok");
    let thread2 = env2.thread_new("validación ok");
    let script2 = env2.script("s", mock_edit("NOTES.md", "# v2 ok\n", "x"));
    let run2 = env2.run_start(&thread2, "x", &script2);
    let sid2 = run2["snapshot"]["snapshot_id"].as_str().unwrap();
    let v = env2.aeril_ok(&[
        "validate", "--thread", &thread2, "--snapshot", sid2, "--", "cat", "NOTES.md",
    ]);
    assert_eq!(v["exit_code"], 0);
    assert_eq!(v["oid"].as_str().unwrap(), run2["snapshot"]["oid"].as_str().unwrap());
    assert!(v["output_tail"].as_str().unwrap().contains("# v2 ok"));
}

#[test]
fn cancelacion_detiene_el_turno_del_motor() {
    let env = TestEnv::new("edge-cancel");
    let thread = env.thread_new("cancelación");
    let script = env.script(
        "slow",
        serde_json::json!({"message": "pensando mucho…", "delay_ms": 30000, "edits": []}),
    );
    let summary = env.aeril_ok(&[
        "run", "start", "--thread", &thread, "--prompt", "x", "--backend",
        &format!("mock:{}", script.display()), "--cancel-after-ms", "1500",
    ]);
    assert_eq!(summary["state"], "cancelled", "summary: {summary}");
    assert_eq!(summary["stop_reason"], "cancelled");

    let out = env.aeril(&["export", "--thread", &thread, "--format", "jsonl"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("\"stop_reason\":\"cancelled\""));
    assert!(!text.contains("checkpoint.ready"), "no hay checkpoint de un turno cancelado");
}
