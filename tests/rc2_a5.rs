//! RC2-A5: submodules/gitlinks nunca se omiten en silencio.
//!
//! Política v0 (conservadora y explícita):
//! - gitlink con puntero modificado (inicializado) → rechazo (RC1);
//! - escrituras BAJO un gitlink no inicializado → rechazo (RC2): el contenido
//!   no se puede capturar de forma completa y NUNCA se reporta "sin cambios";
//! - gitlink no inicializado sin escrituras → se captura el resto normalmente
//!   (el puntero vive en el árbol base y no se toca).

mod common;
use common::*;

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

fn create_workspace(env: &TestEnv, thread: &str) -> std::path::PathBuf {
    let s = env.script(
        "boot",
        mock_edit("NOTES.md", "# v2\n\nbase del ws\n", "boot"),
    );
    let run = env.run_start(thread, "boot", &s);
    assert_eq!(run["state"], "completed", "run de arranque: {run}");
    env.worktree_path(thread)
}

#[test]
fn a5_escritura_bajo_submodulo_no_inicializado_se_rechaza() {
    let env = TestEnv::new("rc2-a5-uninit-write");
    add_submodule(&env);
    let thread = env.thread_new("a5u");
    let wt = create_workspace(&env, &thread);
    let checkpoints_before = env.checkpoint_list(&thread).len();

    // El submodule del worktree está SIN inicializar: `sub/.git` no existe.
    assert!(
        !wt.join("sub/.git").exists(),
        "precondición: submodule no inicializado en el worktree"
    );

    // El agente escribe SOLO debajo del path del gitlink.
    let s = env.script(
        "s-sub",
        mock_edit("sub/vendor.txt", "escrito bajo el submódulo\n", "x"),
    );
    let run = env.run_start(&thread, "x", &s);
    assert_eq!(
        run["state"], "failed",
        "escrituras bajo gitlink no inicializado deben fallar explícitamente: {run}"
    );
    let detail = run["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("submódulo no inicializado") || detail.contains("no inicializado"),
        "error útil para diagnosticar: {detail}"
    );
    assert_eq!(
        env.checkpoint_list(&thread).len(),
        checkpoints_before,
        "jamás un checkpoint parcial ni 'sin cambios' falso"
    );
}

#[test]
fn a5_submodulo_no_inicializado_sin_escrituras_captura_lo_soportado() {
    let env = TestEnv::new("rc2-a5-uninit-clean");
    add_submodule(&env);
    let thread = env.thread_new("a5c");
    let _wt = create_workspace(&env, &thread);

    // Nadie escribió bajo `sub/`: la captura normal debe seguir funcionando.
    let s = env.script("s-ok", mock_edit("NOTES.md", "# v3 normal\n", "x"));
    let run = env.run_start(&thread, "x", &s);
    assert_eq!(run["state"], "completed", "run: {run}");
    let sid = run["snapshot"]["snapshot_id"].as_str().unwrap();
    let show = env.aeril_ok(&["checkpoint", "show", "--thread", &thread, "--snapshot", sid]);
    let files: Vec<String> = show["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap().to_string())
        .collect();
    assert!(files.contains(&"NOTES.md".to_string()), "{files:?}");
    assert!(
        !files.iter().any(|p| p.starts_with("sub/")),
        "el contenido del gitlink no se captura como archivos comunes: {files:?}"
    );
}

#[test]
fn a5_edicion_mixta_con_escritura_bajo_gitlink_se_rechaza_completa() {
    let env = TestEnv::new("rc2-a5-mixed");
    add_submodule(&env);
    let thread = env.thread_new("a5m");
    let _wt = create_workspace(&env, &thread);
    let before = env.checkpoint_list(&thread).len();

    let s = env.script(
        "mixed",
        serde_json::json!({
            "message": "x", "final_message": "y",
            "edits": [
                {"op": "write", "path": "NOTES.md", "content": "# normal + submódulo\n"},
                {"op": "write", "path": "sub/x.txt", "content": "colado\n"}
            ]
        }),
    );
    let run = env.run_start(&thread, "x", &s);
    assert_eq!(run["state"], "failed", "nunca captura parcial: {run}");
    assert_eq!(env.checkpoint_list(&thread).len(), before);
}

#[test]
fn a5_submodulo_inicializado_modificado_sigue_rechazandose() {
    // Regresión del comportamiento RC1 (cambio de puntero en submódulo).
    let env = TestEnv::new("rc2-a5-init");
    add_submodule(&env);
    let thread = env.thread_new("a5i");
    let wt = create_workspace(&env, &thread);
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
    git(&wt.join("sub"), &["commit", "-qm", "avance"]);
    let before = env.checkpoint_list(&thread).len();

    let s = env.script(
        "s2",
        serde_json::json!({"message": "x", "final_message": "y", "edits": []}),
    );
    let run = env.run_start(&thread, "x", &s);
    assert_eq!(run["state"], "failed", "run: {run}");
    assert!(
        run["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("no soportado"),
        "detalle: {}",
        run["detail"]
    );
    assert_eq!(env.checkpoint_list(&thread).len(), before);
}
