# P2-S1 — Perfiles Hermes gestionados: implementación y evidencia

> **Estado final P2-S1 (R3):** código aceptado en
> `8051b325e821e148b7ed103367de7df184211ff4` por veredicto independiente
> **PASS**, sin P0/P1 reproducidos. Las remediaciones históricas están en
> [`phase2-s1-r1-remediation.md`](phase2-s1-r1-remediation.md),
> [`phase2-s1-r2-remediation.md`](phase2-s1-r2-remediation.md) y
> [`phase2-s1-r3-remediation.md`](phase2-s1-r3-remediation.md). En caso de
> discrepancia, R3 prevalece.
>
> **Provenance:** el SHA aceptado es el de código arriba. Este documento está
> siendo corregido en un commit **documentation-only** posterior a la
> aceptación; el HEAD documental no está cubierto directamente por la
> auditoría exacta de `8051b32` y requiere una confirmación estrecha de
> procedencia/delta antes de declararlo merge-ready.

**Rama:** `feat/phase2-s1-profiles` (desde `main` certificado `47f0318eaadded7b93c027ba42efe0f9778f0f87`)
**Feature flag:** `AERIL_HERMES_PROFILES` (default **OFF**)
**Estado:** código aceptado; PR **DRAFT**, pendiente de autorización humana explícita de merge
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
cargo test (suite completa, R3 aceptado): EXIT=0 — 204/204
  · 41 tests de la matriz R2+R3 (tests/phase2_s1_r2.rs)
  · 124 tests certificados de Fase 1 (sin regresiones)
  · tests de perfil P2-S1/R1
cargo clippy --all-targets -- -D warnings: limpio
cargo fmt: aplicado (sin diffs pendientes)
```

Evidencia final: R1 reproducer **7/7 GREEN**, reproducer R3 **5/5 GREEN**,
suite R3 **41/41 PASS** y suite completa **204/204 PASS**. Ver
[`docs/phase2-s1-r3-remediation.md`](phase2-s1-r3-remediation.md) §4–5 para
los logs y comandos exactos. Los tests usan Hermes fake; no acreditan
concurrencia autenticada real.

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
budget-size : binario 5,834,352 bytes (5.8 MB) ≤ 30 MB (decimal)  → PASS
budget-deps : PASS (solo ABI base del OS + zlib del sistema)
budget-mem  : idle 7.1 MB / thread abierto 8.0 MB / delta 0.96 MiB
              (límites 50 MB y 10 MiB)                            → PASS
budget-start: p95 19.73 ms (límite 300 ms)                        → PASS
budget-two  : TUI 8.65 MB · mocks 4.59/4.80 MB · combinado 18.25 MB
              arranque con 2 activos 50.57 ms · consultas max 5.83 ms
budget-scale: app 10.23 MB (≤500 MB)                               → PASS
```

**Budget idle 192 MiB (Hermes autenticado):** sin cambios en este trabajo.
P2-S1 no modifica el presupuesto ni la instrumentación; la medición con Hermes
real pertenece a P2-S0 (VIOLATION registrada) y a su re-baseline en P2-S2.

El release se reconstruyó explícitamente antes de medir. El target Makefile
`$(BIN)` no depende de las fuentes; hasta corregir esa dependencia, un
`make budget` sobre un binario release viejo no es evidencia válida.

---

## 5. Procedimiento de rollback

### 5.1 Esquema v4 y compatibilidad

**P2-S1 SÍ cambia el esquema SQLite.** La migración v4 agrega
`runs.profile TEXT` nullable. Los runs previos a v4 quedan con
`runs.profile=NULL`; el flag OFF no elimina la columna ni revierte la
migración. `Store::open` aplica v4 independientemente del flag.

- Un binario v3 **puede abrir** una base v4 (la columna es nullable y el
  binario v3 no la selecciona), pero **no entiende** la ownership de perfiles
  gestionados.
- El recovery v3 no reconoce `runs.profile`; si queda un backend gestionado
  vivo, v3 puede tratarlo como un backend no gestionado y terminar su grupo.
  **Recovery v3 con estado gestionado vivo es inseguro.**
- El binario v3 **no puede** enforcear las restricciones de downgrade
  descriptas abajo. No se le atribuye esa garantía.

### 5.2 Procedimiento de downgrade permitido

El downgrade de binario solo es admisible después de probar que no queda
estado de ejecución gestionado vivo:

1. Detener nuevas admisiones y `profile warm`.
2. Con el binario **v4**, reconciliar los runs interrumpidos
   (`aeril recover`) y, si corresponde, ejecutar la recuperación explícita de
   perfiles (`aeril profile recover --agent <id> --confirm`).
3. Verificar que no queden runs gestionados en `starting/running`, ni
   backends/descendientes vivos, ni locks de perfil tomados, ni evidencia de
   owner sin resolver. Verificar también tombstones `removing` y el registro
   `agents.json`.
4. Si cualquiera de esas condiciones no puede probarse, **abortar el
   downgrade**. No se debe reemplazar la incertidumbre por una suposición.
5. Recién entonces puede ejecutarse un binario v3. La restricción sigue siendo
   operativa: v3 no la enforcea por sí mismo.

Un `v3` que solo lee listados no demuestra ownership gestionado ni autoriza
recovery. La compatibilidad de lectura no es compatibilidad de recuperación.

### 5.3 Desactivar el flag no es hacer downgrade

Quitar `AERIL_HERMES_PROFILES` (o dejarlo en 0) devuelve el comportamiento de
ejecución al camino certificado, pero **no revierte** la migración v4 ni
borra perfiles/estado. El binario v4 sigue siendo el binario recomendado para
cualquier recovery o borrado de perfiles.

### 5.4 Perfiles: nunca borrado masivo

Un perfil contiene estado durable y potencialmente **credenciales de Hermes**
(`auth.json` dentro de `HERMES_HOME`). Reglas:

- No ejecutar `rm -rf <AERIL_DATA_DIR>/hermes` bajo ninguna circunstancia:
  destruiría credenciales y evidencia sin validación.
- Borrado **por perfil y explícito**, con autorización del usuario:
  `aeril profile remove --agent <id> --confirm` (requiere flag ON, verifica
  ownership, runs no-terminales, lock, procesos y symlinks; fail-closed).
- Si un perfil quedara en HOLD (run no reconciliado o proceso huérfano):
  reconciliar (`aeril recover`), terminar el proceso huérfano con identidad
  verificada y reintentar; o `profile recover --agent <id> --confirm`.
- La desactivación del flag **no implica** borrar perfiles.

### 5.5 Límites de seguridad/operación aceptados

**Manipulación same-UID del registro.** `.registry.lock` y los demás flocks
coordinan procesos de Aeril que participan del contrato. Un proceso externo
del mismo usuario que edite `agents.json` sin tomar el registry lock puede
bypassar ese contrato. Esto es una frontera de coordinación advisory, **no un
sandbox OS** ni una garantía contra un actor same-UID adversarial.

**Downgrade con estado vivo.** El recovery v3 no reconoce `runs.profile` y
no puede distinguir un backend gestionado. El rollback de estado gestionado
vivo está **prohibido**; solo se permite después de la verificación v4 de
§5.2. Si no se puede probar la ausencia de writers/owners/locks, se aborta.

Guarantees permitidos: operación single-Hermes gestionada con v4, flag ON/OFF,
migración v4 y recovery v4 con guards fail-closed. Guarantees **no** soportados:
recovery v3 sobre estado gestionado vivo, aislamiento de un proceso same-UID
que ignore advisory locks, y sandboxing OS.

---

## 6. Limitaciones conocidas y notas para el auditor

- **TOCTOU residual en C4:** entre la última re-verificación de inodo y
  `remove_dir_all` existe una ventana teórica que solo un actor con escritura en el
  directorio padre (0700, mismo usuario) podría explotar. `remove_dir_all` no sigue
  symlinks; el swap queda contenido. Hardening futuro: borrado recursivo por fd
  (`openat`/`unlinkat`). El resto de los checks son fail-closed.
- **`auth_status` de `status`** ejecuta `hermes auth status` (timeout 20 s
  configurable). En P2-S1 no hay fallback si Hermes no está disponible: `unknown`.
- **`--clear-threads`** solo cubre asociaciones históricas del registro. Los
  runs `starting/running` bloquean siempre; no existe bypass de runs activos.
  Lock, owner, procesos, symlinks y tombstones siguen fail-closed.
- **TUI sin cambios** (C1): el gate single-Hermes certificado permanece; la
  habilitación de N=2 autenticado es P2-S2.
- **Sin cambios en clasificación de `failed_turn`** (P2-S2).
- **P3 pendiente (test harness/reproducibilidad):** el reaping de PTY/TUI bajo
  interrupción/SIGKILL no está endurecido en este cierre; el target Makefile
  `$(BIN)` no depende de las fuentes, por lo que el rebuild release debe
  forzarse explícitamente para evidencia de presupuestos.
- **Hermes real no ejecutado en tests**: la suite usa un fake controlado; la
  validación con Hermes real autenticado corresponde a P2-S2 (y a la reutilización
  del harness P2-S0), respetando que los perfiles de P2-S0 no se tocan.

---

## 7. Checklist de auditoría independiente

1. `git diff 47f0318...8051b32` — revisar el subsistema de perfiles, el wiring
   de Runner/ACP, la migración `src/store/schema.rs`, los stores de runs/eventos
   y las suites R1/R2/R3. ¿Sin cambios en `src/tui/mod.rs`?
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
10. ¿El rollback v4 está documentado y el downgrade v3 solo se permite después
    de probar ausencia de writers/owners/locks (§5.2)? ¿Queda claro que flag
    OFF no revierte el esquema v4?

## 8. Commits de la rama

- **SHA de código aceptado:** `8051b325e821e148b7ed103367de7df184211ff4` —
  R3, PASS independiente.
- **Commit documentation-only de este cierre:** el HEAD posterior a este
  documento; no está cubierto directamente por la auditoría exacta de
  `8051b32`. Requiere confirmación estrecha de procedencia/delta antes de
  declarar merge-ready.

Base: `main @ 47f0318eaadded7b93c027ba42efe0f9778f0f87` (no modificado).

