# OneAI 扩展机制（A2A / WASM / Studio / MCP / Scheduler / Gateway / Supervisor）

> 把 Agent 能力对外暴露、对内加壳、可视化与调度的一组扩展 crate。

## 职责

Agent 内核之外，一组「外向」扩展：协议互操作（A2A、MCP）、不可信代码沙箱（WASM）、可视化与时间旅行（Studio）、定时与常驻（Scheduler / Supervisor）、消息接入（Gateway）。

## 各子系统

- **A2A**（`oneai-a2a`）— Agent 间协议 SDK：客户端 + axum JSON-RPC 服务端宿主 + DomainPack→AgentCard 自动暴露。共享密钥 Bearer（`ONEAI_A2A_SECRET`）。
- **WASM**（`oneai-wasm`）— Wasmtime 沙箱执行不可信代码：`WasmTool` / `WasmModuleRegistry` / 资源监控 / WASI 受限 / Native↔Wasm 执行模式。
- **Studio**（`oneai-studio`）— axum HTTP+WS + REST API + 实时事件推送 + D3.js StateGraph 可视化 + Checkpoint 时间旅行。
- **MCP**（`oneai-mcp`）— `McpServerHost`（JSON-RPC 服务端，让 OneAI 向 Claude Code/Cursor 暴露工具）+ `McpPluginRegistry`（发现/配置/连接）+ TOML 配置 + stdio 传输。
- **Scheduler**（`oneai-scheduler`）— 内存任务调度，`Schedule` 四方言（`30m` / `every` / ISO / 5-field cron）+ `JobStore`（CAS at-most-once）。
- **Gateway**（`oneai-gateway`）— 消息网关：axum webhook + adapter（飞书 sha256+AES / 企业微信 sha1 / Loopback），per-channel pack lazy App，流式 coalescer。
- **Supervisor**（`oneai-supervisor`）— headless 监督 daemon：持久 instances + 崩溃恢复（Running→Crashed）+ 换行分隔 JSON protocol + Unix/named-pipe/in-memory IPC。

## 关键类型与文件

| 子系统 | 关键文件 |
|---|---|
| A2A | `crates/oneai-a2a/src/{client,server,router,handler,card,task_store}.rs` |
| WASM | `crates/oneai-wasm/src/{runtime,module,registry,monitor,guest_api,action_template}.rs` |
| Studio | `crates/oneai-studio/src/{server,routes,handlers,ws,graph_dto,checkpoint_dto}.rs` |
| MCP | `crates/oneai-mcp/src/{server,client,router,plugin,discovery,config}.rs` |
| Scheduler | `crates/oneai-scheduler/src/{scheduler,store,runner,orchestrator,job}.rs` |
| Gateway | `crates/oneai-gateway/src/{gateway,directory,profile,runner}.rs` + `adapters/{feishu,wechat,loopback}.rs` |
| Supervisor | `crates/oneai-supervisor/src/{supervisor,registry,runner,transport,protocol,client}.rs` |

## 相关 CLI

[`a2a serve/discover/list/send`](cli-reference.md#a2aagent-to-agent-协议)、[`wasm list/load/run/health/unload/stats`](cli-reference.md#wasm-沙箱)、[`mcp serve/list/add/remove/connect`](cli-reference.md#mcp客户端--服务端)、[`studio`](cli-reference.md#web-ui)、[`cron *`](cli-reference.md#cron定时调度)（Scheduler 走 `cron` 子命令）。

## 深入阅读

- 各 crate 源码目录 `crates/oneai-{a2a,wasm,studio,mcp,scheduler,gateway,supervisor}/`
- 演进背景见 [evolution-plan](evolution-plan-2026-07.md)
