# PA0-C — Preparación: Integración bloqueada, instrucciones contextuales y guion de aceptación

> **Estado:** Implementado en `feat/pa0-c` (`2082505`) sobre `PA0-PERF-R` (`8cce419`) e integrado en `main` (`tests/pa0_c_blocked_integration.rs` C1–C6).
> **Referencia contractual:** [`docs/pa0-alpha-contract.md`](pa0-alpha-contract.md) §3 (`E2E-5` Camino B) y §8 (`PA0-C`).
> **Código actual inspeccionado:** `src/integration.rs`, `src/git/integrate.rs`, `src/review.rs`, `src/cli.rs`, `src/tui/mod.rs`.

---

## 1. Objetivo de PA0-C

Cuando la integración fast-forward asistida está bloqueada, el desarrollador debe poder entender **exactamente por qué** está bloqueada, ver los **OIDs exactos de origen y destino**, y recibir un **procedimiento manual seguro y comprensible** para completar o destrabar el flujo **sin abrir otra terminal a adivinar ni saltarse review, validación ni CAS**.

**Regla dura:** Ninguna instrucción manual de PA0-C puede recomendar `--force`, `git reset --hard` sobre cambios no guardados, ni omitir `review.acknowledged`, `aeril validate` sobre el snapshot/OID exacto o la verificación de ancestría/OID esperado.

---

## 2. Matriz de bloqueos y diseño de instrucciones contextuales

### 2.1 Contexto mínimo que todo bloqueo debe exponer

Siempre que `IntegrationPreview.eligible == false` (TUI `render_integration_preview` y CLI `aeril integrate preview --json` / texto), exponer de forma consistente:

| Campo / Dato | Propósito |
| --- | --- |
| `status` + `status_label` | Categoría precisa (`target_checked_out`, `stale_target`, `diverged`, `validation_required`, `validation_failed`, `review_required`, etc.) |
| `target_branch` | Rama destino explícita (ej. `main`) |
| `source_oid` (completo + corto) | Commit inmutable exacto del `checkpoint.ready` (y su ref `refs/aeril/checkpoints/<snapshot_id>`) |
| `target_oid` (completo + corto) | OID actual de `refs/heads/<target_branch>` (cuando existe) |
| `validation` / `review` | Estado verificado de los gates previos |
| `next_steps` (instrucción contextual) | Pasos exactos copiables y seguros según el bloqueo activo |

---

### 2.2 Escenarios priorizados

#### A. Rama destino checked-out (`main` checked out — `IntegrationStatus::TargetCheckedOut`)
- **Causa real actual:** `git::integrate::checked_out_worktree(repo, target_branch)` detecta el checkout principal o un worktree con `HEAD == refs/heads/<target_branch>`. Mover la ref desde afuera desincronizaría el working tree e índice del usuario.
- **Precondición de seguridad:** Solo ofrecer instrucciones de fast-forward manual si `review == Clear` y `validation == Passed` para `source_oid`. Si falta review o validación, priorizar esos pasos antes de mover refs.
- **Procedimiento contextual (cuando review + validation están OK):**
  1. Verificar que no haya cambios sin commitear en el checkout donde está `main`:
     ```bash
     git -C <checked_out_path> status --short
     ```
  2. Desde ese checkout limpio en `<target_branch>`, validar el OID esperado y aplicar fast-forward estrictamente (sin `--force`):
     ```bash
     git -C <checked_out_path> merge --ff-only <source_oid>
     ```
  3. Re-ejecutar preview en Aeril (`aeril integrate preview --thread <T> --snapshot <S> --target <target_branch>` o abrir `i` en TUI) para confirmar estado `already integrated` (`target_oid == source_oid`) o registrar el cierre explícito.

#### B. Checkout sucio (`dirty checkout` al intentar integrar en `main` checked-out)
- **Causa:** El usuario sigue paso A en un checkout con modificaciones locales sin guardar/commitear; `git merge --ff-only` abortará con seguridad si toca archivos modificados, o el usuario podría perder orientación.
- **Procedimiento contextual:**
  1. Revisar el estado del checkout destino: `git -C <checked_out_path> status`.
  2. Guardar o confirmar el trabajo local antes de integrar:
     - `git -C <checked_out_path> stash push -m "wip antes de integrar <snapshot_short>"` (o hacer commit en una rama propia).
  3. Con el árbol limpio, ejecutar `git -C <checked_out_path> merge --ff-only <source_oid>`.
  4. Restaurar el WIP si aplica (`git -C <checked_out_path> stash pop`) y refrescar en Aeril.
  - **Alternativa sin tocar ese checkout:** si el checkout principal está en otra tarea, cambiar temporalmente el checkout principal a otra rama (`git switch --detach` o rama de trabajo), o reabrir preview en Aeril una vez que `<target_branch>` deje de estar checked out.

#### C. Target movido o stale (`IntegrationStatus::StaleTarget` / `IntegrationBlocked::Stale`)
- **Causa:** Entre la generación del `IntegrationPreview` (`target_oid` esperado) y `confirm_integration` / `integrate apply`, otro commit avanzó `refs/heads/<target_branch>` de `<expected_oid>` a `<actual_oid>`.
- **Procedimiento contextual:**
  1. Inspeccionar qué avanzó la rama destino:
     ```bash
     git log --oneline <expected_oid>..<actual_oid>
     ```
  2. Generar un **preview nuevo** (`Esc` → `i` en TUI o `aeril integrate preview ...`) para re-evaluar ancestría contra `<actual_oid>`:
     - Si `<actual_oid>` ya es `<source_oid>` → queda `already integrated` (no-op).
     - Si `<source_oid>` sigue descendiendo → preview pasa a `fast-forward eligible` contra `<actual_oid>`.
     - Si la rama divergió → cae al flujo **D (Divergencia)**.

#### D. Divergencia (`IntegrationStatus::Diverged` / `IntegrationBlocked::Diverged`)
- **Causa:** `source_oid` no desciende de `target_oid` (alguien avanzó `<target_branch>` con commits que no están en la base del checkpoint). Fast-forward imposible sin reescribir/integrar manualmente.
- **Procedimiento contextual seguro (sin bypass de gates):**
  1. Inspeccionar ambos lados con identidades exactas:
     ```bash
     git log --oneline <source_oid>..<target_oid>   # commits nuevos en destino
     git log --oneline <target_oid>..<source_oid>   # commits del checkpoint
     ```
  2. Opción recomendada (rebase / merge manual en checkout limpio, generando **nuevo commit/estado validado**):
     - Crear rama temporal sobre `<source_oid>` o usar el worktree del agente / rama `<target_branch>`:
       ```bash
       git checkout -b integrate/<snapshot_short> <target_oid>
       git cherry-pick <source_oid>   # o git merge refs/aeril/checkpoints/<snapshot_id>
       ```
     - Resolver conflictos si existen y **ejecutar las pruebas de nuevo** sobre el árbol integrado antes de actualizar `<target_branch>` (Aeril no traslada la validación/ack de `<source_oid>` a un commit distinto creado por merge/rebase).
     - Opcionalmente, en Aeril: lanzar nuevo run / rehidratar sobre la nueva base si se desea trazabilidad completa dentro del thread.

#### E. Validación pendiente, fallida, de otro OID o inválida (`ValidationRequired`, `ValidationFailed`, `WrongOid`, `Invalid`)
- **Causa:** No existe `validation.recorded` con `exit_code == 0`, `tree_before_ok == true`, `tree_after_unchanged == true` atado exactamente a `(snapshot_id, source_oid)`.
- **Procedimiento contextual:**
  1. Exponer explícitamente por qué falló o por qué falta (comando previo, `exit_code`, OID validado vs `source_oid` actual, o mutación detectada en checkout efímero).
  2. Comando exacto copiable para validar en checkout aislado el snapshot exacto:
     ```bash
     aeril validate --thread <thread_id> --snapshot <snapshot_id> -- <comando_de_prueba>
     ```
  3. Recordar que ninguna integración asistida ni manual recomendada puede saltarse este paso mientras el objetivo sea aprobar el snapshot.

#### F. Review pendiente (`ReviewRequired` / `ReviewState::Pending` / `ReviewState::Required`)
- **Causa:**
  - `Pending`: hay `N` comentarios abiertos (`open`) anclados al snapshot (`unresolved_comments > 0`).
  - `Required`: no hay comentarios abiertos, pero falta `review.acknowledged` explícito para `(snapshot_id, source_oid)`.
- **Procedimiento contextual:**
  - Si `Pending`:
    - Revisar/resolver en TUI o CLI (`aeril comment resolve --thread <T> --comment <C>`), o enviar feedback (`f` en TUI / `aeril feedback send`) para que el agente produzca un nuevo checkpoint.
  - Si `Required`:
    - Verificar el diff visible del snapshot `<snapshot_short>` y emitir acknowledgement explícito:
      - TUI: presionar `Esc` (salir del preview), verificar patch y presionar `a`.
      - CLI: `aeril review ack --thread <thread_id> --snapshot <snapshot_id> --oid <source_oid>`.

---

## 3. Guion de aceptación para PA0-C (`E2E-5` Camino B)

> **Regla:** Ejecutar con participante externo o evaluador siguiendo exclusivamente lo que muestra Aeril (TUI/CLI), **sin** ayuda verbal ni búsqueda manual en logs internos.

### Casos de prueba E2E requeridos

1. **C1 — `main` checked out con review y validación OK (`TargetCheckedOut`):**
   - Precondición: repo con `main` checked out en el directorio principal limpio; checkpoint validado y acknowledged.
   - Acción: abrir preview (`i` → `main` o `aeril integrate preview`).
   - Resultado esperado:
     - Aeril muestra `target checked out`, `source_oid`, `target_oid`, path del checkout dueño de `main`, y el comando exacto `git -C <path> merge --ff-only <source_oid>`.
     - El participante entiende qué hace el comando, lo ejecuta sin errores y al reabrir preview en Aeril ve `already integrated` sin abrir otra búsqueda manual de OIDs.

2. **C2 — `main` checked out con árbol sucio (`dirty checkout`):**
   - Precondición: modificación local sin commitear en el checkout principal sobre el archivo tocado por el checkpoint.
   - Resultado esperado: Aeril recuerda verificar `git status` y preservar cambios (`stash`/`commit`) antes del `--ff-only`, sin recomendar `--force` ni `reset --hard`.

3. **C3 — Target movido entre preview y confirmación (`StaleTarget`):**
   - Precondición: abrir preview cuando `main` está en `OID_1`; avanzar `main` externamente a `OID_2`; pulsar `y` (o `integrate apply`).
   - Resultado esperado: CAS rechaza mover la ref; el mensaje muestra `expected=<OID_1>`, `actual=<OID_2>` y comando para inspeccionar `git log --oneline <OID_1>..<OID_2>` + instrucción de regenerar preview.

4. **C4 — Divergencia (`Diverged`):**
   - Precondición: `main` tiene un commit nuevo que no está en `base_oid` del checkpoint.
   - Resultado esperado: Aeril bloquea sin hacer merge, expone `<source_oid>` y `<target_oid>`, comandos de inspección en ambas direcciones (`..`) y advierte explícitamente que un merge/cherry-pick crea un árbol nuevo que debe re-validarse.

5. **C5 — Precedencia de gates (`ReviewPending` / `ValidationRequired` antes de instrucciones Git):**
   - Precondición: `main` checked out **Y** falta validación (o hay comentario abierto).
   - Resultado esperado: Aeril **no** sugiere `git merge --ff-only` mientras falte review/validación; primero muestra la acción de review/validación (`a` / `aeril validate`) para el `snapshot_id` + `source_oid` exactos.

6. **C6 — Verificación de contrato de integridad:**
   - En los 5 casos anteriores: `0` movimientos de ref sin CAS/confirmación, `0` bypasses de review o validación, y `integration.blocked` / `attention` reflejan exactamente la causa tras reiniciar Aeril.
