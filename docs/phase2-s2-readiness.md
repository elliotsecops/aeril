# P2-S2 — Readiness para “dos agentes reales” (HOLD → plan de cierre)

**Estado:** HOLD. Este documento fija el gap real contra el código de
`main @ 8765580`, la matriz de verificación exigida y las decisiones que
requieren autorización explícita. **No implementa P2-S2.**

Referencia del milestone: *Alpha Linux para dos agentes reales* — dos agentes
trabajando simultáneamente en workspaces y perfiles propios, con permisos y
resultados en la tarea correcta, consumo medido y sin procesos/locks sin
explicar.

---

## 1. Lo que ya existe (base sobre la que se construye)

- **Perfiles gestionados (P2-S1, 3 rondas adversariales):** `create/assign/
  status/warm/remove`, layout por agente (`home/`, `san-home/`,
  `profile.json`, `aeril.lock`, `owner.json`), metadata fuera de la DB,
  permisos 0700/0600, lock de escritor con **fd heredado** + binding al inodo,
  owner lifecycle, tombstone de borrado, auth estricta (exit 0 + provider +
  positivo exacto), fail-closed y `--clear-threads` sin bypass de runs activos.
- **Wiring del Runner con flag ON:** perfil obligatorio para `hermes`, lock
  retenido durante todo el writer, entorno sanitizado (`HERMES_HOME`, `HOME`,
  `XDG_*`, `GH_CONFIG_DIR`), admisión linearizada con registry lock, preflight
  anti-sobrevivientes, `runs.profile` durable.
- **TUI integrada (P2-TUI):** workbench de 3 paneles, progreso acotado por run
  (SQL, independiente de la ventana de 100 eventos), checkpoint inmutable,
  comentarios/feedback, integración fast-forward, estados working/completed.
- **Fiabilidad:** WAL retry (PR #2), locks de refs Git con CAS+retry (PR #3),
  migración serializada, reconciliación durable (S0) y fences de lease (RC2.1).

## 2. Gaps concretos para P2-S2 (con evidencia de código)

| # | Gap | Evidencia | Severidad |
|---|---|---|---|
| G1 | **`failed_turn` no se detecta en producción.** El estado del run se deriva solo de `stopReason`: `end_turn` ⇒ `completed`; no existe parsing del boundary de turno fallido de Hermes. | `src/session.rs:562-586`; `grep failed_turn src/` = 0 resultados. En P2-S0 el harness detectaba el aviso exacto: *“Your request was not processed. Send it again if you still want me to carry it out.”* / *“This turn did not complete. Some actions may already have run; verify their effects before resending.”* (el ACP igual responde `end_turn`). | **Bloqueante** |
| G2 | **Gate single-Hermes incondicional en la TUI.** `is_hermes_backend(backend) && !runs.is_empty()` rechaza el segundo run Hermes siempre; no hay transición N=2 por perfiles. | `src/tui/mod.rs:108-110, 194-197` | **Bloqueante** |
| G3 | **No hay asignación de perfil desde la TUI** (solo CLI `profile assign`) ni selector de agente/perfil por tarea. | Friction log A2; `src/tui/mod.rs` sin referencias a `profile::` | **Bloqueante** |
| G4 | **Permisos no son del usuario:** la TUI fija `PermissionPolicySetting::Auto`; el timeline muestra `auto_allow_once`. El criterio “los permisos llegan a la tarea correspondiente” no es observable ni accionable. | `src/tui/mod.rs:432,528`; friction log A3 | **Bloqueante** |
| G5 | **No existe suite E2E con Hermes real** (opt-in). No hay `tests/phase2_hermes.rs` ni `AERIL_HERMES_E2E`. | `ls tests/` | **Bloqueante** |
| G6 | **Política de modelo no explícita.** `RunOptions.model` es opcional y el CLI acepta `--model`; no hay validación/default ni decisión registrada. P2-S0: modelos pagos fallan con `404 insufficient_credits_for_paid_model`; solo free (`:free`) disponibles. | `src/session.rs:76`; `src/cli.rs:335` | **Bloqueante de decisión** |
| G7 | **Budget idle sin acuerdo.** P2-S0: 124.5 MiB antes de sesión (PASS ≤192), **213.7 MiB con sesión** y **258.8 MiB tras turno** (VIOLATION del budget 192). El milestone exige presupuesto acordado explícitamente. | `docs/phase2-s0-hermes-auth-gate.md` §15 | **Bloqueante de decisión** |
| G8 | **Medición agregada N=2 real** inexistente (solo mocks en `budget-two`/`budget-scale`). | `Makefile`, `scripts/bench/*` | Necesaria para PASS |
| G9 | **Barrido de procesos/locks al cierre del E2E real** inexistente (existe el barrido de tests con identidad, pero no para Hermes real). | `tests/phase2_s1_r2.rs` (fixtures), no E2E real | Necesaria para PASS |

## 3. Matriz de verificación (los 6 criterios del milestone)

Cada fila debe quedar con evidencia reproducible y un test nombrado.

| Criterio | Test / aserción propuesta |
|---|---|
| **Dos agentes simultáneos en workspaces y perfiles propios** | `tests/phase2_hermes.rs::rh1_dos_sesiones_aisladas`: dos threads, dos perfiles (`p-A`/`p-B`), dos runs concurrentes; canary distinto por sesión; verificar que cada `sessionId`/`HERMES_HOME`/workspace corresponde a su run y que ningún evento cruza. |
| **Un turno fallido aparece como fallido** | `rh2_failed_turn_never_completed`: fake determinista que emite `end_turn` con el aviso de turno fallido ⇒ run `failed` con razón; E2E real con modelo free forzando el fallo (si es reproducible). |
| **Cancelar/recuperar uno conserva al otro** | `rh3_cancel_one_preserves_other`: cancelar A con B vivo ⇒ A `cancelled`, B `completed`; crash de A (SIGKILL del backend) ⇒ reconciliación S0 de A, B intacto y sin señales a procesos ajenos. |
| **Permisos y resultados en la tarea correcta** | `rh4_permissions_routed`: dos runs pidiendo permiso; responder en A no afecta B; cada `permission.requested/responded` queda en su thread/run (eventos) y la UI (modo ask-human) lo muestra. |
| **Consumo medido y presupuesto acordado** | `rh5_budget_n2_real`: RSS por proceso y agregado durante el turno; comparar contra el presupuesto **acordado** (decisión D2) y registrar el número. |
| **Sin procesos/locks sin explicar** | `rh6_no_leftovers`: barrido por identidad (pid+starttime+cmdline+HERMES_HOME) al final; 0 procesos, 0 locks, 0 perfiles temporales; owners limpios o con razón durable. |

**Gate:** RH-1..RH-6 en verde con evidencia real; single-Hermes se mantiene por
defecto y el N=2 real se habilita solo con perfiles gestionados y flag ON.

## 4. Decisiones requeridas (bloquean la implementación)

| ID | Decisión | Opciones | Impacto |
|---|---|---|---|
| **D1** | **Credenciales reales para S2.** ¿Se autoriza usar el modelo free de Nous con perfiles desechables (device-code por perfil, como P2-S0), sin tocar el perfil del host? | (a) sí, free-only y perfiles descartables; (b) no, S2 queda con fake hasta nueva instrucción | Sin D1 no hay evidencia real (RH-1..RH-6) |
| **D2** | **Presupuesto idle/agregado de Hermes real.** P2-S0 violó 192 MiB (213.7 con sesión, 258.8 tras turno). ¿Se re-baselinea? | (a) re-baseline a **≤288 MiB idle / ≤768 MiB agregado** (propuesto en P2-S0); (b) optimizar Hermes/Aeril antes de aceptar; (c) otro número | Define RH-5 y el PASS del milestone |
| **D3** | **Política de modelo explícita.** | (a) exigir `model` explícito en el perfil y rechazar sin él; (b) default documentado al free (`nous:…:free`) y error claro si no hay créditos | Evita turnos “completados” que en realidad fallaron por créditos (G1/G6) |
| **D4** | **Política de permisos para runs reales.** | (a) ask-human en la TUI (requiere G4); (b) auto con auditoría visible; (c) deny por defecto | Define RH-4 y el recorrido “responder permisos” |

## 5. Orden propuesto (sin reabrir lo aceptado)

1. **D1–D4** (decisiones) y presupuesto acordado por escrito.
2. **G1** (`failed_turn`): detección explícita y versión-documentada; test fake
   determinista + E2E real; nunca `completed` con boundary de fallo.
3. **G4 + G3 + G2** (permisos, asignación de perfil, gate N=2): la TUI deja de
   fijar `Auto`; selector de perfil por tarea; gate condicionado a perfiles
   gestionados (single-Hermes default).
4. **G5** (`tests/phase2_hermes.rs` opt-in) con RH-1..RH-6 y **G8/G9** (medición
   y barrido).
5. Auditoría independiente de S2 (seguridad + concurrencia) y actualización del
   budget aceptado.

## 6. Riesgos y límites ya conocidos

- Strings de `failed_turn` dependen de la versión de Hermes (P2-S0 §1.1):
  re-validar ante cualquier upgrade; la detección debe ser explícita y fallar
  cerrado si el formato cambia (unknown ⇒ no completed).
- Hermes real puede cambiar el modelo/estado de auth entre corridas: el E2E
  debe registrar versión, modelo y `auth_status` por perfil.
- El consumo agregado incluye el backend real (no mock): el presupuesto debe
  declararse por proceso y agregado, y medirse con el mismo harness del cierre
  TUI (RSS + arranque + latencia de interacción).
- Follow-ups abiertos que S2 hereda: race `ESRCH` en `processes_using_home_in`
  (fail-closed), `latest_ready` con ready recuperado tarde, PDEATHSIG del
  harness PTY, latencia de lookup con historiales grandes.
