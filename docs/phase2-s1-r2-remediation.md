# P2-S1 R2 — Remediación adversarial cross-component

**Rama:** `feat/phase2-s1-profiles`
**Baseline R1:** `f1a76350d25ce6254747fda7323635052c8b8dc0`
**PR:** #1 — DRAFT, sin merge
**`main` certificado:** `47f0318eaadded7b93c027ba42efe0f9778f0f87`
**Veredicto de entrada:** HOLD independiente — 5 P1 + 7 P2
**Estado de este documento:** entrega para re-auditoría; **no** declara aceptación.

No se usaron credenciales reales, perfiles P2-S0 autenticados, OAuth, tags,
merge ni cambios a `main`. Todos los fixtures son descartables.

---

## 1. Matriz de causa raíz

| ID | Causa raíz verificada | Invariante restaurado |
|---|---|---|
| P1-A | La validación final thread→perfil ocurría antes de `runs::start` sin retener el lock del registro; `assign` podía cambiar el mapping en la ventana. | La admisión se lineariza en el commit de `runs::start` con profile lock + registry lock retenidos; mapping, `runs.profile`, credenciales y env son el mismo perfil. |
| P1-B | `agents.json` ausente se interpretaba como registro vacío; runs legacy `profile=NULL` no se contaban; un directorio no registrado podía adoptarse. | Evidencia incompleta ⇒ HOLD. Un registro ausente con estado previo no es un registro vacío; un run legacy no-terminal bloquea borrados; un huérfano no se adopta. |
| P1-C | `recovery` consultaba el flag de entorno actual para decidir si un run era gestionado. | El modo histórico sale de `runs.profile` durable, no del entorno. Flag OFF no termina un survivor gestionado. |
| P1-D | Se validaba el directorio del perfil, pero no el root ni `home`/`san-home`/locks/metadata; varios `open` seguían symlinks intermedios. | Root/directorios reales, `O_NOFOLLOW` en archivos de control, lecturas/escrituras no-follow, componentes verificados y borrado solo sobre un directorio real. |
| P1-E | La salida de auth se leía después de que el proceso padre terminaba, secuencialmente y sin deadline compartido; el truncamiento y stderr se ignoraban. | `logged_in` exige captura completa, no truncada, de ambos pipes, provider exacto, sin contradicción y exit 0; deadline absoluto y process group acotado. |
| H1 | El validador estricto solo protegía `profile warm`; el Runner aceptaba cualquier `initialize`. | Contrato compartido `validate_initialize_result`; los runs gestionados lo aplican antes de abrir sesión. |
| H2 | La limpieza verificaba solo el PGID original; un nieto con `setsid` podía escapar. Un fallo al escribir `owner.json` se ignoraba. | Escaneo del perfil después de terminar el grupo; `owner.json` obligatorio o aborto del spawn; evidencia retenida si hay incertidumbre. |
| H3 | `remove` no evaluaba `owner.json`. | `remove` toma el profile lock, evalúa owner y procesos, y rechaza owner vivo/corrupto/no verificable. |
| H4 | La versión se leía fuera de la transacción de migración; dos openers podían intentar el mismo `ALTER TABLE`. | Determinación + DDL bajo transacción IMMEDIATE, check de columna y fast path de solo lectura para v4. |
| H5 | Un borrado parcial no dejaba intención durable y no era reanudable. | Tombstone `removing` durable; `create`/assign/unassign rechazan el perfil; `remove` reanuda con todos los guards. |
| H6 | El perfil no aparecía en `run list`, JSONL, Markdown ni `run.started`. | `runs.profile` se propaga a superficies de lectura y al payload versionado con `serde(default)`. |
| H7 | `agentCapabilities` era opcional en el validador. | El resultado de initialize exige `agentCapabilities` objeto y `authMethods` no vacío, compartido por warm y Runner. |
| P3 | `disk_kb` seguía symlinks; el repro podía dar GREEN falso si solo fallaba uno de warm/run; la limpieza de tests señalaba PIDs sin identidad. | `disk_kb` no atraviesa symlinks; repro exige ambos rechazos; cleanup verifica starttime + cmdline antes de señalar. |

---

## 2. Punto de linearización y modelo de locks

### Admisión de un run gestionado

`Runner::attach_profile` ejecuta, en este orden:

1. toma el **profile writer lock** `P` (`aeril.lock`);
2. verifica que no haya survivor del perfil;
3. ejecuta auth bajo `P` (lento, pero **no** bajo el lock global del registro);
4. toma el **registry lock** `R`;
5. relee thread→profile;
6. llama `runs::start`, que inserta `runs.profile` y `run.started` en una
   transacción SQLite `S`;
7. libera `R`; conserva `P` durante todo el run.

La linearización es el **commit de `runs::start`**. En ese punto están fijados
simultáneamente:

- el mapping del registro;
- el profile lock retenido;
- el `HERMES_HOME`/HOME/XDG/GH env elegido;
- `runs.profile` durable;
- la sesión/credenciales del perfil seleccionado.

Si un `assign`/`unassign` cambia el mapping antes del paso 5, la admisión
falla. Si lo intenta después del paso 4, espera en `R`; cuando entra, el run
ya es `starting` y el guard de runs no-terminales rechaza el cambio.

Un fallo antes de `runs::start` libera ambos locks y no deja run. Un SIGKILL
antes del commit deja solo locks del kernel, que se liberan al morir el
proceso; no hay fila durable que afirme una admisión.

### Orden global

```text
workspace lease (L)  [solo Runner]
        │
        ▼
profile writer P  ──────────────► meta M
        │
        ▼
registry R ─────────────────────► meta M
        │
        ▼
SQLite write S
```

Formalmente: **L → P → R → M → S**. Las operaciones toman un prefijo o
subconjunto en ese orden:

- `Runner`: `L → P → R → S` (auth usa `M` bajo `P`, antes de `R`);
- `profile remove`: `P → R → S`;
- `profile assign/unassign`: `R → S`;
- `profile create`: `R → M`; si hace warm, libera `R` y toma `P → M`;
- `profile warm`: `P → M`;
- `profile status/auth`: `M`;
- `profile list`: sin lock retenido;
- `worktree rm`: `S` (Phase 1, sin cambios).

No existe un camino `R → P`, `S → R`, `M → P` ni `S → L`. La migración v4
corre bajo `S` cuando `Store::open` se invoca desde `assign`/`remove`; no
adquiere `P`/`R` después de `S`.

### Búsqueda de deadlocks

Se revisaron create/assign/unassign/remove/recover/status/warm/admission. El
único punto que tomaba `P` y después esperaba `R` era la admisión; se fijó el
orden `P → R`. `remove` fue reordenado para tomar `P` antes de `R` (antes
tomaba `R` y después `P`). El test `r2_p1a_*` mantiene una admisión detenida
en el barrier mientras un `assign` compite; el resultado es rechazo/deferred,
no deadlock.

---

## 3. Cambios por hallazgo

### P1-A — asignación/admisión

- `ProfileAttachment` conserva el `File` del registry lock.
- `Runner` libera `R` solamente después del commit de `runs::start`, y
  conserva `P` hasta el final del run.
- Auth se ejecuta bajo `P`, no bajo `R`.
- `remove` toma `P` antes de `R`; assignment usa `R → S`.
- Test barrier debug-only: `AERIL_TEST_PROFILE_ADMISSION_BARRIER`.

### P1-B — registro y ownership legacy

- `RegistryState::{Present, MissingEmpty, MissingUnknown}`.
- `load_registry_strict`: destructive/assignment paths fallan con
  `MissingUnknown`.
- `count_legacy_non_terminal_runs`: cualquier run `starting/running` con
  `runs.profile=NULL` bloquea borrados de perfiles hasta reconciliación.
- `create` no adopta un directorio no registrado no vacío.
- `remove` no borra un directorio huérfano no registrado.

### P1-C — recovery durable

- `recovery::backend_state` decide por `run.profile.is_some()`; el flag de
  entorno ya no participa.
- Runs sin perfil conservan el contrato Phase 1 de terminación verificada.
- Runs gestionados quedan `unsafe`/HOLD y requieren operador.

### P1-D — contención de filesystem

- `safe_root` rechaza root symlink/no-directorio y resuelve el root canónico.
- `ensure_real_dir`/`require_real_dir` no usan `create_dir_all` para
  componentes de ownership.
- Lecturas de `profile.json`, `owner.json` y `agents.json` usan
  `symlink_metadata` + `O_NOFOLLOW`.
- `aeril.lock`, `.meta.lock`, `.registry.lock` se abren con `O_NOFOLLOW`.
- `write_atomic` exige padre real y rechaza target symlink.
- `assert_profile_components_safe` cubre home, san-home, metadata y locks.
- `disk_kb` ignora symlinks.

**Límite residual:** un proceso same-UID malicioso todavía puede ganar una
carrera de rename/swap entre la validación de inodo y `remove_dir_all`. No se
implementó `openat2(RESOLVE_BENEATH)` ni borrado fd-relative; se documenta
como límite del threat model, no como garantía de sandbox OS. El perfil root
es 0700 y los componentes se validan dos veces (antes y después del lock).

### P1-E — auth proof acotado

- `capture_auth_output` hace `poll` de stdout+stderr con un deadline
  compartido y cotas de 64 KiB por stream.
- Truncamiento de stdout/stderr ⇒ `unknown`; nunca `logged_in`.
- stderr con estado/negación del provider ⇒ contradicción ⇒ `unknown`.
- Exit no cero, timeout, EOF prematuro o identidad no verificable ⇒
  `unknown` fresco persistido.
- Spawn con `process_group(0)`; timeout termina el grupo.
- No se imprime ni persiste la salida capturada.

### H1/H7 — initialize compartido

- `validate_initialize_result` exige JSON-RPC 2.0 (en `classify`), id
  correcto, `protocolVersion=1`, `agentCapabilities` objeto, `authMethods`
  no vacío con `id` string y `agentInfo` objeto si está.
- `BackendSpec.strict_initialize` lo activa en el Runner gestionado;
  `profile warm` usa el mismo validador.
- Flag OFF conserva el camino Phase 1.

### H2 — descendants y evidencia de dueño

- `warm` exige que `owner.json` se persista; si falla, termina/verifica el
  backend y falla el warm.
- Tras terminar el PGID, escanea el perfil; un nieto con `setsid` que conserva
  `HERMES_HOME` impide declarar limpieza y conserva `owner.json`.
- `verify_no_survivor` limpia owner stale **solo** después de owner checks y
  process scan limpios.

### H3/H5 — borrado

- `remove` evalúa `owner.json` bajo el profile lock.
- Tombstone `RegistryEntry.removing` durable antes de borrar archivos.
- `remove` reanudable; `create`/assign/unassign rechazan el tombstone.
- El borrado vuelve a ejecutar run guards, owner, procesos, path, inodo y
  mountpoint checks en cada intento.

### H4 — migración

- Fast path de solo lectura si `user_version >= 4` y `runs.profile` existe.
- Migración con `IMMEDIATE`; `user_version` se relee dentro de la transacción.
- `column_exists` evita fallar si el DDL v4 quedó aplicado con versión atrasada.
- v1/v2/v3 → v4, 24 openers concurrentes y DDL parcial testeados.

### H6 — auditoría exportable

- `runs.profile` en `RunRow`.
- `profile` en `RunStarted` (serde default, compatible con eventos viejos).
- `run list`, JSONL y Markdown muestran el perfil.

---

## 4. Migración y downgrade

La migración v4 es un `ALTER TABLE runs ADD COLUMN profile TEXT` nullable. No
reescribe filas; los runs legacy quedan `NULL`. `Store::open` migra aunque el
flag esté OFF. El fast path no toma lock de escritura en DBs ya v4, evitando
`database is locked` bajo lectores concurrentes.

**Compatibilidad de lectura con v3:** un binario v3 de `main` (`47f0318`) abrió
una DB v4 y pudo ejecutar `thread list` y `run list` correctamente. La columna
`profile` existe, `user_version=4`, y el run legado quedó con `profile=NULL`.

**Downgrade operativo:** NO se declara seguro. Un binario v3 no conoce
`runs.profile` y su recovery puede terminar un survivor gestionado. Antes de
ejecutar un binario v3:

1. detener/verificar todos los writers Hermes gestionados;
2. ejecutar `aeril recover`/`profile recover` con el binario v4;
3. confirmar que no quedan runs `starting/running` gestionados ni survivors;
4. no usar v3 para recovery mientras exista estado gestionado no resuelto.

Un binario v3 no puede ser corregido retroactivamente por este cambio. No se
afirma rollback compatible para sesiones gestionadas vivas.

---

## 5. Evidencia RED/GREEN

- `docs/phase2-s1-r2-evidence/red-baseline-f1a7635.txt`: suite R2 contra R1;
  **10/12 escenarios RED** (los dos restantes ya estaban protegidos por el
  profile lock o por el parseo de registro corrupto).
- `docs/phase2-s1-r2-evidence/green-candidate.txt`: **29/29 R2 PASS**.
- `docs/phase2-s1-r2-evidence/full-suite.txt`: **192/192 Cargo PASS**,
  incluida la suite Phase 1 y los 3 tests de seguridad del bench.
- `docs/phase2-s1-r2-evidence/r1-repro-green.txt`: `r1_repro.py` **7/7 GREEN**.
- `docs/phase2-s1-r2-evidence/budget-main.txt`,
  `budget-scale.txt`, `budget-two.txt`.

La suite R1 no se reescribió para hacerla pasar: los cambios de producción y
los tests R2 son aditivos; los 15 tests R1 siguen verdes.

---

## 6. Tests R2 y aserciones

| Test | Aserción principal |
|---|---|
| `r2_p1a_reassignment_during_final_mapping_check_is_consistent` | mapping del registro y `runs.profile` nunca divergen; un assign concurrente es rechazado/bloqueado. |
| `r2_p1a_profile_deletion_during_admission_is_blocked` | un remove del perfil durante la admisión no borra el perfil. |
| `r2_p1a_reassignment_during_slow_auth_fails_closed` | auth lenta + reassign ⇒ admisión falla, no se crea run viejo. |
| `r2_p1b_missing_registry_blocks_destructive_removal` | registro ausente con estado ⇒ remove HOLD. |
| `r2_p1b_legacy_null_run_blocks_profile_deletion` | run no-terminal `profile=NULL` bloquea borrado. |
| `r2_p1b_corrupt_registry_is_fail_closed` | registry corrupto no permite borrar. |
| `r2_p1c_flag_off_recovery_does_not_kill_managed_survivor` | flag OFF no termina survivor con `runs.profile`. |
| `r2_p1d_symlinked_root_is_rejected` | root symlink no crea perfil externo. |
| `r2_p1d_symlinked_home_and_san_home_are_rejected` | home/san-home symlink no se siguen ni se chmodifican. |
| `r2_p1d_symlinked_lock_files_are_rejected` | `aeril.lock` symlink fail-closed. |
| `r2_p1d_registry_lock_symlink_is_rejected` | `.registry.lock` symlink fail-closed. |
| `r2_p1d_meta_lock_symlink_is_rejected` | `.meta.lock` symlink fail-closed. |
| `r2_p1d_owner_symlink_is_rejected` | `owner.json` symlink no se sigue en remove. |
| `r2_p1d_agents_json_symlink_is_rejected` | `agents.json` symlink no se sigue. |
| `r2_p1d_symlinked_metadata_is_rejected` | `profile.json` symlink no se sigue. |
| `r2_p1e_truncated_or_contradictory_auth_is_unknown` | truncamiento/flood/contradicción stderr ⇒ no `logged_in`. |
| `r2_p1e_descendant_holding_stdout_cannot_extend_auth_timeout` | deadline compartido; no hang por pipe retenido. |
| `r2_h1_h7_managed_runs_require_strict_initialize` | result null/string/sin capabilities/sin protocol ⇒ run `failed`, sin checkpoint. |
| `r2_h7_warm_requires_agent_capabilities` | warm rechaza `agentCapabilities` ausente. |
| `r2_h2_escaped_grandchild_is_not_reported_clean` | nieto con `setsid` no permite declarar limpieza; owner/lock retenidos. |
| `r2_h2_owner_write_failure_aborts_spawn` | fallo de owner.json aborta warm. |
| `r2_h3_remove_honors_live_and_corrupt_owner_records` | owner vivo/corrupto bloquea remove. |
| `r2_h4_concurrent_migration_is_serialized` | 24 openers concurrentes migran v3→v4 sin error. |
| `r2_h4_interrupted_v4_ddl_is_idempotent` | v1/v2/v3 y DDL v4 parcial convergen a v4. |
| `r2_h5_partial_removal_is_idempotent_and_resumable` | tombstone permite reanudar sin metadata. |
| `r2_h5_partial_removal_resumes_after_owner_recovery` | partial + owner vivo ⇒ HOLD; tras recuperar owner, remove completa. |
| `r2_h5_removing_profile_cannot_be_recreated_or_reassigned` | tombstone bloquea create/assign. |
| `r2_h6_run_list_and_exports_include_profile` | `run list`, JSONL, Markdown y resumen incluyen perfil. |
| `r2_p3_disk_kb_does_not_follow_symlinks` | accounting no atraviesa symlink anidado. |

---

## 7. Regresión y presupuestos (release, binario final)

Host: Intel i7-8550U, 15.4 GB, ext4, build release LTO thin/strip.

| Gate | Resultado |
|---|---|
| Cargo suite | **192/192 PASS** |
| R1 repro | **7/7 GREEN** |
| `cargo fmt --check` | PASS |
| `cargo clippy --all-targets -- -D warnings` | PASS |
| Binario | 5,788,336 B (5.8 MB) ≤ 30 MB |
| RSS idle / thread abierto / delta | 6.5 MB / 7.4 MB / 0.92 MiB (≤50 MB / ≤10 MiB) |
| Arranque p95 (30 muestras, 0 fail) | 12.9 ms ≤ 300 ms |
| 5 mocks: app / agentes / total | 9.68 MB / 20.41 MB / 30.10 MB; app ≤500 MB PASS |
| 2 runs mock en TUI | TUI 8.55 MB; mocks 4.18/4.26 MB; combinado 16.99 MB |
| Arranque con 2 runs activos | 50.6 ms |
| Consultas `run list`/`workspace list` | max 5.59 ms, avg 3.48 ms |

Ningún umbral fue relajado. Los números de Hermes real no se infieren de los
mocks.

---

## 8. Riesgos restantes / límites del threat model

1. **P2-S0 provider semantics:** el caso `failed_turn` reportado por el
   provider mientras ACP responde `stopReason=end_turn` **no** se corrige en
   R2. Es un blocker P2-S2.
2. **Downgrade v3:** no es seguro con writers gestionados vivos; ver §4.
3. **Same-UID filesystem races:** validación de inodo + `remove_dir_all` no es
   fd-relative; un atacante same-UID puede intentar un swap. Se reduce con
   doble validación, `O_NOFOLLOW`, root 0700 y mountpoint checks; no se
   promete sandbox OS.
4. **Procesos que se ocultan:** un proceso same-UID que borre environ/cmdline
   y cierre fds puede no ser atribuible. El contrato no cubre un atacante
   same-UID deliberadamente oculto; se prefiere HOLD cuando hay evidencia
   insuficiente.
5. **Auth probes:** un descendiente que escape con `setsid` y cierre sus pipes
   puede sobrevivir al timeout; no se señala sin identidad verificada. El
   resultado es `unknown` y el operador debe limpiar.
6. **Hermes version contract:** la clasificación auth está verificada contra
   Hermes 0.21.x; otra versión puede quedar `unknown`.
7. **Perfiles reales:** no se ejecutó ninguna operación OAuth real; P2-S0 y
   Nous no se tocaron.

---

## 9. Handoff para el auditor

Foco recomendado:

1. competir `profile assign`/`unassign`/`remove` contra una admisión detenida
   en el barrier y contra auth lenta;
2. forzar SIGKILL entre `owner.json`, spawn, `runs::start` y liberación de
   locks;
3. probar registry ausente + run NULL legacy + DB abierta/cerrada;
4. probar cada componente symlink (root, home, san-home, tres locks,
   profile.json, owner.json, agents.json) con sentinel externo;
5. 24+ openers v3→v4 y DDL v4 parcial;
6. partial remove + owner vivo + escape con `setsid` + flock heredado;
7. verificar que un provider inválido en initialize deja el run `failed`, no
   `completed`;
8. comprobar que el flag OFF no cambia ninguna garantía Phase 1.

Comando base:

```bash
cargo test --test phase2_s1_r2 -- --test-threads=1
python3 scripts/phase2/r1_repro.py --bin target/debug/aeril
```

La aceptación final sigue siendo responsabilidad del auditor independiente.
