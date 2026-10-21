# Aceptación TUI — integración final con hotfixes SQLite y B3

**Veredicto: PASS.** El fallo intermitente del fixture escapado quedó corregido de forma determinista y las tres suites completas pasan. El candidato se clasifica **READY FOR HUMAN MERGE AUTHORIZATION**.

| Componente | Identidad |
|---|---|
| Certified main remoto | `080992cc51641a483107ffa30e2c4da48d7933ea` |
| Hotfix SQLite (PR #2) | `b2184fd41ac24cc81f890aeeeb816af255683e99` |
| Hotfix B3 Git recovery (PR #3) | `05774024a959757feadb27e9024eca4641cab751` |
| Candidato TUI fuente | `18d6910a19324e954cb89b2bce4502755c8e37fb` |
| Merge base congelado | `a0a55b093fcfd3fee770c5d3c9f35c51801b988d` |
| Código final probado (fixture R3) | `90adb61c8945f93fc8b0fba4f962494ba667e732` |
| Tree de código final probado | `ca1bb46fa616fb76943e5159bd9c76c7322b8ee7` |
| Padre del código final | `670afcbb4882a33df749d8066753320362895f68` |
| Rama local | `integration/tui-r2-final-acceptance-r3` — no publicada |

El merge original `a0a55b0` fue normal, de dos padres, sin conflictos. La corrección R3 cambia únicamente el fixture de test `tests/common/fake_hermes.py`; no cambia producción. El parent `670afcb` aporta el cierre documental previo. No se modificó `main`, la rama TUI, ni ningún umbral.

> **Alcance:** aceptación de integración y fiabilidad del harness. Diseño TUI congelado (90/100). No se reabre P2-S1 ni se implementa P2-S2. Sin credenciales ni perfiles de producción. P2-S2 permanece HOLD.

## 1. Entorno y procedencia

- Worktree: `/home/elliot/.local/share/aeril-tui-r3-final-acceptance-20261021/checkout`.
- Target nuevo, inicialmente ausente; no se reutilizó el target de desarrollo.
- Ubuntu 26.04.1 LTS; Linux `7.0.0-38-generic` x86_64; Intel i7-8550U, 8 CPU lógicas, 15.4 GiB RAM, ext4.
- Rust/Cargo `1.93.0`; Git `2.53.0`.
- Builds/tests `--locked --offline`; sin fallback de red ni cambio de lockfile.
- `origin/main` y `origin/feat/tui-workbench-redesign` coinciden con los SHAs declarados. SQLite `b2184fd` y B3 `0577402` son ancestros de main.
- Worktrees y ramas fuente se registraron antes de actuar; `main` local permanece en `3a28af2` con sus dos `.audit/` no rastreados preexistentes. La rama de integración se queda local.

## 2. Root cause del fixture escapado

El test original hacía:

1. `session/prompt` → `os.fork()`.
2. El hijo debía `os.setsid()` y escribir `fake-escaped-pid`.
3. El padre emitía `end_turn` inmediatamente, sin esperar al hijo.

Si Aeril terminaba el process group original antes de que el hijo ejecutara `setsid()`, el hijo era eliminado junto al grupo; no quedaba superviviente y el run terminaba `completed`. La inyección diagnóstica de `time.sleep(0.5)` antes de `setsid()` reprodujo el fallo de forma determinista (`completed` vs `failed`). La evidencia RED está en `artifacts/diagnostic-delayed-setsid.patch` y `logs/red-delayed-setsid.*`; el fallo histórico de `full-1` (229/230 con `r3_p2_1_escaped_backend_prevents_owner_cleanup`) se conserva.

## 3. Corrección test-only

`tests/common/fake_hermes.py` usa ahora un handshake `os.pipe()`:

- El hijo ejecuta `setsid()` primero.
- Lee su `starttime`, escribe `fake-escaped-pid` como `pid start`, hace `flush` + `fsync`, lo relee y verifica liveness propia.
- Solo entonces escribe `b"1"` en el pipe y queda vivo en la sesión escapada.
- El padre espera con `select` un máximo de 5 s; valida PID, starttime, estado no-zombie y `getsid(pid)==pid`; solo entonces emite el ACP `end_turn`.
- Si no hay readiness válida, mata al hijo conocido y falla el fixture explícitamente; nunca emite `end_turn` silenciosamente.

No se modificaron `tests/phase2_s1_r2.rs`, `src/profile.rs`, `verify_no_survivor`, `finish_writer` ni ninguna ruta de producción. El test conserva `state=failed`, `fake-escaped-pid`, `owner.json` y `lock_state=held`.

## 4. Regresión completa

Inventario real: **27 targets / 230 tests**. Tres suites consecutivas después de la corrección:

| Intento | Resultado | Exit |
|---|---:|---:|
| `full-r3-1` | **230/230** | 0 |
| `full-r3-2` | **230/230** | 0 |
| `full-r3-3` | **230/230** | 0 |

El fallo histórico `full-1` de 229/230 no se borra; está preservado en la evidencia del gate anterior. Los tres intentos de esta revisión son independientes y completos.

## 5. Stress de la corrección R3

- `r3_p2_1_escaped_backend_prevents_owner_cleanup`: **300/300 aislado + 300/300 bajo carga**.
- Los workers de carga ejecutaron la suite P2-S1 completa; un worker registró un fallo **distinto** en `r3_p2_1_long_lived_supervisor_owner_is_cleared_after_run`: `processes_using_home_in` devolvió `ESRCH` (`No such process`) mientras un proceso desaparecía entre `readdir` y la lectura de `environ`. El comportamiento es fail-closed: el run no se reporta limpio y se conserva `owner.json`. Es un race preexistente de producción, no del fixture `escape-on-turn`; queda como follow-up no bloqueante. No se alteró producción en esta misión test-only.
- No quedaron procesos escapados, fake-Hermes, PTY/mock, lockfiles ni directorios temporales.

## 6. SQLite, B3 y P2-S1

- SQLite `r2_h4_concurrent_migration_is_serialized`: **100/100 aislado + 100/100 bajo carga**; `Barrier::new(24)` intacta; migración interrumpida 2/2.
- WAL: deadline **5,000 ms**; fallo no migra el esquema; errores no transitorios se propagan.
- B3 `b3_recuperacion_concurrente_no_duplica_ready`: **300/300 aislado + 300/300 bajo carga**; 8 ciclos de workers sin fallos; 6/6 pruebas B3 dirigidas.
- P2-S1 R3 owner tests: **4/4** dirigidos; el test escapado corrige la sincronización sin debilitar seguridad.
- TUI: checkpoint semantics **8/8**, history **7/7**. Latencias del gate: lookup 1,000 **34.2 ms streaming / 38.5 ms certificado**; 50,000 **3,573.5 ms / 2,222.2 ms**. Son informativas; no hay threshold de lookup.
- `ready_for_snapshot()` conserva `seq DESC`, validación de cada registro antes de comparar `snapshot_id` y propagación de errores. `latest_ready()` conserva su proyección acotada. RED/GREEN histórico intacto: 7 tests/3 fallos → 8/8.

## 7. Release y budgets

Binario: **5,928,496 bytes**, SHA-256 `64ed48a52a558e216bc400ab550759e30936bc3e77dab53226ce5eab71f3618c`.

| Medida | TUI integración previa | R3 final | Δ | Umbral |
|---|---:|---:|---:|---:|
| Binario | 5,923,632 B | 5,928,496 B | +4,864 B / +0.08% | ≤30 MB PASS |
| RSS idle / thread abierto | 6,992 / 7,984 KiB | 7,276 / 8,248 KiB | +4.1% / +3.3% | ≤50 MB PASS |
| Delta por thread | 0.97 MiB | 0.95 MiB | −2.1% | ≤10 MiB PASS |
| Startup p95 | 21.61 ms | 22.21 ms | +2.8% | ≤300 ms PASS |
| TUI RSS, 2 runs | 8,932 KiB | 8,912 KiB | −0.2% | app ≤500 MB PASS |
| RSS combinado, 2 runs | 17,944 KiB | 18,052 KiB | +0.6% | informativo |
| Arranque con 2 activos | 51.66 ms | 51.27 ms | −0.8% | informativo |
| Query avg / max | 5.82 / 9.24 ms | 5.71 / 8.00 ms | −1.9% / −13.4% | informativo |
| Scale app / agentes / total | 10.06 / 21.83 / 31.89 MiB | 10.17 / 22.34 / 32.51 MiB | +1.1% / +2.4% / +2.0% | app ≤500 MB PASS |

No se cambió ningún límite. Los agentes de scale son mocks; no predicen Hermes real.

## 8. Integridad, seguridad y alcance

- Diseño visual aprobado (90/100) intacto; capturas y evidencia histórica sin modificar.
- D7: dos literales `"Fase 1"` → `"Aeril"` en `tests/phase1_concurrency.rs` y `tests/phase1_s3.rs`; asserts, deadlines y conteos intactos.
- Managed profiles default-off; single-Hermes enforced; sin credenciales/perfiles de producción.
- Sin migraciones nuevas por TUI; sin P2-S2.
- D5 sigue retirado: main y TUI ya reconocen prefijos `hermes:`.
- Follow-ups: race ESRCH en `processes_using_home_in` (fail-closed, preexistente); `latest_ready` con un `ready` recuperado tarde; captura transitoria que puede marcar el run `failed` aunque el checkpoint siga recuperable; tie-break de listas TUI con más de 200 threads; PDEATHSIG P3.

## 9. Procesos y filesystem

Barrido final por identidad: **0** procesos del gate, **0** fake-Hermes, **0** procesos mock/PTY, **0** lockfiles de test o checkout, **0** directorios temporales. No se enviaron señales ni se usó limpieza por nombre. Procesos históricos ajenos preservados.

## 10. Cierre documental y D1–D7

- **D1:** código final probado `90adb61c8945f93fc8b0fba4f962494ba667e732`, tree `ca1bb46fa616fb76943e5159bd9c76c7322b8ee7`.
- **D2/D6:** **230 tests / 27 targets**; resultados 230/230, 230/230, 230/230. El fallo histórico 229/230 queda preservado, no reescrito.
- **D3:** semántica de checkpoint corregida y verificada 8/8; streaming reverse-seq con validación por registro.
- **D4:** recursos actuales medidos y comparados con la integración TUI previa; no se reutilizan como actuales.
- **D5:** inválido, retirado.
- **D7:** dos cambios de literal, sin debilitar asserts.
- RED/GREEN histórico: 7 tests/3 fallos y 8/8, preservados.

El cierre documental es un commit separado en la misma rama. El SHA/tree final de documentación y la equivalencia byte a byte de código, tests, Cargo, Makefile, scripts y thresholds se registran en `artifacts/documentation-provenance.json` fuera del checkout. No se cherry-pickeó el merge antiguo.

### Clasificación final

**PASS — READY FOR HUMAN MERGE AUTHORIZATION.** El fixture race está corregido, las tres suites completas pasan, SQLite/B3/TUI/budgets y limpieza pasan. El follow-up ESRCH es fail-closed, preexistente y separado de la corrección del fixture; no invalida este gate.

**No se mergea, publica, tagea ni releasea sin autorización humana. P2-S2 sigue HOLD.**
