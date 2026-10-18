# P2-S1 R3 — Remediación focalizada de lock inheritance y owner lifecycle

**Rama:** `feat/phase2-s1-profiles`
**Baseline R2:** `e1bf278d289d1d7009c79c030b629fdd39b1b8af`
**PR:** #1 — DRAFT, sin merge
**`main` certificado:** `47f0318eaadded7b93c027ba42efe0f9778f0f87`
**Veredicto de entrada:** HOLD — 2 P1 + 2 P2
**Estado:** entrega para re-auditoría; **no** declara aceptación.

Sin credenciales reales, perfiles P2-S0, OAuth, tags, merge ni cambios a
`main`. Todos los fixtures son descartables.

---

## 1. Causa raíz y corrección

| ID | Causa raíz | Corrección |
|---|---|---|
| **P1-1** | `Runner::attach_profile` llamaba `make_inheritable()` antes de `auth_status`; el probe de auth heredaba el fd del writer lock y un descendiente `setsid` podía retenerlo. | El lock permanece `FD_CLOEXEC` durante auth y checks. La herencia se habilita únicamente inmediatamente antes del spawn ACP gestionado (Runner y warm). Los probes de auth no heredan el lock. |
| **P1-2** | `removing=true` solo se miraba en algunas operaciones; `run start` y `profile warm` podían iniciar un writer bajo un tombstone. | `ensure_profile_usable` antes de auth/P y `ensure_profile_not_removing` bajo el registry lock final. Warm y admission se rechazan; `status/list` siguen siendo diagnóstico y `status` evita lanzar auth cuando hay tombstone. |
| **P2-1** | `record_admission_owner` dejaba `owner.json` con el PID del supervisor; una TUI viva hacía que un run terminal pareciera un writer activo. | Owner lifecycle explícito: admission owner pre-spawn → backend owner post-spawn → terminal cleanup bajo el profile lock. `finish_writer` limpia solo con owner checks + process scan limpios; superviviente/error de scan/clear deja owner durable y degrada el run a `failed`. |
| **P2-2** | El error de owner en warm ignoraba `terminate_group()` y convertía el error de scan en lista vacía; un fallo de observación podía parecer limpieza exitosa. | Intención de owner antes del spawn; actualización con backend real; errores de terminación y scan se propagan y se formatean explícitamente; no se usa `unwrap_or_default()` para declarar ausencia de supervivientes. |

---

## 2. Invariantes de lock inheritance

1. El perfil writer lock se abre con `FD_CLOEXEC` (default de `File`).
2. Auth, `profile status`, `verify_no_survivor`, resolución de binario/env y
   checks de registry se ejecutan sin herencia.
3. Solo se limpia `FD_CLOEXEC` inmediatamente antes de:
   - `AcpClient::spawn` del backend ACP de un run gestionado;
   - spawn del ACP de `profile warm`.
4. El lock se actualiza con `dev:ino` en `owner.json`; un unlink+create del
   lock path se detecta antes de decisiones críticas.
5. Un descendiente de auth no recibe el lock, aunque abra otros fds o escape
   con `setsid`.
6. El backend ACP sí lo hereda; un SIGKILL del supervisor deja al backend
   capaz de mantener exclusividad, y la recuperación/admission lo verifica.

### Owner lifecycle

```text
profile create
  │
  ├─ admission: record admission owner (supervisor, lock dev:ino)
  │      │
  │      ├─ auth (CLOEXEC, no lock)
  │      ├─ registry final check
  │      ├─ runs.started/profile commit   ← linearization
  │      └─ backend spawn → owner.backend_pid/start
  │
  ├─ active: profile lock + backend/descendants
  │
  └─ terminal:
       finish_writer (profile lock held)
         ├─ owner identity checks
         ├─ profile-wide process scan
         ├─ clean → clear owner
         └─ survivor/scan/clear error → retain owner + run failed
```

`finish_owner` is called explicitly before `runs::end` and is also the
`ProfileAttachment` Drop backstop for early returns/unwind. Evidence is not
cleared merely because SQLite says `completed`.

### Tombstone lifecycle

```text
remove --confirm
  → profile lock
  → registry lock + strict ownership checks
  → run/legacy/owner/process/path checks
  → registry entry.removing = true (durable)
  → filesystem removal
  → registry entry removal
```

`ensure_profile_usable` rejects run/warm before they can initiate a writer;
`ensure_profile_not_removing` repeats the check under the final registry lock.
`create`, `assign`, `unassign`, `warm` and run admission reject the tombstone.
`status`/`list` remain diagnostic; `status` does not spawn auth while
`removing=true`.

---

## 3. Cross-component adversarial checks

| # | Check | R3 result |
|---|---|---|
| 1 | Successful auth forks a detached descendant holding inherited descriptors | GREEN: auth descendant cannot keep `aeril.lock`; run completes and owner clears. |
| 2 | Auth timeout with detached descendant | GREEN: auth is `unknown`; descendant survives without lock. |
| 3 | Auth malformed with detached descendant | GREEN: auth is `unknown`; descendant survives without lock. |
| 4 | Tombstone before run/warm | GREEN: both rejected; run row absent. |
| 5 | Tombstone appears during slow auth | GREEN: final registry check rejects admission; no run row. |
| 6 | SIGKILL immediately after tombstone persisted | GREEN: tombstone survives; second remove completes. |
| 7 | Long-lived TUI completes managed run | GREEN: owner cleared, lock free, CLI warm and a second run succeed while TUI remains alive. |
| 8 | Escaped backend after terminal response | GREEN: SQLite/cleanup marks run failed; owner and inherited lock retained; no false clean. |
| 9 | Owner cleanup fails after a clean scan | GREEN: run failed; `owner.json` retained. |
| 10 | Backend-owner write fails after spawn | GREEN: run failed; backend killed; clean owner evidence cleared only after scan. |
| 11 | Reused supervisor PID | GREEN: stale owner does not block a legitimate warm. |
| 12 | Previous backend survives supervisor SIGKILL | GREEN: retained in the R1 P1-4 regression; admission/warm rejected while survivor exists. |

The direct R2/R3 reproducer covers checks 1–3, 4–5 (tombstone), and 7. The
remaining checks are deterministic Rust fault-injection tests.

---

## 4. RED/GREEN evidence

- `docs/phase2-s1-r3-evidence/red-r2-e1bf278.txt`: same CLI/PTY reproducer
  against the exact R2 binary. **5/5 RED**:
  - auth-success descendant retained lock;
  - auth-timeout descendant retained lock;
  - auth-malformed descendant retained lock;
  - tombstone allowed run + warm;
  - live TUI left a false HOLD.
- `docs/phase2-s1-r3-evidence/green-r3.txt`: **5/5 GREEN** on R3.
- `docs/phase2-s1-r3-evidence/r3-suite.txt`: **41/41** R2+R3 tests PASS.
- `docs/phase2-s1-r3-evidence/full-suite.txt`: **204/204** full Cargo suite.
- `docs/phase2-s1-r3-evidence/r1-repro.txt`: **7/7 GREEN**.
- Budget evidence: `budget-main.txt`, `budget-scale.txt`, `budget-two.txt`.

P2-2 does not have a direct end-to-end R2 RED because the old failure requires
an unavailable `/proc` observation or a partial cleanup after an owner-write
failure. The R2 code path and the deterministic R3 fault-injection tests are
documented; no claim is made that a hostile `/proc` state was reproduced.

---

## 5. Regression and budgets

Release was rebuilt from the final source with `cargo build --release` before
the budget matrix (the Makefile `$(BIN)` target itself still lacks source
prerequisites).

| Gate | Result |
|---|---|
| Cargo suite | **204/204 PASS** |
| R3 suite (R2+R3) | **41/41 PASS** |
| R1 repro | **7/7 GREEN** |
| fmt check | PASS |
| Clippy `-D warnings` | PASS |
| Binary | 5,834,352 B (5.8 MB) ≤ 30 MB |
| RSS idle / thread open / delta | 7.1 MB / 8.0 MB / 0.96 MiB |
| Startup p95 | 19.73 ms ≤ 300 ms (30 samples, 0 fail) |
| 5 mocks app / agents / total | 10.23 / 22.26 / 32.49 MB; app ≤500 MB PASS |
| Two-run TUI / mocks / combined | 8.65 / 4.59+4.80 / 18.25 MB |
| Startup with two active runs | 50.57 ms |
| Query latency | max 5.83 ms, avg 3.98 ms |

No thresholds were relaxed. Mock numbers do not characterize real Hermes.

---

## 6. Remaining risks / unverified cases

1. **P2-S0 provider semantics:** `failed_turn` from the provider while ACP
   returns `end_turn` remains a P2-S2 blocker. R3 does not alter this.
2. **v3 downgrade:** old binaries do not know `runs.profile`; do not downgrade
   with managed writers or unresolved managed runs. See R2 report §4.
3. **Same-UID filesystem races:** inode checks and `O_NOFOLLOW` narrow but do
   not eliminate a deliberate same-UID rename/swap; deletion is not fully
   fd-relative (`openat2`/`unlinkat`).
4. **Hidden same-UID processes:** a process that scrubs environ/cmdline and
   closes fds may not be attributable; the system prefers HOLD where evidence
   is incomplete.
5. **Escaped auth descendants:** R3 prevents lock inheritance; it does not
   kill an escaped auth child without verified identity. The child may remain
   alive after a timeout.
6. **Unavailable `/proc` + owner-write failure:** not deterministically
   combined in a single fixture. R3 tests owner-clear failure, survivor
   detection, and failed backend-owner write independently.
7. **SIGKILL between terminal SQLite write and owner cleanup:** not directly
   injected. The owner record is retained on any pre-clear failure; a later
   recovery/remove revalidates it.
8. **Hermes version contract:** auth parsing is verified for the installed
   0.21.x contract; other versions may yield `unknown`.

---

## 7. Auditor handoff

Use the existing PR #1 branch and the exact head SHA reported in the delivery
message. The minimal focus is:

1. auth descendant fd inheritance with successful/timeout/malformed auth;
2. tombstone before and during admission, and SIGKILL after persistence;
3. terminal owner cleanup with a live TUI, escaped backend, clear failure and
   backend-owner write failure;
4. lock order `L → P → R → M → S` and the absence of an R→P reverse path;
5. the twelve R2 regressions and the full Phase 1 budget matrix.

```bash
cargo test --test phase2_s1_r2 -- --test-threads=1
python3 scripts/phase2/r1_repro.py --bin target/debug/aeril
python3 scripts/phase2/r3_repro.py --bin target/debug/aeril
```

Final acceptance remains with the independent auditor.
