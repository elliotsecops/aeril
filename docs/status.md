# Estado de Aeril

**Actualizado:** 2026-10-18
**Etapa:** desarrollo temprano para desarrolladores. Todavía no es un release de producción ni un instalador universal.

Aeril es un espacio de trabajo de terminal para usar agentes de IA que escriben código. Organiza cada tarea, su historial, sus cambios y su espacio de Git en un solo lugar, para que puedas revisar y continuar el trabajo sin reconstruir el contexto a mano.

No es un modelo de IA, un servicio en la nube ni un sandbox del sistema operativo. El agente sigue siendo externo.

## Qué podés hacer hoy

- Crear tareas y asociarlas a un repositorio Git local.
- Ejecutar un agente externo en un espacio de trabajo separado por tarea.
- Revisar el diff y el checkpoint inmutable de lo que produjo.
- Dejar comentarios anclados a una versión concreta del código.
- Enviar esos comentarios como feedback para otro intento.
- Validar un snapshot en un checkout aislado.
- Integrar por fast-forward a una rama destino explícita.
- Consultar el historial y exportarlo a JSONL o Markdown.

La interfaz de terminal es la superficie principal. La CLI cubre el mismo flujo para scripts y pruebas.

## Estado actual de las capacidades

### Hermes y perfiles gestionados

La gestión de perfiles Hermes de P2-S1 está **integrada en `main`**, pero permanece **detrás de un feature flag y desactivada por defecto** (`AERIL_HERMES_PROFILES`).

- Con el flag **OFF**, se conserva el camino certificado anterior.
- Con el flag **ON**, un run Hermes debe usar un perfil gestionado creado, asignado y autenticado; no hay fallback silencioso al perfil del host.
- La operación real soportada es **un run Hermes a la vez**. La TUI mantiene el límite single-Hermes.

### Concurrencia

Se validaron **dos runs concurrentes con agentes mock** en una TUI, con espacios de trabajo, checkpoints y cancelación independientes.

**Dos sesiones reales de Hermes autenticadas y concurrentes no están certificadas.** Los mocks demuestran el comportamiento del gestor, no el de Hermes real ni el aislamiento de credenciales.

### Recursos

El gestor Aeril está pensado para ser liviano: en la build release auditada, el binario mide aproximadamente 5.8 MB y el proceso en reposo usa alrededor de 7 MB de RAM. Esas cifras **no incluyen** la memoria del agente, del modelo o de los servicios externos que conectes.

Son mediciones de un entorno concreto, no mínimos garantizados para cualquier máquina.

## Límites actuales

- **Credenciales:** no hay aislamiento total de secretos, credenciales ni archivos del resto del host. El agente conserva los permisos del usuario.
- **Contexto del agente:** los checkpoints recuperan archivos verificados, no la memoria conversacional del modelo. Las ediciones que no llegaron a un checkpoint no son recuperables.
- **Aislamiento:** los worktrees separan archivos por tarea, pero no son sandboxes. Los locks coordinan procesos cooperativos de Aeril; no detienen a un proceso malicioso con el mismo usuario.
- **Integración:** solo fast-forward a una rama destino explícita. No hay merge general, cherry-pick ni resolución automática de conflictos.
- **Archivos:** la captura de snapshots tiene límites de tamaño y rechaza explícitamente binarios, submódulos y cambios no soportados.
- **Plataformas:** Linux es el entorno probado. Raspberry Pi, Android con Termux y planes de VPS concretos **no están certificados**.
- **Instalación:** no hay paquete universal; hoy se compila desde el repositorio.

## P2-S2 — próximo hito, en HOLD

P2-S2 es el siguiente paso de ingeniería y **no está autorizado todavía**. Debe resolver, como mínimo:

- la semántica de fallo del provider cuando informa un turno fallido mientras ACP devuelve `end_turn`;
- una re-baseline medida de recursos con Hermes real;
- el aislamiento de credenciales/secretos del host;
- la ejecución concurrente de sesiones Hermes autenticadas.

No hay fecha prometida para P2-S2. Hasta que esos puntos estén cerrados, la concurrencia real de Hermes no debe presentarse como soportada.

## Plataformas

| Entorno | Estado |
|---|---|
| Laptop o PC con Linux | Entorno de desarrollo y pruebas utilizado |
| VM Linux | Instalación posible; depende de la distribución y sus dependencias |
| VPS Linux económico | Objetivo de despliegue ligero, no una garantía para un precio concreto |
| Raspberry Pi | Interesante como plataforma ARM; no certificada |
| Android con Termux | Posibilidad futura; no probado ni soportado oficialmente |

La ligereza del gestor no garantiza que un agente completo, un modelo o un proveedor externo entren en un presupuesto de RAM o de precio determinado.

## Documentación técnica

- [Cómo empezar](../README.md#empezar)
- [Manifiesto y presupuestos](../MANIFIESTO.md)
- [Estado y pruebas de Fase 1](phase1-status.md)
- [Implementación y operación de perfiles P2-S1](phase2-s1-implementation.md)
- [Remediación y evidencia de auditoría R3](phase2-s1-r3-remediation.md)
- [Contrato ACP de Hermes](acp-hermes-spec.md)
- [Handoff histórico de ingeniería](../HANDOFF.md) — contexto de Fase 0/1, no el estado actual

El código de P2-S1 aceptado por la auditoría es `8051b325e821e148b7ed103367de7df184211ff4`. Este documento describe el estado de `main`; no agrega garantías nuevas ni reemplaza los reportes técnicos.
