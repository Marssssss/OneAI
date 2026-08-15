# OneAI CLI Subcommand Reference

An overview of the subcommands of `oneai-cli` (bin `oneai`; with no subcommand it launches the TUI). All defined in `examples/cli/src/main.rs` via clap derive; run `oneai --help` or `oneai <sub> --help` for the full args of any subcommand.

> In-conversation slash commands (`/tools`, `/skills`, `/wf` …) — see [README Quick start → slash commands](../README_EN.md#2-tui--cli-general-agentic-execution).

## Sessions and inference

```bash
oneai                                  # launch the interactive TUI (default)
oneai chat [--domain coding] [--model gpt-4o] [--user <id>]   # launch the TUI (explicit)
oneai run "<prompt>" [--domain coding] [--model ...] [--user <id>]  # non-interactive single-shot, stdout
oneai version                          # version info
oneai init [--format oneai|agents|claude] [--path <dir>] [--force] [--no-llm]  # generate ONEAI.md/AGENTS.md/CLAUDE.md
```

## DomainPack (domain config pack)

```bash
oneai pack show <name>                 # show pack details
oneai pack install <path|git-url>      # install from local path or git
oneai pack validate spec.toml         # validate against JSON Schema (structure + semantics)
oneai pack check <name>               # check an installed pack against the spec
oneai pack containerized              # enable the containerized CodingPack (VM/container as boundary; same-named tools share one backend)
```

Mechanism: [domain-pack-mechanism_EN.md](domain-pack-mechanism_EN.md).

## Skill and Curator (skill lifecycle)

```bash
oneai skill show <name>                # show skill details
oneai curator pin <name>               # pin (exempt from auto-retirement)
oneai curator unpin <name>             # unpin
oneai curator archive <name>           # manually archive (reversible)
oneai curator restore <name>           # restore an archived skill
oneai curator rollback <id>            # restore skill + metadata from a snapshot
```

Skills are discovered from `.claude/.agents/.opencode/.oneai skills` convention dirs; Curator never deletes, only archives + rollbackable. Mechanism: [CLAUDE.md — Skill](../CLAUDE.md).

## Eval framework

```bash
oneai eval run <suite> [--format markdown|json|compact] [--profile] [--record <path>]  # run a suite (--profile emits the efficiency axis, --record records a trace)
oneai eval score <suite>               # run metrics only (no agent execution)
oneai eval replay <path>               # ghost-replay a recorded trace, verify determinism
oneai eval swebench --dataset ./swe_bench_lite.jsonl [--instances <ids>] [--limit N] [--modal]  # SWE-bench three-axis eval
oneai eval memory --suite <jsonl> [--data <file>] [--metrics recall_at_k,f1,bleu1] [--no-embedding] [--k 5] [--format markdown]  # memory eval (LongMemEval 5 capabilities)
```

Mechanism: [eval-mechanism_EN.md](eval-mechanism_EN.md).

## Workflow and state graph

```bash
oneai workflow list [--domain coding]  # list DAG workflows + state graphs
oneai workflow show <name>             # ASCII-render a workflow DAG + steps
oneai workflow run <name> [task] [--domain ...] [--model ...] [--user <id>]  # end-to-end run a DAG workflow
oneai graph list [--domain coding]     # list state graphs (react/plan/reflect/explore)
oneai graph show <name>                # ASCII-render a state graph
oneai graph run <name> <task> [--domain ...] [--model ...] [--user <id>]   # run a state graph with a real provider
```

Mechanism: [workflow-mechanism_EN.md](workflow-mechanism_EN.md).

## Multi-agent collaboration

Inside the main loop, model-driven `delegate` meta-tool does hierarchical sub-agent delegation (multi-delegate per turn + dependency-aware parallel-wave scheduling); `switch_paradigm` switches Plan/Reflect/Explore fixed graph flows; the engine-level GroupChat primitive drives scenario-based multi-role chat. No separate Team/Swarm/Handoff orchestration layer — aggregation/routing/debate patterns are expressed by `delegate` + a deterministic StateGraph. Mechanism: [multi-agent-mechanism_EN.md](multi-agent-mechanism_EN.md).

## Provider pool and smart routing

```bash
oneai provider status                  # provider-pool status: active providers, health, circuit breaking
oneai provider fallback-log [--limit 20]  # recent degradation events
oneai provider test                     # connectivity-check all providers in the pool
oneai provider route "<task>" [--strategy balanced|cost|latency|quality]  # routing decision dry-run
oneai provider route-log [--limit 10]  # recent routing decisions + reasons
oneai provider route-config             # current routing strategy & config
```

Mechanism: [provider-mechanism_EN.md](provider-mechanism_EN.md).

## Token counting and context management

```bash
oneai token count "<text>" [--model ...]  # count tokens
oneai token estimate [--model ...]      # estimate tokens for a sample conversation
oneai token context <model>            # view a model's context-window profile
oneai token models                      # list known tokenizer profiles
oneai token fits "<text>" --model <model> # check whether text fits the context window
oneai token probe [--model ...]        # probe the provider's model-metadata endpoint (L2), show 3-layer resolution
```

Mechanism: [context-management-mechanism_EN.md](context-management-mechanism_EN.md).

## Usage records (token only, no USD)

```bash
oneai usage session <id>               # per-session usage details
oneai usage export [--format json|csv]  # export usage records
```

Mechanism: [persistence-mechanism_EN.md](persistence-mechanism_EN.md).

## Memory (cross-session persistent facts)

```bash
oneai memory search <kw> [--user <id>] [--top_k 10]  # keyword/semantic search of persistent facts
oneai memory list [--user <id>] [--session <id>]      # list facts for a user/session
```

Mechanism: [memory-mechanism_EN.md](memory-mechanism_EN.md).

## Persistent sessions (SQLite)

```bash
oneai session list                     # list saved sessions
oneai session resume <id>             # preview a session's conversation history (print-only; live continuation via tasks continue)
oneai session delete <id>             # delete a session
oneai session info <id>               # session details
oneai session decay                    # run memory decay (evict by salience → archive)
oneai session export-hf <id>          # export as OpenAI messages JSONL (with redaction + optional working-state events)
```

Mechanism: [persistence-mechanism_EN.md](persistence-mechanism_EN.md).

## Working state (cross-session task continuation)

```bash
oneai tasks list                       # list unfinished tasks (reads index.json)
oneai tasks show <id>                  # view a task's goal/steps/decisions/blockers
oneai tasks continue <id>             # bind this task to a new session, derive into memory, continue
oneai tasks archive <id>              # archive a task (gzip the event log)
```

Mechanism: [working-state-mechanism_EN.md](working-state-mechanism_EN.md).

## Cron (scheduled tasks)

```bash
oneai cron add --name <n> --schedule "30m|every 2h|ISO|0 9 * * *" --task "<prompt>" [--platform loopback] [--channel <ch>] [--session <id>] [--pack coding] [--deliver origin|silent]
oneai cron list                        # list scheduled jobs
oneai cron rm <id>                     # remove a job
oneai cron fire <id>                   # fire manually (force, bypasses the due window but no double-fire)
oneai cron serve [--cron-bind 0.0.0.0:9091] [--gateway-bind 0.0.0.0:9090] [--domain ...] [--model ...] [--user <id>]  # start the orchestrator + external /cron/fire receiver
```

Mechanism: [scheduler-mechanism_EN.md](scheduler-mechanism_EN.md) (Scheduler).

## Terminal (terminal backends)

```bash
oneai terminal list                    # list available backends (local / docker / modal / daytona)
oneai terminal exec --backend <name> "<command>" [--timeout 120] [--max-output 100000]  # one-off command
oneai terminal snapshot --backend <name>   # snapshot session state (returns a restorable id)
oneai terminal restore --backend <name> --id <id>  # restore from a snapshot
oneai terminal cleanup --backend <name> [--hibernate]  # tear down (--hibernate stops+keeps restorable; otherwise destroys)
```

Mechanism: [CLAUDE.md — TerminalBackend](../CLAUDE.md) (the ShellTool execution backend).

## Embedding service

```bash
oneai embed generate "<text>" [--model ...] [--provider auto|openai|voyage|ollama|fastembed|openai-compat] [--api-key ...]
oneai embed batch "t1,t2" [same opts]   # batch generate
oneai embed list                       # list available providers + the auto-probe chain
oneai embed health [same opts]          # check embedding-service health
oneai embed dimension [same opts]       # view the model's vector dimension
```

Mechanism: [rag-mechanism_EN.md](rag-mechanism_EN.md).

## WASM sandbox

```bash
oneai wasm load <name> <file.wasm>     # load a module
oneai wasm run <name> [--input <json> | --input-file <path>]  # run a module
oneai wasm health [--name <name>]      # module health check
oneai wasm unload <name>              # unload a module
```

(Plus `oneai wasm stats` for resource-monitoring stats.) Mechanism: [wasm-mechanism_EN.md](wasm-mechanism_EN.md) (WASM).

## MCP (client and server)

```bash
oneai mcp serve [--domain coding]       # run as an MCP server (compatible with Claude Code/Cursor)
oneai mcp list                          # list configured MCP servers
oneai mcp add <name> --transport stdio|sse|streamable_http [--command ...] [--url ...] [--args ...] [--enabled] [--lazy]
oneai mcp remove <name>                # remove an MCP server
oneai mcp connect <name>              # test a connection and show discovered tools
```

`--lazy` (Stage 5) makes the server skip connecting at startup — it's triggered on demand via `tool_search` by `McpLazyConnectTool`; after connecting, the real tools surface to the model and the trigger vanishes. HTTP-transport servers run the full OAuth 2.0 PKCE flow (`--manual` switches to manual code paste, SSH/headless-friendly; tokens persist at `~/.oneai/mcp_oauth/<server>.json`, auto-refresh on 401). A server asking the user back mid-`tools/call` goes through elicitation, via the `InteractionGate::McpElicitation` point. Tools are registered namespaced as `mcp__<server>__<tool>`; each server can carry `McpToolPermissions` setting `PermissionLevel`/`ToolExposure`. Mechanism: [mcp-mechanism_EN.md](mcp-mechanism_EN.md) (MCP).

## A2A (Agent-to-Agent protocol)

```bash
oneai a2a serve [--domain coding] [--port 8080]  # start an A2A server, expose OneAI agent capabilities
oneai a2a discover <url>               # discover a remote A2A agent's capabilities
oneai a2a list                         # list configured A2A endpoints
oneai a2a send <url> "<task message>"  # send a task to a remote A2A agent
```

Mechanism: [a2a-mechanism_EN.md](a2a-mechanism_EN.md) (A2A).

## Gateway (message gateway)

```bash
oneai gateway serve [--bind 0.0.0.0:9090] [--domain ...] [--model ...] [--user <id>]  # start the webhook server (Feishu/WeChat/loopback)
oneai gateway channels                 # list bound channels (platform → session id)
oneai gateway autostart {install|uninstall|status}  # manage the macOS LaunchAgent (auto-starts supervisor+gateway at login)
```

Mechanism: [gateway-mechanism_EN.md](gateway-mechanism_EN.md) (Gateway).

## Supervisor (headless daemon)

```bash
oneai supervisor serve [--socket <path>] [--domain ...] [--model ...] [--user <id>] [--with-gateway] [--gateway-bind ...]  # start the daemon
oneai supervisor list [--socket <path>]  # list supervised instances
oneai supervisor spawn <id> [--domain ...] [--model ...] [--user <id>] [--socket <path>]  # spawn a new instance
oneai supervisor stop <id> [--socket <path>]  # stop an instance
oneai supervisor status <id> [--socket <path>]  # query an instance's status
oneai supervisor rpc <id> "<json>" [--socket <path>]   # one-shot RPC
oneai supervisor rpc-stream <id> [--socket <path>]     # streaming RPC
```

Mechanism: [supervisor-mechanism_EN.md](supervisor-mechanism_EN.md) (Supervisor).

## Serve (engine-bus sidecar)

```bash
oneai serve [--socket ~/.oneai/serve.sock] [--domain ...] [--model ...] [--user <id>]  # launch the engine-bus sidecar
```

Exposes an `AppSession` over the unified engine bus to **out-of-process frontends** (native apps / IDE plugins): write `Directive` JSON lines and read `EngineYield` JSON lines over the socket. UDS (Unix) / named pipe (Windows). Differs from `oneai supervisor serve`: the supervisor is an instance-registry RPC (request/response `spawn/list/stop`); `serve` is a bidirectional concurrent bus (arbitrary-time directive ↔ arbitrary-time yield + approval `request_id` correlation), on a separate socket so both coexist. Mechanism: [bus-mechanism_EN.md](bus-mechanism_EN.md) (engine bus).

## App-Server (JSON-RPC frontend server)

```bash
# Bind multiple transports concurrently; no --listen defaults to ipc://~/.oneai/app-server.sock
oneai app-server --listen stdio --listen ipc://~/.oneai/app-server.sock --listen ws://127.0.0.1:8787
```

The **JSON-RPC 2.0 upgrade** of `oneai serve` (the newline-JSON passthrough above) — same engine + same bus, but the wire speaks a frontend-facing, operation-oriented JSON-RPC schema (`turn/run` has a return value, `session/*`/`group/*`/`scenario/*` CRUD, `approval/respond`, `event` notifications), feeding four **non-Rust frontend** classes: the VS Code extension (`--listen stdio`, spawned on activation), the browser extension (`--listen native-messaging`, 4B-LE length-prefixed framing), and the macOS/Windows desktop sidecar (`--listen ipc://<ephemeral>`, `EngineProcessManager` auto-spawn). **The user never starts a server manually** — any frontend that can spawn a process owns the spawn (Codex-style). `--listen` values:

| value | use |
|---|---|
| `stdio` | IDE extension spawn (LSP-style); stdout is the framed message stream |
| `ipc://<path>` | Unix UDS / Windows named pipe; desktop sidecar |
| `ws://<host>:<port>` | browser / web client |
| `native-messaging` | browser extension (Chrome/Firefox; 4B-LE length-prefix framing) |

The full JSON-RPC method table (`turn/run`·`approval/respond`·`session/*`·`group/*`·`scenario/*`·`shutdown`), all `event` notification `params.kind` variants, four-frontend access status, and auto-spawn details are in [app-server-mechanism_EN.md](app-server-mechanism_EN.md) (§4 schema, §7 frontend-access status, §11 auto-spawn).

## Evolve (self-evolution)

```bash
oneai evolve run --seed <pack.yaml> --suite <name> [--max-generations 3] [--target 0.85] [--patience 2]   # run a generation / multi-gen loop
oneai evolve report ~/.oneai/evolve/run-<ts>    # inspect artifacts offline
oneai evolve diff  ~/.oneai/evolve/run-<ts>    # seed vs frontier config diff
oneai evolve lesson ~/.oneai/evolve/run-<ts>   # cross-generation lesson log
oneai evolve step  ~/.oneai/evolve/run-<ts> --suite <name>   # resume one generation
```

A GEPA-style outer evolution loop — no model weight updates, only mutation over the `DomainPackConfig` (7-layer pack) + `AgentLoopConfig` text/numeric knob space; each generation is scored by a real eval suite, Pareto multi-objective selects the frontier, lessons merge to carry the frontier forward. Three safety gates (`DomainPackValidator` + PermissionResolver static gate + judge/candidate separation) + two regression gates (held-out overfitting check + replay determinism-drift check). Mechanism: [self-evolution-mechanism_EN.md](self-evolution-mechanism_EN.md) (self-evolution).

## Web UI

```bash
oneai studio [--port 3000] [--domain coding] [--model ...] [--user <id>]  # launch the Studio Web UI (StateGraph visualization + Checkpoint time-travel)
```

Mechanism: [studio-mechanism_EN.md](studio-mechanism_EN.md) (Studio).

## Config

```bash
oneai config show                      # show current config
oneai config init                      # create a default config file
```

## Reload (hot reload)

```bash
oneai reload [--domain ...] [--model ...] [--user <id>]  # re-read the data layer without a restart (discovered skills, MCP tool registrations)
```

Mechanism: [CLAUDE.md — DataLayerReloader](../CLAUDE.md).
