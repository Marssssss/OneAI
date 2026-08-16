# OneAI WebUI 重构设计（对标 deepseek-harness）

> 本文是 OneAI 面向终端用户的「完整 Web 界面」重构设计文档。
> 设计基线：先研究 deepseek-harness（以下简称 dsh）的 webUI 如何实现，再与 OneAI 现有引擎能力对标，
> 划出「可直接做 / 暂缺应补 / 暂缺不建议做」三档，并把 OneAI 引擎已有、但 Web 端尚无的
> **场景式对话入口**按 dsh 同款 UI 风格补全，形成 OneAI 自己的 WebUI。

---

## 0. 速览结论

| 维度 | 结论 |
|---|---|
| 对标对象 | dsh `packages/client`（browser 半）+ `packages/host`（host 半）；**不是** dsh 的 `website/`（VitePress 文档站） |
| OneAI 协议底座 | `crates/oneai-app-server`（JSON-RPC 2.0 over stdio/ipc/ws/native-messaging，`ws` 默认开启）—— 这是 webUI 的对接面，**已就绪** |
| OneAI 已落地 Web 前端 | `platforms/browser`（扩展 popup）+ `platforms/vscode`（webview）—— 都是**简化版**，非独立 web app |
| OneAI 调试工具页 | `crates/oneai-studio`（StateGraph/checkpoint/trace，**不走 app-server**，单 agent，无场景）—— 可作「trace 可视化组件」被复用，**不是底座** |
| dsh 有但 OneAI 引擎无 | 多 session 并行编排的 preset composition 文件格式（`agent.cordis.yml`）、Cordis 插件槽位系统、DeepSeek onboarding |
| OneAI 有但 dsh 完全无 | **场景式多角色群聊**（`GroupChatSession` + `BusScenario` + `scenario/*` + `group/*` 全套）—— 这是 OneAI 的差异化能力，Web 端要把它做成一等入口 |
| 推荐技术栈 | React 18 + Vite + TypeScript + CSS Modules + 设计 token（`--oneai-*`），状态走 `useSyncExternalStore` 外部存储，对接 app-server WebSocket |
| 第一交付 | 全新 `platforms/web/` SPA，复用 app-server ws transport + `platforms/shared/scenario-editor.js` 的单一真相源思路 |

---

## 一、dsh-harness webUI 设计解析

### 1.1 技术栈

- **框架**：React 18.2 + Vite 6，纯客户端 SPA（无 SSR）。
- **构建**：`apps/web` 不是独立 dev server——`vite.config.ts` 的 `rejectStandaloneServe()` 在 `serve` 阶段抛错，必须由 `dsh web`（apps/cli）注入 `window.__DSH_BOOT__` 启动清单后才能跑。入口 `apps/web/index.html` → `apps/web/src/main.ts` → `AppWebEntry`（`packages/client/web/src/boot.tsx`）。
- **状态管理**：**不用 Redux/Zustand**，三层外部存储：
  1. Cordis `ctx` 进程级单例服务（`ctx.slots` / `ctx.sessions` / `ctx.connection` …）；
  2. session projection 快照（`HostObservable`：`getSnapshot`+`subscribe`）；
  3. `useSyncExternalStoreWithSelector` 桥（`packages/client/web-react/src/bind.ts`，`bindSnapshotSelector()`）—— client 栈里**唯一的 hook 构造器**。
- **样式**：CSS Modules（`.tsx` + 同名 `.module.css`）+ 设计 token CSS 变量（`--dsw-static-*`）。无 Tailwind、无 CSS-in-JS。`ui-primitives` 自述「Cordis-free React 原语，只用 `--dsw-*` token」。
- **markdown/代码高亮**：自研增量 React 渲染器 over micromark/mdast（`incremental.ts`/`render.tsx`），KaTeX 数学，**单一 shiki 高亮器**用同步 JS 正则引擎（无 oniguruma WASM），启动只装 TS/shell/JSON，其他懒加载。

### 1.2 布局与 shell

- **三栏 grid**：`sidebar | center | details`，由 `AppFrame`（`packages/client/ui-layout/src/client/AppFrame.tsx`）渲染，`gridTemplateColumns` 内联，可拖拽调宽。
- **列让步求解器**（纯函数 `computeColumns`）：center 永不低于 `CENTER_MIN=640`；先缩 details 到 `300`，再自动关 details；sidebar 永不让步，关闭退到 56px rail；窄屏 `<1024` 自动折叠 sidebar。
- **shell 装载与 gating**（`boot.tsx`）：两阶段——解析启动清单 → 渲染 loading 页 → 并行预取 `immediately` 层插件 → 装载 Cordis Loader → 每个插件图节点建 entry → 全 active sweep（`assertEntriesActive`，pending/failed 都 fail-loud）→ 翻转 `settled` → 一次性切到真 UI。**shell 自足铁律**：loading 页不依赖任何插件。
- **槽位（slot）系统**（`packages/client/ui-slots`）：`SlotMap` 通过 TS declaration merging 让插件声明槽位 key；单一 `register` 组合 API；分派模式 single/keyed/list/chain，带 shadowing 与 entry 边界崩溃报告。程序里唯一的 ctx 级 `renderSlot('root')` 在 `app-shell`。
- AppFrame 渲染四个子槽：`sidebar`、`conversation`（session-maybe）、`details`（strict，无 session 时空）、`shell.overlay`。

### 1.3 对话主视图 `ui-conversation`

- `ConversationRoot`：resident 骨架，跨 no-session/session 保持挂载。
- `ChatView`：稳定 keyed 父列表 over business Nodes + 分页 + bottom-follow（`FOLLOW_THRESHOLD=24`）；每行 `ChatNodeSeat` 只订阅一个 Node key，Assistant delta / Tool 生命周期更新只替换自己那一行不 remount。
- **节点分发**：`conversation-nodes/`（`assistant`/`command`/`tool`/`compaction`/`turn-tail`/`inbox`/`retry`/`turn-error`/`turn-max-tokens`），`register.ts` 统一注册到 `conversation.chat.node` 槽。
- **Assistant 内容**（`AssistantMarkdown`）按 block 渲染：`text`→MarkdownText、`reasoning`→ReasoningRow（摘要行，展开=缩进灰文）、`image`→ImageGallery、`tool-call`→由 ChatView 分组成 tool 行、default→JsonBlock。
- **工具调用块**（`ui-tool`）：`ToolCallTree` 递归 root/subcall，注册到 `tool-call` key；`ToolDetails` 注册到 `conversation.details.tool`（选中调用的 args/result）；内置 toolview 原子卡（bash/read/file-mutation/search/web/todo/ask-question）+ diff/read/search/terminal/web 卡片模型。
- **流式**：`streaming` 标志透传 `MarkdownText`，`use-throttled-visual-update.ts` 节流；turn 级 loading dots 在 chat 尾部。
- **审批流**：`ApprovalPanel`——composer 接管式：amber「Waiting for approval」条 + 理由 headline + 命令 + refuse/allow 一次性按钮。
- **附件**（`ui-attachment`，零 cordis）：草稿图轨 / 消息图 / 灯箱 / 拖放。
- **交付物**（`ui-deliverables`）：turn 尾产出文件 + 可点最终响应文件引用。
- **用户提问**（`ui-user-questions`）：ask-user 工具 + composer 接管式 `QuestionComposer` + `PlanReviewPanel`（计划评审三按钮）。
- **消息反馈**（`ui-message-feedback`）：per-message 反馈控件挂 assistant action strip。
- **输入触发**（`ui-input-trigger`）：`/` 和 `@` 检测 + 候选菜单 + pick 路由。
- **权限**（`ui-permission-presets`）：`/permission` popup（当前会话）+ General settings 行（新会话默认），含 `FULL_ACCESS_PRESET` + `RiskConfirmation`。
- **模型选择**（`ui-model-selection`）：`ModelSelect` 两级菜单——root 是 Model/Effort 行对，drill-in 到 provider 分组模型列表 / effort 级别。

### 1.4 侧边栏 `ui-sidebar`

`SidebarRoot` 只管列几何：logo 行（双作 New Session 快捷 / 折叠展开 toggle）→ New Session 按钮 → `sidebar.workspaces` 槽（occupant 是 `ui-workspace`：多级树 / 搜索 / 分组 / 状态点）→ `sidebar.footer.action` + `sidebar.settings`。折叠动画：内容冻结内联裁剪，150ms 后宽内容卸载，4 个上部控件滑入 56px rail。滚动条跟随指针离开 2s 隐藏。

### 1.5 设置 + schema-form

- `ui-settings`（base）：namespace scope 服务 + `settings.section` 槽契约。
- `ui-settings-general`：`SettingsRoot` 居中模态（1080×700）+ 左侧 section nav rail（models=`IconDataOutline16`、agent-presets、plugins、default）。关闭路径：按钮 / mask / Esc。
- `ui-settings-models`：Models 区 + DeepSeek onboarding 对话 + 自定义 provider 卡。
- `ui-settings-plugins`：Plugins 区，feature-owned tabs + 可配置 host-plane plugin 卡。
- `ui-settings-plugin-inventory`：只读 Cordis Loader inventory tab。
- `schema-form`：**不是渲染器**，是 schema/draft model 层（`rehydrateSchema`/`setPath`/`validateDraft`）——编辑器自己渲染控件，schema-form 只提供 helper。
- **agent preset / permission presets**：`ui-agent-preset` 一个 roster 四个面（General 默认行 + new-session hero chip + header label + roster 管理 section），运行中的 session 保留启动时的 composition（host 拒绝热换 preset）。

### 1.6 能力面板呈现方式

大部分能力**不**是独立侧边面板，而是嵌入对话流或 composer 座：

| 能力 | 包 | 呈现 |
|---|---|---|
| plan | `ui-plan` | composer 的 `conversation.input.plan` 座，chip + `/plan` 命令 |
| goal | `ui-goal` | composer 上方 dock 条（图标 + phase + 截断 objective + resume/pause/edit/clear） |
| skill | `ui-skill` | toolview 紧凑 accent 行 + 可展开 disclosure 卡 |
| subagent | `ui-subagent` | session-header actions 槽的子代理目录（树形 descendants）+ `@` 引用源 |
| workflow-run | `ui-workflow-run` | 对话 Node + 嵌套 member disclosure |
| jobs | `ui-jobs` | session-header actions 槽的后台 job 列表（live registry state） |
| trajectory | `ui-trajectory` | **对话视图的一个 tab**（turn-aware event ledger + timing overview），pure-consumer |
| details | `ui-conversation` | 选中 tool call 的 args/result 全量 |

### 1.7 workspace / directory-picker

- `ui-workspace`：empty-state 槽 + sidebar 槽；`WorkspacePicker` + `WorkspaceBrowser`，加 workspace 唯一路 = pick 一个 host 目录。
- `ui-directory-picker-browse`：in-app **Miller 双栏**目录浏览器（680×500 对话），selection-anchored 安静落地，New folder 嵌套 create 对话。
- `ui-directory-picker-native`：renderless 驱动 host OS chooser。

### 1.8 连接与运行时

- client↔host：`packages/client/connection`，选 fixture 或 HTTP/WS 传输，提供共享 `IApiClient`；`ctx.connection` 暴露 `api`/`isLoopback`/`rpc`/`start`。
- host 侧：`packages/host/webserver`（`node:http` + route registry + upgrade registry + index transform taps，**不 serve 文件**）+ `frontend-static`（SPA dist fallback，index 跑 webserver 的 index taps 注入 `window.__DSH_BOOT__`）+ `apiproxy`（JSON-RPC over HTTP/WS，按域分 `sessions`/`approvals`/`goals`/...）。
- SDK：`packages/sdk/{protocol,client,server}`。
- HMR：`pnpm dsh web` + `pnpm run dev:web`，resolve alias 把 workspace 包指向 src，插件包**绝不打进 shell bundle**。

### 1.9 关键结论——dsh 的「场景式对话入口」

**dsh 没有「多角色群聊/场景预设」概念。它的 `preset` 做的是 per-session 单 agent composition，不是多 agent 场景群聊。**

证据：

1. `packages/preset/README.md`：「An agent preset is a directory holding one `agent.cordis.yml`. Mounting it under an agent's scope context gives that session its own tools and prompt sections...」——一个 preset = 一个 agent 的工具+提示词+persona。
2. `apps/cli/config/agent-presets/{standard,code,minimal,cordis}/agent.cordis.yml`：全是**单 agent 编码助手配置**。
3. `ui-agent-preset` 四个面全是单 preset 选择/管理，**没有「选场景→进入多角色对话」入口**。
4. 全局 grep `group.chat|groupchat|speaker|debrief|multiParticipant` 命中均为测试 fixture 或 `turnOrder`（单 agent 对话轮次 ID），**无任何多角色群聊原语**。
5. dsh 的「多 agent」=同一进程并行跑多个**独立** session（各跑各的 preset），而非一个 session 内多角色轮流发言。

**含义**：OneAI 的场景式群聊是 dsh 完全没有的差异化能力。把「场景式对话入口」做成 Web 一等入口，正是 OneAI webUI 区别于 dsh 的核心。

### 1.10 设计语言

- **颜色**：完整 `--dsw-static-*` token，DeepSeek 蓝 + neutral/bluish 双灰阶 + amber/green/red 语义色，亮/暗双套（`body[data-ds-dark-theme]` 重定义）。
- **字体**：`-apple-system, BlinkMacSystemFont, 'Segoe UI', 'PingFang SC', 'Hiragino Sans GB', 'Microsoft YaHei'`；代码字体 `SF Mono`/`JetBrains Mono`/`Fira Code`/Consolas，**刻意不带 bare `monospace` 尾巴**避免 Windows CJK 回退 SimSun。
- **图标**：自有 SVG 图标集（尺寸后缀，如 `IconNewChatOutline16`）+ `BrandWordmark`/`FishLogo`。
- **动效**：`cubic-bezier(0.4,0,0.2,1)`，fast 0.1s / 默认 0.2s / slow 0.3s。
- **交互模式**：审批=composer 接管式 amber 条；工具=递归 tree + per-tool 原子卡 + details 全量；流式=增量 markdown + throttled update + 尾部 loading dots；命令=`ui-commands` `/` 目录 + `ui-input-trigger` `/`/`@` 检测 + `PopupSelectView` 通用弹出；reasoning=摘要行展开；overlay=`Modal`/`RiskConfirmation`/`HoverCard`/`Tooltip`/`Toast`/`Menu`。

---

## 二、OneAI 现状盘点

### 2.1 Web/前端资产

| 资产 | 形态 | 是否产品级 webUI | 对接协议 |
|---|---|---|---|
| `platforms/browser` | Chrome/Firefox MV3 扩展 popup（`chat.js` 158 行 + 场景编辑器 tab） | ❌ 扩展 popup，非独立 web app | app-server **native-messaging**（4B-LE 帧） |
| `platforms/vscode` | VS Code webview + scenarios 侧边栏（`chat.js` 236 行） | ❌ 扩展 webview | app-server **stdio**（newline-JSON） |
| `platforms/shared/scenario-editor.js` | transport-agnostic 场景编辑器单一真相源（248 行），`sync.sh` 同步副本 | — | 取 `rpc.call()` |
| `crates/oneai-app-server` | JSON-RPC 2.0 协议层 + stdio/ipc/ws/native-messaging | ✅ **协议底座，已就绪** | — |
| `crates/oneai-studio` | axum + D3.js StateGraph/checkpoint/trace 工具页 | ❌ 调试工具页，单 agent，无场景 | 自己的 REST `/api/*` + `/ws`，**不走 app-server** |
| `crates/oneai-uniffi` + `platforms/{macos,windows,android,...}` | 原生 GUI | ❌ 原生端 | c_facade 3 符号 pump（macOS/Android）/ 细粒度 P/Invoke（Windows） |

**关键事实**：

- **没有 `platforms/web`**，没有独立 SPA。
- app-server **`ws` feature 默认开启**（`default=["ws"]`），浏览器可直接 WebSocket 接 `ws://host:port`，`transport.rs:167 serve_ws` + 集成测试 `ws_transport_roundtrips_turn_run` 已验证。**这是 webUI 重构最直接的接入点。**
- 浏览器扩展/VS Code 扩展已是「chat + scenario editor + 说话人路由 + 审批」的简化可用版，但**前端是 vanilla JS**，无组件化、无设计系统、无 StreamCoalescer。
- Studio 是单 agent playground：`studio.js` grep `speaker|scenario|group` 全 0 命中。

### 2.2 场景式对话引擎现状（核心）

**已完整落地，四端 UI 对齐：**

- **引擎层**：`GroupChatSession` 在 `oneai-agent` crate；`GroupChatBusObserver` 把 group turn 事件转成 bus 的 speaker-tagged yields（`SpeakerTurn` + 带 `speaker` 字段的 fragment yields），round 级发单个 `TurnComplete`。
- **bus 层**（`crates/oneai-bus/src/protocol.rs`）：
  - `Directive`：`StartGroupChat` / `GroupStart` / `GroupUserMessage` / `GroupSetScriptedOrder`（`protocol.rs:693-706`，`#[non_exhaustive]`）。
  - `EngineYield`：`StreamChunk{turn_id,text,speaker}`（group 时 `speaker=Some(member_id)`）、`SpeakerTurn`、`Thinking`、`ToolCalls`/`ToolResult`、`ApprovalRequest`、`ParadigmSwitch`、`PlanUpdate`、`WorkingState`、`TokenUsage`、`TurnComplete` 等共 ~39 个 yield kind。
  - **数据模型**：`BusGroupScenario`（引擎 launch payload：members/turn_policy/script_order/moderator_id/opener_agent_id/opener_line/title/review_loop/locale，`protocol.rs:199`）+ `BusScenario`（编辑器富模型：在 group 基础上加 id/name/icon/topic_fields/debrief，`protocol.rs:421`）+ `BusScenarioMember`/`BusTopicField`/`BusDebriefConfig`/`BusReviewLoop`/`BusLocale`。
  - **单一权威校验器**：`BusGroupScenario::validate()`(`:249`) / `BusScenario::validate()`(`:453`)，`scenario/validate` RPC 与所有前端编辑器都调它，**消除 per-frontend mirror drift**。`to_group_scenario()`(`:594`) 编译时丢弃 UI-only 字段。
- **场景 RPC**（app-server `adapter.rs:296-376`）：
  - `group/start`→`Directive::StartGroupChat{scenario}`、`group/open`→`GroupStart`、`group/run`→`GroupUserMessage{user_input}`、`group/set_order`→`GroupSetScriptedOrder{order}`（走 bus，ack-only，结果以 event 流回）。
  - `scenario/list`/`get`/`upsert`（先 validate）/`delete`/`validate`：**同步 CRUD，不走路由 bus**。
  - **customs 服务端权威 + presets 本地**：id `preset-*` 开头=本地 preset（`scenario/list` 返回的 server 端 `preset-*` 种子被忽略，让本地更丰富的本地化 preset 胜出）；非 preset=custom，sidecar 模式先 `scenario/upsert` 到服务端再镜像本地。
- **四端 UI 全对齐**：macOS SwiftUI（`AgentStore.swift` 628 行 5 预设双语 + `ScenarioEditor.swift` + `ChatViewModel.swift` 2027 行含 StreamCoalescer 20fps + 说话人路由 + debrief）/ Windows WinUI3 C#（`ScenarioModels.cs`/`ScenarioStore.cs`/`ChatViewModel.cs` 镜像）/ VS Code + 浏览器（共用 `scenario-editor.js`）。
- **FFI**：macOS/Android 走 c_facade 3 符号 pump（`oneai_submit_directive`/`oneai_poll_yield`/`oneai_shutdown`，group 经 `Directive` JSON）；Windows 走细粒度 P/Invoke（`oneai_create_group_session` 等 7 个，**不走 app-server**）。

### 2.3 app-server 协议清单（webUI 对接面）

全部 JSON-RPC 方法（`protocol.rs:33-84` + `adapter.rs:142-383`）：

| 方法 | 性质 | 说明 |
|---|---|---|
| `turn/run` | blocking-ack | `TurnStart` 解析返 `{turn_id, task}` |
| `turn/cancel` | ack | `Directive::Interrupt` |
| `approval/respond` | ack | `Directive::Approve{request_id, response}` |
| `paradigm/switch` | ack | `Directive::SwitchParadigm{to}` |
| `config/update` | ack | `Directive::UpdateConfig{plan_mode}` |
| `session/create` `/load` `/clear` `/delete` | blocking-ack / ack | 会话生命周期 |
| `session/list` | 同步 | `{sessions:[{id,created_at_ms,updated_at_ms,message_count,title}]}` |
| `conversation/compact` | blocking-ack | `keep_recent_turns` |
| `project/init` | blocking-ack | `InitResult`，format/force/no_llm |
| `group/start` `/open` `/run` `/set_order` | ack（走 bus） | 场景群聊 |
| `scenario/list` `/get` `/upsert` `/delete` `/validate` | 同步 | 场景 CRUD |
| `shutdown` | ack | `Directive::Shutdown` |
| `event`（**唯一出站**） | notification | params = 整个 `EngineYield`（带 `kind` tag） |

**bus 机制**：`Directive`（入站，~14 变体）+ `EngineYield`（出站，~39 yield kind）。出站只有 `event` 一个方法；新 yield 变体对旧前端是未知 `kind`（忽略即可），**协议随 bus 增长不破坏**。blocking-ack 由单消费者 per-variant FIFO 队列解析。

### 2.4 流式渲染机制现状

- **app-server 出站**：每个 `EngineYield` → `Notification::event()` → JSON-RPC `event` 通知 → 经 `spawn_yield_forwarder` 推到每条连接。传输：stdio/ipc=newline-JSON，ws=一 text frame 一消息，native-messaging=4B-LE 长度前缀。adapter framing-agnostic。
- **macOS StreamCoalescer**（`ChatViewModel.swift`）：~20fps 批量 flush hot fragments（streamChunk/thinking），`.complete`/`.error` 立即 flush。
- **Web 侧现状**：浏览器扩展 `chat.js` **没有 coalescer**，`rpc.onEvent("stream_chunk")` 每 chunk 直接 `createTextNode` + `scrollTop`，per-token 派发。**长流式性能待优化——这是 webUI 重构应补齐的点（前端 batching，不在 app-server 出站侧加 coalesce 以免破坏 bus 语义）。**
- **无独立 SSE 传输**：流式统一走 `event` 通知，web 端用 WebSocket。

### 2.5 已落地 WebUI 程度

**已实现（可用）**：浏览器扩展 popup + VS Code webview（简化版 chat + scenario editor + 说话人路由 + 审批），经 app-server。app-server ws transport 默认开启，浏览器可直接 WebSocket 接。

**只有引擎/原生端、Web 还没有**：

- 无独立 web app（无 `platforms/web`，无 React/Vue/Svelte SPA）。
- 无产品级 webUI 的「对话主界面 + session 侧边栏 + provider 设置面板 + 场景入口」完整形态。
- 无 Web 端 StreamCoalescer 等价物。
- 无 Studio 级 trace/checkpoint 在产品 webUI 的整合。
- Windows 走 FFI 不走 app-server，其对接经验不能直接迁移到 webUI。

---

## 三、对标矩阵

> 口径：以「OneAI 是否具备支撑该 UI 的引擎能力」分档。
> 🟢 **直接可做** = 引擎/协议已就绪，Web 只需实现前端组件。
> 🟡 **暂缺应补** = 引擎部分有但 Web 端缺口明显，或引擎有小缺口需补；值得做。
> 🔴 **暂缺不建议做** = 与 dsh 强绑定 / 不符合 OneAI 架构 / 投入产出比低。

### 3.1 🟢 直接可做（引擎就绪，Web 补 UI 即可）

| UI 能力 | dsh 包 | OneAI 引擎/协议对应 | 实现路径 |
|---|---|---|---|
| 三栏布局 sidebar/center/details | `ui-layout` `AppFrame` | — | 新建 `platforms/web`，照 dsh 列让步几何 |
| 会话侧边栏（列表/搜索/分组/状态点） | `ui-sidebar` `ui-workspace` | `session/list` `session/create` `session/load` `session/delete` | 订阅 `session/list` + 本地缓存 |
| 对话流（markdown/代码/思考/流式） | `ui-conversation` `AssistantMarkdown` | `EngineYield::StreamChunk`/`Thinking`/`TurnComplete` | React 增量 markdown 渲染器 + **前端 StreamCoalescer 20fps** |
| 工具调用树 + per-tool 卡 | `ui-tool` `ToolCallTree` | `EngineYield::ToolCalls`/`ToolResult` | keyed 节点树 + diff/read/search/terminal/web 卡 |
| 审批流（composer 接管式） | `ui-conversation` `ApprovalPanel` | `EngineYield::ApprovalRequest` + `approval/respond` | amber 条 + refuse/allow，并行审批排队（`approval_queue` 思路，见 memory `issue20`） |
| 模型选择 | `ui-model-selection` | SmartRouter + provider 池（引擎侧配置） | composer 两级菜单；首版只读+热切可后续 |
| 命令面板 / 斜杠命令 | `ui-commands` `ui-input-trigger` | `EngineYield` 无专门协议（客户端自维护命令目录） | `/` `/scenario` `/model` `/permission` 等 |
| 设置模态 | `ui-settings-*` | provider 配置 + DomainPack 选择（`cmd_app_server.rs:363 --domain`） | 首版 General/Models/DomainPacks/Permissions |
| Plan mode 控制 | `ui-plan` | `paradigm/switch` + `EngineYield::PlanUpdate` + `config/update{plan_mode}` | composer chip + `/plan` |
| Goal 指示条 | `ui-goal` | working-state（goal/steps/decisions/blockers 持久化） | composer 上方 dock，`EngineYield::WorkingState{event}` |
| subagent 目录 | `ui-subagent` | `EngineYield::Delegate`/`DelegateComplete` | session-header 抽屉 + `@` 引用 |
| workflow-run 节点 | `ui-workflow-run` | `EngineYield::PlanUpdate`（StateGraph） | 对话 Node + member disclosure |
| trajectory 视图 tab | `ui-trajectory` | `EngineYield::IterationStart` + bus 事件流 | 对话视图 tab，复用 Studio 的 D3 StateGraph 渲染 |
| 附件 / 拖放 | `ui-attachment` | attachment 工具（引擎侧） | 草稿图轨 + 消息图 + 灯箱 |
| 交付物 | `ui-deliverables` | file 工具产出 | turn 尾产出文件 |
| 用户提问 / 计划评审 | `ui-user-questions` | `EngineYield::ApprovalRequest`(McpElicitation/PlanReview) | composer 接管式 `QuestionComposer` + `PlanReviewPanel` 三按钮 |
| 权限预设 | `ui-permission-presets` | `PermissionProfile`（DomainPack layer 3） | `/permission` popup + General 行 |
| 消息反馈 | `ui-message-feedback` | feedback capability（引擎侧） | per-message action strip |
| **场景式对话入口**（见 §五） | dsh **无** | `group/*` + `scenario/*` + `BusScenario`（**引擎完整**） | **OneAI 差异化一等入口** |
| 设计系统（token/暗色/图标） | `ui-theme` `ui-primitives` | — | `--oneai-*` token + 自有 SVG 图标 |
| 目录浏览器（Miller 双栏） | `ui-directory-picker-browse` | workspace 概念（OneAI sidecar-cwd） | 选 sidecar cwd / 项目根 |
| 跨 session 续接 | — | `session/list` surface 未完成任务（working-state） | 侧边栏「Unfinished Work」分组 |

### 3.2 🟡 暂缺应补（值得做，但有缺口）

| 能力 | 缺口 | 建议 |
|---|---|---|
| **Web 端 StreamCoalescer** | 浏览器扩展现 per-token 派发，长流式卡 | 前端加 20fps 批量 flush（同 macOS `ChatViewModel`），**不在 app-server 出站侧加**以免破坏 bus 单消息语义 |
| **provider 配置 RPC** | app-server 启动时配 provider（`cmd_app_server.rs:337`），Web 无法热加 provider | 新增 `provider/add` `/list` `/test` RPC（或先走设置文件 + 重启），首版可只读 + 热切已有 provider |
| **DomainPack 在线选择/管理** | 启动 `--domain` 选，Web 无法切 | `domainpack/list` `/switch` RPC（复用 `PackRegistry`） |
| **skill 在线管理** | 引擎侧 `SkillCurator`/lifecycle 全套，Web 无入口 | `skill/list` `/pin` `/archive` RPC + UI（对齐 macOS） |
| **scenario 编辑器组件化** | 现 `scenario-editor.js` 是 vanilla JS 单文件 | 重写为 React 组件，但**保留 transport-agnostic 单一真相源**思路，仍给 VS Code/浏览器复用 |
| **trajectory / StateGraph 可视化** | Studio 有 D3 渲染但不走 app-server、无对话整合 | 把 Studio `graph-render.js` 抽成可复用模块，接 `EngineYield` 事件流 |
| **i18n 框架** | OneAI 有 `AppLocale`（中/英），Web 端无系统 | 引入 locale store + `t()`，驱动 re-render（同 dsh `LocaleFace`） |
| **HMR/dev 体验** | 无 | Vite dev server + app-server ws，resolve alias 指向源 |
| **sandboxed egress 可视化** | `NetworkApprovalMode{Prompt/Defer/Deny}` + `HostAllowlistStore` 引擎侧有 | `EngineYield::ApprovalRequest`(NetworkApproval) 已走统一审批通道，Web 只需在审批卡里显示 host + allow-once/always/deny |
| **usage 可视化** | `EngineYield::TokenUsage` | 底部 footer / settings 面板 |

### 3.3 🔴 暂缺不建议做（与 OneAI 架构不符 / 投入产出比低）

| 能力 | 不建议原因 |
|---|---|
| **Cordis 插件槽位系统**（dsh `ui-slots` declaration merge） | dsh 强依赖 vendored Cordis；OneAI 是 Rust workspace，无 JS 插件运行时。OneAI 用普通 React 组件树 + 路由即可，不需要运行时插件热装载 |
| **`agent.cordis.yml` preset 文件格式 / roster / composition** | dsh 的 preset = 单 agent 工具+提示词组合；OneAI 对应物是 **DomainPack**（声明式 7 层，Rust 侧）+ **场景 BusScenario**（多角色），不是 YAML 文件。复刻 dsh preset 系统是与 OneAI 架构冲突的重复造轮子 |
| **runtime module 系统 / 插件 bundle fetch**（dsh `packages/client/modules`） | 同上，OneAI 无 JS 插件分发需求 |
| **Typert RPC gateway / BFF 组装**（dsh `packages/api` `packages/typert`） | OneAI 协议层就是 app-server JSON-RPC，不需要 type-graph 代码生成 + BFF 中间层 |
| **`schema-form` 渲染框架** | dsh 的 schema-form 是 schemastery envelope 的 draft model；OneAI 设置项有限且结构固定，手写 React 表单 + 复用 `scenario/validate` 即可，不需要通用 schema 渲染器 |
| **DeepSeek onboarding 对话 / 品牌鱼 logo** | 厂商特定，换成 OneAI 品牌 |
| **`node:http` webserver + index transform taps**（dsh `packages/host/webserver`） | OneAI 静态资源直接由 Vite dev / 简单静态服务器提供，生产可由 app-server 的 ws 同进程或独立 nginx；不需要 dsh 那套 boot-manifest 注入 |
| **`__DSH_BOOT__` 启动清单 + fail-loud active sweep** | dsh 因运行时插件装载才需要；OneAI 是 build-time 静态 bundle，标准 Vite 入口即可 |
| **browser extension 作为产品形态** | 已有 `platforms/browser` 扩展（简化版）。重构后 SPA 可被扩展包装复用，但**不应把扩展当主产品**——独立 web app 才是目标 |
| **多进程并行独立 session 编排 UI** | dsh 因「一进程多独立 session 各跑 preset」才需 session-plane 编排；OneAI 单 app-server 已支持多 session，侧边栏 session 列表足够 |

---

## 四、OneAI WebUI 总体设计

### 4.1 架构选型

```
┌──────────────────────────────────────────────────────────┐
│  platforms/web/   (新, React 18 + Vite + TS SPA)          │
│  ┌────────────────────────────────────────────────────┐  │
│  │ App  ─ 三栏 AppFrame (sidebar|center|details)      │  │
│  │  ├─ SidebarRoot (session 列表 / 场景入口 / 设置)    │  │
│  │  ├─ ConversationRoot (chat + composer + approval)   │  │
│  │  └─ DetailsPanel (tool args/result / trajectory)    │  │
│  └───────────────────┬────────────────────────────────┘  │
│                       │ useSyncExternalStore over ws event │
└───────────────────────┼─────────────────────────────────┘
                        │ JSON-RPC 2.0 (ws, 默认开启)
┌───────────────────────▼─────────────────────────────────┐
│  crates/oneai-app-server  (已就绪)                        │
│   serve_ws  ←─ spawn_yield_forwarder →  event notification │
│   turn/approval/session/group/scenario/*                  │
│   SharedScenarioStore (FileScenarioStore ~/.oneai/...)    │
└───────────────────────┬─────────────────────────────────┘
                        │ Directive / EngineYield (bus)
┌───────────────────────▼─────────────────────────────────┐
│  oneai-agent AgentLoop + GroupChatSession + 各 feature    │
└───────────────────────────────────────────────────────────┘
```

**关键决策**：

1. **技术栈对齐 dsh**：React 18 + Vite + TypeScript + CSS Modules + 设计 token。理由：dsh 已验证这套栈能支撑 ~40 个 UI 域的高质量对话界面；OneAI 直接复用其设计模式与组件划分经验，降低风险。
2. **状态走 `useSyncExternalStore` 外部存储**：不复刻 Cordis，但复刻其「projection 快照 + 选择器 hook」模式。单一 `OneAiRpcClient`（ws）持 event 流，把 `EngineYield` 累积成 per-session projection 快照（`getSnapshot`+`subscribe`），组件用 `useSyncExternalStoreWithSelector` 订阅。
3. **协议直接对接 app-server ws**：不引入 BFF/Typert 中间层。出站只有 `event` 一个 notification 方法，新 yield kind 对旧前端透明。
4. **不复刻槽位系统**：用普通 React 组件树 + context provider。域边界靠目录 + lazy import 划分，而非运行时插件装载。
5. **场景编辑器单一真相源延续**：把 `platforms/shared/scenario-editor.js` 的 transport-agnostic 思路保留——Web 端场景编辑器抽成「无 transport、取 `rpc.call()`」的 React 组件，仍可被 VS Code/浏览器扩展复用（统一升级到 React，或保留 vanilla 版给扩展、React 版给 web，二者共享 `BusScenario` 类型与 validate 调用）。

### 4.2 布局

照 dsh 三栏 + 列让步几何（OneAI 自有常量）：

| 列 | 范围 | 折叠 | 说明 |
|---|---|---|---|
| sidebar | 256–420px | 退到 56px rail | session 列表 + **场景入口** + New Session + 设置触发 |
| center | floor 640px | 永不让步 | 对话主区 + composer |
| details | 300–520px | 自动关闭 | 选中 tool / trajectory / scenario 成员详情 |

窄屏 `<1024` 自动折叠 sidebar；overlay 槽承载模态/审批/命令面板。

### 4.3 组件树（OneAI 化的 dsh 划分）

```
platforms/web/src/
  app/            AppFrame (三栏 + 列让步) / app-shell / boot
  rpc/            OneAiRpcClient (ws JSON-RPC) + projection store + useSyncExternalStore bind
  theme/          --oneai-* token (base/design-platform/shiki/scrollbar) + 暗色 + i18n
  primitives/     React 原语 (Button/Menu/Modal/Tooltip/Toast) + markdown 渲染器 (micromark+shiki) + 图标
  conversation/   ConversationRoot / ChatView / AssistantMarkdown / ToolCallTree / ApprovalPanel
                  / StreamCoalescer (20fps) / composer
  sidebar/        SidebarRoot / SessionList / ScenarioEntry / WorkspacePicker
  details/        DetailsPanel / ToolDetails / TrajectoryTab
  settings/       SettingsRoot (模态) / GeneralSection / ModelsSection / DomainPackSection / PermissionsSection
  commands/       CommandPalette / InputTrigger (/ @ 检测) / PopupSelectView
  scenario/       ScenarioEntry (一等) / ScenarioEditor / ScenarioPicker (见 §五)
  attachment/     AttachmentRail / ImageLightbox / DropOverlay
  trajectory/     TrajectoryView (复用 Studio graph-render)
```

### 4.4 设计 token（`--oneai-*`）

照 dsh `design-platform.css` 结构，但换 OneAI 品牌色：

- 主色 OneAI 品牌色 + neutral 灰阶 + amber/green/red 语义色，亮/暗双套（`body[data-oneai-dark-theme]` 重定义）。
- 字体栈同 dsh（CJK 友好，代码字体不带 bare `monospace`）。
- 动效 `cubic-bezier(0.4,0,0.2,1)`，fast 0.1s / 默认 0.2s / slow 0.3s。
- 几何常量（列宽/圆角/间距）集中一处冻结。

### 4.5 流式与渲染

- **StreamCoalescer**（Web 端补齐）：hot fragment（streamChunk/thinking）攒批 ~20fps flush，`.complete`/`.error` 立即 flush，`requestAnimationFrame` 调度，避免 per-token setState 淹没 React。语义同 macOS `ChatViewModel`。
- **增量 markdown**：自研增量 React 渲染器 over micromark/mdast（可参考 dsh `markdown/`，但 OneAI 自有实现避免引入其 Cordis 依赖）；shiki 同步正则高亮器，启动装 TS/shell/JSON，其余懒加载。
- **keyed chat 节点**：每行只订阅一个 Node key，Assistant delta / Tool 生命周期只替换自己那一行不 remount（关键性能特性，照 dsh `ChatNodeSeat`）。
- **说话人路由**（group 场景）：`EngineYield::StreamChunk{speaker}` 的 `speaker` 字段驱动 bubble 切换；说话人变化时旧 item 标 done、新 item seed（照 macOS `ChatViewModel` 的 `handleSidecarEvent`）。

### 4.6 命令面板与斜杠命令

`/` 命令目录（客户端自维护，不依赖引擎协议）：

- 会话：`/new` `/sessions` `/compact` `/clear`
- 模型/权限：`/model` `/permission`
- 范式：`/plan` `/reflect` `/explore`
- **场景：`/scenario` `/new-scenario` `/edit-scenario`**（见 §五）
- 工作区：`/workspace`
- 设置：`/settings`

`PopupSelectView` 通用弹出选择 UI，`/model` `/permission` `/scenario` 共用。

---

## 五、场景式对话入口设计（OneAI 差异化一等入口）

> 这是 dsh **完全没有**、OneAI 引擎**已完整**的能力。Web 端把它做成一等入口，按 dsh 同款 UI 风格补全，正是 OneAI webUI 区别于 dsh 的核心。

### 5.1 入口位置（三条路径，复用 dsh 交互模式）

1. **侧边栏「场景」分区**（对齐 dsh `sidebar.workspaces` 槽 + `ui-workspace` 的 workspace 选择 UX）：
   - New Session 按钮下方新增 **Scenarios** 分区，列出本地 preset（`preset-*`）+ 自建 custom 场景，每项带 icon + name + 简述。
   - 点场景 → 进入 **话题收集 → 开场 → 多角色对话 → debrief** 全流程（对齐 macOS `newConversation(scenario, topicValues:)`）。
2. **new-session hero chip**（对齐 dsh `ui-agent-preset` 的 `AgentPresetSeat` staged-选择 UX）：
   - 空会话 hero 区显示场景 chip 行（「面试官 × 候选人」「辩论赛」「圆桌讨论」等），staged 选中后填充 composer 提示「选择话题进入 X 场景」。
3. **斜杠命令**（对齐 dsh `/model` `/permission`）：
   - `/scenario` 打开场景选择 `PopupSelectView`；`/new-scenario` 打开编辑器；`/edit-scenario` 编辑当前场景。

> **不复刻 dsh 的 `agent.cordis.yml` preset 文件系统**——dsh preset 是单 agent composition，OneAI 场景是 `BusScenario` 多角色编排（members + turn_policy + topic_fields + debrief）。数据模型用 `BusScenario`，编辑校验调 `scenario/validate`，存储走 `scenario/upsert`/`list`/`delete`，**与 macOS/Windows/VS Code/浏览器四端共用同一权威**。

### 5.2 场景全流程 UI（对齐 macOS/Windows 原生端，Web 化）

```
[Scenarios 分区] ──pick──▶ [ScenarioPicker]
                              │ (scenario/list, 本地 preset + server custom)
                              ▼
                       [TopicIntake]  ← topic_fields (visible_to 控制可见性)
                              │ (把 topic_values 按 visible_to 烘焙进各 member system_prompt)
                              ▼
                       group/start (BusGroupScenario) → group/open (opener)
                              │  EngineYield: SpeakerTurn + StreamChunk{speaker}
                              ▼
                       [GroupChatView]  ← 说话人路由 bubble + StreamCoalescer 20fps
                              │  (turn_policy: scripted/round-robin/moderator;
                              │   review_loop: reviewer_id/approve_marker/max_rounds)
                              ▼
                       [Debrief]  ← group/set_order 收窄到 debrief_member_id
                              │  summary_prompt → 单成员总结
                              ▼
                       [Summary]  → 可存档为新 custom 场景
```

### 5.3 场景编辑器（React 化，保留单一真相源）

把 `platforms/shared/scenario-editor.js`（248 行 vanilla）重写为 React 组件，**保留 transport-agnostic 契约**（取 `rpc.call()`，不绑 ws/stdio）：

- 字段：`id`/`name`/`icon`/`members[{agent_id, role, system_prompt}]`/`turn_policy`/`script_order`/`moderator_id`/`opener_agent_id`/`opener_line`/`topic_fields[{id,label,type,visible_to}]`/`debrief{button_label,summary_prompt,debrief_member_id}`/`review_loop`/`locale`。
- live 校验：每次编辑 debounce 调 `scenario/validate`（返 `{ok, errors:[{field,code,message}]}`），错误就地用 `ScenarioErrorLocalizer`（对齐 macOS）本地化。
- 存：`scenario/upsert`（先 validate）；删：`scenario/delete`。
- preset（`preset-*`）只读不可删，custom 可编辑。
- **复用**：VS Code/浏览器扩展可继续用 vanilla 版，或一并升级到 React 版（共享 `BusScenario` 类型 + validate 调用）。建议长期统一到 React 版，`platforms/shared/scenario-editor.{js→tsx}` 单一真相源。

### 5.4 场景预设

照 macOS `AgentStore.presets(locale:)` 的 5 个双语预设（中/英），Web 端本地生成 preset（`preset-*` id），启动 `scenario/list` 合并规则 `localPresets + serverCustoms`（server 端 `preset-*` 种子被忽略）。预设示例（对齐原生端）：

- 面试官 × 候选人（技术面试）
- 辩论赛（正方/反方/主持）
- 圆桌讨论（多专家 + 主持）
- 代码评审（作者/评审/主持）
- 头脑风暴（多角色 + 主持）

### 5.5 说话人路由与流式

- `EngineYield::StreamChunk{turn_id, text, speaker=Some(member_id)}`：`speaker` 驱动 bubble 切换；说话人变化时旧 item 标 done、新 item seed（照 macOS `ChatViewModel.handleSidecarEvent`）。
- `EngineYield::SpeakerTurn`：round 级边界，分隔不同发言者块。
- group turn 事件经 `GroupChatBusObserver` 转 speaker-tagged yields，round 级单个 `TurnComplete`——Web 端直接消费，不需特殊协议。

### 5.6 debrief

- 场景对话末尾「结束并总结」按钮（对齐 macOS `endScenarioDebrief`）→ `group/set_order` 收窄到 `debrief_member_id` → `runGroupTask(summary_prompt)` → 单成员总结显示。
- 可选：把总结存档为新 custom 场景（`scenario/upsert`）。

---

## 六、迁移路径与分阶段

### Phase W1：骨架与协议接通
- 新建 `platforms/web/`（Vite + React + TS + CSS Modules）。
- `OneAiRpcClient`（ws JSON-RPC）+ projection store + `useSyncExternalStore` bind。
- AppFrame 三栏 + 列让步几何 + `--oneai-*` token + 暗色 + i18n 骨架。
- 接通 `turn/run` + `event` 流：单 agent 对话 + 流式 markdown + StreamCoalescer 20fps。
- **验收**：能跑单 agent 流式对话，长流式不卡。

### Phase W2：对话域 + 侧边栏
- `ChatView` keyed 节点 + `AssistantMarkdown`（micromark+shiki）+ `ToolCallTree` + `ApprovalPanel`（并行审批排队）。
- `SidebarRoot` + SessionList（`session/list`/`create`/`load`/`delete`）+ 跨 session 续接（working-state「Unfinished Work」）。
- composer + 命令面板（`/model` `/permission` `/plan`）+ Plan mode chip。
- **验收**：完整单 agent 工作流，审批/工具/plan 全通。

### Phase W3：场景式对话入口（差异化一等）
- `ScenarioEntry`（侧边栏分区 + hero chip + `/scenario` 命令）+ `ScenarioPicker`。
- `ScenarioEditor`（React 化，`scenario/validate`/`upsert`/`delete`，保留 transport-agnostic 单一真相源）。
- `TopicIntake` → `group/start`/`open`/`run` → `GroupChatView`（说话人路由 + StreamCoalescer）→ `Debrief`。
- 5 预设双语 + local/server 合并。
- **验收**：选场景→话题→多角色对话→debrief 全流程，与 macOS 原生端行为对齐。

### Phase W4：能力面板 + 设置 + 可视化
- `DetailsPanel`/`ToolDetails` + subagent 目录 + workflow-run 节点 + goal bar + skill 管理。
- `TrajectoryTab`（复用 Studio `graph-render.js` 接 `EngineYield`）。
- `SettingsRoot` 模态（General/Models/DomainPacks/Permissions/Plugins）+ provider 配置 RPC（新增 `provider/*`）+ DomainPack 在线选择（`domainpack/*`）。
- 附件/拖放 + 交付物 + 消息反馈。
- **验收**：能力面板与设置完整，trajectory 可视化接 app-server。

### Phase W5：打磨与对齐
- 设计 token 逐像素对齐 + 暗色全覆盖 + 移动响应式。
- VS Code/浏览器扩展升级到 React 场景编辑器共享版。
- e2e 测试（参考 dsh `vitest.web.config.ts` + `stress-tests`）。
- 文档 `docs/webui-mechanism.md`(+`_EN`)。

---

## 七、风险与取舍

| 风险/取舍 | 说明 | 应对 |
|---|---|---|
| React 引入体积 | dsh ~40 域，bundle 较大 | 路由级 lazy import；shiki 懒加载；首屏只装对话域 |
| 不复刻槽位系统 | 失去运行时插件装载 | OneAI 无 JS 插件需求，build-time 静态 bundle 足够；用 lazy import 划域 |
| scenario 编辑器双版本 | 短期 vanilla（扩展）+ React（web）并存 | 共享 `BusScenario` 类型 + `scenario/validate` 调用；长期统一 React |
| provider 热加 | app-server 启动配 provider，Web 无法热加 | Phase W4 新增 `provider/*` RPC 或设置文件 + 重启 |
| Windows FFI 经验不可迁移 | Windows 走 P/Invoke 不走 app-server | Web 直接对标 macOS sidecar 经验（都走 app-server），不参考 Windows FFI 路径 |
| Studio 不走 app-server | trajectory/StateGraph 复用需搭桥 | 把 `graph-render.js` 抽成纯渲染模块接 `EngineYield`，不复用 Studio 的 REST 路由 |
| 流式 bus 语义 | 不在 app-server 出站侧加 coalesce | StreamCoalescer 放前端，保持 bus 单消息语义 |

---

## 八、附：dsh `ui-*` 包 ↔ OneAI 对应映射

| dsh 包 | OneAI 对应 / 取舍 |
|---|---|
| `ui-layout` `AppFrame` | 🟢 直接借鉴三栏 + 列让步几何 |
| `ui-slots` | 🔴 不复刻（Cordis 强依赖），用 React 组件树 |
| `ui-primitives` | 🟢 借鉴原语 + markdown/shiki 渲染（自实现去 Cordis） |
| `ui-theme` | 🟢 借鉴 token 结构，换 `--oneai-*` |
| `ui-conversation` | 🟢 核心借鉴（ChatView/AssistantMarkdown/ApprovalPanel） |
| `ui-sidebar` `ui-workspace` | 🟢 + 场景分区 |
| `ui-tool` | 🟢 ToolCallTree + per-tool 卡 |
| `ui-attachment` `ui-deliverables` | 🟢 |
| `ui-user-questions` | 🟢 QuestionComposer + PlanReviewPanel |
| `ui-message-feedback` | 🟢 |
| `ui-input-trigger` `ui-commands` | 🟢 |
| `ui-model-selection` | 🟢 |
| `ui-agent-preset` | 🔴 不复刻 preset 系统；其 hero chip staged-选择 UX 借鉴给场景入口 |
| `ui-permission-presets` | 🟢 |
| `ui-plan` `ui-goal` `ui-skill` `ui-subagent` `ui-workflow-run` `ui-jobs` `ui-trajectory` | 🟢 全部有引擎对应 |
| `ui-directory-picker-*` | 🟡 简化（sidecar cwd / 项目根，不一定需 Miller 双栏） |
| `ui-settings-*` | 🟢（去掉 DeepSeek onboarding） |
| `schema-form` | 🔴 不复刻，手写表单 + `scenario/validate` |
| `web-react`（bind） | 🟢 `useSyncExternalStoreWithSelector` 模式借鉴（去 Cordis） |
| `connection` | 🟢 借鉴 wire client 思路，对接 app-server ws |
| `modules` | 🔴 不复刻（无运行时插件装载） |
| `host/webserver` `host/frontend-static` `host/apiproxy` | 🔴 不复刻，直接 app-server ws + 静态服务 |
| **（dsh 无）场景入口** | 🟢 OneAI 新建一等入口（§五） |
