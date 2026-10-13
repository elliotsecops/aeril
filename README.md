# Aeril 🪶

Un terminal donde el agente de IA es un **participante nativo** del entorno: cada panel es un *thread* — prompt, razonamiento, diff y comentarios del humano, anclados a un snapshot inmutable y recuperables días después.

**La promesa:** corre completo en un ThinkPad de 2018 con 16 GB. No necesitás comprar una laptop nueva.

> Estado: **Fase 0 en ejecución.** El corte vertical (editar → snapshot → revisar → comentar → feedback → reanudar tras crash → validar → integrar) está implementado en la rama `feat/phase0-vertical-slice`, con el gate A/B automatizado y presupuestos medidos en la laptop objetivo.

Documentación:

- Constitución, presupuestos duros e invariantes: [MANIFIESTO.md](MANIFIESTO.md)
- Estado del proyecto, gate del ensayo y plan: [HANDOFF.md](HANDOFF.md)
- Estado de implementación de Fase 0: [docs/phase0-status.md](docs/phase0-status.md)
- Handoff de ingeniería (cómo reproducir): [docs/phase0-engineering-handoff.md](docs/phase0-engineering-handoff.md)
- Especificación del ACP de Hermes verificada contra el código: [docs/acp-hermes-spec.md](docs/acp-hermes-spec.md)

*Velocidad y liviandad, juntas, en el aire.*
