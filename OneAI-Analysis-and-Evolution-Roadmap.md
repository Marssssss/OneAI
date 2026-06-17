# OneAI 全面分析与演进路线图

> 生成日期: 2026-06-17 | 基于项目代码深度分析 + 10大行业Agent框架对比

---

## 一、OneAI 现状深度分析

### 1.1 项目概况

| 维度 | 数据 |
|------|------|
| 语言 | Rust (workspace 架构) |
| Crate 数量 | 19 |
| 源码行数 | ~39,400 行 (107 个 `.rs` 源文件) |
| 测试数量 | 257 passed, 15 ignored, 0 failed |
| 最大文件 | `tool_interfaces.rs` (2,651 行) |
| License | Apache-2.0 |

### 1.2 架构成熟度评估

```
              Core Types ████████████ 90%
              Provider   ████████░░░░ 70%
              Parser     █████████░░░ 85%
              Tool Sys   ████████░░░░ 75%
              AgentLoop  ███████░░░░░ 65%
              Workflow   ██████░░░░░░ 60%
              Memory     █████████░░░ 85%
              DomainPack ████████████ 95%
              RAG        ███████░░░░░ 65%
              Persistence ███████░░░░░ 65%
              Skill      ████████░░░░ 75%
              Trace      █████████░░░ 85%
              CLI/TUI    ███░░░░░░░░░ 25%
              Cross-Plat ████░░░░░░░░ 40%
```

### 1.3 核心亮点 (OneAI 独有优势)

**1. DomainPack 体系 — 行业最领先的领域配置抽象**

- 6 层声明式领域配置 (Tools+Decorators / ContextSources / PermissionProfile / ParadigmStrategies / CompressionTemplate / Workflows+StateGraphs)
- 一行代码切换领域: `AppBuilder::new().domain_pack(coding_pack("/path"))`
- 支持多领域合并 (strictest-wins 权限合并 + priority-based context 合并)
- ToolDecorator 概念极为优雅 — 不改 Tool 实现，只覆盖其面向 LLM 的描述
- **这是 OneAI 最有价值的架构创新，目前无任何竞品框架有同等抽象**

**2. 模型路由 (ModelRouter) — 内置成本路由**

- 基于 Regex 的任务→模型映射规则
- quick → Haiku, implementation → Sonnet, architecture → Opus
- **这是 Rust Agent 框架中唯一内置成本路由的实现**

**3. 3 层 Parser Defense — 生产级输出解析**

- Constrained Decoding → Fuzzy JSON Repair → Fallback Self-Correction
- 远超任何竞品框架的解析防御深度

**4. 跨平台 UniFFI — 真正的 multi-platform**

- Kotlin/Swift/C++/C# 绑定 + 4 个平台 crate
- 每个 Platform 有原生 ApprovalGate (NSAlert/AlertDialog/UIAlertController)
- **唯一真正跨移动平台的 Rust Agent 框架**

**5. 范式体系 — 4 范式动态切换**

- Plan/ReAct/Reflect/Explore + ParadigmConfig (system prompt + tool filter + decision hint)
- 比任何单范式框架的灵活度更高

### 1.4 关键短板 (需要重点解决)

**[P0 — 致命] AgentLoop 与真实 LLM 的连接未打通**

- `spawn_sub_agent()` → 返回硬编码 `SubAgentSummary` (不实际运行子代理)
- Delegate 和 SwitchParadigm 决策路径本质是死代码
- 整个 Agentic Loop 在真实 LLM 环境下从未完整运行过
- 所有 257 个测试都是纯结构/逻辑测试，**零 E2E 测试**

**[P0 — 致命] TUI/CLI 极其初级**

- TUI 只是工具演示界面，没有与 AgentLoop 的真实对接
- 无多轮对话、无流式推理展示、无范式切换 UI、无 ApprovalGate 交互 UI
- 对比 Claude Code 的完整交互式 REPL，差距巨大

**[P1 — 严重] 工具系统短板**

- GrepTool/GlobTool shell out 到 OS 命令 (安全隐患 + 平台不一致)
- NotebookEditTool 是 placeholder
- MCP 实现了 framing/stdio/SSE 协议但 `connect_and_discover()` 是 `todo!()`
- WebFetchTool 存在但未连接真实 HTTP

**[P1 — 严重] 无 StructuredOutput/schema 验证**

- SubAgentSummary 是自由文本，无 JSON Schema 强制验证
- 对比 OpenAI SDK 的 guardrails + PydanticAI 的 ModelRetry，缺少输出质量保障机制

**[P2 — 中等] Provider 适配不够深**

- Anthropic 仅实现 Messages API，无 Responses API
- Gemini provider 存在但实现较浅
- Provider tool calling 格式差异 (Anthropic tool_use vs OpenAI function_call) 的映射需验证

**[P2 — 中等] Workflow/StateGraph 与 AgentLoop 未闭环**

- StateGraph 定义完整但 `state_executor.rs` 的 LlmInfer/ToolCall node action 未实际调用 provider
- WorkflowDag 和 StateGraph 是独立的数据结构，没有与 AgentLoop 形成闭环调用链

---

## 二、行业框架对比与架构洞察

### 2.1 竞品架构矩阵

| 维度 | OneAI | LangGraph | Claude Code | OpenAI SDK | Agno | PydanticAI | Smolagents |
|------|-------|-----------|-------------|------------|------|------------|------------|
| **编排模型** | 动态Loop+4范式 | 有向图/状态机 | 单Agent工具循环 | Handoff轻量 | Team模式 | 单Agent类型安全 | Code-as-Action |
| **语言** | Rust | Python | TypeScript/Python | Python | Python | Python | Python |
| **领域抽象** | DomainPack 6层 ✨ | 无 | CLAUDE.md隐式 | 无 | 无 | 无 | 无 |
| **状态管理** | Conversation+LoopState | Typed schema+checkpointers | Session+compaction | Conversation context | Session-based | RunContext+minimal | Observation accumulation |
| **HITL** | ApprovalGate (4种) | interrupt()+Command(resume) | Permission system+hooks | Guardrails (非交互) | Application-level | ModelRetry (自动) | AST sandbox |
| **Memory** | STM+LTM+HNSW+compression | Checkpoint persistence | CLAUDE.md+memory files | None built-in | Agent/User/Team memory | In-context per run | Observation accumulation |
| **流式** | IncrementalStreamParser | 4 modes | Terminal real-time | run_streamed() | AgentRunStream | run_stream() | Step-by-step |
| **多Agent** | SubAgent (4+Custom) | Subgraphs+supervisor | Subagent spawning | Handoffs | Teams+nested | No | ManagedAgent |
| **安全** | 3级Permission+ApprovalGate | 无 | Permission+hooks lifecycle | max_turns | None | None | AST sandbox |
| **可观测** | OpenInference trace | LangSmith | Hooks | Built-in trace/span | Pre/post hooks | None | None |

### 2.2 各框架核心创新速览

| 框架 | 核心创新 |
|------|----------|
| **LangGraph** | 有向图+状态机+checkpoint 时间旅行 (任意节点回放/分叉) |
| **CrewAI** | role/backstory 角色化 Agent + hierarchical manager 动态委派 |
| **AutoGen** | 对话即架构 + 事件驱动运行时 + circuit breaker 集群弹性 |
| **OpenAI SDK** | Handoff-as-tool-call + parallel guardrails + 内置 tracing |
| **Claude Code** | Permission+Hooks lifecycle 安全 + allow/deny/modify 三态 + 围栏→生命周期范式 |
| **Google ADK** | A2A 协议 (Agent Card + Task lifecycle) — 唯一跨厂商 Agent 通信标准 |
| **Agno** | 9+向量库RAG + Team模式 (coordinate/route/collaborate) + 丰富内置工具 |
| **PydanticAI** | 类型安全即基础设施 + ModelRetry 自我纠错 + 依赖注入 RunContext[T] |
| **Smolagents** | Code-as-Action (Python代码作为动作) + AST sandbox 内置安全 |

### 2.3 架构洞察 — 8 条关键发现

**洞察 1: 编排谱 — 从显式结构到涌现行为**

- LangGraph/Semantic Kernel = 显式结构 (已知工作流)
- AutoGen/Smolagents = 涌现行为 (动态适应)
- OneAI/OpenAI SDK = 中间地带 (定义模式但灵活执行)
- **OneAI 的 DomainPack+ParadigmStrategies 正是中间地带的最佳表达**

**洞察 2: HITL 三类 — 原生 vs 门控 vs 自动**

- 原生 HITL: AutoGen (对话式)、LangGraph (interrupt/resume)、Claude Code (permission+hooks)
- 门控 HITL: OneAI (ApprovalGate)、CrewAI (human_input flag)
- 自动 HITL: PydanticAI (ModelRetry)、Smolagents (AST sandbox)
- **OneAI 需要补 interrupt/resume 能力 (LangGraph 级) + hooks 系统 (Claude Code 级)**

**洞察 3: Memory 问题 — 无框架完全解决**

- LangGraph checkpoint 最强状态持久化但无语义检索
- Agno knowledge/RAG 最强语义检索但弱工作流状态
- OneAI STM+LTM+HNSW 是最完整的双层设计，但两者未闭环
- **机会: OneAI 可以成为首个真正整合 checkpoint+语义检索 的框架**

**洞察 4: MCP 成为通用工具连接器**

- LangGraph、Agno、Claude Code、Semantic Kernel 都已支持 MCP
- **OneAI 已有 MCP 协议实现 (mcp_real.rs)，但 connect_and_discover() 未完成 — 优先级应极高**

**洞察 5: A2A 开启 Agent 网络**

- Google 的 Agent Card + Task lifecycle 是唯一跨厂商 Agent 通信标准
- MCP (纵向) + A2A (横向) = 完整 Agent 生态
- **OneAI 应率先实现 A2A — Rust 实现的 A2A SDK 将是行业稀缺资产**

**洞察 6: 安全从围栏转向生命周期**

- Claude Code hooks (PreToolUse/PostToolUse) 是 lifecycle-based 安全的标杆
- `allow/deny/modify` 三态输出使自动化 CI/CD Agent 成为可能
- **OneAI 的 ApprovalGate 是围栏式安全 (执行前审批)，需补充 lifecycle hooks**

**洞察 7: Code-as-Action vs JSON-as-Action**

- Smolagents 的 code-as-action 范式挑战 JSON 工具调用正统
- 代码动作有组合性 (循环、变量、嵌套) 但需要更复杂沙箱
- **OneAI 可探索 hybrid 模式: 简单工具调用用 JSON，复杂多步动作用 Rust/WASM 代码沙箱**

**洞察 8: 类型安全从锦上添花变为基础设施**

- PydanticAI 的 ModelRetry 是优雅的自我纠错机制
- 所有主流 LLM API 都已支持原生 structured output
- **OneAI (Rust) 有天然类型安全优势 — 应成为 "Rust 版 PydanticAI"**

---

## 三、OneAI 演进路线图

### Phase 0: 基础打通 (当前 → 2 个月)

> **目标: 让 AgentLoop 在真实 LLM 下完整跑通一圈**

```
Phase 0 优先级矩阵
┌───────────────────────────────────────────────────────────┐
│  P0-1  AgentLoop ↔ Provider 真实闭环        [致命]      │
│  P0-2  TUI ↔ AgentLoop 真实交互闭环          [致命]      │
│  P0-3  MCP connect_and_discover() 实现       [严重]      │
│  P0-4  SubAgentFactory 真实实现               [严重]      │
│  P0-5  E2E 测试框架搭建                       [严重]      │
└───────────────────────────────────────────────────────────┘
```

**P0-1: AgentLoop ↔ Provider 真实闭环**

- 修复 `parse_decision()` 对不同 Provider tool call 格式的处理
- 验证 Anthropic tool_use → ContentBlock::ToolCall 映射
- 验证 OpenAI function_call → ContentBlock::ToolCall 映射
- 在 `AgentLoop::run_with_observer()` 中接入真实 Provider (不再 mock)
- 实现 `run_streaming_iteration_async()` 的真实流式推理

**P0-2: TUI ↔ AgentLoop 真实交互闭环**

- TUI `impl AgentLoopObserver` — 将 observer 回调映射到 TUI 组件
- 流式推理 typewriter 效果 (`on_stream_chunk` → TUI text panel)
- 范式切换 UI (`on_paradigm_switch` → TUI status bar)
- Approval 交互 UI (`on_approval_request` → TUI approval card)
- 多轮对话 (上下文保持，`run_with_conversation()`)

**P0-3: MCP connect_and_discover() 实现完成**

- 完成 `McpRealClient::connect_and_discover()` 的 stdio transport
- 实现 SSE transport 的 MCP 客户端
- 在 ToolRegistry 中注册发现的 MCP 工具
- 验证 MCP 工具在 AgentLoop 中的执行路径

**P0-4: DefaultSubAgentFactory 真实实现**

- 用 AgentLoop 包装 SubAgent — 每个 SubAgent 是一个带 scoped tools + system prompt + budget 的 mini AgentLoop
- 实现 `spawn_sub_agent()` 的真实异步执行 (`tokio::spawn`)
- 从 DomainPack 的 SubAgentTypeDefinition 读取配置 (工具过滤 + system prompt)
- 实现 SubAgentStructuredOutput — 定义 JSON Schema，验证子代理返回值

**P0-5: E2E 测试框架**

- 创建 `tests/e2e/` 目录
- 实现 mock LLM provider (返回预设 tool call序列)
- 场景: "读取文件 → 编辑文件 → 完成" 的完整 AgentLoop 测试
- 场景: "范式切换 Plan→ReAct→Reflect" 的完整循环测试
- 场景: "子代理委派" 的完整路径测试

### Phase 1: 体验提升 (2 → 4 个月)

> **目标: CLI 体验达到 Claude Code 级别**

```
Phase 1 优先级矩阵
┌───────────────────────────────────────────────────────────┐
│  P1-1  GrepTool/GlobTool 原生 Rust 实现      [严重]      │
│  P1-2  Lifecycle Hooks 系统 (Pre/Post)        [重要]      │
│  P1-3  Interrupt/Resume (LangGraph 级)        [重要]      │
│  P1-4  StructuredOutput + ModelRetry           [重要]      │
│  P1-5  WebFetchTool 真实实现                   [中等]      │
│  P1-6  NotebookEditTool 真实实现               [中等]      │
│  P1-7  Provider Responses API                  [中等]      │
└───────────────────────────────────────────────────────────┘
```

**P1-1: 原生搜索工具**

- GrepTool → 使用 `grep` crate (ripgrep 的 Rust library 版本)
- GlobTool → 使用 `glob` + `walkdir` crate
- 消除 shell out 安全隐患和平台不一致性

**P1-2: Lifecycle Hooks 系统**

```rust
/// Claude Code-style lifecycle hooks
pub enum HookPoint {
    PreToolUse,    // 工具执行前 — 可 allow/deny/modify
    PostToolUse,   // 工具执行后 — 可记录/审计
    PreInfer,      // LLM 推理前 — 可修改请求
    PostInfer,     // LLM 推理后 — 可修改响应
    PreCheckpoint, // checkpoint 保存前
    Notification,  // 通知事件
    Stop,          // 循环终止前
}

pub enum HookResult {
    Allow,
    Deny { reason: String },
    Modify { modified_args: serde_json::Value },
}
```

- 这是 **围栏安全 → 生命周期安全** 的关键演进
- 允许 CI/CD 场景下的全自动 Agent (hooks 替代人工审批)
- 允许审计/合规场景 (PostToolUse 记录所有工具调用)

**P1-3: Interrupt/Resume**

- 在 `LoopState` 中添加 `interrupt_points: Vec<InterruptPoint>`
- `AgentLoopObserver::on_interrupt()` → UI 显示暂停状态
- `AgentLoop::resume_from_interrupt()` → 注入人类反馈并继续
- 集成到 StateGraph 的 `interrupt: true` node 标记
- **这是 HITL 从 "审批门" 到 "暂停/恢复" 的关键补充**

**P1-4: StructuredOutput + ModelRetry**

```rust
pub struct StructuredOutputConfig {
    /// JSON Schema for validating model output
    schema: serde_json::Value,
    /// Maximum retry attempts when validation fails
    max_retries: usize,
    /// Whether to re-prompt with error message (PydanticAI's ModelRetry pattern)
    re_prompt_on_failure: bool,
}

/// ModelRetry — when structured output fails validation,
/// re-prompt the model with the validation error for self-correction
pub struct ModelRetry {
    pub error_message: String,
    pub retry_count: usize,
}
```

- 在 AgentLoop 中添加 structured output 验证层
- 解析后验证 JSON Schema，失败时重新推理 (带错误信息)
- 这是 **Rust 版 PydanticAI** 的核心特性

**P1-5~P1-7: 工具和 Provider 补完**

- WebFetchTool → reqwest 实现 HTTP fetch
- NotebookEditTool → 解析 .ipynb JSON, 修改 cell, 写回
- Anthropic Responses API → 新 endpoint

### Phase 2: 架构升级 (4 → 8 个月)

> **目标: 多Agent闭环 + 可观测性 + Memory 闭环**

```
Phase 2 优先级矩阵
┌───────────────────────────────────────────────────────────┐
│  P2-1  Worktree 隔离 + 并行子代理              [重要]      │
│  P2-2  StateGraph ↔ AgentLoop 闭环执行        [重要]      │
│  P2-3  OpenTelemetry 可观测集成                [重要]      │
│  P2-4  STM ↔ LTM 闭环 (自动 evict+检索)       [重要]      │
│  P2-5  A2A 协议 Rust SDK (前瞻性)              [前瞻]      │
│  P2-6  WASM 沙箱执行引擎                       [前瞻]      │
└───────────────────────────────────────────────────────────┘
```

**P2-1: Worktree 隔离 + 并行子代理**

- `worktree_isolation.rs` 已有骨架 — 补完 git worktree 创建/切换/清理
- 非 git 项目用 directory-level 隔离 (tmpdir + copy)
- `AsyncTaskRunner` 实现并行子代理执行
- 子代理完成后合并结果到主 LoopState

**P2-2: StateGraph ↔ AgentLoop 闭环**

- `StateGraphExecutor::execute_node()` 中的 `LlmInfer` action → 调用真实 provider
- `ToolCall` action → 调用真实 ToolRegistry
- `HumanApproval` action → 触发真实 ApprovalGate/Hooks
- `ConditionCheck` action → 基于真实 GraphState 变量路由
- DomainPack 中定义的 StateGraph 可通过 CLI `/wf run <name>` 执行

**P2-3: OpenTelemetry 可观测集成**

- 在 AgentLoop 每次推理/工具调用/范式切换处创建 OTEL span
- `oneai-trace` 扩展为 OTEL exporter (OTLP 协议)
- Span 属性: model, tokens, cost, latency, paradigm, tool_name
- 集成 Grafana/Datadog/Jaeger 可视化
- **Rust OTEL SDK 是行业稀缺品 — OneAI 将是首个 Rust Agent 框架提供原生 OTEL**

**P2-4: STM ↔ LTM 闭环**

- 当 STM 滑动窗口驱逐旧消息时，自动写入 LTM (不是丢弃)
- LTM 写入时自动生成 embedding (调用 EmbeddingService)
- AgentLoop 推理前，自动从 LTM 检索相关记忆注入上下文
- 实现 hybrid scoring (时间衰减 + 向量相似度 + 关键词匹配)
- **这将使 OneAI 成为首个真正闭环 STM→LTM→检索→注入 的框架**

**P2-5: A2A 协议 Rust SDK (前瞻性布局)**

- 实现 Agent Card JSON 解析/生成
- 实现 Task lifecycle (SUBMITTED → WORKING → COMPLETED/FAILED)
- 实现 SSE streaming for long-running tasks
- 实现 Push notification callback
- **Rust A2A SDK 将是行业首个，可成为 Google A2A 生态的 Rust 参考实现**

**P2-6: WASM 沙箱执行引擎**

- 基于 Wasmtime 实现 WASM runtime
- 代码工具 (ShellTool 的替代) → WASM 模块
- 安全模型: 无文件系统访问、无网络访问、仅内存计算
- 允许 Agent 生成 WASM 代码作为动作 (Smolagents code-as-action 的 Rust 安全版本)

### Phase 3: 产品化 (8 → 12 个月)

> **目标: 可发布的 Agent 产品 + 生态位确立**

```
Phase 3 优先级矩阵
┌───────────────────────────────────────────────────────────┐
│  P3-1  Agent SDK API 稳定化 + 文档            [关键]      │
│  P3-2  DomainPack 市场 (Research/Data/IoT)    [重要]      │
│  P3-3  CLI 产品级打磨                          [关键]      │
│  P3-4  Playground/Studio Web UI               [重要]      │
│  P3-5  Eval 框架 + Benchmark                  [重要]      │
│  P3-6  插件生态 (MCP server market)            [前瞻]      │
└───────────────────────────────────────────────────────────┘
```

**P3-1: API 稳定化 + 文档**

- 公开 API 文档 (rustdoc + mdbook)
- 稳定化 trait 接口 (LlmProvider, Tool, ContextSource 等)
- 版本化策略 (semver)
- Migration guide (0.1 → 1.0)

**P3-2: DomainPack 市场**

- ResearchPack (已有骨架) → 完整实现
- DataAnalysisPack (SQL+Chart+Stats)
- IoTPack (设备控制+传感器+规则引擎)
- CreativePack (写作+设计+多模态)
- 社区贡献 DomainPack 的模板/规范

**P3-3: CLI 产品级打磨**

- 参考 Claude Code 的完整交互设计:
  - `/help`, `/clear`, `/compact`, `/model`, `/domain` 命令
  - 权限管理界面 (allow/deny/modify per-tool)
  - Session 管理 (list/resume/delete)
  - 多 session 并行 (类似 Claude Code 的 worktree session)
- 性能优化 (首屏 < 500ms, 推理延迟 < 2s)

**P3-4: Playground/Studio Web UI**

- 类似 LangGraph Studio 的可视化调试器
- StateGraph 可视化 (节点 + 边 + 当前执行位置)
- AgentLoop 实时追踪 (每个 iteration 的决策/工具/结果)
- Checkpoint 时间旅行 (选择任意 checkpoint 恢复)
- **Rust 后端 + WASM frontend → 唯一全 Rust Agent Studio**

**P3-5: Eval 框架 + Benchmark**

- 基于 OpenInference trace 的自动评估
- 指标: success_rate, cost_efficiency, latency, tool_accuracy
- Benchmark 场景集 (coding, research, data analysis)
- 对标 Claude Code/Codex 的同类任务对比

**P3-6: MCP Server 插件生态**

- OneAI 官方 MCP servers (filesystem, git, database, web)
- MCP server discovery registry
- 社区 MCP server 提交规范

### Phase 4: 生态位确立 (12 → 18 个月)

> **目标: 成为 "Rust Agent 生态的基础设施层"**

```
Phase 4 — OneAI 的生态位
┌──────────────────────────────────────────────────────────────────┐
│                                                                    │
│   Python 生态: LangGraph / CrewAI / AutoGen / PydanticAI          │
│   TypeScript: Claude Code / Vercel AI SDK                          │
│   ↓                                                                │
│   Rust 生态: OneAI = 基础设施层                                     │
│                                                                    │
│   OneAI 的三层价值:                                                  │
│   1. Rust Agent SDK — 类型安全、高性能、跨平台                       │
│   2. DomainPack 标准 — 领域配置的通用抽象                             │
│   3. A2A + MCP Rust 实现 — Agent 通信的基础设施                     │
│                                                                    │
│   独特定位:                                                         │
│   - 不是 Python 框架的 Rust 翻译                                    │
│   - 是 Agent 生态中唯一的生产级 Rust 实现                             │
│   - 跨平台 (CLI + Desktop + Mobile) 是 Python 框架永远做不到的       │
│   - 类型安全 + WASM 沙箱是 Python 框架永远做不到的                    │
│                                                                    │
└──────────────────────────────────────────────────────────────────┘
```

**P4 核心方向:**

1. **A2A 协议官方 Rust 参考实现** — 与 Google A2A 项目合作，成为官方 Rust SDK
2. **MCP Rust SDK 官方化** — 与 Anthropic MCP 项目合作，成为官方 Rust SDK
3. **DomainPack 规范标准化** — 提出 DomainPack 作为领域配置的开放规范
4. **WASM Agent Runtime** — 任何语言编写的 Agent 都可以在 OneAI WASM runtime 中安全执行
5. **UniFFI 生态** — Kotlin/Swift/C# SDK 成为移动端 Agent 开发的标准选择

---

## 四、技术决策建议

### 4.1 立即行动 (本周)

| 行动 | 原因 |
|------|------|
| 创建 `tests/e2e/` + MockProvider | 没有真实运行验证的架构是空中楼阁 |
| 修复 `parse_decision()` 的 Provider 格式映射 | 这是 AgentLoop 跑通的第一个障碍 |
| 完成 MCP `connect_and_discover()` stdio transport | MCP 是工具生态的通用连接器 |
| GrepTool/GlobTool 改用 `grep` + `walkdir` crate | 消除安全隐患和平台依赖 |

### 4.2 架构不变原则

| 原则 | 说明 |
|------|------|
| **DomainPack 是核心，不可削弱** | 这是 OneAI 与所有竞品的根本区别 |
| **Rust 类型安全是资产，不是负担** | 不要为了 "方便" 引入 stringly-typed 配置 |
| **crate 独立性必须保持** | 每个 crate 可独立使用，不强制全栈 |
| **trait-driven 设计不可退化为 enum-driven** | LlmProvider/Tool/ContextSource 是 trait，不是 enum |

### 4.3 架构演进方向

| 方向 | 说明 | 时间线 |
|------|------|--------|
| **围栏安全 → Lifecycle 安全** | ApprovalGate + Hooks = 完整安全模型 | Phase 1 |
| **单Agent → Agent 网络** | SubAgent + A2A = 横向扩展 | Phase 2-4 |
| **Memory 闭环** | STM → LTM → 检索 → 注入 = 真正的记忆 | Phase 2 |
| **Code-as-WASM-Action** | WASM 沙箱 = 安全的代码动作 | Phase 2-3 |
| **DomainPack 规范化** | 从 OneAI 特有 → 行业开放规范 | Phase 3-4 |

---

## 五、总结

**OneAI 当前状态: 架构设计世界级，实现深度 65%**

- DomainPack 体系是行业最先进的领域配置抽象 — **这是核心护城河**
- 19 crate 的模块化架构 + Rust 类型安全 — **这是工程护城河**
- 跨平台 UniFFI — **这是 Python 框架永远无法复制的护城河**

**最关键的下一步: 让 AgentLoop 在真实 LLM 下跑通一圈**

所有架构创新 (DomainPack、范式切换、SubAgent、StateGraph) 如果不通过真实 E2E 验证，就只是纸上设计。Phase 0 的 5 个 P0 任务是解锁一切后续演进的前提。

**OneAI 的终极定位: Rust Agent 生态的基础设施层 — 类型安全、跨平台、领域可插拔、Agent 可互联**

这不是与 LangGraph/CrewAI 竞争 Python 生态，而是在 Rust 生态中建立不可替代的基础设施价值 — MCP Rust SDK、A2A Rust SDK、DomainPack 规范、WASM Agent Runtime。这些是 Python 框架永远做不到的事。

---

## 附录: 竞品深度参考

| 框架 | 关键学习点 | OneAI 可借鉴 |
|------|-----------|-------------|
| LangGraph | 有向图+checkpoint 时间旅行 | StateGraph 需要达到同级别的 interrupt/resume/replay |
| Claude Code | Permission+Hooks lifecycle 安全 | Lifecycle Hooks (allow/deny/modify) 应成为 P1 优先 |
| OpenAI SDK | Handoff-as-tool-call + 内置 tracing | SubAgent 委派应采用同模式，trace 应扩展为 OTEL |
| AutoGen | 对话即架构 + circuit breaker | RecoveryManager 应增加 circuit breaker 模式 |
| PydanticAI | ModelRetry 自我纠错 | StructuredOutput+ModelRetry 是 Rust 类型安全的自然延伸 |
| Smolagents | Code-as-Action + AST sandbox | WASM 沙箱是 Rust 的更安全版本 |
| Agno | 9+向量库 + Team 模式 | RAG 应扩展更多 embedding backend |
| CrewAI | role/backstory 角色化 | DomainPack 的 SubAgentTypeDefinition 已覆盖此模式 |
| Google ADK | A2A 协议 | Phase 2 应前瞻性布局 A2A Rust SDK |
| Semantic Kernel | 跨语言一致 (C#/Python/Java) | UniFFI 已实现类似，应加强跨平台一致性测试 |
