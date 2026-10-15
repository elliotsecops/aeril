//! S6-R1 — Seguridad y reproducibilidad del benchmark de 2 runs.
//!
//! Regresiones de los findings del auditor contra el candidato S6:
//! - A: el script NUNCA borra ni sobreescribe un directorio ajeno (sentinel).
//! - B: la limpieza de procesos es identity-aware (cubierto por el unittest
//!   de Python que este archivo ejecuta).
//! - C: los targets standalone del Makefile crean el directorio de salida
//!   (verificado por `make budget-*` en la validación).
//! - D: la validación de integridad exige 2 runs/2 backends/RSS válidos
//!   (cubierto por el flujo completo de `make budget-two`).
//!
//! Todos los tests operan SOLO sobre rutas descartables propias.

mod common;
use common::*;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/bench/two_run.py")
}

fn run_script(data_dir: &Path) -> Output {
    Command::new("python3")
        .arg(script())
        .args(["--bin", BIN, "--data-dir"])
        .arg(data_dir)
        .args(["--json"])
        .output()
        .expect("ejecutar two_run.py")
}

fn tmp_dir(env: &TestEnv, name: &str) -> PathBuf {
    env.root.join(name)
}

/// Finding A: un directorio existente sin sentinel se rechaza sin tocarlo.
#[test]
fn s6r1_canary_dir_refusal_preserves_files() {
    let env = TestEnv::new("s6r1-canary");
    let canary_dir = tmp_dir(&env, "canary");
    std::fs::create_dir_all(&canary_dir).unwrap();
    let precious = canary_dir.join("precioso.txt");
    std::fs::write(&precious, "no tocar").unwrap();

    let out = run_script(&canary_dir);
    assert!(!out.status.success(), "debe rechazar el directorio ajeno");
    assert_eq!(out.status.code(), Some(2), "código de rechazo de seguridad");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("RECHAZADO") && err.contains(".aeril-bench-two-run"),
        "mensaje claro: {err}"
    );
    // El canario queda intacto y no se plantó ningún sentinel.
    assert_eq!(std::fs::read_to_string(&precious).unwrap(), "no tocar");
    assert!(!canary_dir.join(".aeril-bench-two-run").exists());
    let entries: Vec<String> = std::fs::read_dir(&canary_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, vec!["precioso.txt".to_string()], "{entries:?}");
}

/// Finding A: raíz, home y symlinks se rechazan sin efectos.
#[test]
fn s6r1_unsafe_targets_refused() {
    let env = TestEnv::new("s6r1-unsafe");
    for target in [
        "/",
        &std::env::var("HOME").unwrap_or_else(|_| "/root".into()),
    ] {
        let out = run_script(Path::new(target));
        assert!(!out.status.success(), "target inseguro {target} aceptado");
        assert_eq!(out.status.code(), Some(2), "{target}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("RECHAZADO"),
            "{target}"
        );
    }
    // Symlink a un directorio con datos: rechazo sin tocar el destino.
    let real = tmp_dir(&env, "real");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::write(real.join("dato.txt"), "intacto").unwrap();
    let link = tmp_dir(&env, "link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let out = run_script(&link);
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("symlink"));
    assert_eq!(
        std::fs::read_to_string(real.join("dato.txt")).unwrap(),
        "intacto"
    );
}

/// Findings A/B: la suite de seguridad del script (unittest stdlib) pasa.
#[test]
fn s6r1_benchmark_safety_python_suite() {
    let suite = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/bench/test_two_run_safety.py");
    let out = Command::new("python3")
        .arg(suite)
        .output()
        .expect("ejecutar unittest de seguridad");
    assert!(
        out.status.success(),
        "suite de seguridad falló:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
