# P2-S0 — Gate de factibilidad: Hermes autenticado (spike, Security R1)

> Estado: **CONDITIONAL — bloqueado en E2 (autenticación interactiva del
> usuario), con harness endurecido (R1 + R2 + R3).** Rama
> `spike/phase2-s0-hermes-auth`. Plan: `docs/phase2-plan @ 367c6ce`.
> Este documento NO declara Fase 2 habilitada ni GO para P2-S1.
> Evidencia R1/R2 en `docs/phase2-s0-evidence/` (redactada, sin credenciales).

## 0. Cambios de Security R1 (este corte)

1. **Permisos fail-closed**: el harness **deniega** todo pedido del agente por
   defecto; nunca elige una opción `allow*`. Sin opción de deny ⇒ respuesta
   `cancelled` (formato válido del protocolo). **Nota de supersesión**: la
   política `approve-edits` citada aquí quedó reemplazada por R2
   (`approve-verified-edits` / `ask-human`) y por R3 (rechazo de metadata
   multi-archivo no verificable); ver §0bis y §0ter.
2. **HOME sanitizado por perfil**: `san-home-A` y `san-home-B` (0700, sin
   archivos, sin credenciales), con `XDG_*` apuntando al HOME sanitizado.
3. **Default sanitizado**: `--env-mode` por defecto es `sanitized`; el modo
   heredado (`default`) está **prohibido** para `session/new`/`prompt`/`load`
   (exit 3) salvo `--allow-inherited-auth-ops` explícito y **nunca cuenta como
   evidencia de auth**. **Nota de supersesión (R2)**: la allowlist citada aquí
   incluía `SSH_AUTH_SOCK`; R2 lo eliminó (capacidad de autenticación). La
   allowlist vigente es DISPLAY, WAYLAND_DISPLAY, XDG_RUNTIME_DIR, BROWSER;
   jamás se copia el entorno completo.
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

## 0ter. Cambios de Security R3 (F1 — bypass de alcance multi-archivo V4A)

**F1 (raíz)**: el builder instalado de Hermes 0.21.5+8877.g865ba90
(`acp_adapter/edit_approval.py::_proposal_for_patch_v4a`) surfacea los edits
multi-archivo como un **string de display** `", ".join(paths)` en
`content[0].path`, **sin metadata por archivo y sin `locations[]`**. El harness
R2 hacía `realpath` sobre ese string unido: `"inrepo.txt, ../outside/victim.txt"`
resolvía dentro del repo (el `..` cancelaba el componente `"inrepo.txt, "`) y el
pedido — cuyo objetivo real incluía un archivo FUERA del repo — era aprobado
con `allow_once`. Bypass de alcance confirmado en rojo (4 FAIL, ver
`f1-red-green-r3.txt`).

**Corrección (mínima, harness-only)** en `_verify_edit_scope()`:
- se rechaza cualquier `path` que contenga el separador de join V4A `", "` y
  cualquier display de move/rename `" -> "`;
- **nunca** se corre `realpath` sobre un string de display unido ni se lo parte
  en paths "verificados" sin un contrato autoritativo por archivo;
- si **cualquier** ruta es no verificable, se rechaza el pedido **completo**,
  con motivo explícito y sin eco del display crudo;
- se preservan los chequeos single-file (absolutas, `../`, symlinks).

**Políticas**: para el payload F1, `approve-verified-edits` **deniega** (jamás
`allow_once`) y `ask-human` **deniega antes de preguntar** (el callback humano
NO se invoca; el display unido nunca se presenta como lista verificada).
`deny` (default) y el rechazo de execute/shell quedan intactos.

**Alcance**: esto es **verificación de metadata**, no un sandbox OS: un archivo
legítimo con esos delimitadores puede rechazarse conservativamente (aceptable
para el spike). `locations[]` **no** es emitido por el builder V4A observado en
esta versión; si una versión futura lo emite con paths por archivo, se
re-evaluará el contrato.

**Reset**: ver §11 — manifiesto mínimo aprobado por auditoría (solo `p-A`/`p-B`).

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

**Manifiesto MÍNIMO (aprobado por auditoría — R3)**: solo `p-A` y `p-B` pueden
eliminarse, tras **repetir** los chequeos de ownership/procesos/symlinks/
mountpoints y **solo con aprobación explícita del usuario**. Se preservan
`repo-A/B`, `san-home-A/B` y los diagnósticos locales salvo autorización
separada.

```bash
S=/home/elliot/p2s0-spike
# Chequeos renovados ANTES de borrar (deben pasar):
realpath "$S/p-A" "$S/p-B"                       # rutas literales esperadas
stat -c '%U:%G %a %F' "$S/p-A" "$S/p-B"          # elliot:elliot 700 directory
test ! -L "$S/p-A" && test ! -L "$S/p-B"         # no symlinks
findmnt -R "$S" -o TARGET --noheadings | wc -l   # 0 mountpoints internos
# Ningún proceso con HERMES_HOME apuntando a p-A/p-B (environ/lsof) — ver T3.
# Solo con aprobación explícita:
rm -rf "$S/p-A" "$S/p-B"
test ! -e "$S/p-A/auth.json" && echo "auth.json eliminado ✓"
grep -rl "credential_pool" "$S/p-A" "$S/p-B" 2>/dev/null | wc -l   # esperado: 0
# NO se borran repo-A/B, san-home-A/B ni el resto del scratch (manifest mínimo).
```

## 12. Próximo paso

1. Usuario aprueba el **reset** de `p-A`/`p-B` (paso 0 de §4).
2. Usuario ejecuta E2 (runbook §4) y reporta etiquetas de `auth list`.
3. Se reanuda el spike: E3/E4 (prompts canary con `--permissions ask-human`
   — aprobación humana por edición — o `approve-verified-edits` acotado al
   repo), E7-pico, L1–L4 → informe final → GO/NO-GO con auditoría.

## 14. Revisión E2 (credenciales) y decisión de seguridad

**E2 — resultado por perfil** (metadata-only; sin leer/comparar/hashear valores):
- p-A y p-B: credencial **nous** `device_code` (oauth), `auth status nous` =
  "logged in", `active_provider="nous"`, ids de registro distintos
  (`f87efb` / `7a9348`); flujos device-code interactivos separados en
  `HERMES_HOME`/`HOME` sanitizados distintos.
- **Independencia de grants**: *fuertemente indicada* (flujos y registros
  separados por perfil) pero **no demostrable criptográficamente con
  metadata** y, por política, no se comparan materiales de token. La
  verificación autoritativa por archivo no está disponible en este nivel.

**Hallazgo — `gh_cli` en ambos perfiles (contaminación confirmada)**:
- El registro `copilot` (`api_key`, `source=gh_cli`) está **persistido** en
  `credential_pool` de ambos `auth.json` (`updated_at` coincide con el auth
  interactivo del usuario).
- **Raíz**: `git_credentials.py::_gh_cli_token` / `github_api.py::_gh_cli_token`
  ejecutan el subproceso `gh auth token`; `hermes auth list` **sí lo invoca**
  (strace: `execve("/usr/bin/gh", ["gh","auth","token"])`). Bajo el env
  sanitizado, `gh auth status` responde "not logged in" (verificado) ⇒ la
  discovery está **bloqueada en runtime**; el registro persistido provino de un
  flujo de auth cuyo entorno permitió a `gh` ver el login del host (p. ej. HOME
  real en esa ejecución). No se puede reconstruir retroactivamente ese entorno.
- **Registros inesperados**: SÍ — `copilot` en ambos perfiles (derivado del host).

**¿Fallback inesperado a Copilot?** **No** en la configuración actual:
`providers={nous: enabled}` (copilot no habilitado), sin `config.yaml` ⇒
`get_fallback_chain()` vacío, `requested != "auto"` ⇒ un `AuthError` de nous
**no** cae a otro proveedor (`runtime_provider.py:1074-1081`), y el rung de
pool es del proveedor solicitado. Residual: si en el futuro se configura una
cadena de fallback o se cambia `active_provider`, el registro persistido podría
ser elegible ⇒ conviene removerlo.

**Decisión de seguridad**: el lanzamiento sanitizado es suficiente para
**prevenir discovery nueva** (verificado), pero **no** para la garantía de
"perfiles solo con credenciales propias" mientras el registro derivado del host
esté presente ⇒ configuración actual **insuficiente** para las garantías
declaradas de E2.

**Mitigación mínima propuesta**:
1. **Harness (hecho)**: `GH_CONFIG_DIR=<san-home>/.config/gh` en los modos
   sanitized/nohome — defensa en profundidad: aunque HOME se herede por error,
   `gh` no puede leer la config del host (verificado con HOME señuelo).
   32/32 tests.
2. **Perfiles (PENDIENTE — requiere aprobación del usuario; no ejecutado)**:
   remover el registro ajeno por perfil y re-verificar:
   ```bash
   S=/home/elliot/p2s0-spike
   for P in A B; do
     env -i PATH=/usr/bin:/bin LANG=C.UTF-8 HOME=$S/san-home-$P \
         XDG_CONFIG_HOME=$S/san-home-$P/.config GH_CONFIG_DIR=$S/san-home-$P/.config/gh \
         HERMES_HOME=$S/p-$P /home/elliot/.local/bin/hermes auth remove copilot 1
     env -i PATH=/usr/bin:/bin LANG=C.UTF-8 HOME=$S/san-home-$P \
         XDG_CONFIG_HOME=$S/san-home-$P/.config GH_CONFIG_DIR=$S/san-home-$P/.config/gh \
         HERMES_HOME=$S/p-$P /home/elliot/.local/bin/hermes auth list   # solo nous
   done
   ```
3. **GO/NO-GO para E3**: **NO-GO** hasta completar (2) y re-verificar
   metadata-only; después **GO** para E3 (prompts canary con `ask-human`).

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
