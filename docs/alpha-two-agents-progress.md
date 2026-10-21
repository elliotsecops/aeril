# Alpha “dos agentes reales” — progreso de implementación

**Rama:** `feat/alpha-two-agents` (desde `main @ 8765580`). Sin merge, sin tags.
**Alcance aprobado:** D1–D4 con el orden A4 → G1 → G4+G3 → G2 → G5/G8/G9 → auditoría.
**Presupuesto del ensayo (D2):** ≤288 MiB por perfil en reposo (post-turno, procesos
atribuibles) y ≤768 MiB de pico agregado; la violación histórica de 192 MiB sigue
registrada como violación. **Presupuestos propios de Aeril intactos.**

---

## Hecho

### A4 — entrada de comentarios segura (`49638b5`)

- `c` sobre un checkpoint verificado abre el editor; sobre cualquier otro evento
  (o sin evento) abre un **modal de bloqueo** (`Mode::Blocked`) con la razón
  exacta. El modal captura todo el teclado: solo Esc/Enter lo cierran; ninguna
  letra ejecuta atajos.
- `start_comment` es fail-closed: sin evento / evento no-checkpoint / checkpoint
  ilegible / sin archivos anclables ⇒ modal, nunca un status transitorio.
- Regresiones PTY (`tests/phase2_tui_comment_safety.rs`):
  1. texto `iqf i q f iq` + Enter ⇒ comentario guardado íntegro; la TUI no se
     cierra, no integra, no lanza feedback ni runs;
  2. `c` sobre `end` ⇒ “Comentario bloqueado”; `i/q/f` inactivas; sin efectos;
     Esc restaura los atajos normales.

### G1 — clasificación de turno fallido (este commit)

- **Señal estructurada primero:** el cierre sigue derivándose de `stopReason` y
  errores (no-`end_turn` ⇒ `failed`; `cancelled` ⇒ `cancelled`).
- **Fallback textual acotado** (solo cuando el ACP responde `end_turn`):
  - backend **exclusivamente `hermes`**;
  - **versión soportada** `0.21.x` (comparación por token, no substring);
  - **igualdad exacta** del texto COMPLETO del turno nuevo con uno de los dos
    avisos terminales verificados en P2-S0 (con `trim`).
  - El texto del turno se acumula solo desde `on_turn_started` (replay/pings no
    cuentan) y con tope de 64 KiB.
- Resultado: `state=failed`, `stop_reason` preservado (`end_turn`), `detail`
  explícito y **sin checkpoint**.
- Evidencia determinista (fake Hermes, flujo de perfiles con flag ON):
  `tests/phase2_alpha_g1_failed_turn.rs` (6 tests) —
  aviso exacto / aviso parcial / aviso fragmentado ⇒ `failed`;
  **cita del aviso dentro de un texto mayor ⇒ `completed`**;
  **versión no soportada ⇒ `completed`** (sin fallback textual);
  turno normal ⇒ `completed`. Unit tests del clasificador (3).
- **Evidencia real: PENDIENTE** (parte de G5; no se aprueba por sustitución con
  el fake).

## Próximo (orden aprobado)

1. **G4 + G3:** permisos `ask-human` por defecto para ejecuciones reales (cada
   solicitud identifica tarea, run, herramienta y acción; la respuesta autoriza
   solo esa solicitud; cancelación/timeout/cierre ⇒ sin autorizar; `Auto` solo
   explícito) y selección/asignación de perfil desde la TUI.
2. **G2:** N=2 experimental solo con perfiles gestionados y workspaces distintos;
   default single-Hermes.
3. **G5 + G8/G9:** RH-1..RH-6 con Hermes real (D1: free, perfiles desechables),
   presupuesto D2 medido (antes de sesión, post-turno, varios turnos), barrido de
   procesos/locks por identidad; evidencia fake y real separadas.
4. **Auditoría independiente** antes de habilitar N=2 como comportamiento
   soportado.

## Precisiones de alcance registradas (frente paralelo)

- **A5 — validar sin integrar ni liberar `main`.** La validación de un snapshot
  debe poder ejecutarse sin integrar y sin exigir que `main` esté libre; se
  conservan las guardas del checkout destino. Un worktree adicional **no**
  resuelve por sí solo que la rama esté ocupada: la solución debe desacoplar
  validación de integración y respetar las guardas.
- **A6 — relación extremo a extremo.** Se debe conservar y mostrar el vínculo
  snapshot revisado → validación → commit/rama entregada → PR (sin perder qué
  se revisó, qué se validó y qué se entregó).

## Límites vigentes

- Sin credenciales reales en esta rama ni en los tests; D1 se ejecutará solo con
  perfiles desechables y autenticación device-code del titular.
- Sin merge ni habilitación general de multi-Hermes.
- Presupuestos de Aeril, estilo aprobado (90/100) y evidencia histórica sin
  cambios.
