# Fase 0 — Estado de implementación

> Rama: `feat/phase0-vertical-slice` · Base: `main` (v0.4) ·
> Gate definido en `HANDOFF.md` §9 · Fecha: 2026-10-13
>
> **RC1 (reliability hardening):** ver `docs/phase0-remediation-matrix.md`
> (rama `fix/phase0-reliability-rc1`, commits `4e81f7d` y `dfd9416`).
> **RC2 (blockers de aceptación):** ver `docs/phase0-rc2-acceptance.md`
> (rama `fix/phase0-reliability-rc2`).

## Garantías exactas RC2 (política documentada)

- **Pipeline ACP byte-bounded**: la cola interna transporta líneas CRUDAS (sin
  decodificar); el productor reserva bytes antes de encolar (backpressure) y el
  consumidor libera por Drop en éxito/error/cancelación/cierre. Límites:
  línea ≤ 4 MiB (`AERIL_ACP_MAX_LINE_BYTES`), en vuelo ≤ 8 MiB
  (`AERIL_ACP_INFLIGHT_BYTES`), turno acumulado ≤ 64 MiB
  (`AERIL_ACP_MAX_TURN_BYTES`). Un frame mayor al presupuesto transita solo.
  `kill()`/`Drop` terminan el **grupo de procesos** del backend.
- **Lease de workspace**: un TTL vencido **nunca** transfiere ownership con el
  proceso dueño VIVO (incluye SIGSTOP y `lease release --force`, que RC2.1 ya
  no permite como bypass); recuperación = muerte verificada del proceso dueño
  (o toma automática por PID muerto/reutilizado). La transferencia automática exige muerte
  probada (PID ausente o reutilizado según `starttime`). Si un run pierde
  ownership (o falla la renovación), ABORTA: cancela el backend, registra
  `workspace.lease_lost`, `run.ended failed`, sin checkpoint ni completitud.
  `worktree rm` requiere dueño muerto. **Fence RC2.1**: `checkpoint.prepared`,
  `checkpoint.ready` y `run.ended completed` verifican el ownership del lease
  DENTRO de la misma transacción IMMEDIATE del append; un dueño obsoleto no
  publica ready ni reporta completitud (falla explícita + `workspace.lease_lost`;
  marcador terminal para que la recuperación no complete un checkpoint
  rechazado; la recuperación de crashes legítimos no cambia). Ámbito:
  coordinación advisory entre procesos de Aeril; no es un sandbox (MANIFIESTO §8).
- **Submódulos/gitlinks**: enumerados por ÍNDICE, no por status. Inicializado
  con puntero modificado ⇒ rechazo; **no inicializado con contenido bajo su
  path ⇒ rechazo explícito** (jamás se tra-versa como archivos comunes ni se
  reporta "sin cambios"); no inicializado y vacío ⇒ captura normal del resto.

Leyenda:
**V** = implementado y verificado con test automatizado ·
**v** = implementado y verificado a mano ·
**N** = implementado, no verificado ·
**B** = bloqueado por dependencia externa ·
**F** = no implementado (fuera del alcance declarado de Fase 0).

## El corte vertical

| Pieza | Estado | Evidencia |
|---|---|---|
| Store SQLite (3 tablas, `UNIQUE(thread_id,seq)`, FKs, payloads versionados) | **V** | `store::tests`, `tests/*` (reinicio = proceso nuevo) |
| Workspace verificado (worktree + base OID + `workspace_id` estable) | **V** | `test_a_*`, `wave_a::a6_*` |
| **Lease de workspace entre procesos de Aeril** (B5) | **V** | `wave_b::b5_*` (run concurrente bloqueado, rm bloqueado, takeover pid/ttl) |
| Snapshot inmutable del working tree real (sin índice/rama del humano) | **V** | `test_a_*` (OIDs comparados, no texto) |
| Snapshot: gitlinks/submódulos rechazados explícitamente (A5) | **V** | `wave_a::a5_*` |
| Límites de patch/archivos con rechazo explícito (B2) | **V** | `wave_b::b2_patch_*` |
| Protocolo `prepared → ref (CAS por OID) → verify → ready` | **V** | `test_b_crash::crash_en_cada_frontera` |
| Publicación de refs idempotente ante carreras del mismo OID (B3) | **V** | `wave_b::b3_creacion_*` |
| Ref divergente: no se sobrescribe, queda `failed` terminal | **V** | `edge_cases::ref_conflictiva_*` |
| Objetos faltantes: se reporta, jamás se fabrica `ready` | **V** | `edge_cases::objetos_faltantes_*` |
| Recuperación al reabrir, idempotente, sin tocar el working tree actual | **V** | `test_b_crash`; recuperación concurrente: `wave_b::b3_recuperacion_*` |
| Caída abrupta externa real (SIGKILL desde afuera) | **V** | `test_b_crash::crash_externo_*` |
| Comentario anclado (snapshot/path/lado/rango/blobs/context hash), solo tras `ready` | **V** | `test_a_*`, `test_b_crash` |
| Patch original reabrible tras nuevas ediciones y borrado del worktree | **V** | `test_a_retencion_de_la_revision_comentada` |
| Comentarios paginados con resúmenes livianos (B2) | **V** | `wave_b::b2_comentarios_paginados_*` |
| Adaptador ACP (ndjson/stdio, permisos por `optionId`, cancelación, sesión durable) | **V** | `agent_flow::*`, `edge_cases::cancelacion_*` |
| Entrada ACP acotada: línea/turno/stderr (B1) | **V** | `wave_b::b1_*` |
| Cancelación con plazo fijo e cumplimiento no cooperativo (B6) | **V** | `wave_b::b6_*` |
| Permisos fail-closed con auditoría durable y transmisión separada (A2) | **V** | `wave_a::a2_*` |
| Resume real de sesión ACP con intento/aceptación registrados (A3) | **V** | `wave_a::a3_*` |
| Hermes real (edición → checkpoint → comentario → feedback → checkpoint) | **v** | demo manual 2026-10-13 (ver handoff §Hermes) |
| Feedback con entrega durable basada en evidencia del turno (A4) | **V** | `wave_a::a4_*`, `agent_flow::feedback_*` |
| Validación ligada al OID exacto en checkout aislado (A6/B7) | **V** | `wave_a::a6_*`, `wave_a::b7_*` |
| Integración FF con CAS atómico y rechazo de stale/divergente/ocupado (A1) | **V** | `wave_a::a1_*`, `edge_cases::integracion_*` |
| Integración: intención durable + reconciliación tras crash (B4) | **V** | `wave_b::b4_*` |
| TUI mínima (threads, timeline, patch, comentar, feedback, integrar, cancelar) | **v** | uso manual + harness de arranque (pty, abre thread) |
| Export a texto plano (`jsonl`, `md`) | **V** | usado en todos los tests |
| Recolector de refs: SOLO listar candidatos | **F** | decidido: en el ensayo no se libera nada (MANIFIESTO §5) |
| Remapeo de comentarios a snapshots nuevos | **F** | operación separada, posterior (no silenciosa) |
| Merge/cherry-pick, resolución de conflictos | **F** | fuera de alcance v0 (solo FF) |
| PTY / emulador `vt100` / paneles divididos | **F** | el multiplexor es Fase 1; Fase 0 mide el corte vertical |
| `session/fork`, `session/resume` silencioso, multi-sesión | **F** | no usados por el flujo v0 |

## Presupuestos (RC1, 2026-10-13)

Método y condiciones declarados en `docs/phase0-engineering-handoff.md`
§Mediciones RC1 (NVMe ext4, caché warm, release LTO, dataset 100×1000×10 con
comentarios). `make budget` exit 0.

| Métrica | Límite | Medido | Estado |
|---|---|---|---|
| Binario release stripped | ≤ 30 MB (decimal) | **5.4 MB** | ✅ PASS |
| RSS idle TUI (5 s) | ≤ 50 MB | **6.4 MB** | ✅ PASS |
| Caché residente por thread (idle → thread abierto) | ≤ 10 MiB | **0.8 MiB** | ✅ PASS |
| Arranque p95 (entrada que ABRE un thread, externo, n=30) | ≤ 300 ms | **25.2 ms**, 30/30 ok | ✅ PASS |
| Dependencias de runtime | solo ABI del OS | OS + libz del sistema (declarada) | ✅ PASS |
| 5 runs concurrentes: **app** | ≤ 500 MB | **9.7 MB** (agentes mock) | ✅ PASS |
| 5 runs concurrentes: agentes + total | medido aparte | **21.4 MB / 31.1 MB** | informativo |
| 5 agentes HERMES reales | — | **no medible en v0** (perfil compartido) | ⚠️ declarado |

## Decisiones registradas (desviaciones o ajustes de Fase 0)

1. **`tokio` acotado**: runtime propio solo por run (2 workers), sin
   infectar la TUI (síncrona). Sin `portable-pty`/`vt100` todavía.
2. **Sin cuarta tabla de checkpoints**: el anti-join sobre `events` con el
   índice `(kind, thread_id)` alcanza en v0; la migración a 4ª tabla es
   mecánica si la limpieza de refs lo pide.
3. **Perfiles de motor por run**: no implementado (limitación de Hermes:
   comparte `~/.hermes`). Se documenta como límite: no lanzar runs
   concurrentes de Hermes desde Aeril en v0.
4. **`git` CLI solo en tests**; el producto habla `git2` exclusivamente.
5. **Política de permisos v0**: `auto` (allow_once) o `deny`, registrada por
   pedido. No hay UI de aprobación todavía (el modelo de confianza del
   MANIFIESTO §8 no promete sandbox).
6. **El recolector de objetos Git no se ejecuta**; los commits huérfanos de
   capturas fallidas quedan (inofensivos) hasta el mantenimiento normal.
