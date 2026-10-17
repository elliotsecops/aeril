# P2-S1 — Ciclo de vida de perfiles Hermes gestionados (PLAN de implementación)

> Estado: **PLAN — sin autorización de implementación.** Baseline: `main @
> 47f0318`. Deriva de `docs/phase2-plan.md` §P2-S1 y de la evidencia P2-S0
> (`docs/phase2-s0-hermes-auth-gate.md`). No modifica contratos de Fase 0/1.

## 1. Objetivo

Introducir, **detrás de un feature flag**, perfiles Hermes gestionados por
Aeril: mapeo estable `agent_id → perfil`, ciclo de vida explícito
(create/list/status/remove), entorno sanitizado por perfil, escritor único,
verificación de auth sin leer credenciales, fail-closed y borrado seguro —
**manteniendo single-Hermes como default** y sin habilitar multi-Hermes en
producto hasta P2-S2.

## 2. Invariantes preservados

- Fencing de lease, checkpoints, cancelación y recuperación S0/RC2.1/S4-R1
  intactos (suite completa verde).
- `main` avanza solo por fast-forward certificado.
- Un **escritor ACP por `HERMES_HOME`** (los locks de Hermes lo asumen).
- Aeril **nunca** lee/copia/imprime credenciales; solo metadata de estado.
- Nada toca `~/.hermes` ni el perfil del usuario.

## 3. Arquitectura

```
<AERIL_DATA_DIR>/hermes/
  agents.json                 # mapeo agent_id → perfil (Aeril, atómico)
  <agent_id>/
    profile.json              # metadata Aeril (sin secretos)
    home/                     # HERMES_HOME (estado Hermes completo)
    aeril.lock                # flock de escritor único
```

- **`profile.json`** (0600): `agent_id`, `created_ms`, `hermes_version`,
  `provider`, `model`, `last_auth_check_ms`, `auth_status`
  (`logged_in|absent|unknown`), `warmed_ms`. **Nunca** tokens.
- **`agents.json`** (0600): `{ "<agent_id>": { "profile": "<agent_id>", "thread_ids": [...] } }`
  — mapeo estable por thread (asignación explícita; sin heurísticas).
- **Metadata FUERA de la DB (justificación)**: el ciclo de vida del perfil es
  filesystem+CLI; ponerlo en SQLite requeriría una migración v4 y acoplaría
  perfiles borrables a filas históricas. `agents.json` con escritura atómica
  (tmp+rename+fsync) preserva los invariantes requeridos (estabilidad,
  auditabilidad, borrado seguro) sin migración. **Si en P2-S2 el mapeo por
  thread demuestra necesitar integridad transaccional con runs, se propondrá
  una migración mínima en su propio gate.**
- Permisos: dirs `0700`, archivos propios `0600`; validación de paths por
  `realpath` (rechazo de symlinks, mountpoints internos, raíz/home, rutas
  fuera de `<AERIL_DATA_DIR>/hermes`).

## 4. Interfaces

| Interfaz | Contrato |
|---|---|
| `aeril profile create <agent_id> [--provider <p>] [--model <m>]` | crea `home/` + `profile.json` + entrada en `agents.json`; permisos; **sin** autenticar (imprime el runbook de `auth add`); pre-warm opcional `--warm` |
| `aeril profile list [--json]` | perfiles con `auth_status` (metadata-only) y uso por thread |
| `aeril profile status <agent_id>` | estado de auth (via CLI `hermes auth status <provider>` o `authMethods` de `initialize`), lock, pre-warm, disco |
| `aeril profile warm <agent_id>` | pre-warm acotado con progreso |
| `aeril profile remove <agent_id> --confirm` | borrado seguro (checks abajo); nunca automático |
| `aeril profile assign <agent_id> --thread <id>` | fija el mapeo thread→perfil |
| Feature flag | `AERIL_HERMES_PROFILES=1` (env) **o** `hermes_profiles = true` en config de datos; default OFF ⇒ comportamiento actual |
| TUI | flag OFF: gate single-Hermes actual. Flag ON: se permite un 2º run hermes **solo** con perfil distinto autenticado; con 1 perfil configurado el 2º sigue rechazado; N=2 intacto |

## 5. Seguridad

- **Spawn**: `HERMES_HOME=<perfil>/home` + env sanitizado (PATH mínimo,
  `HOME=<perfil>/san-home` con `XDG_*` y `GH_CONFIG_DIR` propios) — reutiliza
  el modelo validado en P2-S0; **jamás** el entorno heredado para operaciones
  autenticadas.
- **Escritor único**: `flock(LOCK_EX|LOCK_NB)` sobre `aeril.lock` tomado por el
  supervisor del run mientras vive la sesión ACP; segundo spawn ⇒ error
  accionable (fail-fast), sin cola.
- **Auth-status sin credenciales**: solo `hermes auth status <provider>` (salida
  de estado) y/o `authMethods` del `initialize`; **prohibido** abrir
  `auth.json`.
- **Fail-closed**: sin auth verificada ⇒ el run hermes se rechaza con mensaje
  accionable; **sin fallback** a otro perfil, a credenciales ambientales ni al
  perfil del host.
- **Sin cross-profile**: un run usa exactamente su perfil; el id de sesión de
  otro perfil no se reutiliza.

## 6. Pre-warm acotado

`profile warm`: primer `hermes acp` + `initialize` con timeout duro
(≤120 s, presupuesto del plan) y progreso por etapas (spawn → initialize →
listo); registra `warmed_ms`; si excede, aborta con estado `unknown` y no
bloquea el perfil. Sin descargas nuevas fuera de `HERMES_HOME`.

## 7. Ciclo de vida y borrado seguro

`profile remove` exige: `--confirm`; lock no tomado; ningún run activo del
perfil; `profile.json` con `agent_id` coincidente (propiedad); `realpath`
literal dentro de `<AERIL_DATA_DIR>/hermes`; sin symlinks/mountpoints; sin
handles abiertos. Borra **solo** `<agent_id>/` (home incluido) y actualiza
`agents.json`. **Nunca** borra perfiles automáticamente ni toca `~/.hermes`.
Rollback de la feature: flag OFF ⇒ comportamiento previo; los perfiles quedan
en disco sin uso.

## 8. Migración

**Ninguna propuesta.** Sin cambios de esquema; sin dependencias nuevas. Si
P2-S2 requiere integridad transaccional thread↔perfil, se propondrá una
migración mínima y auditada aparte.

## 9. Tests y matriz de auditoría

**Unitarios**: validación de paths (symlink/mountpoint/raíz/home), permisos
0700/0600, `agents.json` atómico, parser de `auth status`, lock (doble
escritor), flag OFF no altera nada.

**Integración** (`tests/phase2_s1_profiles.rs`, shim `hermes` por PATH —
determinista, sin credenciales reales): create/list/status/remove; permisos;
doble escritor rechazado; fail-closed sin auth; sin fallback; pre-warm acotado
con timeout; borrado seguro (perfil ajeno/incompleto/symlink rechazado); flag
OFF ⇒ TUI rechaza el 2º hermes (gate actual) y flag ON con 2 perfiles
autenticados (shim) ⇒ permite 2 con perfiles distintos; N=2 y single-Hermes
default intactos; regresión completa Fase 0/1 (leases/fences/checkpoints/
cancelación) verde.

**Matriz de auditoría independiente**: (1) flag OFF = comportamiento idéntico
al certificado; (2) evidencia de entorno por perfil sin cruces; (3) ningún
acceso a `auth.json` (grep + strace); (4) lock y fail-closed demostrados;
(5) borrado seguro con evidencia; (6) sin migraciones; (7) presupuestos Fase 0/1
sin regresión (`make budget`); (8) evidencia real con los perfiles P2-S0
(create/list/status/assign) sin re-autenticar.

## 10. Downstream blockers (precondiciones de P2-S2)

1. **Clasificación de fallos de proveedor**: implementar detección de
   `failed_turn` (boundary/display_kind/ausencia de usage) y mapearla a run
   `failed`; **prohibido** `failed_turn → completed` (test de aceptación ya
   definido en `docs/phase2-s0-hermes-auth-gate.md` §16/F3).
2. **Política de modelo explícita**: selección por perfil documentada
   (free/créditos); sin fallback implícito a modelos pagos.
3. **Budget idle**: resolver por optimización o **re-baseline aprobado**
   (propuesto ≤288 MiB; hoy VIOLATION 213.7/258.8).
4. **Invariantes**: fencing/leases/checkpoints/cancelación con regresión verde.
5. P2-S3 y P2-S4 conservan su alcance del plan.

## 11. Rama y alcance de archivos propuesto

- **Rama**: `feat/phase2-s1-profiles` (desde `main @ 47f0318`).
- **Archivos**: `src/profile.rs` (nuevo: layout, validación, lock, metadata,
  auth-status, ciclo de vida), `src/cli.rs` (subcomando `profile` + flag),
  `src/agent/mod.rs` (spawn hermes con `HERMES_HOME`+env sanitizado),
  `src/session.rs` (asignación de perfil por thread, gated), `src/tui/mod.rs`
  (gate condicionado al flag), `tests/phase2_s1_profiles.rs` (nuevo),
  `docs/phase2-s1-status.md` (al implementar). **Sin** migraciones, sin deps
  nuevas, sin cambios a `MANIFIESTO.md`.

## 12. Criterios de aceptación independiente (borrador)

- Todos los tests del §9 verdes + suite completa (124+ nuevos) + fmt/clippy +
  `make budget*` sin regresión.
- Flag OFF byte-equivalente en comportamiento al certificado (gate actual).
- Evidencia con perfiles reales P2-S0: create/list/status/assign/warm sin
  re-autenticar y sin tocar `~/.hermes`; doble escritor y fail-closed
  demostrados.
- Sin migración de esquema; sin nuevas dependencias; `main` intacto hasta la
  integración certificada.

## 13. Autorización solicitada

Se solicita **autorización explícita para implementar P2-S1** en
`feat/phase2-s1-profiles` con el alcance del §11, sujeto a aceptación
independiente. Hasta entonces: sin desarrollo, sin merge, sin tag, sin
multi-Hermes en producto.
