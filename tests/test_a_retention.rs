//! Prueba A del gate (HANDOFF §9): retención de la revisión.
//!
//! comentar un cambio → el archivo cambia de nuevo → borrar el worktree →
//! "reiniciar Aeril" → el comentario sigue mostrando EL CAMBIO ORIGINAL
//! comentado, comparado por OIDs inmutables, no por texto similar.

mod common;
use common::*;

#[test]
fn test_a_retencion_de_la_revision_comentada() {
    let env = TestEnv::new("retention");
    let thread = env.thread_new("Retención de revisión");

    // 2-4: worktree verificado + edición del agente + checkpoint S1.
    let s1 = env.script(
        "s1",
        mock_edit(
            "NOTES.md",
            "# Notas v2\n\nEditado por el agente.\nLinea tres.\n",
            "editando",
        ),
    );
    let run1 = env.run_start(&thread, "Editá NOTES.md", &s1);
    assert_eq!(run1["state"], "completed", "run1: {run1}");
    let s1_id = run1["snapshot"]["snapshot_id"].as_str().unwrap().to_string();
    let s1_oid = run1["snapshot"]["oid"].as_str().unwrap().to_string();
    assert_eq!(run1["snapshot"]["had_changes"], true);

    // 5: comentario anclado a S1.
    let c = env.aeril_ok(&[
        "comment", "add", "--thread", &thread, "--snapshot", &s1_id, "--path", "NOTES.md",
        "--start", "3", "--end", "3", "--text", "la línea 3 es vaga",
    ]);
    let comment_id = c["comment_id"].as_str().unwrap().to_string();

    let before = env.aeril_ok(&["comment", "show", "--thread", &thread, "--comment", &comment_id]);
    assert_eq!(before["blob_verified"], true, "el blob del ancla debe verificar");
    assert_eq!(before["snapshot_oid"].as_str().unwrap(), s1_oid);
    let s1_patch = before["original_patch"].as_str().unwrap().to_string();
    assert!(s1_patch.contains("+# Notas v2"));
    assert_eq!(
        before["anchored_text"].as_str().unwrap(),
        "Editado por el agente."
    );

    // 6-7: el archivo vuelve a cambiar → S2 (con un árbol/OID distinto).
    let s2 = env.script(
        "s2",
        mock_edit("NOTES.md", "# Notas v3\n\nEditado de nuevo.\n", "de nuevo"),
    );
    let run2 = env.run_start(&thread, "Editá de nuevo", &s2);
    assert_eq!(run2["state"], "completed", "run2: {run2}");
    let s2_oid = run2["snapshot"]["oid"].as_str().unwrap().to_string();
    assert_ne!(s1_oid, s2_oid, "S1 y S2 deben ser objetos distintos");
    let s2_patch = env
        .aeril_ok(&["checkpoint", "show", "--thread", &thread, "--snapshot",
            run2["snapshot"]["snapshot_id"].as_str().unwrap()])
        ["patch"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(s1_patch, s2_patch);

    // 8: borrar el worktree de ejecución (como haría el usuario).
    env.aeril_ok(&["worktree", "rm", "--thread", &thread]);
    assert_eq!(
        env.worktree_path_opt(&thread),
        None,
        "el worktree debe estar borrado"
    );

    // 9: "reiniciar Aeril" (cada invocación del CLI ya es un proceso nuevo)
    //    + recuperación al reabrir.
    let rec = env.aeril_ok(&["recover"]);
    assert_eq!(rec.as_array().unwrap().len(), 0, "no hay nada pendiente");

    // 10: el comentario reabre EXACTAMENTE el patch de S1.
    let after = env.aeril_ok(&["comment", "show", "--thread", &thread, "--comment", &comment_id]);
    assert_eq!(
        after["original_patch"].as_str().unwrap(),
        s1_patch,
        "el patch mostrado debe ser el de S1, byte a byte"
    );
    assert_eq!(after["snapshot_oid"].as_str().unwrap(), s1_oid);
    assert_eq!(after["blob_verified"], true);
    assert_eq!(
        after["anchored_text"].as_str().unwrap(),
        "Editado por el agente."
    );
    assert!(!after["original_patch"].as_str().unwrap().contains("v3"));
    assert_eq!(after["state"], "open");

    // Comparación por contenido inmutable: la ref de S1 sigue en el OID de S1.
    let ref_oid = env.rev_parse(&format!("refs/aeril/checkpoints/{s1_id}"));
    assert_eq!(ref_oid, s1_oid, "la ref de S1 no debe moverse");

    // Y el listado de checkpoints conserva ambos con sus OIDs exactos.
    let ready = env.ready_snapshots(&thread);
    assert!(ready.iter().any(|(id, oid)| id == &s1_id && oid == &s1_oid));
    assert!(ready.iter().any(|(_, oid)| oid == &s2_oid));
}

#[test]
fn test_a_bis_el_comentario_sobrevive_a_mas_ediciones() {
    // Variante: dos ediciones más después del comentario; el ancla no cambia.
    let env = TestEnv::new("retention-more");
    let thread = env.thread_new("Más ediciones");
    let s1 = env.script("s1", mock_edit("NOTES.md", "# v2\n\nfragmento comentado\n", "uno"));
    let r1 = env.run_start(&thread, "uno", &s1);
    let sid = r1["snapshot"]["snapshot_id"].as_str().unwrap().to_string();
    let oid = r1["snapshot"]["oid"].as_str().unwrap().to_string();
    let c = env.aeril_ok(&[
        "comment", "add", "--thread", &thread, "--snapshot", &sid, "--path", "NOTES.md",
        "--start", "3", "--end", "3", "--text", "nota",
    ]);
    let cid = c["comment_id"].as_str().unwrap().to_string();
    for i in 0..2 {
        let s = env.script(
            &format!("more{i}"),
            mock_edit("NOTES.md", &format!("# v{} \n\ncontenido cambiado {i}\n", 3 + i), "mas"),
        );
        let r = env.run_start(&thread, "seguir", &s);
        assert_eq!(r["state"], "completed");
    }
    let view = env.aeril_ok(&["comment", "show", "--thread", &thread, "--comment", &cid]);
    assert_eq!(view["snapshot_oid"].as_str().unwrap(), oid);
    assert_eq!(view["anchored_text"].as_str().unwrap(), "fragmento comentado");
}
