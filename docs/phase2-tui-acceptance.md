# Aceptación TUI — workbench de tres paneles (fase de aceptación final)

**Rama:** `feat/tui-workbench-redesign` · **Base:** `main @ 3a28af2b5acb7efa94540d15e4fe068af89259e1`
**Tip de código auditado:** `6255957c4e6048f866be910fea2052f343667b39`
**Tip de la rama al cierre de este paquete:** ver §9 (push posterior a esta entrega)
**PR:** ninguno abierto para el TUI; el trabajo vive en la rama de feature (sin merge, sin tag, sin release).

> **Alcance:** verificación de aceptación (historias largas, regresión, presupuestos,
> capturas, higiene de procesos). **No** es un cambio de producto: el código de
> producción solo cambia en dos lecturas acotadas (`checkpoint::latest_ready`,
> `checkpoint::ready_for_snapshot`) con **misma semántica** y memoria constante.
> Sin P2-S2, sin tocar los invariantes de seguridad del subsistema de perfiles, sin
> credenciales reales, sin perfiles de producción.

---

## 1. Provenance: qué es `6255957` y por qué faltaba en el remoto

Auditoría de refs (locales + remoto) antes de actuar:

| Dato | Valor |
|---|---|
| SHA completo | `6255957c4e6048f866be910fea2052f343667b39` |
| Padre | `253307171aa870c8a85f27c8d9a01a25a5c1b926` (**exactamente** el HEAD remoto declarado) |
| Rama / worktree | `feat/tui-workbench-redesign` · `/home/elliot/aeril-tui` (limpio, `ahead 1`) |
| Estado remoto antes | `refs/heads/feat/tui-workbench-redesign = 2533071` (el commit **no** estaba pusheado) |
| Diff vs `2533071` | 2 archivos: `src/store/events.rs` (+45), `src/tui/mod.rs` (+182/−29) — sin tests en archivos aparte |
| Contenido | fix de **historia acotada**: `events::run_progress` (proyección SQL: `EXISTS` + `ORDER BY seq DESC LIMIT 1`) y la TUI consume el progreso del run **fuera** de la ventana de 100 eventos; test in-crate `long_history_does_not_hide_current_run_progress` (150 y 1.000 eventos) |

**Veredicto:** descendiente directo válido del HEAD remoto declarado, con el fix
esperado y su test, alcance acotado y worktree limpio ⇒ **publicado** como
fast-forward `2533071..6255957` (sin merge, sin tag).

```bash
git ls-remote origin refs/heads/feat/tui-workbench-redesign
# 6255957c4e6048f866be910fea2052f343667b39
```

## 2. Verificación de historias largas (matriz completa)

Criterio: **la verificación no puede resolverse cargando el historial completo en
memoria.** Por eso se asertan (a) ventana acotada y (b) memoria estable.

| Escenario | Evidencia | Resultado |
|---|---|---|
| 150 eventos en un run | `one_run_with_150_events_keeps_progress_and_bounded_window` | ✅ ventana = 100; `run.started` fuera de la ventana; progreso completo (goal/sesión/actividad/checkpoint) |
| 1.000 eventos en un run | `one_run_with_1000_events_keeps_progress_and_bounded_window` | ✅ idéntico |
| Dos runs secuenciales + checkpoint verificado antiguo | `two_sequential_runs_keep_historical_checkpoint_selection` | ✅ `run_progress` aislado por run; la selección de checkpoint sigue siendo la **histórica** (`snap-old`, run viejo) |
| Eventos recientes sin `run.started` | `recent_window_without_run_started_keeps_durable_goal` | ✅ el goal sigue disponible (hecho durable, no de la ventana) |
| Run activo sin checkpoint | `active_run_without_checkpoint_does_not_invent_one` | ✅ `has_checkpoint=false`; `latest_ready` = `None`; no se inventa checkpoint |
| Run completed / failed | `completed_and_failed_runs_expose_terminal_state` | ✅ estado terminal expuesto; hitos por run; checkpoint del run completado preservado |
| Selección de checkpoint histórico con run más nuevo activo | test in-crate `historical_checkpoint_is_shown_while_a_newer_run_is_active` | ✅ el panel muestra el checkpoint del run viejo; el run nuevo sigue activo |
| Semántica de hitos (TUI) | tests in-crate: run activo sin checkpoint → paso **en curso** `●`; run terminal sin checkpoint → `✕`; run fallido → `Run finalized ✓` | ✅ |
| Memoria estable (1.500 eventos + 500 checkpoints) | `tui_refresh_path_does_not_materialize_full_history` | ✅ delta RSS < 24 MiB tras 20 ciclos de refresh (ventana + proyección + checkpoint) |

**Hallazgo de la verificación (acotado en este paquete):** la selección de
checkpoint materializaba **todos** los `checkpoint.ready` del thread
(`fetch_by_kind` + `pop`). Se acotó manteniendo la **semántica certificada**:
cursor en streaming ordenado por `seq DESC`, deserializando y **validando cada
registro antes** de comparar su `snapshot_id`, y propagando los errores de
validación (memoria constante, sin materializar el historial). Una variante
intermedia que filtraba por `json_extract(payload,'$.snapshot_id')` en SQL era
más rápida pero **cambió la semántica** (se saltaba checkpoints
estructuralmente inválidos y admitía operaciones que antes fallaban); esa
regresión fue detectada en la auditoría y está cerrada aquí — ver §10 (P2
remediation) con evidencia RED/GREEN determinista. El camino de *comentarios*
(`review::list_comment_summaries`) sigue cargando el historial de comentarios del
thread: no se toca (volumen acotado por actividad humana, no por eventos de
agente) y queda reportado como limitación conocida (§7).

## 3. Regresión y comandos

```bash
cd <worktree de la rama TUI>
cargo test --no-fail-fast                 # 25 binarios
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build --release                     # forzar: el target $(BIN) del Makefile
                                           # no depende de las fuentes (§7)
make budget && make budget-two && make budget-scale
```

| Gate | Resultado |
|---|---|
| Suite completa | **218/218**, EXIT=0 (25 binarios; 207 previos + 11 nuevos) |
| `cargo fmt --check` | limpio |
| `cargo clippy --all-targets -- -D warnings` | limpio |
| Perfiles (P2-S1) | `tests/phase2_s1_{profiles,r1,r2}.rs` verdes (sin cambios en este paquete) |

## 4. Binario release y recursos (SHA de código auditado + este paquete)

```text
budget-size : 5,920,624 bytes (5.9 MB) ≤ 30 MB                        → PASS
budget-deps : PASS (solo ABI base del OS + zlib del sistema)
budget-mem  : idle 7.0 MB · thread abierto 7.9 MB · delta 0.89 MiB  → PASS
budget-start: p95 31.1 ms ≤ 300 ms                                    → PASS
budget-two  : TUI 9,120 KB (8.9 MB) · mocks 4,756/4,656 KB
              combinado 18,532 KB (18.1 MB) · arranque 2 activos 52.8 ms
              consultas avg 7.9 ms / max 11.2 ms                      → PASS
budget-scale: app 10.2 MB (gate 500 MB) · agentes 23.0 MB · total 33.1 MB → PASS
```

### Comparación contra el baseline certificado anterior (Fase 1)

| Métrica | Baseline Fase 1 | Actual (rama TUI) | Δ | Gate |
|---|---|---|---|---|
| TUI RSS (2 runs) | 8.1–8.3 MB | 8.9 MB | +0.7 MB (+8%) | informativo |
| RSS mock (c/u) | 4.0 MB | 4.5–4.6 MB | +0.5 MB | informativo |
| RSS combinado | ~16.6 MB | 18.1 MB | +1.5 MB | informativo |
| Startup p95 | 12.7–13.9 ms | 31.1 ms | ×2.3 | ≤300 ms ✅ |
| Latencia de consulta | ~3.1 ms | 7.9 ms (avg) | ×2.5 | informativo |
| RSS idle | 6.8 MB | 7.0 MB | +0.2 MB | ≤50 MB ✅ |
| Delta por thread | 0.79–0.86 MiB | 0.89 MiB | +0.04 | ≤10 MiB ✅ |
| Binario | 5.6 MB | 5.9 MB | +0.3 MB | ≤30 MB ✅ |
| scale: app | ~9.8 MB | 10.2 MB | +0.4 MB | ≤500 MB ✅ |

**Lectura:** el rediseño de TUI **no rompe ningún gate**; la deriva es real pero
acotada (+8% RSS de TUI, ×2.3 latencia de arranque绝对 dentro de un budget de
300 ms). Queda como riesgo operativo vigilado (§7), no como bloqueo.

## 5. Aceptación visual

**Resultado de aceptación visual: 90/100** (proporcionado por el brief de
aceptación; la captura independiente de este paquete corrobora las tres
geometrías pedidas).

Capturas en `docs/phase2-tui-acceptance/screenshots/` (visor de terminal
reconstruido desde un pty real con `TIOCSWINSZ`, más el flujo ANSI crudo):

| Geometría | Artefactos | Contenido verificado |
|---|---|---|
| 180×50 | `tui-180x50.png` · `.txt` · `.ansi` | tres paneles; el panel central muestra el run (goal, pasos 5/5), el derecho el checkpoint verificado (snapshot, OID, ficheros) |
| 120×40 | `tui-120x40.png` · `.txt` · `.ansi` | degradación por truncado (`.txt` con el nombre de tarea acortado) sin solapes ni pérdida de estructura |
| 80×24 | `tui-80x24.png` · `.txt` · `.ansi` | panel de checkpoint colapsado (sin panel visible); el detalle del run conserva goal, pasos y timeline |

Higiene de las capturas: **sin rutas del host, sin correos, sin tokens** (scan
automático; las únicas coincidencias son cabeceras de hunk `@@ -1,3 +1,3 @@` de un
diff, no datos sensibles). Los fixtures de captura son desechables en `/tmp`.

## 6. Limpieza de procesos (hallazgo histórico de PTY/TUI)

**Histórico.** El 17-oct quedaron **8 TUIs huérfanas** (`aeril --backend
mock:/tmp/aeril-test-s3-t*`) en busy-loop a ~80% CPU cada una (~6.5 núcleos),
reparentadas a `systemd` porque su harness de tests murió por señal (sin
`Drop`). Se limpiaron por identidad (cmdline + cwd) sin tocar procesos ajenos.

**Reproducción controlada (fixture desechable, marcador único en
`AERIL_DATA_DIR`, identidad por pid+starttime+environ):**

| Escenario | Resultado |
|---|---|
| A) operación normal (fin de suite) | **0 procesos de test vivos** tras la suite completa (218/218) |
| B) holder muere por SIGTERM (caída ordenada) | TUI **muere** con el holder; 0 huérfanos |
| C) holder muere por SIGKILL (sin `Drop`), TUI ociosa | TUI **muere** (EOF de pty) ✅ |
| D) holder muere por SIGKILL, TUI con **run activo** | TUI **muere** ✅ |
| E) holder muere por SIGKILL, TUI con **flood** de eventos | TUI **muere** ✅ |
| F) A/B con el binario **pre-rediseño** (`main`) | también muere ✅ |

**Conclusión:** con el código actual **no se reproduce** la fuga de huérfanos
(operación normal e interrumpida). La severidad residual es **P3 (higiene del
harness)**: si un árbol de tests completo muere por señal, la TUI puede quedar
 viva hasta que el pty le avise; no hay impacto en producción (la TUI de
producción no es hija de un harness killed y sale con EOF de pty).

**Riesgo residual (no bloqueante):** no existe `PR_SET_PDEATHSIG` en el spawn
del harness PTY, así que la dependencia del EOF de pty es implícita.
**Recomendación (fuera de este paquete):** `pre_exec` con `PDEATHSIG=SIGKILL`
en `tests/common/pty.rs` + un barrido de huérfanos por marcador al final de la
suite. Nada de esto afecta al producto.

**Leftovers históricos identificados (NO se tocaron):** un proceso
`/home/elliot/aeril/target/debug/aeril run start --thread 047e8cc6…` vivo desde
el 17-oct 19:45 (≈2 días) en el worktree de `main` — ajeno a este trabajo y a
esta rama; y dos fakes dormidos de la auditoría
(`p2s1-audit-scratch4/5`, 0% CPU). Se dejan intactos: la auditoría decide.

## 7. Limitaciones conocidas de la terminal

1. **80×24**: el panel de checkpoint no cabe y se colapsa (sin checkpoint visible
   en la vista de lista). Comportamiento deliberado de degradación, no pérdida
   de datos (el detalle sí lo muestra).
2. **Truncado con elipsis** en nombres largos de tarea (`ejemplo de tar…`) a
   120/80 columnas.
3. **Sin terminal (sin tty):** la TUI hace panic de inicialización de ratatui
   (`failed to initialize terminal: No such device or address`, exit 101) en vez
   de un mensaje de error limpio. Comportamiento **preexistente** (no de este
   rediseño) y fuera del alcance de aceptación; candidato a endurecer.
4. **Rutas absolutas en la cabecera** (workspace path) se muestran en la TUI; en
   las capturas del paquete fueron saneadas, pero la TUI real las muestra.
5. **El target `$(BIN)` del Makefile no depende de las fuentes:** `make budget`
   puede reutilizar un binario release viejo. Hay que forzar
   `cargo build --release` antes de medir (documentado en §3 y en el closeout P2-S1).

## 8. Riesgos operativos restantes

| Riesgo | Nivel | Estado |
|---|---|---|
| Deriva de latencia/RSS de la TUI (+8% RSS, ×2.3 startup) | Bajo | gates en verde; vigilar en la próxima iteración |
| `PDEATHSIG` ausente en el harness PTY (P3) | Bajo | no reproducible con el código actual; recomendación §6 |
| TUI sin tty → panic en vez de error limpio | Bajo | preexistente, fuera de alcance |
| `review::list_comment_summaries` carga el historial de comentarios del thread | Bajo | acotado por volumen humano; no tocado en este paquete |
| TUI ↔ subsistema de perfiles | Nulo | la TUI no referencia `profile::` ni el flag; el gate single-Hermes sigue vigente (`is_hermes_backend`, extendido a prefijos `hermes:`) |

## 9. Commits de esta rama y HEAD remoto

| SHA | Asunto |
|---|---|
| `6255957c4e6048f866be910fea2052f343667b39` | fix(tui): make run progress independent of recent-event window (publicado en esta misión) |
| `8b5ba89ff34ee79cc39a978a6cb82d6dadf74d2d` | test(tui): verificar historias largas y acotar la búsqueda de checkpoints (este paquete) |
| (este commit) | docs(tui): paquete de aceptación TUI (este paquete) |

**HEAD remoto exacto de `feat/tui-workbench-redesign` al cierre:** el tip publicado de
la rama (commit de este paquete) — el SHA exacto se registra en el handoff de la
misión y en `git ls-remote origin refs/heads/feat/tui-workbench-redesign`.

**Main sin tocar:** `3a28af2b5acb7efa94540d15e4fe068af89259e1` (sin merge, sin
tag, sin release).

## 10. P2 remediation — semántica de `ready_for_snapshot` (cierre del HOLD)

**Regresión (HOLD de auditoría):** la variante con `json_extract` filtraba por
`snapshot_id` **en SQL**, por lo que nunca deserializaba los registros que no
coincidían; un `checkpoint.ready` más nuevo y estructuralmente inválido quedaba
sin detectar y la operación que antes fallaba ahora tenía éxito.

**Corrección (commit único, alcance mínimo):** `checkpoint::ready_for_snapshot`
vuelve a la semántica certificada — cursor en streaming `seq DESC`,
deserialización y **validación de cada registro antes** de comparar el
`snapshot_id`, y **propagación** de los errores de validación — manteniendo la
memoria acotada. `latest_ready()` (optimización aceptada) queda intacto.

**Evidencia determinista:**
`docs/phase2-tui-acceptance/evidence/red-checkpoint-semantics.txt` (RED: 3/8
fallan, `prod=Ok("ok:snap-ok")` vs `cert=Err("json: missing field
\`snapshot_id\`")`) → `green-checkpoint-semantics.txt` (GREEN: 8/8).

**Matriz obligatoria cubierta** (`tests/phase2_tui_checkpoint_semantics.rs`,
8 tests): snapshot válido existente · múltiples snapshots y snapshot_ids
duplicados (gana el más nuevo) · checkpoint más nuevo no relacionado
estructuralmente inválido (debe propagar) · campos obligatorios faltantes y
tipos incorrectos · snapshot pedido inexistente · historiales de 1.000 y 50.000
checkpoints · **paridad con el algoritmo certificado** (comparación directa
contra una reimplementación local del algoritmo de `main@3a28af2`).

**Latencia del lookup (peor caso: snapshot más viejo ⇒ recorrido completo):**

| Checkpoints | Streaming (nuevo) | Certificado (`main`, materializa) |
|---|---|---|
| 1.000 | 38.0 ms | 49.6 ms |
| 50.000 | 1.981 ms | 2.432 ms |

## 11. Recomendación para aceptación final independiente

**Recomendación: aceptar el TUI (rama `feat/tui-workbench-redesign`) para
integración**, con estas condiciones:

1. Revisión de los dos cambios de acotado (`checkpoint::latest_ready`,
   `checkpoint::ready_for_snapshot`): semántica idéntica verificada por los
   tests de historial largo y los de la suite completa (218/218).
2. Re-auditoría de la deriva de TUI (RSS +8%, startup ×2.3) contra el presupuesto
   por sesión que se defina para P2-S2.
3. `PDEATHSIG` en el harness PTY como hardening del P3 (no bloquea la
   integración; es deuda del harness).
4. Tras la integración autorizada: actualizar el GIF de la demo pública
   (`docs/public-status-demo`) para mostrar exactamente esta implementación
   (paso pendiente, condicionado a la autorización de merge).

**Bloqueos para la aceptación:** ninguno en el código. **Pendiente de
decisión humana:** autorización explícita de merge de la rama TUI (y la
actualización del GIF de la demo, que hoy muestra una implementación anterior).
