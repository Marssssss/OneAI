# 模型驱动的 delegate / switch_paradigm 端到端打通方案

## Context（为什么做这件事）

当前 OneAI 的 `AgentLoop` 设计意图是**由模型决策**是否走子代理（`delegate`）、是否切换范式进入固定图流程（`switch_paradigm`）。但代码核查发现这套机制在非 mock 环境下基本是死路，且存在两条路径的集成度冲突：

1. **模型拿不到 meta-tool 定义**：`parse_decision`（`agent_loop.rs:2106-2152`）和 StateGraph 的 `parse_graph_decision`（`agent_loop.rs:3454`）/ `DirectProviderActionExecutor::parse_decision`（`state_executor.rs:303`）都靠**拦截名为 `delegate`/`switch_paradigm` 的 ToolCall** 来识别意图，但全仓库没有任何 `ToolDefinition { name: "delegate"/"switch_paradigm" }` 被注入给真实模型。`build_tool_definitions_for_paradigm`（`agent_loop.rs:2890-3006`）只发真实工具 + `plan_state::control_tool_definitions()`。结果：真实模型既不知道这两个工具存在，system prompt 也只有一句模糊的"can delegate... switch to a planning paradigm"（`agent_loop.rs:546`）。两条路径只在 `mock_provider` 测试里被触发。

2. **三个范式图缺失**：`apply_paradigm_switch_with_graph`（`agent_loop.rs:2582-2591`）按 paradigm 查 `react-loop`/`plan-workflow`/`reflect-workflow`/`explore-workflow`。CodingPack 只定义了 `react-loop`（`coding_pack.rs:748` `react_state_graph()`），其余三个未定义 → 切到 Plan/Reflect/Explore 时 `get_state_graph` 返回 None，退化成纯语义切换，"进入固定图流程"对三者不成立。

3. **内联触发用弱桥**：主 loop 的 `SwitchParadigm` 分支（`agent_loop.rs:1810-1828`）调用 `apply_paradigm_switch_with_graph`，其中用 `StateGraphExecutor::with_direct_provider_defaults`（`agent_loop.rs:2610`）即 `DirectProviderActionExecutor`——**绕过 hooks/domain/parser**。而顶层 `run_with_state_graph`（`agent_loop.rs:1934-1944`）用全桥 `AgentLoopGraphActionExecutor`。同一个范式切换，两条路径集成度不一致。

**目标**：延续"模型决策"方案不变，把上述未实现/冲突部分打通——让真实模型能调用 `delegate`/`switch_paradigm`，让四个范式切换都能进入各自的固定图流程，并消除内联/顶层两条路径的集成度差异。

**用户决策**（已确认）：①补齐三个缺失 StateGraph；②内联 executor 升级为全桥 `AgentLoopGraphActionExecutor`。

## 复用先例：`plan_state` 的 meta-tool 模式

`plan_state.rs` 已有完整的"被拦截、不进 registry、但定义注入给模型"的 meta-tool 先例，本方案直接照抄：

- 常量 + 谓词：`TOOL_TASK_CREATE` 等 + `is_control_tool()`（`plan_state.rs:79-93`）
- 定义工厂：`control_tool_definitions()` 返回 4 个 `ToolDefinition`（`plan_state.rs:98-171`）
- 注入点：`build_tool_definitions_for_paradigm` 末尾 `control_defs`（`agent_loop.rs:2990-3005`），plan_mode 下只暴露 `exit_plan_mode`
- 拦截执行：主 loop `filtered_calls` 分流，控制工具走 `apply_control_tool` 不经 ToolExecutor（`agent_loop.rs:1591-1595`）

`delegate`/`switch_paradigm` 的拦截其实已存在（在 `parse_decision` 的 ContentBlock 层，早于 `filtered_calls` 分流），所以**只需补"定义注入"，拦截路由无需新建**。

## 实施步骤

### 步骤 0：交付物落盘（用户要求）
本 Plan 经批准后，作为执行第一步，将本设计文档写入 `D:\rust\OneAI\Paradigm-Delegate-MetaTool-Plan.md`（用户要求输出到当前目录 md）。

### 步骤 1：新增 meta-tool 模块
新建 `crates/oneai-agent/src/meta_tool.rs`，镜像 `plan_state.rs` 结构：

- `pub const TOOL_DELEGATE: &str = "delegate";`
- `pub const TOOL_SWITCH_PARADIGM: &str = "switch_paradigm";`
- `pub fn is_meta_tool(name: &str) -> bool`（同 `is_control_tool` 风格）
- `pub fn meta_tool_definitions() -> Vec<oneai_core::ToolDefinition>`，两个定义：
  - **delegate**：`parameters_schema` = `{task: string (required), agent_type: string enum ["Plan","Explore","Code","Review"] (required), budget_tokens: integer default 5000}`。`description` 写清调用时机：子任务边界清晰、需要独立 context 窗口、主 loop 不必沾染中间步骤时。
  - **switch_paradigm**：`parameters_schema` = `{paradigm: string enum ["plan","react","reflect","explore"] (required)}`。`description` 写清：当 ReAct 不适合当前子任务（需结构化规划/深度反思/广度探索）时调用。
- `agent_type` 枚举与 `SubAgentKind::from_str`（`paradigm_strategy.rs:48`，变体 `Plan/Explore/Code/Review/Custom`，见 `team.rs:694`）保持一致；`paradigm` 枚举与 `parse_decision` 的 match 分支（`agent_loop.rs:2130-2135`）保持一致。

### 步骤 2：注入 meta-tool 定义
- `build_tool_definitions_for_paradigm`（`agent_loop.rs:2990-3005`）：在 `control_defs` 之后 `append` `meta_tool_definitions()`。**plan_mode 下不注入**（plan_mode 只暴露 `exit_plan_mode`，避免规划阶段 delegate/切换）。
- `build_tool_definitions_for_state`（`agent_loop.rs:3518`，StateGraph 路径）：同样注入，使图内 `LlmInfer` 节点也能 delegate/switch。
- `tier_order`（`agent_loop.rs:2924-2943`）：meta-tool 落在默认 tier 5 即可（specialized 之后、shell 之前），不单独排序。

### 步骤 3：拦截保护（防御性，低优先）
`parse_decision` 已在 ContentBlock 层把 `delegate`/`switch_paradigm` 转成 `AgentDecision`，不会进入 `filtered_calls`。为防回归，在主 loop 控制工具分流块（`agent_loop.rs:1591-1595`）的 `if !is_control_tool` 分支前加 `is_meta_tool` 兜底断言/跳过，确保即便未来路径变化也不会把它们误派给 `ToolExecutor`。`parse_graph_decision` 与 `DirectProviderActionExecutor::parse_decision` 已识别，无需改。

### 步骤 4：system prompt 增补可执行协议
`AgentLoopConfig::default().system_prompt`（`agent_loop.rs:543-556`）中那句模糊的"can delegate... switch to a planning paradigm"替换为：
- 列出 `delegate` 工具、`agent_type` 取值、调用时机
- 列出 `switch_paradigm` 工具、`paradigm` 取值、调用时机
- 明确：调用 `delegate` 后本轮交由子代理，主 loop 等其 summary；调用 `switch_paradigm` 后进入对应固定图流程
- 首版用静态枚举文本 + 注释指向 `SubAgentTypeDefinition::defaults()`；后续可改为构建时从 `domain_pack.sub_agent_definitions` 动态拼接（不在本轮）。

### 步骤 5：内联 executor 升级为全桥
`apply_paradigm_switch_with_graph`（`agent_loop.rs:2573-2623`）：
- 删除 `StateGraphExecutor::with_direct_provider_defaults(...)`（2610-2615）
- 改为构造 `AgentLoopGraphActionExecutor { provider, tools, parser, approval_gate, domain_pack, hook_registry, recovery_manager, config }`（clone `self` 各 Arc，完全复用 `run_with_state_graph:1934-1944` 的构造方式）
- 用 `StateGraphExecutor::new(action_executor, delegate_factory, approval_gate, max_iterations)`，`max_iterations = self.config.hard_max_iterations.unwrap_or(50)`
- 其余逻辑（查 graph、建 initial_state、执行、结果 feed 回 LoopState `2625-2627`）保持不变
- 效果：内联触发的图与顶层图模式走同一套 hooks/域权限/OutputParser，消除一致性空洞

### 步骤 6：补齐三个 StateGraph
`crates/oneai-domain/src/coding_pack.rs`，仿 `react_state_graph()`（748-）新增三个函数，并注册到 `state_graphs: vec![...]`（429-431）：

- `plan_workflow_state_graph()` → name `"plan-workflow"`：`plan`（LlmInfer，`include_tool_definitions=true` 但 tool_filter 只留 `exit_plan_mode`/`task_create`，呼应 plan_mode）→ 路由 `IsFinalAnswer`→`end` / `RequestsDelegation`→`delegate` 节点 → 回 `plan`。终结点 `end`。
- `reflect_workflow_state_graph()` → name `"reflect-workflow"`：`reflect`（LlmInfer，对 `last_result` 推理）→ `decide`（ConditionCheck: `is_final_answer`/`has_tool_calls`）→ `act`/`end`。
- `explore_workflow_state_graph()` → name `"explore-workflow"`：`explore`（LlmInfer，`RequestsDelegation`→多个 `Delegate` 节点）→ `synthesize`（LlmInfer 汇总）→ `end`。
- 全部复用现有 9 种 `EdgeCondition`（`HasToolCalls`/`IsFinalAnswer`/`RequestsDelegation`/`Always` 等）与 6 种 `NodeAction`；`max_iterations` 兜底防死循环。

### 步骤 7（可选，低优先）：去重
`parse_graph_decision`（`agent_loop.rs:3454`）与 `DirectProviderActionExecutor::parse_decision`（`state_executor.rs:303-362`）拦截逻辑重复。可抽共用 helper 到 `meta_tool.rs`，但本轮不硬性要求，仅记录为技术债。

## 关键文件

| 文件 | 改动 |
|---|---|
| `crates/oneai-agent/src/meta_tool.rs` | 新建：常量 + `is_meta_tool` + `meta_tool_definitions` |
| `crates/oneai-agent/src/lib.rs` | `pub mod meta_tool;` |
| `crates/oneai-agent/src/agent_loop.rs` | 步骤 2（注入）、3（兜底）、4（prompt）、5（内联全桥）|
| `crates/oneai-domain/src/coding_pack.rs` | 步骤 6（三个新图 + 注册）|
| `D:\rust\OneAI\Paradigm-Delegate-MetaTool-Plan.md` | 步骤 0（本设计文档落盘）|

## 验证

1. **单测**：`cargo test -p oneai-agent` —— 新增 `meta_tool::meta_tool_definitions()` 的 schema 断言；新增"`build_tool_definitions_for_paradigm` 在非 plan_mode 下包含 delegate/switch_paradigm、plan_mode 下不含"的断言。`mock_provider.rs:122,143` 已有 delegate/switch_paradigm 构造器，复用。
2. **e2e**：`cargo test -p oneai-agent --test e2e_tests` —— `e2e_tests.rs:974` 已有 `run_with_state_graph("react-loop")` 用例，扩展到三个新图名。
3. **domain**：`cargo test -p oneai-domain` —— CodingPack 三个新图的构建 + `get_state_graph("plan-workflow")` 等命中。
4. **lint**：`cargo clippy --workspace --all-targets`（保持 clean，符合仓库约定）。
5. **实跑**：`cargo run -p oneai-cli-demo`，用真实 provider 在复杂任务里观察模型自行 `delegate`/`switch_paradigm`，trace 里应出现 `agent.delegate`（`agent_loop.rs:1794`）/`agent.paradigm_switch`（1815）事件，且内联触发的图经过 hooks（PreToolUse/PostToolUse 日志可见）。
6. **回归**：确认 plan_mode 下不暴露 meta-tool；确认 `delegate`/`switch_paradigm` 不被 `ToolExecutor` 分发（无 "tool not found" 错误）；确认内联全桥不丢 conversation（结果正确 feed 回 LoopState）。

## 风险

- **内联全桥重入**：`AgentLoopGraphActionExecutor` 只持 clone 的 Arc（只读基础设施数据），不回写主 `LoopState`；graph 完成后由 `apply_paradigm_switch_with_graph` 现有逻辑（2625-2627）把 `last_result` feed 回。需验证 `conversation` 同步不丢消息。
- **新图死循环**：三个新图的 `EdgeCondition` 设计须保证有终结路径，`max_iterations` 已兜底。
- **token 增量**：system prompt 增补 + 两个 meta-tool 定义量小，可忽略；但注意 tool list 位置偏差（`tier_order` 注释提到的 GLM/Qwen 15-30% drop）——meta-tool 落 tier 5，不挤占 specialized 工具靠前位置。
