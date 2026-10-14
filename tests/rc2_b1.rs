//! RC2-B1: cota de bytes del pipeline ACP con backpressure real.
//!
//! Backend fake (`__mock-agent` con `flood`) emitiendo frames válidos mientras
//! el consumidor es deliberadamente lento. Presupuestos chicos y explícitos
//! (sin variables de entorno globales), runtime multi-hilo para que el
//! productor realmente se adelante, y contadores de instrumentación del
//! cliente para asertar la cota de bytes en vuelo.

mod common;
use aeril::agent::acp::{
    AcpClient, AcpLimits, PermissionDecision, PermissionRequest, TurnObserver,
};
use common::*;
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

struct SlowObserver {
    updates: Arc<AtomicUsize>,
    sleep: Duration,
}

impl TurnObserver for SlowObserver {
    fn on_update(&mut self, _update: &Value) -> aeril::error::Result<()> {
        self.updates.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(self.sleep);
        Ok(())
    }
    fn on_permission(
        &mut self,
        _req: &PermissionRequest,
    ) -> aeril::error::Result<PermissionDecision> {
        Ok(PermissionDecision::Cancelled {
            policy: "test_default_deny".into(),
        })
    }
}

fn flood_spec(env: &TestEnv, frames: u64, frame_bytes: usize) -> aeril::agent::BackendSpec {
    let script = env.script(
        "flood",
        serde_json::json!({
            "flood": {"frames": frames, "frame_bytes": frame_bytes},
            "final_message": "fin",
            "edits": []
        }),
    );
    aeril::agent::BackendSpec {
        name: "mock".into(),
        version: "rc2-test".into(),
        program: BIN.to_string(),
        args: vec![
            "__mock-agent".into(),
            "--script".into(),
            script.display().to_string(),
        ],
    }
}

fn has_orphan(script_hint: &str) -> bool {
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return false;
    };
    for p in procs.flatten() {
        let Ok(cmdline) = std::fs::read(p.path().join("cmdline")) else {
            continue;
        };
        let text = String::from_utf8_lossy(&cmdline);
        if text.contains(script_hint) {
            return true;
        }
    }
    false
}

/// Corre un turno de flood con consumidor lento y devuelve
/// (peak_inflight, budget, updates_vistos, stop_reason) con watch anti-deadlock.
fn run_flood(
    env: &TestEnv,
    spec: aeril::agent::BackendSpec,
    limits: AcpLimits,
    sleep_ms: u64,
) -> (usize, usize, usize, String) {
    let updates = Arc::new(AtomicUsize::new(0));
    let updates_thread = updates.clone();
    let cwd = env.root.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let result: Result<(usize, usize, String), String> = rt.block_on(async {
            let mut client = AcpClient::spawn_with_limits(&spec, &cwd, limits)
                .await
                .map_err(|e| e.to_string())?;
            client
                .initialize("rc2", "0")
                .await
                .map_err(|e| e.to_string())?;
            let mut obs = SlowObserver {
                updates: updates_thread,
                sleep: Duration::from_millis(sleep_ms),
            };
            client
                .session_new(&cwd, &mut obs)
                .await
                .map_err(|e| e.to_string())?;
            let completion = client
                .prompt("x", &mut obs, None)
                .await
                .map_err(|e| e.to_string())?;
            let peak = client.inflight_peak_bytes();
            let budget = client.inflight_budget_bytes();
            let _ = client.kill().await;
            Ok((peak, budget, completion.stop_reason))
        });
        let _ = tx.send(result);
    });
    let res = rx
        .recv_timeout(Duration::from_secs(120))
        .expect("deadlock: el pipeline no terminó en 120 s");
    let (peak, budget, stop) = res.expect("pipeline ACP");
    (peak, budget, updates.load(Ordering::SeqCst), stop)
}

#[test]
fn b1_cota_de_bytes_en_vuelo_con_consumidor_lento() {
    let env = TestEnv::new("rc2-b1-bound");
    let frames: u64 = 100;
    let frame_bytes: usize = 60_000;
    let total = frames as usize * frame_bytes; // 6 MB transmitidos
    let budget: usize = 256 * 1024;
    let spec = flood_spec(&env, frames, frame_bytes);
    let (peak, reported_budget, updates, stop) = run_flood(
        &env,
        spec,
        AcpLimits {
            max_line_bytes: 1024 * 1024,
            max_turn_bytes: 64 * 1024 * 1024,
            inflight_bytes: budget,
        },
        20,
    );
    assert_eq!(stop, "end_turn", "el turno debe completar por backpressure");
    assert_eq!(
        updates as u64,
        frames + 1,
        "todos los frames + el mensaje final"
    );
    assert_eq!(reported_budget, budget);
    assert!(
        peak <= budget,
        "la cota de bytes en vuelo se respeta: peak {peak} > budget {budget}"
    );
    assert!(
        peak >= budget / 2,
        "el consumidor lento realmente acumuló cola (peak {peak} vs budget {budget})"
    );
    assert!(
        peak <= total / 8,
        "peak {peak} debe ser una fracción del total {total} (no se encoló todo)"
    );
}

#[test]
fn b1_configuracion_extrema_no_paniquea() {
    // Presupuestos absurdos por env no deben tumbar el proceso (clamp).
    let env = TestEnv::new("rc2-b1-extreme");
    let spec = flood_spec(&env, 3, 1_000);
    let (peak, budget, updates, stop) = run_flood(
        &env,
        spec,
        AcpLimits {
            max_line_bytes: usize::MAX,
            max_turn_bytes: usize::MAX,
            inflight_bytes: usize::MAX,
        },
        0,
    );
    assert_eq!(stop, "end_turn");
    assert_eq!(updates as u64, 4);
    assert!(peak <= budget, "peak {peak} budget {budget}");
}

#[test]
fn b1_frame_individual_mayor_que_el_budget_transita_solo() {
    let env = TestEnv::new("rc2-b1-single");
    let frames: u64 = 3;
    let frame_bytes: usize = 200_000;
    let budget: usize = 64 * 1024;
    let spec = flood_spec(&env, frames, frame_bytes);
    let script_path = spec.args[2].clone();
    let (peak, reported_budget, updates, stop) = run_flood(
        &env,
        spec,
        AcpLimits {
            max_line_bytes: 1024 * 1024,
            max_turn_bytes: 64 * 1024 * 1024,
            inflight_bytes: budget,
        },
        1,
    );
    assert_eq!(stop, "end_turn");
    assert_eq!(updates as u64, frames + 1, "los frames grandes se entregan");
    assert_eq!(reported_budget, budget);
    assert!(
        peak <= budget,
        "un frame mayor al budget ocupa TODO el cupo (peak {peak} ≤ {budget})"
    );
    // Sin huérfanos: el grupo de procesos del backend se termina en kill().
    // El path del script es único de este test (los demás corren en paralelo).
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !has_orphan(&script_path),
        "quedó un proceso del backend vivo tras kill()"
    );
}
