# Fase 1 — S1: aislamiento de Hermes para concurrencia (spike)

> Spike time-boxed (2026-10-14). **No implementa S2–S6.** Perfiles desechables
> bajo `/tmp/opencode/s1/`; el perfil `~/.hermes` del usuario **no fue
> modificado** (evidencia: strace + handles de archivo). Ninguna credencial fue
> impresa, copiada ni commiteada.

## Entorno

- Hermes: `0.21.5+8877.g865ba90` (`hermes acp --version`), provider del perfil
  del usuario: `nous` (OAuth), modelo `stealth/space-bunny-alpha` (404 en el
  catálogo actual; se usaron modelos free para demos en Fase 0).
- Kernel: Linux x86_64 (Ubuntu 26.04), `ptrace_scope=1`, `strace` disponible.
- Intérprete: toolchain compartido bajo `~/.hermes/tools/python-3.14.7…` y
  código de `~/.hermes/hermes-agent` (se **leen**; no son datos de perfil).
- Perfiles desechables: `/tmp/opencode/s1/p-A`, `p-B` (cada uno con un
  `config.yaml` mínimo propio: `model.provider: nous`). Repos desechables
  `repo-A`, `repo-B`.

## Mecanismos de aislamiento verificados

1. **`HERMES_HOME` es de primera clase** (`hermes_constants.get_hermes_home()`:
   override → env `HERMES_HOME` → default `~/.hermes`). El propio código pide a
   los spawners pasar `HERMES_HOME` explícito ("The subprocess spawner should
   pass HERMES_HOME explicitly"). Todos los paths de datos derivan de ahí:
   `state.db`, `sessions/`, `logs/`, `cache/uv`, `installs/`, `auth.json`.
2. **Sin escrituras al árbol del usuario**: strace de arranque
   (`strace -f -e trace=openat … env HERMES_HOME=/tmp/…/p-A hermes acp`)
   mostró cero `O_CREAT/O_WRONLY` bajo `/home/elliot/.hermes` (solo lecturas
   del código/pycache). uv/leasas/locks van al perfil:
   `p-A/cache/uv/.lock`, `p-A/installs/…/.prepare.lock`, `…/.leases/<id>`.
3. **Runtime por perfil (no compartido)**: cada perfil materializa sus propias
   dependencias (`p-A/installs/95d75e39…/environments/<hash>/…`) y logs
   (`p-A/logs/agent.log`, `errors.log`); ~392 MB por perfil. El único share es
   el **intérprete Python** (lectura) y el checkout del código.
4. **Clonado oficial de perfiles excluye credenciales**:
   `_CLONE_CONFIG_FILES = ["config.yaml", ".env", "SOUL.md"]` (skills y
   memorias aparte); `auth.json` queda fuera a propósito ("auth.json /
   .anthropic_oauth.json copied verbatim fork single-use OAuth grants"). El CLI
   `hermes profile` existe (`-p <nombre>` selecciona perfil) pero crear uno
   registra servicio de gateway y notifica al multiplexor: por eso Aeril debe
   **gestionar sus propios directorios HERMES_HOME** (sin registro oficial) en
   S3, o usarlo solo si el usuario lo pide.

## Evidencia de sesiones y estado

- **`state.db` por perfil**: `p-A/state.db` y `p-B/state.db` (con `-wal/-shm`)
  abiertos por el proceso respectivo (handles); `HERMES_HOME=/tmp/…/p-A hermes
  sessions list` → "No sessions found." (store del perfil).
- **Sesiones reales NO verificables sin credenciales**: con perfil desechable,
  `session/new` devuelve `-32603 {"details": "Hermes is not logged into Nous
  Portal."}` y `authMethods` = solo `hermes-setup`. Es decir: **la identidad de
  sesión no se pudo ejercitar end-to-end** (crear/persistir/`session/load`
  cruzado) — no se declara verificado.
- `session/load` de sesión desconocida sigue respondiendo OK sin historial
  (quirk ya documentado en `docs/acp-hermes-spec.md`); la separación real será
  observable con sesiones autenticadas.

## Comportamiento concurrente (procesos reales)

- Dos `hermes acp` simultáneos (p-A y p-B, `session/new` falla por auth):
  coexisten sin interferencia; handles de archivo **disjuntos** (cada uno solo
  su `state.db`, logs, installs/leasas del perfil).
- **Kill aislado**: `kill -9` al Hermes de A → el Hermes de B sigue vivo; un
  proceso nuevo sobre p-B inicializa normal (D: `initialize` 3.3 s, mismo error
  de auth esperable).
- **Locking por perfil**: hay locks de runtime (`auth.lock`, `.leases`,
  `.prepare.lock`, multiplexor de gateway) que asumen **un escritor por
  HERMES_HOME**. Compartir un perfil entre dos ACP concurrentes no está
  soportado por diseño.
- **Descubrimiento de credenciales ambientales** (hallazgo de seguridad):
  `HERMES_HOME` desechable + `hermes auth list` encontró una credencial
  `copilot` vía `gh auth token` (fuente externa al perfil). Es decir, el
  aislamiento de credenciales no es hermético: el perfil puede heredar
  credenciales que ya existan en el entorno (gh CLI, `~/.codex`, `~/.claude`).
  Coherente con el modelo de confianza de Aeril (el agente tiene los permisos
  del usuario), pero invalida cualquier claim de "perfil sin acceso a
  credenciales del host".

## Recursos medidos (esta laptop, dos perfiles)

| Métrica | Valor |
|---|---|
| RSS de `hermes acp` inicializado (idle, en hold) | ~132 MiB por proceso (134.8/134.7 MB VmRSS) |
| Primer arranque de un perfil nuevo (materializa deps) | ~39 s (bajo carga, concurrente); ~3.3–3.9 s en arranques posteriores |
| Disco por perfil (deps + caches) | ~392 MB |
| Dos procesos concurrentes | OK en 15 GiB con ~2 GB disponibles; sin contención entre perfiles |

## Comandos usados (sin secretos)

```bash
# arranque instrumentado de un perfil desechable
strace -f -e trace=openat -o trace-A.txt \
  env HERMES_HOME=/tmp/opencode/s1/p-A hermes acp < /dev/null

# probe ACP (initialize/session/new/session/list/hold)
python3 /tmp/opencode/s1/s1_acp_probe.py --label A \
  --hermes-home /tmp/opencode/s1/p-A --cwd /tmp/opencode/s1/repo-A --hold-secs 25 --out A.json

# auth/estado por perfil
HERMES_HOME=/tmp/opencode/s1/p-A hermes auth list      # etiquetas, jamás el valor
HERMES_HOME=/tmp/opencode/s1/p-A hermes sessions list  # store del perfil
```

El probe scrubea del entorno las variables que contengan
`API_KEY/TOKEN/SECRET/PASSWORD/CREDENTIAL` antes de lanzar Hermes y trunca
stderr; no escribe credenciales en los JSON de evidencia.

## Limitaciones restantes

1. **Sesiones reales en paralelo: no verificadas** (bloqueadas por auth por
   perfil; `session/new` no abre sin login). No se puede afirmar
   concurrencia segura de agentes reales todavía.
2. **Provisioning de credenciales es interactivo** (`hermes auth add` /
   `hermes model` por perfil). Copiar `auth.json` está prohibido por política y
   es inseguro para refresh OAuth concurrente; compartir un perfil entre
   procesos concurrentes no está soportado (locks de un solo escritor).
3. **Credenciales ambientales** pueden ser descubiertas por cualquier perfil
   (gh/codex/claude): el aislamiento es de estado, no de secretos del host.
4. Primer arranque de perfil es lento (~39 s) y ~400 MB de disco por agente:
   el onboarding debe pre-calentar perfiles.
5. Código/intérprete compartidos (lectura): un update de Hermes afecta a todos
   los perfiles a la vez (fuera de alcance de S1).

## Veredicto

**`CONDITIONAL`.**

- **Soportado y verificado**: aislamiento de **estado en disco** por
  `HERMES_HOME` (db, logs, installs, locks, caches), coexistencia de procesos
  concurrentes con handles disjuntos, kill aislado, y ausencia de escrituras al
  perfil del usuario.
- **No soportado todavía**: concurrencia de **sesiones reales** (requiere
  autenticar cada perfil) y aislamiento de **credenciales** (ambientales +
  provisioning interactivo). Con el estado actual, **modo soportado: un Hermes
  a la vez**; el modo multi-agente con Hermes real queda condicionado a
  perfiles autenticados por separado y a una segunda validación con sesiones
  reales.

## Aproximación mínima para S3 (si se habilita)

1. Aeril gestiona un directorio de perfil por agente:
   `<Aeril data>/hermes/<agent-id>/` y lanza `hermes acp` con `HERMES_HOME`
   **explícito** (recomendación del propio código), cwd = worktree del agente.
   No usar `hermes profile create` por defecto (registra gateway/multiplexor).
2. Onboarding por perfil (una vez, interactivo, sin copiar archivos de
   credenciales): `HERMES_HOME=… hermes auth add <provider>` o `hermes model`.
   Pre-calentar dependencias del perfil (evita el primer arranque de ~39 s).
3. Regla dura: **un proceso ACP por perfil** (los locks de Hermes asumen un
   escritor); Aeril ya tiene leases de workspace — añadir en S3 una guarda
   equivalente perfil↔run.
4. Gate de habilitación: repetir este spike con **dos perfiles autenticados**
   y sesiones reales (crear, persistir, `session/list`, `session/load` cruzado
   negativo, kill), documentar RSS/arranque, y solo entonces retirar el modo
   single-Hermes.
