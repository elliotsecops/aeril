# P2-S0 — Gate de factibilidad: Hermes autenticado (spike, Security R1)

> Estado: **CONDITIONAL — bloqueado en E2 (autenticación interactiva del
> usuario), con harness endurecido (R1 + R2).** Rama
> `spike/phase2-s0-hermes-auth`. Plan: `docs/phase2-plan @ 367c6ce`.
> Este documento NO declara Fase 2 habilitada ni GO para P2-S1.
> Evidencia R1/R2 en `docs/phase2-s0-evidence/` (redactada, sin credenciales).

## 0. Cambios de Security R1 (este corte)

1. **Permisos fail-closed**: el harness **deniega** todo pedido del agente por
   defecto; nunca elige una opción `allow*`. `--permissions approve-edits`
   (decisión explícita del usuario) solo aprueba pedidos `kind == "edit"`;
   execute/shell/desconocido se deniegan. Sin opción de deny ⇒ respuesta
   `cancelled` (formato válido del protocolo).
2. **HOME sanitizado por perfil**: `san-home-A` y `san-home-B` (0700, sin
   archivos, sin credenciales), con `XDG_*` apuntando al HOME sanitizado.
3. **Default sanitizado**: `--env-mode` por defecto es `sanitized`; el modo
   heredado (`default`) está **prohibido** para `session/new`/`prompt`/`load`
   (exit 3) salvo `--allow-inherited-auth-ops` explícito y **nunca cuenta como
   evidencia de auth**. Variables no-secretas permitidas por lista blanca
   (`--env-allow`: DISPLAY, WAYLAND_DISPLAY, XDG_RUNTIME_DIR, BROWSER,
   SSH_AUTH_SOCK); jamás se copia el entorno completo.
4. **Un solo escritor por perfil**: la responsividad tras el kill se verifica
   con un segundo `initialize` **sobre la misma conexión** (`--ping-after`);
   la reutilización de un perfil se prueba **solo después** de terminar el
   writer original (secuencial).
5. **Evidencia redactada**: sin texto de mensajes/razonamiento del agente ni
   títulos de tool calls; solo tipos de evento, estados, timings, decisiones de
   permisos y errores truncados. El canary se verifica en memoria y solo se
   persiste el booleano.
6. **Tests de seguridad**: `scripts/phase2/test_s0_probe.py` → **24/24**
   (deny-by-default, execute nunca aprobado, cancel sin deny, guard de modo
   heredado, HOMEs distintos 0700, env sin variables de auth del host,
   `--env-allow` restringido, CLI rechaza default) + R2 (abajo).

## 0bis. Cambios de Security R2 (este corte)

1. **Permisos con ALCANCE verificado**: `approve-edits` fue reemplazado por
   `approve-verified-edits` — solo aprueba `kind == "edit"` cuando TODAS las
   rutas (`content[{type:"diff",path}]` y `locations[{path}]`) resuelven
   (realpath, siguiendo symlinks) **dentro del repo descartable**. Sin rutas
   verificables, ruta absoluta, `../`, symlink que escapa, kind distinto o
   rutas mixtas con una fuera ⇒ **deny**. Nuevo modo **`ask-human`**
   (preferido para E3): además de la verificación, exige aprobación humana
   explícita por pedido (`allow` por TTY); sin TTY ⇒ deny.
2. **SSH_AUTH_SOCK eliminado** de la allowlist de entorno (un socket de agente
   SSH es una capacidad de autenticación, no una variable inocua). Quedan solo
   display/browser: `DISPLAY`, `WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR`, `BROWSER`.
   Nota residual: `XDG_RUNTIME_DIR` expone sockets de runtime del usuario y
   solo se permite por pedido explícito del operador (necesario para Wayland).
3. **Auditoría de perfiles contaminados** (§8): realpaths literales, owner
   correcto, sin symlinks, sin procesos activos, sin mountpoints; el reset
   `rm -rf` elimina el estado (no se recrea encima).
4. **Revisión de evidencia histórica** (§13): 27 blobs de la rama escaneados
   contra patrones de credenciales ⇒ **0 valores encontrados**; solo la
   etiqueta no-secreta `gemini` en archivos históricos.
5. **Re-validación R2** (sin auth): E1/E5/E6 re-corridos con `authMethods=
   [hermes-setup]`, ping 18.1 ms en el writer original, reuso secuencial
   1.80 s, strace 0 escrituras a `~/.hermes`, sin strays, sin cambios de
   runtime de producto (`git diff main..HEAD` = solo scripts/phase2 + docs).

## 1. Versión exacta de Hermes

- `hermes --version` → `Hermes Agent v0.21.5+8877.g865ba90 (2026.9.24) · upstream 865ba906`
- `hermes acp --version` → `0.21.5+8877.g865ba90` — **idéntica a S1/spec**;
  V1–V9 del plan siguen aplicando (V1/V2/V4 confirmadas en vivo; V5 pendiente).

## 2. Entorno y seguridad de la ejecución

| Ítem | Valor |
|---|---|
| Scratch | `/home/elliot/p2s0-spike` (ext4, `owner=elliot:elliot`, `mode=700`) |
| Perfiles | `p-A`, `p-B` (descartables; sin `hermes profile create`) |
| HOMEs sanitizados | `san-home-A`, `san-home-B` (**0700**, 0 archivos, sin credenciales) |
| Repos | `repo-A`, `repo-B` (descartables) |
| Perfil del usuario | `~/.hermes` **no modificado por el spike**: strace R1 = **0 escrituras** de nuestros procesos (11 304 lecturas de código compartido). Hay un gateway del usuario corriendo (PID 2451); su actividad propia no es nuestra |
| Permisos del agente | **denegados por defecto** (sin prompts en E1/E5/E6: `permissions: []`) |

## 3. E1 — Perfiles independientes (R1)

✅ Dos `HERMES_HOME` + dos `HOME` sanitizados independientes. Evidencia R1:
- `state.db`(+wal/shm), `sessions/`, `logs/`, `cache/`, `installs/`,
  `auth.json` por perfil; locks por perfil: `auth.lock`,
  `.hermes-update-in-progress.lock`, `state.db.fts_rebuild.lock`,
  `state.db.quarantine.lock` (`e1-locks-*-r1.txt`).
- Handles **disjuntos**: A no abre rutas de p-B y viceversa; ambos solo
  comparten la lectura del intérprete Python (`~/.hermes/tools/.../python3.14`)
  — declarado, no un defecto (`e1-handles-*-r1.txt`).
- `authMethods=[hermes-setup]` en AMBOS (sin credenciales ambientales) —
  init 1.96 s / 2.05 s; RSS idle 126/128 MB; sin strays.
- Disco: **407 MiB en disco por perfil** (`du`).

## 4. E2 — Autenticación interactiva — **PENDIENTE (usuario)**

⚠️ **No ejecutada.** No se usaron credenciales del host ni `gh`; no se copió
`auth.json`.

⛔ **PASO 0 OBLIGATORIO (con aprobación del usuario): resetear los perfiles
contaminados** — ver §8: las corridas previas con env heredado persistieron
metadata de credenciales del host en `p-A/p-B/auth.json`. Antes de E2:

```bash
S=/home/elliot/p2s0-spike
# REQUIERE APROBACIÓN EXPLÍCITA DEL USUARIO (borra perfiles descartables):
rm -rf $S/p-A $S/p-B && python3 <repo>/scripts/phase2/s0_probe.py setup --scratch $S
```

**Runbook corregido (a ejecutar por el usuario, entorno sanitizado):**

```bash
S=/home/elliot/p2s0-spike
for P in A B; do
  H=$S/p-$P; SAN=$S/san-home-$P
  env -i PATH=/usr/bin:/bin LANG=C.UTF-8 HOME=$SAN \
      XDG_CONFIG_HOME=$SAN/.config XDG_CACHE_HOME=$SAN/.cache \
      XDG_DATA_HOME=$SAN/.local/share XDG_STATE_HOME=$SAN/.local/state \
      HERMES_HOME=$H /home/elliot/.local/bin/hermes auth add   # interactivo
  env -i PATH=/usr/bin:/bin LANG=C.UTF-8 HOME=$SAN HERMES_HOME=$H \
      /home/elliot/.local/bin/hermes auth list                 # solo etiquetas
done
```

Si el OAuth necesita variables de navegador, agregar **solo** las no-secretas
necesarias (DISPLAY/WAYLAND_DISPLAY/XDG_RUNTIME_DIR/BROWSER); nunca el entorno
completo. Si el proveedor no permite **dos grants independientes** ⇒
**NO-GO documentado**.

## 5. E5 — Coexistencia y kill aislado (corregido, un writer por perfil)

✅ R1 (sin segundo writer):
- A y B activos simultáneamente en perfiles distintos (PIDs 552900/552899).
- `kill -9` a A ⇒ **B (writer original) sigue vivo** y responde un segundo
  `initialize` **en la misma conexión**: `ping_ok=true`, **17.2 ms**.
- A termina `signal 9` (externa); B termina `signal 15` (limpia); sin strays.
- Reutilización de `p-B` **después** de terminar el writer original:
  `initialize` 1.64 s (secuencial, single-writer preservado).
- Evidencia: `probe-*-e5-r1.json`, `probe-B-post-r1.json`.

## 6. E6 — Entorno sanitizado (HOME por perfil)

✅ Bootstrap pre-auth con `san-home-A` (y sin HOME): `initialize` **1.68–1.81 s**,
`authMethods=[hermes-setup]`, `session/new` falla honestamente
(`-32603: "Hermes is not connected to any AI provider yet. Run \`hermes model\`…"`).
**Sin `gemini` en la evidencia sanitized** (verificado por grep en el JSON).
Caveat: el **onboarding** sanitizado se prueba en E2; si falla ⇒ fallo del gate
de aislamiento (sin fallback silencioso).

## 7. E7 — Mediciones reales (R1)

| Métrica (Hermes; Aeril no participó) | Valor |
|---|---|
| Cold init (primera materialización) | **26.7 s** (B, JSON) · ~46 s (A, cota) |
| Warm init (sanitizado) | **1.64–2.05 s** |
| RSS idle (sanitizado, sin turno) | **126–133 MB** |
| RSS pico durante un turno real | **PENDIENTE (auth)** |
| Disco por perfil (`du`) | **407 MiB** |
| Agregado idle (2 procesos) | ≈ **250–265 MB** |
| Terminación | SIGTERM limpia ≤5 s; SIGKILL externo aislado |

Los presupuestos del plan (192/320 MB) deben re-calibrarse en P2-S4 con turnos
reales; no se relajan ahora. **Mocks ≠ Hermes** y **Aeril ≠ Hermes**.

## 8. Exposición de credenciales del host (hallazgo endurecido)

- **Crossover concreto (corridas default-env previas)**: AMBOS perfiles
  descubrieron credenciales ambientales (`gemini` en `authMethods`) y
  `session/new` funcionó **sin auth por perfil**.
- **Contaminación persistida (nuevo, crítico)**: `p-A/p-B/auth.json` contienen
  un `credential_pool` con entradas de **`gemini`, `qwen-oauth`, `copilot`**
  (metadata: `label`, `source`, `base_url`, `expires_at_ms`,
  `secret_fingerprint`; **no se observaron secretos crudos en la estructura**).
  Mtimes: `p-A` 22:28:48 y `p-B` 22:30:52 — **atribuidas a las corridas con
  env heredado**, antes de cualquier corrida sanitizada. Implicación: el modo
  heredado no solo descubre credenciales ambientales: **las materializa en el
  perfil**. Por eso R1 lo prohíbe para operaciones autenticadas y los perfiles
  deben **resetearse** antes de E2 (§4).
- Fuentes ambientales presentes (solo nombres/existencia): `GEMINI_API_KEY`,
  `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_BASE_URL`; `~/.gemini`, `~/.codex`,
  `~/.claude`, `~/.config/gh/hosts.yml`.
- Con env sanitizado, `authMethods=[hermes-setup]` y `session/new` falla
  honestamente: la minimización funciona para descubrimiento **accidental**;
  **no** es aislamiento (código con los mismos permisos puede leer rutas
  absolutas del host; plan corregido §2C).

## 9. L1–L4 (restauración) — no testeado

Requiere sesiones autenticadas. Protocolo listo (canary impredecible por trial
+ controles negativos). Si L4 no se demuestra: `verified=false` y no bloquea
P2-S1.

## 10. Recomendación y blockers

**Recomendación para P2-S1: CONDITIONAL — PENDIENTE de E2.**

- Demostrado: perfiles/HOMEs independientes (E1), coexistencia con **un
  writer por perfil** y kill aislado (E5 corregido), bootstrap sanitizado sin
  descubrimiento ambiental (E6), mediciones frío/warm/RSS/disco (E7),
  permisos fail-closed (14/14 tests).
- **Blockers**: (1) E2 interactiva del usuario; (2) **reset de perfiles
  contaminados** (§4/§8) antes de E2; sin eso no hay GO para P2-S1.
- Si E2 falla (grants no independientes u onboarding incompatible con
  sanitizado): **NO-GO documentado**.
- **Auditoría independiente requerida** de esta evidencia, del harness R1 y de
  los tests de seguridad.

## 11. Manifest de limpieza (auditado; NO ejecutado — requiere aprobación)

Auditoría T3 (read-only): `realpath(p-A/p-B)` = rutas literales dentro de
`/home/elliot/p2s0-spike` (ext4, sin mountpoints internos), owner
`elliot:elliot`, mode 700, **no symlinks**; **sin procesos activos** con esos
perfiles (environ/lsof). El reset elimina el estado contaminado porque borra el
directorio completo (`rm -rf`), y `setup()` solo recrea directorios vacíos (no
copia nada previo).

```bash
S=/home/elliot/p2s0-spike
rm -rf "$S/p-A" "$S/p-B"                 # 407 MiB c/u (contaminados: credential_pool)
rm -rf "$S/repo-A" "$S/repo-B" "$S/san-home-A" "$S/san-home-B"
rm -rf "$S"                              # total ≈ 822 MiB (incluye logs/evidencia locales)

# Verificación POST-reset (debe fallar/no encontrar nada):
test ! -e "$S/p-A/auth.json" && echo "auth.json eliminado ✓"
grep -rl "credential_pool" "$S" 2>/dev/null | wc -l   # esperado: 0
# ~/.hermes, repos del usuario y el resto del sistema NO están en el manifest.
```

## 13. Revisión de evidencia histórica (T4)

Se escanearon **todos los blobs alguna vez commiteados en la rama**
(`git rev-list 47f0318..HEAD` → 27 blobs bajo `scripts/phase2/**` y
`docs/phase2-s0*`) contra patrones de credenciales (GitHub/OpenAI/Google/
Slack/AWS/JWT/GitLab/Bearer/hex largos/keywords). Resultado: **0 valores de
credenciales encontrados** — ninguna remediación de secretos requerida. Los
archivos eliminados en R1 (p. ej. `probe-B-e1.json`) siguen en la historia de
Git y contienen únicamente la **etiqueta de proveedor no-secreta** `gemini`
(identidad de proveedor, no un valor). No se reproduce ningún valor en este
informe.

## 12. Próximo paso

1. Usuario aprueba el **reset** de `p-A`/`p-B` (paso 0 de §4).
2. Usuario ejecuta E2 (runbook §4) y reporta etiquetas de `auth list`.
3. Se reanuda el spike: E3/E4 (prompts canary con `--permissions ask-human`
   — aprobación humana por edición — o `approve-verified-edits` acotado al
   repo), E7-pico, L1–L4 → informe final → GO/NO-GO con auditoría.
