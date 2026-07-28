# Hermes-Agent & PI-Agent 深度研究 → OneAI 前进方向

> 研究日期：2026-07-28
> 研究对象：[NousResearch/hermes-agent](https://github.com/NousResearch/hermes-agent)（Python，221k★，"the agent that grows with you"）、[earendil-works/pi](https://github.com/earendil-works/pi)（TypeScript monorepo，79k★，"self-extensible coding agent"）
> 研究方法：浅克隆两仓库（`/tmp/agent-research/{hermes-agent,pi}`），分 5 个子代理深读源码并按 file:line 取证，对照 OneAI 现有架构（CLAUDE.md + memory）综合而成。
> 目的：提取两个项目的**先进设计思想**，启发 OneAI 做得更好，形成可指导前进方向的文档。

---

## 0. TL;DR — 五个最关键的启发 + 一条戒律

1. **Prompt caching is sacred（Hermes）**：长对话每轮复用缓存前缀，任何改动过去上下文 / 交换工具集 / 重建 system prompt 都会让缓存失效、成本翻倍。OneAI 的 `apply_paradigm_switch` **重建 system prompt** 正是 Hermes 禁止的行为——这是本次研究发现的**最高优先级架构张力**，必须调和（见 §4-P0）。

2. **窄腰 + 能力在边缘 / Footprint Ladder（Hermes）**：每个 model tool 每次调用都要发送，所以核心工具门槛极高。能力应按阶梯落地：扩展现有代码 → CLI+skill → service-gated tool(`check_fn`) → plugin → MCP → core tool（最后手段）。OneAI 的 DomainPack 缺一条"`check_fn`：前置条件缺失时**从 schema 中彻底隐藏**工具（零足迹）"，这是补强 DomainPack 故事最便宜的一步。

3. **闭环学习（Hermes）——"随你成长的代理"**：三套独立 loop——①每 N 轮 fork 一个 reflect 子代理回看对话、原地 patch skill/memory；②闲置触发的 curator 把窄 skill 归并为类级 umbrella，永不删除只归档、可回滚；③跨会话 FTS5 全文检索 + Honcho 辩证用户建模。OneAI 有 skill-creator 但**全靠用户手动触发**，没有 cadence-fired 自主改进、没有 skill 生命周期状态机、没有 curator——这是 OneAI 从"带 skill 的代理"变成"随你成长的代理"的最大缺口。

4. **纯循环 + 有状态外壳的分缝（PI）**：PI 把 `agentLoop` 做成"对快照的纯 async generator"，状态全在 `Agent` 外壳里（队列、生命周期、listener 扇出）。`continue()` / `shouldStopAfterTurn` / `prepareNextTurn` 都是一行回调。OneAI 的 `AgentLoop` 更丰富（paradigm/delegate/StateGraph）但把这些塞进 match 臂——这条**架构分缝**值得迁移，与具体功能无关。

5. **供应链与发布纪律（PI）——OneAI 最弱项**：精确版本锁定 + `min-release-age` + lockfile 提交闸 + lifecycle-script allowlist + lockstep 版本 + 隔离安装冒烟 + OIDC 可信发布 + provenance + 每日 `npm audit signatures`。OneAI v1.1.0 发往 crates.io + 原生 app，却用 caret 版本、零漏洞扫描、手动 `cargo login` token、无隔离冒烟——这是**发布侧的硬伤**，对一个"框架"尤其致命。

戒律：**不要抄 PI 的"无内置权限系统"**。OneAI 的三层 `Read/Standard/Full` + `InteractionGate` 五决策点是优势，是 PI 没有、还得靠容器化绕回来的东西。PI 的极简在这里是教训的反面——OneAI 应吸收的是 PI 的**Gondolin 综合**（auth 留宿主、工具副作用路由进 micro-VM、靠 tool-override 实现），而非砍掉权限模型。

---

## 1. 两个项目是什么

| 维度 | Hermes-Agent | PI-Agent |
|---|---|---|
| 定位 | 自我进化的**个人**代理（CLI / TUI / 桌面 / 消息网关） | 精简**自扩展**的 coding agent + 统一 LLM API 工具链 |
| 语言/形态 | Python 单仓（7659 文件，maximalist） | TS monorepo（1148 文件，minimalist） |
| 哲学 | "核心窄腰，能力在边缘"；prompt cache 神圣不可侵犯 | "极简核心 + 自扩展"；无权限系统，靠容器化隔离 |
| 差异化 | 闭环学习（skill 自创建/自改进/curator 归并）、多平台网关、cron、serverless 终端后端、trajectory 压缩喂训练 | `addedToolNames`（工具结果声明新工具）、jiti 热重载扩展、生成式 model catalog + 哈希校验、Bun 独立二进制、OIDC 可信发布 |
| 工程纪律 | AGENTS.md = 设计意图层（Footprint Ladder / 贡献评判 / "验证前提"） | AGENTS.md = 硬纪律（可擦 TS、无 inline import、lockstep、并发会话 git 规则） |

**定位矩阵**：Hermes = "广而活"（一个进程服务 ~20 消息平台 + cron + 6 种终端后端，scale-to-zero）；PI = "窄而硬"（4 个包，核心 ~5 文件，把权限/容器/选模型/UI 全留给上层）。OneAI 的位置：**框架层**（DomainPack 7 层声明式 + 原生多端 + AgentLoop 动态引擎），介于两者之间——既有 Hermes 的广度野心（MCP/A2A/StateGraph/eval/studio），又有 PI 该有的工程硬度。

---

## 2. 横切设计哲学（真正可迁移的思想）

### 2.1 Prompt caching is sacred — 与 OneAI paradigm-switch 的张力

**Hermes 的做法**（`agent/system_prompt.py:549-585`、`AGENTS.md:19-23,1133-1145`）：
- system prompt 按 `stable / context / volatile` 三层**构建一次**，缓存于 `agent._cached_system_prompt`，会话期内**永不重渲染**。唯一清缓存入口 `invalidate_system_prompt()`，**只在上下文压缩后**调用。
- 时间戳用 `strftime('%A, %B %d, %Y')`——**日级精度**，因为分钟级变化会让 KV 缓存每次重建失效。模型需要精确时间时用工具查。
- 静态前缀再切一刀：对外请求时按 `static_system_prefix` 精确匹配拆成 `[static_prefix, suffix]`，给 stable 前缀独立缓存断点。
- 4 个 `cache_control` 断点（Anthropic 上限），carrier-aware 跳过空消息。
- **严格角色交替**：永不允许连续两条同角色消息、永不中途注入合成 user 消息。压缩 summary 反而翻成 `role="user"` 只为满足交替（`context_compressor.py:2050-2057`）。
- skill 斜杠命令**注入为 user 消息而非 system prompt**，保缓存；改 system prompt 的命令默认**延迟到下个会话**生效，`--now` 显式 opt-in。

**OneAI 的张力**：`apply_paradigm_switch` + `AgentLoopGraphActionExecutor` 在模型/工作流请求切换 Plan/Reflect/Explore 范式时**内联升级 system prompt + 工具过滤**——这正是 Hermes 列为"会让缓存失效、成本翻倍"的行为。OneAI 把范式塞进 system prompt，Hermes 把一切塞进 user 消息。

**调和方向**（见 §4-P0）：把 system prompt 拆成**缓存稳定前缀**（身份/能力/权限/记忆锚——会话期字节稳定）+ **范式后缀**（Plan/Reflect 指令作为**可丢弃的尾部 user/system 注入**，或干脆只换工具集不换 prompt）。范式切换应**只动工具过滤 + 末尾追加一段指令**，绝不重写前缀。时间戳改日级精度。

### 2.2 窄腰 + 能力在边缘 / Footprint Ladder

**Hermes 阶梯**（`AGENTS.md:182-211`）：扩展现有代码 → CLI+skill → service-gated tool(`check_fn`) → plugin → MCP server in catalog → new core tool。**"ABC + orchestrator"规则**：当 3+ PR 集成同一*类别*（memory 后端 / provider / notifier），不要一个个合，先设计 ABC + orchestrator，把内置包成第一个 provider。cron 栈即范例：`CronScheduler` ABC + `run_one_job` orchestrator + `InProcessCronScheduler` 内置 + `ChronosCronScheduler` plugin。

**`check_fn` 实操**（`toolsets.py:68,79,427`）：Home Assistant 工具 gated on `HASS_TOKEN`、computer-use gated on `cua-driver` 已装、桌面工具 gated on `HERMES_DESKTOP`。前置条件缺失 → **该工具根本不进发给模型的 schema**（零足迹，不是"禁用"）。**延迟加载**：`platform_registry` 每平台注册一个廉价 deferred loader，真模块（discord.py/slack_bolt 等重 SDK）只在首次查找时 import——否则每次 `hermes chat` 都多几秒。

**对 OneAI**：DomainPack 7 层已是声明式，但缺 `check_fn`——OneAI 的工具是"装了就在 schema 里"。补一个 `service_available()` 过滤（前置缺失则排除出 schema）即可让 DomainPack 获得"零足迹-未配置"属性。`ToolRegistry` 已按范式过滤，加一个 service 检查是低投入高清晰度。**延迟加载**对 OneAI 的 provider/MCP 也有意义：当前所有 provider 编进每个二进制。

### 2.3 闭环学习 — "随你成长的代理"（Hermes 最大差异化）

三套独立 loop（全在 `agent/`）：

**Loop A — 每轮后台 review（自主改进 nudge）**（`background_review.py:635`、`turn_finalizer.py:649-659`）：
- 两个独立计数器：memory cadence（轮次，默认 10，`turn_context.py:567`）+ skill cadence（迭代，默认 10，`conversation_loop.py:1291`）。**从持久化历史 hydrate**，重启不丢进度。
- 触发点在 `final_response` 交付后、且 `not interrupted`——review 永不与用户任务争模型注意力。
- fork 的 AIAgent **继承父运行时**（同 provider/model/key）→ 命中父预热的前缀缓存（实测 ~26% 成本下降）。但硬关 DB 写入 / 压缩 / 外部 memory provider 写入（`_persist_disabled=True`、`compression_enabled=False`），只共享 session_id 取暖——否则 fork 把 harness prompt 写进用户真实会话行，下轮被当常驻指令读回，"代理变成了 curator"。
- 工具白名单只 `memory`+`skill_manage`；`max_iterations=16`；递归 nudge 关掉（fork 自己的 interval=0）。
- 路由到更便宜的 aux 模型时改放**紧凑 digest**（不同模型=冷缓存，少写即纯赚）；同模型则全量重放。
- **学习策略**写进 prompt（`background_review.py:170-387`）：主动而非被动（"什么都不做的 pass 是错失学习机会，不是中性结果"）；更新偏好序（patch 已加载 skill → patch umbrella → 加 support 文件 → 新建 class-level umbrella 最后手段）；**frustration 是一等 skill 信号**（"太啰嗦/别这样/记住这个"→嵌进治理该任务的 skill）；**反模式**（不捕获环境依赖失败"command not found"、不把"browser 工具不能用"硬成约束——"会变成代理拿来说服自己拒绝数月的断言"）。

**Loop B — curator（闲置触发的库维护）**（`curator.py`）：
- 非定时，而是 `idle_for_seconds >= min_idle_hours`（默认 2h）+ 距上次 ≥ interval（默认 7d）才触发；首次故意延后一个完整周期，防新装立刻改库。
- 两阶段：①`apply_automatic_transitions` 纯函数无 LLM，按 `last_activity_at` 走 active(30d)→stale→archived(90d)；pinned 和**被 cron 引用的 skill** 永不动（否则暂停的 cron 任务会失去指令）；use_count==0 给 30 天宽限（"没用过是证据缺失，不是陈旧证据"）。②LLM consolidation（默认关，opt-in）做 umbrella 归并，max_iterations=9999，prompt 强制"几百个各捕获一次会话的窄 skill 是库的失败而非特性"。
- **可恢复性是承重的**：`curator_backup.py` 每次突变前 tar.gz 快照 `~/.hermes/skills/`+`cron/jobs.json`，留 5 份，`rollback()` 自己先再快照一次（可撤销的撤销）。**永不删除只归档**，`.archive/` 可 `hermes curator restore`。
- 归并时**重写 cron 引用**：skill X 并入 umbrella Y 时，列了 X 的 cron 任务就地改引用 Y，否则定时任务会静默无指令运行。

**Loop C — 跨会话召回 + 用户建模**（`memory_provider.py:43`、`memory_manager.py:364`）：
- `MemoryProvider` ABC：`initialize/system_prompt_block/prefetch/queue_prefetch/sync_turn/get_tool_schemas/handle_tool_call` + 可选 `on_turn_start/on_session_end/on_pre_compress/on_memory_write/on_delegation/backup_paths`。
- `MemoryManager` 编排内置 + **至多一个**外部 provider（防 schema 膨胀和后端冲突）。所有 sync/prefetch 跑**单 worker 后台执行器**，把 turn N 串行在 turn N+1 前，provider 不必自管顺序——教训来自一个配错的 Hindsight daemon 阻塞 298s 让所有界面标"running"数分钟。
- Honcho 辩证用户建模：两层注入**user 消息**（不动 system prompt 保缓存），`<memory-context>` 围栏。基础层每 `contextCadence` 轮（会话摘要/用户表征/peer card/AI 自表征）；辩证补充层每 `dialecticCadence` 轮做多 pass `.chat()`，`dialecticDepth` 1-3 控 pass 数（2=审计+综合，3=+矛盾调和）。三正交旋钮：多久一次、几 pass、多用力。
- FTS5 跨会话全文检索（`hermes_state.py`）：三张虚拟表 `messages_fts`(unicode61)/`_trigram`(CJK 子串)/`_cjk`(自定义可加载分词器)，对**每条曾持久化的消息**做跨会话搜索。

**对 OneAI 的缺口**（详 §3）：OneAI 的 skill-creator 是**用户手动触发**的；没有 cadence-fired 自主 review、没有 skill 生命周期状态机、没有 curator、没有 FTS5 跨会话检索原始 transcript。OneAI 的 memory 已有 Letta 三层 + Mem0 冲突更新 + 语义/关键词召回，但召回的是**事实**不是**原始对话**。

### 2.4 纯循环 + 有状态外壳的分缝（PI）

**PI 的分缝**（`packages/agent/src/agent-loop.ts:155-275` + `agent.ts:171-577`）：
- `agentLoop` 是**纯 async generator**，对 `AgentContext` 快照产 `AgentEvent`，**不持状态**，返回 `newMessages`。状态全在 `Agent` 类（生命周期 + 队列 + listener 扇出）。
- **双层队列**：外层 drain *follow-up* 队列（"代理本该停时队列塞进工作，让它继续"）；内层 drain *steering* 消息——在当前 assistant turn 的工具调用**完成后、下次 LLM 调用前**注入，而非流式中途。
- **终止不由计数器管**，由三个可组合信号：①assistant 无 toolCall；②`shouldStopAfterTurn` 回调（压缩前溢出的典型）；③**整批工具结果全部 `terminate:true`** 才停（批级共识，非单工具）；④`stopReason==="length"` → **整批 tool call 全失败而非执行可能截断的参数**（流式 tool-call 参数能 JSON parse 但其实被截断——真实健壮性细节）。
- `prepareNextTurn` 是**轮间突变钩子**，可在 turn 间换 context/model/thinkingLevel——这正是 OneAI `apply_paradigm_switch` 占的缝，但 PI 把它留成通用回调返回 `AgentLoopTurnUpdate`。
- `continue()` 从已有 context 续跑不加消息（最后消息须是 user/toolResult）——干净的 retry 原语，OneAI 无直接等价。

**迁移价值**：OneAI 的 `AgentLoop` match 臂里塞了范式切换/delegate/StateGraph。把"对快照的纯循环"和"有状态外壳（队列+生命周期）"分开，能让 `continue`/`shouldStopAfterTurn`/`prepareNextTurn` 成一行回调。这是**架构迁移**，独立于任何单功能。

### 2.5 自扩展三机制（PI 的 headline）

PI 的"self-extensible"**不是 codegen**，是三层叠加，第一层对 OneAI 是全新的：

**(a) 工具结果声明新工具 `addedToolNames`**（`types.ts:362-364` + `extensions/wrapper.ts:17-37`）：工具执行可返回"这些工具名自此 transcript 点起可用"。wrapper 算 `getActiveTools()` 执行前后的 diff，盖到结果上；agent-session 立即 `_refreshToolRegistry()`（`agent-session.ts:2520-2545`），**同一会话内、无需 reload**。机制：工具 execute 内调 `pi.registerTool()+setActiveTools()` → wrapper 检测 diff → 结果带 `addedToolNames` → 会话刷新注册表 → 下轮 system prompt 含新工具。模型调 `setup_db`，下轮 `query_db`/`migrate_db` 就出现了。

**(b) jiti 热加载扩展**（`extensions.md`、`loader.ts`）：`~/.pi/agent/extensions/*.ts`（全局）或 `.pi/extensions/*.ts`（项目级，经 trust）自动发现，用 jiti 加载所以 TS 免编译。`export default function(pi: ExtensionAPI)` 注册 tools/commands/shortcuts/flags/providers/event handlers。`/reload` 会话中热重载扩展/skill/prompt/theme/context-file；文档展示一个工具把自己排队成 `/reload-runtime` follow-up，让 LLM 自己触发重载。

**(c) skill = 渐进披露 markdown**（agentskills.io spec）——OneAI 已有等价（skill-creator + progressive disclosure）。

**对 OneAI**：OneAI 的 `meta_tool.rs` 注入 `delegate`/`switch_paradigm`，但**工具不能在运行中声明新工具**；OneAI 刷新工具集靠 DomainPack 切换，不是每轮从工具输出。`addedToolNames` 是 PI 最紧的自扩展环，与 OneAI 模型驱动的 `delegate` 同源（更细粒度版）。热重载对 OneAI 的**数据层**（skill markdown / MCP / MemoryProfile JSON / StateGraph 文件）可行；Rust DomainPack 仍是编译期。

### 2.6 声明式 provider 兼容性 + 生成式 catalog（PI）

**`Api`/`Provider` 分缝**（`models.ts:75-120,556-623`、`types.ts:16-72`）：PI 把**线协议**（`KnownApi`: openai-responses / anthropic-messages / google-generative-ai / bedrock-converse-stream / pi-messages…）和**provider 身份**（40+ provider ID）分开。一个 provider 可暴露多个 Api；一个 Api（如 anthropic-messages）被 Anthropic/Bedrock/Fireworks/Kimi/ZAI 复用。加一个 OpenAI 兼容 provider 是一行配置对着一个 `openai-responses` Api impl，不是新 trait impl。OneAI 的 `LlmProvider` trait 把这俩**焊在一起**——加 Ollama/vLLM/LM Studio/Groq 都要新 impl。

**每模型 `Compat` 能力标志**（`types.ts:509-642`）：`OpenAICompletionsCompat`/`AnthropicMessagesCompat` 各 ~20 个标志，从 `baseUrl` 自动探测——`supportsDeveloperRole`/`supportsReasoningEffort`/`maxTokensField`/`thinkingFormat`(10 变体)/`supportsLongCacheRetention`/`sendSessionAffinityHeaders`/`supportsStrictMode`/`cacheControlFormat`…这是 PI 处理 40+ "OpenAI 兼容但各有微妙破损"的方式——能力标志**每模型**，非每 provider。OneAI 今天硬编码 `if provider == "ollama"` 分支。

**生成式 catalog + 哈希校验**（`scripts/generate-models.ts` + `scripts/model-data.ts`）：build 时从 `models.dev/api.json` + OpenRouter/NVIDIA/CF 等活源拉取，合并手工修正文件，发射 TS shard + JSON 数据文件 + `.manifest.json`（schema 版本 + 结构哈希 + 每文件 SHA256）。`check:model-data` 在 CI 里**重推导期望结构、逐文件验哈希、逐字段校验**，不符硬失败最多列 30 个错。release 源码归档内嵌快照，包维护者可 `--offline-model-data` 可复现重建。**OneAI 的 `BUILTIN_MODEL_CONTEXT` 手维护且漂移**；L2 provider probe 异步、依赖网络、首轮路径慢。

**对比 OneAI 的三层解析**：OneAI 是**运行时**解析（L1 用户 config > L2 活探测 > L3 内置表），PI 是**构建时**生成快照 + 运行时同步查表。两者非互斥——OneAI 可把生成式 catalog 作 L3 的权威替代，把 probe 降级为可选覆盖。

### 2.7 供应链与发布纪律（PI）——OneAI 最弱项

| 机制 | PI | OneAI 现状 |
|---|---|---|
| 精确版本锁定 | `.npmrc save-exact=true` + `check-pinned-deps.mjs` CI 强制 | caret 版本 `tokio={version="1"}`，无强制 |
| 最小发布年龄 | `min-release-age=2`（拒绝 2 天内发布） | 无 |
| lockfile 提交闸 | `check-lockfile-commit.mjs` pre-commit，未设 `PI_ALLOW_LOCKFILE_CHANGE=1` 则拒 | 无，自由提交 Cargo.lock |
| 生命周期脚本 allowlist | `generate-coding-agent-shrinkwrap.mjs` 每个 `hasInstallScript:true` 须在 allowlist | cargo build script 默认全跑 |
| lockstep 版本 | `sync-versions.js` 强制所有包同版本 | 各 crate 独立继承 workspace 版本，无强检 |
| 隔离发布冒烟 | `release:local` 打包后在**仓外**安装、跑 `--help/--version/--list-models/"Say exactly: ok"` + tmux 交互 | `publish_crates.sh` 直接 `cargo publish`，无隔离冒烟 |
| 独立二进制 | Bun 跨编译 darwin/linux/win 全架构 standalone | 原生 staticlib/framework 脚本，无 standalone 二进制 |
| 可信发布 | tag 触发 CI，OIDC 铸 provenance 短命 token，`publish.mjs` 幂等跳过已发布 | 手动 `cargo login` 长命 token，无 provenance，无幂等 |
| 漏洞扫描 | `npm-audit.yml` 每日 cron + `npm audit signatures` 验签名 | 零 `cargo audit`/`cargo deny` |

OneAI v1.1.0 发往 crates.io + 5 端原生 app，这整列都是**发布侧硬伤**。

### 2.8 无头监督进程 + 崩溃恢复（PI `pi-server`）

`packages/server/src/`：Unix socket IPC 守护进程，**监督长寿命 agent 子进程**，桥到远程 Radius 网关。`Supervisor` 持 `live_instances: Map`，每个 `LiveInstance` = 持久化 `InstanceRecord`（写 `instances.json`）+ 活 `RpcProcessInstance`（spawn `pi --mode rpc` 子进程，JSON-RPC over stdin/stdout）+ subscribers。`recoverAfterRestart()` 重连扛过 daemon 崩溃的实例。`SIGINT/TERM` 序列化 `shutdown()`。子进程崩了只标 error、不拖垮 supervisor。

**OneAI 的痛点**：原生 app（macOS/Win/iOS/Android/Harmony）会话**随 UI 进程死**。`FileWorkingStateStore` 持久化任务状态但不持久化"活会话重连句柄"。一个 `oneai-supervisor` 后台 daemon 持有长寿命 AgentLoop 进程、UI 重启可重连，能修 OneAI "后台/被杀就会话丢失"问题。

### 2.9 容器化优于权限的张力 + Gondolin 综合（PI）

PI **无内置权限系统**（`containerization.md`），三模式：Gondolin（工具路由进 QEMU micro-VM，auth 留宿主）/ Plain Docker / OpenShell。**Gondolin 最妙**：`pi` 跑宿主（auth/OAuth 留宿主），一个 tool-override 扩展按名覆盖 `read/write/edit/bash/grep/find/ls` 的 execute，执行进 micro-VM，挂载宿主 cwd 到 `/workspace`。靠的是 PI 的 **tool-override 机制**（扩展可注册同名内置工具替换 execute，`--no-builtin-tools` 零内置起步）+ `ReadOperations/WriteOperations/...` Remote Operations 接口（工具委托给 SSH/容器而不重写 UI）。

**张力**：OneAI 的三层权限是**飞行前、进程内、每调用**——便宜、跨 DomainPack 可组合（strictest-wins）、配原生对话框，但进程本身有全宿主权限。PI 的容器化是**事后、进程外、粗粒度**——不能逐调用问"这个 rm -rf 该跑吗"（除非扩展自建 gate），但有权限系统给不了的**文件系统/进程隔离**（被攻陷的代理逃不出 VM）。

**Gondolin 是综合**：auth 留宿主 + 工具副作用进 VM + 靠 tool-override。OneAI 的 `ShellTool` blacklist/sandbox 是其弱化版。**OneAI 应吸收的是 Gondolin，不是砍权限**——一个"容器化 CodingPack"：DomainPack 的 PermissionProfile 保持 `Full`（VM 即边界），内置 `read/write/bash` 工具按名被 pack 提供的 VM-backed 实现覆盖。这需要 OneAI 的 `ToolRegistry` 支持**按名覆盖**（当前只 add）。

### 2.10 行级差分 TUI（PI-TUI）

PI-TUI 是**手搓框架**，非 ratatui。三策略：首渲染全吐不清屏；全重渲染（宽/高变、内容缩、`firstChanged<prevViewportTop`）；**差分**——逐行比 `previousLines` vs `newLines` 找 `firstChanged/lastChanged`，移光标、清到尾、只渲染变行。关键：**同步输出** `\x1b[?2026h…l` 包裹每次更新（原子、不闪）；**渲染合并定时器**——一 tick 内多个 `requestRender()` 合成一次（"别每 token 重绘"）；**每行宽度契约** `render(width)` 不得超宽否则报错。对比 ratatui **cell 级**差分（算 diff 网格吐变 cell），PI 是 **line 级**——粗，但行是预样式字符串，diff 是平凡字符串相等，成本 O(lines) 非 O(rows×cols)。对"底部追加行 / 某行 spinner 变"的 chat TUI，行级 diff + 只吐变行远便宜于 cell 网格。

**对 OneAI**：OneAI 的 TUI perf 笔记（`tui-render-perf-fix.md`/`stream-macOS-mainqueue-flooding.md`/`macos-streaming-freeze-lazyvstack.md`）**反复重发现**这个道理；macOS 的 `StreamCoalescer 20fps` 是同思路再发明。PI 有一个成熟的独立答案。最小胜场是**合并定时器**——给 OneAI TUI 渲染路径加 `render_requested` 标志 + `set_timeout` 去抖（对标 `tui.ts:740-759`）。

### 2.11 多平台网关 + cron + serverless 终端后端（Hermes——OneAI 最大缺失面）

**网关**（`gateway/`）：一个进程服务 ~20 消息平台，靠 `BasePlatformAdapter` ABC（只 `connect/handle_message/send`）+ `self.adapters: dict` asyncio 多路复用，每平台自带重连监督。**能力标志而非子类分支**：`supports_code_blocks/supports_status_text/supports_async_delivery/splits_long_messages/typed_command_prefix`。规范化 `MessageEvent` 信封 + `SessionSource`（platform/chat_id/thread_id/user_id）绑进 **task-local contextvars**（不是 `os.environ`——后者让并发消息 A/B 互踩 `HERMES_SESSION_THREAD_ID`、把通知路由错线程）。`channel_directory.json` 缓存友好名→ID，5 分钟重建，于是 Telegram 起的对话能在 Discord 续。profile routing 按 (guild/channel/thread) 特异性评分路由到不同 profile（各自 model/tools/memory/persona）。

**cron**（`docs/chronos-managed-cron-contract.md` + `cron/`）：核心是**让托管网关 scale-to-zero 时仍能 fire cron**——agent 请 NAS 在每个任务真实下次 fire 时间 arm 一个外部 one-shot，NAS 到点回调 `POST /api/cron/fire`（短命 NAS 铸 JWT，`purpose=cron_fire`，agent 不持调度方凭证）。三跳信任。自然语言调度 `parse_schedule("30m"/"every 2h"/"0 9 * * *"/ISO)`。`deliver="origin"` 把结果送回建任务的频道。store 级 CAS `claim_job_for_fire` 实现**跨副本 at-most-once**。启动/唤醒时 reconcile 自愈补缺/取消孤儿，**无周期性唤醒沉睡代理**。

**serverless 终端后端**（`tools/environments/`）：6 个 backend（local/docker/ssh/singularity/modal/daytona）共 `BaseEnvironment` ABC，spawn-per-call `bash -c`，session 快照重 source。**Modal** `sandbox.snapshot_filesystem()`+terminate，存 `object_id`，下会话从快照 restore——FS 跨 sandbox 销毁存活，Modal 只在 sandbox 活时计费→**闲置零成本**。**Daytona** `auto_stop_interval=0`，cleanup 时 `sandbox.stop()`（FS 保留）非 `delete()`，下会话 `start()` 复跑。配 `FileSyncManager` 双向同步 `~/.hermes`（凭证/skill/缓存）。配网关 `scale_to_zero.py`（idle 检测→`go_dormant()`，Fly `autostop:"suspend"` 冻整机，`wakeUrl` 唤醒），**整个 agent 进程闲置可零成本**，cron 和消息 relay 是唯一唤醒源。

**OneAI 现状**：原生 app 是**单进程 UI 客户端**，非多平台消息多路复用器；`oneai-scheduler` 是 tokio 定时器的 `InMemoryScheduler`，**重启即死**、无 NL 调度、无外部 one-shot、无 scale-to-zero；`ShellTool` 直接 `Command::new` **本地唯一**，无 backend ABC、无快照/停-续持久化、无 hibernate-on-idle。这是 OneAI "可达性"与"成本"两侧最大缺口。

---

## 3. OneAI 的强项（不该抄的地方）

研究也是对 OneAI 的肯定——以下 OneAI 领先，PI/Hermes 没有：

- **三层权限 + InteractionGate 五决策点**（PreInfer/PostInfer/ToolApproval/PlanDecision/PlanReview）+ 原生平台 NSAlert/AlertDialog/UIController 对话框——PI 无、靠容器化绕；Hermes 有 `check_fn` 但无 OneAI 这套统一 gate。
- **DomainPack 7 层声明式可组合**（多域 strictest-wins 权限合并、priority merge context sources）——Hermes 靠 plugin/skill 拼凑，PI 靠扩展文件。
- **AgentLoop 动态引擎**：DirectAnswer/ToolCalls/Delegate/SwitchParadigm + StateGraph↔AgentLoop 闭环 + 并行多 delegate（Kahn wave）——PI 是固定两层队列、Hermes 是 conversation_loop。
- **memory eval**（LongMemEval 5 能力 + Mem0 F1/BLEU1 + Recall@k/NDCG）+ **SWE-bench 三轴**（能力×成本×效率）+ `efficiency.rs`——PI eval 是 ~9KB 委托 vitest-evals，Hermes 无。
- **WASM 沙箱、A2A server host、MCP host、Studio（axum+WS+D3 StateGraph viz+checkpoint 时间旅行）、SQLite 持久化跨会话 resume、working-state 事件日志投影**——PI 全无、Hermes 部分有但形态不同。
- **5 端原生**（macOS SwiftUI/WinUI3/Android/iOS/Harmony）+ GroupChat 原语——两项目都无此覆盖。

结论：OneAI 的缺口集中在**①发布侧工程纪律 ②闭环学习 ③可达性（网关/cron/serverless）④provider 抽象现代化**，不在 agent 框架层本身。

---

## 4. 建议路线图（按优先级）

每条：What / Why / How-to-adapt（OneAI 是 Rust，有 DomainPack/InteractionGate/AgentLoop/原生端）/ Effort / Fit。

### P0 — 立即（低投入、高杠杆、修硬伤）

**P0-1 cache-stable system prompt + 范式后缀化**（解 §2.1 张力）
- What：拆 system prompt 为**稳定前缀**（身份/能力/权限/记忆锚/工具索引，会话期字节稳定，构建一次缓存）+ **范式尾部**。范式切换只动尾部追加 + 工具过滤，绝不重写前缀；时间戳降日级精度。范式指令走 user 消息或可丢弃尾部 system，skill 斜杠命令注 user 消息。
- Why：OneAI 每轮重发全上下文本就贵，paradigm-switch 重写 system prompt 让 Anthropic 前缀缓存**每轮失效**——这是直接成本。
- How：`crates/oneai-agent/src/context_assembler.rs` + `system prompt` 构建逻辑。把范式 prompt 从"整段换"改成"append-only 尾部"。`apply_paradigm_switch` 改为只调 `ToolRegistry` 过滤 + 追加尾部指令。对齐 Hermes `stable/context/volatile` 三层 + `invalidate_system_prompt` 仅压缩后调。
- Effort：中。Fit：高，直击现有架构最大张力。

**P0-2 `check_fn` Footprint gate on ToolRegistry + DomainPack**（补 §2.2）
- What：给 `Tool` 注册加 `service_available: Option<fn()->bool>`；返回 false 的工具**从发给模型的 schema 中彻底排除**（零足迹，非禁用）。DomainPack Tool 层加 `gated_tools: Vec<(Tool, fn()->bool)>`。文档把 Footprint Ladder（extend→skill→service-gated tool→plugin→MCP→core tool）写进 CLAUDE.md 作为新能力决策准则。
- Why：OneAI 工具"装了就在 schema"。`check_fn` 让 per-domain/per-config 工具表小而聚焦，直接改路由质量、降 prompt token。是补强 DomainPack 最便宜的一步。
- How：`crates/oneai-tool` 的 `ToolRegistry` 过滤路径（已按范式过滤）加 `service_available()` 检查。`InteractionGate::PreInfer` 可顺带 surface"工具 X 已配置但前置缺失"。低投入高清晰度。
- Effort：低。Fit：极高，落在现有 DomainPack/InteractionGate 上。

**P0-3 供应链纪律：精确锁定 + cargo audit/deny + lockfile 闸 + OIDC 发布 + 隔离冒烟**（解 §2.7）
- What：(a) CI 强制每个 `Cargo.toml` 外部 dep 精确版本（`=1.2.3`，对标 `check-pinned-deps`）。(b) `.github/workflows/audit.yml` 每日 `cargo audit` + `cargo deny check`（advisories+license+build-scripts allowlist）。(c) `Cargo.lock` 提交闸（未设 `ONEAI_ALLOW_LOCKFILE_CHANGE=1` 则拒）。(d) tag 触发 CI OIDC 可信发布到 crates.io + provenance，`publish.mjs` 式幂等跳过已发布。(e) `release:local` 等价：`cargo package`→仓外 `cargo add`→跑 `say-exactly-ok` 冒烟再 tag。
- Why：OneAI v1.1.0 发 crates.io + 5 端原生 app，这整列是发布侧硬伤——对一个"框架"尤其致命。
- How：脚本 + workflow，对标 PI 的 `.npmrc`/`check-pinned-deps.mjs`/`check-lockfile-commit.mjs`/`npm-audit.yml`/`local-release.mjs`/`publish.mjs`。crates.io 已支持 GitHub OIDC 可信发布。
- Effort：低-中（纯工具链）。Fit：高，无关架构、纯收益、可立即做。

### P1 — 近期（中投入、补能力缺口）

**P1-1 闭环学习：cadence-fired reflection meta-tool + skill 生命周期 + curator**（解 §2.3，OneAI 最大差异化缺口）
- What：
  1. `AgentLoop` 加 turn/iter cadence 计数器（`AgentLoopConfig`，默认 0=关，向后兼容；从 `FileWorkingStateStore` hydrate）。跨阈值、`DirectAnswer` 交付后、`not interrupted` 时，经现有 `delegate`/`SubAgent` spawn 一个 **reflect 子代理**，工具白名单只 `memory`+`skill_manage`，跑 Hermes 式 review prompt（frustration-as-signal、patch→umbrella→support-file→create 偏好序、反模式禁捕获环境失败）。复用 `InteractionGate::PostInfer` gate 触发。
  2. 给 OneAI skill 加 `SkillState`(Active/Stale/Archived)+`use_count`+`last_activity_at`+`pinned`+`created_by`(Agent/User/Bundled)。`SkillTool` 每次调 `bump_use`。纯函数 `apply_automatic_transitions`（30d/90d，pinned+cron-referenced 豁免，use==0 宽限）。永不删除只归档 + `curator_backup` tar.gz 快照留 N=5 + 可撤销 `rollback`。归并时重写 workflow/StateGraph 的 skill 引用。这整体作 DomainPack 一层（`MemoryProfile` 内或新 curator 子配置）。
  3. LLM consolidation **默认关 opt-in**。
- Why：把 OneAI 从"带 skill 的代理"变"随你成长的代理"。复用现有 SubAgent/delegate/AgentLoopGraphActionExecutor/FileWorkingStateStore，不新子系统。
- How：reflect 子代理必须 (a) 继承父 provider 取暖、(b) `WriteOrigin::BackgroundReview` 标记、(c) **持久化隔离**——共享 session_id 取暖但硬关 `SqliteSessionStore` 写和压缩（对标 `_persist_disabled`/`compression_enabled=False`）、(d) 关递归 nudge、(e) `max_iterations=16`。`auxiliary.background_review.{provider,model}` 经 `SmartRouter` 路由便宜模型 + digest 重放。把学习策略 prompt 移植进 OneAI 的 `skill-creator` skill body。CLI `oneai curator run/status/restore/pin` 对标 `pack validate`。
- Effort：中-高。Fit：高，落在现有 SubAgent/DomainPack/FileWorkingStateStore 上。**这是 OneAI 最该做的差异化**。

**P1-2 headless 监督进程 `oneai-supervisor`**（解 §2.8）
- What：后台 daemon 监督长寿命 AgentLoop 进程，持久化 `~/.oneai/server/instances.json` 实例注册表，`recover_after_restart()` 重连。UDS（Unix）/ named pipe（Win）`~/.oneai/server.sock`，`spawn/list/stop/status/rpc/rpc_stream`。原生 app 作 IPC 客户端重连。
- Why：OneAI 原生 app 后台/被杀就会话死。修"会话存活"问题。
- How：对标 `pi-server`。先做 in-proc supervised tokio task，进程隔离作 opt-in 后续。复用 `oneai-trace` OTEL。可作 `oneai-studio` 的一个 mode。
- Effort：中。Fit：高，直击原生 app 痛点。

**P1-3 生成式 model catalog + 哈希校验 + 每模型 Compat 标志**（解 §2.6）
- What：(a) build 时从 models.dev + provider `/models` 生成 `crates/oneai-provider/src/catalog/models.generated.rs`（`pub static MODELS: &[ModelEntry]`，内嵌 JSON）+ `.manifest.json`（schema 版本+结构哈希+每文件 SHA256）。`cargo xtask check-model-data` 重推导验哈希跑 CI。release 内嵌快照 `--offline`。(b) `ModelEntry` 扩展带 `reasoning/input_modalities/thinking_format/supports_strict/cache_retention`。(c) `compat.rs` 加 `OpenAICompat`/`AnthropicCompat`/`GeminiCompat` 标志集，从 `base_url` 探测，取代 `if provider=="ollama"` 分支。
- Why：OneAI 的 L3 表漂移、L2 probe 异步慢、能力硬编码 per-provider。OpenAI 兼容面在涨（Ollama/vLLM/LM Studio/Groq），各微妙破损。
- How：把生成式 catalog 作三层解析的 L3 权威替代，probe 降级可选覆盖。`Api`/`Provider` 分缝是更大重构（可后续），先上 Compat 标志 + catalog。
- Effort：中。Fit：高，现代化 provider 层、喂 SmartRouter 能力感知路由。

### P2 — 中期（新用户面 / 自扩展 / 训练数据）

**P2-1 消息网关 `oneai-gateway` + ChannelDirectory + SessionSource + profile routing**（解 §2.11 上半）
- What：新 `oneai-gateway` crate（peer `oneai-a2a`）暴露 `MessagePlatform` trait（`connect/handle_message/send` + 能力标志作 default trait method）+ `PlatformRegistry`（延迟加载）+ 规范 `MessageEvent`+`SessionSource`。一个 `GatewayRunner` tokio 多路复用。`ChannelDirectory`（友好名→ID，定期重建）+ `tokio::task_local!` 带 SessionSource（防并发互踩）。`ProfileRoute` 表 (platform/guild/channel/thread)→DomainPack，特异性评分，`AgentLoop` 在 `context_assembler` 时选 pack。
- Why：OneAI 原生 app 是单进程 UI 客户端；网关让 OneAI 变"随处可达的代理"（Telegram bot/Discord bot/Slack app）。是最大新用户面。
- How：网关坐 `AppBuilder` 之上如 `oneai-a2a` server host：`AppBuilder::gateway_runner()` 每 `MessageEvent` 绑 SessionSource 驱动 AgentLoop。复用 `FilePersistence`/`SqliteSessionStore` 存跨 channel session origin。首 2-3 adapter（Telegram `teloxide`/Discord `serenity`/Slack）。profile routing 复用 `MergedDomainPack` + `apply_paradigm_switch` 机制扩展到换整 pack。**能力标志作 default trait method**是关键可移植教训。
- Effort：高。Fit：高，落在现有 AppBuilder/A2A server host 范式上。

**P2-2 cron ABC + 外部 one-shot provider（Chronos 式）**（解 §2.11 中）
- What：把 `InMemoryScheduler` 提为一个 `CronScheduler` 式 trait 后的一个 provider（最小面 `name/start`，`fire_due/reconcile/on_jobs_changed` 安全默认）+ `FileJobStore`（JSONL，对标 `FileWorkingStateStore`）+ `parse_schedule("30m"/"every 2h"/cron/ISO)`+ `deliver="origin"` + 外部 one-shot provider（`provision(job_id,fire_at,callback_url)` + axum `/cron/fire` JWT 验证）+ store 级 CAS at-most-once。
- Why：OneAI 调度器重启即死、无 NL 调度、无外部触发。cron 是触达真实世界的 agent 的顶级用户请求。依赖 P2-1 网关做投递。
- How：`InMemoryScheduler` 留零配默认。`AppBuilder::cron_provider(...)` 加法。投递复用网关 `send()`。**ABC+orchestrator 模式**（Hermes AGENTS.md:208-211）套到现有调度器。
- Effort：中。Fit：高。

**P2-3 终端 backend ABC + Modal/Daytona serverless**（解 §2.11 下）
- What：`oneai-tool` 抽 `TerminalBackend` trait（`execute/snapshot/restore/cleanup(hibernate:bool)`），`ShellTool` 持 `Arc<dyn TerminalBackend>` 而非直 `Command::new`。`LocalBackend`=现状零行为变，`DockerBackend`（最便携）→ Modal/Daytona。`cleanup(hibernate=true)` 是单 chokepoint（Modal snapshot+terminate、Daytona stop FS 保留）。`FileSyncManager` 同步 `~/.oneai`。
- Why：OneAI ShellTool 本地唯一。serverless 给 (a) 成本(b) 隔离(c) 跨平台一致(d) 配网关 scale-to-zero。直接赋能移动端长跑 coding agent。
- How：spawn-per-call+session 快照模型直接移植。DomainPack PermissionProfile 可让 `Full` 风险命令自动路由 sandbox backend。先 Local=现状，加 Docker，再 Modal/Daytona。**为 P2-4 PTC 预留 host**。
- Effort：中-高。Fit：高，独立价值（隔离+成本+移动 coding）。

**P2-4 自扩展：`addedToolNames` + 数据层热重载**（解 §2.5）
- What：(a) 工具结果加 `added_tool_names: Vec<String>`，`AgentLoop` 在工具批 finalize 后 diff `ToolRegistry` active 集，新注册工具（execute 内 `ToolRegistry::register`）盖到结果上，下轮前 `refresh_tool_registry()`，不换范式/不重启。`PermissionLevel::Standard` gate + DomainPack `ParadigmStrategies` 工具过滤刷新后重应用。(b) `/reload` 等价：会话中重读 DomainPack 数据层（skill markdown/MCP/MemoryProfile JSON/StateGraph），发 `reload` 事件，可由模型经控制工具触发（对标 PI 的 follow-up 排队）。Rust DomainPack 仍编译期。
- Why：`addedToolNames` 是 PI 最紧自扩展环，OneAI 无。热重载是迭代 agent 行为的大 UX 胜场（macOS 笔记显示用户重启重派生状态很痛）。
- How：(a) `Tool` 结果类型加字段，turn 边界加 refresh（对标 `agent-session.ts:2520-2545`）。(b) `AppBuilder` 持 file-watched `ResourceLoader`，`reload_runtime` 控制工具排队 follow-up（对标 `extensions.md:1300-1327`），快照 working-state→拆扩展 runtime→重读→`OnResume` 对账。
- Effort：中。Fit：中-高。

**P2-5 OSS 会话导出喂训练 `oneai session export-hf`**（解 §2.7 末 + Hermes trajectory）
- What：CLI 子命令导 OneAI 会话（conversation + working-state 事件 + tool calls/results）为 HF-dataset 兼容 JSONL，正则脱敏（API key/token），`huggingface-cli upload`。配 `oneai-eval` 把导出会话当 eval case 重放（扩 `replay.rs`）。
- Why：OneAI 有原始材料（`FileWorkingStateStore` JSONL + conversations）但无真实会话捕获 pipeline。这造训练数据飞轮 + 真实世界 eval 语料（超 SWE-bench curated 玩具任务）。配 Hermes 的 trajectory 压缩（ShareGPT 格式、protect head+tail、snap boundary 不落在 tool turn）可作 SFT/RL 数据。
- How：`Conversation`/`Message` 已 serde-serializable。`secrecy` crate 脱敏。对标 `pi-share-hf` + Hermes `trajectory_compressor.py`。
- Effort：低-中。Fit：高，扩现有 eval/replay。

### P3 — 远期（精细化 / 综合）

**P3-1 行级差分 TUI / 渲染合并定时器**（解 §2.10）：最小先上**合并定时器**（`render_requested`+`set_timeout` 去抖，对标 `tui.ts:740-759`），大胜场是 chat 面板绕 ratatui 全重绘吐 ANSI diff 行包 `?2026` 同步输出。Effort 低-中。

**P3-2 Gondolin tool-override + Remote Operations 接口**（解 §2.9）：`oneai-tool` 定义 `BashOperations/ReadOperations/...` trait，`ShellTool`/`ReadTool` 持 `Box<dyn BashOperations>`；`ToolRegistry` 支持**按名覆盖**（当前只 add）；`ContainerizedCodingPack` DomainPack 提供 VM-backed 同名工具，PermissionProfile 保持 `Full`（VM 即边界）。这是 PI 极简 + OneAI 权限的诚实综合。Effort 中-高。

**P3-3 `Api`/`Provider` 分缝**（解 §2.6 深）：把 OneAI `LlmProvider` trait 拆成线协议（`Api`）+ provider 身份，OpenAI 兼容 provider 作配置行对一个 `openai-responses` Api impl。是 P1-3 的更大后续。Effort 高。

**P3-4 `observe`/`on` hook 分缝 + per-event-type 突变语义**（解 PI hooks）：`InteractionGate` 拆 observe（只读）/on（参与语义）两通道，每事件类型自带 result 型 + merge 策略（phantom-type），新增 `before_provider_request`/`before_compact`。`InteractionGate` 降为默认 impl 注册 `on(...)` handler，向后兼容。Effort 中-高。

**P3-5 纯循环/有状态外壳分缝**（解 §2.4）：把 `AgentLoop` 的纯循环抽成对快照的 generator，状态移到有状态外壳，`continue`/`shouldStopAfterTurn`/`prepareNextTurn` 变回调。架构迁移，独立功能。Effort 高。

---

## 5. 不做什么（戒律）

抄 PI/Hermes 时要守的边界，多数来自 Hermes `AGENTS.md` 的"我们不想要"段，与 OneAI 已有纪律对齐：

- **不抄 PI 的"无权限系统"**。OneAI 三层权限 + InteractionGate 是优势。吸收 Gondolin 综合，不砍权限。
- **不把第三方产品塞进 core 树**。observability 后端 / vendor SaaS / analytics dashboard 作**独立 plugin 仓**装进 `~/.oneai/plugins/`，不在 `crates/` 里维护别人的产品。这是耦合-维护决策，非质量门槛。
- **不写 speculative 基础设施**。无具体消费者的 hook/callback/扩展点不加——加 hook 容易，插件依赖后移除难。（消费者即使分开发布也算"非 speculative"。）
- **不留 lazy-reading 逃生口**。对 agent 必须全文读的工具（skill/prompt/playbook）不加 `offset/limit` 分页——模型会只读第一页跳过其余。
- **不用 env 变量做非密配置**。`.env` 只放凭证；超时/阈值/feature flag/显示偏好走 `config.yaml`/DomainPack。需要内部 env 桥接可以，但用户面文档指向 config。
- **不写 change-detector 测试**。测断言"两数据必须如何关联"（不变量），非冻结当前值（模型列表/枚举数/版本字面量）。
- **不让"修复"毁掉它保护的功能**。读原 commit 意图（`git log -p -S`）再限制行为；找保功能的修法。
- **不在 mid-conversation 破缓存**（与 P0-1 一致）：mutate 过去上下文 / 换工具集 / 重建 system prompt 是禁忌，唯一例外是压缩。
- **不并行多个 memory provider**（OneAI 已守：至多一外部 provider），防 schema 膨胀 + 后端冲突。

---

## 6. 需要决策的架构张力点

1. **paradigm-switch vs cache**（P0-1）：范式指令走 user 消息 vs 可丢弃尾部 system vs 只换工具不换 prompt？三者对缓存友好度递增、对范式表达力递减。**推荐**：稳定前缀 + 尾部追加 + 工具过滤，范式 prompt 作尾部 user 注入。
2. **DomainPack vs Footprint Ladder**：`check_fn` 作 DomainPack Tool 层字段（声明式、随 pack 走）还是作 `ToolRegistry` 全局机制？**推荐**：两层都有——Tool 层声明 gated_tools，ToolRegistry 统一执行过滤。
3. **curator 是 DomainPack 层还是独立 crate**？**推荐**：作 `MemoryProfile`（layer 7）子配置 + `oneai-skill` 内实现，对标 `WorkingStatePolicy` 折进 layer 7 的先例。
4. **网关是 `oneai-app` 之上的 adapter 还是独立 crate**？**推荐**：独立 `oneai-gateway` crate（peer `oneai-a2a`），坐 AppBuilder 之上，与 A2A server host 对称。
5. **生成式 catalog 取代 L3 还是叠加**？**推荐**：取代 L3 为权威，L1 用户 config 仍可覆盖，L2 probe 降级为可选（仅 catalog 缺失时）。

---

## 附录 A：关键文件证据索引（取证 file:line）

**Hermes（`/tmp/agent-research/hermes-agent`）**
- 学习 loop：`agent/background_review.py:635,170-387,744,837-853`；`agent/curator.py:305,1497,417`；`agent/curator_backup.py:216,544`；`agent/learning_graph.py:254`；`agent/memory_manager.py:364,414,698`；`agent/memory_provider.py:43`；`agent/onboarding.py:170`；`hermes_state.py:1428,1492,1582`（FTS5 三表）
- 缓存/上下文：`agent/system_prompt.py:523-585,152-169`；`agent/prompt_caching.py:54-218`；`agent/context_compressor.py:4868-5469,2050-2057`；`agent/context_engine.py:89-490,194-211`；`agent/conversation_compression.py:1047-1068,1746-1757,2276-2285`；`agent/context_breakdown.py:232-257`；`agent/trajectory.py:30-56`；`trajectory_compressor.py`（ShareGPT、protect head/tail、snap boundary）
- 网关/cron/backends：`gateway/platforms/base.py:2471,2532,1879-1901`；`gateway/session_context.py:10-25,158`；`gateway/channel_directory.py`；`gateway/profile_routing.py:62-72`；`docs/chronos-managed-cron-contract.md`；`cron/jobs.py:550`；`cron/scheduler_provider.py:10-13,27`；`tools/environments/{base,modal,daytona}.py`；`tools/code_execution_tool.py:1-29,424,492`；`toolsets.py:31,68,79,427`；`toolset_distributions.py`
- 哲学：`AGENTS.md:19-23,88-91,182-211,1133-1145`

**PI（`/tmp/agent-research/pi`）**
- agent core：`packages/agent/src/agent-loop.ts:155-275,411-554,600-754`；`packages/agent/src/agent.ts:171-577,349-377`；`packages/agent/src/types.ts:327-437`；`packages/agent/docs/hooks.md:9-392`
- 自扩展：`packages/agent/src/types.ts:362-364`；`packages/coding-agent/src/core/extensions/wrapper.ts:17-37`；`packages/coding-agent/src/core/extensions/loader.ts`；`packages/coding-agent/docs/extensions.md:1,1273-1327,2020-2081`；`packages/coding-agent/docs/skills.md`
- TUI：`packages/tui/src/tui.ts:691-759,1040-1510`；`packages/tui/README.md:592-728`
- 测试：`packages/ai/src/providers/faux.ts:49-73`；`packages/coding-agent/test/suite/harness.ts:72-209`；`packages/coding-agent/docs/containerization.md:1-43`
- provider/catalog：`packages/ai/src/models.ts:75-120,127-187,463-487,556-623,639-693`；`packages/ai/src/types.ts:16-72,79-98,359-642,491-503`；`packages/ai/src/api/lazy.ts:38-71`；`packages/ai/src/utils/retry.ts:97-186`；`packages/ai/scripts/generate-models.ts:910-1057`；`packages/ai/scripts/model-data.ts:90-260`
- server：`packages/server/src/{serve,supervisor,rpc-process,handler,radius}.ts`；`packages/server/src/ipc/protocol.ts`
- eval：`packages/evals/src/pi-harness.ts:1-180`；`packages/evals/src/extensions.eval.ts`
- 供应链：`.npmrc`；`scripts/check-pinned-deps.mjs`；`scripts/check-lockfile-commit.mjs`；`scripts/generate-coding-agent-shrinkwrap.mjs:13-15,241-260`；`scripts/sync-versions.js`；`scripts/local-release.mjs`；`scripts/build-binaries.sh`；`scripts/create-source-archive.sh`；`scripts/publish.mjs`；`.github/workflows/build-binaries.yml:223-269`；`.github/workflows/npm-audit.yml`；`.husky/pre-commit`；`AGENTS.md`（Releasing 段）

---

## 附录 B：OneAI 对应现状（对照锚点）

- system prompt / 范式：`crates/oneai-agent/src/agent_loop.rs`（`apply_paradigm_switch`）、`crates/oneai-agent/src/context_assembler.rs`
- 工具/权限：`crates/oneai-tool/src/tool_interfaces.rs:54,521`（ShellTool 本地）、`crates/oneai-core/src/traits.rs:182`（InteractionGate 5 点）、`ToolRegistry`
- 调度：`crates/oneai-scheduler/src/scheduler.rs:27`（InMemoryScheduler，重启即死）
- provider/模型：`crates/oneai-provider/`（OpenAI/Anthropic/Ollama/Gemini）、`crates/oneai-core/src/model_context.rs`（三层解析 + `BUILTIN_MODEL_CONTEXT`）
- 记忆：`crates/oneai-memory/`（Letta 三层 + Mem0 冲突更新 + `MemoryFactStore` 接 VectorBackend）
- skill：`crates/oneai-skill/`（skill-creator + progressive disclosure + 约定目录发现）
- eval：`crates/oneai-eval/src/`（EvalCase/EvalMetric/EvalRunner + 6 metrics + 3 suites + memory + swebench + efficiency + replay）
- 发布：`Cargo.toml:73-88`（caret 版本）、`scripts/publish_crates.sh`、`.github/workflows/ci.yml`
- 原生端：`crates/oneai-platform-{desktop,android,ios,harmony}/`

---

*本文档由 5 个并行子代理深读两仓库源码取证综合。子代理完整报告存于研究上下文。后续如需就单条深入（如 P1-1 闭环学习的 prompt 移植、P2-1 网关 trait 设计），可基于附录 A 的 file:line 直接展开。*
