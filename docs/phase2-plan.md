# Fase 2 — Plan técnico: Hermes real autenticado y concurrente

> Estado: **PLAN — sin autorización de implementación.** Baseline: `main @ 47f0318`
> (Fase 1 cerrada). Este documento no reemplaza a `MANIFIESTO.md` y no modifica
> ningún contrato de Fase 0/1. Autoría: planificación; ejecución sujeta a
> revisión independiente y autorización explícita.

## 0. Objetivo ejecutivo

Llevar a Aeril de **concurrencia mock determinista (N=2, certificada)** a
**operación real con Hermes ACP autenticado**: perfiles independientes con
credenciales propias, dos sesiones ACP autenticadas simultáneas sobre
workspaces verificados, consumo medido con presupuestos explícitos, y garantías
de seguridad auditables — sin copiar credenciales, sin contaminación cruzada y
sin debilitar ninguna garantía de Fase 0/1.

La pregunta GO/NO-GO central: **¿puede soportarse concurrencia real autenticada
sin crossover de credenciales ni interferencia de procesos inaceptables?** Si
la evidencia dice que no, se conserva single-Hermes como modo soportado y se
recomienda un enfoque de aislamiento revisado.

## 1. Evidencia revisada y baseline de compatibilidad

Documentos: `MANIFIESTO.md`, `docs/phase1-s1-hermes-isolation.md`,
`docs/phase1-status.md`, `docs/acp-hermes-spec.md`, `docs/phase1-plan.md`.
Código relevante: `src/agent/mod.rs` (`resolve_backend`), `src/agent/acp.rs`
(spawn con grupo de procesos propio, cancel/plazo, kill), `src/session.rs`
(Runner, lease de workspace, `engine_session_id`, `attempted/accepted/verified`),
`src/store/leases.rs`, `src/tui/mod.rs` (gate single-Hermes, N=2).

**Baseline instalado verificado (2026-10-15):**
`hermes --version` → `Hermes Agent v0.21.5+8877.g865ba90 (2026.9.24) · upstream
865ba906` · `hermes acp --version` → `0.21.5+8877.g865ba90`. **Idéntico a la
versión de S1 y a la del `acp-hermes-spec`** — las conclusiones de S1 siguen
aplicando, con la advertencia de §1.1.

### 1.1 Suposiciones dependientes de versión (re-validar ante cualquier cambio)

| # | Suposición | Fuente | Si cambia |
|---|---|---|---|
| V1 | `HERMES_HOME` es de primera clase y todo el estado deriva de ahí | S1 + spec | el modelo de perfiles de §2A se re-diseña |
| V2 | `hermes profile create` registra gateway/multiplexor (efectos laterales) | S1 | podría habilitarse como alternativa de gestión |
| V3 | `auth.json` queda fuera de `profile create --clone` por grants OAuth single-use | S1 | prohibición de copia seguiría vigente por política |
| V4 | Locks de runtime asumen **un escritor por `HERMES_HOME`** | S1 | se evaluaría compartir perfiles (no recomendado) |
| V5 | `session/load` re-ejecuta replay ANTES de responder; sesión desconocida ⇒ OK vacío | spec §2.8 | cambia el protocolo de restauración (§9) |
| V6 | `session/resume` de id desconocido crea sesión nueva **en silencio** | spec §6.9 | test de restauración debe usar `load`, no `resume` |
| V7 | 4 slots de executor compartidos entre sesiones del MISMO proceso | spec §6.6 | punto de contención a medir (§4) |
| V8 | Permisos: timeout 300 s; `_send_update` descarta a los 5 s | spec §6.6 | política de cancelación/UX |
| V9 | Credenciales ambientales descubribles (`gh auth token`, `~/.codex`, `~/.claude`) | S1 | modelo de amenaza §3C |

## 2. Modelo de seguridad

Se separan tres conceptos que Fase 1 ya distinguía pero que la Fase 2 debe
comprometer por escrito.

### A. Aislamiento de estado por perfil — **garantizable (verificado en S1)**

- Un `HERMES_HOME` por identidad de agente: `state.db` (+`-wal/-shm`),
  `sessions/`, `logs/`, `cache/`, `installs/`, `auth.json` y locks
  (`auth.lock`, `.leases`, `.prepare.lock`) por perfil.
- Lo único compartido es el **intérprete Python y el checkout del código**
  (solo lectura) — declarado, no un defecto.
- Invariante: **un escritor ACP activo por `HERMES_HOME`** (lock propio de
  Aeril + respeto de los locks de Hermes).

### B. Aislamiento de autenticación — **garantizable con onboarding interactivo**

- Credenciales viven en el `auth.json` del perfil; se provisionan **por
  perfil** con `hermes auth add` / `hermes model` / `hermes auth upgrade`
  (interactivo, humano en el loop).
- **Prohibido por política**: copiar, imprimir, loguear o reutilizar
  `auth.json`/tokens; usar `--clone` como fuente de credenciales; leer el
  perfil del host (`~/.hermes`) como fallback.
- Ciclo de vida: refresh/cooldown/exhaustion son del pool **del perfil**
  (`hermes auth refresh|status`); un fallo de auth se reporta y se detiene el
  run (fail-closed), nunca se reintenta con otro perfil.

### C. Aislamiento de credenciales del host — **NO garantizable bajo mismo usuario**

- S1 demostró que un perfil descubre credenciales fuera de `HERMES_HOME`
  (copilot vía `gh auth token`; `~/.codex`, `~/.claude`). Con el mismo usuario,
  un proceso hijo puede leer cualquier archivo del usuario.
- **Garantías que sí se pueden dar (mismo usuario):**
  - G1: Aeril nunca lee/copia/imprime credenciales (auditable: sin accesos a
    `auth.json` en el código de producto).
  - G2: los perfiles no comparten `auth.json` ni tokens entre sí.
  - G3: el spawn minimiza descubrimiento ambiental: `HERMES_HOME` explícito +
    limpieza de variables de credenciales conocidas (`GH_TOKEN`,
    `GITHUB_TOKEN`, `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, etc.) + **HOME
    sanitizado por agente** (validado en P2-S0; ver abajo).
  - G4: cualquier credencial que el agente use proviene de su perfil o del
    entorno explícitamente declarado — no por herencia silenciosa.
- **NO garantizado (se declara):** N1: un agente deliberadamente malicioso que
  lea archivos del host (p. ej. `~/.config/gh/hosts.yml`) — requiere sandbox OS
  o usuario separado; N2: aislamiento contra otros usuarios del sistema (fuera
  de alcance); N3: que una credencial ambiental no sea descubrible si el
  usuario decide no sanitizar HOME; N4: no-repudio de acciones del agente.
- **Opciones de aislamiento más fuerte (análisis, no baseline):**
  - *HOME sanitizado por agente* (recomendado como baseline de minimización):
    `HOME=<perfil>/home` con `.gitconfig` curado (user.name/email) y sin
    configs de herramientas; tradeoff: herramientas que dependen de configs del
    host (gh, npm, ssh) dejan de verlas — es el objetivo, pero puede romper
    tooling legítimo; mitigación: allowlist explícita de env por agente.
    **Riesgo abierto**: el bootstrap de `hermes` podría requerir
    `$HOME/.hermes/hermes-agent`; se valida en P2-S0 y, si rompe, se cae a
    sanitización de env únicamente + limitación declarada.
  - *Usuario Unix separado por perfil*: aislamiento fuerte real; costo: root,
    permisos cruzados sobre worktrees/repo (git), gestión de usuarios —
    desproporcionado para un terminal liviano; se documenta como modo futuro.
  - *Sandbox OS (bubblewrap/landlock, sin root)*: confinamiento efectivo;
    costo: dependencia externa de runtime (contradice "0 deps de app" si fuera
    obligatorio), complejidad de bind-mounts (worktree + perfil) y de permisos;
    se propone como **estudio separado opcional ("modo endurecido")** después
    de P2-S2, no como baseline.

**Modelo de amenaza (resumen):** activos = tokens OAuth por perfil, credenciales
del host, contenido de sesiones, código del worktree. Adversarios = (a)
contenido de repo malicioso que intente exfiltrar credenciales, (b) agente
malicioso/comprometido, (c) error de configuración de Aeril (cruce de
perfiles), (d) otros procesos del mismo usuario. Frontera = mismo usuario,
procesos separados con `HERMES_HOME` distinto. Las garantías G1–G4 aplican a
(a) y (c); (b) y (d) quedan declarados fuera hasta sandbox/usuario separado.

## 3. Arquitectura propuesta y alternativas

### Propuesta (baseline)

- **Perfil = directorio gestionado por Aeril** en
  `<AERIL_DATA_DIR>/hermes/<agent_id>/`:
  - `home/` → valor de `HERMES_HOME` (estado Hermes completo).
  - `profile.json` → marcador de propiedad de Aeril (id, proveedor/modelo
    declarados, timestamps, versión de Hermes al crear) — nunca credenciales.
  - `aeril.lock` → `flock` de escritor único, tomado por el supervisor del run
    mientras la sesión ACP vive.
  - permisos `0700` en el directorio y archivos propios.
- **Sin `hermes profile create` por defecto** (V2: gateway/multiplexor). Aeril
  gestiona los directorios crudos, como S1 verificó.
- **Asignación**: cada thread/run declara `agent_id`; el `agent_id` mapea a un
  perfil autenticado. Sin perfil autenticado ⇒ el run hermes se rechaza con
  mensaje accionable (single-Hermes se mantiene como default).
- **Spawn**: `hermes acp` con `HERMES_HOME=<perfil>/home`, env sanitizado
  (G3) y `HOME` según resultado de P2-S0; grupo de procesos propio (ya
  existente en `acp.rs`), sin cambios en supervisión S0/S0-R1.
- **TUI**: el gate actual (`is_hermes_backend && !runs.is_empty()` ⇒ rechazo)
  pasa a: backend hermes permitido hasta N=2 **solo si** cada run tiene perfil
  distinto y autenticado; con un solo perfil configurado, el segundo run
  hermes sigue rechazado. Límite N=2 intacto.
- **Sesión**: `engine_session_id` ya se persiste por run; la reanudación usa
  `session/load` (nunca `resume`, V6) con `cwd` = worktree del run.

### Alternativas evaluadas

| Alternativa | Veredicto | Motivo |
|---|---|---|
| `hermes profile create -p <n>` | Rechazada como default | V2: registra gateway/multiplexor; puede ofrecerse opt-in futuro |
| Compartir un perfil entre 2 ACP | Rechazada | V4: locks asumen un escritor |
| Usuario Unix por perfil | Diferida | root + propiedad de worktrees; desproporcionado |
| bwrap/landlock | Estudio separado | mejora real pero dependencia/complejidad; post-P2-S2 |
| HOME sanitizado | **Recomendado con validación** | minimiza descubrimiento ambiental (G3); riesgo de bootstrap en P2-S0 |

## 4. Milestones y grafo de dependencias

```
P2-S0 (factibilidad de auth real, 2 perfiles)
  └─► P2-S1 (ciclo de vida de perfiles gestionados)
        └─► P2-S2 (sesiones ACP autenticadas concurrentes, N=2)
              └─► P2-S3 (fiabilidad adversarial) ──► P2-S4 (presupuestos + auditoría final)
P2-S3a (fixes acotados S3-panic + wording de integración)  ← independiente, paralelizable tras P2-S0
```

Cada milestone incluye: precondiciones · alcance · invariantes de seguridad ·
tests automatizados · evidencia real-Hermes requerida · presupuestos ·
PASS/FAIL · rollback · auditoría independiente.

### P2-S0 — Gate de factibilidad: dos perfiles autenticados reales

- **Precondiciones**: usuario presente para OAuth/API keys; ~1 GB libres;
  Hermes 0.21.5 instalado; perfiles desechables fuera del repo (scratch).
- **Alcance**: runbook + recolector de evidencia (solo comandos read-only sobre
  el perfil: `auth status`, `auth list` con **etiquetas, jamás valores**);
  **cero código de producto nuevo** (se permite un script de evidencia en
  `scripts/`); cero copia/impresión de credenciales.
- **Experimentos**: (E1) crear 2 perfiles; (E2) autenticar cada uno
  interactivamente con el proveedor elegido **dentro del perfil** (ambient
  `gh` NO cuenta como evidencia); (E3) `session/new` + un prompt por perfil con
  edición canary en worktrees distintos; (E4) persistencia (`sessions list`
  por perfil); (E5) kill aislado entre procesos; (E6) **HOME sanitizado**:
  ¿arranca `hermes` sin el HOME del host? (decisión G3); (E7) medir frío/warm,
  RSS y disco por perfil.
- **Evidencia**: informe `docs/phase2-s0-hermes-auth-gate.md` con salidas
  redactadas (sin tokens), timings y decisión E6.
- **PASS**: dos perfiles completan un turno real cada uno, con sesiones y
  estado separados, sin copia/impresión de credenciales y sin interferencia.
  **FAIL**: el proveedor no permite grants independientes, o el bootstrap
  requiere HOME del host (⇒ G3 limitado a env + declaración), o se observa
  interferencia de procesos/locks.
- **Rollback**: eliminar perfiles scratch; no toca `main` ni datos del usuario.
- **Auditoría**: revisión independiente de la evidencia (obligatoria).

### P2-S1 — Ciclo de vida de perfiles gestionados

- **Alcance**: `agent_id → perfil` (§3), creación/reuso/limpieza explícita
  (`aeril profile create|list|status|rm`, nombres a definir), permisos 0700,
  pre-warm de dependencias (primer `hermes acp`/`initialize` con progreso y
  cap de tiempo), lock de escritor único, `auth status` por perfil con manejo
  de fallos (fail-closed, sin fallback a otros perfiles), y **cero
  migraciones de esquema si es evitable** (metadatos en `profile.json`; si hace
  falta DB, migración mínima justificada en su propio gate).
- **Invariantes**: un escritor por perfil; nunca leer/copiar credenciales;
  limpieza solo con acción explícita (patrón sentinel/`--unsafe-ok` de S4-R1).
- **Tests**: unitarios de mapeo/perfiles + integración con perfiles simulados
  (sin auth) + evidencia real de create/reuse/pre-warm con 1 perfil.
- **Presupuestos**: cold materialization ≤120 s (justificado en §5); warm ≤10 s.
- **PASS**: crear/reusar/limpiar perfiles con permisos y locks correctos;
  fallo de auth honesto y visible.
- **Rollback**: perfiles desechables; feature flag de configuración.

### P2-S2 — Sesiones ACP autenticadas concurrentes (N=2)

- **Alcance**: transición del gate TUI (§3), asignación de perfil por run,
  spawn con `HERMES_HOME`+env sanitizado, y suite de integración real que
  espeja S5 G1–G9 con Hermes autenticado (sin duplicar la arquitectura de
  tests: nuevo `tests/phase2_hermes.rs` con opt-in por env, p. ej.
  `AERIL_HERMES_E2E=1`, para no romper CI sin credenciales).
- **Tests requeridos (real)**: sesiones simultáneas; prompts con canary en
  workspaces distintos; persistencia por perfil; cancelación de una sesión con
  la otra viva; crash de un proceso Hermes (SIGKILL) con reconciliación S0 y
  continuación del otro; separación cross-perfil (session id de A no visible
  en B); N=2 intacto y single-Hermes default con 1 perfil.
- **Invariantes**: G1–G4; leases/checkpoints/fences de Fase 0/1 sin cambios.
- **Presupuestos**: §5 (por proceso y agregado).
- **PASS**: matriz §7 RH-1..RH-10 en verde con evidencia real.
- **Rollback**: flag de configuración revierte a single-Hermes; sin cambios de
  esquema destructivos.
- **Auditoría**: independiente (seguridad + concurrencia).

### P2-S3 — Fiabilidad adversarial

- **Alcance**: pruebas adversarias sobre ambos perfiles: crash de uno con el
  otro activo; cancelación; reinicio de backend y recuperación de grupo de
  procesos; expiración/refresh de auth (perfil desechable con `auth.json`
  corrupto/ilegible); estado de perfil corrupto (`state.db` truncado en copia
  desechable); contención de recursos (V7, disco, CPU); acceso cross-perfil no
  autorizado; ausencia de checkpoints duplicados o completitud falsa;
  identidad de proceso incierta (semántica S0-R1 preservada).
- **PASS**: cada escenario falla honestamente, sin crossover ni señales a
  procesos ajenos; evidencia por escenario.
- **Rollback**: n/a (tests); cualquier corrección entra por fix acotado.

### P2-S3a — Fixes acotados independientes (paralelizable)

- **Alcance mínimo**: (a) panic-hook de S3 (evitar que un panic capturado
  degrade el teclado/terminal; repro manual + fix dirigido + regresión);
  (b) wording/liveness de reconciliación de integración (no rotular
  "abandonado" un intento en vuelo sin evidencia).
- **Explícito**: no se combina con el primer spike de sesiones autenticadas;
  rama y gate propios.

### P2-S4 — Performance real y aceptación final

- **Alcance**: benchmarks reproducibles con Hermes real (método declarado,
  warm/cold, 1 y 2 procesos, agregado, disco, latencia de cancelación y
  recuperación, contención V7), con **separación app vs backend**; auditoría
  final independiente de seguridad y fiabilidad; actualización de
  `docs/phase2-status.md`.
- **Regla**: las mediciones mock NO sustituyen a Hermes real.

## 5. Presupuestos propuestos (justificados, provisionales hasta P2-S4)

Método: medición externa con beacons (arranque) y VmRSS por proceso (memoria);
host declarado; p95 sobre ≥20 muestras; **ningún umbral se fija por "lo
observado pasa"**, sino por razonamiento + margen explícito.

**App (Aeril) — contratos Fase 0/1 preservados:**

| Métrica | Límite | Justificación |
|---|---|---|
| Arranque p95 | **≤300 ms** | contrato vigente (MANIFIESTO) |
| RSS idle | **≤50 MB** | contrato vigente |
| Binario stripped | **≤30 MB** | contrato vigente |
| Caché por thread | **≤10 MiB** | contrato vigente |
| Overhead de supervisión con 2 sesiones reales | **≤15 MB sobre idle** | la supervisión es I/O+hilos+SQLite (mock N=5 midió ~10 MB de app); margen para locks/flock y buffers ACP |

**Backend Hermes (por proceso; medición real, no inferida de mocks):**

| Métrica | Límite propuesto | Justificación |
|---|---|---|
| RSS idle por `hermes acp` | **≤192 MiB** | observado ~132 MiB; +45% de headroom por drift de versión/deps y estado de sesión cargado |
| RSS pico por turno estándar | **≤320 MiB** | idle + historial/deltas/reasoning en memoria; validar en P2-S4 (provisional) |
| Agregado (Aeril + 2 Hermes pico) | **≤768 MiB** | 50 (app) + 2×320 (pico) + ~10% de coordinación |
| Disco por perfil | **≤512 MiB** | observado ~392 MB (deps+installs+caches); headroom para updates/logs; política de limpieza de logs/caches declarada |
| Cold materialization | **≤120 s** | observado ~39 s bajo carga; 3× por variabilidad de red en installs; UI con progreso obligatoria |
| Warm init | **≤10 s** | observado 3.3–3.9 s; ~2.5× de headroom |
| Cancelación cooperativa (ack) | **≤2 s p95** | un `session/cancel` + respuesta; medido en P2-S4 |
| Cancelación no cooperativa | **≤10 s (plazo fijo)** | contrato existente `AERIL_CANCEL_GRACE_SECS` (default 10) |
| Recuperación tras SIGKILL de un supervisor | **≤5 s** con `recover` | reconciliación S0 ya probada; medir con Hermes real |
| Contención 2 sesiones (turno) | **≤1.5× la latencia de 1 sesión** | V7: 4 slots de executor compartidos *dentro* de cada proceso; con 2 procesos separados la contención esperada es de recursos de máquina, no de cola |

Regla cultural vigente: subir un presupuesto exige justificación escrita y
aprobación explícita.

## 6. Matriz de aceptación real-Hermes (P2-S2/P2-S4)

| ID | Escenario | Evidencia | PASS |
|---|---|---|---|
| RH-1 | 2 perfiles autentican independientemente | `auth status` por perfil (sin valores) | ambos OK, sin credenciales compartidas |
| RH-2 | 2 sesiones ACP concurrentes | 2 procesos `hermes acp`, `HERMES_HOME` distintos | session/new OK en ambos |
| RH-3 | Prompts reales con ediciones canary | diffs en worktrees distintos + checkpoints | 2 checkpoints listos, sin cruces |
| RH-4 | Persistencia por perfil | `sessions list` por perfil | cada sesión en su store |
| RH-5 | Restauración (protocolo §9) | niveles L1–L4 | L4 (canary recall) o `verified=false` honesto |
| RH-6 | Cancelación aislada | cancelar A; B completa | A `cancelled` sin checkpoint; B intacto |
| RH-7 | Crash aislado | SIGKILL al Hermes de A | B continúa; A reconciliado una vez |
| RH-8 | Acceso cross-perfil denegado | `session/load` del id de A con HERMES_HOME de B | no recupera historial de A (fallo o vacío) |
| RH-9 | Fallo de auth honesto | perfil con auth corrupta/vencida | run falla cerrado, sin fallback |
| RH-10 | Presupuestos §5 | mediciones reales | todos los límites en verde |

## 7. Prerrequisitos explícitos

- **Recursos**: ~1 GB libres para 2 perfiles; CPU/RAM de la laptop objetivo
  (i7-8550U/16 GB); red para materializar deps.
- **Autenticación**: usuario presente para onboarding interactivo por perfil;
  proveedor que permita grants independientes (a validar en P2-S0);
  prohibición de reutilizar el perfil del host o `--clone` como fuente de
  credenciales.
- **Compatibilidad**: Hermes `0.21.5+8877.g865ba90` (V1–V9); cualquier upgrade
  re-abre las suposiciones versionadas.
- **Operación**: modo CI sin credenciales debe seguir verde (tests real-Hermes
  opt-in por env).

## 8. Riesgos y rollback

| Riesgo | Mitigación | Rollback |
|---|---|---|
| Grants OAuth no independientes por perfil | P2-S0 decide; proveedor alternativo o API keys | conservar single-Hermes |
| HOME sanitizado rompe bootstrap de hermes | validar en P2-S0 (E6) | sanitización de env + limitación declarada |
| Interferencia/locks compartidos | un escritor por perfil + locks propios | abortar P2-S2; single-Hermes |
| Credenciales ambientales visibles | G3 + declaración N1/N3 | documentar; sandbox como estudio |
| Crecimiento de memoria de Hermes | presupuesto con headroom + medición S4 | límite de sesiones a 1 |
| Drift de versión de Hermes | tabla V1–V9 + re-validación | pinear compatibilidad; NO-GO si rompe V1–V4 |
| Fuga de credenciales en evidencia/logs | política G1 + revisión de artefactos | purga de evidencia; auditoría |

## 9. Hipótesis de restauración de sesión (`session/load`)

Niveles observables (solo L4 cuenta como restauración de contexto):

- **L1 — Request aceptado**: `session/load` responde OK.
- **L2 — Identidad**: la sesión existe en `sessions list` del perfil.
- **L3 — Historial**: se observa el replay (chunks `user_message_chunk` /
  `agent_message_chunk` / `tool_call`) ANTES de la respuesta (V5).
- **L4 — Contexto disponible**: en un prompt NUEVO tras el load, el agente
  responde correctamente sobre un **canary no sensible**.

**Protocolo canary (determinista, no sensible):** en el turno 1, pedir al
agente: *"Memorizá este número para mi próxima pregunta: 7391. Respondé OK."*;
matar el proceso (no la sesión); relanzar; `session/load` con el `sessionId` y
el `cwd` del run; esperar el replay; prompt nuevo: *"¿Qué número te pedí
memorizar?"* → PASS si la respuesta contiene `7391`. Variante negativa: el
mismo prompt sobre una sesión desconocida (V5) debe NO responder 7391 (control
de falso positivo).

**Si L4 falla**: `verified=false` se mantiene; comportamiento de producto
honesto ("sesión reanudada; contexto no verificado") y **la restauración no
bloquea** el resto de Fase 2.

## 10. Definition of Done (Fase 2)

1. P2-S0..S4 con PASS independiente; P2-S3a con regresión verde.
2. RH-1..RH-10 con evidencia real archivada (redactada).
3. Presupuestos §5 medidos con método reproducible y en verde (o re-aprobados
   por escrito con justificación).
4. Garantías G1–G4 verificadas; N1–N4 declaradas en docs.
5. Cero copia/impresión de credenciales en código, evidencia o logs (auditado).
6. `session/load`: L4 verificado **o** `verified=false` honesto documentado.
7. Fase 0/1 sin regresiones: suite completa + fmt + Clippy + `make budget*`.
8. `docs/phase2-status.md` con límites y certificación explícita.

## 11. Exclusiones de alcance

Sandbox OS obligatorio, usuarios Unix separados, GUI de onboarding,
`hermes profile create`/gateway/multiplexor, >2 agentes, upgrade de Hermes,
copiar/importar credenciales, claims de contexto sin L4, GC automático de
perfiles, y cualquier cambio a `MANIFIESTO.md` o a los contratos de Fase 0/1.

## 12. Primer paso autorizado recomendado

**P2-S0** (gate de factibilidad), con la salvedad de que es un experimento
acotado: perfiles desechables, onboarding interactivo, evidencia redactada,
decisión E6 (HOME sanitizado) y veredicto GO/NO-GO para la concurrencia real.

## 13. Recomendación GO/NO-GO

**GO condicional para P2-S0.** El gate es barato, no toca `main`, no introduce
credenciales en el repo y responde la pregunta que decide toda la Fase 2. El
**NO-GO** para concurrencia real queda condicionado a: (a) imposibilidad de
grants independientes por perfil, (b) interferencia de procesos/locks con dos
perfiles, o (c) crossover de credenciales no mitigable — en cuyo caso se
conserva single-Hermes y se recomienda el enfoque revisado (§2C).

## 14. Suposiciones no resueltas (a resolver con evidencia, no por diseño)

1. **Independencia de grants por perfil** (P2-S0/E2): que el proveedor permita
   dos autorizaciones independientes sin compartir tokens.
2. **HOME sanitizado y bootstrap de hermes** (P2-S0/E6): si `hermes` arranca
   sin el HOME del host; de esto depende G3 completo o la versión limitada.
3. **Restauración de contexto L4** (§9): si el canary recall funciona; de lo
   contrario `verified=false` se mantiene y la Fase 2 continúa sin ese claim.
4. **Crecimiento real de memoria por turno**: el pico de 320 MiB es
   provisional hasta P2-S4.
5. **Contención real entre dos PROCESOS** (V7 aplica dentro de un proceso; con
   dos procesos la expectativa es contención de máquina, a medir).
6. **Drift de versión de Hermes**: V1–V9 valen para `0.21.5+8877.g865ba90`;
   un upgrade re-abre el análisis antes de continuar.

---

*SHIP FAST, SHIP SECURE.*
