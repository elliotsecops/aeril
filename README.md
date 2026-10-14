# Aeril 🪶

Un terminal donde el agente de IA es un **participante nativo** del entorno: cada panel es un *thread* — prompt, razonamiento, diff y comentarios del humano, anclados a un snapshot inmutable y recuperables días después.

**La promesa:** corre completo en un ThinkPad de 2018 con 16 GB. No necesitás comprar una laptop nueva.

> Estado: **Fase 0 completa, auditada e integrada en `main`.**
> Commit certificado: [`41ab86cc28acad3476da3662723fd2411d6db16b`](https://github.com/elliotsecops/aeril/commit/41ab86cc28acad3476da3662723fd2411d6db16b) (2026-10-14, fast-forward).
> La auditoría final independiente pasó **62/62 tests**, Clippy (`-D warnings`) y formateo.

**Entregado en Fase 0** (el corte vertical completo, un thread):

- editar → snapshot inmutable → revisar → comentar → feedback → reanudar tras crash → validar → integrar (fast-forward), con el gate A/B automatizado.
- Workspaces verificados por Aeril, refs de checkpoint propias, recuperación al reabrir sin tocar el working tree, comentarios anclados a contenido inmutable, permisos fail-closed, pipeline ACP byte-bounded, leases de workspace con fence transaccional.
- Presupuestos medidos en la laptop objetivo: arranque p95 ≈ 26 ms, RSS idle ≈ 6.7 MB, binario ≈ 5.5 MB (declaración completa en el handoff de ingeniería).

**Planificado (Fase 1):** dos agentes concurrentes en workspaces verificados independientes, ciclo de vida/ownership multi-run, resume de threads sin depender del worktree original, aislamiento de perfiles del backend. No implementado aún.

**Límites declarados** (no sobreprometer):

- Hermes real **no** fue medido con 5 agentes (comparte `~/.hermes`); el bench de escala usa agentes mock y lo declara.
- Los leases coordinan procesos de Aeril entre sí: no son un sandbox de seguridad.
- Submódulos y patches grandes (> 4 MiB) se rechazan explícitamente en v0.

Documentación:

- Constitución, presupuestos duros e invariantes: [MANIFIESTO.md](MANIFIESTO.md)
- Estado del proyecto y próximo paso: [HANDOFF.md](HANDOFF.md)
- Estado de implementación: [docs/phase0-status.md](docs/phase0-status.md)
- Handoff de ingeniería (cómo reproducir): [docs/phase0-engineering-handoff.md](docs/phase0-engineering-handoff.md)
- Aceptación de la segunda auditoría (blockers RC2/RC2.1): [docs/phase0-rc2-acceptance.md](docs/phase0-rc2-acceptance.md)
- Matriz de remediación de la primera auditoría: [docs/phase0-remediation-matrix.md](docs/phase0-remediation-matrix.md)
- Especificación del ACP de Hermes verificada contra el código: [docs/acp-hermes-spec.md](docs/acp-hermes-spec.md)

*Velocidad y liviandad, juntas, en el aire.*
