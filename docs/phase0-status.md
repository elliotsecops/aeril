# Fase 0 — Estado de implementación

> Rama: `feat/phase0-vertical-slice` · Base: `main` (v0.4) ·
> Gate definido en `HANDOFF.md` §9 · Fecha: 2026-10-13

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
| Workspace verificado (worktree + base OID + `workspace_id` estable) | **V** | `test_a_*`, `edge_cases::validacion_*` |
| Snapshot inmutable del working tree real (sin índice/rama del humano) | **V** | `test_a_*` (OIDs comparados, no texto) |
| Protocolo `prepared → ref (CAS por OID) → verify → ready` | **V** | `test_b_crash::crash_en_cada_frontera` |
| Ref divergente: no se sobrescribe, queda `failed` terminal | **V** | `edge_cases::ref_conflictiva_*` |
| Objetos faltantes: se reporta, jamás se fabrica `ready` | **V** | `edge_cases::objetos_faltantes_*` |
| Recuperación al reabrir, idempotente, sin tocar el working tree actual | **V** | `test_b_crash` (se ensucia el árbol antes de recuperar) |
| Caída abrupta externa real (SIGKILL desde afuera) | **V** | `test_b_crash::crash_externo_*` |
| Comentario anclado (snapshot/path/lado/rango/blobs/context hash), solo tras `ready` | **V** | `test_a_*`, `test_b_crash` (comentar antes de ready falla) |
| Patch original reabrible tras nuevas ediciones y borrado del worktree | **V** | `test_a_retencion_de_la_revision_comentada` |
| Comentario resuelto NO desprotege el snapshot | **V** | `agent_flow::feedback_entregado_*` |
| Adaptador ACP (ndjson/stdio, permisos por `optionId`, cancelación, sesión durable) | **V** | `agent_flow::*`, `edge_cases::cancelacion_*` |
| Hermes real (edición → checkpoint → comentario → feedback → checkpoint) | **v** | demo manual 2026-10-13 (ver handoff §Hermes) |
| Feedback con entrega durable (`sent` → `delivered`), fallo explícito sin ack | **V** | `agent_flow::feedback_*` |
| Validación ligada al OID exacto del snapshot (rechaza workspace desincronizado) | **V** | `edge_cases::validacion_*` |
| Integración fast-forward a rama explícita; stale/divergente/ocupado bloquean | **V** | `edge_cases::integracion_*` |
| TUI mínima (threads, timeline, patch, comentar, feedback, integrar, cancelar) | **v** | uso manual + harness de arranque (pty) |
| Export a texto plano (`jsonl`, `md`) | **V** | usado en todos los tests |
| Recolector de refs: SOLO listar candidatos | **F** | decidido: en el ensayo no se libera nada (MANIFIESTO §5) |
| Remapeo de comentarios a snapshots nuevos | **F** | operación separada, posterior (no silenciosa) |
| Merge/cherry-pick, resolución de conflictos | **F** | fuera de alcance v0 (solo FF) |
| PTY / emulador `vt100` / paneles divididos | **F** | el multiplexor es Fase 1; Fase 0 mide el corte vertical |
| `session/fork`, `session/resume` silencioso, multi-sesión | **F** | no usados por el flujo v0 |

## Presupuestos

| Métrica | Límite | Medición | Estado |
|---|---|---|---|
| Binario release stripped | ≤ 30 MB (decimal) | ver handoff §Mediciones | ver handoff |
| RSS idle TUI | ≤ 50 MB | ídem | ídem |
| Arranque p95 (entrada procesada, desde fuera) | ≤ 300 ms | ídem | ídem |
| Dependencias de runtime | 0 de app | `ldd` con whitelist | ídem |
| 5 agentes ≤ 500 MB | — | **UNVERIFIED**: Fase 0 mide 1 agente; sin claim | **N** |

Condiciones de caché y dataset declaradas en `make budget-*` (ver
`bench-data/*.json` tras correrlos). Los números son de esta máquina
(ThinkPad X1C6, i7-8550U, 16 GB, NVMe) con page cache **warm** del host.

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
