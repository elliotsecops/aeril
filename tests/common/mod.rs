//! Helpers compartidos por los tests de integración.
//!
//! Todo test corre en directorios temporales aislados (repo y data dir
//! propios). Nunca se toca el repositorio real de Aeril.
//!
//! Cada binario de test compila este módulo entero: no todos usan todos los
//! helpers, por eso se permite dead_code localmente.
#![allow(dead_code)]

pub mod pty;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub const BIN: &str = env!("CARGO_BIN_EXE_aeril");

pub struct TestEnv {
    pub root: PathBuf,
    pub repo: PathBuf,
    pub data: PathBuf,
}

impl TestEnv {
    pub fn new(name: &str) -> TestEnv {
        let root = std::env::temp_dir().join(format!("aeril-test-{name}-{}", aeril::ids::new_id()));
        let repo = root.join("repo");
        let data = root.join("data");
        std::fs::create_dir_all(&data).expect("mkdir data");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@aeril.local"]);
        git(&repo, &["config", "user.name", "Aeril Test"]);
        std::fs::write(repo.join("NOTES.md"), "# Notas\n\nVersión original.\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "base"]);
        TestEnv { root, repo, data }
    }

    pub fn aeril(&self, args: &[&str]) -> Output {
        self.aeril_env(args, &[])
    }

    pub fn aeril_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .env("AERIL_DATA_DIR", &self.data)
            .current_dir(&self.root);
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().expect("ejecutar aeril")
    }

    pub fn aeril_ok(&self, args: &[&str]) -> serde_json::Value {
        let out = self.aeril(args);
        assert!(
            out.status.success(),
            "aeril {args:?} falló:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        parse_json(&out.stdout)
    }

    pub fn thread_new(&self, title: &str) -> String {
        let v = self.aeril_ok(&[
            "thread",
            "new",
            "--repo",
            self.repo.to_str().unwrap(),
            "--title",
            title,
        ]);
        v["thread_id"].as_str().unwrap().to_string()
    }

    pub fn script(&self, name: &str, script: serde_json::Value) -> PathBuf {
        let path = self.root.join(format!("{name}.json"));
        std::fs::write(&path, script.to_string()).unwrap();
        path
    }

    pub fn run_start(&self, thread_id: &str, prompt: &str, script: &Path) -> serde_json::Value {
        self.aeril_ok(&[
            "run",
            "start",
            "--thread",
            thread_id,
            "--prompt",
            prompt,
            "--backend",
            &format!("mock:{}", script.display()),
        ])
    }

    pub fn checkpoint_list(&self, thread_id: &str) -> Vec<serde_json::Value> {
        self.aeril_ok(&["checkpoint", "list", "--thread", thread_id])
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    pub fn ready_snapshots(&self, thread_id: &str) -> Vec<(String, String)> {
        self.checkpoint_list(thread_id)
            .iter()
            .filter(|c| c["state"] == "ready")
            .map(|c| {
                (
                    c["snapshot_id"].as_str().unwrap().to_string(),
                    c["oid"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    pub fn head_oid(&self) -> String {
        git_out(&self.repo, &["rev-parse", "HEAD"])
    }

    pub fn file(&self, rel: &str) -> String {
        std::fs::read_to_string(self.repo.join(rel)).unwrap_or_default()
    }

    /// Path del worktree del último run (para borrarlo "como el usuario").
    pub fn worktree_path_opt(&self, thread_id: &str) -> Option<PathBuf> {
        let _ = thread_id;
        let dir = self.data.join("worktrees");
        if !dir.exists() {
            return None;
        }
        let mut found = None;
        for entry in std::fs::read_dir(&dir).ok()? {
            let entry = entry.ok()?;
            if entry.file_type().ok()?.is_dir() {
                found = Some(entry.path());
            }
        }
        found
    }

    pub fn worktree_path(&self, thread_id: &str) -> PathBuf {
        self.worktree_path_opt(thread_id)
            .expect("hay un worktree de ejecución")
    }

    pub fn update_ref(&self, refname: &str, oid: &str) {
        git(&self.repo, &["update-ref", refname, oid]);
    }

    pub fn rev_parse(&self, rev: &str) -> String {
        git_out(&self.repo, &["rev-parse", rev])
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        if std::env::var("AERIL_KEEP_TESTS").is_ok() {
            eprintln!("AERIL_KEEP_TESTS: se conserva {}", self.root.display());
        } else {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

pub fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git CLI");
    assert!(
        out.status.success(),
        "git {args:?} falló: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

pub fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git CLI");
    assert!(out.status.success(), "git {args:?} falló");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub fn parse_json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).unwrap_or_else(|e| {
        panic!(
            "stdout no es JSON ({e}): {}",
            String::from_utf8_lossy(bytes)
        )
    })
}

pub fn mock_edit(path: &str, content: &str, message: &str) -> serde_json::Value {
    serde_json::json!({
        "message": message,
        "final_message": "listo",
        "edits": [{"op": "write", "path": path, "content": content}],
    })
}

/// Extrae el contenido textual del patch para el path dado (líneas +).
pub fn added_lines(patch: &str) -> Vec<String> {
    patch
        .lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .map(|l| l[1..].to_string())
        .collect()
}

#[cfg(unix)]
pub fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}
