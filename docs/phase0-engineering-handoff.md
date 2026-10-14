# Fase 0 — Handoff de ingeniería

> **Rama:** `feat/phase0-vertical-slice` (no mergeada a `main`).
> **Qué es:** el corte vertical completo del ensayo de viabilidad —
> editar → snapshot inmutable → revisar → comentar → feedback → reanudar tras
> crash → validar → integrar (fast-forward) — con presupuestos medidos.
> **Estado detallado:** ver `docs/phase0-status.md`.

## Cómo reproducir

```bash
# Compilar y testear (usa 2 jobs por la RAM de la laptop)
make build          # release
make test           # 44 tests: unitarios + gates A/B + bordes + regresión RC1
make clippy         # -D warnings, limpio

# Gate (los dos ya corren en make test, acá a mano):
cargo test --test test_a_retention      # Prueba A: retención de la revisión
cargo test --test test_b_crash          # Prueba B: crash en cada frontera

# Demo end-to-end con el backend mock (sin LLM):
SB=/tmp/demo-aeril && mkdir -p $SB/data
git init -q -b main $SB/repo && printf 'hola\n' > $SB/repo/NOTES.md
git -C $SB/repo add -A && git -C $SB/repo -c user.email=t@t -c user.name=t commit -qm base
export AERIL_DATA_DIR=$SB/data
B=target/release/aeril
cat > $SB/s1.json <<'EOF'
{"message":"edito","final_message":"listo","edits":[{"op":"write","path":"NOTES.md","content":"hola\nrevisado\n"}]}
EOF
TH=$($B thread new --repo $SB/repo --title demo | python3 -c 'import json,sys;print(json.load(sys.stdin)["thread_id"])')
$B run start --thread $TH --prompt "editá" --backend mock:$SB/s1.json

# Con Hermes real (verificado, requiere modelo con créditos/free del catálogo Nous):
#   $B thread new --repo <repo> --title real
#   $B run start --thread <id> --prompt "..." --model nous:poolside/laguna-s-2.1:free

# Presupuestos (requiere autorización para correr en la laptop):
make bench-seed     # fixtures 100 threads × 1000 eventos × 10 diffs
make budget         # size + deps + mem + start
make budget-scale   # reporta UNVERIFIED (sin claim de 5 agentes en Fase 0)
```

## Mapa del código

| Módulo | Responsabilidad |
|---|---|
| `src/store/` | SQLite v1 (WAL, `synchronous=FULL`, FKs): esquema, append transaccional con `RETURNING`, paginación. |
| `src/git/worktree.rs` | Workspaces verificados: rama propia por workspace, validación, recreación explícita registrada, nunca reutilización silenciosa de un workspace inválido. |
| `src/git/snapshot.rs` | Snapshot inmutable desde el working tree (statuses → `TreeUpdateBuilder`, sin tocar índice/rama), refs `refs/aeril/checkpoints/<id>` con CAS por OID, verificación de objetos. |
| `src/checkpoint.rs` | Protocolo `prepared→ready` + recuperación idempotente al reabrir (jamás desde el working tree actual). |
| `src/review.rs` | Comentarios anclados a snapshot (path/lado/rango/blobs/context hash); patch original persistido; estados open→sent→delivered→resolved. |
| `src/agent/acp.rs` | Cliente ACP (ndjson/stdio): initialize, session/new|load, prompt con streaming, `request_permission` (respuesta por `optionId`), cancel, `set_model`, kill. |
| `src/agent/mock.rs` | `aeril __mock-agent`: backend ACP simulado y determinista para tests (pide permisos y aplica ediciones de verdad). |
| `src/session.rs` | Orquestación de runs: base OID, workspace, eventos normalizados, feedback (`sent`/`delivered`/`failed`), validación por OID, integración FF. |
| `src/tui/` | TUI mínima: threads, timeline, patch, comentar (`c`), feedback (`f`), integrar (`i`), cancelar (`x`). |
| `src/bench.rs` + `scripts/bench/startup.py` | Fixtures sintéticas y medición externa del arranque (pty + beacon, tecla procesada). |
| `src/crash.rs` | Inyección de fallos determinista (`AERIL_CRASH_AFTER` / `AERIL_PAUSE_AFTER` + marcador). |

## Garantías y cómo se hacen cumplir

1. **El snapshot es el working tree real**, no HEAD: se construyen blobs del
   contenido en disco (incluye modificados sin commitear y archivos nuevos),
   sin tocar el índice ni la rama del humano.
2. **Un comentario ancla a contenido inmutable**: `snapshot_id` + path + lado
   + rango + blob OIDs + hash del fragmento. Reabrir el comentario lee del
   tree del snapshot; si el blob no coincide con el OID anclado,
   `blob_verified=false` se expone, no se oculta.
3. **`prepared` antes de publicar la ref y `ready` después de verificar**:
   cualquier caída en el medio se reconcilia al reabrir; una ref divergente
   detiene y queda `failed` terminal; objetos faltantes se reportan y
   **nunca** se reconstruye el snapshot desde el working tree actual.
4. **La entrega de feedback no se declara sin evidencia**: `feedback.sent` se
   escribe antes de lanzar el motor; `feedback.delivered` solo con la primera
   evidencia de procesamiento; sin ack queda `feedback.failed`.
5. **Integración sin force**: solo fast-forward, con expectativa de OID
   (`stale`), ancestría (`diverged`) y destino no chequeado; cualquier
   bloqueo se registra y preserva todo el trabajo.
6. **Validación atada al árbol exacto**: `validate` rechaza validar si el
   worktree no coincide con el tree del snapshot (OID a OID).

## Hallazgos de compatibilidad con Hermes (0.21.5)

- ACP ndjson/stdio funcionó sin sorpresas con el protocolo documentado en
  `docs/acp-hermes-spec.md` (initialize, session/new, prompt streaming,
  permission por `optionId`, `session/load`). La demo real completó el ciclo
  entero: edición → checkpoint → comentario → `feedback send` (reanudando la
  sesión del motor) → segundo checkpoint → borrado de worktree → retención →
  fast-forward.
- **El catálogo de modelos del portal Nous no siempre coincide con el modelo
  configurado en Hermes**: `stealth/space-bunny-alpha` da 404 y los modelos
  pagos requieren créditos. `--model` con un free del catálogo
  (`nous:poolside/laguna-s-2.1:free` respondió en ~1 s) resolvió la demo.
  El error del motor se muestra como mensaje del agente (nada se oculta).
- Quirks usados: `session/load` responde OK incluso para sesiones
  desconocidas (no distinguible — se registra `resumed=true` solo si el load
  salió bien, con esta salvedad anotada); `tool_call_update` no repite el
  título (se arrastra desde el `tool_call` inicial); permisos de edición
  llegan como request con diff embebido.
- Limitación de perfil: Hermes comparte `~/.hermes` entre procesos; en v0
  **no lanzar dos runs de Hermes en paralelo desde Aeril** (los tests usan el
  backend mock para concurrencia).

## Limitaciones conocidas (Fase 0)

- Un archivo textual por checkpoint; binarios, submódulos y directorios se
  rechazan explícitamente.
- Sin remapeo de comentarios a snapshots nuevos (operación aparte, futura).
- Sin recolector de refs (solo listado de candidatos conceptual).
- TUI mínima: sin splits, sin PTY embebido, sin navegador de archivos.
- La conversación persistida es de Aeril; los "thoughts" (`agent_thought_chunk`)
  no se persisten en v0 (se documenta; es una decisión de ruido, no una
  limitación técnica).
- `budget-scale` (5 agentes) sin verificación en Fase 0: no hay claim.

## Mediciones RC2 (make budget + budget-scale, 2026-10-13)

| Presupuesto | Límite | Medido | Veredicto |
|---|---|---|---|
| Arranque a UI usable que ABRE un thread (p95, n=30, externo) | 300 ms | **25.6 ms** (0 fallos) | ✅ PASS |
| RSS idle / con thread abierto | 50 MB | **6.7 / 7.5 MB** | ✅ PASS |
| Caché residente por thread (delta idle→abierto) | 10 MiB | **0.82 MiB** | ✅ PASS |
| Binario release stripped | 30 MB | **5.5 MB** | ✅ PASS |
| 5 runs mock: app | ≤ 500 MB | **8.7 MB** (agentes 16.2 / total 24.9 aparte) | ✅ PASS |
| Cola ACP en vuelo (presupuesto 256 KiB en test) | ≤ presupuesto | **peak 241 KB**, deltas RSS planos frente al volumen | ✅ PASS |

Detalle pre/post de B1 en `docs/phase0-rc2-acceptance.md` §1. **Hermes real ×5
sigue sin medirse** (perfil compartido): el bench de escala usa mocks y lo
declara.

## Mediciones RC1 (histórico, make budget 2026-10-13)

**Declaración del benchmark (RC1 §4):**
- OS/arch: Linux x86_64 · CPU: Intel i7-8550U · RAM: 15.4 GiB · Disco: NVMe `/dev/nvme0n1p5` (fstype **ext4**, reportado por el harness).
- Dataset: fixtures 100 threads × 1.000 eventos × 10 diffs **+ 10 comentarios por thread** (336.9 MB, seed 19.8 s).
- Build: release (`lto=thin`, `strip=symbols`, `codegen-units=1`).
- Condición de caché: **warm** declarada (no se vació la caché del SO).
- Endpoint de arranque: el proceso externo mide hasta que la entrada (Enter)
  **abre un thread real** (timeline + últimos 100 eventos + resúmenes de
  comentarios). Un fallo cualquiera ⇒ el gate falla (`n_fail == 0`).

| Presupuesto | Límite | Medido | Veredicto |
|---|---|---|---|
| Arranque a UI usable que ABRE un thread (p95, n=30, externo) | 300 ms | **25.2 ms** (p50 21.7 / max 89.3; 30/30 ok) | ✅ PASS |
| RSS idle (5 s) | 50 MB | **6.4 MB** | ✅ PASS |
| **Caché residente por thread** (RSS idle → thread abierto) | 10 MiB | **0.8 MiB** | ✅ PASS |
| Binario release stripped | 30 MB (decimal) | **5.4 MB** | ✅ PASS |
| Dependencias de runtime (`ldd`) | 0 de app | ABI del OS + libz del sistema (declarada) | ✅ PASS |
| **5 runs concurrentes (app)** | ≤ 500 MB | **9.7 MB** (`aeril bench scale`, 5 agentes mock) | ✅ PASS (app) |
| Agentes y total con 5 runs | medidos aparte | agentes **21.4 MB**, total **31.1 MB** | informativo |

**Advertencia explícita:** el bench de escala usa agentes MOCK y caracteriza el
overhead de la aplicación. **No prueba** el consumo de Hermes real: los
procesos de Hermes comparten `~/.hermes` y no es medible de forma aislada en
v0 (`make budget-scale` imprime esta nota).

Salidas crudas: `bench-data/{seed,start,mem,scale}.json` (regenerables con
`make bench-seed budget budget-scale`).

## Remediación RC1

Los 13 hallazgos del brief de auditoría (A1–A6, B1–B7) están mapeados a
correcciones, tests y commits en **`docs/phase0-remediation-matrix.md`**
(Wave A `4e81f7d`, Wave B `dfd9416`). Los gates A/B originales no regresaron.
La segunda auditoría (3 blockers: amplificación ACP, lease con dueño vivo,
gitlinks no inicializados) se remedió en **`docs/phase0-rc2-acceptance.md`**
(rama `fix/phase0-reliability-rc2`, implementación `11fd0d7`).

## Próxima prioridad de ingeniería

**Fase 1 — corte vertical útil:** dos agentes con workspaces verificados en
paralelo y review durable (ya disponible), más la reconstrucción de
workspace desde snapshot para resume sin worktree (hoy se recrea desde el
último snapshot en feedback; falta el caso general de "resume sin checkout").
Antes de ampliar backend/targets: medir primero (regla del manifiesto).
