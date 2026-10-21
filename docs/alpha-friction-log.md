# Alpha Linux — recorrido desde una instalación limpia: registro de fricciones

**Objetivo:** validar el milestone propuesto **“Alpha Linux para dos agentes reales”**
desde el punto de vista de una persona ajena al proyecto, usando **solo la TUI**
cuando sea posible, y registrar cada punto donde necesita salir a la CLI, editar
configuración interna o preguntar al autor.

**Binario:** release construido desde `origin/main @ 8765580` (5.928.496 B).
**Agente:** mock controlado (sin credenciales reales; el flujo de agente real es
P2-S2, ver `docs/phase2-s2-readiness.md`).
**Fixtures:** desechables en `/tmp`; capturas saneadas (sin rutas del host,
correos ni tokens). Sin procesos huérfanos al terminar (verificado).

---

## 1. Método

Dos pasadas sobre un repo de prueba real (Git, `NOTES.md` inicial):

1. **Pasada ingenua** (`scripts/alpha/alpha_journey.py --mode naive`): una
   persona que no conoce la arquitectura intenta el recorrido completo con la
   TUI. Registra fricciones y footguns.
2. **Pasada feliz** (`--mode happy`): mismas acciones con el conocimiento mínimo
   descubierto (crear tarea por CLI, elegir agente por flag, seleccionar el
   checkpoint antes de comentar, liberar `main` del checkout).

El recorrido objetivo era: *abrir repo → crear tarea → elegir agente → escribir
pedido → responder permisos → revisar cambios → comentar → enviar feedback →
validar → entregar*.

## 2. Qué funciona hoy (evidencia)

| Paso | Resultado | Evidencia |
|---|---|---|
| TUI arranca en un directorio vacío | ✅ estado vacío claro (“Select a task…”) | `naive-01-first-run.txt` |
| Ayuda `?` | ✅ lista de atajos | `naive-02-help.txt` |
| Abrir tarea | ✅ Enter; detalle con run, checklist y checkpoint | `happy-04-review-checkpoint.txt` |
| Lanzar run y estado **“está trabajando”** | ✅ `running`, `● Agent activity captured`, `● 1 run(s) active` | `happy-02-working.txt` |
| Estado **“terminó con este resultado”** | ✅ `completed` + mensaje final + timeline | `happy-03-completed.txt` |
| Revisar checkpoint | ✅ snapshot, OID, archivos, `verified · immutable`, patch con hunks y `PgUp/PgDn` | `happy-04-review-checkpoint.txt` |
| Comentar el checkpoint | ✅ (con el checkpoint seleccionado) → “Review notes 1 comment(s)” | `happy-05-comment-input.txt`, `happy-06-comment-saved.txt` |
| Enviar feedback | ✅ `feedbacksent 1 comments` → run de feedback → `feedbackdelivered 1 comments`, sesión reanudada | `happy-08-feedback-done.txt` |
| Validar / integrar | ✅ `merge fast-forward completed`; `main` avanza con el checkpoint (verificado con `git log`) | `happy-09-integrated.txt` |

**Conclusión:** el núcleo del recorrido (tarea → run → revisión → comentario →
feedback → integración fast-forward) **funciona desde la TUI** y la garantía de
checkpoint inmutable se aprovecha sin salir de la interfaz.

## 3. Fricciones encontradas (ordenadas por impacto)

### A. Bloqueantes para la alpha

| ID | Fricción | Evidencia | Fix propuesto |
|---|---|---|---|
| **A1** | **No hay onboarding de repositorio/tarea en la TUI.** El primer arranque muestra una lista vacía y ninguna acción; abrir un repo y crear la tarea exige `aeril thread new --repo … --title …` desde la CLI. | `naive-01-first-run.txt`; paso CLI del driver | Acción “nueva tarea” en la TUI (selector de directorio/repo + título), o `aeril` con `--repo` que cree la primera tarea guiada. |
| **A2** | **El agente se elige fuera de la TUI** (flag `--backend` al lanzar). No hay selector por tarea ni configuración visible del agente. | Lanzamiento `--backend mock:…` | Selector de agente/perfil en la TUI; persistir la elección por tarea. |
| **A3** | **Los permisos no son del usuario:** la TUI fija `PermissionPolicySetting::Auto`; el timeline muestra `allow_once (auto_allow_once)`. El paso “responder permisos” del recorrido **no existe**. | `happy-04-review-checkpoint.txt` (perm auto_allow_once); `src/tui/mod.rs` líneas 432/528 | Modo ask-human en la TUI con bandeja de permisos por tarea y estado “necesita mi intervención”. |
| **A4** | **Comentar un checkpoint es un footgun.** `c` exige el evento `checkpoint.ready` seleccionado; si no, el texto se interpreta como **atajos** en modo Normal: la `i` de “bien” abrió el input de integración y corrompió la rama destino (`maino se ve bien…`); una `q` habría cerrado la TUI. La selección no es evidente (solo color). | `naive-12-comment-footgun.txt` (mensaje literal) | Modo de entrada explícito para comentarios desde el panel de review; bloquear atajos mientras se escribe; indicador de selección visible en el timeline. |
| **A5** | **Integrar falla si la rama destino está en el checkout principal:** “la rama destino checkout principal está siendo usada por un checkout: integrá desde un checkout limpio”. El caso más común (un solo clon en `main`) no puede validar desde la TUI. | `naive-16-integrate-guard.txt` | Integrar en un worktree/checkout dedicado gestionado por Aeril, o explicar y ofrecer el cambio de rama desde la TUI. |
| **A6** | **No hay entrega (commit/push/PR)** ni en la TUI ni en la CLI: el paso final del recorrido exige salir a git manualmente y Aeril pierde la relación revisado↔validado↔entregado. | Ayuda de la TUI (sin atajo); CLI sin subcomando | Hito propio (workstream 3): commit + push + apertura de PR con rama destino, cambios y estado de validación visibles. |

### B. Usabilidad / distribución

| ID | Fricción | Evidencia | Fix propuesto |
|---|---|---|---|
| **B1** | **El estado de finalización/merge se atrasa** en cabecera/status hasta la siguiente tecla o `r`: tras terminar el run la cabecera seguía en `1 run(s) active`; tras integrar, el status seguía en `run completado`. El timeline sí se actualiza. | `happy-03-completed.txt`, `happy-09-integrated.txt` | Refresco periódico/por evento del run summary (sin exigir keypress). |
| **B2** | **`aeril --version` y `aeril --help` panickean sin TTY** (caen al TUI). No hay `help` ni `doctor`. | Probe CLI: panic `failed to initialize terminal` | Flags estándar `--version`/`--help`; `aeril doctor` (Git, backend, auth, data dir) sin secretos. |
| **B3** | **Sin diagnóstico de entorno**: no hay verificación de Git/backend/auth ni explicación del data dir en el primer arranque. | — | `doctor` + primera ejecución guiada. |
| **B4** | **Latencia de uso con historiales grandes**: el cierre aceptado registra búsquedas de checkpoint de **~2–3,5 s con 50.000 checkpoints**; el arranque de 22 ms no describe esa experiencia. | `docs/phase2-tui-acceptance.md` §6 | Índice/consulta acotada por snapshot; medir latencia de interacción (no solo startup). |
| **B5** | **Estado de feedback por comentario** poco visible: el panel muestra “1 comment(s)”; el timeline sí distingue `feedbacksent`/`feedbackdelivered`. | `happy-08-feedback-done.txt` | Mostrar estado por comentario (pendiente/enviado/entregado) en el panel. |

## 4. Recorrido paso a paso (resultado)

| Paso del recorrido | ¿TUI? | Notas |
|---|---|---|
| Abrir repositorio | ❌ CLI | A1 |
| Crear tarea | ❌ CLI | A1 |
| Elegir agente | ❌ flag CLI | A2 |
| Escribir pedido | ✅ `n` | — |
| Responder permisos | ❌ automático | A3 |
| Revisar cambios | ✅ `Tab`, `↑↓`, `PgUp/PgDn` | — |
| Comentar | ⚠️ requiere seleccionar `checkpoint.ready` | A4 |
| Enviar feedback | ✅ `f` | — |
| Validar (integrar) | ⚠️ solo si la rama destino no está en el checkout | A5 |
| Entregar (commit/push/PR) | ❌ no existe | A6 |

**Intervenciones externas necesarias para completar el recorrido:** 4 (crear
tarea, elegir agente, seleccionar checkpoint, liberar `main`) + 1 inexistente
(entrega). **Preguntas al autor:** 0 una vez conocido el truco de selección.

## 5. Prioridad recomendada (alineada al milestone)

1. **A6 + A5** (entrega y validación multiagente): sin esto no hay “trabajo
   entregado”; es el hito propio del workstream 3.
2. **A3** (permisos humanos): condición del recorrido y de “necesita mi
   intervención”; además es requisito de P2-S2 (“los permisos llegan a la tarea
   correspondiente”).
3. **A1 + A2** (onboarding): sin esto una persona ajena no puede empezar sola.
4. **A4** (footgun de comentario): riesgo de acciones destructivas por tipeo.
5. **B1/B2/B3** (pulido de estado y distribución) y **B4** (latencia en uso).

## 6. Reproducción

```bash
# Binario release desde origin/main
cargo build --release
python3 scripts/alpha/alpha_journey.py --bin target/release/aeril --mode happy
python3 scripts/alpha/alpha_journey.py --bin target/release/aeril --mode naive
```

El driver crea un repo y data dir desechables, usa el agente mock (sin
credenciales), captura pantallas saneadas y limpia sus procesos por identidad.

## 7. Límites de este registro

- No se probaron agentes reales (P2-S2 sigue HOLD; ver
  `docs/phase2-s2-readiness.md`).
- No se probó concurrencia N=2 real; el recorrido fue de un solo agente mock.
- Las capturas son reconstrucciones de texto del visor (color/atributos no
  visibles); los PNG de aceptación existentes cubren la parte visual.
- No se evaluó instalación desde un paquete (binario copiado, no `.deb`/tarball
  firmado); eso pertenece al workstream 5.
