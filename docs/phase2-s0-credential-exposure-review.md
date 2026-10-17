# P2-S0 — Revisión de exposición de credenciales en transcript local

> Revisión metadata-only (sin reproducir valores). Autorizada por el disclosure
> del auditor independiente: un transcript local contiene valores de token OAuth
> impresos. **No se borró evidencia ni se rotaron credenciales automáticamente.**

## 1. Ubicación exacta

- **Archivo**: `/home/elliot/.local/share/opencode/shell/b7c682c7b2b226f0fbaedd9bb14ce07218f9402c/sh_1479ad801001zBOxUGz8Ayjj4l.out`
  (9 766 B; modo original **664 = lectura para grupo/otros**; owner `elliot`).
- Contiene **4 strings opacos de exactamente 2 013 caracteres** — el tamaño
  medido de `access_token`/`agent_key` de Nous — y marcadores `p-A` **y** `p-B`.
- **Atribución**: referenciado por mensajes `msg_147e9b90…` y `msg_147ead3ad…`
  de la sesión `ses_ec8daba12ffecJQT…` (título "Aeril Agent A", dir
  `/home/elliot/aeril-phase0`) — la sesión del audit que produjo la impresión.
- **Contención ejecutada**: `chmod 600` del archivo (sin borrar ni modificar
  contenido). El resto del árbol `shell/` sigue 775/664 (sin valores
  token-sized verificados por escaneo).

## 2. Quién/qué puede acceder

- Localmente: cualquier usuario/proceso con acceso al filesystem podía leerlo
  **antes** del chmod (664). Ahora solo `elliot` (y root).
- El transcript también vive en la DB de OpenCode (`opencode.db`, modo **600**)
  como metadata de mensajes; el **contenido del shell-out** es el archivo
  citado (la DB no contiene valores token-sized — verificado por escaneo
  estructural: máximo 15 chars en campos `access_token`).
- Sin `share_url` en ninguna sesión (`session`/`session_v2`) ⇒ **no hubo
  upload/share por la función de share de OpenCode**.

## 3. ¿Sincronizado / compartido / respaldado?

| Vector | Hallazgo | Riesgo |
|---|---|---|
| **Syncthing** | Proceso activo; carpeta **`path="~"` (HOME completa), `sendreceive`, SIN patrones de ignore**, compartida con devices `elliot-thinkpad` y `staging` | **ALTO: el transcript está dentro del alcance de sync ⇒ puede haberse propagado a esos peers.** No hay `.stversions` (versionado off). `chmod` NO detiene syncthing |
| OpenCode share | `share_url` vacío en todas las sesiones | Nulo |
| Timeshift | `/timeshift` existe pero `include_home: None` y schedules off | Nulo (home no respaldado) |
| Clientes cloud (Dropbox/Drive/OneDrive/Nextcloud) | No instalados/activos | Nulo |
| `/tmp/opencode` | Limpiado en R6 (0 entradas en ese momento); recreado por el auditor con artefactos **sin** valores token-sized | Nulo |

## 4. ¿Ambos grants afectados?

**Potencialmente sí**: el archivo contiene 4 valores de 2 013 chars (dos por
perfil: `access_token` + `agent_key`) y marcadores de p-A y p-B. No se puede
determinar qué valor pertenece a qué perfil sin reproducir valores (prohibido)
⇒ se asume **ambos grants comprometidos**.

## 5. ¿El proveedor soporta revocación explícita?

- Hermes 0.21.5 **no tiene revocación provider-side en el CLI**: `hermes auth
  logout <provider>` solo limpia estado local; no se encontró endpoint de
  revocación en el código.
- ⇒ La revocación debe hacerse **en el proveedor** (Nous Portal: sesiones/
  seguridad de la cuenta, o soporte de Nous). La re-autenticación por sí sola
  **no** revoca los grants viejos.

## 6. Recomendación de contención (inmediata, ya hecha la parte local)

1. ✅ `chmod 600` del archivo identificado (hecho).
2. **Usuario**: verificar en `elliot-thinkpad` y `staging` si existe
   `~/.local/share/opencode/shell/b7c682c7…/sh_1479ad80…out`; si existe,
   tratarlo como comprometido y eliminarlo en esos peers (decisión del usuario).
3. **Usuario**: agregar ignores de syncthing para `.local/share/opencode`
   (idealmente `.local/share/opencode/shell`) en la carpeta `~` — o revisar si
   sincronizar HOME completa sigue siendo deseado.
4. No borrar la evidencia local (auditoría) mientras esté contenida.
5. No compartir `opencode.db` ni el árbol `shell/` sin filtrar.

## 7. Runbook de revocación y re-autenticación (dirigido por el usuario)

> **Re-ejecutar `auth add` NO es revocación probada.** Los tokens viejos siguen
> válidos hasta que el proveedor los revoque.

```bash
# PASO 1 (usuario, en Nous): revocar/cerrar las sesiones OAuth de esta cuenta
#   (portal de Nous → seguridad/sesiones, o soporte). Aplica a AMBOS grants.

# PASO 2 (opcional local): limpiar el estado local viejo de cada perfil
S=/home/elliot/p2s0-spike
for P in A B; do
  env -i PATH=/usr/bin:/bin LANG=C.UTF-8 HOME=$S/san-home-$P \
      XDG_CONFIG_HOME=$S/san-home-$P/.config GH_CONFIG_DIR=$S/san-home-$P/.config/gh \
      HERMES_HOME=$S/p-$P /home/elliot/.local/bin/hermes auth logout nous
done

# PASO 3: re-autenticar con device-code NUEVO por perfil (runbook §4 de
# docs/phase2-s0-hermes-auth-gate.md), uno por uno, interactivo.

# PASO 4: verificar SOLO metadata (sin valores)
for P in A B; do
  env -i ... HERMES_HOME=$S/p-$P hermes auth status nous   # "logged in"
  env -i ... HERMES_HOME=$S/p-$P hermes auth list          # solo etiquetas
done
```

Hasta completar el PASO 1, cualquier claim de aislamiento debe declarar que
los grants previos están **potencialmente comprometidos** (transcript + sync).
