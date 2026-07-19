# OneAI 多 Agent 机制白皮书

> 版本：对应代码库 `0.2.0` / 1.0.0 线。本文基于对 `crates/oneai-agent`、`oneai-workflow`、`oneai-domain`、`oneai-memory`、`oneai-core` 源码的逐文件审阅撰写，所有机制均标注 `file:line` 以便核对。文末与业界前沿多 Agent 系统（LangGraph / AutoGen / CrewAI / OpenAI Swarm / MetaGPT / SWE-agent / Claude Code 子代理 / Google A2A / MCP）对标。
>
> 说明：撰写时本环境无法联网检索，前沿对标部分基于截至 2025 年初的训练知识，已尽量标注可核对的论文/项目名；具体版本号以各项目最新发布为准。

---

## 0. 一句话概括

OneAI 的多 Agent 系统是一个 **「Claude-Code 式动态 Agentic Loop + 模型驱动的 delegate/switch_paradigm 元工具 + LangGraph 式可循环 StateGraph 闭环 + 四种编排原语（Team/Swarm/Handoff/GroupChat）+ 压缩耦合记忆」** 的引擎：每一轮迭代由模型决定下一步是直接作答、调工具、委托子 Agent、还是切换范式——不是固定管线。委托支持一轮多委托 + 依赖感知的 Kahn 波次并行调度；范式切换可内联升级系统提示与工具集，并可挂载 DomainPack 预定义的 StateGraph 图流；多 Agent 协作既能在主 Loop 内通过子 Agent 分层分解，也能经由 Team/Swarm/Handoff/GroupChat 原语做聚合/路由/移交/对话。整个编排行为由 DomainPack 声明，一行 `AppBuilder::domain_pack(...)` 切换。

---

## 1. 架构总览：分层与执行模型

```
                         ┌──────────────────────────────────────────────┐
                         │            AppBuilder (oneai-app)             │
                         │  所有子系统可选插拔 → App → AppSession         │
                         └──────────────────────┬───────────────────────┘
                                                │ 组装
                         ┌──────────────────────▼───────────────────────┐
                         │              AgentLoop (oneai-agent)         │
                         │  动态循环：infer → parse_decision → 分发       │
                         │  决策：DirectAnswer / ToolCalls / Delegate /  │
                         │        SwitchParadigm                         │
                         └──┬───────────┬───────────┬───────────┬───────┘
                            │           │           │           │
            ┌───────────────▼─┐ ┌───────▼────────┐ │  ┌────────▼──────────┐
            │ ContextAssembler│ │ ToolExecutor   │ │  │ StateGraphExecutor │
            │ + Pinned 注入    │ │ + 域权限/审批    │ │  │ (AgentLoopGraph    │
            │ + 抗压缩重注入   │ │ + SmartRouter  │ │  │  ActionExecutor)   │
            └─────────────────┘ └────────────────┘ │  └────────────────────┘
                                ┌─────────────────▼─────────────┐
                                │  委托 / 编排原语层             │
                                │  · SubAgentWrapper(+worktree) │
                                │  · spawn_sub_agents_batch     │
                                │    (Kahn 波次 DAG 调度)        │
                                │  · TeamCoordinator (4 策略)   │
                                │  · SwarmOrchestrator (3 路由) │
                                │  · HandoffTool/Manager        │
                                │  · GroupChatSession           │
                                │  · AsyncTaskRunner (后台)     │
                                └───────────────────────────────┘
                                                │
                                ┌───────────────▼───────────────┐
                                │  长程支撑                      │
                                │  · MemoryManager (召回/压缩抽取)│
                                │  · ContextCompressor+FactExt  │
                                │  · PlanState (活任务清单)     │
                                │  · ErrorRecovery / Retry       │
                                │  · ProviderPool / SmartRouter  │
                                └───────────────────────────────┘
```

**关键 crate 分工：**

| crate | 角色 | 关键文件 |
|---|---|---|
| `oneai-core` | 基础类型与 trait：`ContentBlock`/`Conversation`、`TokenBudget`/`ContextBudgetManager`、`Team`/`Swarm`/`Handoff` 配置与日志 trait、`RecallStrategy` | `budget.rs`、`team.rs`、`swarm.rs`、`handoff.rs` |
| `oneai-agent` | 多 Agent 引擎本体：动态 Loop、范式、子 Agent、并行、编排原语 | `agent_loop.rs:4741`、`sub_agent.rs:870`、`parallel_executor.rs`、`team.rs:1080`、`swarm.rs:998`、`handoff.rs:871`、`group_chat.rs:874`、`meta_tool.rs` |
| `oneai-workflow` | StateGraph 引擎：可循环图、条件边、中断点、`GraphActionExecutor` 桥 | `state_graph.rs:512`、`state_executor.rs:1093`、`dag.rs`、`executor.rs` |
| `oneai-domain` | DomainPack 7 层声明式领域配置（含范式策略、StateGraph、MemoryProfile） | `domain_pack.rs:50`、`paradigm_strategy.rs`、`memory_profile.rs` |
| `oneai-memory` | 长程记忆：三层、压缩耦合抽取、三因子召回 | `manager.rs:655`、`compression.rs:492`、`fact_extraction.rs` |

---

## 2. 动态 Agentic Loop：核心执行模型

文件：`crates/oneai-agent/src/agent_loop.rs`

### 2.1 一轮迭代做什么

OneAI 的 `AgentLoop` 不是固定的 `Plan → Parallel → ReAct → Reflect` 管线，而是一个**模型驱动的动态循环**（灵感来自 Claude Code 的 Agentic Loop 架构，见模块头注释 `agent_loop.rs:1-15`）。每轮迭代：

1. **刷新与压缩决策**（`run_loop` @ `agent_loop.rs:1041-1077`）：刷新 DomainPack 的 `ContextSource`，组装"持久日志 + 临时上下文源 + 固定块（TaskAnchor/PlanProgress/skill 菜单）"。若请求会溢出 token 预算，则压缩**持久日志**（而非临时组装），再把固定块重新注入到压缩后的持久日志上。
2. **构建推理请求**（`agent_loop.rs:1092-1119`）：按当前活跃范式过滤工具定义（`build_tool_definitions_for_paradigm`），注入约束输出配置、思考预算、prompt-cache 策略。
3. **PreInfer 门**（`agent_loop.rs:1121-1181`）：先跑进程内 hooks（仅审计/日志），再走 `InteractionGate::PreInfer`——应用层可注入系统消息、替换请求、要求基于反馈重试、或跳过本轮。这取代了旧的交互式 `LifecycleHook` 路径。
4. **推理**（`agent_loop.rs:1222-1234`）：流式或非流式，非流式套 `tokio::select!` + `CancellationToken` 以便中断即时打断在途请求。
5. **PostInfer 门 + 解析决策**（`parse_decision` @ `agent_loop.rs:2367-2468`）：把模型输出解析成 `AgentDecision`。
6. **分发决策**（`agent_loop.rs:1434` 起）：按决策类型走不同分支。

### 2.2 决策四态：`AgentDecision` 枚举

`agent_loop.rs:143-162`：

```rust
pub enum AgentDecision {
    DirectAnswer { text: String },        // 模型给出最终答案 → 循环结束
    ToolCalls { calls: Vec<ToolCallRequest> },   // 调一个或多个工具 → 执行并回灌
    Delegate { tasks: Vec<DelegateTask> },      // 委托一批子任务给子 Agent
    SwitchParadigm { paradigm: ParadigmKind },  // 切换范式（进入固定图流）
}
```

- **DirectAnswer**：无工具调用、无委托时，把文本拼接为最终答案，`mark_complete()`，循环结束。
- **ToolCalls**：`execute_tool_calls`（`agent_loop.rs:2470`）并行执行所有调用（`futures::future::join_all`），每调用先经 `SmartToolRouter`（shell 重定向，见 `route_shell_to_specialized` @ `agent_loop.rs:2643`）、再经域权限 profile 解析、再经审批门，最后回灌 `tool_result`。
- **Delegate**：调用 `spawn_sub_agents_batch`（见 §4.2）。
- **SwitchParadigm**：调用 `apply_paradigm_switch_with_graph`（见 §3.3）。

### 2.3 终止由 TokenBudget 治理，而非 max_iterations

循环条件（`agent_loop.rs:968`）：

```rust
while !state.is_complete() && state.iterations < self.config.hard_max_iterations.unwrap_or(usize::MAX) {
```

主终止信号是 `state.is_complete()`（模型产出 `DirectAnswer` 或外部中断）。`hard_max_iterations` 只是**安全后背网**（防失控循环），默认 `usize::MAX`。真正的运行时约束是 `TokenBudget`——每轮 `ContextBudgetManager::needs_compression` 决定是否压缩，把长会话钉死在上下文窗口内（见 §6）。这与业界"用固定 `max_steps=50` 截断"的做法本质不同：OneAI 让"任务是否完成"由模型判断，让"上下文是否溢出"由预算治理，二者正交。

### 2.4 中断/恢复（人类在环）

- **外部中断**：`request_interrupt`（`agent_loop.rs:2272`）置位原子标志，下一轮迭代边界捕获，保存 checkpoint，返回 partial result 暂停（`agent_loop.rs:969-994`）。
- **恢复**：`resume_from_interrupt`（`agent_loop.rs:2307`）按人类反馈继续。`CancellationToken` 让推理在途也能即时打断。
- **速率限制**：连续 rate-limit 错误达 `MAX_CONSECUTIVE_RATE_LIMIT_ERRORS=10` 才终止，否则等 5s 重试（`agent_loop.rs:1242-1284`），避免被限流误杀长程任务。

---

## 3. 四范式与模型驱动切换

### 3.1 四范式 `ParadigmKind`（`agent_loop.rs:166-173`）

`Plan / ReAct / Reflect / Explore`——每个范式是一组 `(system_prompt, tool_filter, decision_hint)`（`ParadigmConfig` @ `agent_loop.rs:195-305`），灵感来自 Aider 的 Architect/Editor 双模型模式，OneAI 扩展为 4 个：

| 范式 | 工具集 | 职责 |
|---|---|---|
| Plan | read/grep/glob/list/env（**无执行工具**） | 仅分解任务为有序步骤 |
| ReAct | 全工具集（含 edit/shell/web_fetch） | 推理-行动-观察-迭代（默认执行态） |
| Reflect | 只读工具 | 审查现状，找错与改进点 |
| Explore | read/grep/glob/list/web_fetch | 广度搜索，不改任何东西 |

### 3.2 语义切换：`apply_paradigm_switch`（`agent_loop.rs:2980-3005`）

切换范式不只是返回一句"已切换"，而是**真实可观测的行为变化**（解决"范式切换语义空洞"gap）：
1. 删除旧系统消息，注入范式专属 system prompt；
2. 注入 `decision_hint` 告诉模型该范式下该做什么决策；
3. 把 `ParadigmConfig` 存入 `LoopState`，后续 `build_tool_definitions_for_paradigm` 据此过滤工具。

### 3.3 图流切换：`apply_paradigm_switch_with_graph`（`agent_loop.rs:3017-3140`）

若 DomainPack 为该范式预定义了 StateGraph（键 `react-loop` / `plan-workflow` / `reflect-workflow` / `explore-workflow`），则先做语义切换，再用 `StateGraphExecutor` 执行该图流，结果注入回主 Loop 对话。失败则回退到纯语义切换。这把"范式 = 固定图流"做实：ReAct 不是隐式 while 循环，而是一条显式可循环、可中断、可检视的图（见 §5）。

### 3.4 模型如何触发切换：`switch_paradigm` 元工具

`meta_tool.rs:88-108`：`switch_paradigm` 的 `ToolDefinition` 被注入推理请求，模型可在任意轮调用它切换范式。该调用在 `parse_decision` 的 `ContentBlock` 层被拦截成 `AgentDecision::SwitchParadigm`（`agent_loop.rs:2415-2426`），**永不进 ToolExecutor**——`is_meta_tool`（`meta_tool.rs:33`）作为防御性后背。

---

## 4. 子 Agent 委托与并行调度

### 4.1 SubAgent：分层分解，只回灌摘要

`sub_agent.rs:175-200`：`SubAgentWrapper` 把一个 `AgentLoop` 包装成 `SubAgent`——独立上下文窗口、scoped 工具集、专属 system prompt、token 预算。核心原则（`sub_agent.rs:8-10`）：**子 Agent 只回灌 `SubAgentSummary`（摘要 + key_findings + token 用量），不回灌完整对话**。这直接对应 Claude Code 的子代理模式（子代理的最终文本即返回值），保持主上下文窗口干净，使深分解不污染主上下文。

`SubAgentKind`（`sub_agent.rs:39-106`）：`Plan / Explore / Code / Review / Custom`，每类有默认 system prompt 与工具集。**Code 类用 git worktree 隔离**（`worktree_config`，见 §4.3），只读类不用。

### 4.2 一轮多委托 + Kahn 波次 DAG 调度

文件 `meta_tool.rs:46-87` 的 `delegate` schema 支持 `id` + `depends_on`，模型可在**同一轮**发多个 `delegate` 扇出（`agent_loop.rs:114-140` 的 `DelegateTask` 注释）。`parse_decision` 把同轮所有 `delegate` 收集成 `AgentDecision::Delegate { tasks }` 批次，校验 `depends_on` 引用未知 id 则丢弃（`agent_loop.rs:2436-2462`）。

`spawn_sub_agents_batch`（`agent_loop.rs:2852-2963`）实现**Kahn 拓扑波次调度**：

```
while 还有 pending:
    wave = 所有 depends_on 已完成的任务
    若 wave 为空 → 剩余任务成环 → 报错（cycle 检测）
    并行 spawn 这一整波（JoinSet）
    等整波结束 → 把各任务摘要写入 completed
    下一波里，depends_on 本轮的任务其 task 文本被自动前置上游摘要
```

- **无依赖任务并行**，**有依赖任务串行**且其 task 描述被自动前置上游 `summary` + `key_findings`（`agent_loop.rs:2897-2918`）——模型无需重述上游结果。
- **环检测**：若某波无法前进，剩余任务即成环，直接报错（`agent_loop.rs:2878-2885`）。
- **失败语义**：单个子 Agent 失败立即上抛而非静默丢弃；依赖它的后续任务会在下一波触发 cycle 守卫（`agent_loop.rs:2939-2946`）。
- 结果按输入顺序回灌（`agent_loop.rs:2957-2962`），保证确定性。

这是对"逐个串行委托"的关键升级：把 DAG 编排权交给模型，运行时按拓扑序自动并行化。

### 4.3 Worktree 隔离：并行写不冲突

`worktree_isolation.rs:1-29`：多个 Code 子 Agent 并行修改同一文件会冲突。`WorktreeIsolation` 用 `git worktree add -b <branch>` 给每个子 Agent 一个隔离副本（共享 `.git`，轻量；各在自己分支）。完成后 `merge_back` 合回主分支，冲突则保留 worktree 人工解决，无改动则即时清理。不可用时回退到目录级隔离。这对应 Claude Code 的 agent 隔离做法，解决 P1#13 并行写冲突。

### 4.4 ParallelExecutor + ScopeState（MVI/Redux 式状态隔离）

`parallel_executor.rs:1-11`：另一条并行路径（多用于 Plan 分解的 non-coupled 步骤）。每个子 Agent 克隆**只读全局记忆**到隔离 `ScopeState`，在私有 sandbox 内本地变更，产出 `Reduction`；全部完成后由 `StateReducer` 合并回 `GlobalState`（`parallel_executor.rs:77-144`）。这是 Redux/MVI 的单向数据流模式，与 §4.2 的"摘要回灌"互补：一个保上下文干净，一个保状态一致。

### 4.5 AsyncTaskRunner：后台非阻塞委托

`async_task_runner.rs:1-25`：主 Agent 可委托任务给后台 worker，自己继续干活，稍后查询结果。状态机 `Pending → Running → Completed/Failed/Cancelled`，预算感知，进度经 `AgentLoopObserver` 推 TUI。对应 Claude Code 的后台子代理。

---

## 5. StateGraph ↔ AgentLoop 闭环（P2-2 桥）

文件 `crates/oneai-workflow/src/state_graph.rs`、`state_executor.rs`。

### 5.1 可循环图

`StateGraph`（`state_graph.rs:1-21`）受 LangGraph 核心创新启发：**支持循环边**，使 ReAct 循环（Think→Act→Observe→Think）成为显式图循环而非隐式 while——状态可见、可检视、可中断。区别于 `WorkflowDag`（纯 DAG，并行步骤编排）。

`NodeAction`（`state_graph.rs:43-130`）6 种节点动作：`LlmInfer / ToolCall / Delegate / HumanApproval / ConditionCheck / SwitchParadigm`。`EdgeCondition` 9 种条件路由（含 `ParadigmEquals`、`IterationExceeds`）。

### 5.2 GraphActionExecutor 桥：图流复用 Loop 全基础设施

`state_executor.rs:79-99`：`GraphActionExecutor` trait 让 StateGraph 执行时复用 AgentLoop 全管线，而非另起一套直连 provider：
- **LlmInfer 节点**：拿到按范式过滤的工具定义、域装饰器、PreInfer/PostInfer hooks、上下文组装、OutputParser（`agent_loop.rs:3821-3902` 的 `AgentLoopGraphActionExecutor::execute_llm_infer`）。
- **ToolCall 节点**：走完整权限/审批管线（域 PermissionProfile → 审批门）（`agent_loop.rs:3904-3968`）。

这意味着：无论是主 Loop 的 `run_with_state_graph` 顶层路径，还是内联 `apply_paradigm_switch_with_graph` 触发的图流，**都共用同一个 `AgentLoopGraphActionExecutor`**（`agent_loop.rs:3769-3807`，桥结构注释明确指出两条路径共享以消除一致性漏洞）。

### 5.3 Checkpoint 与时间旅行

`StateGraphExecutor` 支持 `interrupt: true` 节点（HumanApproval）暂停，`max_iterations` 兜底防无限循环。Studio Web UI 的"Checkpoint 时间旅行"即建立在此可检视性之上。

---

## 6. 长程任务支撑

长程（long-horizon）任务的核心难题是：上下文会溢出、目标会被遗忘、子任务会丢失、错误会累积。OneAI 用以下机制系统性应对。

### 6.1 持久/临时分离 + 固定块抗压缩重注入

`context_assembler.rs:72-101` 的**临时重注入模型**：

- `state.conversation` 是**持久日志**（system prompt、user task、assistant 回复、tool 结果）——循环追加、持久化、可被压缩。
- ContextAssembler 每轮产出**全新的临时组装**（持久日志克隆 + 所有 ContextSource 缓存 + 固定块），推理请求用它，**永不写回持久日志**。
- 因此固定状态（env 感知、core memory、TaskAnchor、PlanProgress）靠**重注入**而非"指望压缩器保留"来扛压缩——压缩器只看见临时组装，被它摘要掉的东西下一轮自动恢复（`context_assembler.rs:77-90`）。

三个固定块（`context_assembler.rs:155-185`）：
- **`[Task Anchor]`**：原始任务 + 蒸馏意图，镜像到 `metadata["task_anchor"]`（每个压缩器逐字保留）。
- **`[Plan & Progress]`**：活任务清单的 ✅/🔄/⏳ 渲染，镜像到 `metadata["plan_state"]`。
- **运行时块**：今日日期 + 时间敏感问题应优先 `web_search`/`web_fetch` 的指引，附在 system prompt 末尾（`runtime_context_block` @ `context_assembler.rs:198-212`），因 system prompt 比临时系统消息更扛压缩。

修复历史 bug（`agent_loop.rs:1056-1058` 注释）：非压缩轮 `assembled` 曾被丢弃、请求用裸持久日志，导致 ContextSource 注入永远到不了模型——现每轮都组装真实请求大小再判溢。

### 6.2 压缩耦合事实抽取（"压缩即丢失"闭环）

`compression.rs:80-90` 的 `with_fact_extraction`：每次压缩时，被摘要掉的 `discarded_messages` 经 `FactExtractor`（按 schema）抽取成事实，Mem0 式冲突更新入 `archive`。被压缩丢弃的轮次不再"丢了就丢"，而是落档为可召回的长期事实。

### 6.3 三因子召回每轮注入

`manager.rs:9-13, 347-580`：`MemoryManager::recall_facts(query, top_k)` 用**相关度 + 近因 + 重要度**三因子（Generative-Agents 式）从 archival 召回，每轮经 `CoreMemorySource` 注入。配 `EmbeddingService` 走语义相关度，否则退化关键词。会话末 `reflect()` 把整段对话提炼成 episodic 事实。详见 `docs/memory-mechanism.md`。

> 注：召回当前存在已知缺陷——存储事实的 embedding 恒为 None，语义召回退化为关键词（见记忆 `memory-semantic-recall-inactive.md`）。机制完整但语义路径待修。

### 6.4 PlanState：活任务清单防遗忘

`plan_state.rs:1-9`：区别于一次性产计划的 `PlanAgent`，`PlanState` 是执行中模型经 `task_create/task_update/task_list` 控制工具持续变异的活清单，存于 `LoopState`（agent 侧），镜像到 `metadata["plan_state"]` 抗压缩 + 抗重载。每轮 `[Plan & Progress]` 块把 ✅/🔄/⏳ 重新注入，模型无需重读被压缩掉的轮次就知道进度。

### 6.5 错误恢复 + 重试 + 容错

- `error_recovery.rs`：`RecoveryManager` 按失败工具结果选恢复策略，`select_recovery_strategy`（`agent_loop.rs:3664`）。
- provider 级 429 重试（`ProviderRetryConfig` + `send_with_retry`），AgentLoop 级 `MAX_CONSECUTIVE_RATE_LIMIT_ERRORS` 兜底。
- `ProviderPool` 故障转移链 + `SmartRouter` 多因子路由 + 熔断器（`circuit_breaker` @ `agent_loop.rs:1018-1029`）——provider 失败时不杀长程任务而是降级/转移。

### 6.6 串成一句话的长程闭环

一轮长程迭代 = 刷新 ContextSource → 组装（持久日志 + 固定块 + 召回事实）→ 判溢出则压缩持久日志（被压缩的轮次抽取成事实落档）→ 注入 PlanProgress/TaskAnchor（模型不忘目标/进度）→ PreInfer 门 → 推理 → 解析决策 → 工具/委托/切换 → PostInfer → 回灌 → 下一轮。任一环失败有重试/降级/熔断兜底，目标与进度靠固定块与元数据双重抗压缩。这使 OneAI 能在固定上下文窗口内跑任意长任务。

---

## 7. 四种多 Agent 编排原语

除主 Loop 内的子 Agent 委托外，OneAI 提供四种显式编排原语，覆盖不同协作拓扑。

### 7.1 TeamCoordinator（聚合，4 策略）

`team.rs:1-15, 48-151`：输入 `TeamConfig`（策略 + 角色 + 预算），经 `SubAgentFactory` 创建成员，按策略执行：

| 策略 | 拓扑 | 用途 |
|---|---|---|
| **Coordinate** | 全员并行同任务 → 协调者合成共识 | 多专家交叉验证 |
| **Route** | 路由 agent 选最佳专家，只跑一个 | 任务分流到对口专家 |
| **Collaborate** | 串行，各建在上一个输出上 | 流水线式接力 |
| **Debate** | 多方辩论 → judge 仲裁 | 多视角对抗求优 |

共享团队预算，按角色数分配；记 token、记 `TeamCoordinationLog`。

### 7.2 SwarmOrchestrator（动态池，3 路由）

`swarm.rs:1-15, 45-143`：复杂任务分解为 `SwarmTask`，按能力路由到最佳 agent，并发执行（尊重依赖），结果按质量阈值校验，失败用替代 agent 重试，聚合成 `SwarmResult`。路由：`BestFit`（最高质量）/ `LoadBalanced`（考虑当前负载）/ `Fastest`（最高速度）。（注：代码注释把 CostOptimized 也列在枚举里，见 `oneai-core` 的 `SwarmRouting`，已删 USD 成本维度后保留 token/速度/质量导向。）

### 7.3 HandoffTool / HandoffManager（移交，模型驱动）

`handoff.rs:1-54`：**移交即工具调用**。`HandoffTool` 实现 `Tool` trait 注册进 ToolRegistry，模型像调普通工具一样调它（`target` + `reason`）。`HandoffManager` 检测到该调用后：解析目标与原因 → 经 `SubAgentFactory` 创建接收 agent → 转移对话上下文（或摘要，`transfer_conversation`）→ 接收 agent 续跑。关键设计是模型自然决定何时移交、移交给谁——工具描述告知可用目标。

### 7.4 GroupChatSession（共享转录对话，引擎原语）

`group_chat.rs:1-29`：区别于 Team 的**聚合**（扇出 N 个 → 合并一个结果），GroupChat 是**对话**：N 个 persona agent 在**一个共享 Conversation** 里轮流发言，人在环中。对应 AutoGen GroupChat / Coze 多 Agent 对话模式，下沉到引擎层使每个原生端口（macOS/Windows/Android/iOS）免费获得，无需在 UI 层重实现编排。

- 每成员是精简 `AgentLoop`（persona system prompt，共享 provider/tools/parser）。
- 一条共享 `Conversation` 持对话；每成员跑在**派生转录**（共享减去 system 消息）上，自己的 persona system prompt 由 loop 新鲜注入；只把该成员最终答案以 `metadata["speaker"]=<id>` 标记回灌。
- **轮次策略**（`TurnPolicy` @ `group_chat.rs:100-116`）：`Scripted`（固定序，如面试 `[coach, interviewer]`）/ `RoundRobin`（成员序）/ `Moderator`（主持成员选下一位发言者，可交回 `"user"`）。
- **ReviewLoopConfig**（`group_chat.rs:126-134`）：写作工坊式评审-修改循环——writer 起草 → editor 评审 → writer 修改 → … 直到 editor 吐 `approve_marker` 或达 `max_rounds` 上限。

### 7.5 四原语对比

| 原语 | 拓扑 | 控制权 | 主控上下文 | 典型场景 |
|---|---|---|---|---|
| SubAgent 委托 | 分层（父→子） | 模型驱动 meta-tool | 父保持干净（只收摘要） | 任务分解、隔离执行 |
| Team | 扇出/串行/辩论 | 预设策略 | 协调者合成 | 多专家验证/流水线/对抗 |
| Swarm | 动态池 + 路由 | 能力路由器 | 聚合结果 | 大规模复杂任务并发 |
| Handoff | 链式移交 | 模型驱动 tool | 转移给接收方 | 专家接力、换手 |
| GroupChat | 共享对话轮流 | 轮次策略/主持 | 共享转录 | 多角色对话、人在环 |

---

## 8. 编排行为的声明式配置：DomainPack

`domain_pack.rs:50-89`：DomainPack 是领域知识声明式配置的中央单元，7 层：

1. **Tools + ToolDecorators**：领域工具集 + 基础工具描述/权限覆盖
2. **ContextSources**：带 refresh policy 的环境感知（git status、文件树…）
3. **PermissionProfile**：领域权限分级
4. **ParadigmStrategies**：任务模式 → 范式序列/子 Agent 配置映射
5. **CompressionTemplate**：压缩保留优先级
6. **Workflows + StateGraphs**：领域预定义工作流与可循环图
7. **MemoryProfile**：记忆策略（RecallStrategy、core memory 预算、事实 schema）

`CodingPack` 是内置参考实现，`ResearchPack` 为研究域。多 DomainPack 可合并（`merge.rs`）：权限严格者优先，ContextSource 优先级合并。一行 `AppBuilder::domain_pack(...)` 切换整个编排行为。`#[non_exhaustive]` 守护公开枚举以兑现 v0.2.0 稳定性承诺。

**关键意义**：编排范式（何时切 Plan、何时委托、用哪种图流、压缩保什么、召回怎么算）不是硬编码在 agent_loop 里，而是声明在 DomainPack——同一个引擎，换 pack 即换"领域人格"。

---

## 9. 与业界前沿对标

> 以下对标基于训练知识（截至 2025 年初），目的在于定位 OneAI 的设计坐标，非逐版本逐特性精确比对。

### 9.1 总览对标表

| 维度 | OneAI | 业界前沿参照 | 评价 |
|---|---|---|---|
| **执行模型** | 动态 Agentic Loop，模型每轮决策 4 态 | Claude Code Agentic Loop、OpenAI "Building Effective Agents" | 同源思想，OneAI 把决策显式枚举化、可观测 |
| **循环结构** | 可循环 StateGraph（显式图循环） | LangGraph cyclic graphs | OneAI 受 LangGraph 启发（`state_graph.rs:7-9`），且与 Loop 双向闭环 |
| **委托/子 Agent** | meta-tool `delegate`，只回灌摘要 | Claude Code subagents、Devin 子任务 | 与 Claude Code 模式一致，强调主上下文干净 |
| **并行委托** | 一轮多委托 + Kahn 波次 DAG 调度 + 环检测 | LLMCompiler（并行函数调用）、LangGraph parallel branches | OneAI 把 DAG 编排权交模型，拓扑自动并行化 |
| **隔离** | git worktree + ScopeState(MVI/Redux) | Claude Code worktree isolation | 同样用 git worktree，额外加状态隔离 |
| **多 Agent 编排原语** | Team/Swarm/Handoff/GroupChat 四原语 | AutoGen GroupChat、CrewAI roles、Swarm handoff、MetaGPT SOP | OneAI 把四类拓扑收敛为引擎内原语，原生端口共享 |
| **移交机制** | Handoff as Tool（模型自然决定） | OpenAI Swarm `handoff` function | 几乎同一设计：移交即工具调用 |
| **范式切换** | 4 范式 + 内联升级 prompt/工具集 + 图流挂载 | Aider Architect/Editor、Reflexion、Plan-and-Solve | OneAI 扩展为 4 范式且与 StateGraph 联动 |
| **长程上下文** | 持久/临时分离 + 固定块重注入抗压缩 | LangGraph state channels、Letta memory blocks | OneAI 的"重注入而非靠压缩器"思路独特 |
| **记忆** | Letta 三层 + Mem0 冲突更新 + 三因子召回 + 压缩耦合抽取 | Letta、Mem0、Generative Agents、Zep-Graphiti | 融合多家，"压缩即丢失"闭环是亮点（详见记忆白皮书） |
| **协议互操作** | A2A SDK（P2-5）+ MCP server 生态（P3-6） | Google A2A Protocol、Anthropic MCP | OneAI 既实现 A2A 客户端/服务端又接 MCP |
| **人类在环** | InteractionGate 5 决策点 + 中断/恢复 + Checkpoint 时间旅行 | LangGraph interrupt、AutoGen human-in-loop | OneAI 用统一 5 决策点 gate 收敛，Studio 提供时间旅行 |
| **声明式领域** | DomainPack 7 层可合并 | CrewAI 的 role/goal、AgentScope 的配置 | OneAI 更彻底：编排、记忆、压缩、图流全声明式 |

### 9.2 对标 AutoGen / LangGraph

**vs LangGraph**：LangGraph 的核心创新是"可循环有状态图 + 通道式状态"。OneAI 的 `StateGraph` 直接吸收此思想（`state_graph.rs:7-9` 明确标注受 LangGraph 启发），并更进一步——`GraphActionExecutor` 桥让图流执行复用 AgentLoop 全管线（hooks/权限/工具组装/parser），图与 loop 不是两套系统而是一体两面：主 loop 可 `switch_paradigm` 进入图流，图流的 `Delegate` 节点又经 `DelegateFactory` 回到子 Agent 工厂。LangGraph 的 channel 状态对应 OneAI 的 `GraphState` + `metadata` 抗压缩持久。

**vs AutoGen**：AutoGen v0.4 的 actor-based 多 Agent 对话与 GroupChat 是其标志。OneAI 的 `GroupChatSession`（`group_chat.rs:1-29`）明确对标 AutoGen GroupChat / Coze 多 Agent 对话，但下沉为引擎原语——共享转录 + speaker 标记 + 三轮次策略 + 评审循环，使原生端口（而非 Python 脚本）能直接驱动多角色对话。OneAI 比 AutoGen 多了"模型驱动的并行委托 DAG"这一层（AutoGen 偏顺序对话）。

### 9.3 对标 Claude Code / Devin

OneAI 在注释中多次明示对标 Claude Code：动态 Agentic Loop（`agent_loop.rs:1-15`）、子 Agent 只回灌摘要（`sub_agent.rs:8-10, 116-118`）、worktree 隔离（`worktree_isolation.rs:24`）、后台 AsyncTaskRunner（`async_task_runner.rs:1-25`）。差异：OneAI 用 Rust 实现且把编排行为声明式化进 DomainPack，使同一引擎可跨域（编码/研究/IoT）与跨端（桌面/移动）部署。对标 Devin 的子任务分解，OneAI 的"一轮多委托 + 依赖感知并行"是对串行分解的并行化升级。

### 9.4 对标 MetaGPT / SWE-agent / CrewAI

- **MetaGPT**：用 SOP（标准作业流）编码多 Agent 协作。OneAI 的 DomainPack 第 6 层 Workflows+StateGraph 是等价物——把领域 SOP 声明为可循环图。CodingPack 即"编码域的 SOP"。
- **SWE-agent**：其 Agent-Computer Interface（约束原始 shell 到专用命令）对应 OneAI 的 `SmartToolRouter`（`route_shell_to_specialized` @ `agent_loop.rs:2643-2642`），把 `shell cat` 重定向到 `read_file`、`ls` 重定向到 `list_directory`——即使模型（GLM/Qwen）忽略工具偏好规则，运行时仍正确路由。OneAI 还有 SWE-bench 三轴（能力×成本×效率）评测框架（见记忆 `swe-bench-eval-three-axis.md`）。
- **CrewAI**：role-based 多 Agent。OneAI 的 Team `AgentRole` + 4 策略覆盖其角色编排，但 OneAI 多了 GroupChat（对话式）和 Swarm（动态池）两种 CrewAI 没有的拓扑。

### 9.5 协议层：A2A 与 MCP

OneAI 既实现 **Google A2A 协议**（`oneai-a2a`：P2-5 客户端 SDK + P4-1 服务端 host，`A2AClient`/`A2AServerHost`/`A2ARouter`/`TaskStore`，DomainPack→AgentCard 自动生成）又接 **Anthropic MCP**（`oneai-mcp`：McpServerHost + McpPluginRegistry + AppBuilder 集成 + CLI）。这让 OneAI Agent 既能作为 A2A 服务被其他 agent 调用、又能消费 MCP 工具/数据源——跨框架互操作不依赖适配层。

### 9.6 OneAI 的相对差异化

综合对标，OneAI 在以下点较业界前沿有独立设计：

1. **决策显式枚举化**：把模型每轮输出收敛为 4 态 `AgentDecision` 并可观测（observer 14 回调 + OTEL trace span），而非黑盒 while 循环。
2. **委托 DAG 内联化**：模型一轮即可表达多委托 + 依赖，运行时 Kahn 波次自动并行——把"并行编排"从框架代码下沉到模型能力。
3. **图流与 Loop 双向闭环**：`GraphActionExecutor` 桥让 StateGraph 不是旁路系统，范式切换即进入图流、图流节点又回调 loop 基础设施。
4. **固定块抗压缩重注入**：持久/临时分离 + TaskAnchor/PlanProgress 靠重注入扛压缩，而非依赖压缩器保留——长程目标不丢。
5. **编排声明式化**：编排范式、记忆、压缩、图流全声明在 DomainPack 7 层，一行切域、可合并多域。
6. **四原语收敛**：Team/Swarm/Handoff/GroupChat 在同一引擎内，原生跨端共享，非 Python 脚本胶水。

---

## 10. 已知局限与待办

- **语义召回失效**：存储事实 embedding 恒为 None，三因子召回退化关键词（`memory-semantic-recall-inactive.md`，机制文档 `docs/memory-mechanism.md`）。
- **图流桥未完全接线**：`AgentLoopGraphActionExecutor` 的 `parser`/`hook_registry`/`recovery_manager` 字段已克隆但尚未在 `GraphActionExecutor` impl 内读取（`agent_loop.rs:3790-3796` 注释标注为 follow-up），即图流路径的 OutputParser 决策解析、PreInfer/PostInfer 触发、工具错误恢复尚未与主 loop 完全一致。
- **Swarm 分解为启发式**：`swarm.rs:157-160` 注释明示任务分解是启发式，生产可换 LLM 驱动分解。
- **仅 4 范式**：`ParadigmKind` 为 `#[non_exhaustive]`，可扩展但当前内置仅 Plan/ReAct/Reflect/Explore。

---

## 附：关键文件索引

| 机制 | 文件:行 |
|---|---|
| 动态 Loop 主循环 | `crates/oneai-agent/src/agent_loop.rs:945` (`run_loop`) |
| 决策解析 | `agent_loop.rs:2367` (`parse_decision`) |
| 范式配置/默认 | `agent_loop.rs:195-305` (`ParadigmConfig`) |
| 语义范式切换 | `agent_loop.rs:2980` (`apply_paradigm_switch`) |
| 图流范式切换 | `agent_loop.rs:3017` (`apply_paradigm_switch_with_graph`) |
| 并行委托调度 | `agent_loop.rs:2852` (`spawn_sub_agents_batch`) |
| 工具执行+域权限 | `agent_loop.rs:2470` (`execute_tool_calls`) |
| SmartToolRouter | `agent_loop.rs:2643` (`route_shell_to_specialized`) |
| 图流桥 | `agent_loop.rs:3769-3810` (`AgentLoopGraphActionExecutor`) |
| 元工具定义 | `crates/oneai-agent/src/meta_tool.rs:45` |
| 子 Agent 包装 | `crates/oneai-agent/src/sub_agent.rs:175` |
| Worktree 隔离 | `crates/oneai-agent/src/worktree_isolation.rs:1` |
| 并行执行器 | `crates/oneai-agent/src/parallel_executor.rs:77` |
| 后台任务 | `crates/oneai-agent/src/async_task_runner.rs` |
| Team 协调 | `crates/oneai-agent/src/team.rs:48` |
| Swarm 编排 | `crates/oneai-agent/src/swarm.rs:45` |
| Handoff | `crates/oneai-agent/src/handoff.rs:55` |
| GroupChat | `crates/oneai-agent/src/group_chat.rs:1` |
| StateGraph | `crates/oneai-workflow/src/state_graph.rs:1` |
| StateGraph 执行器 | `crates/oneai-workflow/src/state_executor.rs:1` |
| 上下文组装 | `crates/oneai-agent/src/context_assembler.rs:46` |
| 记忆管理器 | `crates/oneai-memory/src/manager.rs:58` |
| 压缩耦合抽取 | `crates/oneai-memory/src/compression.rs:80` |
| PlanState | `crates/oneai-agent/src/plan_state.rs:17` |
| DomainPack | `crates/oneai-domain/src/domain_pack.rs:50` |

*本文随 `0.2.0`/1.0.0 线代码同步。机制变更请同步更新文件:行索引。*
