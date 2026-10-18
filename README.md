# Aeril 🪶

**Tu espacio de trabajo para agentes de IA, directamente en la terminal.**

Trabajá con agentes que escriben código sin perder de vista qué están haciendo, qué cambiaron ni dónde quedó cada tarea. Aeril reúne sus ejecuciones, archivos, revisiones e historial en un solo lugar.

**Menos ventanas. Menos consumo de recursos. Más control sobre tu trabajo.**

> **Hecho para máquinas modestas.** La idea es que puedas usar tu entorno de agentes sin necesitar una computadora costosa: desde una laptop sencilla hasta una VM o un VPS económico. Aeril es ligero; el agente de IA que conectes puede consumir bastante más memoria. Consulta [qué plataformas hemos probado](#dónde-puede-correr).

## ¿Para qué sirve?

Imaginá que estás construyendo una aplicación y le pedís a un agente de IA que corrija un error. Después querés revisar los cambios, dejarle indicaciones y continuar mañana. En lugar de reconstruir a mano qué pasó, Aeril organiza ese trabajo para vos.

Con Aeril podés:

- **Mantener las tareas ordenadas.** Cada tarea tiene su propio historial y espacio de trabajo.
- **Ver qué cambió.** Revisá las modificaciones antes de integrarlas a tu proyecto.
- **Dar feedback y continuar.** Comentá los cambios y usá esas observaciones para el siguiente intento.
- **Retomar después de un cierre inesperado.** Aeril conserva el historial y los puntos de control verificados para que sepas dónde quedó el trabajo.
- **Evitar interferencias.** Supervisa los espacios de trabajo y comprueba que no se estén usando de forma incompatible.
- **Trabajar sin un entorno pesado.** La interfaz corre en la terminal y está diseñada para usar poca memoria.

Aeril **no es otro modelo de IA**: es el lugar desde donde organizás y supervisás agentes externos. Hermes es uno de los motores con los que se integra.

## Cómo se ve el flujo

```text
Abrís una tarea
      ↓
El agente trabaja en su espacio
      ↓
Revisás qué modificó
      ↓
Dejás comentarios o aprobás
      ↓
Continuás, guardás o integrás el trabajo
```

Tu proyecto sigue siendo un repositorio Git normal. Aeril no te obliga a cambiar de editor ni a adoptar un sistema de versiones propio.

## Empezar

**Estado actual: versión temprana para desarrolladores.** Todavía no ofrecemos un instalador universal ni prometemos una experiencia lista para cualquier máquina.

Necesitás Linux, Git, Rust/Cargo y un agente externo compatible si querés ejecutar tareas reales. Construí Aeril desde el repositorio:

```bash
git clone https://github.com/elliotsecops/aeril.git
cd aeril
cargo build --release
./target/release/aeril
```

Ese último comando abre la interfaz de terminal. Para registrar un proyecto Git desde la CLI:

```bash
./target/release/aeril thread new --repo /ruta/a/tu/proyecto --title "Corregir inicio de sesión"
./target/release/aeril thread list
```

Esto crea y lista tareas; **no inicia por sí solo un agente**. Para trabajar con un motor real, necesitás configurarlo por separado.

## ¿Dónde puede correr?

Aeril está diseñado para mantener bajo el consumo de recursos del **gestor**, independientemente de dónde ejecutes tus agentes.

| Entorno | Situación |
|---|---|
| Laptop o PC con Linux | Entorno de desarrollo y pruebas utilizado |
| VM Linux | Opción natural para instalar y probar; depende de la distribución y sus dependencias |
| VPS Linux económico | Objetivo de despliegue ligero, no una garantía para un plan de precio concreto |
| Raspberry Pi | Interesante como plataforma ligera; compatibilidad ARM no certificada |
| Android con Termux | Posibilidad futura; no probado ni soportado oficialmente |

**¿Un VPS de $5?** Esa es la clase de infraestructura económica a la que apuntamos, **no una promesa de que un agente de IA completo funcione dentro de ese presupuesto**. Los motores, modelos y servicios externos tienen sus propios requisitos y costes.

## ¿Qué funciona hoy?

Aeril ya incluye la interfaz de terminal, historial de tareas, revisión de cambios, puntos de control, recuperación y gestión de espacios de trabajo. Las pruebas automatizadas verificaron dos ejecuciones concurrentes con agentes simulados.

También se integró la gestión de perfiles Hermes, con controles para evitar el uso simultáneo incorrecto de una sesión. **Esta función permanece desactivada por defecto.**

Lo que **todavía no** estamos anunciando como listo:

- Dos sesiones reales y autenticadas de Hermes funcionando simultáneamente.
- Aislamiento total del resto de los archivos o credenciales de tu computadora.
- Restauración automática de la memoria conversacional de un agente.
- Compatibilidad certificada con Raspberry Pi, Termux o todos los proveedores de VPS.

Podés seguir los avances y límites conocidos en [HANDOFF.md](HANDOFF.md) y [el estado de Fase 1](docs/phase1-status.md).

## Nerd stuff 🤓

Los detalles de ingeniería viven aquí, no en la presentación del producto.

**Construcción:** Rust, interfaz de terminal con Ratatui, datos locales en SQLite y espacios de trabajo basados en Git. Los motores externos se comunican con Aeril mediante un protocolo estructurado; Hermes utiliza ACP.

**Mediciones del gestor Aeril** (build release, candidato P2-S1 R3 auditado):

| Medida | Resultado |
|---|---:|
| Tamaño del ejecutable | 5.8 MB |
| RAM en reposo | ~7 MB |
| RAM adicional al abrir un hilo | ~1 MiB |
| Arranque, p95 | ~20 ms |
| Suite de pruebas Rust | 204/204 PASS |

Son mediciones en un entorno específico, **no mínimos garantizados para cualquier hardware**. La RAM de Hermes o de cualquier otro agente externo **no está incluida** en el dato de RAM en reposo.

**Garantías y límites:** Aeril conserva evidencia de ejecuciones y cambios; los puntos de control recuperan archivos verificados, no pensamientos ni memoria privada de un modelo. Los espacios de trabajo separan tareas, pero no son sandboxes del sistema operativo. Los bloqueos coordinan procesos cooperativos y no impiden que un proceso malicioso con el mismo usuario altere archivos.

**Detalles para auditores y colaboradores:**

- [Manifiesto y presupuestos](MANIFIESTO.md)
- [Estado y pruebas de Fase 1](docs/phase1-status.md)
- [Implementación de perfiles Hermes P2-S1](docs/phase2-s1-implementation.md)
- [Remediación y evidencias de auditoría R3](docs/phase2-s1-r3-remediation.md)
- [Contrato ACP de Hermes](docs/acp-hermes-spec.md)
- [Estado actual y siguientes pasos](HANDOFF.md)

---

**Aeril. Una terminal más ligera para trabajar con agentes de IA.**
