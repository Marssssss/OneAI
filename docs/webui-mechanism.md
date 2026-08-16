# OneAI WebUI 机制

> `platforms/web` —— OneAI 的浏览器前端：一个 React SPA，通过 WebSocket 直连 `oneai app-server` 的 JSON-RPC 2.0 协议层，与 IDE 插件 / 桌面（Swift/C#）/ 移动共享同一条前端协议。一个引擎进程喂所有这类前端；WebUI 是其中「纯浏览器」一类。本文档记录 W1–W5 重构后的最终形态（架构、协议接通、投影、流式节流、几何、场景、测试、构建）。

## 1. 概述（是什么）

WebUI 是一个 Vite + React 19 + TypeScript SPA（`platforms/web/`）。它不是独立 dev server 的产物——`vite.config.ts` 在 `serve` 阶段直接服务 SPA，浏览器打开后通过裸 WebSocket 连到 `oneai app-server --listen ws://127.0.0.1:8787`（可经 `VITE_APP_SERVER_URL` 覆盖）。无 HTTP 路由代理：ws 与 vite 的 5173 是两个独立 host，浏览器各自直连。

它**不复刻 dsh 的 Cordis/slots 插件装载系统**——OneAI 无 JS 插件需求，build-time 静态 bundle 足够；用路由级 lazy import + shiki 懒加载划域即可。设计 token 走 `--oneai-*` CSS 变量层，主题切换是单属性翻转。

## 2. 分层（关键）

- **L4 引擎**（不变，`oneai-agent`+`oneai-app`）：`AgentLoop` + `BusObserver` + `BusInteractionGate`，无感于外面是 web 还是 TUI。
- **L3 bus**（不变，`oneai-bus`）：`Directive`/`EngineYield` newline-JSON + `InProcessBus`。app-server 把它适配成 JSON-RPC。
- **L2 app-server**（`oneai-app-server`）：JSON-RPC 2.0 适配——inbound `turn/run`→`UserMessage` 等；outbound 把 `bus.subscribe_yields()` 封成单一 `event` 通知（`params` = 完整 `EngineYield`，含 `kind` tag）。WebUI 只是一个 ws 客户端，不感知 L2/L3 内部。
- **L1 WebUI**（本文档）：`OneAiRpcClient`（ws JSON-RPC）→ `ProjectionStore`（yield→UI 状态投影）→ React（`useSyncExternalStore` 绑定）。

## 3. 进程拓扑

```
浏览器 ──ws──> oneai app-server --listen ws://127.0.0.1:8787 ──bus──> AgentLoop
            (JSON-RPC 2.0: turn/run·session/*·scenario/*·approval/respond …)
            <──event── (EngineYield: turn_start·stream_chunk·tool_result·turn_complete …)
```

一个 app-server 进程可并发喂 TUI（in-process）、IDE（stdio）、web（ws）、桌面（ipc）多类前端；WebUI 是其中 ws 一类。`oneai serve`（newline-JSON passthrough sidecar）是 escape hatch，与 `app-server` 用不同 socket 共存。WebUI 默认连 `app-server`。

## 4. 协议接通（OneAiRpcClient）

`src/rpc/client.ts`。一个 `OneAiRpcClient`：

- **call<P,R>(method, params)**：发 `{jsonrpc:'2.0', id, method, params}`，`id` 单调递增；await 到匹配 `id` 的响应（`result`/`error`）。`turn/run` 的响应是 `{turn_id}`（TurnStart 即返）——流式 chunk 是后续 `event` 通知，不在响应里。
- **notify(method, params)**：fire-and-forget 通知（无 id）。
- **onEvent(fn)** / **onStatus(fn)**：注册 `event` 通知回调与连接状态回调。
- **inbound `handleMessage`**：解析 JSON 帧——有 `id` 的是响应（resolve pending）；无 `id` 的是通知（`event`/`session_created` 等）→ 派发到 `EventListener`。

类型契约在 `src/rpc/types.ts`：`EngineYield` 是 `#[serde(tag="kind", rename_all="snake_case")]` + `#[non_exhaustive]` 的 TS 镜像——前端只枚举当前消费的 variant，**必须忽略未知 `kind`**（匹配 Rust 契约，新 variant 透明）。

## 5. ProjectionStore——yield→UI 状态投影

`src/store/projection.ts` 是单向投影器：`consume(y: EngineYield)` 把每条 yield 折算进一个 `WorkingState`，再经 `emit()` 构建不可变快照（`ProjectionSnapshot`）通知 React。**hot 变体（`stream_chunk`/`thinking`）走 coalescer 延迟通知；其余（`turn_start`/`tool_result`/`turn_complete`/`approval_request`/`session_*` …）立即通知**（`emitNow()`）。

- `subscribe`/`getSnapshot`：`useSyncExternalStore` 契约。React 拿到的 `snapshot.nodes` 是浅拷贝新数组；节点对象按 edit 替换（不原地改），memoized 行组件靠 diff 跳过未变行。
- `attach()`：在 `useEffect` 里把 `rpc.onEvent` 接到 `consume`——而非构造函数。StrictMode 下 effect 会模拟卸载再重挂，构造函数里的订阅不会重建（`useMemo` 单例构造函数不重跑）→ 事件断流。effect 驱动使订阅/退订对称。
- 投影产物：`ChatNode` 树（user/assistant/system · text/thinking/tool/plan/error · streaming/done/error）+ `approvalQueue`（并行审批排队，head 即 `currentApproval`）+ `trajectory`（turn 级事件账本）+ `working`（goal/steps/decisions/blockers）+ `subagents` + `usage` + `turnTimings`。

## 6. StreamCoalescer——20fps 前端节流

`src/stream/coalescer.ts`。app-server **不在出站侧 coalesce**——保持 bus 单消息语义。节流放前端：

- `request()`（hot 路径）：dedupe + 在 ~50ms（20fps）窗口后于 rAF 排空。底层可变状态**立即**更新（同步读总是最新文本），只是通知 React 被合并。
- `flushNow()`（终态/非 hot 路径）：无条件立即排空并取消 pending coalesced 排空。`pushUserMessage`/`turn_complete`/`session_loaded` 依赖此路径——**不能**用 `dirty` 标志门控，否则会静默吞掉。
- rAF 不可用时退回 `setTimeout(fn,0)`（jsdom/SSR 安全），生产硬化 + 测试友好。

## 7. AppFrame——列让步几何（桌面）+ overlay 抽屉（移动）

`src/layout/AppFrame.tsx` + `.module.css`。镜像 dsh 的 `AppFrame` + `computeColumns`：

- **桌面**：三栏 grid `sidebar | gutter | center | details`，列宽由 JS 求解器 `computeColumns(width, prefs)` 决定，列数必须匹配渲染子节点数（多一个 gutter 会把 center 挤进 0px 列）。让步顺序：先收 details 到 min→0（auto-close），再收 sidebar 到 56px rail，最后才让 center 跌破 640 地板。拖拽 gutter 改 `sidebarWidth`（sticky prefs，跨重挂存活）。
- **移动**（`width ≤ --oneai-mobile-breakpoint:900px`）：放弃 grid，改单列 + 顶栏（汉堡键 + 标题 + 快捷主题切换）+ 侧栏 `position:fixed` overlay 抽屉（`transform: translateX` 滑入）+ 遮罩。details 隐藏（次要面板）。抽屉状态由 App 控制（controlled），故选会话/场景后能自动收起；离开移动宽度时 effect 收起避免残留。Esc / 遮罩点击关闭。触摸目标走 `--oneai-touch-target:44px`（仅移动断点内放大，桌面密度不动）。

## 8. 场景式对话 + 群聊说话人路由

场景编辑器 `src/scenario/`（`ScenarioEditor` React 版，`scenario/validate`/`upsert`/`delete` 走 `ScenarioListStore`，单一真相源 transport-agnostic）+ 5 双语预设。`compile.ts` 把 `BusScenario`+话题值编译成引擎 `BusGroupScenario`：把话题背景块按 `visible_to` 拼进各 member 的 `system_prompt`，标题由「场景名·话题值」组成。

群聊流程：`group/start`→`group/open`（opener 先说）→`group/run`（每轮）→`speaker_turn`（轮级边界，finalize 上一个说话人节点）→`stream_chunk` 带 `speaker` id（前端按 member 名/色路由气泡）→`turn_complete`。Debrief 按钮在场景配置了 `debrief` 且未触发时显示。

> **W5 取舍**：VS Code/浏览器扩展**暂未**迁移到 React 共享版，仍用 vanilla `platforms/shared/scenario-editor.js`（`sync.sh` 同步）。设计文档风险表本身写「长期统一 React」，本轮规避 webview CSP/bundle 风险。

## 9. 设置 / probe RPC / provider 池化

`SettingsRoot`（General/Models/Permissions）+ `SkillsModal` + `DomainPackModal` 走 `SettingsStore`→`config/get`·`provider/list`·`domainpack/list`·`skill/list`·`config/read`（`AppProbe` trait 穿透 `serve_all` 链）。provider 全 live 管理：`provider/add`·`delete`·`set_active` 改 `ProviderPool` 的 `Arc<RwLock<Vec>>`（snapshot 模式，clone 出后 drop 守卫不跨 await），active_index live 切不重建 App。架构红线：DomainPack 热切需 `--domain` 重启（`App.domain_pack` 是 immutable Arc，无池化绕开路径）。

## 10. 附件 / 交付物 / 消息反馈

- **附件**（W4）：Composer 拖放/粘贴/📎→base64 image `ContentBlock`→`turn/run` 的 `content` 直灌引擎。
- **交付物**（W4）：`ToolOutput.artifacts`（`write_file`/`apply_patch` 填）→`turn_complete` 时聚合同 turn 所有 tool artifacts→挂到该 turn 最后一个 assistant text 节点（DeliverableStrip）。
- **消息反馈**（W4）：`FeedbackStore` trait + SQLite `message_feedback` 表；`feedback/submit`·`feedback/list` 同步 CRUD（镜像 `session/list`，不经 bus）。反馈键=节点运行时 `turn_id`，**只对当前对话新鲜模型输出开放反馈**——历史会话回放节点 `turnId` 恒 null → 不显 👍/👎（设计理由：反馈是对新鲜输出的反应，非历史追溯）。

## 11. 暗色 token + 响应式

`src/theme/tokens.css`：`--oneai-*` 双套（`:root` light / `body[data-oneai-theme='dark']` dark redefinition）。组件**只用 token，不硬编码色**。`readInitialTheme`（`src/theme.ts`，单元可测）：显式 localStorage 选择 wins（`explicit=true`，停止跟随）；否则用 OS `prefers-color-scheme` 且 `explicit=false`——App 的 `matchMedia` change listener 持续跟随 OS 直到用户手动 toggle（显式选择优先语义）。移动断点 `--oneai-mobile-breakpoint` + 触摸目标 `--oneai-touch-target` 见 §7。

## 12. 测试矩阵

两层，均在 `platforms/web/`，`npm run test` / `npm run e2e`：

- **vitest**（逻辑层 + 组件，jsdom）：`src/stream/coalescer.test.ts`（20fps/flushNow/dirty）、`src/scenario/compile.test.ts`（话题编译/visible_to）、`src/store/projection.test.ts`（喂 yield 序列→断言 ChatNode 树/approvalQueue/trajectory/deliverables，直接 `consume(y)` 灌入，fake timers + `advanceTimersToNextFrame` 排空 rAF drain）、`src/theme.test.ts`（readInitialTheme 三分支）。`ProjectionStore.consume` 因此公开为 test seam。
- **Playwright e2e**（mock ws 端到端）：`playwright.config.ts` 的 `globalSetup` 起一个 `ws` 服务（`e2e/mock-server.ts`，:8788）讲同一条 JSON-RPC，回放脚本化 yield（`e2e/fixtures/scripts.ts`）而非真跑引擎——确定性、零 Rust。`webServer` 跑 `vite`（`VITE_APP_SERVER_URL` 指 mock）。用例：chat（流式回复）、approval（tool 暂停→respond→续）、theme（body 属性翻转）、responsive（375×812 汉堡+抽屉）。`fullyParallel:false` + `workers:1`（mock 单共享服务器）。

## 13. 构建

`npm run build` = `tsc --noEmit && vite build`（`dist/`）。`npm run typecheck` 单独 tsc。shiki 语言包懒加载；首屏只装对话域。bundle 较大（shiki 全语言）属已知，可后续 `manualChunks` 优化。npm devDep 不经 cargo 供应链门（`Cargo.lock`/`deny.toml` 不受影响）；`npm audit` 须 0 漏（vitest 升 4.x 清零 esbuild 旧链传递）。

## 13.5 一行启动（`oneai web`）

对标 deepseek-harness `npx @deepseek-ai/dsh web`：**一个命令**拉起引擎 + webUI + 浏览器，零源码、零额外进程。引擎进程同端口（axum）既托管 SPA 静态 dist、又提供 `/ws` JSON-RPC 升级——`oneai-app-server` 的 `serve_web`（feature `http`，axum0.8+`tower-http fs`）把 `ServeDir`（含 `index.html` SPA fallback）+ `WebSocketUpgrade` 挂到一路由，`serve_ws_axum` 桥接 axum WebSocket 到既有 `serve_connection` 的 `mpsc<String>` seam（复用全部 JSON-RPC 处理，零重复）。

- **运行**：`npx oneai-cli web`（npm 包内带 dist、postinstall 拉平台二进制）或 `oneai web`（全局/cargo 装）。默认 `http://127.0.0.1:8787`，自动开浏览器（`--no-open` 跳过，`--port/--host/--dist/--domain/--model/--user` 可配）。
- **ws URL 自洽**：`App.tsx` 在 `VITE_APP_SERVER_URL` 未设时从页面 origin 派生 `${wss?}://${host}/ws`，故同一份 dist 在任意 host:port 都能用，无需按端口重构建；dev（`npm run dev` on 5173）仍可用 env 覆盖指向独立 app-server。
- **dist 来源**：web dist 是平台无关 JS，打进 `oneai-cli` npm tarball（`npm publish` 的 `prepublishOnly` 跑 `platforms/npm/scripts/build-web.sh` 构建+stage 进 `web-dist/`，gitignore）；launcher 启动时注入 `ONEAI_WEB_DIST` 指向包内 dist。cargo/二进制用户走 `--dist` 或自动探测（`./platforms/web/dist` / `~/.oneai/web-dist`）。
- **复用**：CLI 的 `build_engine_server`（抽自 `cmd_app_server`，建 app+pool+stores+probe+pump）被 `cmd_web` 与 `cmd_app_server` 共用，引擎构建一处。`serve_web` 内部建 `Dispatcher`+`subscribe_yields`（同 `serve_all`）。

## 14. 深入阅读

- `docs/webui-refactor-design.md`（+`_EN`）—— W1–W5 分阶段迁移路径与取舍表。
- `docs/app-server-mechanism.md`（+`_EN`）—— L2 JSON-RPC schema、Dispatcher、多 transport。
- `docs/bus-mechanism.md`（+`_EN`）—— L3 `Directive`/`EngineYield` 语义。
- `docs/cross-platform-mechanism.md`（+`_EN`）—— 四类原生前端与 app-server 的关系（web 是其中一类）。
