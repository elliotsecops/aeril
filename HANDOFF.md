# Aeril — Handoff (estado al 2026-10-13 — v0.4)

> **Para la próxima sesión.** Leé esto primero, después `MANIFIESTO.md` (v0.4), después la nota de Obsidian (v0.4). No necesitás el historial de esta conversación para continuar.
>
> **v0.4** integra la **segunda pasada** del revisor: (1) esquema mínimo de 3 tablas pero con snapshot real desde el primer comentario; (2) protocolo prepared→ready y política de limpieza de refs por evidencia, nunca por edad; (3) corrección competitiva —Orca SÍ tiene el cierre review→feedback→integración, la diferenciación es la promesa verificable dentro del presupuesto—; (4) 5-10 días son una demostración estrecha, no el release robusto; (5) v0 integra solo por fast-forward a rama destino explícita.

---

## 1. Qué es Aeril

Un **terminal/entorno donde el agente de IA es un participante nativo**. No es un shell donde escribís `opencode`: cada panel es un *thread* —el prompt, el razonamiento del agente, el diff de sus cambios y los comentarios del humano, anclados a un snapshot inmutable y recuperables días después. Es lo que Orca prometió, sin Electron.

**Tesis:** liviano por diseño, como contrato verificado en CI, no como slogan. Corre completo en un ThinkPad de 2018 con 16 GB. Las herramientas se adaptan al usuario, no al revés.

**Diferenciador estratégico (a validar):** el modelo de threads es, sin querer, un **registro de auditoría de desarrollo**. Para fintech/empresas reguladas que usan agentes de IA sobre código, responde "¿quién cambió esta línea, por qué, y quién lo autorizó?" —un problema que se agrava con el tiempo y que ningún modelo de OpenAI resuelve. Monetización: open source gratis para el dev individual (distribución), licencia enterprise por la capa de auditoría/compliance.

---

## 2. Estado de los documentos (todo en disco, verificado)

| Documento | Ruta | Versión |
|---|---|---|
| Constitución | `/home/elliot/aeril/MANIFIESTO.md` | **v0.4** |
| Plan + arquitectura | `~/Documentos/Obsidian Vault/Aeril - Plan de Producto y Arquitectura.md` | **v0.4** |
| Handoff | `/home/elliot/aeril/HANDOFF.md` | **v0.4** |

v0.3 integró la primera revisión; v0.4 la segunda pasada. El revisor (Codex, 2026-10-13) dejó además:
- `/home/elliot/Documentos/ChatGPT/Aeril/docs/architecture-review-2026-10-13.md` (auditoría completa, 1ra pasada)
- `/home/elliot/Documentos/ChatGPT/Aeril/docs/data-model-proposal.sql` (esquema de discusión con 5 entidades)
- `/home/elliot/Documentos/ChatGPT/Aeril/docs/architecture-second-pass-2026-10-13.md` (2da pasada: esquema mínimo, protocolo de checkpoints, limpieza de refs, corrección competitiva)
- `/home/elliot/Documentos/ChatGPT/Aeril/.research/architecture-2026-10-13/schema-validation.json` (15 checks de integridad del DDL, no recuperación real)
- `.../architecture-2026-10-13/ref-validation.json` (update-ref verificado: borrado con OID obsoleto falla; transacción mueve ref a cuarentena y el blob sigue alcanzable)

---

## 3. Decisiones ya tomadas (no reabrir sin razón)

- **Nombre:** Aeril (de *aery/aerial* —aire/vuelo/liviano). Libre en crates.io, npm, `.dev`, `.sh`. Descartados: Ravel (1.6k repos), Zell (35k★ **Zellij**, misma categoría).
- **Lenguaje/stack:** Rust + `ratatui`/`crossterm` + `portable-pty` + **`tui-term`/`vt100`** (emulador) + `rusqlite` (WAL) + `git2` + `tokio`. GUI (tardía): `axum` servido desde el binario, nunca Electron.
- **Motor de agentes:** externo por **contrato estructurado (ACP)**. Hermes es un **backend inicial opcional**, NO obligatorio. Verificado: `hermes acp` existe en el CLI.
- **Local-first, un solo usuario.** Sin CRDTs, sin cloud, sin sync multi-usuario.

---

## 4. Las 6 invariantes ( Constitución §3, v0.3)

1. Un solo binario. Cero runtime de app.
2. Motor externo por contrato estructurado (ACP u otro); Hermes opcional.
3. Un store transaccional local (SQLite) **de Aeril**; los backends mantienen su estado privado.
4. GUI = web desde el binario, nunca Electron.
5. Datos exportables a texto plano (`aeril export` → JSONL/Markdown/patch); SQLite es binario, se promete export sin pérdida.
6. Cada ejecución que edita código corre en un **workspace verificado por Aeril**; un worktree **no es un sandbox**.
7. **Un thread durable sobrevive a la muerte de su proceso y de su worktree**; el comentario se ancla a un snapshot inmutable, no a `event.id` ni a una línea.

*(son 7; la 7 se agregó en la revisión)*

---

## 5. Correcciones clave que trajo la revisión (ya aplicadas)

- **Ancla de snapshot** (la más importante): `event.id` conservaba la conversación pero no la ubicación en el código. Ahora el comentario lleva path/lado/rango/blob oid sobre una versión inmutable; el remapeo da `exact`/`outdated`/`unresolved`.
- **HEAD no guarda lo revisado**: capturar el estado revisado + refs propias (`refs/aeril/checkpoints/<id>`); reconciliación SQLite↔Git (una ref huérfana es preferible a un evento que afirma guardar código inexistente).
- **`UNIQUE(thread_id, seq)` + FK activada**; `run_id` para identificar ejecución; `payload_version` + validación por `kind`.
- **"Primer frame" es métrica tramposa**: el gate mide "UI utilizable + entrada procesada", desde fuera del proceso, con hardware/disco/perfil declarados.
- **10 MB por thread = caché residente**, no techo de todo estado.
- **Hermes delegación/memoria NO son gratis**: la delegación comparte directorio por default (worktree opt-in, degrada en silencio); dos procesos no deben compartir el home. Para v0, Aeril lanza y verifica cada agente editor.

**Segunda pasada (v0.4):**

- **Esquema mínimo = 3 tablas (`threads`/`runs`/`events`), pero el snapshot existe desde el primer comentario.** Los atributos del workspace viven en `runs`; el snapshot es un evento de checkpoint. Se posponen tablas, no garantías: IDs estables (`repo_id`/`workspace_id`/`snapshot_id`), base OID exacta por run, payloads versionados, snapshot con ref + OID esperado + patch revisado. Una 4ª tabla pequeña de checkpoints es aceptable si los pins en JSON complican la limpieza.
- **Protocolo de checkpoint:** construir objetos → persistir `checkpoint.prepared` (ref + OID esperado) → publicar `refs/aeril/checkpoints/<id>` → verificar → persistir `checkpoint.ready`. Comentarios solo tras `ready`. Un crash entre prepared y ready es una operación recuperable al reabrir.
- **Limpieza de refs por evidencia, nunca por edad.** Protegido = citado por comentario retenido (incluso resuelto), revisión/validación retenida, run activo u operación pendiente. `prepared` coherente → reconciliar. Abandono confirmado sin consumidores → cuarentena (`refs/aeril/quarantine/`), 7 días desde la primera clasificación durable. **Una ref sin fila en SQLite NO es basura** (puede ser una DB restaurada) → cuarentena conservadora, jamás TTL automático. Liberar = revalidar consumidores + CAS con OID esperado (update-ref transaccional). Resolver un comentario NO desprotege su snapshot. No encadenar checkpoints (padre común = base de la tarea). Sin `gc --prune=now` con escritores. En el ensayo, el recolector solo lista candidatos.

---

## 6. Roadmap vigente (corte vertical primero)

| Fase | Entregable | Estimado |
|---|---|---|
| **0 — Ensayo de viabilidad** | Un thread completo y recuperable (editar→snapshot→revisar→comentar→feedback→reanudar tras crash), 1 backend, 1 repo, Linux x86_64, medido en la laptop objetivo | **5-10 días** |
| 1 — Corte vertical útil | Review durable + 2 agentes con workspaces verificados | 4-6 sem |
| 2 — Alpha multiagente | 3-5 runs, integración asistida, búsqueda, notificaciones | 4-6 sem |
| 3 — Ampliación | Otros backends/targets; GUI web al final | tras alpha |

**La unidad de valor es el cierre del trabajo:** snapshot → revisión → feedback → reanudación → tests ligados al snapshot → integración segura. Los splits y la GUI web van después.

---

## 7. Estado de las decisiones (v0.4)

**Resueltas en la segunda pasada:**

1. ~~Esquema mínimo v0~~ → **3 tablas + snapshot desde el primer comentario** (protocolo prepared/ready, ver §5). Los pines en eventos son aceptables; una 4ª tabla de checkpoints si la limpieza lo pide.
2. ~~Política de limpieza de refs~~ → **por evidencia de abandono, cuarentena, CAS con OID esperado** (ver §5).
3. ~~Competencia~~ → **Orca SÍ tiene el cierre** (annotate → commit/push → GitHub merge); Warp tiene review interactivo + integración de ramas. La diferenciación es la **promesa verificable**: retomás una revisión días después y sabés qué código viste, qué feedback llegó, qué versión se validó y qué se integró, dentro del presupuesto.

**Siguen abiertas:**

4. **Modelo de confianza:** ✅ **Resuelto.** Redactar en el manifiesto: *"Aeril separa los archivos de trabajo de cada tarea; no restringe los permisos del agente sobre la computadora."* El agente conserva los permisos del usuario: puede acceder a otras carpetas, usar credenciales accesibles, conectarse a Internet, arrancar programas, y tomar el mismo puerto/servicio que otro agente. Borrar el worktree NO detiene los procesos que el agente lanzó ni deshace acciones externas.
5. **Cuándo extraer `workspaces`/`artifacts` a tablas propias:** cuando los pins/checkpoints en JSON vuelvan difícil la limpieza (el revisor sugiere que una 4ª tabla puede simplificar antes que las 5 completas).
6. **Criterio de éxito del ensayo:** ✅ **Resuelto — dos pruebas, ambas obligatorias** (ver §9). Gate = (A) retención de revisión: comentar un cambio, el archivo vuelve a cambiar, borrar el worktree → Aeril muestra **el cambio original comentado**. (B) recuperación de crash: cerrar Aeril de golpe mientras guarda el snapshot → al reabrir, termina de recuperarlo o explica qué falta, sin reemplazarlo por el contenido actual.

---

## 8. Contexto de esta sesión (por si sirve)

- **Host de Elliot:** ThinkPad X1 Carbon 6th, i7-8550U, 16 GB RAM, 256 GB NVMe, Ubuntu 26.04 + KDE/Wayland. **RAM y disco apretados** (OOM activo bajo carga). Terminal del día a día: **Warp** (0.8 GB, medido). Zed (1.7 GB) y Orca (1.8 GB) **desinstalados**; goose y antigravity también.
- **5 agentes** trabajando para Elliot en paralelo, en panes divididos de Warp.
- **Elliot:** founder de Cambia+ (stablecoin neobank, fintech regulada). Prioridades actuales de Cambia+: KYC mobile, withdrawals por red, FCM. Aeril es proyecto estratégico en paralelo, **no** fuente de ingreso inmediato —busca que sea escalable y sostenible, resolviendo problemas permanentes (auditoría de agentes sobre código), no features del "ahora".
- **NVIDIA_API_KEY** en `.zshrc` línea 84 quedó con valor literal `redacted-secret` (no es una key real; free tier, no usada). El warning de Hermes por esa línea ya no aparece.

---

## 9. Gate del ensayo de viabilidad (definido v0.4, del revisor)

**Dos pruebas obligatorias. Ambas tienen que pasar para aprobar el ensayo.**

**Prueba A — retención de revisión (demuestra que conserva la revisión):**
1. El agente modifica un archivo.
2. Vos comentás ese cambio.
3. El archivo vuelve a cambiar.
4. Eliminás el worktree.
5. ✅ Aeril sigue mostrando **el cambio original que comentaste** (no el contenido actual).

**Prueba B — recuperación de crash (demuestra que se recupera de una caída):**
1. Cerrás Aeril de golpe mientras está guardando ese snapshot (entre `prepared` y `ready`).
2. Al reabrir, ✅ termina de recuperarlo **o explica qué falta** —sin reemplazarlo por el contenido actual del archivo.

**Perfil de benchmark de arranque (para los 300 ms):**
- Datos artificiales de referencia: **100 conversaciones, 1.000 eventos por conversación, 10 diffs por conversación.** (Referencia inicial, no una descripción de usuarios futuros.)
- Al abrir, cargar **solo** la lista de conversaciones + las últimas 100 entradas de la conversación visible. El resto queda en SQLite hasta que se pida.
- El punto que valida: **tener mucho historial guardado no vuelve lento el arranque.**

---

## 10. Próximo paso concreto

Arrancar la **Fase 0** con el gate de §9 ya definido (no hay que rediseñarlo). Las piezas investigables en paralelo: `tui-term`/`vt100`, el adaptador ACP de Hermes, la capa de worktrees con `git2`, y el benchmark de arranque.

*Constitución, plan y este handoff están en v0.4. Este handoff es la puerta de entrada; `MANIFIESTO.md` es la fuente de verdad de las invariantes.*
