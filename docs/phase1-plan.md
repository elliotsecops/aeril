# Fase 1 — Plan de ingeniería (aprobado 2026-10-14)

> Baseline: `main @ 99d2f7b` · **S0 integrado y aceptado** (PASS S0-R1, `e38a7d4` en main) · **S1 CONDITIONAL — single-Hermes sigue siendo el default soportado** (`docs/phase1-s1-hermes-isolation.md`) · **S2 integrado** (PASS de auditoría independiente; `b63f582` en main) · **S3 integrado** (CONDITIONAL PASS de auditoría independiente; `99d2f7b` en main) · **S4 implementado** (`60921bc5e01e7ae1c90dbe47336184fcf4473d6a`) · **S4-R1 (borrado seguro) en `fix/phase1-s4-deletion-safety`** (`ecdbd38eb14466ac0e1b667de40d4a44d9042e14`; rojo reproducido en `dac8ab2`).
> Objetivo: **dos agentes concurrentes** (dos threads, mismo repo, un workspace verificado
> cada uno) con estado durable, revisión independiente y recuperación segura.
> Toda la validación primaria es **mock determinista**; Hermes real en paralelo queda
> **condicionado al spike S1** (ver G8). Filosofía: *ship fast, ship secure*.

## 0. Distinciones que el plan NO puede confundir

1. **Historia durable del thread**: eventos, runs, comentarios y snapshots en SQLite +
   refs `refs/aeril/checkpoints/*`. Sobrevive a todo; es lo que Aeril posee.
2. **Rehidratación de workspace desde un checkpoint verificado**: recrear un worktree
   desde el OID exacto de un snapshot `ready` (protocolo existente). Es contenido
   revisado, no contexto del agente.
3. **Restauración de contexto del motor**: `session/load` de ACP. Con Hermes 0.21.5 es
   **no verificable** (`attempted/accepted/verified`, `verified=false` siempre).

**Nunca afirmar recuperación de ediciones del agente que no fueron capturadas en un
checkpoint.** Un crash mid-turn pierde el trabajo no-chequeado del worktree; la
recuperación reconcilia el RUN (fallo explícito) y, si había `prepared` legítimo,
completa el checkpoint con sus objetos inmutables — nada más.

## 1. Alcance del corte (mínimo útil)

- Dos runs concurrentes como máximo (**N=2**), uno por thread, sobre el **mismo repo**,
  con **workspaces verificados distintos**.
- Estado por thread visible (activo/terminado/interrumpido) y cancelación por run.
- Reconciliación durable de runs interrumpidos al reabrir (S0).
- Rehidratación de workspace por thread sin depender del worktree original.
- Presupuestos y fences de Fase 0 intactos.

**Límite de concurrencia N=2 — alcance de enforcement:** es un límite de producto de
Fase 1, aplicado por procesos de Aeril: (a) la TUI no lanza más de 2 runs supervisados
a la vez; (b) `bench scale` puede simular más para medir overhead, pero eso no cambia
el límite de producto; (c) NO es un límite del sistema operativo ni un scheduler: no
restringe procesos externos. Se documentará en la UI/CLI y en `docs/phase1-*`.

## 2. Secuencia de implementación

| # | Paso | Entregable | Estado |
|---|---|---|---|
| **S0** | **Reconciliación durable de runs interrumpidos** | `run.ended failed/interrupted` idempotente al reabrir; evidencia de supervisor y backend; gate de reuso conservador | ✅ **integrado y aceptado** (`e38a7d4` en main; +7 tests S0 y suite de seguridad R1) |
| S1 | Spike de aislamiento de perfiles de Hermes | Informe con evidencia (`HERMES_HOME`, state.db/installs/locks por perfil, kill aislado, credenciales) | ✅ **CONDITIONAL**: estado aislado verificado; sesiones reales y credenciales por perfil pendientes ⇒ single-Hermes default (`docs/phase1-s1-hermes-isolation.md`) |
| S2 | Estado de runs activos | `aeril run list [--active] [--limit]`: supervisor (alive/dead/unknown) + lease (owned/missing/held_by_other) + status efectivo; solo lectura, sin migraciones | ✅ **integrado** (`b63f582`; 8 tests S2; suite 83/83; make budget sin cambios) |
| S3 | TUI concurrente (2 runs, cancelar por run) | `n` nuevo run en el thread abierto; slots por invocación (N=2, 1/thread, Hermes real 1/vez); `x` cancela solo el thread seleccionado y el slot se libera únicamente con el ack terminal; `q` bloqueado con runs vivos; mismo panel único; harness PTY solo para tests | ✅ **integrado y aceptado** (CONDITIONAL PASS; `99d2f7b` en main; 9 tests T1–T9) |
| S4 | `workspace list` + ciclo de vida | `workspace list [--thread T] [--limit N]` (solo lectura, dedupe por workspace, estados y elegibilidad de reuso) + guard de `worktree rm` (lease vivo RC2.1; `--unsafe-ok` explícito) + rehidratación verificada | ✅ `60921bc5e01e7ae1c90dbe47336184fcf4473d6a` (11 tests S4; suite 103/103; budget PASS) |
| S5 | E2E de concurrencia (mock) | `tests/phase1_concurrency.rs` (G1–G7, G9) | pendiente |
| S6 | Presupuestos + docs de fase | `make budget*` extendido, `docs/phase1-status.md` | pendiente |

### S4 — Semántica de `workspace list` y borrado seguro

- **Inspección read-only.** `aeril workspace list [--thread T] [--limit N]` agrega el
  run más reciente por `workspace_id` (dedupe: un workspace reutilizado por varios
  runs aparece UNA vez) y reutiliza la verificación de worktree (git), los
  clasificadores supervisor/lease de S0/S2 y el veredicto de reuso S0. No
  reconcilia, no crea worktrees y no toca el filesystem ni la DB.
- **Estados (`status`) con precedencia documentada**: `repo_unavailable`
  (diagnóstico por workspace si el repo no abre) → `unsafe_for_reuse` (evento
  durable `workspace.recovery_unsafe`; nunca se olvida, D4) → `verified_leased`
  (lease con dueño vivo y worktree ok) → `stale_or_uncertain` (run sin resolver
  sin lease vivo) → `missing_worktree` / `invalid_worktree` (invalid|mismatched) →
  `unsafe_for_reuse` (veredicto S0 bloqueado) → `verified_available`.
  `reuse.eligible` combina worktree utilizable + veredicto S0 + ausencia de lease
  vivo; `reuse.reason` explica el motivo.
- **Worktree ausente ≠ snapshot ausente.** Los checkpoints viven en eventos +
  refs por OID y sobreviven al borrado del worktree; `workspace list` reporta el
  worktree, no la memoria de revisión.
- **Borrado (S4-R1).** `aeril worktree rm --thread T [--unsafe-ok]`:
  - **Cualquier lease presente en el chequeo final serializado bloquea el
    borrado**, incluso stale (un lease stale se resuelve por su ciclo de vida:
    verificar la muerte del proceso y `aeril lease release --workspace W`, que
    rechaza dueños vivos). Un dueño VIVO no se desplaza jamás (RC2.1; `--force`
    no lo pisa y el mensaje ya no lo sugiere como bypass).
  - **Por defecto se rechaza un run sin resolver con supervisor vivo o
    desconocido aunque falte el lease** (D1): el lease no es la única evidencia
    de ejecución. `--unsafe-ok` es un reconocimiento explícito de riesgo — NO
    termina backends ni prueba que el workspace haya dejado de modificarse — y
    tampoco pisa el fence de lease.
  - **Serialización (D2)**: la decisión final y el borrado corren dentro de una
    transacción `IMMEDIATE` (el mismo mecanismo de `leases::try_acquire`), con
    re-lectura de identidad del target, del lease y de los guards S0/supervisor
    usando la conexión de la transacción. Si una adquisición gana primero, el
    borrado aborta; si el borrado gana, la adquisición espera y después aplica
    sus reglas ordinarias (verificación falla ⇒ crea workspace nuevo; no escribe
    en el path borrado). No es una transacción atómica DB+filesystem: la
    transacción serializa contra adquisiciones de lease de Aeril.
  - El borrado **preserva checkpoints, refs, ramas y comentarios**; Aeril solo
    verifica procesos del grupo que él creó y no declara verificación fuera de
    esa frontera.
  - Los límites conocidos del camino de panic de S3 **siguen pendientes y no
    bloquean** S4/S4-R1 (catch_unwind solo cubre el cierre del runner; el panic
    hook no se endureció).
- **Rehidratación.** Con el worktree borrado, un feedback sobre un snapshot
  `ready` recrea el worktree en el OID del snapshot (evento `workspace.recreated`)
  y lo registra; la sesión del motor se reporta aparte (`attempted/accepted`). No
  se afirma restauración de contexto del motor ni recuperación de ediciones no
  capturadas.
- **Follow-up S3 (no bloqueante, fuera del alcance de S4)**: (a) `catch_unwind`
  solo cubre el cierre del runner, no panics en el resto de la TUI; (b) no se
  endureció el panic hook: un panic del runner puede ensuciar el frame del PTY
  hasta el próximo redraw. Se registran como trabajo futuro; no son gates de S4.

## 3. Gates de aceptación

- **G1 Concurrencia real (mock)**: 2 runs simultáneos, mismo repo, workspaces distintos
  (ids/paths únicos), cada uno publica su checkpoint; sin contaminación cruzada de
  eventos/ficheros; leases distintos.
- **G2 Crash bajo concurrencia**: SIGKILL a A mid-turn mientras B corre; B completa;
  al reabrir, A queda `run.ended failed/interrupted` durable, su workspace no se reusa
  sin evidencia de terminación del backend, su thread es revisable y resumible
  (rehidratación desde su snapshot).
- **G3 Cancelación aislada**: cancelar A (plazo fijo) mientras B activo; B completa;
  A `cancelled` sin checkpoint; sin procesos huérfanos de A.
- **G4 Retención cruzada**: borrar el worktree de A; comentarios/patch de A siguen
  abriéndose por OID; el workspace de B intacto; refs intactas.
- **G5 Resume sin worktree**: ambos worktrees borrados ⇒ rehidratación por thread desde
  SU snapshot (sin mezcla); se registra `attempted/accepted` de sesión; no se afirma
  restauración de contexto.
- **G6 Permisos fail-closed (por run)**: inyección de fallo en la auditoría de permisos
  de A no autoriza nada en A **y no afecta a B**. El test inyecta un fallo acotado al
  run A (`AERIL_FAULT_APPEND` en el proceso de A); una caída global de SQLite es otro
  escenario (out of scope del gate) y no se confunde con este.
- **G7 Presupuestos**: `make budget` pasa; escenario de escala (≥2 runs concurrentes)
  con app ≤ 500 MB y agentes medidos aparte; arranque p95 ≤ 300 ms con 2 runs activos.
- **G8 Honestidad Hermes**: concurrencia con Hermes real **solo** si S1 la verifica.
  Mock-only es un hito de ingeniería, **no** prueba de readiness con agentes reales;
  el informe final lo explicita.
- **G9 Integraciones concurrentes**: dos integraciones del MISMO commit base a la misma
  rama destino no pueden pisarse: la primera gana; la perdedora devuelve
  stale/diverged **sin cambiar la rama destino** (la transacción de refs de Fase 0 ya
  lo garantiza; el gate lo prueba en el escenario concurrente de Fase 1).

## 4. Riesgos (recursos y correctitud)

- **Contención SQLite** con N conexiones WAL + apps IMMEDIATE cortos: N=2; medir.
- **RAM**: TUI + 2 runs + backends; agentes medidos aparte; gate G7.
- **Aislamiento Hermes incierto (S1)**: puede terminar en restricción “un Hermes a la
  vez”, documentada; no bloquea el corte con mock.
- **Huérfanos de backend** ante muerte abrupta del supervisor: en el kill normal se
  mata el grupo de procesos; tras SIGKILL del supervisor, la reconciliación verifica
  por PID+starttime y termina lo que haya quedado vivo antes de permitir reuso (S0).
- **Evidence gaps**: runs legados sin evidencia de backend ⇒ reuso bloqueado
  (conservador) hasta remover el worktree; documentado.
- **Frontera de procesos (S0-R1, `fix/phase1-s0-process-safety`)**: Aeril inspecciona
  y supervisa el **grupo de procesos del backend que él mismo crea** (líder +
  descendientes), pero **no es un sandbox del SO**. Procesos que escapan por su
  cuenta del grupo/sesión registrados (p. ej. `setsid`) quedan **fuera de la
  garantía de atribución**: no se reclaman ni se señalan, y su existencia no se
  detecta. Identidad no verificable (`pid_start=0`, `/proc` ilegible) o PID
  reutilizado ⇒ `workspace.recovery_unsafe` persistente + reuso bloqueado, **sin
  señales destructivas**; un líder muerto con miembros vivos de su grupo homónimo
  tampoco se reclama (posible reutilización de PGID) y bloquea el reuso (D1/D2/D4
  de la auditoría S0, corregidos en esta rama).

## 5. Fuera de alcance (explícito)

Paneles divididos / multiplexor PTY / `tui-term`·`vt100`; backends nuevos; merge /
cherry-pick / resolución de conflictos (solo FF); remapeo de comentarios; recolector
de refs (sigue solo-listado); GUI web; multiusuario/CRDT; scheduler con colas o N>3;
UI de permisos más allá de auto/deny.

## 6. Definition of done de Fase 1

Los gates G1–G9 automatizados en verde con mock, presupuestos re-medidos, y un informe
de aceptación con los límites declarados (Hermes real condicionado a S1). Sin claims
de contexto restaurado ni de trabajo no-chequeado recuperado.
