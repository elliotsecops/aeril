//! A4 — entrada de comentarios segura (regresión).
//!
//! Dos escenarios con la TUI real en un PTY:
//! 1. Con el checkpoint verificado seleccionado, `c` abre el editor y el texto
//!    puede contener `i`, `q`, `f` y Enter SIN disparar atajos globales
//!    (integración, salida, feedback); el comentario se guarda íntegro.
//! 2. Con un evento que NO es checkpoint, `c` abre un diálogo de bloqueo
//!    inequívoco y las letras/atajos quedan inactivos hasta cerrarlo.

mod common;
use common::pty::TuiPty;
use common::*;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

fn mock_script(env: &TestEnv) -> PathBuf {
    env.script(
        "mock",
        json!({
            "message": "trabajando",
            "final_message": "listo",
            "delay_ms": 500,
            "edits": [{"op": "append", "path": "NOTES.md", "content": "\n- avance del agente\n"}]
        }),
    )
}

fn spawn_tui(env: &TestEnv, script: &Path) -> TuiPty {
    let backend = format!("mock:{}", script.display());
    TuiPty::spawn(env, &["--backend", &backend], &[])
}

/// Espera la lista de tareas y abre la tarea; lanza un run y espera el cierre.
fn open_task_and_run(tui: &mut TuiPty, title: &str) {
    let mut seen = false;
    for _ in 0..100 {
        tui.pump(150);
        if tui.output_contains(title) {
            seen = true;
            break;
        }
    }
    assert!(seen, "la tarea no apareció en la TUI");
    tui.enter();
    tui.pump(400);
    tui.send(b"n");
    tui.pump(200);
    tui.send(b"revisa el repo");
    tui.enter();
    let mut done = false;
    for _ in 0..120 {
        tui.pump(150);
        if tui.output_contains("endcompleted") {
            done = true;
            break;
        }
    }
    assert!(done, "el run no llegó a completed");
    // Refresco explícito del detalle (el summary puede llegar con lag).
    tui.send(b"r");
    tui.pump(400);
}

/// Review + selección determinista del `checkpoint.ready` (penúltimo evento).
fn select_checkpoint(tui: &mut TuiPty) {
    tui.send(b"\t");
    tui.pump(300);
    for _ in 0..50 {
        tui.down();
    }
    tui.up();
    tui.pump(300);
}

fn comments(env: &TestEnv, thread: &str) -> Vec<Value> {
    env.aeril_ok(&["comment", "list", "--thread", thread])
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn run_count(env: &TestEnv, thread: &str) -> usize {
    env.aeril_ok(&["run", "list", "--thread", thread])
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0)
}

// ── 1) texto con letras-atajo + Enter: se guarda, no dispara acciones ───────

#[test]
fn comment_text_with_shortcut_letters_is_saved_without_side_effects() {
    let env = TestEnv::new("tui-comment-safe");
    let thread = env.thread_new("seguro");
    let head_before = env.head_oid();
    let script = mock_script(&env);
    let mut tui = spawn_tui(&env, &script);

    open_task_and_run(&mut tui, "seguro");
    select_checkpoint(&mut tui);

    tui.send(b"c");
    tui.pump(300);
    assert!(
        tui.output_contains("COMENTARIO"),
        "c sobre un checkpoint verificado debe abrir el editor de comentario"
    );

    // Texto que contiene i, q y f (atajos globales) — NO deben ejecutarse.
    let text = "iqf i q f iq";
    tui.send(text.as_bytes());
    tui.pump(200);
    tui.enter();
    tui.pump(600);

    assert!(tui.alive(), "la 'q' del texto no debe cerrar la TUI");
    let head_after = env.head_oid();
    assert_eq!(
        head_after, head_before,
        "la 'i'/'f' del texto no debe integrar ni lanzar feedback"
    );
    tui.kill();

    let cs = comments(&env, &thread);
    assert_eq!(cs.len(), 1, "el comentario debe guardarse una vez");
    assert_eq!(cs[0]["text"], text, "el texto debe conservarse íntegro");
    assert_eq!(run_count(&env, &thread), 1, "no debe lanzarse otro run");
}

// ── 2) evento no-checkpoint: diálogo de bloqueo, atajos inactivos ───────────

#[test]
fn comment_on_non_checkpoint_shows_blocking_dialog_and_keeps_shortcuts_inactive() {
    let env = TestEnv::new("tui-comment-blocked");
    let thread = env.thread_new("bloqueado");
    let head_before = env.head_oid();
    let script = mock_script(&env);
    let mut tui = spawn_tui(&env, &script);

    open_task_and_run(&mut tui, "bloqueado");
    // Selección en el último evento (`end`), que NO es checkpoint.ready.
    tui.send(b"\t");
    tui.pump(300);
    for _ in 0..50 {
        tui.down();
    }
    tui.pump(200);

    tui.send(b"c");
    tui.pump(400);
    assert!(
        tui.output_contains("Comentario bloqueado"),
        "c sobre un evento no-checkpoint debe abrir el diálogo de bloqueo"
    );

    // Letras-atajo dentro del diálogo: ninguna debe ejecutar acciones.
    tui.send(b"iqfq");
    tui.pump(400);
    assert!(tui.alive(), "las teclas del diálogo no deben cerrar la TUI");
    assert_eq!(env.head_oid(), head_before, "no debe integrar");
    assert!(
        comments(&env, &thread).is_empty(),
        "no debe crear comentarios"
    );
    assert_eq!(run_count(&env, &thread), 1, "no debe lanzar runs");

    // Esc cierra el diálogo y la TUI vuelve a aceptar atajos normales.
    tui.esc();
    tui.pump(400);
    tui.send(b"?");
    tui.pump(400);
    assert!(
        tui.output_contains("↑↓ select · Enter open"),
        "tras cerrar el diálogo, '?' debe abrir la ayuda normal"
    );
    tui.kill();
}
