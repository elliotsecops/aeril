# P2-S0 — Gate de factibilidad: Hermes autenticado (spike)

> Estado: **CONDITIONAL — bloqueado en E2 (autenticación interactiva del
> usuario).** Rama `spike/phase2-s0-hermes-auth` (desde `main @ 47f0318`).
> Plan: `docs/phase2-plan @ 367c6ce`. Este documento NO declara Fase 2
> habilitada ni GO para P2-S1. Evidencia en `docs/phase2-s0-evidence/`.

## 1. Versión exacta de Hermes

- `hermes --version` → `Hermes Agent v0.21.5+8877.g865ba90 (2026.9.24) · upstream 865ba906`
- `hermes acp --version` → `0.21.5+8877.g865ba90`
- **Idéntica a S1 y al `acp-hermes-spec`**: V1–V9 del plan siguen aplicando.
  Confirmadas en vivo durante el spike: V1 (`HERMES_HOME` honrado), V2 (no se
  usó `hermes profile create`), V4 (locks por perfil observados), V5 (quirks de
  load/resume documentados en el spec; L1–L4 pendientes de auth).

## 2. Entorno y seguridad de la ejecución

| Ítem | Valor |
|---|---|
| Scratch | `/home/elliot/p2s0-spike` (ext4, `/dev/nvme0n1p5`, `owner=elliot:elliot`, `mode=700`) |
| Espacio libre | 20 GB (perfiles ≈ 0.8 GB ⇒ margen amplio) |
| Perfiles | `p-A`, `p-B` (descartables; sin `hermes profile create`) |
| Repos | `repo-A`, `repo-B` (descartables, `git init` propios) |
| Perfil del usuario | `~/.hermes` **no modificado por el spike** (strace: 0 escrituras de nuestros procesos; 11 303 accesos de solo lectura a código/pycache). Hay un **gateway del usuario corriendo** (PID 2451) cuya actividad propia cambia mtimes; la evidencia autoritativa es el strace por-proceso |
| Credenciales | Nunca leídas/impresas/copiadas; sin `auth.json` en la evidencia |

## 3. E1 — Perfiles independientes

✅ Dos `HERMES_HOME` separados con estado propio. Evidencia:
- `state.db` (+`-wal/-shm`), `sessions/`, `logs/`, `cache/`, `installs/`,
  `auth.json` por perfil; locks por perfil observados **en vivo**:
  `auth.lock`, `.hermes-update-in-progress.lock`, `state.db.fts_rebuild.lock`,
  `state.db.quarantine.lock` (`e1-locks-live.txt`).
- `sessions list` por perfil → "No sessions found." (coherente con V5/§6.7:
  `session/new` de sesión vacía no persiste).
- Disco: **407 MiB en disco por perfil** (`du`; tamaño aparente 1.29 GB antes
  de deduplicar hardlinks) — comparable a los ~392 MB de S1.
- Procesos: handles disjuntos por perfil; sin strays al final del spike.

## 4. E2 — Autenticación interactiva — **PENDIENTE (usuario)**

⚠️ **No ejecutada**: requiere participación humana en el flujo del proveedor.
**No se usaron** credenciales del host ni `gh`; **no** se copió `auth.json`.

**Runbook exacto (a ejecutar por el usuario):**

```bash
# Preferido: entorno sanitizado (el mismo que usará el runtime)
S=/home/elliot/p2s0-spike
env -i PATH=/usr/bin:/bin LANG=C.UTF-8 HOME=$S/san-home HERMES_HOME=$S/p-A \
    /home/elliot/.local/bin/hermes auth add     # elegir proveedor y completar
env -i PATH=/usr/bin:/bin LANG=C.UTF-8 HOME=$S/san-home HERMES_HOME=$S/p-B \
    /home/elliot/.local/bin/hermes auth add

# Verificación SIN exponer valores (solo etiquetas/estado):
env -i PATH=/usr/bin:/bin LANG=C.UTF-8 HOME=$S/san-home HERMES_HOME=$S/p-A \
    /home/elliot/.local/bin/hermes auth list
# idem p-B

# Modelo por perfil si hace falta:
env -i ... HERMES_HOME=$S/p-A /home/elliot/.local/bin/hermes model
```

Si el onboarding sanitizado falla por entorno (p. ej. OAuth de navegador),
reintentar con el entorno normal **solo para el onboarding** y documentarlo
como desviación explícita (el runtime seguiría sanitizado). **Nunca** como
fallback silencioso.

**Criterio**: dos grants **independientes** por perfil; si el proveedor no lo
permite → **NO-GO documentado** (plan §4, P2-S0).

## 5. E3–E5 — Operaciones ACP reales

- **E5 ✅ (sin auth, precondición de procesos)**: dos `hermes acp` **coexisten
  simultáneamente** (PIDs 509310/509311, env sanitizado), `initialize` en ambos;
  `kill -9` a A ⇒ **B sigue vivo y operativo**, y un **proceso nuevo sobre p-B
  responde `initialize` en 1.69 s** mientras B sigue en hold. Terminaciones:
  A `signal 9` (externa), B `signal 15` (limpia). Sin strays.
- **E3/E4 — PENDIENTES (auth)**: `session/new` real, prompts canary en
  `repo-A`/`repo-B`, persistencia por perfil. El harness (`--session-new`,
  `--prompt`, `--load`) está listo.
- **TUI**: no se habilitó N=2 Hermes (restricción respetada).

## 6. E6 — Entorno sanitizado (HOME sanitizado)

✅ **Bootstrap positivo (pre-auth)**:
- `--env-mode sanitized` (env mínimo + `HOME=$S/san-home` + `HERMES_HOME`
  explícito): `initialize` **1 754 ms**, `authMethods=[hermes-setup]` (sin
  descubrimiento ambiental), `session/new` falla **honestamente**
  (`"Hermes is not connected to any AI provider yet. Run \`hermes model\`…"`).
- `--env-mode nohome` (sin HOME): idéntico (1 710 ms, `[hermes-setup]`).
- El launcher de Hermes usa rutas absolutas (no depende del HOME real para
  arrancar); el HOME sanitizado **corta el descubrimiento de credenciales
  ambientales** en runtime.
- **Caveat**: el **onboarding** bajo entorno sanitizado no se probó (es parte
  de E2). Si falla, aplica la regla del plan corregido: **fallo del gate de
  aislamiento**, sin fallback silencioso.

## 7. E7 — Mediciones reales

| Métrica (Hermes; Aeril NO participó en este spike) | p-A | p-B |
|---|---|---|
| Cold init (primera materialización) | ~46 s (observado en orquestación; sin JSON) | **26.7 s** |
| Warm init (sanitizado) | **1.75 s** | **1.68 s** |
| RSS idle (sanitizado, sin turno) | 124–191 MB* | 129–212 MB* |
| RSS pico durante un turno real | **PENDIENTE (auth)** | **PENDIENTE (auth)** |
| Disco por perfil (`du`) | 407 MiB | 407 MiB |
| Agregado idle (2 procesos) | ≈ 246–253 MB | — |
| Terminación | SIGTERM limpio ≤5 s; SIGKILL externo aislado | idem |

\* Rango según actividad: mínimo con `initialize` solamente; mayor cuando hubo
intento de `session/new`. Los números de S1 (~132 MB) quedan **por debajo** de
lo observado hoy (195/212 MB en env default con sesión gemini; 124–129 MB en
sanitizado sin sesión): **los presupuestos del plan (192/320 MB) deben
re-calibrarse en P2-S4 con turnos reales** — no se relajan ahora.

**Aeril vs Hermes**: el spike no levanta Aeril; los números de app siguen los
contratos de Fase 0/1 (`make budget`), medidos aparte.

## 8. Exposición de credenciales del host (hallazgo concreto)

- Con el **entorno heredado (default)**, AMBOS perfiles desechables
  descubrieron una credencial **`gemini` del host** (`authMethods=[gemini,
  hermes-setup]`) y `session/new` **tuvo éxito sin autenticación por perfil**.
  Es exactamente el crossover que el gate debe excluir: un spawn con env
  heredado **no** sirve para runs "autenticados por perfil".
- Fuentes ambientales presentes (solo nombres/existencia, jamás valores):
  variables `GEMINI_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_BASE_URL`;
  archivos `~/.gemini`, `~/.codex`, `~/.claude`, `~/.config/gh/hosts.yml`.
- Con **env sanitizado**, `authMethods=[hermes-setup]` y `session/new` falla
  honestamente: la minimización funciona para descubrimiento **ambiental
  accidental**; **no** es aislamiento (el plan corregido ya lo declara: código
  con los mismos permisos puede leer rutas absolutas del host).

## 9. L1–L4 (restauración) — no testeado

Requiere sesiones autenticadas. Protocolo listo en el harness (canary
impredecible por trial + controles negativos, plan §9). Si L4 no se demuestra:
`verified=false` se mantiene y no bloquea P2-S1.

## 10. Recomendación y blockers

**Recomendación para P2-S1: CONDITIONAL — PENDIENTE de E2.**

- Lo demostrado hoy: perfiles independientes (E1), coexistencia concurrente y
  kill aislado de procesos (E5), bootstrap sanitizado sin descubrimiento
  ambiental (E6, pre-auth), mediciones frío/warm/RSS/disco (E7 parcial).
- **Blocker único y preciso**: E2 — autenticación interactiva del usuario en
  ambos perfiles (dos grants independientes). Sin eso **no hay GO** para P2-S1
  y no se puede completar E3/E4/E7-pico/L1–L4.
- Si E2 falla (grants no independientes o onboarding incompatible con entorno
  sanitizado): **NO-GO documentado** para concurrencia real, según el plan.
- **Auditoría independiente requerida**: revisión de esta evidencia, del
  harness (`scripts/phase2/s0_probe.py`) y de las salidas de E2 cuando existan.

## 11. Manifest de limpieza (NO ejecutado — requiere aprobación del usuario)

```bash
# Perfiles y repos descartables (NO borrar sin aprobación si E2 ya se ejecutó:
# podrían contener credenciales provisionadas por el usuario):
rm -rf /home/elliot/p2s0-spike/p-A      # 407 MiB
rm -rf /home/elliot/p2s0-spike/p-B      # 407 MiB
rm -rf /home/elliot/p2s0-spike/repo-A /home/elliot/p2s0-spike/repo-B
rm -rf /home/elliot/p2s0-spike          # total ≈ 822 MiB
# No hay cambios en ~/.hermes ni en el sistema fuera de este directorio.
```

## 12. Próximo paso

Usuario ejecuta E2 (runbook §4) → se reanuda el spike con E3/E4/E7-pico/L1–L4 →
informe actualizado → decisión GO/NO-GO para P2-S1 con auditoría independiente.
