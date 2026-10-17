# P2-S1 — Perfiles Hermes gestionados: implementación y evidencia

**Rama:** `feat/phase2-s1-profiles` (desde `main` certificado `47f0318eaadded7b93c027ba42efe0f9778f0f87`)
**Feature flag:** `AERIL_HERMES_PROFILES` (default **OFF**)
**Estado:** implementación completa, pendiente de auditoría independiente
**Restricciones respetadas:** sin merge a `main`, sin tags, sin P2-S2, sin multi-Hermes en
producto, sin credenciales reales, sin tocar perfiles autenticados de P2-S0.

---

## 1. Resumen de implementación

### Módulo nuevo: `src/profile.rs`

Layout por perfil (`<AERIL_DATA_DIR>/hermes/<agent_id>/`):

```text
<agent_id>/
  home/          HERMES_HOME (estado Hermes completo)
  san-home/      HOME sanitizado (XDG_* y GH_CONFIG_DIR derivan de acá)
  profile.json   metadata de Aeril, 0600 — NUNCA credenciales
  aeril.lock     flock de escritor único (retenido todo el writer ACP)
agents.json      registro agent_id→perfil y thread→perfil, 0600
.registry.lock   flock del registro
```

Interfaces implementadas (CLI `aeril profile …`, todas requieren flag ON):

| Comando | Semántica |
|---|---|
| `profile create --agent <id> [--provider P] [--model M] [--warm]` | Idempotente; repara create interrumpido solo si el dir está registrado, vacío o tiene metadata propia; nunca adopta dirs ajenos; rechaza symlinks/fuera del root |
| `profile list` | Estados `ok \| incomplete \| missing \| orphan`; los huérfanos se reportan, jamás se adoptan |
| `profile status --agent <id>` | Verifica auth **sin leer credenciales** (`hermes auth status <provider>` con entorno sanitizado) y reporta lock/disco |
| `profile warm --agent <id>` | Pre-warm acotado (`hermes acp` + initialize) con timeout duro y progreso; retiene el lock |
| `profile assign --agent <id> --thread <t>` / `unassign --thread <t>` | Mapeo estable thread→perfil; unicidad por thread |
| `profile remove --agent <id> --confirm [--clear-threads]` | Borrado explícito y verificado (ownership, locks, procesos, symlinks, mountpoints) |

### Wiring del Runner (`src/session.rs` + `src/agent/acp.rs`)

- Con flag **ON** y backend `hermes`: el thread **debe** tener perfil asignado y
  `auth_status == logged_in`; se toma el `flock` del perfil **antes** de spawnear y se
  retiene hasta el final del run; el spawn usa entorno sanitizado del perfil
  (`HERMES_HOME`, `HOME`, `XDG_*`, `GH_CONFIG_DIR`, `PATH` mínimo + dir de hermes,
  binario absoluto). **Sin fallback** al perfil del host.
- Con flag **OFF**: el camino certificado queda intacto (sin lookup, sin lock, sin env).
- `BackendSpec` gana `env: Vec<(String, String)>` (vacío = heredar, comportamiento previo).
- Backends no-hermes (mock) no tocan el subsistema de perfiles ni con flag ON.

### Correcciones de arquitectura obligatorias

- **C1 — sin concurrencia Hermes autenticada:** la TUI **no fue modificada**; el gate
  single-Hermes certificado sigue vigente. El flag habilita solo gestión + uso
  single-run de un perfil; dos escritores sobre el mismo perfil se rechazan por
  `flock` (evidencia T9/T14). La concurrencia autenticada queda para P2-S2.
- **C2 — crash consistency:** `agents.json`/`profile.json` con escritura atómica
  (tmp + fsync + rename + fsync del dir) bajo `.registry.lock` (flock, liberado por
  el kernel al morir el proceso ⇒ sin locks stale); create/assign/remove idempotentes
  y reconciliación de estados parciales sin adopción silenciosa (T3, T11, T12).
- **C3 — frontera de autenticación:** Aeril nunca abre/parsea/copia `auth.json`
  (no hay ninguna referencia en `src/profile.rs`); el estado de auth se lee por la
  CLI de Hermes o el handshake ACP. Acceso de Hermes a su propio store: auditado
  aparte (T6 verifica que Aeril ni siquiera materializa el archivo).
- **C4 — borrado seguro:** verificación de symlink/realpath/ownership/mountpoint,
  lock de escritor, escaneo de `/proc/*/environ` por `HERMES_HOME` del perfil,
  re-verificación de inodo inmediatamente antes de `remove_dir_all` (que no sigue
  symlinks), y limpieza del registro bajo lock. Fail-closed ante cualquier anomalía
  (T10). Limitación residual documentada en §6.

---

## 2. Tests

### Suite nueva: `tests/phase2_s1_profiles.rs` (15 tests)

Hermes fake controlado por el auditor (script Python en PATH propio; auth/modos
configurados por archivos dentro de `HERMES_HOME`). Sin credenciales reales.

| Test | Cubre |
|---|---|
| T1 | Flag OFF rechaza gestión y no crea nada |
| T2 | Layout, permisos 0700/0600, metadata-only (sin tokens) |
| T3 | Create idempotente + reparación de create interrumpido + no-adopción |
| T4 | `list`: estados y huérfanos sin adopción |
| T5 | `assign`/`unassign`: unicidad y estabilidad del mapeo |
| T6 | Auth status sin leer/materializar credenciales |
| T7 | Warm OK: `warmed_ms` + entorno sanitizado verificado por el fake |
| T8 | Warm timeout acotado (1 s), lock liberado, perfil no corrompido |
| T9 | Contención de `flock` (warm lento): status/warm/remove fail-closed; liberación |
| T10 | Guardas de remove: `--confirm`, threads, symlink swap |
| T11 | Remove interrumpido (dir ausente) → reconciliación idempotente |
| T12 | Concurrencia del registro (3+3 creates en paralelo, sin corrupción) |
| T13 | Runner fail-closed: sin asignación / sin auth verificada |
| T14 | Runner: env sanitizado + lock retenido todo el writer + release por muerte |
| T15 | Rollback: flag OFF preserva comportamiento certificado (hermes fake y mock) |

### Resultados

```text
cargo test (suite completa): EXIT=0 — 142/142
  · 124 tests certificados de Fase 1 (sin regresiones)
  · 3 unit tests nuevos (src/profile.rs)
  · 15 tests de integración nuevos (tests/phase2_s1_profiles.rs)
cargo clippy --all-targets -- -D warnings: limpio
cargo fmt: aplicado (sin diffs pendientes)
```

Evidencia por test: todos los `t1..t15` en verde (ver §3 para el detalle de
lock/crash). Los tests corren en ~3.4 s en total (deterministas, sin sleeps
largos: el timeout de warm es de 1 s configurable).

### Unit tests (`src/profile.rs`)

3 tests: validación de `agent_id`, roundtrip del registro, parsing del flag.

---

## 3. Evidencia de locks y crash/recovery

- **Lock de escritor único:** `flock(LOCK_EX|LOCK_NB)` sobre `aeril.lock`, tomado por
  `warm` y por el Runner antes del spawn y retenido hasta el final del writer
  (T9, T14). `is_held` es una prueba sin efectos colaterales (no crea archivos).
- **Lock del registro:** `flock(LOCK_EX)` bloqueante en `.registry.lock` para toda
  mutación; T12 lanza creates concurrentes y verifica JSON válido + entradas exactas.
- **Sin locks stale:** flock se libera al morir el proceso; T14 mata el run con
  SIGKILL/SIGTERM y verifica que el lock se libera y que `remove` completa después.
- **Crash consistency:** T11 simula remove interrumpido (dir borrado, registro
  intacto) ⇒ `list` reporta `missing` y `remove` limpia el registro de forma
  idempotente. T3 simula create interrumpido (sin `profile.json`) ⇒ `incomplete`
  y reparación explícita.
- **Fail-closed:** sin perfil asignado, sin auth, lock ocupado, symlink, dir ajeno
  no vacío, thread asignado sin `--clear-threads` ⇒ error explícito, sin fallback.

---

## 4. Presupuestos de performance

```text
budget-size : binario 5,553,296 bytes (5.6 MB) ≤ 30 MB (decimal)  → PASS
budget-deps : PASS (solo ABI base del OS + zlib del sistema)
budget-mem  : idle 6.3 MB / thread abierto 7.1 MB / delta 0.86 MiB
              (límites 50 MB y 10 MiB)                            → PASS
budget-start: p95 11.67 ms (límite 300 ms)                        → PASS
budget-two  : TUI 8.2 MB · mocks 4.0 MB c/u · combinado 16.6 MB
              arranque con 2 activos 50.9 ms · consultas avg 3.1 ms
              (Fase 1 certificó 8.1–8.3 MB / 4.0 MB)              → sin regresión
```

**Budget idle 192 MiB (Hermes autenticado):** sin cambios en este trabajo.
P2-S1 no modifica el presupuesto ni la instrumentación; la medición con Hermes
real pertenece a P2-S0 (VIOLATION registrada) y a su re-baseline en P2-S2.

---

## 5. Procedimiento de rollback

1. **Desactivar:** quitar `AERIL_HERMES_PROFILES` del entorno (o dejarlo en 0). El
   comportamiento vuelve al certificado en `main @ 47f0318`: los comandos
   `aeril profile *` se rechazan y el Runner no consulta perfiles (T1, T15).
2. **Sin migración de datos:** el subsistema es metadata fuera de la DB
   (`<data_dir>/hermes/`); no hay cambios de esquema. Rollback de código no
   requiere tocar la DB ni los worktrees.
3. **Borrar artefactos (opcional):** `rm -rf <AERIL_DATA_DIR>/hermes` con Aeril
   detenido (o `aeril profile remove --agent <id> --confirm --clear-threads`).
   Los perfiles creados en P2-S1 son descartables y no contienen credenciales de
   producción (la autenticación real requiere device-code del operador).
4. **Reversión total:** `git revert` del merge del PR (o abandono de la rama).
   `main` no fue modificado por esta rama.

---

## 6. Limitaciones conocidas y notas para el auditor

- **TOCTOU residual en C4:** entre la última re-verificación de inodo y
  `remove_dir_all` existe una ventana teórica que solo un actor con escritura en el
  directorio padre (0700, mismo usuario) podría explotar. `remove_dir_all` no sigue
  symlinks; el swap queda contenido. Hardening futuro: borrado recursivo por fd
  (`openat`/`unlinkat`). El resto de los checks son fail-closed.
- **`auth_status` de `status`** ejecuta `hermes auth status` (timeout 20 s
  configurable). En P2-S1 no hay fallback si Hermes no está disponible: `unknown`.
- **`--clear-threads`** omite el chequeo de runs activos en la DB: es una
  declaración explícita del operador de que las referencias son históricas; los
  checks de lock/procesos/symlinks siguen fail-closed.
- **TUI sin cambios** (C1): el gate single-Hermes certificado permanece; la
  habilitación de N=2 autenticado es P2-S2.
- **Sin cambios en clasificación de `failed_turn`** (P2-S2).
- **Hermes real no ejecutado en tests**: la suite usa un fake controlado; la
  validación con Hermes real autenticado corresponde a P2-S2 (y a la reutilización
  del harness P2-S0), respetando que los perfiles de P2-S0 no se tocan.

---

## 7. Checklist de auditoría independiente

1. `git diff main...feat/phase2-s1-profiles` — ¿el diff toca solo
   `src/profile.rs`, `src/cli.rs`, `src/lib.rs`, `src/session.rs`,
   `src/agent/{mod,acp}.rs`, `tests/rc2_b1.rs`, `tests/phase2_s1_profiles.rs`,
   `docs/phase2-s1-implementation.md`? ¿Sin cambios en `src/tui/mod.rs`?
2. ¿`main` intacto en `47f0318` y sin tags nuevos?
3. ¿Flag OFF ⇒ T1/T15 pasan y el código no consulta perfiles?
4. ¿Ninguna referencia de **acceso** a `auth.json` en el diff (solo comentarios que
   documentan la prohibición)? ¿`profile.json` metadata-only?
5. ¿Escrituras atómicas + fsync + locks en toda mutación? ¿Idempotencia y
   reconciliación sin adopción silenciosa?
6. ¿`flock` retenido durante todo el writer ACP y liberado por muerte del proceso?
7. ¿Borrado fail-closed ante symlink/mountpoint/proceso/lock/ownership/threads?
8. ¿Sin credenciales reales en tests, docs ni artefactos? ¿Perfiles P2-S0 intactos?
9. ¿Suite completa + fmt + clippy + presupuestos verdes? ¿Sin cambios al budget
   de 192 MiB idle?
10. ¿Rollback probado conceptualmente (flag OFF) y documentado?

## 8. Commits de la rama

- `b5eba7176f4a403ab6efb89f98913c51d1a65896` — implementación + tests + doc
  (`feat(p2s1): perfiles Hermes gestionados (feature flag OFF por default)`)
- segundo commit (docs-only): registro de estos SHAs (tip de la rama al cierre)

Base: `main @ 47f0318eaadded7b93c027ba42efe0f9778f0f87` (no modificado).

