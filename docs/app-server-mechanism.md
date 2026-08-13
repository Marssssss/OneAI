# OneAI App-Server 机制

> `oneai-app-server` —— 引擎与一切**非 Rust 前端**（IDE 插件 / web / TS-JS / 桌面 macOS-Swift·Windows-C#）之间的 JSON-RPC 2.0 协议层。一条前端协议 + 多 transport（stdio / unix-socket / named-pipe / WebSocket）并发监听，一个引擎进程喂所有这类前端。

## 1. 概述（是什么）

`oneai-app-server` 是在 `oneai-bus`（L3 引擎级 newline-JSON 协议）之上加的一层 **L2 适配器**：把面向前端的 **JSON-RPC 2.0** schema 映射到底层 `Directive`/`EngineYield` 语义。引擎无感——它看到的仍是同一个 `InProcessBus`，不论另一端是 in-process 的 TUI 还是 out-of-process 的 IDE/web/桌面。

非 Rust 前端进来后从「桌面一个」变「四类」，统一成一条 JSON-RPC 前端协议比「桌面 passthrough + IDE 各自」省 schema、省工具链、省维护：JSON-RPC 让 IDE/MCP 生态工具链（`generate-ts`/`json-schema`）可复用；操作导向 API（`turn/run` 有返回值）天然 RPC。

## 2. 分层（关键）

- **L3 bus**（不变，`oneai-bus`）：`Directive`/`EngineYield` newline-JSON + `InProcessBus`。内部 canonical 协议；TUI 直连（零序列化）。
- **L2 app-server 适配器**（本 crate）：
  - **inbound**：JSON-RPC 方法 → `bus.submit(Directive)`。`turn/run`→`UserMessage`、`turn/cancel`→`Interrupt`、`approval/respond`→`Approve`、`session/*`→对应 session directive …。
  - **outbound**：`bus.subscribe_yields()` → 单一 JSON-RPC `event` 通知（`params` = 完整 `EngineYield`，含 `kind` tag）。
  - **Dispatcher**：解析「阻塞型」请求（`turn/run` 等）的返回值——在引擎后续 yield 到达时 fulfill oneshot。
- **L1 多 transport**（本 crate）：stdio（IDE LSP 式 spawn）/ ipc（`oneai-supervisor::IpcListener`，Unix=UDS / Win=named-pipe）/ ws（`tokio-tungstenite` + `TcpListener`，浏览器 WS 握手）并发监听。
- **L4 引擎**（不变）：`spawn_directive_pump` → AgentLoop + `BusObserver` + `BusInteractionGate`，无感于外面是谁。由 CLI（`oneai app-server`）构建后把 `Arc<InProcessBus>` 传给 crate 的 `serve_all`。

## 3. 进程拓扑

```
oneai app-server --listen stdio --listen ipc://~/.oneai/app-server.sock --listen ws://127.0.0.1:8787
```

一个进程并发监听三类 transport；IDE 插件 spawn 走 stdio、web 连 WS、桌面连 ipc、TUI in-process 直连 bus。**一个引擎进程喂五类前端**（含 TUI + 移动 Shape A）→ 崩溃隔离 + 单二进制多前端 + 签名解耦（`.app`/`.vsix` 薄壳，`oneai` 引擎二进制独立升级）。

与 `oneai serve`（newline-JSON passthrough sidecar）区别：`app-server` 讲 JSON-RPC 操作导向 schema（IDE/MCP 工具链友好），`serve` 是原始 bus passthrough（escape hatch，仍可用）。两者用不同 socket（`app-server.sock` vs `serve.sock`）故共存。

## 4. JSON-RPC schema

`id` 用 `serde_json::Value`（支持 `null`/str/num）—— notification 无 `id`。手写信封（不复用 `oneai-a2a` 的 `JsonRpcRequest`，它 `id:u64`、HTTP-only、无 notification，不适配双向流）。

### Inbound 请求（有 `id`，期望响应）

| method | params | → Directive | 响应 |
|---|---|---|---|
| `turn/run` | `{content:[ContentBlock]}` | `UserMessage` | TurnStart 即返 `{turn_id}` |
| `turn/cancel` | `{reason?:InterruptReason}` | `Interrupt` | ack `{ok:true}` |
| `approval/respond` | `{request_id,response:InteractionResponse}` | `Approve` | ack |
| `paradigm/switch` | `{to:BusParadigmKind}` | `SwitchParadigm` | ack |
| `config/update` | `{plan_mode?:bool}` | `UpdateConfig` | ack |
| `session/create` | `{id?:String}` | `CreateSession` | SessionCreated 即返 `{id}` |
| `session/load` | `{id:String}` | `LoadSession` | SessionLoaded 即返 `{id,messages}` |
| `session/clear` | `{}` | `ClearSession` | SessionCleared 即返 `{id}` |
| `session/delete` | `{id:String}` | `DeleteSession` | ack（结果经 `event`） |
| `conversation/compact` | `{keep_recent_turns:usize}` | `Compact` | ack（结果经 `event`） |
| `project/init` | `{format?,force?,no_llm?}` | `InitProject` | InitResult 即返 `{message}` |
| `group/start` | `{scenario:BusGroupScenario}` | `StartGroupChat` | ack |
| `group/open` | `{}` | `GroupStart` | ack |
| `group/run` | `{user_input:String}` | `GroupUserMessage` | ack |
| `group/set_order` | `{order:[String]}` | `GroupSetScriptedOrder` | ack |
| `scenario/list` | `{}` | —（同步 CRUD，非 Directive） | `{scenarios:[BusScenario]}` |
| `scenario/get` | `{id:String}` | — | `BusScenario`（缺 → `-32602`） |
| `scenario/upsert` | `{scenario:BusScenario}` | — | `{ok, id?}`（先 validate，不过返 errors） |
| `scenario/delete` | `{id:String}` | — | ack |
| `scenario/validate` | `{scenario:BusScenario}` | — | `{ok, errors:[{field,code,message}]}` |
| `shutdown` | `{}` | `Shutdown` | ack |

- **`scenario/*` 是纯共享状态 CRUD，不走 Directive/bus**——直接读写进程级 `ScenarioStore`，即返。共享场景库（macOS / VS Code / 浏览器编辑同一份 `~/.oneai/scenarios.json`），`scenario/validate` 是单一权威校验（杀掉各前端客户端镜像引擎检查的漂移）。`BusScenario` 是 rich 编辑单元（cast + turn policy + topic fields + debrief + review loop）；启动时前端编译成 `BusGroupScenario` 提交 `group/start`（见 `BusScenario::to_group_scenario`）。
- **「即返」**= Dispatcher 在对应 yield 到达时 fulfill oneshot（去 `kind` tag，只回字段）；**ack** = `bus.submit` 成功后立即 `{ok:true}`。
- **`turn/run` 在 TurnStart 即返 `turn_id`**（非阻塞到 TurnComplete）—— 流式片段期间照常发 `event` 通知，回合结束由 `turn/complete`（即 `turn_complete` 的 `event`）收尾。客户端无长占请求、turn_id 早知道。
- **`session/delete` / `conversation/compact` 是 ack 而非「即返」**——因 pump 在失败时发 `EngineYield::Error`（非结果 yield），阻塞型会挂；结果（`SessionDeleted` / `CompactResult` / `Error`）经 `event` 通知到达，前端按 `params.kind` 分支。
- 未知方法 → `-32601`；submit 失败 → `-32603`；坏 JSON → `-32700`；缺 method → `-32600`；参数错 → `-32602`。

### Outbound 通知（无 `id`）

单一方法 `event`，`params` = 完整 `EngineYield` JSON（含 `kind` tag）。前端按 `params.kind` 分支：`turn_start`/`stream_chunk`/`thinking`/`tool_calls`/`tool_result`/`delegate`/`delegate_complete`/`speaker_turn`/`paradigm_switch`/`approval_request`/`working_state`/`context_accounting`/`plan_update`/`tools_added`/`init_result`/`compact_result`/`token_usage`/`error`/`turn_complete`/`iteration_start`/`session_created`/`session_loaded`/`session_cleared`/`session_deleted`/`session_ended`。

新 yield 变体（`#[non_exhaustive]`）以未知 `kind` 抵达，前端忽略——零 per-variant RPC 方法爆炸，协议随 bus 长变体不 break 老前端。`approval_request` 的 `request_id` 在 `params` 内，前端发 `approval/respond` 回。

## 5. Dispatcher——为何单一消费者

总线是 `broadcast`：每个连接的 yield forwarder 都看见**每条** yield。但「阻塞型」请求的解析是**全局 FIFO** 关注点——若每连接各自解析，同一条 `TurnStart` 会被 N 个消费者各 pop 一次。故一个 app-server 进程**一个 `Dispatcher`**，跨所有连接/transport 共享，持有按变体分桶的 FIFO 队列（`pending_turns`/`pending_session_create/load/clear/delete`/`pending_compact`/`pending_init`），由**单个** yield 消费者任务 drain。

引擎 directive pump **串行**处理 directive（bounded mpsc 逐条 drain）→ 同变体 yield 的发射顺序与 submit 顺序一致 → FIFO-per-variant 正确。订阅在 spawn **前**完成（`serve_all`/测试均 `bus.subscribe_yields()` 后再 spawn run），避免首 yield 丢失竞态。

## 6. 文件与核心抽象

| 项 | 位置 |
|---|---|
| crate doc + `ListenSpec` + `serve_all` + `AppServerError` | `crates/oneai-app-server/src/lib.rs` |
| JSON-RPC 信封（`Request`/`Response`/`Notification`/`RpcError`）+ 方法常量 + 错误码 + `decode_inbound` | `crates/oneai-app-server/src/protocol.rs` |
| `Dispatcher`（按变体 FIFO 队列 + 单 yield 消费者） | `crates/oneai-app-server/src/dispatcher.rs` |
| `serve_connection`（outbound forwarder + inbound 分发 + 方法→Directive 映射 + scenario/* 同步 arm） | `crates/oneai-app-server/src/adapter.rs` |
| transports：`serve_stdio`/`serve_ipc`/`serve_ws`/`serve_native_messaging` + 行/帧桥 | `crates/oneai-app-server/src/transport.rs` |
| `ScenarioStore` trait + `FileScenarioStore`（`~/.oneai/scenarios.json` 原子 rename）+ `InMemoryScenarioStore` + 内置预设 | `crates/oneai-app-server/src/scenario.rs` |
| CLI 子命令 `oneai app-server`（建引擎 + `--listen` 解析 + `serve_all` + 建 scenario_store） | `examples/cli/src/cmd_app_server.rs` |

```rust
// 引擎侧由 CLI 构建（不在 crate 内）：
let (builder, directive_rx) = AppBuilder::new().engine_bus();
// … build app + spawn_directive_pump …
let scenario_store: SharedScenarioStore = Arc::new(FileScenarioStore::new(default_scenarios_path()).await?);
let server = oneai_app_server::serve_all(specs, bus, scenario_store).await?;  // bus: Arc<InProcessBus>
tokio::select! { _ = server => {}, _ = tokio::signal::ctrl_c() => {} }
```

## 7. 与既有前端通路的关系

| 前端 | 路径 | 是否经 app-server |
|---|---|---|
| TUI（`examples/cli`） | in-process，`Arc<InProcessBus>` 直连 L3 | 否（零序列化） |
| 原生 macOS（桌面 sidecar） | app 内 `OneAiRpcClient`（UDS ipc）+ `EngineProcessManager` spawn `oneai app-server --listen ipc://<ephemeral>` | **是**（infra 落地；`ChatViewModel` 仍在 FFI 默认，sidecar 接线带开关待 macOS 宿主验证，见 §11） |
| 原生 Windows（桌面 sidecar） | `OneAiRpcClient.cs`（named pipe ipc）+ `EngineProcessManager.cs` spawn | **是**（client+spawn 骨架；WinUI 接线延后，Windows 本机不可编译） |
| VS Code 扩展（TS） | `platforms/vscode`：激活 spawn `oneai app-server --listen stdio`，webview 经 postMessage 中转 | **是**（完整扩展 + chat webview；tsc/esbuild 净，0 漏洞；ExtensionHost 运行时验证待用户宿主） |
| 浏览器扩展 | `platforms/browser`：Chrome/Firefox native messaging（4B-LE 前缀）spawn `oneai app-server --listen native-messaging` | **是**（popup + scenario editor；install-host.sh macOS/Linux；Windows host 延后） |
| 移动（iOS/Android/HarmonyOS） | in-process c_facade 三符号泵（Shape A） | 否（on-device 无 spawn + 无云端引擎兜底） |
| 飞书 gateway | 经 bus 投递，挂同一 daemon 的 L2 适配器（仍走 bus） | 可选 |
| A2A | 独立进程（协议边界，P5-C 决定） | 否 |

## 8. 扩展点

- **加新前端方法**：在 `adapter::handle_request` 加 `method::` 常量 + match arm。引擎型方法→提交对应 `Directive`（阻塞型在 `Dispatcher` 加 `pending_*` 队列 + `register_*` + `dispatch` arm）；纯共享状态方法（如 `scenario/*`）直接读写 `Arc<dyn ScenarioStore>`，即返，不走 Directive/bus。
- **加新 transport**：在 `transport.rs` 加 `serve_<x>`（把具体字节 ↔ `mpsc<String>` 桥到 `serve_connection`）+ `ListenSpec` 变体 + `parse` 分支 + `serve_all` arm。`native-messaging` transport 即此模式（4B-LE 前缀成帧）。
- **自定义前端 UI**：前端连任一 transport，发 `turn/run`，按 `event` 的 `params.kind` 渲染；遇 `approval_request` 取 `params.request_id` 弹框，回 `approval/respond`；场景经 `scenario/*` 共享编辑 + `scenario/validate` 实时校验。

## 9. 测试

`crates/oneai-app-server`（45 测）：单元（`ListenSpec` 解析含 native-messaging / 信封 round-trip / 方法→Directive 映射 / Dispatcher FIFO 与 kind 剥离 / native-messaging 成帧 round-trip + 4B-LE 前缀 + 超长拒绝 + EOF / `BusGroupScenario::validate` 各分支 / `BusScenario::validate` rich + `to_group_scenario` + JSON round-trip / `ScenarioStore` 原子 rename + upsert/replace + builtin 预设校验）+ 集成（mpsc-channel 驱动的 `serve_connection`：`turn/run`→event 流+turn_id / approval 回路 / `turn/cancel` fire token / `session/create` / 未知方法 -32601 / 坏 JSON -32700 / **scenario/list·validate·upsert·get·delete 全回路**）+ WS e2e（真 ephemeral port + `tokio-tungstenite` 客户端 round-trip）。

## 10. 深入阅读

- [bus-mechanism.md](bus-mechanism.md) —— L3 引擎总线（本层之下的 canonical 协议）
- [cross-platform-mechanism.md](cross-platform-mechanism.md) —— 桌面 sidecar vs 移动 Shape A vs TUI in-process
- [cli-reference.md](cli-reference.md) —— `oneai app-server` 子命令
- 源码：`crates/oneai-app-server/src/`（6 文件：`lib`/`protocol`/`adapter`/`dispatcher`/`transport`/`scenario`）+ `examples/cli/src/cmd_app_server.rs` + `platforms/{vscode,browser,macos,windows}`

## 11. Auto-spawn（Codex 模型）——用户不手动起 server

设计原则：**能 spawn 进程的前端自己 owns spawn**，用户永远不手动跑 `oneai app-server`。这正是 Codex CLI 的做法——VS Code 扩展 `child_process.spawn` 激活时拉起引擎子进程，JSON-RPC over stdio，崩溃指数退避重启、停用释放。按前端 spawn 能力分派：

| 前端 | spawn 能力 | auto-spawn 方案 |
|---|---|---|
| VS Code 扩展 | ✅ | 激活 `spawn(oneai, app-server --listen stdio)`（`platforms/vscode/src/server.ts`），webview 经 postMessage 中转（webview 不能 spawn） |
| 浏览器扩展 | ❌（沙箱） | Chrome native messaging：`install-host.sh` 注册一次 host manifest，之后浏览器按需 spawn `oneai app-server --listen native-messaging`（4B-LE 前缀成帧，`serve_native_messaging`） |
| macOS 桌面 | ✅ | `EngineProcessManager.swift` 启动 spawn `oneai app-server --listen ipc://<ephemeral app-server-<pid>.sock>`（`.app/Contents/Resources/bin` 优先 → PATH），接 `OneAiRpcClient`，exit 重启 |
| Windows 桌面 | ✅ | `EngineProcessManager.cs` spawn `--listen pipe://oneai-<pid>`（骨架，接线延后） |
| 移动 | ❌ | in-process c_facade（Shape A，on-device 无 spawn） |

**stdout 纪律**：`stdio` 与 `native-messaging` 模式下 stdout 是消息流——`oneai app-server` 把 banner/诊断全走 stderr（LSP 惯例），`tracing` 亦走 stderr，保证 stdout 只剩成帧消息，不污染协议。

**二进制定位**：前端先查 bundle（`.app/Contents/Resources/bin/oneai` / `.vsix` 内），再退 PATH——与「签名解耦、引擎二进制独立升级」一致；VS Code 用 `oneai.oneaiPath` 配置 + PATH fallback（对齐 Codex `codex.cliPath`）。

**macOS 接线现状（诚实）**：`OneAiRpcClient` + `EngineProcessManager` infra 完整可编译、不破坏现有 app 编译；`ChatViewModel` 的 `oneai_engine_transport` 开关接线（单 agent + group/scenarios 走 sidecar，FFI 全局兜底）+ `specView` topic-baking 移植是剩余工作，需 macOS 宿主运行时验证——FFI 仍是默认 transport，sidecar infra 待采纳。

