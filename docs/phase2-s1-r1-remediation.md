# P2-S1 R1 — Remediación de seguridad tras auditoría independiente

**Rama:** `feat/phase2-s1-profiles` (PR #1, draft, sin merge)
**Baseline auditado:** `39eaf15bf8fb1db662c21d244d6dbfaf8814ed21`
**Candidato corregido:** ver §9 (SHAs) y PR #1
**Veredicto de auditoría:** HOLD — 4 P1 + hardening P2
**Alcance:** P2-S1 únicamente. Sin merge, sin tags, sin tocar `main`, sin P2-S2, sin
multi-Hermes en producto, sin credenciales reales.

---

## 1. Protocolo y método

1. Estado verificado: SHA exacto, worktree limpio, rama y PR en draft.
2. Mapa de modos de fallo por hallazgo (§2).
3. **RED reproducido contra el candidato original** con fixtures descartables y
   crashes REALES (SIGKILL de supervisor), no filas fabricadas:
   `docs/phase2-s1-r1-evidence/red-baseline-39eaf15.txt`
   (6/7 escenarios RED; el restante es la fase de reconciliación que el
   candidato original tampoco podía satisfacer de forma segura).
   Nota: esa captura RED es la del momento del hallazgo; el GREEN posterior
   incluye además los casos añadidos durante la remediación (initialize
   fragmentado, DB ausente) que no representan violaciones nuevas del baseline.
4. Fixes con invariantes explícitos; tests adversariales nuevos (§5).
5. **GREEN re-ejecutado con el mismo script** contra el candidato corregido:
   `docs/phase2-s1-r1-evidence/green-candidate.txt` (0 RED).
6. Revisión adversarial de segundo orden (§6) — incluye fallos encontrados
   durante la propia remediación.
7. Gates completos (§7) y entrega (§9).

Reproductor independiente (auditor):

```bash
python3 scripts/phase2/r1_repro.py --bin <ruta-al-binario>
# salida: una línea por escenario (RED = violación, GREEN = fail-closed)
```

El script usa `tests/common/fake_hermes.py` (mismo fake que la suite) y crea
directorios temporales; no toca perfiles P2-S0 ni credenciales.

---

## 2. Mapa de modos de fallo → invariante violado

| # | Modo de fallo (RED verificado) | Invariante violado |
|---|---|---|
| P1-1a | `remove --clear-threads` con run durable `running` (supervisor+backend SIGKILL) **borró el perfil** | Ningún perfil se borra si hay runs no-terminales; `--clear-threads` no pisa seguridad de runtime |
| P1-1b | `unassign` durante run no-terminal + `remove` **borró el perfil** | El vínculo durable run→perfil no puede perderse por reasignación |
| P1-1c | DB inaccesible (`chmod 000`) ⇒ chequeo **saltado** (`let Ok(store)`), perfil borrado | Errores de DB en decisiones de seguridad ⇒ HOLD (jamás `unwrap_or(0)`/skip) |
| P1-2 | `exit 1` con `"nous: logged in"`; `"not currently logged in"`; `"github: logged in"`; positivo+negativo ⇒ `logged_in` | `logged_in` solo con respuesta inequívoca (exit 0, provider, estado exacto, sin contradicción) |
| P1-3 | `initialize` aceptado con id≠1, `result:null`, `{"result":…}` sin jsonrpc, sin protocolVersion, frame >1 MiB | Pre-warm solo con respuesta válida del request emitido, con cotas de frame/stream y deadline |
| P1-4 | SIGKILL del supervisor: child sobrevive; segundo `warm` **aceptado** sobre el mismo HERMES_HOME | ≤1 escritor vivo por HERMES_HOME, incluso tras la muerte del supervisor |
| P2-H1 | mountinfo/lock ilegibles ⇒ "sin mounts"/"libre" (optimista) | Incertidumbre de inspección ⇒ fail-closed en operaciones destructivas |
| P2-H2 | temp `.{name}.tmp-{pid}` colisionable; RMW de metadata sin serializar (lost updates) | Escrituras atómicas únicas + RMW serializado |
| P2-H3 | Doc de rollback sugería `rm -rf <data>/hermes` | Los perfiles pueden contener credenciales: nunca borrado masivo |
| P2-H4 | Sin test ON-mode de run completo; rechazo de hermes no-gestionado sin documentar | Comportamiento ON documentado y probado |

**Causas compartidas** (por qué un fix podía no bastar): (a) manejo optimista
de errores (errores que se interpretan como "seguro"); (b) "presencia de un
token" como prueba (substring / clave `result` / clave en registry);
(c) propiedad y liveness no re-verificadas en el momento de admisión;
(d) el único vínculo run→perfil era mutable (registry), no durable (DB).

---

## 3. Fixes por P1

### P1-1 — Borrado seguro con runs activos/no reconciliados

- **Vínculo durable run→perfil (schema v4):** `runs.profile TEXT` (nullable),
  migración `ALTER TABLE` idempotente; el Runner lo escribe al crear el run.
  `unassign`/`reassign`/`--clear-threads` ya no pueden borrar la evidencia de
  propiedad de un run no-terminal.
- **Guard de runs no-terminales SIEMPRE** en `remove` (con y sin
  `--clear-threads`), por perfil durable **o** por thread (legacy):
  `SELECT COUNT(*) … state IN ('starting','running')`.
- **Fail-closed de DB:** apertura y consulta fallidas ⇒ error explícito
  (`no se pudo abrir/verificar … fail-closed`). Sin `unwrap_or(0)`.
- **`unassign`/`assign`** rechazan threads con runs no-terminales (HOLD) —
  no se puede desasignar/reasignar "en caliente".
- **Orden seguro en `remove`:** ownership/path → guard de runs → guard
  histórico (`--clear-threads`) → **adquisición** del flock (no solo check) →
  procesos sobrevivientes → re-verificación de inodo → borrado → registro.
- **Idempotencia:** dir ausente + entrada presente ⇒ limpieza de registro
  (previa verificación de runs); fallo entre FS y registro se repara en la
  siguiente ejecución sin corrupción.
- **Reconciliación acotada:** `aeril recover` (mecanismo existente) marca los
  runs interrumpidos como `failed`; recién entonces `remove` procede.

### P1-2 — Corrección de auth-status

- **Parseo estricto version-aware** (`classify_auth_output`, función pura):
  contrato del Hermes instalado v0.21.x (`<provider>: logged in` /
  `<provider>: logged out (…)` / `<provider>: Nous · free tier`).
- Exige: **exit 0**; **exactamente una** línea de estado del provider
  esperado (contradicciones ⇒ `unknown`); positivo **exacto** `logged in`
  (un `exit 1` con texto positivo ⇒ `unknown`); negaciones reconocidas ⇒
  `absent`; cualquier otra cosa ⇒ `unknown`.
- **Salida acotada** (64 KiB) y **timeout acotado**; la salida nunca se
  persiste (puede contener `client_id`).
- **Verdicto fresco siempre persistido** (`auth_status` + `last_auth_check_ms`)
  bajo `.meta.lock`: timeout/exit≠0/malformado ⇒ `unknown` persistido (no se
  deja un `logged_in` stale como si fuera actual).
- `list` reporta el verdicto cacheado **con** `auth_checked_ms` (nunca lo
  presenta como fresco); la admisión del Runner siempre re-verifica.

### P1-3 — Initialize estricto

- **Validador estricto** (`validate_initialize_frame`, función pura):
  JSON-RPC 2.0, sin `method` (notificaciones legítimas se ignoran **solo** si
  no traen `id/result/error`), `id` == 1, sin `error`, `result` objeto,
  `protocolVersion` == 1, `agentCapabilities` objeto, `agentInfo` objeto si
  está, `authMethods` array **no vacío** con ids string.
  Contrato verificado contra el Hermes instalado (`acp_adapter/server.py`:
  siempre anuncia ≥1 método; `PROTOCOL_VERSION = 1`).
- **Cotas duras:** frame ≤ 1 MiB, stream total ≤ 8 MiB, deadline fijo
  (`AERIL_PROFILE_WARM_TIMEOUT_SECS`, default 120 s) con `poll()`.
- **EOF a mitad de frame / sin respuesta / stream infinito** ⇒ rechazo.
- **Terminación verificada antes de liberar el lock:** grupo de procesos
  propio, SIGTERM → plazo → SIGKILL → verificación con
  `recovery::live_group_members` (ningún miembro vivo; grupo no inspeccionable
  ⇒ error, jamás "terminado").

### P1-4 — Exclusividad de escritor ante crash del supervisor

- **Flock heredado:** el fd del lock se marca heredable (`F_SETFD` sin
  `FD_CLOEXEC`) antes del spawn; el backend retiene el OFD ⇒ el lock
  **sobrevive la muerte del supervisor** (verificado: `lock_state=held` con el
  child vivo tras SIGKILL).
- **Evidencia de dueño (`owner.json`):** pid+starttime+pgid del supervisor y
  backend del warm; en admisión, pid vivo con starttime coincidente ⇒ HOLD;
  identidad no verificable ⇒ HOLD; ausente/reutilizado/zombie ⇒ stale limpio.
- **Escaneo `/proc` fail-closed con discriminación:** procesos de otro uid se
  omiten (no pueden ser dueños de un 0700); `environ` con EACCES
  (`PR_SET_DUMPABLE=0`, normal en hosts reales) **no** se trata como dueño —
  se buscan señales alternativas (`cmdline`/`fd` apuntando al perfil) y, sin
  señales, se omite; errores reales de inspección ⇒ HOLD. Los *auth probes*
  propios (`hermes auth status`) se clasifican por cmdline y no bloquean
  admisiones. Alcance de amenaza: procesos same-uid maliciosos que se oculten
  quedan **fuera** del contrato (documentado; la auditoría lo excluye).
- **Preflight de admisión doble:** en el CLI (`run start`) **antes** de la
  reconciliación de crash, y en `Runner::start` antes del lease. Un run con
  escritor sobreviviente se **rechaza**; jamás se admite "matando primero".
- **Reconciliación no mata con perfiles gestionados:** `reconcile_interrupted_runs`
  ya no termina automáticamente backends de runs con `profile` cuando el flag
  está ON: los marca inseguros y la decisión (terminar el proceso huérfano con
  identidad verificada) queda en el operador. Con flag OFF, el comportamiento
  certificado de Fase 1 queda intacto.
- **Reconciliación acotada:** `profile recover --agent <id> --confirm`
  (lock tomado + verificación de sobrevivientes; jamás fuerza).

---

## 4. Matriz de remediación P2 (H1–H5)

| ID | Fix | Evidencia |
|---|---|---|
| H1 | `mountinfo` ilegible ⇒ `Err` (nunca "sin mounts"); `LockState::{Free,Held,Unknown}` distingue ausencia definitiva de incertidumbre; escaneo de procesos fail-closed con discriminación documentada | Unit: `mountinfo_fault_injection`, `proc_scan_fault_injection`; Integración: `r1_h1_unreadable_lock_is_unknown_and_blocks_removal` (chmod 000 ⇒ `lock_state=unknown`, remove denegado) |
| H2 | Temporales únicos `O_EXCL`+`O_NOFOLLOW` (8 intentos), fsync de archivo y **directorio propagado**; RMW de `profile.json` serializado por `.meta.lock` (flock por perfil) | Unit: `atomic_write_concurrent_unique_tmp` (8 hilos, sin colisión ni residuos); Integración: `r1_h2_concurrent_meta_updates_no_lost_update` (6 status + 1 warm ⇒ ambos campos sobreviven) |
| H3 | Doc de rollback reescrito: sin `rm -rf` masivo; borrado por perfil validado (`profile remove --confirm`), advertencia de credenciales/estado durable, autorización del usuario | `docs/phase2-s1-implementation.md` §5 actualizado |
| H4 | Documentado por qué hermes no-gestionado se rechaza con flag ON (fail-closed por diseño); test ON-mode de run completo con perfil creado/asignado/autenticado (fake) | Integración: `r1_h4_flag_on_full_run_with_managed_profile` (state=completed); OFF-mode certificado intacto (`t15`) |
| H5 | Presupuestos re-medidos sobre el binario final (release) del SHA de entrega; sin relajar umbrales | §7 (budgets) y doc de implementación §4 |

### H4 — Por qué hermes no-gestionado se rechaza con el flag ON

Con `AERIL_HERMES_PROFILES=1` y backend `hermes`, un thread **sin** perfil
asignado se rechaza por diseño (`no tiene perfil asignado … sin fallback al
perfil del host`). Es el comportamiento fail-closed exigido por P2-S1: el flag
ON declara que los runs hermes de Aeril operan exclusivamente con perfiles
gestionados (HERMES_HOME/HOME/XDG propios y lock de escritor), y **no existe
fallback** al `~/.hermes` del host. Los tests de Hermes no-gestionado siguen
siendo válidos **con el flag OFF** (comportamiento certificado, `t15`), que es
el modo por defecto. El test `r1_h4_flag_on_full_run_with_managed_profile`
demuestra el camino ON completo (crear → asignar → autenticar → run
`completed`) con un perfil fake.

---

## 5. Inventario de tests nuevos

### `tests/phase2_s1_r1.rs` (15 tests)

| Test | Cubre |
|---|---|
| `r1_p1_1a_unresolved_run_blocks_removal_even_with_clear_threads` | Crash genuino (SIGKILL) → HOLD con y sin `--clear-threads`; `aeril recover` habilita |
| `r1_p1_1b_unassign_during_unresolved_run_denied` | `unassign`/`reassign` durante run no-terminal ⇒ HOLD; post-reconciliación OK |
| `r1_p1_1c_unreadable_database_denies_removal` | DB `chmod 000` ⇒ remove denegado, perfil intacto |
| `r1_p1_1d_active_run_blocks_removal` | Run vivo ⇒ remove denegado (guard de runs/lock) |
| `r1_p1_1e_terminal_run_allows_explicit_removal` | Run terminal + asociación histórica ⇒ `--clear-threads` explícito permite |
| `r1_p1_1f_partial_crash_removal_is_idempotent` | Crash entre FS y registro ⇒ limpieza idempotente, registro íntegro |
| `r1_p1_1g_missing_database_denies_removal` | DB ausente ⇒ HOLD; tras materializarla, removal normal |
| `r1_p1_2_auth_matrix_never_false_positive` | Matriz 10 modos + recuperación `unknown→logged_in` + timestamps frescos |
| `r1_p1_2_runner_admission_rejects_exit1_positive_text` | Admisión rechaza `exit 1` con texto positivo; sin lock residual |
| `r1_p1_3_warm_strict_initialize_matrix` | 13 modos de initialize (válidos/fragmentados/adversariales) |
| `r1_p1_3_noncooperative_child_is_terminated` | Child ignora SIGTERM ⇒ escalada acotada verificada, sin huérfanos |
| `r1_p1_4_surviving_writer_after_supervisor_sigkill` | **Escenario obligatorio de 10 pasos** (ver §8) |
| `r1_h1_unreadable_lock_is_unknown_and_blocks_removal` | Lock ilegible ⇒ unknown + remove denegado |
| `r1_h2_concurrent_meta_updates_no_lost_update` | Concurrencia de metadata sin lost updates |
| `r1_h4_flag_on_full_run_with_managed_profile` | Flag ON: run completo con perfil fake autenticado |

### Unit tests nuevos en `src/profile.rs`

`auth_classifier_adversarial_matrix` (17 casos), `init_validator_adversarial_matrix`
(18 casos), `mountinfo_fault_injection`, `proc_scan_fault_injection`,
`atomic_write_concurrent_unique_tmp`, `non_terminal_run_guard_covers_starting_and_running`.
Total unit del módulo: 9 (3 previos + 6 nuevos).

### Suite P2-S1 original actualizada

`t14` ahora exige HOLD antes de reconciliar y permite el borrado solo después
de `aeril recover`; el fake hermes es el canónico compartido
(`tests/common/fake_hermes.py`).

---

## 6. Revisión adversarial de segundo orden (auto-derrota)

Fallos **encontrados y corregidos durante esta remediación** (no inventados):

1. **"Admitir matando" (hallazgo real):** la reconciliación de crash de Fase 1
   (llamada por `run start` antes del preflight) terminaba al backend
   sobreviviente con identidad verificada y **después** admitía el segundo run.
   La invariante de ≤1 escritor se mantenía, pero el escenario obligatorio de
   la auditoría exige rechazo. Fix: preflight en CLI **antes** de reconciliar +
   gate en `reconcile_interrupted_runs` para runs con perfil (no termina;
   HOLD + operador).
2. **Falso positivo de auth probes:** los `hermes auth status` concurrentes
   llevan HERMES_HOME sanitizado y el escaneo los confundía con escritores
   (HOLD espurio). Fix: clasificación por `cmdline` (probe vs writer ACP);
   cmdline ilegible ⇒ conservador.
3. **Host inutilizable:** tratar `environ` EACCES (dumpable=0: systemd --user,
   ssh-agent, …) como incertidumbre rompía toda admisión en hosts normales —
   exactamente el anti-patrón que la auditoría prohíbe. Fix: discriminación
   por señales alternativas + exclusión de otros uid + contrato de amenaza
   documentado.
4. **Lock recreando perfiles borrados:** `ProfileLock::acquire` creaba
   directorios (carrera de admisión tras un `remove`). Fix: si el perfil no
   existe, `acquire` falla (no recrea).
5. **`is_held` con efectos colaterales:** `list`/`status` creaban el lock file
   (y un dir en perfiles "missing"). Fix: `lock_state` sin escrituras.
6. **Deadline de warm bloqueante:** lectura byte a byte sin `poll()` ignoraba
   el deadline si el backend no emitía nada (test t8 tardaba >60 s). Fix:
   `poll()` con timeout acotado por iteración.

Puntos evaluados sin cambio (limitaciones documentadas):

- **TOCTOU residual en borrado (C4):** ventana teórica entre la última
  re-verificación de inodo y `remove_dir_all` (que no sigue symlinks); solo
  explotable por un actor con escritura en el directorio padre (0700, mismo
  usuario). Hardening futuro: borrado recursivo por fd (`openat`/`unlinkat`).
- **Procesos same-uid que se ocultan deliberadamente** (dumpable=0 malicioso):
  fuera del contrato de amenaza (la auditoría excluye sandboxing contra
  same-uid malicioso). El flock heredado sigue siendo la barrera primaria.
- **Runs legacy sin `runs.profile`** (pre-v4): cubiertos por el chequeo por
  thread; la migración es mecánica y sin pérdida.

---

## 7. Gates de verificación (revisión final)

```text
cargo test --no-fail-fast (suite completa): EXIT=0 — 163/163, 23 binarios
  · Fase 1 completa (124) + P2-S1 original (15) + R1 (15) + unit (9 del módulo
    profile entre los 14 del lib)
  · Nota de flakiness preexistente: `g9_integracion_concurrente_ff` (Fase 1,
    concurrencia de integración git) falló en 2 de 5 corridas SOLO bajo carga
    extrema de la máquina (load ≈12–14): el perdedor recibió el lock de
    `refs/heads/destino` en vez del mensaje stale "se movió" (el ganador nunca
    es pisado; el invariante se mantiene). Pasa en aislamiento (11/11) y en la
    corrida final completa. No se toca código de integración/git en este diff.
cargo fmt --check: limpio
cargo clippy --all-targets -- -D warnings: limpio

Presupuestos (binario RELEASE reconstruido explícitamente del SHA final;
`cargo build --release` ANTES de medir — el target `make budget` reutiliza un
binario existente si no se fuerza el rebuild):
  budget-size : 5,783,728 bytes (5.8 MB) ≤ 30 MB                    → PASS
  budget-deps : PASS (solo ABI base del OS + zlib del sistema)
  budget-mem  : idle 6.8 MB · thread abierto 7.7 MB · delta 0.91 MiB
                (límites 50 MB / 10 MiB)                            → PASS
  budget-start: p95 55.9–61.9 ms ≤ 300 ms                           → PASS
  budget-two  : TUI 8.6 MB · mocks 4.3 MB c/u · combinado 17.2 MB
                arranque con 2 activos 32.7–56.3 ms
                consultas avg 20.3 ms / max 27.5–33.5 ms
  budget-scale: app 9.8 MB (gate 500 MB) · agentes 21.0 MB · total 30.9 MB
                                                                     → PASS
```

**A/B de atribución (misma carga, mismo harness):** base `39eaf15`
(release reconstruido) p50 52–57 ms / p95 88–90 ms vs candidato R1
p50 54–56 ms / p95 75–98 ms ⇒ **sin regresión atribuible**; la diferencia
frente a la medición en reposo (~12 ms) es ambiental, no del diff.

**Contexto ambiental (honestidad de medición):** durante estas mediciones la
máquina presentaba `load average ≈ 12` y procesos fake Hermes huérfanos de la
propia auditoría independiente (`/tmp/opencode/p2s1-audit-scratch4/5`, no
tocados por este trabajo). Los valores absolutos de latencia quedan inflados
respecto de corridas previas en reposo (p95 ~12 ms); **todos los gates pasan
con margen amplio** y no se relajó ningún umbral. No se reclama una mejora ni
una regresión de latencia sin A/B en reposo (fuera del alcance de R1).

---

## 8. Escenario obligatorio P1-4 (10 pasos, reproducible)

`r1_p1_4_surviving_writer_after_supervisor_sigkill` ejecuta exactamente:

1. Perfil `p14` con fake Hermes (modo linger) + run hermes.
2. Confirmar writer y lock activos (`lock_state=held`).
3. SIGKILL al supervisor Aeril.
4. Confirmar child vivo (`kill(0)`) y **lock aún tomado por el child**
   (fd heredado).
5. Segundo `profile warm` ⇒ **rechazado** (lock).
6. Segundo `run start` ⇒ **rechazado** (preflight), sin terminar al
   sobreviviente (`alive(fake)` verificado tras el rechazo).
7. `profile remove --confirm --clear-threads` ⇒ **rechazado**.
8. Terminar el child descartable (SIGKILL) y verificar terminación.
9. `aeril recover` ⇒ run reconciliado (`failed`).
10. `profile warm` legítimo ⇒ **OK**; `remove` explícito ⇒ OK.
    El test no deja procesos huérfanos (verificado).

Variante no-cooperativa: `r1_p1_3_noncooperative_child_is_terminated`
(SIGTERM ignorado ⇒ SIGKILL verificado; sin huérfanos).

---

## 9. Entrega

- **Baseline auditado (RED):** `39eaf15bf8fb1db662c21d244d6dbfaf8814ed21`
- **Commit de remediación:** `2238d7730b0e3c8456ddb5dcd2e158eaa47acb01`
  (`fix(p2s1-r1): remediación de los 4 P1 + hardening (auditoría independiente)`)
- **Rama:** `feat/phase2-s1-profiles` — PR #1 (draft, sin merge). El tip de la
  rama al cierre (incluye este registro docs-only) se reporta en el PR.
- **Diff acotado:** `src/profile.rs`, `src/cli.rs`, `src/session.rs`,
  `src/recovery.rs`, `src/store/{schema,runs,mod}.rs` (schema v4 justificado en
  P1-1), `tests/phase2_s1_profiles.rs`, nuevos `tests/phase2_s1_r1.rs`,
  `tests/common/fake_hermes.py`, `scripts/phase2/r1_repro.py`, `docs/…`.
  Sin cambios de dependencias (`Cargo.toml`/`Cargo.lock` intactos) ni de
  `main`/tags.
- **Rollback:** ver §5 del doc de implementación (sin `rm -rf`; flag OFF
  restaura el comportamiento certificado; perfiles se dejan en disco salvo
  borrado explícito validado).

---

## 10. Checklist de auditoría (R1)

1. ¿`r1_repro.py` sobre `39eaf15` reproduce los RED documentados? (evidencia
   en `docs/phase2-s1-r1-evidence/red-baseline-39eaf15.txt`)
2. ¿El mismo script sobre el candidato corregido da 0 RED? (`green-candidate.txt`)
3. ¿`remove` verifica runs no-terminales **siempre** (con `--clear-threads`) y
   fail-closed ante DB inaccesible? (`r1_p1_1*`)
4. ¿`unassign`/`reassign` no pueden borrar el vínculo de un run no-terminal?
5. ¿`logged_in` requiere exit 0 + provider + positivo exacto + sin
   contradicción, y los fallos persisten `unknown` fresco? (`r1_p1_2*` +
   matriz unit)
6. ¿El warm acepta solo el initialize estricto con cotas y deadline?
   (`r1_p1_3*` + matriz unit)
7. ¿El lock sobrevive al supervisor (fd heredado) y la admisión rechaza con
   sobreviviente sin matarlo? (`r1_p1_4`, escenario 10 pasos)
8. ¿La reconciliación no termina backends con perfil gestionado (flag ON)?
9. ¿Flag OFF intacto? (`t15`, suite Fase 1 completa)
10. ¿Sin credenciales reales en tests/evidencia? ¿Perfiles P2-S0 intactos?
11. ¿Diff acotado a los módulos justificados? (incluye `src/store/schema.rs`
    +`src/store/runs.rs`+`src/recovery.rs` por el vínculo durable v4,
    justificado en §3/P1-1)
12. ¿Presupuestos re-medidos y sin cambios de umbrales? (§7)
