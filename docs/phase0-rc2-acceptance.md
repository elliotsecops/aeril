# Fase 0-RC2 — Aceptación de los tres blockers de merge

> **Base auditada (segunda auditoría independiente):** `1f359e71897e2192077f70253e7768f34dab1b4e`
> **Implementación RC2 (fixes + tests):** `11fd0d7889246ace45da3a3fa5cd8ffb72fe35cc`
> **Este documento:** commit posterior en la misma rama `fix/phase0-reliability-rc2`
> (el SHA final de la rama se reporta al pie).

Los tres escenarios fueron **reproducidos con tests deterministas contra el
código pre-fix** antes de corregirlos. Nada se declara resuelto sin test.

---

## 1. B1 — Amplificación de memoria del pipeline ACP

**Escenario original:** `mpsc::channel(1024)` acota mensajes, no bytes; el
productor decodificaba JSON completo antes de encolar y el consumidor aplicaba
el presupuesto de 64 MiB recién al desencolar (auditor: +210 MiB por 192 MiB
de entrada).

**Reproducción pre-fix** (`scripts/bench/b1_mem_probe.py`, RSS del proceso Aeril):

| Entrada | Estado | ΔRSS pre-fix |
|---|---|---|
| 160 frames × 300 KB (48 MB) | completed | **+34.1 MB** |
| 300 frames × 300 KB (90 MB) | failed (presupuesto de turno, DESPUÉS de encolar) | **+40.1 MB** |

El delta **crecía con el stream** — la firma de la amplificación.

**Corrección:** la cola internas transporta la **línea cruda sin decodificar**;
el productor **reserva bytes en un semáforo ANTES de encolar** (backpressure
real); el consumidor decodifica al desencolar y **libera por Drop** (éxito,
error, cancelación o cierre); límites: línea 4 MiB (`AERIL_ACP_MAX_LINE_BYTES`),
en vuelo 8 MiB (`AERIL_ACP_INFLIGHT_BYTES`), turno 64 MiB
(`AERIL_ACP_MAX_TURN_BYTES`); un frame mayor al presupuesto espera la cola
vacía y transita solo (cota sin allocs extra). `kill()`/`Drop` terminan el
**grupo de procesos** del backend.

**Verificación:**

- `tests/rc2_b1.rs::b1_cota_de_bytes_en_vuelo_con_consumidor_lento`:
  backend fake emitiendo 100 × 60 KB con consumidor de 20 ms/frame;
  **peak en vuelo 241.032 ≤ 262.144 bytes** (presupuesto) y ≤ total/8;
  los 101 updates se entregan (backpressure, no pérdida).
- `tests/rc2_b1.rs::b1_frame_individual_mayor_que_el_budget_transita_solo`:
  frame de 200 KB con presupuesto de 64 KB transita exclusivo; sin procesos
  huérfanos tras `kill()` (verificado escaneando `/proc`).
- Post-fix (`b1_mem_probe`): 48 MB → **+26.4 MB**, 90 MB → **+22.4 MB**
  (**plano**: ya no escala con el stream), 48 MB en frames de 30 KB → +10.5 MB.
  El residuo se debe al costo transitorio por frame (decode + serialización +
  SQLite) y a la retención de arenas del allocador, acotado por tamaño de
  frame × constante, no por volumen.

## 2. B5 — Lease con dueño vivo

**Escenario original:** el TTL vencido transfería ownership aunque el proceso
dueño siguiera vivo; dos runs completos modificaban el mismo workspace.

**Reproducción pre-fix:** `tests/rc2_b5.rs::b5_ttl_vencido_con_dueno_vivo_no_transfiere_ownership`
contra `1f359e7` → falla: B adquirió el workspace con A mid-turn y produjo su
propio run completo (snapshot incluido) sobre el mismo checkout. También
reproducible con SIGSTOP (`b5_suspension_*`).

**Corrección (invariante: Aeril nunca autoriza dos runs supervisados vivos
sobre el mismo workspace):**

- Adquisición: dueño **vivo ⇒ `Busy` SIEMPRE** (TTL vencido incluido); la
  transferencia automática queda solo para dueños probadamente muertos
  (PID ausente o **PID reutilizado** detectado por `starttime`, esquema v3).
- Recuperación explícita: `aeril lease status` / `aeril lease release
  --workspace W`. **RC2.1: `--force` ya no puede pisar a un dueño vivo** — la
  única vía es terminar el proceso y verificar su muerte (o esperar la toma
  automática por PID muerto/reutilizado). Ver
  `docs/phase0-rc2-acceptance.md` §RC2.1.
- Dueño: si la renovación falla (0 filas o error del store) el run **aborta**:
  cancela el backend vía la señal combinada del turno, registra
  `workspace.lease_lost`, marca `run.ended failed` y **no captura checkpoint
  ni reporta completitud**.
- `worktree rm` sigue bloqueado mientras el dueño esté vivo (aun con TTL
  vencido); la muerte debe estar probada para recuperar.
- Supervisión: `kill()`/`Drop` terminan el grupo de procesos del backend.

**Verificación** (`tests/rc2_b5.rs`, 6 tests, procesos Aeril reales + mock
determinista): transferencia bloqueada con dueño vivo; suspensión SIGSTOP con
TTL corto sigue bloqueando y libera tras reanudar/completar; pérdida forzada
de ownership ⇒ aborto fallido sin checkpoint (evento durable + backend sin
huérfanos, sin runs `running` colgados); PID reutilizado ⇒ toma de control
registrada; `worktree rm` bloqueado con heartbeat vencido; dueño muerto ⇒
recuperación sin dos escritores.

## 3. A5 — Gitlinks no inicializados

**Escenario original:** escrituras bajo un gitlink no inicializado podían ser
invisibles al status: el run reportaba completitud sin checkpoint.

**Reproducción pre-fix** (`tests/rc2_a5.rs` contra `1f359e7`):

- write SOLO bajo el gitlink → **completed** con snapshot (el contenido del
  submódulo se tra-versaba como archivos comunes: prohibido);
- edición mixta (archivo normal + write colado) → **completed** con el write
  bajo el gitlink **omitido en silencio**. Ambos modos reproducidos.

**Corrección:** los gitlinks se enumeran desde el **ÍNDICE** (independiente del
status). Inicializado ⇒ puntero capturado desde el árbol base y cambios de
puntero rechazados (RC1). **No inicializado con CUALQUIER contenido bajo su
path ⇒ rechazo explícito** con error accionable (`git submodule update --init`
o quitar los archivos). No inicializado y vacío ⇒ se captura el resto normal.
La garantía exacta queda documentada en `docs/phase0-status.md`.

**Verificación:** 4 tests — rechazo de write bajo gitlink; soportado sin
escrituras; mixto rechazado completo (sin checkpoint parcial); regresión RC1 de
submódulo inicializado modificado.

---

## 4. Archivos modificados

| Área | Archivos |
|---|---|
| B1 | `src/agent/acp.rs` (cola cruda + semáforo + `AcpLimits` + grupo de procesos), `src/agent/mock.rs` (`flood`), `scripts/bench/b1_mem_probe.py`, `tests/rc2_b1.rs` |
| B5 | `src/store/leases.rs` (pid_start, liveness estricta, release explícito), `src/store/schema.rs` (v3), `src/session.rs` (guard con aborto + cancelación combinada + evento), `src/store/events.rs` (`workspace.lease_lost`), `src/cli.rs` (`lease`), `tests/rc2_b5.rs`, ajuste de `tests/wave_b.rs` al invariante nuevo |
| A5 | `src/git/snapshot.rs` (guard de gitlinks por índice), `tests/rc2_a5.rs` |
| Scope-control | campo muerto `RunSummary.session_resumed` eliminado |

## 5. Resultados de la suite

```
cargo test --locked --offline --jobs 2        → 56 tests, 0 fallos
cargo fmt --all -- --check                    → limpio
cargo clippy --locked --offline --jobs 2 --all-targets -- -D warnings → limpio
```

Gates clásicos (sin regresión): **Gate A** retención (`test_a_retention`),
**Gate B** crash (`test_b_crash`), CAS de integración (`wave_a::a1_*`),
fail-closed de permisos (`wave_a::a2_*`), session/load real (`wave_a::a3_*`),
entrega de feedback (`wave_a::a4_*`), carreras de publicación
(`wave_b::b3_*`), intención de integración (`wave_b::b4_*`), cancelación con
plazo (`wave_b::b6_*`).

## 6. Benchmark (RC2)

Declaración completa en `docs/phase0-engineering-handoff.md` §Mediciones RC2:
NVMe ext4, caché warm declarada, release LTO, dataset 100×1000×10 con
comentarios.

| Métrica | Límite | Medido (make budget/scale) | |
|---|---|---|---|
| Arranque p95 (abre thread, n=30, externo) | 300 ms | **25.6 ms** (0 fallos) | ✅ |
| RSS idle / con thread abierto | 50 MB | **6.7 / 7.5 MB** | ✅ |
| Caché residente por thread | 10 MiB | **0.82 MiB** | ✅ |
| Binario stripped | 30 MB | **5.5 MB** | ✅ |
| 5 runs mock: app | ≤ 500 MB | **8.7 MB** (agentes 16.2 / total 24.9 aparte) | ✅ |
| Cola ACP en vuelo | ≤ 8 MiB (configurable) | **peak 241 KB ≤ 256 KB** (test con presupuesto chico) | ✅ |

**Límites honestos:** Hermes real ×5 **no** fue benchmarkeado (perfil
compartido, no medible en v0). El RSS residual del probe B1 (~26 MB con frames
de 300 KB) está acotado por tamaño de frame × constante; la cota de **cola**
(8 MiB default) está probada por instrumentación.

## 7. Riesgos residuales (declarados)

1. **Lease advisory**: coordina procesos de Aeril; no aísla de procesos
   externos con permisos del usuario (MANIFIESTO §8). Filas legacy sin
   `pid_start` caen a liveness simple por PID (documentado).
2. **Backend huérfano con padre muerto**: si el proceso Aeril muere de golpe,
   los hijos del backend no son matados por nadie; dependen de EOF en stdin
   para autoterminarse (el mock lo hace; Hermes se asume, no verificado).
3. **Amplificación de decode por frame**: un único frame ≤ 4 MiB puede decodear
   a un `Value` algo mayor transitoriamente; acotado por línea, no acumulativo.
4. **Submódulos**: política conservadora de rechazo; el contenido de
   submódulos no se soporta en v0.
5. El documento original de la segunda auditoría no vino adjunto; la
   trazabilidad se apoya en el brief y en la reproducción pre-fix registrada.

## RC2.1 — Fence de lease (parche final, rama `fix/phase0-lease-fence`)

**Defecto confirmado:** `lease release --force` borraba el lease de un dueño
VIVO; B adquiría W y ambos runs completaban sobre el mismo workspace
(reproducido con dos procesos Aeril reales: el test A falla en el commit
`62d64e4` con el release aceptado sobre un PID vivo).

**Correcciones:**

1. **`--force` no pisa liveness (jamás).** `release_external` rechaza la
   liberación si el dueño está confirmado vivo, incluso con `--force`; el
   error explica la única vía: terminar el proceso dueño, verificar su muerte
   (`ps -p <pid>`) y reintentar — o esperar la toma automática por PID
   muerto/reutilizado. `pid_start` (starttime) sigue siendo la verificación.
   `worktree rm` continúa bloqueado mientras el dueño viva.
2. **Fence transaccional de checkpoints.** `checkpoint.prepared` y
   `checkpoint.ready` verifican el ownership DENTRO de la misma transacción
   `BEGIN IMMEDIATE` del append (sin ventana verificación↔persistencia). Un
   dueño obsoleto no puede publicar `ready`; si la pérdida ocurre tras
   publicar la ref (objetos inmutables, jamás se borra la ref), se escribe un
   `checkpoint.failed` terminal para que la recuperación NO lo complete —
   la distinción es: crash legítimo sin marcador ⇒ recovery completa (Gate B
   intacto); pérdida de lease con marcador ⇒ terminado.
3. **`run.ended` exitoso fenceado.** El estado `completed` (incluido el caso
   sin cambios, que no captura checkpoint) sólo se registra si el run sigue
   siendo el dueño; si no, se degrada a `failed` con razón explícita y evento
   `workspace.lease_lost` en la MISMA transacción.
4. Extra pequeño: la configuración extrema de límites ACP por env ya no puede
   paniquear (`Semaphore::MAX_PERMITS`/u32 con clamp).

**Tests de regresión (tests/rc2_1_lease_fence.rs):**

| Test | Escenario | Resultado |
|---|---|---|
| A | release --force con dueño VIVO y heartbeat vigente ⇒ conflicto; B bloqueado; A conserva exclusividad y contenido; libera al terminar | ✅ |
| B | heartbeat vencido + dueño VIVO (intervalo/TTL default) ⇒ release rechazado; ningún segundo run adquiere | ✅ |
| C | dueño muerto verificado ⇒ `lease release` (sin force) funciona; B adquiere y completa solo | ✅ |
| D | ownership robado por fixture de DB con renovación default ⇒ fence rechaza `prepared`: sin `checkpoint.ready` nuevo, `run.ended failed`, evento `workspace.lease_lost`, nada pendiente | ✅ |
| E | Gate B original (`test_b_crash`) sin regresión: los `prepared` legítimos tras SIGKILL se recuperan | ✅ |

Más `b1_configuracion_extrema_no_paniquea` y la migración v2→v3
(`store::tests::migracion_v2_a_v3_conserva_datos`).

**Verificación:** 61 tests (0 fallos), `fmt` y `clippy -D warnings` limpios,
suite con `--locked --offline`. **Performance: evidencia RC2 heredada** (los
cambios no tocan el arranque ni el RSS; no se repitió la campaña).

## 8. Autoevaluación de merge readiness

Los **tres blockers RC2** tienen reproducción pre-fix, corrección implementada
y tests deterministas en verde; los gates Phase 0 y las regresiones RC1 no
muestran fallos. **Considero la rama lista para revisión de aceptación
independiente**, con los riesgos residuales de §7 declarados. No la declaro
mergeada: eso corresponde a la auditoría externa.

**SHA final de la rama:** se reporta en el mensaje de entrega (commit de este
documento).
