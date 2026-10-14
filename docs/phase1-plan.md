# Fase 1 — Plan de ingeniería (aprobado 2026-10-14)

> Baseline: `main @ 4d14e09` · **S0 integrado y aceptado** (PASS S0-R1, `e38a7d4` en main) · **S1 completado: verdicto CONDITIONAL — single-Hermes sigue siendo el default soportado** (`docs/phase1-s1-hermes-isolation.md`) · **S2 implementado** (`331667196eebef6e308c1376f64e147d8b8a864d`).
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
| S2 | Estado de runs activos | `aeril run list [--active] [--limit]`: supervisor (alive/dead/unknown) + lease (owned/missing/held_by_other) + status efectivo; solo lectura, sin migraciones | ✅ `331667196eebef6e308c1376f64e147d8b8a864d` (8 tests S2; suite 83/83; make budget sin cambios) |
| S3 | TUI concurrente (2 runs, cancelar por run) | uso manual + pty | pendiente |
| S4 | `workspace list` + ciclo de vida | CLI + tests | pendiente |
| S5 | E2E de concurrencia (mock) | `tests/phase1_concurrency.rs` (G1–G7, G9) | pendiente |
| S6 | Presupuestos + docs de fase | `make budget*` extendido, `docs/phase1-status.md` | pendiente |

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
