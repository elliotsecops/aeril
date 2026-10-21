# Aceptación TUI — integración final con hotfixes SQLite y B3

**Veredicto de este gate: HOLD.** El primer intento de suite completa falló en un test P2-S1 preexistente y ajeno al merge: `r3_p2_1_escaped_backend_prevents_owner_cleanup` observó `completed` donde exige `failed`. Los intentos 2 y 3 pasaron, pero no borran el fallo del intento 1 ni permiten declarar PASS.

| Componente | Identidad |
|---|---|
| Certified main remoto | `080992cc51641a483107ffa30e2c4da48d7933ea` |
| Hotfix SQLite (PR #2) | `b2184fd41ac24cc81f890aeeeb816af255683e99` |
| Hotfix B3 Git recovery (PR #3) | `05774024a959757feadb27e9024eca4641cab751` |
| Candidato TUI fuente | `18d6910a19324e954cb89b2bce4502755c8e37fb` |
| Merge local de aceptación | `a0a55b093fcfd3fee770c5d3c9f35c51801b988d` |
| Git tree combinado probado | `752f3b8b3f89de92785ce247cf02d8253e0ee1a5` |
| Padres del merge | `080992cc51641a483107ffa30e2c4da48d7933ea` + `18d6910a19324e954cb89b2bce4502755c8e37fb` |
| Rama local | `integration/tui-r2-final-acceptance` — no publicada |

El merge fue normal, no fast-forward, de dos padres. No hubo conflictos textuales. `src/checkpoint.rs`, `src/store/mod.rs` y `src/tui/mod.rs` se auto-mezclaron y fueron revisados para confirmar que ambos lados sobreviven. No se modificó `main`, la rama TUI, ni ningún presupuesto.

> **Alcance:** aceptación de integración, no rediseño. El diseño visual aprobado (90/100) queda congelado. No se reabre P2-S1 ni se implementa P2-S2. No se usan credenciales ni perfiles de producción. P2-S2 permanece HOLD.

## 1. Entorno y procedencia

- Worktree: `/home/elliot/.local/share/aeril-tui-final-acceptance-20261021/integration`.
- Target nuevo, inicialmente ausente; no se reutilizó el target de desarrollo.
- Ubuntu 26.04.1 LTS; Linux `7.0.0-38-generic` x86_64; Intel i7-8550U, 8 CPU lógicas, 15.4 GiB RAM, ext4.
- Rust/Cargo `1.93.0`; Git `2.53.0`.
- Builds y tests con `--locked --offline`; no hubo fallback de red ni cambio de lockfile.
- `origin/main` y `origin/feat/tui-workbench-redesign` coincidieron con los SHAs declarados. SQLite `b2184fd` y B3 `0577402` son ancestros de main; el candidato TUI desciende del main previo `3a28af2`.

Estado inicial registrado: `main` local estaba en `3a28af2` con los dos `.audit/` no rastreados preexistentes; la rama TUI estaba limpia. No se modificaron. La rama de integración no se publicó.

## 2. Merge y resolución

- Commit: `a0a55b093fcfd3fee770c5d3c9f35c51801b988d`; tree `752f3b8b3f89de92785ce247cf02d8253e0ee1a5`.
- Padres exactos: main `080992c…` y TUI `18d6910a…`.
- Resultado: **auto-merge limpio; cero conflictos textuales**.
- Auto-mezclados: `src/checkpoint.rs`, `src/store/mod.rs`, `src/tui/mod.rs`.
- `src/checkpoint.rs` conserva B3 (`Locked` retry, `Deferred`, conflicto terminal) **y** TUI (`latest_ready` acotado, `ready_for_snapshot` reverse-stream con validación por registro).
- `src/tui/mod.rs` conserva el rediseño aceptado; su único delta frente a la rama TUI es el brazo de resumen para `RecoveryOutcome::Deferred`.
- `src/store/mod.rs` conserva el retry WAL de SQLite junto al test TUI de `run_summaries`.
- El diff contra main contiene solo los 21 archivos TUI aprobados; no toca Cargo, Makefile, scripts, thresholds, perfiles ni migraciones.
- El diff contra la rama TUI contiene solo los cambios de main: SQLite, B3 y sus tests; no se perdió ningún hunk.

Revisión independiente del merge: **APPROVE WITH FINDINGS**, sin bloqueo. Confirmó que ambos parches parentales sobreviven y que no hay override semántico.

## 3. Gate de regresión completa

Inventario real: **27 targets / 230 tests**. Se ejecutaron tres suites consecutivas con `--jobs 2 --no-fail-fast`:

| Intento | Resultado | Exit | Estado |
|---|---:|---:|---|
| `full-1` | **229 passed, 1 failed** | 101 | **FAIL** — `phase2_s1_r2::r3_p2_1_escaped_backend_prevents_owner_cleanup` |
| `full-2` | **230 passed, 0 failed** | 0 | PASS |
| `full-3` | **230 passed, 0 failed** | 0 | PASS |

El fallo de `full-1` está preservado en `logs/full-1.{stdout,stderr}.log`: el run devolvió `state=completed` donde el test exige `failed` para un backend escapado. El test y `src/profile.rs`/`src/session.rs` son idénticos a main; el diff TUI no los toca. En aislamiento pasó **50/50** repeticiones (`artifacts/owner-cleanup-repeat-summary.json`) y también pasó en `full-2`/`full-3`. Es un fallo intermitente de timing del fixture P2-S1, no una regresión demostrada del TUI/B3/SQLite, pero **sigue siendo un fallo del gate**.

Bajo el criterio estricto de tres suites consecutivas, el candidato no es PASS. La remediación estrecha es estabilizar la sincronización del fixture `escape-on-turn`/detección del descendiente escapado (test-only, con revisión) y repetir el gate completo; no se autoriza cambiar producción ni relajar la aserción.

Otros gates:

| Gate | Resultado |
|---|---|
| `cargo build --release --locked --offline` | PASS |
| `cargo fmt --all -- --check` | PASS |
| `cargo clippy --locked --offline --jobs 2 --all-targets -- -D warnings` | PASS |
| Tests TUI dirigidos | **15/15** — semantics 8/8; history 7/7 |
| `make budget`, `budget-two`, `budget-scale` | PASS, sin cambios de umbral |

## 4. SQLite / P2-S1

- El test original `r2_h4_concurrent_migration_is_serialized` conserva `Barrier::new(24)`, 24 aperturas concurrentes, `user_version == 4` y `profile IS NULL`.
- Stress: **100/100 aislado + 100/100 bajo carga** de dos workers Wave B. Cero fallos.
- `r2_h4_interrupted_v4_ddl_is_idempotent`: PASS; DDL y `user_version` se aplican en una transacción `IMMEDIATE`.
- `sqlite_wal_retry`: PASS; deadline observado **5,000 ms**, `DatabaseBusy`, `user_version=0` y journal mode previo intacto tras fallar WAL.
- La contención SQLite no produjo migraciones parciales ni duplicadas.

Evidencia: `artifacts/sqlite-stress-overview.json`, `artifacts/sqlite-stress-*.jsonl`, `logs/target-wal-deadline.stdout.log`, `logs/target-sqlite.stdout.log`.

## 5. B3 Git recovery

- Pruebas dirigidas: **6/6**.
- Stress del test original: **300/300 aislado + 300/300 bajo carga**; dos workers completaron 14 ciclos de Wave B sin fallos.
- `Locked` se reintenta; mismo OID es idempotente; OID distinto es conflicto terminal; no hay force-overwrite; refs simbólicas se rechazan.
- Agotamiento transitorio produce `Deferred`, no `checkpoint.failed`; una recuperación posterior produce exactamente un `checkpoint.ready`.
- `append_resolution` conserva la transacción `IMMEDIATE` y la idempotencia.
- El test B3 original no fue modificado; los tres tests nuevos de la corrección están presentes.
- No quedaron `.lock` de refs.

Evidencia: `artifacts/b3-stress-overview.json`, `artifacts/b3-stress-*.jsonl`, `logs/target-b3.stdout.log`.

## 6. TUI

Ejecución dirigida del árbol combinado:

- `phase2_tui_checkpoint_semantics`: **8/8**.
- `phase2_tui_history`: **7/7**.
- Progreso run-scoped con 150 y 1,000 eventos; goal durable fuera de la ventana; aislamiento entre runs; selección histórica; estados active/completed/failed; memoria acotada; lookup con 1,000 y 50,000 registros.
- Latencias medidas en este gate: streaming **42.3 ms / 2,185.0 ms**; referencia certificada materializada **39.3 ms / 2,081.1 ms**. Son informativas; no hay un threshold de latencia de lookup.
- `ready_for_snapshot()` conserva cursor `seq DESC`, deserializa y valida **cada** registro antes de comparar `snapshot_id`, y propaga errores. No usa `json_extract` para saltarse inválidos.
- `latest_ready()` conserva la consulta acotada aceptada. El hallazgo de selección tardía de un `ready` antiguo sigue como follow-up separado; este gate no lo redefine.

RED/GREEN histórico preservado sin reescribir: RED = **7 tests, 3 fallos**; GREEN = **8/8**, al añadirse el test de latencia.

## 7. Release y recursos

Binario: **5,928,496 bytes**, SHA-256 `64ed48a52a558e216bc400ab550759e30936bc3e77dab53226ce5eab71f3618c`.

| Medida | TUI integración previa | Combinado actual | Δ | Umbral |
|---|---:|---:|---:|---:|
| Binario | 5,923,632 B | 5,928,496 B | +4,864 B / +0.08% | ≤30 MB PASS |
| RSS idle / thread abierto | 6,992 / 7,984 KiB | 6,816 / 7,864 KiB | −2.5% / −1.5% | ≤50 MB PASS |
| Delta por thread | 0.97 MiB | 1.02 MiB | +5.2% | ≤10 MiB PASS |
| Startup p95 | 21.61 ms | 24.05 ms | +11.3% | ≤300 ms PASS |
| TUI RSS, 2 runs | 8,932 KiB | 8,924 KiB | −0.1% | app ≤500 MB PASS |
| RSS combinado, 2 runs | 17,944 KiB | 17,428 KiB | −2.9% | informativo |
| Arranque con 2 activos | 51.66 ms | 51.58 ms | −0.2% | informativo |
| Query avg / max | 5.82 / 9.24 ms | 5.72 / 8.14 ms | −1.7% / −11.9% | informativo |
| Scale app / agentes / total | 10.06 / 21.83 / 31.89 MiB | 9.84 / 21.06 / 30.90 MiB | −2.2% / −3.5% / −3.1% | app ≤500 MB PASS |

La diferencia de hash es esperada por los dos hotfixes. El aumento de tamaño es 0.08%. Los mocks no predicen consumo de Hermes real. No se cambiaron límites.

## 8. Seguridad y alcance

- Diseño visual aprobado intacto; capturas y evidencia histórica sin modificar.
- D7: solo dos literales de espera de primer frame `"Fase 1"` → `"Aeril"` en `tests/phase1_concurrency.rs` y `tests/phase1_s3.rs`; asserts, deadlines y conteos intactos.
- Managed profiles permanece default-off; single-Hermes sigue enforced; no se usan credenciales reales.
- Sin migraciones nuevas por el TUI; sin implementación P2-S2.
- D5 sigue retirado por inválido: main y TUI ya reconocen prefijos `hermes:`.
- Follow-ups no bloqueantes: selección tardía en `latest_ready`; un fallo transitorio de captura en vivo puede marcar el run `failed` aunque el checkpoint siga recuperable (comportamiento heredado de main PR #3); tie-break de listas TUI con más de 200 threads.

## 9. Procesos y filesystem

Barrido final por identidad de gate/fixture: **0** procesos Aeril/mock/PTY, **0** procesos fake-Hermes, **0** lockfiles de test o del checkout, **0** directorios temporales de test. No se enviaron señales ni se usó limpieza por nombre. Procesos históricos ajenos preservados. PDEATHSIG sigue como follow-up P3.

Evidencia: `artifacts/process-and-lock-cleanup.json`.

## 10. Cierre documental y D1–D7

- **D1:** código probado `a0a55b093fcfd3fee770c5d3c9f35c51801b988d`, tree `752f3b8b3f89de92785ce247cf02d8253e0ee1a5`.
- **D2/D6:** **230 tests / 27 targets**; intentos 229/230, 230/230, 230/230. El primer intento es FAIL y el veredicto es HOLD.
- **D3:** semántica de checkpoint corregida y verificada 8/8; streaming reverse-seq con validación por registro.
- **D4:** recursos del árbol actual medidos y comparados con la integración TUI previa; no se reutilizan como si fueran actuales.
- **D5:** inválido, retirado.
- **D7:** dos cambios de literal, sin debilitamiento de aserciones.
- RED/GREEN histórico: 7/3 y 8/8, preservados.

Este documento se transfirió manualmente desde el cierre anterior `ef0d937…`; no se cherry-pickeó el merge antiguo. El commit documental final y la prueba de que solo cambia documentación se registran en `artifacts/documentation-provenance.json` fuera del checkout.

### Recomendación final: **HOLD**

El merge, SQLite, B3, TUI y budgets son correctos, pero el requisito estricto de tres suites completas consecutivas no se cumple por el flake P2-S1 de `full-1`. Se requiere una corrección test-only revisada de la sincronización del fixture, o una decisión humana explícita sobre ese fallo, y repetir las tres suites completas. No se autoriza merge, tag ni release.

**P2-S2 sigue HOLD.**
