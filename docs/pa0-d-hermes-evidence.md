# PA0-D — Evidencia de binario Linux reproducible y certificación con Hermes real

**Estado:** Verificado localmente (previo al piloto externo **PA0-E**).  
**Referencias contractuales:** [`docs/pa0-alpha-contract.md`](pa0-alpha-contract.md) §3 (`E2E-1`–`E2E-7`), §7 (`Gate estricto de Hermes real`) y §8 (`PA0-D`).

---

## 1. Binario Linux x86_64 reproducible y flujo `E2E-1`

- **Script reproducible:** [`scripts/release/package-linux.sh`](../scripts/release/package-linux.sh) (`make package-linux`) compila con `cargo build --release --locked`, `CARGO_INCREMENTAL=0`, `TZ=UTC`, `LC_ALL=C`, `--remap-path-prefix` en Rust y `-ffile-prefix-map` / `-g0` en C (`cc` para `sqlite3`/`libgit2`/`zlib`), valida presupuestos (`scripts/budget-size.sh`, `scripts/budget-deps.sh`) y genera un tarball determinista (`tar --sort=name --mtime --numeric-owner | gzip -n`) con `.sha256`.
- **Prueba de reproducibilidad bit a bit entre rutas distintas:** dos compilaciones limpias desde cero en `/tmp/aeril-repro-alpha` y `/tmp/aeril-repro-beta` (`SOURCE_DATE_EPOCH=1764160000`, `--locked --offline`) produjeron hashes idénticos byte a byte:
  - Binario (`aeril`, `6,299,088` bytes / `6.3 MB`): `eb65935265b210737333d7315ba363683463d6a4695673694db5a65968da7e46` en ambas rutas.
  - Tarball (`aeril-0.1.0-linux-x86_64.tar.gz`): `1b1ada33345828001af28200860787c59291e8db1265c1461c203b7a8b3d26c9` en ambas rutas.
- **Dependencias dinámicas (`ldd`):** únicamente ABI base del sistema (`linux-vdso.so.1`, `libz.so.1`, `libgcc_s.so.1`, `libm.so.6`, `libc.so.6`, `/lib64/ld-linux-x86-64.so.2`).
- **Compilación final de `src/` sobre `main` (tras ajuste `[--model <id>]` en [`src/cli.rs`](../src/cli.rs)):**
  - `bin_sha256`: `ca2cc34262b68c00e078691bd01df9d7242ba7b74c7198c0ec3bace1af8ed0c3` (`6,299,088` bytes, `6.3 MB`).
  - `cargo fmt --check`: PASS.
  - `cargo clippy --locked --offline --all-targets -- -D warnings`: PASS.
  - Suite Rust completa (`cargo test --locked --offline --jobs 2`): **357/357 PASS** (`35` unit tests + `322` tests de integración, `0` fallos, `0` ignorados).
  - Presupuestos PTY reales sobre dataset `100×1000×10` en `tmpfs` (caché caliente del host): arranque `p50 = 10.89 ms`, `p95 = 11.82 ms` (`30/30` corridas OK); RSS idle `10,060 KiB` (`9.8 MB`), RSS con thread abierto `10,472 KiB` (`10.2 MB`, delta `0.4 MiB`); 2 runs mock activos con `two_run.py` TUI RSS `9,628 KiB`, combinado `18,768 KiB`, arranque `53.13 ms`, latencia promedio de consulta `4.28 ms`.
- **Simulacro limpio `E2E-1`:** desempaquetado con verificación `sha256sum -c`, instalación en `HOME` temporal sin variables ocultas ni `AERIL_DATA_DIR`, ejecución de `aeril version` (`aeril 0.1.0`), `aeril --help`, `aeril thread new` y apertura PTY `120×40` de la TUI confirmando visibilidad del primer thread en **`5.4 ms`**.

---

## 2. Certificación real con Hermes autenticado (1 sesión a la vez, §7)

- **Agente utilizado:** `/home/elliot/.local/bin/hermes`, versión `Hermes Agent v0.21.5+8877.g865ba90` (`hermes acp --version` = `0.21.5+8877.g865ba90`, `hermes acp --check` = `Hermes ACP check OK`), sin mocks ni `fake_hermes.py`, ejecutado en repositorios Git desechables con `AERIL_HERMES_PROFILES` deshabilitado (`1` sesión Hermes autenticada por vez).
- **Recorrido E2E completo de los 8 pasos (§7, repo `/tmp/aeril-pa0d-live/repo`, proveedor `gemini:gemini-2.5-flash`):**
  1. **Crear thread (`E2E-1`):** `thread_id = def3450083724c969c7b4d48b4129267`, `base_oid = 34e571cc8953916023f2ab1cba8103b59ab7422a`.
  2. **Lanzar run real de edición (`E2E-2`):** `run_id = 9e759104e494487cac7dc42bdec4f682`, `engine_session_id = a6c86eb1-5e86-4222-8234-36872242d44e`, duración `41,022 ms`.
  3. **Supervisión y permisos reales (`E2E-2`):** captura de tool calls reales (`read_file`, `patch`, `terminal: python3 -m unittest test_calc.py`), 3 solicitudes reales de `session/request_permission` respondidas y transmitidas (`ok: true`, política `auto_allow_once`) y cierre `completed` (`stop_reason: end_turn`).
  4. **Checkpoint verificable S1:** `checkpoint.prepared` → `checkpoint.ready` con `snapshot_id = 41158689c634404eab3bd428212320ab`, `oid = 9429f7bb936df3aa47ca0aca49ca374f44167f61`, ref `refs/aeril/checkpoints/41158689c634404eab3bd428212320ab` (`calc.py` + `test_calc.py` reales).
  5. **Comentario anclado + feedback real (`E2E-3`):** comentario `881b09ef3ef342b494251f86581a6227` en `calc.py:1` (pasó `attention` a `review_required`); `aeril feedback send --backend hermes --model gemini:gemini-2.5-flash` (`run_id = 1a6614283ea54f07bf631c9d65d5aefb`, duración `51,171 ms`) reanudó sesión vía `session/load` (`attempted: "load", resumed: true`), editó `calc.py` + `test_calc.py` agregando `subtract` y `test_subtract`, y publicó el segundo checkpoint `S2` (`snapshot_id = 0af099983e044b5ca539194d54b62525`, `oid = 7f984c3ae160f74d354045adc5267876cdf2c503`). Al resolver el comentario y registrar `review ack` sobre `S1`, `attention` confirmó explícitamente que el ack de `S1` **no** desbloquea `S2` (`review: "required"` para `S2`).
  6. **Validación + review explícito sobre S2 (`E2E-4` / `E2E-3`):** `aeril validate --thread ... --snapshot 0af099983e044b5ca539194d54b62525 -- python3 -m unittest -v` registró `exit_code = 0`, `tree_before_ok = true`, `tree_after_unchanged = true`, `duration_ms = 127`. Antes del ack en `S2`, `integrate preview --to main` respetó precedencia `C5` (`review_pending`, cero sugerencias `--ff-only`). Tras `aeril review ack` sobre `S2`, `attention` pasó a `ready_to_integrate`.
  7. **Integración Camino B (`PA0-C`) y Camino A (`CAS`) (`E2E-5`):**
     - **Camino B (`main` checked out limpio):** `integrate preview --to main --text` mostró `target checked out`, ruta exacta `/tmp/aeril-pa0d-live/repo`, ambos OIDs completos y `$ git -C /tmp/aeril-pa0d-live/repo merge --ff-only 7f984c3ae160f74d354045adc5267876cdf2c503`. Tras ejecutar exactamente ese comando, el re-preview devolvió `already_integrated`.
     - **Camino A (`destino` no checked out):** `integrate preview --to destino` devolvió `eligible_fast_forward` (`files = 2`); `integrate apply --confirm --expect 34e571cc8953916023f2ab1cba8103b59ab7422a` movió `refs/heads/destino` a `7f984c3ae160f74d354045adc5267876cdf2c503` vía CAS (`noop = false`), quedando `python3 -m unittest -v` (`3` tests) en `OK`.
  8. **Cierre y reapertura (`E2E-7`):** tras cerrar y reabrir CLI y TUI PTY `120×40` sin terminal adicional, `aeril attention --json` mostró `status = "done"`, `status_label = "integrado"`, `integration = "integration.completed"`, y la TUI renderizó el thread cerrado con su historial completo.

---

## 3. Ensayo separado de interrupción y recuperación (`E2E-6`)

En cumplimiento de [`docs/pa0-alpha-contract.md`](pa0-alpha-contract.md) §7 (separación obligatoria entre cierre normal y crash):

- Se inició un thread independiente (`/tmp/aeril-pa0d-recover`, `thread_id = e9d0fb494bb24ae48df1a9750fab27d6`, `run_id = ea04f01872fb406eafb7619f587762a8`) con `--backend hermes --model gemini:gemini-2.5-flash`.
- Al confirmarse `backend.spawned` se midieron por separado **`aeril` (`pid 2179214`, `8,036 KiB` RSS)** y el hijo real **`python3` / `hermes acp` (`pid 2179348`, `30,664 KiB` RSS al arranque)**, y se envió `SIGKILL` al supervisor `aeril`.
- Antes de recuperar, `aeril attention --json` clasificó `status = "recovery_required"`, `priority = 2`, `next_action = "aeril recover"`.
- Al ejecutar `aeril recover`, Aeril reportó `action = "run_interrupted"`, terminó el proceso hijo huérfano `hermes acp` (`backend terminado durante la recuperación (pid 2179348)`), cerró el run como `failed` sin fabricar `completed` ni `checkpoint.ready`, mantuvo `unsafe_state = false` tras el cierre confirmado del proceso hijo, y actualizó `attention` a `status = "failed"` / `next_action = "revisar fallo"`. No se presume reversibilidad de efectos externos previos al crash.

---

## 4. Fricciones reales detectadas y límites declarados

- **Errores de proveedor/modelo en `hermes acp v0.21.5` devueltos como turno normal:** al probar modelos no disponibles o sin cuota (`nous:stealth/space-bunny-alpha` → HTTP 404, `nous:anthropic/claude-sonnet-5.5` → HTTP 404 saldo `$0.00`, `opencode-go:qwen3.6-plus` → HTTP 400 modelo no incluido, y primera corrida `feedback send` con Gemini free tier → HTTP 429 límite 5 RPM), `hermes acp` emitió el diagnóstico en `agent.message` pero cerró el RPC `session/prompt` con `stopReason: "end_turn"`. Aeril no fabricó checkpoints ni ocultó el mensaje en el timeline, aunque `attention` clasificó el run terminado sin cambios como `"sin trabajo pendiente"`.
- **Artefactos binarios sin `.gitignore` (`__pycache__/*.pyc`):** cuando el repositorio Python carecía de `.gitignore` para `__pycache__/` y Hermes ejecutó `python3 -m unittest` en el worktree, la captura falló en modo cerrado (`archivo binario no soportado en snapshot v0: __pycache__/calc.cpython-314.pyc`). Se añadió la recomendación explícita en [`README.md`](../README.md) de mantener `.gitignore` configurado para caches/artefactos binarios (`__pycache__/`, `node_modules/`, `target/`), junto con `[--model <id>]` en [`src/cli.rs`](../src/cli.rs).
- **Alcance de la certificación:** `PA0-D` valida **una sesión Hermes autenticada por vez** en Linux `x86_64`. `P2-S2` (dos sesiones Hermes autenticadas concurrentes) permanece en **HOLD**, y la aceptación externa multiusuario queda sujeta al piloto **PA0-E** (`3–5` desarrolladores sin asistencia).
