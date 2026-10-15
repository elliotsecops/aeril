# Fase 1 — Estado de aceptación de ingeniería (S6)

> Fecha: 2026-10-15 · Baseline: `main @ 1cfe627` (S5 integrado) · Rama S6:
> `feat/phase1-s6-final-acceptance` · Este documento es el registro de
> aceptación auditable de la Fase 1. No reemplaza a `MANIFIESTO.md`.

> **Estado S6 (actualizado tras la auditoría):** la auditoría independiente de
> S6 devolvió **FAIL** contra `9ea0e68`, confinado al **tooling del benchmark**
> (A: limpieza destructiva de `--data-dir`; B: terminación de PIDs sin
> identidad; C: targets standalone del Makefile; D: validación de mediciones).
> Los gates de producto quedaron verdes. La remediación vive en
> `fix/phase1-s6-benchmark-safety` (SHA en el commit de entrega de S6-R1) y
> **queda pendiente de re-verificación independiente**: S6 **no** está aceptado.

## 0. Verdicto de readiness (tres claims separados)

**A. Ingeniería de concurrencia mock de Fase 1 — COMPLETA.**
Independientemente verificada: G1–G9 en verde (S5, PASS de auditoría), invariantes
de seguridad preservados (S0/S0-R1, RC2.1, S4-R1), presupuestos de Fase 0
re-medidos en verde (release). Límite de producto N=2 (por instancia de TUI).

**B. Operación real con UN Hermes — soportada, con preguntas abiertas.**
Lo validado con Hermes real (no mock): el backend ACP `hermes` funciona de punta
a punta en el corte vertical de Fase 0 (demo real registrada); S1 verificó el
aislamiento de **estado en disco** por `HERMES_HOME` (state.db/installs/locks por
perfil, sin escrituras a `~/.hermes`). Lo NO validado: restauración verificada de
contexto conversacional (`session/load` responde OK sin garantizar historial;
`verified=false` siempre) y provisioning de credenciales por perfil.

**C. Multi-Hermes real / deployment productivo — NO CERTIFICADO.**
No existe evidencia independiente de sesiones ACP autenticadas concurrentes ni
de aislamiento de credenciales del host. El shim determinista de G8 **no** es un
Hermes autenticado. No se infiere readiness productivo de tests mock.

**Declaración recomendada (si S6 pasa):**

> Phase 1 engineering acceptance is complete for deterministic mock-agent
> concurrency (N=2). Real concurrent authenticated Hermes sessions and verified
> conversational-context restoration remain outside the certified scope.

## 1. Baseline Fase 0 y garantías preservadas

- Corte vertical completo (un thread) auditado e integrado: `41ab86cc` (FF),
  62/62 tests + Clippy + fmt; closeout `201515d`; plan Fase 1 `a2bac84`.
- Presupuestos vigentes: arranque p95 ≤ 300 ms · RSS idle ≤ 50 MB · binario
  ≤ 30 MB · app + 5 agentes mock ≤ 500 MB · caché por thread ≤ 10 MiB.
- Invariantes preservados y con tests vigentes: leases de workspace con fence
  transaccional (RC2.1: un dueño vivo no se desplaza, ni con `--force`),
  presupuestos de bytes del pipeline ACP, checkpoints inmutables por OID,
  permisos fail-closed, política de gitlinks/submódulos.

## 2. Milestones S0–S6

| # | Verdicto | Commit certificado | Capacidades verificadas | Evidencia | Límites |
|---|---|---|---|---|---|
| **F0/RC2.1** | PASS (auditoría) | `41ab86cc` (FF) | corte vertical, checkpoints, leases+fence, presupuestos | `docs/phase0-*`, 62 tests | mock + demo Hermes real; sin concurrencia |
| **S0** | PASS (auditoría S0-R1) | **`e38a7d4`** (certificado; histórico: `130c94f`→`6adaa5d`) | reconciliación durable de runs interrumpidos, evidencia supervisor/backend, gate de reuso conservador | `tests/phase1_s0.rs`, `tests/phase1_s0_safety.rs` | identidad no verificable ⇒ bloqueo conservador |
| **S1** | **CONDITIONAL** | `4d14e09` | `HERMES_HOME` honrado end-to-end; state.db/installs/caches/locks por perfil; cero escrituras a `~/.hermes`; kills aislados | `docs/phase1-s1-hermes-isolation.md` | sesiones reales y credenciales por perfil NO provisionadas; ~132 MiB RSS por `hermes acp` (no presupuestado) |
| **S2** | PASS (auditoría) | `b63f582` (FF) | `run list [--active]`: supervisor alive/dead/unknown + lease owned/missing/held_by_other + status efectivo, solo lectura | `tests/phase1_s2.rs` (8) | señales de lectura; la resolución es de S0 |
| **S3** | CONDITIONAL PASS (auditoría) | `99d2f7b` (FF) | TUI N=2: slots por invocación, 1 run/thread, cancelación por thread con ack, salida segura, Hermes real 1/vez | `tests/phase1_s3.rs` (9) | follow-ups de panic (ver §6) |
| **S4** | auditoría detectó D1/D2 | `60921bc`→`48e94af` | `workspace list` read-only, dedupe, estados/elegibilidad, borrado guardado | `tests/phase1_s4.rs` (11) | reemplazado por S4-R1 |
| **S4-R1** | PASS (auditoría) | `dac8ab2` (rojo) → `ecdbd38` → `880b7c7` (FF) | borrado seguro: guard de supervisor vivo/desconocido (D1) + fence transaccional de lease (D2); `--unsafe-ok` no pisa leases | `tests/phase1_s4_r1.rs` (7) | fences serializan contra adquisiciones de Aeril, no contra procesos externos |
| **S5** | PASS (auditoría) | `1cfe627` (FF) | G1–G9 end-to-end (matriz §3) | `tests/phase1_concurrency.rs` (11) | mock determinista; Hermes real concurrente NO verificado |
| **S6** | **FAIL de auditoría (solo tooling de bench) → remediación S6-R1** | `4096b15` (candidato fallido `9ea0e68`); remediación en `fix/phase1-s6-benchmark-safety` | presupuestos release re-medidos, registro de aceptación, límites declarados; S6-R1 corrige ciclo de vida del data dir, limpieza identity-aware (pidfd), targets standalone y validación de mediciones | §4/§5 + `make budget*` + `tests/phase1_s6_bench.rs` + unittest de seguridad | aceptación S6 pendiente de re-verificación independiente |

## 3. Matriz final G1–G9

| Gate | Topología | Invariante esperado | Resultado observado | Evidencia | Limitaciones |
|---|---|---|---|---|---|
| **G1** | TUI única (supervisores = hilos del MISMO proceso; backends = procesos hijos) | 2 runs concurrentes reales, identidades/workspaces/leases/checkpoints aislados | ✅ solape de intervalos; 2 activos simultáneos vía `run list` externo; 2 leases vivos; contenido único por worktree; 2 snapshots; main intacto | `g1_dos_runs_concurrentes_desde_una_tui` | hilos, no procesos OS independientes |
| **G2a** | 2 supervisores CLI independientes | crash de A aislado: B completa; recuperación única; backend verificado/terminado; sin reuso inseguro | ✅ B `completed`; `run_interrupted` 1 vez (`unsafe_state=false`, backend terminado/muerto); checkpoint previo revisable; reuso seguro tras limpieza | `g2a_crash_supervisor_aislado` | bloqueo conservador por identidad no verificable se cubre en S0/S4 |
| **G2b-1** | TUI única | muerte SOLO del backend de A ⇒ A honesto, B intacto | ✅ A `failed` sin checkpoint; B `completed` con checkpoint; TUI viva | `g2b_falla_backend_de_a_en_tui` | matar un backend NO mata al supervisor |
| **G2b-2** | TUI única | SIGKILL del proceso TUI ⇒ ambos runs reconciliados (B NO sobrevive) | ✅ ambos `failed` una sola vez; sin checkpoints; sin flags inseguros (backend verificado) | `g2b_sigkill_de_la_tui_completa_se_reconcilia` | sin aislamiento por hilo a nivel SO |
| **G3** | TUI única (×2) | cancelación independiente: slot-until-ack; A sin checkpoint; B completa; sin backends residuales | ✅ rechazo visible del reemplazo; terco por plazo (~6 s) y cooperativo; `kill(pid,0)` negativo tras el ack | `g3_cancelacion_independiente_y_por_plazo` | el ack depende de la nota terminal del runner |
| **G4** | CLI | revisión durable tras borrado seguro | ✅ refs/comentarios/`checkpoint show` accesibles; B y main intactos | `g4_revision_durable_tras_borrado` | — |
| **G5** | CLI | rehidratación desde el OID EXACTO por hilo, sin cruces | ✅ `workspace.recreated.from_oid` == snapshot de cada hilo; HEAD == OID; `attempted=load`, `accepted=true`, **`verified=false`** | `g5_restauracion_independiente` | archivos ≠ contexto del agente; ediciones no chequeadas no recuperables |
| **G6** | 2 procesos CLI | fallo de auditoría de permisos aislado por proceso | ✅ A fail-closed (archivo sin editar, `permission.responded=0`, `audit_failed=1`); B completa sin contaminación | `g6_auditoria_de_permisos_aislada` | reintento acotado ante kill externo (ver §6) |
| **G7** | TUI única | presupuestos en el escenario real de 2 runs | ✅ mediciones §4 (debug y release) | `g7_mediciones_con_dos_runs`, `make budget-two` | mocks ≠ Hermes |
| **G8** | TUI única + shim | política single-Hermes aplicada | ✅ segundo Hermes simultáneo rechazado (`Hermes real: un run por vez`) | `g8_hermes_una_sesion_por_vez` | **CONDITIONAL para Hermes real concurrente**: el shim determinista NO es un Hermes autenticado |
| **G9** | 2 procesos CLI | integración FF concurrente con CAS: un solo ganador | ✅ exactamente 1 éxito; perdedor `stale` ("se movió") + `integration.blocked`; ref en OID ganador; `recover`×2 no fabrica un segundo éxito | `g9_integracion_concurrente_ff` | reconciliación puede rotular un intento en vuelo como abandonado (ref autoritativa; ver §6) |

**Distinciones que se preservan explícitamente:**

1. Dos supervisores dentro de una TUI son **hilos Rust del mismo proceso**; no
   son procesos OS matables independientemente.
2. Supervisores CLI independientes **sí** dan aislamiento de proceso.
3. Matar un backend **no** termina necesariamente a su supervisor.
4. Matar el proceso TUI termina la supervisión de **ambos** runs.
5. Restaurar el workspace desde un OID exacto **no** es restaurar el contexto
   conversacional del agente.
6. El shim determinista de Hermes **no** es una sesión Hermes autenticada.

## 4. Presupuestos y performance finales

Host de medición: Intel i7-8550U (4c/8t), 15.4 GB RAM, kernel `7.0.0-38-generic`,
ext4 sobre `/dev/nvme0n1p5`, caché de páginas WARM, build release
(LTO thin + strip), `CARGO_BUILD_JOBS=2`.

**Release (binario `target/release/aeril`):**

| Métrica | Valor | Límite | Verdicto |
|---|---|---|---|
| Arranque p50 (historia sembrada: 100 threads/1000 eventos/10 diffs, 30 muestras, externo+beacons) | **10.81 ms** | — | — |
| Arranque p95 (ídem, `n_fail=0`) | **13.01 ms** | ≤ 300 ms | PASS |
| Arranque de una TUI con **dos runs activos** (beacons, 2 muestras) | **50.4 / 50.5 ms** | — | — |
| RSS idle | **6.8 MB** | ≤ 50 MB | PASS |
| RSS con thread abierto | 7.7 MB | — | — |
| Caché residente por thread (delta) | **0.85 MiB** | ≤ 10 MiB | PASS |
| TUI RSS con 2 runs mock activos (2 muestras) | **8.1 / 8.3 MB** | — | — |
| RSS por backend mock (cada uno) | **4.0 MB** | — | — |
| RSS combinado (TUI + 2 mocks) | **16.0 / 16.5 MB** | — | — |
| Escala 5 mocks: app | **9.65 MB** | ≤ 500 MB (app) | PASS |
| Escala 5 mocks: agentes (medidos aparte) | 19.89 MB | — | — |
| Escala 5 mocks: total | 29.54 MB | — | — |
| Latencia de consultas (`run list --active` / `workspace list`, 2 runs activos) | **máx 4.2–4.8 ms** (avg ~3.2 ms) | — | — |
| Binario (stripped, decimal) | **5,553,296 B (5.6 MB)** | ≤ 30 MB | PASS |
| Dependencias runtime de la app | ABI base + zlib del sistema | 0 deps de app | PASS |

Comandos reproducibles: `make budget` (size+deps+mem+start), `make budget-scale`,
`make budget-two` (escenario de 2 runs; data dir descartable). Los JSON quedan en
`bench-data/{start,mem,scale,two_run}.json`.

**Debug (informativo, NO comparar contra los límites de release):** startup con
historia ~99.6 ms; TUI 10.3 MB; mocks 5.7 MB c/u; combinado 21.7 MB; creación de
run ~965 ms; consultas ≤6 ms (G7).

**Los agentes mock NO son Hermes**: estas cifras caracterizan el overhead de la
app y de un agente mock; no permiten inferir el consumo de Hermes real
(~132 MiB RSS por `hermes acp` medido en S1, con perfil compartido).

## 5. Regresión final

- `cargo test --locked --offline --jobs 2 --no-fail-fast` → **121/121**, 0 fallos.
- `cargo fmt --all -- --check` → limpio.
- `cargo clippy --locked --offline --jobs 2 --all-targets -- -D warnings` → limpio.
- `make budget` → PASS · `make budget-scale` → PASS · `make budget-two` → PASS
  (2 corridas reproducibles).
- Regresiones de integración/procesos re-ejecutadas además de la suite:
  `phase1_concurrency` (11), `phase1_s4_r1` (7), `phase1_s0`+`safety`, `phase1_s3`
  (9) — todas verdes. Estabilidad: S5 acumuló 30+ corridas; los dos flakes
  observados se corrigieron sin tocar aserciones (ver §6).

## 6. Limitaciones y deuda técnica (priorizada)

### 6.1 Hermes concurrency — severidad ALTA para multi-Hermes real (no bloquea el hito mock)
- **Consecuencia**: dos sesiones ACP reales concurrentes podrían compartir
  estado/credenciales del host ⇒ corrupción de sesión o cruce de credenciales.
- **Mitigación actual**: la TUI impone 1 Hermes real por vez (S3/G8); S1 verificó
  aislamiento de **estado en disco** por `HERMES_HOME`; single-Hermes es el
  default soportado.
- **Bloquea el hito mock**: NO. **Evidencia requerida antes de release amplio**:
  provisioning de perfiles autenticados (`hermes auth add`/`model` por perfil),
  validación con sesiones reales concurrentes (create/persist/list/load/kill,
  N=2) y revisión de aislamiento de credenciales.

### 6.2 Context restoration — severidad MEDIA (honestidad)
- **Consecuencia**: un usuario podría creer que el agente recuerda la
  conversación; `session/load` aceptado no prueba restauración.
- **Mitigación**: `engine.session` reporta `attempted/accepted/verified`;
  `verified=false` siempre en v0; la rehidratación de **archivos** es desde OID
  exacto; las ediciones no chequeadas no son recuperables.
- **Bloquea**: NO. **Evidencia requerida**: protocolo de restauración
  verificable (o wording de producto que no prometa contexto).

### 6.3 Process safety — severidad MEDIA
- **Consecuencia**: los worktrees Git **no son sandboxes OS**; la atribución por
  grupo de procesos no cubre descendientes que escapan del grupo registrado;
  identidad desconocida ⇒ fail-closed; `--unsafe-ok` es un override de riesgo
  deliberado, no una certificación.
- **Mitigación**: frontera documentada (S0-R1); bloqueo conservador; fence
  transaccional de borrado (S4-R1).
- **Bloquea**: NO. **Evidencia requerida**: sandbox OS (fuera de alcance) o guía
  de usuario explícita.

### 6.4 S3 panic handling — severidad MEDIA-BAJA (UX/confiabilidad)
- **Consecuencia**: (a) un panic capturado del runner puede dejar visibilidad
  stale transitoria del run mientras el proceso TUI sigue vivo; (b) el panic
  hook global de ratatui puede restaurar modos de terminal durante un panic
  capturado de un worker, degradando el teclado subsecuente.
- **Mitigación**: el panic se convierte en nota terminal (libera el slot); el run
  a medias queda para S0. **Bloquea**: NO. **Evidencia requerida**: repro manual
  + cambio dirigido antes de pruebas de usuario amplias.

### 6.5 Integration reconciliation — severidad BAJA
- **Consecuencia**: un `integration.requested` en vuelo puede registrarse como
  "abandonado" por la reconciliación de OTRO proceso mientras el primero sigue
  vivo; la ref Git sigue siendo autoritativa y no se fabrica éxito.
- **Mitigación**: el éxito durable (`integration.completed` + ref) es la
  evidencia; `recover` repetido no duplica éxitos. **Bloquea**: NO. **Evidencia
  requerida**: cambio dirigido de wording/liveness de intentos.

### 6.6 Test-harness retry (G6) — severidad BAJA (solo tests)
- **Consecuencia**: el reintento acotado ante terminación por señal no puede
  probar que el SIGKILL fue externo; **no** es recuperación automática de
  crashes de la aplicación.
- **Mitigación**: el reintento solo ocurre con `status.code() == None` y repite
  las MISMAS aserciones; errores reales no se reintentan. **Bloquea**: NO.

## 7. Smoke test manual reproducible

Con el binario release (`make build`) y `AERIL_DATA_DIR` a elección:

```bash
B=target/release/aeril
$B thread new --repo /ruta/al/repo --title A
$B thread new --repo /ruta/al/repo --title B
$B                                   # abre la TUI (backend mock: --backend mock:<script>)
```

1. En la TUI: el **último thread creado aparece primero** (la lista ordena por
   actividad: `B` está arriba) · `Enter` abre **B** · `n` · escribí `tarea B` · `Enter`.
2. `Esc` · `↓` al thread **A** · `Enter` · `n` · `tarea A` · `Enter`.
3. Observá dos runs activos (header `runs 2/2`; o `$B run list --active`).
4. `Esc` · seleccioná A · `x` (cancelar); B completa solo.
5. Revisá: `$B checkpoint list --thread A` / `--thread B`; `$B workspace list`;
   `$B checkpoint show --thread B --snapshot <id>`.
6. Reabrí la TUI (`$B`): historia durable intacta; `$B export --thread A --format jsonl`
   muestra el timeline completo.

## 8. Declaración de cierre

Los gates de producto de Fase 1 (121/121 + fmt + Clippy + presupuestos release)
siguen verdes, pero la **auditoría de S6 devolvió FAIL en el tooling del
benchmark** (hallazgos A–D) y su remediación (S6-R1) **todavía no fue
re-verificada de forma independiente**: S6 no se declara aceptado y **no se
anuncia el cierre formal de Fase 1**. El cierre queda sujeto a la aceptación
independiente de S6-R1 sobre `fix/phase1-s6-benchmark-safety`, con las
limitaciones de §6 declaradas y sin claim de readiness productivo general ni de
Hermes real concurrente.
