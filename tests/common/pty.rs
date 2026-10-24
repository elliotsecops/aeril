//! Harness PTY para tests de la TUI (S3).
//!
//! Abre un par pseudo-terminal, lo asigna a stdin/stdout/stderr del proceso
//! TUI y permite enviar teclas y drenar la salida. NO es un multiplexor de
//! paneles: es infraestructura de test para ejercitar el flujo real de
//! usuario (teclas → TUI → DB) con procesos reales.
#![allow(dead_code)]

use super::{BIN, TestEnv};
use std::os::fd::{FromRawFd, RawFd};
use std::process::{Child, Command, Stdio};

pub struct TuiPty {
    master: RawFd,
    pub child: Child,
    buf: Vec<u8>,
}

impl TuiPty {
    /// Lanza la TUI (`aeril` sin subcomando) conectada a un PTY de 120x40.
    pub fn spawn(env: &TestEnv, args: &[&str], extra_env: &[(&str, &str)]) -> TuiPty {
        Self::spawn_sized(env, args, extra_env, 40, 120)
    }

    /// Igual que [`spawn`] con un tamaño de terminal explícito (rows, cols).
    pub fn spawn_sized(
        env: &TestEnv,
        args: &[&str],
        extra_env: &[(&str, &str)],
        rows: u16,
        cols: u16,
    ) -> TuiPty {
        unsafe {
            let mut master: libc::c_int = -1;
            let mut slave: libc::c_int = -1;
            let winsz = libc::winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            let rc = libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &winsz,
            );
            assert_eq!(rc, 0, "openpty falló: {}", std::io::Error::last_os_error());
            // El master no debe heredarse por el hijo.
            libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC);

            // Entregar el slave como stdio del hijo (dup por cada fd).
            let slave_in = libc::dup(slave);
            let slave_out = libc::dup(slave);
            assert!(slave_in >= 0 && slave_out >= 0, "dup del slave falló");

            let mut cmd = Command::new(BIN);
            cmd.args(args)
                .env("AERIL_DATA_DIR", &env.data)
                .env("TERM", "xterm-256color")
                .current_dir(&env.root)
                .stdin(Stdio::from(std::fs::File::from_raw_fd(slave_in)))
                .stdout(Stdio::from(std::fs::File::from_raw_fd(slave_out)))
                .stderr(Stdio::from(std::fs::File::from_raw_fd(slave)));
            for (k, v) in extra_env {
                cmd.env(k, v);
            }
            let child = cmd.spawn().expect("spawn de la TUI");
            TuiPty {
                master,
                child,
                buf: Vec::new(),
            }
        }
    }

    /// Envía bytes crudos (teclas) al PTY.
    pub fn send(&mut self, bytes: &[u8]) {
        let mut rest = bytes;
        while !rest.is_empty() {
            let n = unsafe {
                libc::write(
                    self.master,
                    rest.as_ptr() as *const libc::c_void,
                    rest.len(),
                )
            };
            assert!(
                n > 0,
                "write al PTY falló: {}",
                std::io::Error::last_os_error()
            );
            rest = &rest[n as usize..];
        }
    }

    pub fn enter(&mut self) {
        self.send(b"\r");
    }

    pub fn esc(&mut self) {
        self.send(b"\x1b");
    }

    pub fn down(&mut self) {
        self.send(b"\x1b[B");
    }

    pub fn up(&mut self) {
        self.send(b"\x1b[A");
    }

    /// Escribe texto + Enter.
    pub fn type_line(&mut self, text: &str) {
        self.send(text.as_bytes());
        self.enter();
    }

    /// Drena la salida pendiente del PTY durante `ms` (evita bloquear a la TUI
    /// por buffer lleno). La salida se acumula acotada para asserts opcionales.
    pub fn pump(&mut self, ms: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        let mut chunk = [0u8; 8192];
        loop {
            let remain = deadline.saturating_duration_since(std::time::Instant::now());
            if remain.is_zero() {
                break;
            }
            let mut pfd = libc::pollfd {
                fd: self.master,
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout = remain.as_millis().min(50) as libc::c_int;
            let rc = unsafe { libc::poll(&mut pfd, 1, timeout) };
            if rc > 0 && (pfd.revents & libc::POLLIN) != 0 {
                let n = unsafe {
                    libc::read(
                        self.master,
                        chunk.as_mut_ptr() as *mut libc::c_void,
                        chunk.len(),
                    )
                };
                if n > 0 {
                    self.buf.extend_from_slice(&chunk[..n as usize]);
                    // Solo interesa la cola reciente para asserts de texto.
                    let keep = 256 * 1024;
                    if self.buf.len() > keep {
                        let cut = self.buf.len() - keep;
                        self.buf.drain(..cut);
                    }
                } else if n == 0 {
                    break;
                } else {
                    let e = std::io::Error::last_os_error();
                    if e.kind() != std::io::ErrorKind::Interrupted {
                        break;
                    }
                }
            }
        }
    }

    pub fn output_lossy(&self) -> String {
        String::from_utf8_lossy(&self.buf).into_owned()
    }

    /// Salida sin secuencias ANSI y sin espacios: ratatui omite los espacios
    /// sin cambios con movimientos de cursor, así que buscar frases "con
    /// espacios" en bruto no es confiable. Aplanado sí lo es.
    pub fn output_flat(&self) -> String {
        let bytes = &self.buf;
        let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            if b == 0x1b {
                if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                    let mut j = i + 2;
                    while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                        j += 1;
                    }
                    i = if j < bytes.len() { j + 1 } else { bytes.len() };
                } else if i + 1 < bytes.len() && bytes[i + 1] == b']' {
                    let mut j = i + 2;
                    while j < bytes.len() && bytes[j] != 0x07 {
                        j += 1;
                    }
                    i = if j < bytes.len() { j + 1 } else { bytes.len() };
                } else {
                    i += 2;
                }
            } else if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                i += 1;
            } else {
                out.push(b);
                i += 1;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// ¿Aparece `needle` en la salida renderizada? Compara sobre el texto
    /// aplanado (sin ANSI ni espacios) de ambos lados.
    pub fn output_contains(&self, needle: &str) -> bool {
        let flat = self.output_flat();
        let want: String = needle.chars().filter(|c| !c.is_whitespace()).collect();
        flat.contains(&want)
    }

    /// ¿Sigue vivo el proceso de la TUI?
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Espera la salida normal durante `ms`; devuelve el status si terminó.
    pub fn wait_exit(&mut self, ms: u64) -> Option<std::process::ExitStatus> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Some(status);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            self.pump(50);
        }
    }

    /// Termina el proceso de la TUI sin piedad (limpieza de test).
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        unsafe {
            libc::close(self.master);
        }
        self.master = -1;
    }
}

impl Drop for TuiPty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if self.master >= 0 {
            unsafe {
                libc::close(self.master);
            }
        }
    }
}
