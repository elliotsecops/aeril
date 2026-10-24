# Aeril Private Alpha — Contrato PA0

**Estado:** contrato congelado el 2026-10-24.
**Baseline:** `main@ab268ac42b4e2629f060d9d43dc2bfe02049ef11`.
**Alcance:** PA0-A (contrato y métricas) y PA0-B (Attention Inbox). PA0-C, PA0-D y
PA0-E quedan explícitamente fuera de esta entrega.

Este documento define qué significa completar una tarea real con Aeril antes de
agregar capacidades. Los umbrales son criterios de aceptación propuestos, no
resultados demostrados.

---

## 1. Objetivo de producto

Un desarrollador, sin asistencia del equipo fundador, instala Aeril, delega una
tarea real a un agente, supervisa el trabajo, revisa el checkpoint, ejecuta una
validación relevante, completa una integración segura —o identifica correctamente
por qué está bloqueada— y recupera el historial al reabrir la aplicación.

La unidad de aceptación es el recorrido completo, no una lista de capacidades
técnicas aisladas.

### Definición de “tarea real”

Un cambio acotado en un repositorio Git del participante, ejecutado por Hermes
autenticado y revisado por el participante. Los backends mock y los scripts de
test sirven para ingeniería y regresión, pero **no certifican compatibilidad con
Hermes**: esa evidencia requiere una sesión real autenticada (PA0-D).

---

## 2. Alcance congelado

### Dentro de PA0

| Área | Compromiso |
| --- | --- |
| Instalación Linux | Binario reproducible y flujo de build documentado (PA0-D). |
| Delegación | Un thread, un run de Hermes real por vez. |
| Supervisión | Estado del run, actividad y resultado visibles. |
| Revisión | Patch, archivos, comentarios y acknowledgement explícito. |
| Validación | Prueba relevante atada al snapshot y OID exactos. |
| Integración | Fast-forward asistido con preview y confirmación, o bloqueo explicado. |
| Recuperación | Historial durable y próxima acción correcta tras reiniciar. |
| Attention | Proyección read-only de estado y siguiente acción por tarea. |

### Fuera de PA0 (no autorizado en este milestone)

- Búsqueda global.
- Concurrencia de 3–5 runs.
- Interfaz web.
- Motor genérico de merges o resolución automática de divergencias.
- Sistema complejo de indexación.
- P2-S2 o ampliación del modelo de sesiones.
- Preparación automática de checkouts para destrabar integraciones.
- Un segundo motor de tareas, una segunda base o un esquema paralelo para
  Attention.
- Afirmar atomicidad transaccional entre SQLite y Git, o inmunidad frente a
  escritores externos que no respetan los controles de Aeril.

La compatibilidad con Hermes se limita a **una sesión autenticada a la vez**.
Un backend fake que habla ACP no es prueba de compatibilidad.

---

## 3. Escenarios E2E

Cada escenario se ejecuta con un participante que no conoce la arquitectura
interna. El facilitador solo puede registrar observaciones; no puede resolver el
flujo por el participante.

### E2E-1 — Instalación y primer arranque

**Precondiciones:** Linux x86_64 limpio, Git instalado, acceso al artefacto de
PA0-D, repositorio de prueba propio.

**Pasos:** instalar o construir el binario; ejecutar `aeril version`; abrir la TUI;
registrar un proyecto con `aeril thread new`; verlo en la lista de tareas.

**PASS:** el participante completa todos los pasos sin intervención directa del
fundador y sin editar configuración no documentada.

**FAIL:** requiere ayuda del fundador, edita configuración no documentada, el
binario no arranca o el thread no aparece en la lista.

**Evidencia:** comandos ejecutados, versión, captura del primer frame, tiempo
hasta el primer thread visible, punto exacto de abandono si lo hubo.

**No soportado:** instalación universal en macOS/Windows, binario sin toolchain
Rust y soporte de múltiples versiones de Linux en PA0.

### E2E-2 — Delegación y supervisión

**Precondiciones:** E2E-1 completo, Hermes autenticado disponible.

**Pasos:** abrir un thread; lanzar una tarea de edición con un objetivo real;
observar progreso y resultado; esperar el cierre del run.

**PASS:** el participante puede explicar qué está haciendo el agente y si el run
terminó, falló o quedó esperando algo. No se acepta un estado “verde” sin
evidencia durable.

**FAIL:** el estado mostrado contradice el run durable, un backend fake se
presenta como Hermes, o el participante no puede distinguir progreso de cierre.

**Evidencia:** run, estado final, checkpoint `prepared`/`ready` o razón de fallo,
eventos de permiso si existieron.

**No soportado:** más de un Hermes autenticado a la vez, backends adicionales y
certificación de Hermes con mocks o ACP simulado.

### E2E-3 — Revisión del checkpoint

**Precondiciones:** existe un `checkpoint.ready` verificable.

**Pasos:** abrir el checkpoint; identificar archivos y cambios; agregar una
observación si hace falta; resolverla; marcar el checkpoint como revisado con la
tecla `a` o `aeril review ack`.

**PASS:** el participante identifica qué cambió y por qué. “No hay comentarios”
no se interpreta como “revisado”: el acknowledgement es un hecho durable
explícito, exacto al `snapshot_id` y `oid`.

**FAIL:** se crea un acknowledgement sin acción humana, se marca un snapshot
distinto del patch visible, o se acepta un checkpoint sin `prepared`, con
marcador `failed` o con ref/objetos no verificables.

**Evidencia:** `review.acknowledged` con snapshot/OID exactos; comentarios y
resoluciones; ausencia de acknowledgement si el participante no lo emitió.

**No soportado:** acknowledgement automático por abrir el panel, revisar un diff
o navegar; aceptar un `ready` contradictorio; retroactividad sobre checkpoints
históricos.

### E2E-4 — Validación

**Precondiciones:** checkpoint revisado.

**Pasos:** ejecutar una prueba relevante con
`aeril validate --thread T --snapshot S -- COMANDO`; leer el resultado.

**PASS:** el participante entiende el comando ejecutado y su resultado. La
validación queda atada al snapshot y OID exactos.

**FAIL:** una validación de otro OID, un checkout que no reproduce el tree o un
árbol que cambia durante la prueba habilitan la integración; o el participante no
puede explicar el resultado.

**Evidencia:** `validation.recorded` con snapshot, OID, comando, exit code,
hashes de salida y flags de integridad del checkout.

**No soportado:** validación remota/distribuida, ejecución de pruebas fuera del
checkout aislado y reutilización de una validación para otro snapshot/OID.

### E2E-5 — Integración segura o bloqueo comprensible

**Precondiciones:** revisión y validación aceptadas para el snapshot exacto.

**Camino A — integración asistida disponible:** preview con rama destino,
operación y evidencia; confirmación explícita; CAS sobre el OID esperado.

**Camino B — integración bloqueada (PA0-C):** Aeril identifica la causa, muestra
los OID relevantes y ofrece instrucciones contextualizadas para completar la
operación desde un checkout apropiado.

**PASS:** ninguna ref se mueve sin preview elegible y confirmación explícita.
El camino bloqueado no permite saltear validación, review ni CAS. El participante
puede explicar por qué está bloqueado y qué haría a continuación.

**FAIL:** una ref se mueve sin preview/confirmación, el bloqueo se presenta como
éxito, o el camino manual omite validación/review/CAS.

**Evidencia:** `integration.requested`, `integration.completed` o
`integration.blocked`; OID antes/después; preview; instrucciones mostradas.

**No soportado:** merges genéricos, resolución automática de divergencias,
preparación automática de checkouts y publicación de releases. El camino B
corresponde a PA0-C y todavía no está implementado: E2E-5 no puede certificarse
hasta que PA0-C esté disponible.

### E2E-6 — Recuperación y reapertura

**Precondiciones:** al menos un run o checkpoint interrumpido de forma controlada.

**Pasos:** cerrar Aeril; reabrir; ejecutar `aeril recover` si corresponde; volver
a la tarea.

**PASS:** el historial durable se conserva y la próxima acción mostrada no
contradice la evidencia. Una recuperación exitosa no se presenta como prueba de
que todas las acciones externas del agente sean reversibles.

**FAIL:** se pierde historial, se duplican eventos, se inventa un éxito o la
próxima acción recomienda integrar con evidencia de recuperación pendiente.

**Evidencia:** eventos antes/después, `aeril attention --json`, estado del
checkpoint, resultado de `recover`, ausencia de duplicados.

**No soportado:** reversibilidad de acciones externas del agente, recuperación
distribuida y garantías ante escritores externos que no respetan los leases de
Aeril.

### E2E-7 — Cierre y retorno

**Precondiciones:** tarea integrada o correctamente bloqueada.

**Pasos:** cerrar y reabrir Aeril; localizar la tarea; confirmar que no hay acción
pendiente o que la acción pendiente sigue siendo correcta.

**PASS:** el participante reconstruye el estado desde Aeril, sin abrir otra
terminal ni buscar OIDs manualmente.

**FAIL:** necesita otra terminal, búsqueda manual de OIDs o documentación externa
para saber qué pasó o qué hacer.

**Evidencia:** salida de `aeril attention`, estado del thread, tiempo hasta
localizar la tarea.

**No soportado:** retención histórica más allá de la base local, búsqueda global y
sincronización entre máquinas.

---

## 4. Criterios de aceptación del piloto externo

Cohorte objetivo: 5 desarrolladores; mínimo 3 para ejecutar el piloto. Los
resultados de 3–4 participantes se reportan como observación cualitativa, no como
inferencia estadística.

| Indicador | Umbral PA0 | Cómo se verifica |
| --- | --- | --- |
| Instalación sin asistencia | 100% de la cohorte | E2E-1 |
| Tarea real completada | ≥80% (redondeo hacia arriba: 3/3, 4/4, 4/5) | E2E-2 a E2E-5 |
| Revisión comprendida | ≥80% explica qué cambió y por qué | E2E-3, entrevista breve |
| Validación comprendida | ≥80% ejecuta y explica el resultado | E2E-4 |
| Integración segura o bloqueo identificado | ≥80%; 0 bypass inseguro | E2E-5 |
| Recuperación tras reiniciar | ≥80% | E2E-6/E2E-7 |
| Ayuda del fundador en el flujo principal | 0 intervenciones | Registro del facilitador |
| Problemas críticos de integridad | 0 | Auditoría de eventos y refs |
| Retorno | Observación cualitativa durante varias semanas | Entrevistas y uso registrado |

Un participante que abandona cuenta como no completado. Se registra el paso
exacto de abandono, no solo el resultado final.

### Métricas a registrar

Por participante:

- tiempo hasta instalar y abrir la TUI;
- tiempo hasta crear el thread y lanzar el run;
- tiempo hasta comprender el checkpoint;
- tiempo hasta ejecutar la validación;
- tiempo hasta integrar o explicar el bloqueo;
- número y motivo de intervenciones del fundador;
- escapes: abrir otra terminal, buscar un OID a mano, consultar documentación
  externa o reconstruir el estado fuera de Aeril;
- paso exacto de abandono, si ocurre;
- problemas de integridad observados;
- citas textuales y fricciones repetidas.

Estas métricas se capturan antes de implementar PA0-C/D/E para no confundir
mejoras de producto con resultados de adopción.

---

## 5. Contrato de Attention (PA0-B)

Attention es una **proyección read-only** sobre runs, checkpoints, validación,
review e integración existentes. No escribe eventos, no mueve refs, no ejecuta
validaciones y no crea un segundo motor de tareas.

### Campos por tarea

`thread_id`, `thread_title`, `status`, `priority`, `status_label`, `next_action`,
`reason`, `run_state`, `run_id`, `snapshot_id`, `source_oid`,
`checkpoint_run_id`, `historical_checkpoint`, `validation`, `review`,
`integration`.

La acción sugerida nunca contradice el estado durable ni los controles de Git.

### Precedencia determinista

De mayor a menor prioridad:

1. **Integrity hold** — evidencia contradictoria o ilegible.
2. **Permission pending** — pedido de permiso de un run todavía vivo.
3. **Recovery required** — checkpoint preparado sin resolución y sin supervisor
   vivo; integración solicitada sin desenlace.
4. **Integration blocked** — bloqueo durable para el OID exacto del checkpoint.
5. **Review required** — comentarios abiertos o falta de acknowledgement.
6. **Validation failed** — fallo, OID incorrecto o evidencia inválida.
7. **Validation required** — no hay validación para snapshot + OID.
8. **Ready to integrate** — revisión y validación verificadas para el snapshot
   exacto. La acción es **abrir el preview de integración**, no afirmar que Git
   ya es elegible.
9. **In progress** — el agente sigue trabajando.
10. **Failed** — el último run falló o fue cancelado.
11. **Done** — integrado, o run terminado sin checkpoint pendiente.
12. **No run** — la tarea todavía no tiene trabajo revisable.

### Distinciones obligatorias

- `run.completed` no implica `checkpoint.ready`.
- Un `prepared` sin resolución es recuperable; un `ready` verificado es revisable.
- La validación y el acknowledgement se atan a `snapshot_id` + `oid` exactos.
- Una integración completada solo cierra el checkpoint cuyo OID coincide.
- Un evento de integración anterior o de otro OID no oculta un checkpoint nuevo.
- Los comentarios resueltos no equivalen a revisión; hace falta el acknowledgement.
- Un permiso pendiente de un supervisor muerto es recuperación, no autorización.
- Un `permission.requested` sin respuesta al cierre, o un
  `permission.audit_failed`, es integridad/recovery: nunca un run limpio. Los
  eventos legacy sin `run_id` también fallan cerrado.
- Un checkpoint `ready` exige `prepared` consistente, sin marcador `failed`, y
  ref/objetos Git verificables para poder revisarse o integrarse.
- Si el último run es posterior al checkpoint, el checkpoint se marca
  `historical_checkpoint` y no se presenta como resultado actual del run nuevo.
  La acción de revisión sigue disponible, pero la integración siempre pasa por
  el preview y el CAS.

### Superficies

- **TUI:** la lista principal muestra, por tarea, el estado durable y
  `→ próxima acción`; la tecla `a` registra el acknowledgement del snapshot cuyo
  patch está visible (`displayed_snapshot`), nunca “el último ready” si el panel
  muestra otro snapshot.
- **CLI:** `aeril attention` imprime una vista legible;
  `aeril attention --json` devuelve la proyección completa para scripts y tests.
  `--limit N` acota las tareas consideradas.

---

## 6. Decisión de revisión y gate de integración

- Se agrega `review.acknowledged` con `snapshot_id` y `oid`, actor `human`.
- Es idempotente para el mismo snapshot/OID; repetirlo no crea otro evento.
- No se puede emitir con comentarios abiertos en ese snapshot/OID. El chequeo de
  comentarios y la escritura del ack comparten una transacción `IMMEDIATE`.
- La ausencia de comentarios **no** se interpreta como revisión.
- El ack exige un `checkpoint.ready` real con `prepared` consistente en
  snapshot/OID/ref/base/files/run_id, sin `checkpoint.failed` para el mismo
  snapshot, y con ref/objetos Git verificables y ancestría desde la base.
- El preview de integración asistida exige, para el snapshot exacto:
  1. checkpoint `ready` y `prepared` consistentes;
  2. sin comentarios abiertos;
  3. `review.acknowledged` durable;
  4. validación `Passed` para el mismo OID.
- La confirmación sigue revalidando preview, source y target esperado antes del
  CAS. El acknowledgement no reemplaza ninguna verificación de Git.

### Política de invalidación posterior al acknowledgement

El acknowledgement identifica bytes inmutables (`snapshot_id` + `oid`), no una
secuencia de comentarios. La política implementada es:

- **Comentario nuevo después del ack, mismo snapshot:** queda abierto y bloquea
  la integración (`review pending`). No borra el ack.
- **Comentario abierto existente:** impide emitir el ack y bloquea la integración.
- **Comentario resuelto después del ack:** el mismo ack vuelve a considerarse
  válido, porque el snapshot/OID no cambió. No se exige un segundo ack.
- **Feedback seguido de un checkpoint nuevo:** el snapshot nuevo tiene otro
  `snapshot_id`/`oid` y exige un ack nuevo.
- **Checkpoint posterior en el mismo thread con otro OID:** el ack anterior no
  participa; el checkpoint nuevo exige revisión explícita.

Un cambio de código siempre produce un snapshot/OID nuevo; por lo tanto, nunca
hereda un acknowledgement anterior. Los comentarios son anotaciones sobre bytes
inmutables y no convierten un cambio no revisado en revisado.

---

## 7. Gate estricto de Hermes real (PA0-D)

No se certifica “Hermes compatible” con un backend fake. La aceptación requiere
una sesión real autenticada y evidencia de que el participante puede:

1. crear un thread;
2. lanzar una tarea de edición;
3. ver progreso y resultado;
4. obtener un checkpoint verificable;
5. agregar una observación y enviar feedback;
6. validar el snapshot exacto;
7. completar el flujo de integración admitido;
8. cerrar y reabrir Aeril conservando el historial.

Todo bajo la restricción vigente de **un Hermes autenticado a la vez**. Las
pruebas de cierre normal se registran por separado de las pruebas de crash: una
recuperación exitosa no demuestra reversibilidad de acciones externas.

---

## 8. Condiciones de cierre del milestone

| Bloque | Condición de cierre | Estado |
| --- | --- | --- |
| PA0-A | Este contrato revisado y congelado antes de implementar PA0-B. | Entregado en esta rama |
| PA0-B | Attention determinista, read-only, visible en TUI/CLI y cubierta por tests de precedencia, integridad, permisos, recovery, read-only y tamaños 80×24 / 120×40 / 180×50. | Entregado en esta rama |
| PA0-C | Camino manual visible para integración bloqueada, sin debilitar Git. | Pendiente |
| PA0-D | Binario Linux reproducible + flujo Hermes real autenticado. | Pendiente |
| PA0-E | 3–5 desarrolladores externos ejecutan el recorrido sin asistencia. | Pendiente |

PA0 no se considera cerrado por tener tests verdes. El gate de producto es el
piloto externo; el gate técnico es la evidencia durable y la ausencia de
problemas críticos de integridad.

---

## 9. Riesgos y límites declarados

- La coordinación de leases es advisory entre procesos de Aeril; no aísla de
  escritores externos con permisos sobre los mismos archivos.
- SQLite y Git no comparten una transacción atómica. La intención durable y la
  reconciliación reducen la ventana de crash, no la eliminan.
- La detección de permisos y recuperación depende de evidencia local (PID,
  starttime, lease); no es un sistema distribuido.
- La muestra de 3–5 participantes no permite inferencias de retención.
- La integración soportada es fast-forward; una divergencia se informa y no se
  resuelve automáticamente en PA0.
- La proyección de review escanea comentarios/feedback del thread sin un límite
  artificial de cantidad; PA0 asume historiales acotados y no usa esta consulta
  como índice de búsqueda global.
- `make budget` y los benchmarks no certifican Hermes real; los mocks miden
  consumo y latencia de la aplicación, no compatibilidad de protocolo.
