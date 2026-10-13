# Especificación exacta del ACP que habla `hermes acp`

> Documento extraído del **código instalado**, no de la documentación. Cada afirmación cita
> archivo+líneas (o un ejemplo citado literalmente). Lo que no tiene evidencia está marcado
> **UNCERTAIN**. Las discrepancias con los apuntes de research están en §7.
>
> Verificado el 2026-10-13 contra:
> - Hermes instalado: `/home/elliot/.hermes/hermes-agent` (git install) — `hermes --version` →
>   `Hermes Agent v0.21.5+8877.g865ba90 (2026.9.24) · upstream 865ba906`; `hermes acp --version` → `0.21.5+8877.g865ba90`.
> - SDK cliente/servidor ACP: `agent-client-protocol==0.9.0` (pin en `pyproject.toml:415`), instalado en
>   `/home/elliot/.hermes/installs/95d75e3983ec2bca/environments/de82ddb1132e41e3a1530d53f80602e9/venv/lib/python3.14/site-packages/acp/`.
> - Fuentes de research: `hermes-acp.md`, `hermes-acp-internals.md`, `acp-session-setup.md` (dir
>   `/home/elliot/Documentos/ChatGPT/Aeril/.research/architecture-2026-10-13/`).
> - `hermes acp --check` → `Hermes ACP check OK`, exit 0 (ver §6).

**Alias de rutas usados en este documento**

| Alias | Ruta absoluta |
|---|---|
| `[agent]` | `/home/elliot/.hermes/hermes-agent` |
| `[acp]`  | `/home/elliot/.hermes/installs/95d75e3983ec2bca/environments/de82ddb1132e41e3a1530d53f80602e9/venv/lib/python3.14/site-packages/acp` |

El servidor Hermes vive en `[agent]/acp_adapter/` (`entry.py`, `server.py`, `session.py`, `events.py`,
`permissions.py`, `tools.py`, `edit_approval.py`, `content.py`, `commands.py`, `model_catalog.py`,
`provenance.py`, `auth.py`).

---

## 1. Transporte y framing

1. **stdio puro** (`stdin`/`stdout` del subproceso). Hermes no abre puerto ni socket.
   - `[agent]/acp_adapter/entry.py:218-220`: `agent = HermesACPAgent(); asyncio.run(acp.run_agent(agent, use_unstable_protocol=True))`.
   - `[acp]/core.py:60-72`: `run_agent` construye `stdio_streams(limit=stdio_buffer_limit_bytes)` y
     `AgentSideConnection(...)`; el default de buffer es **50 MiB** (`[acp]/core.py:32-35`).
2. **JSON-RPC 2.0 delimitado por salto de línea (newline-delimited JSON). NO es framing LSP, NO hay
   `Content-Length`.**
   - Escritura: `[acp]/task/sender.py:32-36`:
     `data = (json.dumps(payload, separators=(",", ":")) + "\n").encode("utf-8")` — JSON **compacto**
     (sin espacios), UTF-8, terminado en `\n`.
   - Lectura: `[acp]/connection.py:148-160`: bucle `line = await self._reader.readline()` y
     `json.loads(line)`.
   - Evidencia de cliente real en el repo: `[agent]/evals/acp_empty_session_wire.py:89-101` escribe
     `proc.stdin.write(json.dumps(request) + '\n')` y parsea una línea JSON por respuesta.
3. **stdout reservado al protocolo; logs a stderr.**
   - `[agent]/acp_adapter/entry.py:65-76` (`_setup_logging` fuerza `StreamHandler(sys.stderr)` y
     limpia handlers del root logger) y `entry.py:11-12` (comentario "stdout is reserved for ACP JSON-RPC").
   - `[agent]/acp_adapter/session.py:85-88` redirige prints del agente a stderr (`_acp_stderr_print`).
4. **Quién inicia requests**: en ACP el *cliente* llama métodos del agente; el *agente* solo puede
   mandar **notifications** (`session/update`) y **requests** al cliente (`session/request_permission`,
   `fs/*`, `terminal/*`). Hermes solo usa `session/update` y `session/request_permission`
   (grep en `[agent]/acp_adapter/`: no hay llamadas a `fs/read_text_file`, `fs/write_text_file` ni
   `terminal/*`; ver §2.6).
   - Métodos servidor→cliente definidos: `[acp]/meta.py:18-28` (`CLIENT_METHODS`).
5. **IDs JSON-RPC**:
   - Los requests del cliente los numera el cliente; la respuesta ecoa el mismo `id`
     (`[acp]/connection.py:197-216`).
   - Los requests **del servidor** (permission) usan un contador interno que **arranca en 0**:
     `[acp]/connection.py:80` (`self._next_request_id = 0`) y `:134-141`. Aeril no debe asumir que
     `id != 0`.
6. **Serialización (claves que se omiten)**:
   - Notificaciones: `model_dump(by_alias=True, exclude_none=True, exclude_defaults=True)`
     (`[acp]/utils.py:54-56` + `:123-125`). Campos `None`/default **desaparecen del JSON**.
   - Respuestas a requests: `model_dump(mode="json", by_alias=True, exclude_none=True, exclude_unset=True)`
     (`[acp]/connection.py:204-213`). Los campos no seteados explícitamente **no viajan**.
7. **Un mensaje = un request o una notification; nunca lotes JSON-RPC.**
   - `[acp]/connection.py:164-174`: si hay `method`+`id` → request; `method` sin `id` → notification;
   `id` sin `method` → respuesta.
8. Formato JSON de los campos: los modelos pydantic usan `alias` camelCase (p.ej. `sessionId`,
   `toolCallId`, `optionId`, `mcpServers`, `rawInput`, `updatedAt`); ver `[acp]/schema.py` en cada modelo.

---

## 2. Secuencia mínima para correr un prompt

Orden mínimo real (evidencia: `[agent]/evals/acp_empty_session_wire.py:108-116` y
`[agent]/tests/e2e/core/providers/test_native_copilot_acp_streaming.py:171-174`):

```text
client → initialize                     (request, id=N)
server → {result: InitializeResponse}    (mismo id)
client → session/new {cwd, mcpServers}   (request, id=N+1)
server → {result: {sessionId, models, modes, _meta}}
client → session/prompt {sessionId, prompt:[{type:"text",text}]}  (request, id=N+2)
server → session/update … (N notifications, sin id)
server → {result: {stopReason, usage?}}  (mismo id)
```

`authenticate` **no** es obligatorio para `session/new`/`session/prompt` (no hay gate en
`[agent]/acp_adapter/server.py`; `detect_provider()` solo se usa para anunciar auth y para el model
picker: `server.py:298-313,534,558`).

### 2.1 `initialize`

- Método: `"initialize"` (`[acp]/meta.py:4-5`).
- **Params** (modelo `InitializeRequest`, `[acp]/schema.py:1549-1589`):
  - `protocolVersion` (int, **requerido**, 0..65535, alias `protocolVersion`).
  - `clientCapabilities` (opcional; default `ClientCapabilities()` = `{fs:{readTextFile:false,writeTextFile:false}, terminal:false, auth:{terminal:false}}`; `[acp]/schema.py:1372-1410`).
  - `clientInfo` (opcional) — `Implementation`: `{name, title?, version}` (`[acp]/schema.py:386-425`).
  - `_meta` (opcional).
- **Result** (`InitializeResponse`, `[acp]/schema.py:2308-2359`), construido en
  `[agent]/acp_adapter/server.py:540-551`:
  - `protocolVersion`: **siempre `acp.PROTOCOL_VERSION` = 1** (`[acp]/meta.py:29`), ignorando lo que
    mandó el cliente (`server.py:535-541`; solo se loguea).
  - `agentInfo`: `{ "name": "hermes-agent", "version": get_version_info().base_version }`
    (hoy `"0.21.5"`; `server.py:542`).
  - `agentCapabilities` (`server.py:543-549`):
    - `loadSession: true`
    - `promptCapabilities: {"image": true}` (audio/embeddedContext no seteados → omitidos; default `false`)
    - `sessionCapabilities: {"fork": {}, "list": {}, "resume": {}}`
    - `mcpCapabilities` **omitido** (no seteado; default http=false, sse=false → `[acp]/schema.py:469-486,2098-2105`).
  - `authMethods` (`[agent]/acp_adapter/auth.py:30-50`; siempre ≥1):
    - Si hay credenciales resueltas: `{"id": "<provider>", "name": "<provider> runtime credentials", "description": "Authenticate Hermes using the currently configured <provider> runtime credentials."}` (`AuthMethodAgent`, sin campo `type`; `[acp]/schema.py:99-121`).
    - Siempre: `{"id": "hermes-setup", "name": "Configure Hermes provider", "type": "terminal", "args": ["--setup"], "description": "Open Hermes' interactive model/provider setup in a terminal. …"}` (`TerminalAuthMethod`; `[acp]/schema.py:1317-1318`).
- Ejemplo de `result` (armado a partir de `server.py:540-551` + `auth.py:41-50`; no es captura literal):

```json
{"jsonrpc":"2.0","id":1,"result":{
  "protocolVersion":1,
  "agentInfo":{"name":"hermes-agent","version":"0.21.5"},
  "agentCapabilities":{
    "loadSession":true,
    "promptCapabilities":{"image":true},
    "sessionCapabilities":{"fork":{},"list":{},"resume":{}}
  },
  "authMethods":[
    {"id":"nous","name":"nous runtime credentials","description":"Authenticate Hermes using the currently configured nous runtime credentials."},
    {"id":"hermes-setup","name":"Configure Hermes provider","type":"terminal","args":["--setup"],"description":"Open Hermes' interactive model/provider setup in a terminal. Use this when Hermes has not been configured on this machine yet."}
  ]
}}
```

### 2.2 `authenticate` (si el cliente lo usa)

- Método `"authenticate"`, params `{"methodId": "<id>"}` (`[acp]/schema.py:191-214`; `methodId` alias).
- Handler: `[agent]/acp_adapter/server.py:553-566`.
  - `methodId == "hermes-setup"` → `{}` **solo si hay credenciales**; si no, devuelve `None`.
  - `methodId == provider` detectado (lowercase, `.strip()`) → `{}`.
  - Cualquier otro → `None`.
  - `None` se serializa como `result: {}` (éxito vacío), **no** como error
    (`[acp]/agent/router.py:102-108` usa `normalize_result`; `[acp]/utils.py:59-65` convierte `None`→`{}`).
    Es decir: un `authenticate` fallido **no** es distinguible de uno exitoso por la forma del `result`.
- El `id` del provider sale de `hermes_cli.runtime_provider.resolve_runtime_provider()`
  (`[agent]/acp_adapter/auth.py:11-27`).

### 2.3 `session/new`

- Método `"session/new"` (`[acp]/meta.py:11`).
- **Params** (`NewSessionRequest`, `[acp]/schema.py:1645-1670`):
  - `cwd` (string, **requerido**, debe ser absoluto).
  - `mcpServers` (**requerido**, puede ser `[]`) — lista de:
    - stdio: `{"name","command","args":[...],"env":[{"name","value"}]}`
    - http: `{"type":"http","name","url","headers":[{"name","value"}]}`
    - sse: `{"type":"sse","name","url","headers":[...]}` — deprecado en MCP
    (`[acp]/schema.py:488-563,1637-1643`).
  - `_meta` opcional. Campos extra desconocidos se ignoran (pydantic) — **UNCERTAIN** solo si el
    cliente manda un tipo inválido en un campo conocido: eso produce `-32602` (§2.10).
- Hermes acepta los 3 transportes y los mapea a su registro MCP en
  `[agent]/acp_adapter/server.py:427-473`, `:170-173` (`_mcp_server_config`: stdio →
  `{command,args,env:{<name>:<value>}}`; http/sse → `{url,headers:{…}}`).
- **Result** (`NewSessionResponse`, `[acp]/schema.py:2848-2898`; construido en
  `[agent]/acp_adapter/server.py:616-621`):
  - `sessionId`: **UUID4 en texto plano** (`[agent]/acp_adapter/session.py:179`,
    `session_id = str(uuid.uuid4())`).
  - `models`: `{"availableModels":[{"modelId":"<provider>:<model>","name":"…","description":"…"}],"currentModelId":"<provider>:<model>"}`
    (`[agent]/acp_adapter/model_catalog.py:146-152,155-235`; `server.py:298-313`).
    Ids: `provider:model` o `custom:<slug>:<model>` (doc en `[agent]/website/docs/user-guide/features/acp.md:276-277`).
  - `modes`: `{"currentModeId":"default","availableModes":[{"id":"default","name":"Default","description":"Ask before edits."},{"id":"accept_edits",…},{"id":"dont_ask",…}]}`
    (`[agent]/acp_adapter/server.py:240-252,282-291`).
  - `_meta`: `{"hermes":{"sessionProvenance":{…}}}` opcional (`[agent]/acp_adapter/provenance.py:65-86`,
    `server.py:605-609`).
  - `configOptions` **omitido** (Hermes no lo setea).
- Efectos: construye un `AIAgent` con `platform="acp"`, toolsets resueltos por
  `platform_toolsets.acp` (default `hermes-acp`) y MCP de config
  (`[agent]/acp_adapter/session.py:103-108,494-581`). La construcción corre **fuera del event loop**
  (`asyncio.to_thread`, `server.py:617-619`).
- **Sesión vacía no se persiste**: no se crea fila durable hasta que haya historial
  (`[agent]/acp_adapter/session.py:356-362`; assert en `evals/acp_empty_session_wire.py:141`).
- Tras responder, Hermes agenda `available_commands_update` y `usage_update`
  (`server.py:603-604`, `commands.py:89-90`).

Ejemplo (params tal cual los manda un cliente; `evals/acp_empty_session_wire.py:111`):

```json
{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/home/user/project","mcpServers":[]}}
```

### 2.4 `session/prompt`

- Método `"session/prompt"` (`[acp]/meta.py:12`).
- **Params** (`PromptRequest`, `[acp]/schema.py:2488-2551`):
  - `sessionId` (string, requerido).
  - `prompt` (array de content blocks, requerido). El bloque de texto es exactamente
    `{"type":"text","text":"…"}` (`TextContentBlock`; `[acp]/schema.py:2060-2074,2248-2249`;
    helper `[acp]/helpers.py:83-84`).
    Otros bloques válidos: `{"type":"image","data":"<base64>","mimeType":"image/png","uri"?}`,
    `{"type":"resource_link","name","uri",…}`, `{"type":"resource","resource":{…}}`, `{"type":"audio",…}`
    (`[acp]/schema.py:2450-2469,2530-2543`).
  - `messageId` (opcional, UUID). Hermes **lo ignora**: la firma del handler es
    `prompt(self, prompt, session_id, **kwargs)` (`[agent]/acp_adapter/server.py:818`) y nunca
    devuelve `userMessageId` en la respuesta (`schema.py:1771-1777`).
- **Conversión**: `[agent]/acp_adapter/content.py:256-282` convierte text/image/resource_link/
  resource embebido a contenido OpenAI; **los bloques `audio` se descartan silenciosamente** (no hay
  rama para `AudioContentBlock`). El texto plano para comandos/eco sale de `_extract_text`
  (`content.py:231-233`).
- **Atajos tempranos** (todos responden `stopReason:"end_turn"`):
  - Prompt sin texto ni contenido útil → `{"stopReason":"end_turn"}` (`server.py:825-829`).
  - `/comando` text-only → se ejecuta el slash command y la respuesta va como `agent_message_chunk`
    (`server.py:833-844`).
  - Sesión ocupada → encola o redirige, manda `agent_message_chunk` con
    `"Queued for the next turn. (N queued)"` y responde `end_turn` (`server.py:732-752,846-850`).
  - `sessionId` desconocido → `{"stopReason":"refusal"}` (`server.py:820-823`).
- **Ejecución**: el turno corre en un `ThreadPoolExecutor(max_workers=4, thread_name_prefix="acp-agent")`
  con copia de ContextVars (`server.py:46,858-875`); el event loop solo transporta.
- **Result** (`PromptResponse`, `[acp]/schema.py:1730-1777`; `server.py:948-1006`):
  - `stopReason` ∈ `"end_turn" | "cancelled" | "refusal"` (`StopReason` completo en `[acp]/schema.py:14`: incluye `max_tokens`/`max_turn_requests`, que Hermes no emite).
  - `usage` (opcional; solo si el turno reportó tokens): `{"inputTokens","outputTokens","totalTokens","thoughtTokens"?,"cachedReadTokens"?}`
    (`server.py:998-1006`; `[acp]/schema.py:1124-1166`).
  - La respuesta se envía **después** de cerrar tool calls colgados y de emitir el texto final
    (`server.py:858-867,883`).

Ejemplo (forma exacta del bloque de texto):

```json
{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{
  "sessionId":"5b0c9a1e-2f0e-4a6c-9d5d-3d1c8a2b7e44",
  "prompt":[{"type":"text","text":"Arreglá el bug del parser"}]}}
```

### 2.5 Notificaciones `session/update` que emite Hermes (catálogo completo)

Envelope (siempre): `{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"<sid>","update":{…}}}`
(`[acp]/agent/connection.py:79-100`; `[acp]/schema.py:3125-3162`). `update.sessionUpdate`
discrimina la variante. **Hermes NO adjunta `_meta` a los updates** salvo `session_info_update`
(`server.py:415-425`; el resto pasa `field_meta=None`, p.ej. `permissions.py:60-64`).

Variantes realmente emitidas por el código de `[agent]/acp_adapter/` (grep exhaustivo §7-b):

| `sessionUpdate` | Struct | Campos que manda Hermes | Generado en |
|---|---|---|---|
| `agent_message_chunk` | `ContentChunk` | `content:{type:"text",text}`, `messageId` (UUID4) | `events.py:247-252,220-236`; `server.py:840,849,980-989` |
| `agent_thought_chunk` | `ContentChunk` | `content:{type:"text",text}`, `messageId` | `events.py:239-244` + `server.py:943-944` |
| `user_message_chunk` | `ContentChunk` | `content:{type:"text",text}` (sin `messageId`) | solo al drenar prompts encolados: `server.py:1017` |
| `tool_call` | `ToolCallStart` | `toolCallId`, `title`, `kind`, `status`, `content[]`, `locations[]`, `rawInput?` | `tools.py:780-814` |
| `tool_call_update` | `ToolCallProgress` | `toolCallId`, `kind`, `status`, `content[]`, `rawOutput?` | `tools.py:817-846`, `permissions.py:113` |
| `plan` | `AgentPlanUpdate` | `entries:[{content,priority,status}]` | `events.py:30-52,294-295` (resultado del tool `todo`) |
| `available_commands_update` | `_AvailableCommandsUpdate` | `availableCommands:[{name,description,input?}]` | `commands.py:73-90` |
| `session_info_update` | `_SessionInfoUpdate` | `title`, `updatedAt` (ISO-8601), `_meta?` | `server.py:398-425,803-808` |
| `usage_update` | `_UsageUpdate` | `size`, `used` | `server.py:367-379` |

Detalles de campos (esquema SDK):

- `ContentChunk` (`[acp]/schema.py:2450-2485`): `content` (bloque con discriminador `type`),
  `messageId?` (UUID; "a change in messageId indicates a new message has started").
- `ToolCallStart`/`ToolCall` (`[acp]/schema.py:2982-3038,3121-3122`): `toolCallId` **requerido**,
  `title` **requerido**; `kind` ∈ `read|edit|delete|move|search|execute|think|fetch|switch_mode|other`;
  `status` ∈ `pending|in_progress|completed|failed`; `content` ∈ `{type:"content",content:{…}}` |
  `{type:"diff",path,newText,oldText?}` | `{type:"terminal",terminalId}`; `locations:[{path,line?}]`;
  `rawInput`/`rawOutput` (alias `rawInput`/`rawOutput`).
- `ToolCallProgress` = `ToolCallUpdate` + `session_update` (`[acp]/schema.py:2691-2728,2974-2975`):
  todos los campos opcionales menos `toolCallId`.
- `plan` (`[acp]/schema.py:1700-1727,2389-2439`): entrada con `content`, `priority` ∈ `high|medium|low`
  (Hermes siempre `medium`, `events.py:51`), `status` ∈ `pending|in_progress|completed`
  (los `cancelled` se mapean a `completed` con prefijo `[cancelled]`, `events.py:27,49-51`);
  el cliente **reemplaza la lista entera** en cada update.
- `available_commands_update` (`[acp]/schema.py:2138-2182`): comandos que Hermes publica
  (`commands.py:48-70`): `help, model, tools, context, reset, compress, steer, queue, version`;
  `input` = `UnstructuredCommandInput{hint}` para `model`, `steer`, `queue`.
- `session_info_update` (`[acp]/schema.py:743-768`): `title`, `updatedAt`. Se emite cuando cambia el
  título (auto-titling: `server.py:803-808`) y cuando compresión rota el head interno
  (`server.py:960-969,398-425`), con `_meta.hermes.sessionProvenance`.
- `usage_update` (`[acp]/schema.py:1169-1187`): `{"size":<context window>,"used":<tokens estimados>}`;
  solo si `context_length > 0` (`server.py:366-379`). Se manda al final de cada turno y (agendado)
  tras `session/new`/`load`/`resume` (`server.py:603-604,1005`).

**Variantes del SDK que Hermes NO emite** (existen en la unión `SessionNotification` pero ningún
call-site de `[agent]/acp_adapter/` las construye): `current_mode_update` y `config_option_update`
(`[acp]/schema.py:2052-2057,2978-2979`; confirmado por grep §7-b).

**Agrupación de mensajes del asistente (importante para la UI):** Hermes asigna un `messageId`
UUID4 por mensaje y lo comparte entre deltas contiguos; el sentinel `None` del core (antes de
ejecutar tools / fin de stream) cierra el id y el próximo delta abre uno nuevo
(`events.py:185-236`). Si el turno ya streameó texto, la respuesta final **no** se re-emite —
salvo que un plugin la haya transformado (`server.py:971-989`).

Ejemplo de `agent_message_chunk` (reconstruido de `events.py:227-234`; nótese `messageId` en camelCase):

```json
{"jsonrpc":"2.0","method":"session/update","params":{
  "sessionId":"5b0c9a1e-2f0e-4a6c-9d5d-3d1c8a2b7e44",
  "update":{"sessionUpdate":"agent_message_chunk",
            "content":{"type":"text","text":"Listo, ya lo arreglé."},
            "messageId":"0f6a1c2e-8c4b-4b1e-a0d0-6f5f2b9d7a11"}}}
```

Ejemplo de `tool_call` de un `terminal` (reconstruido de `tools.py:760,780-814` +
`TOOL_KIND_MAP` `tools.py:18-32`; `locations` vacío para terminal):

```json
{"jsonrpc":"2.0","method":"session/update","params":{
  "sessionId":"5b0c9a1e-2f0e-4a6c-9d5d-3d1c8a2b7e44",
  "update":{"sessionUpdate":"tool_call",
            "toolCallId":"tc-9f3a1c2b7d40",
            "title":"terminal: echo hello",
            "kind":"execute",
            "status":null,
            "content":[{"type":"content","content":{"type":"text","text":"$ echo hello"}}],
            "locations":[]}}}
```

*(`status` viaja solo si está seteado: `acp.start_tool_call(...)` en `tools.py:811-814` no pasa
`status`, y `exclude_none`/`exclude_defaults` lo omiten; el ejemplo lo muestra como `null`
únicamente para marcar que NO existe en el wire.)*

Ejemplo de `tool_call_update` de completitud (`tools.py:817-834`; `rawOutput` sólo si no es "polished"
y el resultado no es JSON estructurado):

```json
{"jsonrpc":"2.0","method":"session/update","params":{
  "sessionId":"5b0c9a1e-2f0e-4a6c-9d5d-3d1c8a2b7e44",
  "update":{"sessionUpdate":"tool_call_update",
            "toolCallId":"tc-9f3a1c2b7d40",
            "kind":"execute",
            "status":"completed",
            "content":[{"type":"content","content":{"type":"text","text":"hello\n"}}]}}}
```

Ejemplo de `plan` (de un resultado del tool `todo`; `events.py:41-52`):

```json
{"jsonrpc":"2.0","method":"session/update","params":{
  "sessionId":"5b0c9a1e-2f0e-4a6c-9d5d-3d1c8a2b7e44",
  "update":{"sessionUpdate":"plan",
            "entries":[{"content":"Arreglar parser","priority":"medium","status":"in_progress"}]}}}
```

Ejemplo de `usage_update` (`server.py:379`):

```json
{"jsonrpc":"2.0","method":"session/update","params":{
  "sessionId":"5b0c9a1e-2f0e-4a6c-9d5d-3d1c8a2b7e44",
  "update":{"sessionUpdate":"usage_update","size":200000,"used":15321}}}
```

Cierre garantizado de tool calls: todo `tool_call` abierto termina en `completed` o `failed`;
`tool.completed` cierra con su propio resultado, el `step_callback` es solo fallback, y al final del
turno se marcan `failed` los que quedaron abiertos (`events.py:78-116,131-141`; `server.py:885-894`),
con texto `"This tool call ended without a result (blocked, denied, or interrupted)."`
(`tools.py:837-846`).

### 2.6 `session/request_permission` (el agente llama al cliente)

- **Dirección: servidor → cliente.** Es un *request* (tiene `id`) y espera respuesta.
  Método `"session/request_permission"` (`[acp]/meta.py:21`; emisor
  `[acp]/agent/connection.py:102-113`).
- **Params** (`RequestPermissionRequest`, `[acp]/schema.py:2901-2928`):
  - `sessionId` (string).
  - `toolCall` (**`ToolCallUpdate`**, no `ToolCallStart`): `toolCallId`, `title?`, `kind?`, `status?`,
    `content?`, `locations?`, `rawInput?` (`[acp]/schema.py:2691-2728`).
  - `options`: lista de `PermissionOption` = `{"optionId","name","kind"}` con
    `kind` ∈ `allow_once|allow_always|reject_once|reject_always` (`[acp]/schema.py:11,1673-1697`).
- **Quién llama y por qué** (dos orígenes en Hermes, ambos desde el hilo worker vía
  `asyncio.run_coroutine_threadsafe`, `permissions.py:87-90`):
  1. Aprobación de **comandos peligrosos de terminal** (`tools.approval.prompt_dangerous_approval` →
     callback instalado por turno en `server.py:780-785,927-929`).
     El `toolCall` sintético (`permissions.py:54-64`):
     `toolCallId = "perm-check-<N>"` (contador global desde 1), `title = "<description>: <command>"`
     (o solo el comando), `kind = "execute"`, `status = "pending"`,
     `content = [{"type":"content","content":{"type":"text","text":"<description>\n$ <command>"}}]`,
     `rawInput = {"command":…, "description":…}`.
  2. Aprobación de **edits** (`edit_approval.py:199-208`): `toolCallId = "edit-approval-<N>"`,
     `title = "Approve edit: <path>"`, `kind = "edit"`, `status = "pending"`,
     `content = [{"type":"diff","path":…,"oldText":…?,"newText":…}]`,
     `rawInput = {"tool":"write_file|patch","arguments":{…}}`.
- **Respuesta del cliente** (`RequestPermissionResponse`, `[acp]/schema.py:1825-1849,647-675`):
  - Permitir: `{"outcome":{"outcome":"selected","optionId":"allow_once"}}`.
  - Denegar: `{"outcome":{"outcome":"selected","optionId":"deny"}}` **o**
    `{"outcome":{"outcome":"cancelled"}}` (ambos → deny; `permissions.py:67-74`).
  - `optionId` desconocido para Hermes → se trata como `deny` con warning (`permissions.py:71-74`).
- **Después de la respuesta**, Hermes cierra el globo del `toolCall` con un `session/update`
  `tool_call_update` del mismo `toolCallId`: `status:"completed"` si la opción elegida empieza con
  `allow`, `status:"failed"` si no (`permissions.py:103-113`).
- **Timeout / fallo**: `resolve_permission_timeout(None)` usa `approvals.timeout` de `config.yaml`
  (**default 300 s**; `permissions.py:117-125`; `[agent]/tools/approval_context.py:247-262`).
  Al vencer: `future.cancel()` y el callback devuelve `"timeout"` (distinto de un deny explícito;
  `permissions.py:94-99,145-148`). Si el request no se puede agendar o falla → `"deny"`
  (`permissions.py:83,100-102,149-150`).
- Hermes **no** llama `fs/read_text_file`, `fs/write_text_file` ni `terminal/*` del cliente
  (grep §7-b): las capabilities `fs`/`terminal` del cliente se ignoran.
- Ejemplo del request (params; reconstruido de `permissions.py:59-64`):

```json
{"jsonrpc":"2.0","id":0,"method":"session/request_permission","params":{
  "sessionId":"5b0c9a1e-…",
  "toolCall":{"sessionUpdate":"tool_call_update","toolCallId":"perm-check-1",
              "title":"dangerous command: rm -rf build","kind":"execute","status":"pending",
              "content":[{"type":"content","content":{"type":"text","text":"dangerous command\n$ rm -rf build"}}],
              "rawInput":{"command":"rm -rf build","description":"dangerous command"}},
  "options":[{"optionId":"allow_once","kind":"allow_once","name":"Allow once"},
             {"optionId":"allow_session","kind":"allow_always","name":"Allow for session"},
             {"optionId":"allow_always","kind":"allow_always","name":"Allow always"},
             {"optionId":"deny","kind":"reject_once","name":"Deny"},
             {"optionId":"deny_always","kind":"reject_always","name":"Deny always"}]}}
```

### 2.7 `session/cancel`

- **Notification cliente → servidor** (sin `id`, sin respuesta). Método `"session/cancel"`,
  params `{"sessionId":"..."}` (`[acp]/meta.py:6`; `[acp]/schema.py:1329-1349`;
  ruta: `[acp]/agent/router.py:112`).
- Handler: `[agent]/acp_adapter/server.py:643-664` → setea el `cancel_event` de la sesión y
  `request_hard_interrupt(agent)`; si hay turno en curso guarda el texto como
  `interrupted_prompt_text`.
- El turno en curso responde `{"stopReason":"cancelled"}` (`server.py:972-1006`).
- Extra Hermes: el próximo prompt de texto (o `/steer`) puede re-enganchar el pedido cancelado como
  corrección (`server.py:201-211,708-730`).

### 2.8 `session/load` (reanudar) y replay

- Método `"session/load"` (`[acp]/meta.py:10`). **Params** (`LoadSessionRequest`,
  `[acp]/schema.py:2362-2386`): `sessionId`, `cwd`, `mcpServers` (**requerido**, puede ser `[]`).
- **Result** (`LoadSessionResponse`, `[acp]/schema.py:2805-2845`): `models?`, `modes?`, `_meta?`.
  **No incluye `sessionId`** (es el mismo que mandó el cliente).
- **Replay obligatorio ANTES de responder** (spec + implementación): `server.py:579-609`. Orden por
  mensaje persistido (`server.py:125-167`):
  1. `user` → `user_message_chunk` con `content:{type:"text",text}` (sin `messageId`).
  2. `assistant` → `agent_thought_chunk` (si hay `reasoning_content`/`reasoning`) y luego
     `agent_message_chunk`; después, por cada `tool_calls[i]`, un `tool_call` con
     `toolCallId` = id guardado (`id`/`call_id`/`tool_call_id`).
  3. `tool` → `tool_call_update` de completitud; si el tool fue `todo`, además `plan`.
  - Los chunks de replay **no llevan `messageId`** (`server.py:114-122`).
  - Resúmenes de compactación llevan `_meta: {"hermes":{"compactionSummary":true}}` (chunk entero)
    o `{"hermes":{"containsCompactionSummary":true}}` (`server.py:82-97,120-122`).
- Actualiza el `cwd` de la sesión (y lo persiste) y re-registra los MCP de la sesión
  (`server.py:626-631`; `session.py:252-266`).
- **Sesión desconocida → NO es error**: el handler devuelve `None` y el SDK serializa `{}` como
  `result` (`server.py:626-629`; `[acp]/agent/router.py:62-68`; `[acp]/utils.py:59-65`). El cliente
  verá un load "exitoso" sin historial.
- La restauración es desde `~/.hermes/state.db` compartida (source `'acp'`), recreando el `AIAgent`
  y reabriendo la fila si estaba `ended_at` (`session.py:446-490,276-302`). Si `source != 'acp'` →
  `None` (`session.py:456-457`).

### 2.9 Otros métodos

| Método | Params | Result / comportamiento | Evidencia |
|---|---|---|---|
| `session/resume` | `sessionId`, `cwd`, `mcpServers` | Igual que load **sin replay**; si la sesión no existe **crea una nueva con UUID nuevo y responde igual** (la respuesta no tiene `sessionId`, así que el cliente no puede descubrir el nuevo id) | `server.py:633-641`; `schema.py:2931-2971` |
| `session/fork` | `sessionId`, `cwd`, `mcpServers?` | `{sessionId: <uuid nuevo>, models, modes}`; deep-copy del historial | `server.py:666-678`; `schema.py:2754-2802`; `session.py:197-208` |
| `session/list` | `cursor?`, `cwd?` | `{sessions:[{sessionId,cwd,title?,updatedAt?}], nextCursor?}`; página fija de 50, `cursor` = último `session_id` (cursor desconocido → página vacía) | `server.py:680-704`; `schema.py:1611-1634,714-741` |
| `session/set_model` | `sessionId`, `modelId` | `{}`; `-32603` si hay turno/comando en curso; `-32602` (`invalid_params`) si ningún provider sirve el modelo | `server.py:1022-1058` |
| `session/set_mode` | `sessionId`, `modeId` | `{}`; modos `default|accept_edits|dont_ask` (modo desconocido → `default`) | `server.py:240-252,1060-1072` |
| `session/set_config_option` | `sessionId`, `configId`, `value` (+`type?`) | `{configOptions: []}`; `configId="edit_approval_policy"` mapea el valor a modo; otros ids se guardan | `server.py:1074-1093`; `schema.py:833-891,2664-2684` |
| `session/close` | — | **No implementado**: el router tiene la ruta pero el agente no tiene `close_session` → `-32601 method_not_found`; **tampoco se anuncia** en capabilities | `[acp]/agent/router.py:70-77`; `[acp]/router.py:61-72`; `server.py:543-549` |

### 2.10 Errores

Objeto de error: `{"code":<int>,"message":<str>,"data":<any|null>}`
(`[acp]/exceptions.py:45-46`; emisión `[acp]/connection.py:217-237`).

| Código | Cuándo | Evidencia |
|---|---|---|
| `-32700` | JSON inválido (parse error; en la práctica la línea se descarta con log) | `exceptions.py:16-18`; `connection.py:154-158` |
| `-32600` | request inválido | `exceptions.py:20-22` |
| `-32601` | método desconocido o no implementado; `data = {"method": "<name>"}`. También responde `-32601` a sondas de liveness `ping`/`health`/`healthcheck`, que Hermes **no** considera error (`entry.py:39-62`) | `exceptions.py:24-26`; `router.py:180-182` |
| `-32602` | params inválidos (pydantic `ValidationError` → `data={"errors":[...]}`); también modelo no servible en `session/set_model`: `data={"details":"…"}` | `connection.py:222-227`; `server.py:1041-1047` |
| `-32603` | excepción interna del handler (`data={"details":str(exc)}`); Hermes lo usa p.ej. sesión ocupada en `set_model` | `connection.py:228-237`; `server.py:1031-1034` |
| `-32000` | auth required (definido, Hermes no lo emite) | `exceptions.py:36-39` |
| `-32002` | resource not found (definido, Hermes no lo emite) | `exceptions.py:40-43` |

---

## 3. Cómo llegan las ediciones de archivos al cliente

1. **Kinds**: `write_file`, `patch`, `skill_manage` → `kind:"edit"` (`TOOL_KIND_MAP`,
   `tools.py:18-32`).
2. **Bloque de diff** = `FileEditToolCallContent` → exactamente
   `{"type":"diff","path":"<path>","newText":"<contenido nuevo>","oldText":"<contenido previo>"|omitido}`
   (`[acp]/schema.py:293-315,1077-1079`; builder `[acp]/helpers.py:133-134`).
   `oldText` puede faltar (archivo nuevo o no legible: `edit_approval.py:63-69,84`).
3. **Hay tres momentos distintos en que puede aparecer el diff:**

   | Momento | ¿Lleva diff? | Evidencia |
   |---|---|---|
   | `tool_call` (inicio) con política de auto-aprobación activa (`accept_edits`/`dont_ask`, ruta no sensible) | **Sí**: `content=[{type:"diff",…}]` | `events.py:162-175`; `tools.py:799-800`; `edit_approval.py:148-169` |
   | `tool_call` (inicio) con política `ask` / ruta sensible | No: texto explicativo (`"Preparing replace edit for <path>. Approval prompt shows the diff."` / `"Preparing write to <path>. …"`; para V4A `mode:"patch"` siempre texto) | `tools.py:754-760`; test `tests/acp_adapter/test_mcp_e2e.py:183-194` |
   | `session/request_permission` (aprobación de edit) | **Sí**: el `toolCall.content` trae el diff completo | `edit_approval.py:199-208` |
   | `tool_call_update` (fin) | **No** para `write_file`/`patch`: solo texto resumen (`"✅ write_file completed for \`path\`"`, `message`, `replacements`, `files_modified`) | `tools.py:477-492,675-694` |
   | `tool_call_update` (fin) de `skill_manage` | **Sí**: parsea el diff unificado del resultado y emite 1+ bloques `type:"diff"` | `tools.py:679-689,639-672` |
4. **`locations`**: solo en el `tool_call` inicial, y únicamente desde `arguments.path`
   (+ `line` = `arguments.offset` o `arguments.line`): `extract_locations`, `tools.py:849-853`.
   Los `tool_call_update` de Hermes **no** llevan `locations` ni `rawOutput` estructurado.
5. **`rawInput`**: en el inicio se manda solo para tools desconocidas-con-args (`tools.py:808-810`);
   en la aprobación de edit va `{tool, arguments}`. En completitud, `rawOutput` se omite para tools
   "polished" o resultados JSON (`tools.py:829-834`).
6. **Título**: `"<tool>: <preview>"` con el mismo preview del CLI (`build_tool_preview`, máx 80 chars;
   `tools.py:192-197`).
7. En el flujo interactivo, por tanto, **el diff que el editor debe mostrar en el prompt de permiso
   viene dentro de `session/request_permission.params.toolCall.content[].type=="diff"`**, y el
   `tool_call` inicial solo trae texto. Un cliente que quiera "follow-along" de archivos debe mirar
   `tool_call.locations` (path/line) y los bloques diff del permission request o del cierre de
   `skill_manage`.

---

## 4. Flujo de permisos (qué debe responder Aeril) y auto-approve

### 4.1 Opciones exactas que Hermes puede mandar

**A) Comandos peligrosos de terminal** — `permissions.py:33-51` (ids estables por caso):

| Caso | `options` (en orden) |
|---|---|
| `allow_permanent=True, allow_session=True, smart_denied=False` | `allow_once`("Allow once",kind `allow_once`) → `allow_session`("Allow for session", **kind `allow_always`**) → `allow_always`("Allow always",kind `allow_always`) → `deny`("Deny",kind `reject_once`) → `deny_always`("Deny always",kind `reject_always`) |
| `allow_permanent=False, allow_session=True` | `allow_once` → `allow_session` → `deny` → `deny_always` |
| `allow_session=False` **o** `smart_denied=True` | solo `allow_once` → `deny` |
| SDK sin kind `reject_always` (no es el caso con 0.9.0) | se omite `deny_always` (`permissions.py:49-50`; kind soportado en `schema.py:11`) |

> Nota: ACP **no tiene un kind session-scoped**; Hermes usa el `optionId` para semántica y el kind
> `allow_always` como "hint persistente" (`permissions.py:44-45`). **Aeril debe responder por
> `optionId`, nunca por `kind`.**

**B) Aprobación de edits** — `edit_approval.py:231-236`: exactamente
`allow_once`("Allow edit", kind `allow_once`) y `deny`("Deny", kind `reject_once`). Solo `allow_once`
se considera aprobación (`edit_approval.py:237-238`).

### 4.2 Mapeo a la semántica Hermes

- `permissions.py:17-19`: `allow_once→once`, `allow_session→session`, `allow_always→always`,
  `deny→deny`, `deny_always→deny`.
- `once` no persiste nada; `session` → `approve_session(session_key, key)` (cache en proceso,
  scope = `HERMES_SESSION_KEY`, que en ACP es el `sessionId` del ACP; `server.py:772-778`);
  `always` → `approve_permanent(key)` + `save_permanent_allowlist(...)` (persiste `command_allowlist`
  en el perfil) — `[agent]/tools/approval.py:381-391`.
- **`deny_always` NO escribe una lista de denegación persistente**: mapea a `"deny"` igual que `deny`
  (`permissions.py:18`). La etiqueta "Deny always" sobre-promete. *(Quirk.)*
- Un `outcome:"cancelled"` (DeniedOutcome) o cualquier `optionId` desconocido ⇒ `deny`
  (`permissions.py:67-74`).
- Timeout ⇒ `"timeout"` ⇒ el turno reporta "timed out without user response" en la herramienta, no
  "denied" (`permissions.py:145-148`; `[agent]/tools/approval.py:894-895`).
- La espera es bloqueante en el hilo del agente: el callback usa `future.result(timeout=…)`
  (`permissions.py:94-99`). El default es `approvals.timeout` (300 s; `approval_context.py:247-262`).

### 4.3 Auto-approve

- El auto-approve de Hermes en ACP **solo existe para edits**, y depende del "mode" de la sesión
  (`server.py:236-252`, `edit_approval.py:44-47,148-169`):

  | mode id | política | efecto |
  |---|---|---|
  | `default` | `ask` | siempre pide diff por permission (no auto-aprueba) |
  | `accept_edits` | `workspace_session` | auto-aprueba si **todos** los paths resuelven dentro de `cwd` o del temp dir; nunca para rutas sensibles (`.git`, `.ssh`, `.env`, `.env.local`, `.env.production`, `id_rsa`, `id_ed25519`) |
  | `dont_ask` | `session` | auto-aprueba todo edit no sensible |

  Cuando auto-aprueba, **no se manda** `session/request_permission`: el diff viaja en el
  `tool_call` inicial (`events.py:162-175`).
- Los comandos de terminal **no** tienen auto-approve por mode: siempre pasan por el callback
  (`server.py:780-785,927-929`). Puede haber auto-aprobación de Hermes/plugins antes del callback
  (p.ej. `command_allowlist`) — no rastreada aquí (UNCERTAIN en detalle, fuera del adaptador ACP).
- `approvals.mode: manual` no fuerza el prompt en un host que responda `allow_once` programáticamente;
  la doc lo afirma para Buzz (`hermes-acp.md:303-321`) — **no verificable en este checkout** (el
  bridge Buzz no está acá) → **UNCERTAIN**.

### 4.4 Qué debe hacer Aeril

1. Implementar el handler de **request** `session/request_permission` (responder con `id` y `result`).
2. Mostrar `toolCall.title` / `content` (text o diff) y las `options` **tal cual**, respondiendo
   `{"outcome":{"outcome":"selected","optionId":"<optionId>"}}`.
3. Para denegar: `selected`+`deny` (o `cancelled`). Para timeout propio: puede responder
   `cancelled` (equivale a deny) o dejar vencer el timeout de Hermes (300 s).
4. No esperar `session/update` con el mismo `toolCallId` de `perm-check-N` para construir el prompt de
   permiso: el contenido está en el **request**, y el `tool_call_update` de cierre llega **después**
   de responder (`permissions.py:103-113`).

---

## 5. Quirks

1. **Pin de versión**: `agent-client-protocol==0.9.0` exacto (`pyproject.toml:415`;
   `[tool.uv.exclude-newer-package] agent-client-protocol = false`, `pyproject.toml:681`).
   Ojo: el código del SDK está generado de `schema refs/tags/v0.11.2` (`[acp]/meta.py:1-2`); la
   versión PyPI (0.9.0) y el ref de schema (v0.11.2) no coinciden. `sorted()` de `session/update`
   incluye `usage_update`, `session_info_update`, etc.
2. **`prompt()` responde recién al final del turno.** El streaming va por notificaciones; el
   resultado final del request solo marca terminación (`server.py:883,948-1006`).
3. **Replay en `session/load`**: se emite **antes** de responder, con `user_message_chunk`/
   `agent_thought_chunk`/`agent_message_chunk`/`tool_call`/`tool_call_update`/`plan`; sin
   `messageId`; con `_meta.hermes.compactionSummary` en resúmenes (`server.py:125-167,579-609`).
   Un cliente que no espere notificaciones durante el `load` pierde el transcript.
4. **Sesión como hilo durable**: `sessionId` ACP = UUID4; internamente el `AIAgent` puede rotar su
   `session_id` por compresión y el ACP id sigue estable; la rotación se avisa con
   `session_info_update` + `_meta.hermes.sessionProvenance` (`server.py:385-425,960-969`;
   `provenance.py:65-77`). Al terminar el proceso se estampan `ended_at` en todas las sesiones
   (`entry.py:226-231`); un `load`/`resume` posterior las reabre (`session.py:459-466`).
5. **Claves de deduplicación**:
   - `messageId` (UUID4) agrupa deltas de un mismo mensaje del asistente; cambio de id = burbuja
     nueva (`events.py:185-236`).
   - `toolCallId` de tools = `tc-<12 hex>` (`tools.py:67-68`), y Hermes mantiene una **cola FIFO por
     nombre de tool** para cerrar la invocación correcta con llamadas homónimas en paralelo
     (`events.py:70-75,119-131`). Si Aeril ve dos `tool_call` con el mismo título, los ids son
     distintos: únicos por `toolCallId`.
   - `perm-check-<N>` y `edit-approval-<N>` son contadores **por proceso**, no por sesión
     (`permissions.py:21`; `edit_approval.py:42`).
6. **Timeouts internos**:
   - `_send_update`: el hilo worker espera hasta **5 s** a que el loop realmente despache cada
     notificación; si no, la descarta con log debug (`events.py:55-67`).
   - Permisos: `approvals.timeout` (300 s default) (`permissions.py:117-125`).
   - Concurrencia: 4 slots de executor compartidos entre sesiones (`server.py:46`).
   - MCP: discovery global en thread de fondo; `_make_agent` espera acotado (~1.5 s por
     `mcp_discovery_timeout`) y hay refresh tardío hasta 30 s (`entry.py:204-216`,
     `session.py:557-568`, `server.py:475-524`).
7. **`session/new` de sesión vacía no persiste** (probe de model discovery no crea fila)
   (`session.py:356-362`).
8. **Prompts concurrentes en la misma sesión**: no se solapan; se encolan o redirigen
   (`server.py:732-752,1008-1018`). `set_model` en sesión ocupada ⇒ `-32603`.
9. **`session/resume` de un id desconocido crea sesión nueva silenciosamente y no informa el id**
   (`server.py:636-641`; `ResumeSessionResponse` no tiene `sessionId`).
10. **Nada de `session/close`** (`-32601`).
11. **Sondas**: `ping`/`health`/`healthcheck` reciben `-32601` (sirven como "vivo"), y Hermes filtra
    el ruido de logs (`entry.py:39-62`).
12. **La sesión de aprobación "session" se llavea por ACP session id** vía `HERMES_SESSION_KEY`
    (`server.py:772-778`) — una nueva sesión ACP (o reiniciar el editor) re-pregunta.
13. **Edit approval es pre-ejecución y deny-by-default**: si el diff no se puede construir (p.ej.
    `old_string` inexistente en un `patch` replace), la llamada se bloquea con
    `{"error":"Edit approval denied: could not prepare diff (…)"}` (`edit_approval.py:185-196`).
14. **Audios descartados**: bloques `audio` en el prompt no llegan al modelo (`content.py:261-273`).
15. **`session/set_model` persiste**: reconstruye el agente y guarda (`server.py:349-358`); el cambio
    es por sesión, no global.
16. `available_commands_update` y `usage_update` se agendan **después** de responder nuevas/cargadas
    (`server.py:603-604`), pueden llegar antes del primer prompt.

---

## 6. Requisitos para arrancar el server

1. **Comandos** (cualquiera arranca el mismo server):
   - `hermes acp` → `hermes_cli/main_agent_cmds.py:80-91` → `acp_adapter.entry.main([...])`.
   - `hermes-acp` → script que termina en `from acp_adapter.entry import main; sys.exit(main())`.
   - `python -m acp_adapter` (`[agent]/acp_adapter/__main__.py`, `entry.py:8`).
2. **Dependencia**: extra `[acp]` (`pyproject.toml:415`). Sin ella: mensaje en stderr
   "The ACP server can't start: its protocol packages are missing…" + exit 1
   (`hermes_cli/main_agent_cmds.py:85-91`).
3. **Flags** (`entry.py:94-104`; parser en `hermes_cli/subcommands/acp.py:10-34`):
   `--version` (imprime `derived_version`, hoy `0.21.5+8877.g865ba90`, exit 0),
   `--check` (verifica `import acp` + `HermesACPAgent`; imprime `Hermes ACP check OK`; exit 0),
   `--setup` (corre `hermes model` interactivo para terminal auth; ofrece instalar browser tools
   solo si hay TTY: `entry.py:120-140`), `--setup-browser [--yes]`.
   - **`--check` verificado en esta máquina: `Hermes ACP check OK`, exit 0.**
4. **No requiere TTY** para operar: el bucle ACP es no interactivo; stdout/stdin se usan como pipes.
   Lo único que consulta TTY es el prompt opcional de browser tras `--setup` (`entry.py:132-140`).
5. **Entorno/credenciales**:
   - Carga `~/.hermes/.env` (HERMES_HOME) con `load_hermes_dotenv` (`entry.py:81-91,174-175`).
   - Provider/credenciales salen del resolver normal de Hermes (mismos que `hermes` CLI):
     `resolve_runtime_provider()` (`[agent]/acp_adapter/session.py:538-551`; `auth.py:11-27`).
     En esta máquina: `model.provider: "nous"` + `hermes auth add nous` (OAuth) — § respuesta final.
   - `HERMES_ACP_SKIP_CONFIGURED_MCP=1` (exacto, `.strip() != "1"`): salta el arranque de los MCP
     **globales** de `config.yaml` antes del loop; los MCP enviados en `session/new` **sí** se
     registran (`entry.py:210-216`). Cualquier otro valor mantiene el default.
   - Windows: importa el memory provider en el hilo principal antes de los threads
     (`entry.py:195-202`) y usa un feeder de stdin por thread (`[acp]/stdio.py:51-65,99-111`).
6. **Herramientas**: `platform_toolsets.acp` (default `hermes-acp`) + `agent.disabled_toolsets`;
   MCP extra por sesión siempre se agrega (`session.py:103-108,517-529`).
7. **Logs**: a stderr; stdout solo ACP (`entry.py:65-76`). En caso de crash: `logger.exception` y
   `sys.exit(1)` (`entry.py:223-225`).

---

## 7. Discrepancias research ↔ código instalado

(a) **`hermes-acp-internals.md`** (`/home/elliot/Documentos/ChatGPT/Aeril/.research/architecture-2026-10-13/`,
líneas 96-107): mapeo de permisos "`allow_once`→once, `allow_always`→always, reject→deny". El código
instalado además soporta **`allow_session`→session** y **`deny_always`→deny**
(`[agent]/acp_adapter/permissions.py:17-19,42-50`). La doc está incompleta.

(b) **`hermes-acp-internals.md:80-82`**: dice que `thinking_callback` es `None` y el razonamiento se
reenvía por `step_callback`. Instalado: `agent.thinking_callback = None` pero el razonamiento va por
**`agent.reasoning_callback`** (`server.py:943-944`), que emite `agent_thought_chunk`
(`events.py:239-244`).

(c) **`hermes-acp-internals.md:200-205`** ("non-text prompt blocks are currently ignored for request
text extraction"): instalado convierte image/resource_link/resource a contenido multimodal
(`content.py:256-282`) y anuncia `promptCapabilities.image=true` (`server.py:545`). Solo los bloques
`audio` se descartan. La limitación quedó desactualizada salvo por audio.

(d) **`hermes-acp.md` (user guide) :388-397** resume las opciones de aprobación como "allow once /
allow always / deny". El wire real incluye `allow_session` y `deny_always` (`permissions.py:42-50`),
y la aprobación de **edits** ofrece únicamente `allow_once`/`deny` (`edit_approval.py:233-234`).

(e) **`acp-session-setup.md`** es la spec genérica del protocolo (ACP v1); Hermes la cumple en lo
estructural, con extras: `session/new` devuelve `models`/`modes` (permitidos por el esquema como
unstable) y los `_meta` de procedencia (`server.py:605-609`). No hay contradicción, sino campos
adicionales que la spec no documenta.

(f) Los apuntes no mencionan `session_info_update`, `usage_update` ni `available_commands_update`
(fuera del listado de la user guide); sí existen en el wire (ver §2.5).

(g) La instalación documentada en `hermes-acp.md:63-67` (`pm.sync_venv(['acp'])`) coincide con la
realidad: en esta máquina el paquete está en un venv gestionado y `hermes acp --check` pasa.

---

## 8. Incógnitas (UNCERTAIN)

1. Timings de entrega/orden **observados en vivo** (no hice una captura de wire de un prompt real;
   los ejemplos JSON están reconstruidos de los constructores). Framing sí está verificado en
   código y en el harness `evals/acp_empty_session_wire.py`.
2. Comportamiento de hosts concretos (Zed/Buzz/VS Code): p.ej. si responden permisos
   automáticamente con `allow_once` (afirmado para Buzz en la doc, `hermes-acp.md:306-321`).
   No verificable en este checkout.
3. Auto-aprobaciones de Hermes **antes** del callback ACP (allowlist de config, heurísticas "smart")
   para comandos: existen en `tools/approval.py` pero su interacción exacta por turno ACP no la
   tracé línea a línea.
4. Si algún cliente espera `userMessageId` en `PromptResponse`: Hermes nunca lo setea
   (`server.py:948-1006`); compatible con el esquema (opcional), pero un cliente que lo exija fallará.
5. Semántica de `deny_always` como bloqueo permanente: el SDK define el kind `reject_always` pero
   Hermes lo mapea a `"deny"` (no crea lista de denegación). Si una versión futura del SDK/adaptador
   cambia el mapeo, revisar `permissions.py:17-19`.
