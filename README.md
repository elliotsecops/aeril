# Aeril 🪶

Un terminal donde el agente de IA es un **participante nativo** del entorno: cada panel es un *thread* — prompt, razonamiento, diff y comentarios del humano, anclados a un snapshot inmutable y recuperables días después.

**La promesa:** corre completo en un ThinkPad de 2018 con 16 GB. No necesitás comprar una laptop nueva.

> Estado: **Fase 0 completa y auditada; Fase 1 con gates de producto verdes y
> aceptación de ingeniería pendiente de re-verificación independiente (S6-R1:
> tooling del benchmark en remediación).** Registro: [`docs/phase1-status.md`](docs/phase1-status.md).
> Commit certificado Fase 0: [`41ab86cc`](https://github.com/elliotsecops/aeril/commit/41ab86cc28acad3476da3662723fd2411d6db16b) (62/62 tests).
> S5 (G1–G9) certificado: [`1cfe627`](https://github.com/elliotsecops/aeril/commit/1cfe6279cf9de6260493da937590b63c116c49ac).
> Concurrencia mock determinista (N=2) validada; sesiones Hermes reales
> concurrentes y restauración verificada de contexto quedan **fuera del alcance
> certificado**.

**Entregado en Fase 0** (el corte vertical completo, un thread):

- editar → snapshot inmutable → revisar → comentar → feedback → reanudar tras crash → validar → integrar (fast-forward), con el gate A/B automatizado.
- Workspaces verificados por Aeril, refs de checkpoint propias, recuperación al reabrir sin tocar el working tree, comentarios anclados a contenido inmutable, permisos fail-closed, pipeline ACP byte-bounded, leases de workspace con fence transaccional.
- Presupuestos medidos en la laptop objetivo: arranque p95 ≈ 26 ms, RSS idle ≈ 6.7 MB, binario ≈ 5.5 MB (declaración completa en el handoff de ingeniería).

**Entregado en Fase 1 (aceptación de ingeniería con mock determinista):**

- Reconciliación durable de runs interrumpidos (S0) y visibilidad `run list [--active]` (S2).
- TUI con dos runs supervisados a la vez (N=2, uno por thread), cancelación
  independiente con ack y salida segura (S3).
- Ciclo de vida de workspaces: `workspace list` de solo lectura y borrado
  guardado con fence transaccional de lease (S4/S4-R1).
- E2E G1–G9: concurrencia real, crash de supervisor, falla de backend,
  cancelación, revisión durable, rehidratación por OID exacto e integración
  fast-forward concurrente (S5).
- Presupuestos release re-medidos: arranque p95 ≈ 13 ms, RSS idle ≈ 6.8 MB,
  binario ≈ 5.6 MB, 5 mocks ≈ 29.5 MB total (app 9.7 MB).

**Límites declarados** (no sobreprometer):

- Hermes real sigue siendo **single-run** (la TUI lo impone): sesiones ACP
  autenticadas concurrentes **no verificadas**; el aislamiento de estado por
  `HERMES_HOME` sí fue verificado (S1), pero las credenciales del host **no
  están garantizadas** y el onboarding multi-perfil no está implementado.
- Restauración de **contexto conversacional no verificada** (`verified=false`);
  los checkpoints restauran archivos por OID exacto, no memoria del agente, y
  las ediciones no chequeadas no son recuperables.
- Hermes real **no** fue medido con 5 agentes (comparte `~/.hermes`); el bench de
  escala y el escenario de 2 runs usan agentes mock y lo declaran.
- Los leases coordinan procesos de Aeril entre sí y los worktrees **no** son un
  sandbox de seguridad.
- Submódulos y patches grandes (> 4 MiB) se rechazan explícitamente en v0.

Documentación:

- Constitución, presupuestos duros e invariantes: [MANIFIESTO.md](MANIFIESTO.md)
- Estado de aceptación de Fase 1 (S6): [docs/phase1-status.md](docs/phase1-status.md)
- Plan de Fase 1 (S0–S6, gates G1–G9): [docs/phase1-plan.md](docs/phase1-plan.md)
- Estado del proyecto y próximo paso: [HANDOFF.md](HANDOFF.md)
- Estado de implementación: [docs/phase0-status.md](docs/phase0-status.md)
- Handoff de ingeniería (cómo reproducir): [docs/phase0-engineering-handoff.md](docs/phase0-engineering-handoff.md)
- Aceptación de la segunda auditoría (blockers RC2/RC2.1): [docs/phase0-rc2-acceptance.md](docs/phase0-rc2-acceptance.md)
- Matriz de remediación de la primera auditoría: [docs/phase0-remediation-matrix.md](docs/phase0-remediation-matrix.md)
- Especificación del ACP de Hermes verificada contra el código: [docs/acp-hermes-spec.md](docs/acp-hermes-spec.md)

*Velocidad y liviandad, juntas, en el aire.*
