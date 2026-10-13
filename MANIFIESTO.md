# AERIL — Manifiesto Ligero

**Versión:** 0.4 (segunda pasada de revisión aplicada)
**Estado:** documento fundacional. Va antes de la primera línea de código.
**Nombre:** Aeril — del campo semántico de *aery/aerial* (aire, vuelo, lo que se mueve sin esfuerzo). Velocidad y liviandad en una palabra. Disponible en crates.io, npm, .dev y .sh.

**Historial:** v0.1 (borrador) → v0.2 (nombre) → v0.3 (1ra revisión: snapshot+ancla, workspace verificado, store de Aeril, UI utilizable ≠ primer frame, corte vertical primero) → **v0.4** (2da pasada: esquema mínimo de 3 tablas con snapshot real desde el primer comentario; protocolo prepared/ready; limpieza de refs por evidencia; corrección competitiva; integración acotada a fast-forward).

> *Velocidad y liviandad, juntas, en el aire.*

---

## 1. Por qué existe este producto

El desarrollador promedio, hoy, abre su entorno de trabajo y se come 8 GB de RAM antes de escribir una línea. VS Code, Slack, Chrome, Spotify, Docker. Las herramientas se volvieron pesadas porque durante años **el hardware absorbió el problema**: en vez de optimizar, la industria subió el piso de RAM requerido y el usuario terminó obligado a comprar una laptop nueva para seguir haciendo lo mismo.

Este producto se para en el lado opuesto de esa pendiente.

**La promesa, en una frase:**

> Corre completo en tu ThinkPad de 2018 con 16 GB. No necesitás comprar una laptop nueva.

No es una limitación autoimpuesta. Es el posicionamiento. Y es una promesa que los competidores grandes **no pueden copiar** sin reescribir su arquitectura desde cero: VS Code no puede prometerla sin tirarse abajo, Orca no puede sin dejar de ser Electron.

Además, por primera vez en años, ser liviano **no implica sacrificar experiencia**. Rust maduro, ratatui en producción, SQLite, WebGPU: en 2026 se puede hacer una app rápida sin que se vea pobre. Eso es lo que hace que esta ventana exista ahora y no hace cinco años.

---

## 2. Los presupuestos (duros, no aspiracionales)

Estos números son **contrato**, no objetivo. Si una build los rompe, la build falla. Ninguna app se vuelve pesada por una decisión; se vuelve pesada por mil decisiones chicas que parecían inofensivas. Los presupuestos existen para que cada una de esas decisiones chicas tenga que justificarse.

| Recurso | Presupuesto | Cómo se mide |
|---|---|---|
| RAM base del binario (TUI, idle) | **≤ 50 MB** | RSS tras arranque + 5 s reposo, con historial representativo |
| Binario en disco (release, stripped) | **≤ 30 MB** | Tamaño de `target/release/aeril`, por plataforma |
| Arranque a UI utilizable | **≤ 300 ms (p95)** | Instrumentado desde fuera del proceso; entrada procesada, no frame vacío. Ver §6b |
| Dependencias de runtime | **0 de app** | `ldd`: solo ABI del OS. Shell/git/motor son requisitos de funciones puntuales |
| RAM del conjunto con 5 agentes | **≤ 500 MB app + medir conjunto** | Aeril + motores + shells, publicado junto al límite por panel |
| Caché residente por thread | **≤ 10 MiB** | Páginas vivas en RAM; el estado durable tiene cuota y política aparte |

**Las tres definiciones que el gate debe dejar por escrito antes de medir:** (a) *UI utilizable* ≠ primer frame —el rectángulo vacío que se congela después no cumple; se mide hasta entrada procesada, desde fuera del proceso. (b) *Arranque frío* —reiniciar el proceso no vacía la page cache del OS; el frío real de filesystem se mide en un runner controlado, no etiquetando el cálido como frío. (c) *Hardware/disco/perfil de historial* declarados —un p95 absoluto sobre CI compartido no es una garantía reproducible para un ThinkPad.

**Nota sobre el 10 MB por thread:** no es un límite de todo estado. Historial arbitrario y logs no caben en una cota fija (1 MB/s consume 10 MB en 10 s). Es el presupuesto de **caché residente**; el almacenamiento durable tiene cuota y limpieza separadas, y jamás expulsa en silencio un diff comentado.

**Regla cultural que hace que se cumplan en el mes 6:** cada PR que sube un presupuesto tiene que justificarse por escrito en la descripción y requiere aprobación explícita. El default ante toda propuesta de feature nueva es **no**.

---

## 3. Invariantes arquitectónicos

Decisiones que no se renegocian feature por feature:

1. **Un solo binario. Cero runtime.** Sin Node, sin Electron, sin VM, sin intérprete, sin daemon obligatorio. Si algo necesita un runtime, va en un proceso aparte y opcional.
2. **El motor de agentes es externo, hablado por un contrato estructurado (ACP u otro protocolo con sesiones, permisos, diffs y cancelación).** Nunca embebido en el mismo proceso. Hermes es un **backend inicial opcional**, no una dependencia arquitectónica obligatoria. El contenedor no puede pesar más que su contenido. No se inventa un socket propio si ya existe un protocolo estándar.
3. **Un store transaccional local (SQLite) para Aeril.** Threads, eventos, runs y comentarios viven ahí. Los motores de agentes externos conservan su propio estado privado; "un único store" significa uno de Aeril, no prohibir stores de backends.
4. **La GUI, cuando exista, es web servida desde el propio binario.** Nunca Electron. Misma DB, mismos threads, otra superficie.
5. **Datos propios exportables a texto plano, formatos documentados, cero formatos propietarios opacos.** `aeril export` produce JSONL/Markdown/patch inspeccionables. SQLite es binario: no se promete que `cat`/`jq` lean el `.db` directo, se promete exportación sin pérdida.
6. **Cada ejecución que edita código corre en un workspace verificado por Aeril.** Aeril crea y valida el workspace antes de que el agente escriba. Un worktree **no es un sandbox**: no se anuncia aislamiento de red, secretos ni rutas absolutas. Overhead de motores con cero tareas activas: cero.
7. **Un thread durable sobrevive a la muerte de su proceso y de su worktree.** La conversación, el snapshot revisado y los comentarios existen aunque el checkout desaparezca. Un comentario se ancla a un snapshot inmutable (path, lado, rango, blob oid), no a un `event.id` ni a un número de línea suelto.
8. **Modelo de confianza declarado (no sobreprometido).** Aeril separa los archivos de trabajo de cada tarea; **no restringe los permisos del agente sobre la computadora.** El agente conserva los permisos del usuario: puede acceder a otras carpetas, usar credenciales accesibles, conectarse a Internet, arrancar programas, y tomar el mismo puerto/servicio que otro agente. Borrar el worktree NO detiene los procesos que el agente lanzó ni deshace sus acciones externas.

---

## 4. Reglas de contribución

Para los agentes y humanos que construyan esto:

1. **El presupuesto manda.** Todo PR se mide contra la tabla de §2.
2. **Medir antes de opinar.** "Es rápido" no es evidencia; el test de timing lo es.
3. **Restar antes de sumar.** Ante un problema nuevo, la primera solución evaluada es quitar algo, no agregar.
4. **Ninguna dependencia nueva** sin declarar su coste en RAM, disco y arranque, y sin que entre en presupuesto.
5. **La deuda de peso es deuda real.** Se paga en la misma iteración o se anota explícitamente. No se arrastra en silencio.
6. **El scope de v1 es cerrado.** Lo que no está en §5 no se construye "de paso".

---

## 5. Lo que NO construimos

Cada ítem de esta lista es un lugar donde Orca y Delta se volvieron pesados. Renunciamos a ellos a propósito:

- ❌ Sincronización multi-usuario en tiempo real / CRDTs
- ❌ Cloud runners, acceso mobile, push
- ❌ Marketplace de plugins (v1)
- ❌ Runtime propio de agentes (se habla con backends externos por contrato; Hermes es uno opcional)
- ❌ Un VCS nuevo (git worktrees; git sigue siendo el checkpoint —pero guardando también el patch revisado)
- ❌ Splash screen, wizard de onboarding, animaciones de carga
- ❌ Telemetría que no sea explícitamente opt-in y local
- ❌ Delegación de código gestionada por el motor (en v0 Aeril lanza y verifica cada agente editor)
- ❌ Emulador de terminal universal / shell integration equivalente a Warp (v0 soporta shells y CLIs sin interfaz estructurada, sin prometer paridad total)
- ❌ Continuidad de tareas tras salir de la app (exige supervisor residente; decisión posterior)
- ❌ Integración Git general (merge/cherry-pick) en v0: solo fast-forward a rama destino explícita; divergencia o conflicto = bloqueo comprensible que conserva el trabajo
- ❌ Recolector destructivo de snapshots en el ensayo: solo listar candidatos; la liberación validada llega después

---

## 5c. Persistencia y checkpoints (v0.4, de la segunda pasada)

**Esquema mínimo = tres tablas** (`threads`/`runs`/`events`), pero el snapshot existe desde el primer comentario. Se posponen tablas, no garantías.

**Identidades estables desde el día 1** (la migración posterior a `workspaces`/`artifacts` debe ser mecánica): `repo_id`, `workspace_id`, `snapshot_id` son identidades; las rutas son atributos y la ref Git es un locator de contenido. Cada `run` fija base OID exacta, sesión externa y perfil del motor —reanudar no cambia retroactivamente la base. Payloads versionados.

**Protocolo de checkpoint:**

```text
construir objetos inmutables
→ persistir checkpoint.prepared  (snapshot_id, ref, OID esperado)
→ publicar refs/aeril/checkpoints/<snapshot_id>   (si ya existe con el mismo OID = retry; otro OID = conflicto, detener)
→ verificar ref + objetos
→ persistir checkpoint.ready
```

Los comentarios solo se habilitan tras `ready`. Un crash entre `prepared` y `ready` deja una operación **recuperable** al reabrir; nunca se reconstruye desde el checkout actual y se llama el mismo snapshot.

**Limpieza de refs por evidencia, jamás por edad.** Protegido (nunca expira): snapshot citado por comentario retenido **incluso resuelto**, por revisión/validación retenida, por run activo u operación pendiente. Resolver un comentario NO desprotege su snapshot. `prepared` coherente → reconciliar. Abandono confirmado sin consumidores → cuarentena (`refs/aeril/quarantine/`), 7 días desde la primera clasificación durable. **Una ref sin fila en SQLite no prueba basura** (puede ser una DB restaurada) → cuarentena conservadora, jamás TTL automático. Liberar = revalidar consumidores + `update-ref` transaccional con OID esperado (CAS). No encadenar checkpoints (padre común = base de la tarea; el orden vive en SQLite). Sin `gc --prune=now` con escritores.

---

## 5b. El corte vertical va primero (corregido tras revisión de arquitectura)

El orden de construcción original (multiplexor → threads) se invierte. El primer riesgo a retirar no es "qué tan lindos son los paneles", es **si un thread completo y recuperable entra en el presupuesto**:

> editar → snapshot → revisar → comentar → feedback → reanudar → validar → integrar, sobreviviendo a un crash.

**Ensayar eso con un solo thread, un backend, un repo, en 5-10 días.** Si entra en 300 ms y 50 MB, el producto existe y los paneles son una extensión. Si no entra, ningún layout ni GUI lo arregla. Los splits, el navegador de archivos y la GUI web vienen después —construirlos antes diluye la única ventaja defendible (que el flujo sobrevive al reinicio y corre liviano).

---

## 6. Cómo se verifica

En CI, y localmente con `make budget`:

```
make budget        # corre todos los chequeos
make budget-size   # binario ≤ 30 MB
make budget-deps   # ldd: cero dependencias fuera de base del sistema
make budget-mem    # RSS idle ≤ 50 MB
make budget-start  # arranque frío p95 ≤ 300 ms
make budget-scale  # app con 5 agentes ≤ 500 MB
```

**Build roja = no merge.** No hay excepción por "es solo esta vez".

---

## 7. El compromiso

> Si un día este producto necesita 16 GB para funcionar, este producto falló.
>
> Las herramientas se adaptan al usuario. No el usuario a las herramientas.

---

*Este documento es la Constitución del proyecto. Toda decisión de arquitectura se evalúa contra él antes de evaluarse contra la competencia.*
