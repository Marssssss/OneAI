# OneAI Extension Mechanism (A2A / WASM / Studio / MCP / Scheduler / Gateway / Supervisor)

> A set of extension crates that expose agent capabilities outward, sandbox inward, visualize, and schedule.

## Responsibility

Beyond the agent core, a set of "outward-facing" extensions: protocol interop (A2A, MCP), untrusted-code sandbox (WASM), visualization & time-travel (Studio), scheduled & persistent (Scheduler / Supervisor), message intake (Gateway).

## Subsystems

- **A2A** (`oneai-a2a`) — Agent-to-Agent protocol SDK: client + axum JSON-RPC server host + DomainPack→AgentCard auto-exposure. Shared-secret Bearer (`ONEAI_A2A_SECRET`).
- **WASM** (`oneai-wasm`) — Wasmtime sandbox for untrusted code: `WasmTool` / `WasmModuleRegistry` / resource monitor / WASI restricted / Native↔Wasm execution modes.
- **Studio** (`oneai-studio`) — axum HTTP+WS + REST API + live event push + D3.js StateGraph visualization + Checkpoint time-travel.
- **MCP** (`oneai-mcp`) — `McpServerHost` (JSON-RPC server, exposing OneAI tools to Claude Code/Cursor) + `McpPluginRegistry` (discover/configure/connect) + TOML config + stdio transport.
- **Scheduler** (`oneai-scheduler`) — in-memory task scheduling; `Schedule` four dialects (`30m` / `every` / ISO / 5-field cron) + `JobStore` (CAS at-most-once).
- **Gateway** (`oneai-gateway`) — message gateway: axum webhook + adapters (Feishu sha256+AES / WeChat sha1 / Loopback), per-channel lazy App, streaming coalescer.
- **Supervisor** (`oneai-supervisor`) — headless supervisor daemon: persistent instances + crash recovery (Running→Crashed) + newline-delimited JSON protocol + Unix/named-pipe/in-memory IPC.

## Key types & files

| Subsystem | Key files |
|---|---|
| A2A | `crates/oneai-a2a/src/{client,server,router,handler,card,task_store}.rs` |
| WASM | `crates/oneai-wasm/src/{runtime,module,registry,monitor,guest_api,action_template}.rs` |
| Studio | `crates/oneai-studio/src/{server,routes,handlers,ws,graph_dto,checkpoint_dto}.rs` |
| MCP | `crates/oneai-mcp/src/{server,client,router,plugin,discovery,config}.rs` |
| Scheduler | `crates/oneai-scheduler/src/{scheduler,store,runner,orchestrator,job}.rs` |
| Gateway | `crates/oneai-gateway/src/{gateway,directory,profile,runner}.rs` + `adapters/{feishu,wechat,loopback}.rs` |
| Supervisor | `crates/oneai-supervisor/src/{supervisor,registry,runner,transport,protocol,client}.rs` |

## Related CLI

[`a2a serve/discover/list/send`](cli-reference_EN.md#a2a-agent-to-agent-protocol), [`wasm list/load/run/health/unload/stats`](cli-reference_EN.md#wasm-sandbox), [`mcp serve/list/add/remove/connect`](cli-reference_EN.md#mcp-client-and-server), [`studio`](cli-reference_EN.md#web-ui), [`cron *`](cli-reference_EN.md#cron-scheduled-tasks) (Scheduler via the `cron` subcommand).

## Further reading

- Each crate's source dir `crates/oneai-{a2a,wasm,studio,mcp,scheduler,gateway,supervisor}/`
- Evolution background in [evolution-plan](evolution-plan-2026-07.md) (Chinese)
