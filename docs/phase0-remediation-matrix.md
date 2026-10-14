# Fase 0-RC1 — Matriz de remediación de la auditoría de confiabilidad

> Base auditada: `1376f25` (`feat/phase0-vertical-slice`) ·
> Rama de remediación: `fix/phase0-reliability-rc1` ·
> Commits: **Wave A** `4e81f7d` · **Wave B** `dfd9416`
>
> Nota: el documento de auditoría no vino adjunto con la tarea. Los findings
> se tomaron textualmente del brief de remediación (A1–A6, B1–B7 y la sección
> de correctitud del benchmark = 13 ítems; el brief los numera como "12").
> Cada fila indica cómo se reprodujo/verificó el fallo: todos los tests nuevos
> corren sobre repos y stores temporales aislados.

## Wave A — corrección e integridad

| # | Fallo original | Corrección | Test (archivo::nombre) | Resultado | Commit | Riesgo residual |
|---|---|---|---|---|---|---|
| A1 | Integración con check-then-`set_target`: no atómica; un avance concurrente del destino podía pisarse | CAS real con transacción de refs de libgit2 (`lock_ref` + `commit`, GIT_EMODIFIED ⇒ `Stale{expected,actual}`); el commit del escritor concurrente queda intacto; ancestría verificada antes; hook de test para la carrera | `wave_a::a1_cas_rechaza_avance_concurrente_y_preserva_el_commit_ajeno`; `wave_a::a1_cas_idempotente_si_otro_ya_integró_lo_mismo` | ✅ PASS | `4e81f7d` | Ninguno conocido para FF simple; merge/cherry-pick siguen fuera de alcance (v0) |
| A2 | Si fallaba la persistencia de `permission.*`, igual se autorizaba (eprintln + continuar) | Fail-closed: pedido persistido antes de evaluar, decisión persistida antes de responder; error ⇒ respuesta `cancelled` + turno terminado; `permission.transmitted` separa decisión durable de transmisión; inyección `AERIL_FAULT_APPEND` | `wave_a::a2_pedido_no_persistido_no_autoriza`; `wave_a::a2_decision_no_persistida_no_autoriza`; `wave_a::a2_camino_feliz_registra_decision_y_transmision` | ✅ PASS | `4e81f7d` | El evento `permission.responded` es intención durable: la entrega se evidencia por `permission.transmitted`/error del turno, no se afirma por el guardado |
| A3 | `--resume` registraba la sesión previa pero abría `session/new` (leía el run actual, que nace sin sesión) | El ID previo se pasa explícitamente desde el run anterior; `session/load` real con fallback registrado; `engine.session` guarda `attempted`/`accepted`/`verified` (acceso ≠ restauración; Hermes responde OK a sesiones desconocidas) | `wave_a::a3_resume_usa_el_id_previo_y_lo_registra`; `wave_a::a3_resume_con_load_fallido_cae_a_sesion_nueva_y_lo_registra` | ✅ PASS | `4e81f7d` | La restauración efectiva del contexto no es verificable con Hermes 0.21.5 (`verified=false` siempre, documentado) |
| A4 | `feedback.delivered` se marcaba con CUALQUIER `session/update` (incluso `usage_update` demorado o previo al prompt) | Evidencia asociada al turno: solo mensajes/tool calls/plan y solo tras `on_turn_started`; `append` de `delivered` ANTES del flag en memoria; tests de setup demorado, conexión perdida, fallo de persistencia y reintento | `wave_a::a4_updates_de_setup_no_entregan_feedback`; `wave_a::a4_conexion_perdida_no_entrega_feedback`; `wave_a::a4_persistencia_de_delivered_fallida_no_marca_entrega`; `wave_a::a4_reintento_tras_fallo_entrega_de_verdad` | ✅ PASS | `4e81f7d` | `delivered` significa "el motor empezó a procesar el turno", no "terminó el trabajo" (eso lo dice `run.ended`/checkpoint) |
| A5 | Los submódulos se excluían del status (`exclude_submodules`) ⇒ cambios solo-submódulo quedaban fuera en silencio (`Ok(None)`) | Gitlinks/submódulos y directorios se detectan y RECHAZAN con error explícito; test de submódulo-only y de estados soportados (untracked/borrados/renombrados/ignorados) dentro del worktree | `wave_a::a5_submodulo_modificado_se_rechaza_explicitamente`; `wave_a::a5_estados_soportados_se_capturan_y_los_ignorados_no` | ✅ PASS | `4e81f7d` | v0 no captura contenido de submódulos: se rechaza, no se soporta (documentado) |
| A6 | La validación corría en el worktree del agente (TOCTOU: el agente podía editar lo validado) | Checkout efímero propio (rama+worktree desde el OID exacto, limpieza en Drop), verificación del tree antes y después, registro de comando/OID/observaciones; el worktree del agente puede estar sucio, cambiando o borrado | `wave_a::a6_validacion_usa_checkout_aislado_del_snapshot`; `wave_a::a6_modificacion_concurrente_del_worktree_no_afecta_la_validacion` | ✅ PASS | `4e81f7d` | Aisle CONTENIDO, no es sandbox de seguridad: procesos externos con permisos del usuario pueden tocar cualquier ruta (MANIFIESTO §8) |

## Wave B — durabilidad, recursos y ciclo de vida

| # | Fallo original | Corrección | Test | Resultado | Commit | Riesgo residual |
|---|---|---|---|---|---|---|
| B1 | Lectura ACP sin cotas: una línea gigante se acumulaba entera; el turno podía crecer sin límite; stderr sin cota | Cota por línea con rechazo explícito antes de asignar (`AERIL_ACP_MAX_LINE_BYTES`), presupuesto acumulado por turno/fase de control (`AERIL_ACP_MAX_TURN_BYTES`) con muerte del backend, ring acotado de stderr (200 líneas × ≤4 KiB) | `wave_b::b1_linea_gigante_se_rechaza_antes_de_acumularla`; `wave_b::b1_presupuesto_del_turno_es_acumulado` | ✅ PASS | `dfd9416` | Los límites default (16/64 MiB) son holgados para Hermes real; ajustables por env y documentados |
| B2 | Memoria de revisión sin límites: patch completo por checkpoint y lista de comentarios con TODOS los patches cargados | Límites de patch (4 MiB) y archivos (500) con rechazo explícito antes de publicar; resúmenes de comentarios paginados SIN patch; el patch se carga solo para el seleccionado; medición de caché residente por thread en el benchmark | `wave_b::b2_patch_demasiado_grande_se_rechaza_explicitamente`; `wave_b::b2_comentarios_paginados_sin_cargar_patches`; `scripts/bench/startup.py --mem` (delta idle→thread) | ✅ PASS | `dfd9416` | Los límites son de v0 (4 MiB/500); repos grandes requieren streaming de patches (futuro) |
| B3 | Creación de ref no idempotente ante carrera del mismo OID; dos recuperadores podían duplicar `checkpoint.ready` | Re-lectura tras fallo de creación: mismo OID ⇒ `AlreadyExists` (idempotente), otro OID ⇒ conflicto sin sobrescribir; resolución de recovery con re-chequeo dentro de transacción IMMEDIATE | `wave_b::b3_creacion_concurrente_del_mismo_oid_es_idempotente`; `wave_b::b3_creacion_concurrente_de_otro_oid_es_conflicto`; `wave_b::b3_recuperacion_concurrente_no_duplica_ready` | ✅ PASS | `dfd9416` | El caso "otro OID" sigue siendo conflicto terminal por diseño (jamás se pisa) |
| B4 | Integración sin intención durable: un crash podía dejar la ref movida sin registro (o al revés) | `integration.requested` ANTES del CAS + puntos de crash `integrate_intent`/`integrate_committed`; reconciliación al reabrir decide por evidencia de la ref (movida ⇒ completed recovered; sin cambio ⇒ abandono; movida por otro ⇒ bloqueo); nunca mueve refs | `wave_b::b4_crash_antes_del_cas_deja_intento_abandonado_no_fabricado`; `wave_b::b4_crash_despues_del_cas_se_reconcilia_con_evidencia` | ✅ PASS | `dfd9416` | La reconciliación no puede distinguir "movió otro escritor" de "se movió sola": se reporta como bloqueo, nunca como éxito |
| B5 | Dos procesos CLI podían editar el MISMO workspace a la vez; borrado de worktree con run activo | Lease por workspace (tabla v2, `ON CONFLICT` atómico), renovación cada ~15 s, toma de control por PID muerto o TTL vencido (evento `workspace.lease_taken_over`), `worktree rm` bloqueado con run vivo; todo run adquiere lease (incluidos los que crean workspace) | `wave_b::b5_run_concurrente_y_borrado_bloqueados_por_lease`; `wave_b::b5_lease_vencido_por_ttl_se_toma_y_se_registra` | ✅ PASS | `dfd9416` | Advisory: coordina procesos de Aeril; NO aísla de procesos externos con los permisos del usuario (documentado) |
| B6 | Tras `session/cancel`, la espera seguía el timeout de actividad (un backend charlatán podía impedir el corte) | Plazo fijo tras cancelar (`AERIL_CANCEL_GRACE_SECS`, default 10 s) independiente del streaming; al vencer se termina y reap el backend; el resultado se marca `cancelled` + "no confirmada" | `wave_b::b6_backend_que_ignora_la_cancelacion_termina_por_plazo` | ✅ PASS | `dfd9416` | El estado queda `cancelled` con detalle explícito (nunca "completed") |
| B7 | El hash de salida de validación se calculaba sobre una cola acotada con un contador aproximado | Hash FNV incremental sobre TODOS los bytes de stdout/stderr (por stream y combinado) + conteos exactos, reteniendo solo colas de 8 KiB para display | `wave_a::b7_hash_cubre_todos_los_bytes_no_solo_la_cola` (implementado en Wave A por tocar el mismo código de A6) | ✅ PASS | `4e81f7d` | FNV es identidad no criptográfica (documentado); swappable sin cambiar el contrato |

## Correctitud del benchmark (§4)

| Requisito | Antes | Ahora |
|---|---|---|
| La entrada medida ABRE un thread real | Solo primer frame/tecla trivial | El endpoint es `thread_opened` (Enter → timeline + últimos 100 eventos + resúmenes de comentarios) |
| Datos de revisión ejercitados | Seed sin comentarios | Seed 10 comentarios/thread; la vista carga resúmenes (sin patches) |
| Fallos ⇒ gate falla | p95 podía pasar con muestras fallidas | `n_fail == 0` obligatorio; se reportan todas las muestras y fallos |
| p95 y muestras completas | Solo p95 | p50/p95/min/max + lista de muestras + n_ok/n_fail |
| Caché por thread ≤ 10 MiB | No medido | RSS idle vs RSS con thread abierto (delta ≤ 10 MiB), 3 s de reposo |
| Condiciones declaradas | "warm" a secas | fstype real del data dir + warm declarado (no se disfraza tmpfs de NVMe) |
| 5 agentes | `budget-scale` fallaba con UNVERIFIED | `aeril bench scale`: 5 runs mock concurrentes en UN proceso ⇒ app ≤ 500 MB gate; agentes y total medidos aparte; nota explícita: NO prueba Hermes real (perfil compartido) |

## Gates clásicos (no regresión)

| Gate | Estado | Evidencia |
|---|---|---|
| A — retención de la revisión | ✅ | `test_a_retention` (2 tests) |
| B — recuperación tras crash | ✅ | `test_b_crash` (2 tests, SIGKILL interno y externo) |

## Limitaciones no resueltas (bloqueantes de merge a criterio del revisor)

1. **Restauración de sesión no verificable con Hermes** (`session/load` responde OK para sesiones desconocidas): se registra `verified=false` y la limitación queda en la spec ACP.
2. **Hermes real en paralelo**: no medible/seguro en v0 por el perfil compartido (`~/.hermes`); el bench de escala usa mocks y lo declara.
3. **Leases advisory**: no protegen contra procesos externos ajenos a Aeril.
4. **Submódulos**: rechazados explícitamente, no soportados.
5. **Patches grandes** (> 4 MiB): rechazados en v0; sin streaming todavía.
6. El documento de auditoría original no estaba adjunto; la trazabilidad se apoya en el texto del brief y en estos tests.

## Cómo re-verificar

```bash
cargo test                      # 44 tests (A/B, wave_a, wave_b, bordes)
cargo clippy --all-targets -- -D warnings
make budget                     # size/deps/mem(incl. caché por thread)/start
make budget-scale               # 5 runs mock: app ≤ 500 MB (agentes aparte)
```
