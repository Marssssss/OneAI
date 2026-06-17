# OneAI vs Claude Code / Codex CLI / OpenCode — 全维度对比与差距分析

## Context

OneAI 是一个跨平台 Agent 框架（20 crate Rust 架构），目标是成为通用领域可切换的 Agent 平台。Claude Code、Codex CLI、OpenCode 是当前最先进的 coding agent 实现。本分析从 14 个维度系统对比，识别差距并给出改进/重构方案。

---

## 1. Agent Loop & 执行模型

| 特性 | OneAI | Claude Code | Codex CLI | OpenCode |
|------|-------|-------------|-----------|----------|
| 循环类型 | 动态循环 + 4 范式切换 | 单循环 (text/tool_calls) | 单循环 (shell/apply_patch) | 单循环 (Effect-TS) |
| 决策类型 | DirectAnswer/ToolCalls/Delegate/SwitchParadigm | text/tool_calls | shell/apply_patch | text/tool_calls |
| 范式切换 | Plan/ReAct/Reflect/Explore (概念完整) | 无 (EnterPlanMode 是工具) | 无 | 无 |
| 最大迭代 | hard_max_iterations=50 | 无硬限制 | 无硬限制 | 无硬限制 |
| 预算控制 | TokenBudget + ContextBudgetManager | 无显式预算 | 无 | 无 |

### ⚠️ 关键差距

**[Critical] `spawn_sub_agent()` 和 `run_paradigm()` 是伪实现**
- `sub_agent.rs:764-773`: `spawn_sub_agent()` 返回硬编码 `SubAgentSummary`，不实际运行子代理
- `agent_loop.rs:776-778`: `run_paradigm()` 返回字符串 `"{} paradigm applied"`，不实际切换行为
- 整个 `Delegate` 和 `SwitchParadigm` 决策路径是死代码

**[High] 范式切换语义空洞**
- 当 `SwitchParadigm` 触发时，不重新配置 system prompt、可用工具集或决策解析规则

**[Medium] 无 Responses API 支持**
- Anthropic provider 仅实现 Messages API，未实现 agent-oriented Responses API

**[Low] hard_max_iterations 应保留而非去除**
- TokenBudget 是计量约束 (控制总 token 用量)，hard_max_iterations 是逻辑约束 (防止无限循环)
- 当 budget 计量出错 (provider 返回 0 usage) 或模型反复调用同一工具时，hard_max_iterations 是唯一安全绳
- OneAI 的 Delegate/SwitchParadigm 增加了非自然终止路径，更需要兜底
- **建议**: 保留但提高默认值到 200，或设为 Option<usize> (None = 仅 budget 约束)

**[Medium] Provider tool calling 格式差异未处理**
- Anthropic 用 `tool_use` content block，OpenAI 用 `function_call` 格式
- `parse_decision()` 用通用 ContentBlock 解析，但不同 provider 的 tool call 格式差异可能导致解析不一致
- 需验证 OpenAI provider 的 tool call 格式是否与 Anthropic 格式正确映射到同一 ContentBlock

### 🔧 改进方案

1. **实现 DefaultSubAgentFactory::create()** — 用现有 paradigm agent 包装为 SubAgent trait 实现，附带 scoped tool set + system prompt + token budget
2. **让范式切换生效** — SwitchParadigm 时实际切换 system prompt、过滤工具、调整决策解析
3. **添加 Responses API** — 在 anthropic.rs 中实现 Responses API endpoint

---

## 2. 工具系统

| 特性 | OneAI | Claude Code | Codex CLI | OpenCode |
|------|-------|-------------|-----------|----------|
| 工具数量 | 8+2 (coding+calc/write) | 13+ | 2 (shell+apply_patch) | 13 |
| 工具描述 | 1-2 句简短描述 | 多段落 + 用法指南 + 示例 | 简短但精准 | 中等详细 |
| 搜索实现 | shell out (grep/find) | 原生 ripgrep | 原生 grep | 原生 grep |
| 文件编辑 | old_string/new_string | old_string/new_string | apply_patch (结构化diff) | old_string/new_string |
| Web 工具 | 无本地实现 (MCP todo) | WebFetch + WebSearch | 无 | WebFetch + WebSearch |
| Notebook | placeholder | 完整实现 | 无 | 完整实现 |

### ⚠️ 关键差距

**[Critical] GrepTool/GlobTool shell out 到 OS 命令**
- `tool_interfaces.rs:862-910`: GrepTool 调用 `grep`/`Select-String` shell 命令
- `tool_interfaces.rs:985-1026`: GlobTool 调用 `find`/`Get-ChildItem` shell 命令
- Read-level 工具却调用 shell，存在安全绕过风险
- 平台依赖 (Windows/Unix 不同行为)

**[Critical] 无 WebFetch/WebSearch 本地工具**
- MCP web_search config 定义了但 `connect_and_discover()` 是 `todo!()`

**[High] NotebookEditTool 是 placeholder**
- `tool_interfaces.rs:1185-1205`: 仅返回 "Notebook edit operation recorded"

**[High] 工具描述过于简短**
- 1-2 句话，缺乏行为指南、示例、偏好建议

### 🔧 改进方案

1. **GrepTool/GlobTool 改为原生 Rust 实现** — 使用 `ripgrep` crate + `walkdir`/`glob` crate，不依赖 OS shell
2. **添加 WebFetchTool/WebSearchTool** — `reqwest` 实现 HTTP fetch，搜索 API 集成
3. **实现 NotebookEditTool** — 解析 .ipynb JSON，修改 cell，写回文件
4. **丰富工具描述** — 在 CodingPack ToolDecorator 和 base description 中增加行为指南

---

## 3. 子代理 / 多代理

| 特性 | OneAI | Claude Code | Codex CLI | OpenCode |
|------|-------|-------------|-----------|----------|
| 子代理类型 | 4+Custom (Plan/Explore/Code/Review) | 6 (Explore/Plan/claude等) | 无 | 无 |
| 返回值 | SubAgentSummary (自由文本) | StructuredOutput (JSON Schema) | 无 | 无 |
| 隔离机制 | ScopeState (概念) | git worktree | 无 | 无 |
| 工具集过滤 | SubAgentTypeDefinition 定义了但未实现 | 每种类型有 scoped tools | 无 | 无 |
| 并行执行 | parallel_executor 存在 | 可并行 | 无 | 无 |

### ⚠️ 关键差距

**[Critical] DefaultSubAgentFactory::create() 是 `todo!()`**
- `sub_agent.rs:207`: panic，Delegate 决策路径是死代码

**[Critical] 无 StructuredOutput schema 验证**
- SubAgentSummary.summary 和 key_findings 是自由文本，无 JSON Schema 验证

**[High] 无 git worktree 隔离**
- Claude Code 为并行子代理创建独立 git worktree，OneAI 无此概念

**[High] scoped tool set 未实际过滤**
- CodingPack 定义了 available_tools 但 factory todo!() 所以未实现

### 🔧 改进方案

1. **实现 DefaultSubAgentFactory** — 用 paradigm agent 包装，从 SubAgentTypeDefinition 读取配置
2. **添加 StructuredOutput** — 定义 JSON Schema，验证子代理返回值，提取结构化字段
3. **实现 git worktree 隔离** — 或 directory-level 隔离 (非 git 项目)
4. **实际过滤工具集** — 创建子代理时根据 available_tools 过滤 ToolRegistry

---

## 4. 权限与安全

| 特性 | OneAI | Claude Code | Codex CLI | OpenCode |
|------|-------|-------------|-----------|----------|
| 分级系统 | PermissionLevel (Read/Standard/Full) | 3-tier (类似) | 3-mode (suggest/auto-edit/full-auto) | Once/Always/Reject |
| 域级覆盖 | PermissionProfile (domain pack) | 无 (硬编码) | 无 | 无 |
| 沙箱 | SandboxMode enum (未实现) | macOS Seatbelt + Docker nsjail | Docker (主要安全边界) | 无 |
| 命令黑名单 | regex 黑名单 | 更全面 | Docker 隔离 | 无 |
| 审批交互 | ChannelApprovalGate + TUI | TUI 交互 | CLI 交互 | TUI 交互 |
| 观察模式 | ApprovalResponse::Observe | 无 | 无 | 无 |

### ⚠️ 关键差距

**[Critical] 无真实沙箱实现**
- `tool_interfaces.rs:75-86`: SandboxMode::Enabled 不做任何实际隔离
- 命令黑名单 (regex) 是表面过滤，非进程级隔离
- Claude Code 用 macOS Seatbelt 实际限制进程，Codex 用 Docker 实际隔离

**[High] Read-level 工具调用 shell**
- GrepTool/GlobTool 是 Read 级别 (auto-approved) 却调用 shell 命令

**[Medium] 无 Docker/nsjail 沙箱选项**

### 🔧 改进方案

1. **实现真实沙箱** — macOS: `sandbox-exec` profile; Linux: Docker/nsjail; Windows: AppContainer 或 restricted token
2. **消除 GrepTool/GlobTool shell 依赖** (见工具系统改进)
3. **添加 Docker 沙箱 backend** — 可配置沙箱后端

---

## 5. 上下文管理

| 特性 | OneAI | Claude Code | Codex CLI | OpenCode |
|------|-------|-------------|-----------|----------|
| 上下文注入 | ContextAssembler + ContextSource | per-iteration injection | 无 | Context Epoch (baseline→incremental) |
| 刷新策略 | EveryIteration/OnceAtStart/OnChange/Periodic | 每轮注入 | 无 | 基线+增量 |
| 变化检测 | EnvironmentDiff (概念) | 无显式 | 无 | diff-based |
| 压缩 | ContextBudgetManager + CompressionTemplate | 无显式压缩 | 无 | 无 |
| 缓存 | 无 | Anthropic prompt caching | 无 | 无 |
| 项目指令 | 无 | CLAUDE.md | 无 | AGENTS.md |

### ⚠️ 关键差距

**[Critical] `take_snapshot()` 是 `todo!()`**
- `context_assembler.rs:265-268`: EnvironmentDiff 机制无法工作
- `OnChange` policy 实现为 "always load" (line 222-224)

**[Critical] 无项目指令文件读取**
- 零引用 CLAUDE.md/AGENTS.md/ONEAI.md
- 项目特定行为指南是 coding agent 的**核心上下文来源** — Claude Code 的 CLAUDE.md 是影响 Agent 行为质量的第一因素
- CodingPack 的 5 个 ContextSource (GitStatus/FileTree/ProjectConfig/Date/Environment) 中没有项目指令来源
- 项目指令包含：代码风格、技术约束、测试要求、部署规范、团队偏好 — 这些信息直接决定 Agent 输出的准确度

**[High] 无 Context Epoch 模式**
- OpenCode 的创新: 首轮注入基线，后续仅注入增量
- OneAI 的 RefreshPolicy::OnChange 暗示此模式但未实现 (assemble() 中 OnChange 实现为 "always load")
- **Token 成本影响**: 全量刷新每轮注入 file tree + git status ≈ 2000-5000 tokens，增量仅 ≈ 100-500 tokens
- 在 20-50 轨对话中，差异可达 **50,000-250,000 tokens** — 这是 P1 成本问题而非 P2 体验问题

**[High] 无 prompt caching**
- Anthropic provider 不设置 cache_control headers
- 重复发送 system prompt + context 每轮浪费 token

### 🔧 改进方案

1. **实现 take_snapshot()** — 扫描文件变化、git status，计算 EnvironmentDiff
2. **添加 ProjectInstructionsSource** — 读取 ONEAI.md/CLAUDE.md/AGENTS.md，OnceAtStart + 高优先级
3. **实现 Context Epoch** — 首轮完整注入，后续仅注入 diff (compute_diff 已实现)
4. **添加 prompt caching** — Anthropic provider 设置 cache_control: ephemeral

---

## 6. 记忆与持久化

| 特性 | OneAI | Claude Code | Codex CLI | OpenCode |
|------|-------|-------------|-----------|----------|
| STM | ShortTermMemorySync (滑动窗口) | conversation context | conversation | conversation |
| LTM | LongTermMemory (vector+content) | 无 | 无 | 无 |
| 嵌入 | 自实现简易 HNSW | 无 | 无 | 无 |
| 压缩 | LLM summarization + CompressionTemplate | 无 | 无 | 无 |
| 检查点 | ProgressiveCheckpoint + FilePersistence | 无显式 | 无 | 无 |
| 混合评分 | hybrid_scorer (语义+时间) | 无 | 无 | 无 |

### ⚠️ 差距

**[Medium] ProgressiveCheckpointManager::list() 是 todo!()**
**[Medium] SqliteCheckpointBackend 未实现** (仅 FilePersistence)
**[Low] 向量存储嵌入提供者未接线**

### 🔧 改进方案

1. 实现 MemoryCheckpointBackend::list()
2. 添加 rusqlite 实现 SqliteCheckpointBackend
3. 接线嵌入提供者 (LLM embedding endpoint)

---

## 7. 错误恢复

| 特性 | OneAI | Claude Code | Codex CLI | OpenCode |
|------|-------|-------------|-----------|----------|
| 恢复策略 | 6种 (Retry/Fallback/Rollback/Assertion/ExternalFeedback/Escalate) | 基本 retry | 基本 retry | 无 |
| 外部验证 | ExternalValidator trait (概念创新) | 无 | 无 | 无 |
| 回滚 | checkpoint-based | 无 | 无 | 无 |
| 断言钩子 | Assertion trait | 无 | 无 | 无 |

### ⚠️ 差距

**[Medium] RecoveryManager.apply() 返回 outcome 但 AgentLoop 未接线**
- `agent_loop.rs:436-570`: run_loop() 无错误恢复集成

**[Low] 无内置 validators** (仅框架，无具体实现)

### 🔧 改进方案

1. 将 RecoveryManager 接入 AgentLoop — 每轮迭代后检查错误并应用恢复策略
2. 实现内置 validators: CompilationValidator, TestValidator, FileExistsValidator

---

## 8. 流式输出与 UI

| 特性 | OneAI | Claude Code | Codex CLI | OpenCode |
|------|-------|-------------|-----------|----------|
| 流式架构 | IncrementalStreamParser + AgentLoopObserver | TypeScript streaming | 基本流式 | Effect-TS + Vercel AI SDK |
| 工具意图检测 | ToolIntentDetected 事件 | 类似 | 无 | 无 |
| TUI | ratatui + Vim 模式 | terminal UI | 基本 CLI | terminal UI |
| 审批 UI | ChannelApprovalGate + TUI card | 交互式 | CLI 交互 | 交互式 |
| 渲染缓存 | MessageRenderCache | 无 | 无 | 无 |

### ⚠️ 差距

**[Medium] Anthropic 流式 tool call 累积损坏**
- streaming 时 ToolCall args 始终为 "{}" (空)
- `anthropic.rs:370-402`: input_json_delta 未正确组合

**[Medium] AgentLoop 流式模式未使用 IncrementalStreamParser**
- `run_streaming_iteration_async()` 手动累积，绕过了专门设计的解析器

### 🔧 改进方案

1. 修复 Anthropic streaming tool call 累积
2. 重构 run_streaming_iteration_async() 使用 IncrementalStreamParser
3. 确保 TUI observer bridge 正确连接 on_stream_chunk()

**[Critical] Anthropic 流式 tool call args 始终为空 `"{}"`**
- `anthropic.rs:395`: 注释明确说 `args: "{}".to_string(), // Simplified`
- `input_json_delta` 事件中的参数片段未被累积组合到最终 ToolCall 中
- 所有 Anthropic 流式 tool call 无法传递参数，是功能级缺陷

**[Medium] IncrementalStreamParser 未被流式迭代使用**
- `run_streaming_iteration_async()` (`agent_loop.rs:784-876`) 手动累积文本和 tool call
- 完全绕过了专门设计的 `IncrementalStreamParser` (`streaming.rs`)
- parser 存储为 AgentLoop 字段但从未在流式迭代中被调用

**[Medium] TUI Observer Bridge 需验证**
- AgentLoopObserver 有 13+ 回调，但 `observer.rs` 和 `session.rs` 的桥接实现是否正确连接所有回调需实际运行验证
- `on_stream_chunk()` 的 typewriter 效果是否正确传递到 TUI 渲染

### 🔧 改进方案

1. **修复 Anthropic streaming tool call 累积** — 为每个 tool call ID 维护独立的 args buffer，在 `input_json_delta` 事件中累积片段，在 `content_block_stop` 时组合为完整 JSON
2. **重构 run_streaming_iteration_async()** — 使用 `IncrementalStreamParser::process_chunk()` 替代手动累积
3. **验证 TUI observer bridge** — 确保所有 13 个 Observer 回调正确连接到 App state 更新

---

## 9. MCP 集成

| 特性 | OneAI | Claude Code | Codex CLI | OpenCode |
|------|-------|-------------|-----------|----------|
| 架构 | McpConnection + McpServerManager + 3 transport | 完整实现 | 无 | 可能通过 SDK |
| 传输 | Stdio/SSE/StreamableHttp (概念) | Stdio/SSE | 无 | Stdio |
| 默认服务器 | filesystem MCP (概念) | 多个预注册 | 无 | 可能 |
| API Key 配置 | 可选 (web_search) | 支持 | 无 | 支持 |

### ⚠️ 关键差距

**[Critical] 所有 MCP 方法是 `todo!()`**
- `mcp_real.rs:124-131`: connect_and_discover()
- `mcp_real.rs:136-147`: call_tool()
- MCP 集成结构完整但功能全部死亡

### 🔧 改进方案

1. **用 rmcp crate 实现 MCP 协议** — connect_and_discover() + call_tool()
2. 添加服务器生命周期管理 (健康检查、重连、优雅关闭)
3. 添加更多默认 MCP config (GitHub, Slack, database 等)

---

## 10. 域配置 (DomainPack)

| 特性 | OneAI | Claude Code | Codex CLI | OpenCode |
|------|-------|-------------|-----------|----------|
| 配置方式 | DomainPack 5层声明式 | 硬编码 TypeScript | 3 autonomy mode 硬编码 | AGENTS.md + skill |
| 可扩展性 | 可添加新域 (ResearchPack 等) | 需修改源码 | 需修改源码 | 文件驱动 |
| 域合并 | MergedDomainPack (strictest wins) | 无 | 无 | 无 |
| 声明式配置 | 仅 Rust 代码构建 | 无 | 无 | YAML/TOML |

**OneAI 在此维度是领先者** — DomainPack 架构是真正的差异化优势

### ⚠️ 差距

**[Medium] 仅 CodingPack 实现**
**[Low] DomainPack 无 YAML/TOML 文件格式**

**⚠️ DomainPack 实现成熟度修正评估**

自评评估了各层 40-70% 完成度，但实际代码级审查显示更低的成熟度：

```
第1层(工具集) | 自评 70% → 实际 50% (Grep/Glob shell out, Notebook placeholder, 无 Web 工具)
第2层(上下文) | 自评 40% → 实际 20% (take_snapshot todo!(), 无项目指令, OnChange 退化)
第3层(权限)   | 自评 80% → 实际 60% (配置完整但 Read-level 工具 shell out 绕过体系)
第4层(范式)   | 自评 30% → 实际 10% (spawn_sub_agent fake, run_paradigm fake)
第5层(压缩)   | 自评 50% → 实际 30% (NoopCompressor 默认, 截断 fallback 无)
```

整体 DomainPack 实现成熟度约 **25-30%** 而非自评暗示的 40-50%。灵活的配置系统在底层有漏洞 (Read-level 工具调用 shell 绕过权限体系) 时，不如简单可靠的硬编码系统。

### 🔧 改进方案

1. 实现 ResearchPack、DataAnalysisPack 等
2. 添加 YAML/TOML DomainPack 格式，允许用户声明式配置域

---

## 11. 跨平台

| 特性 | OneAI | Claude Code | Codex CLI | OpenCode |
|------|-------|-------------|-----------|----------|
| 桌面 | ✅ ratatui TUI | ✅ CLI | ✅ CLI | ✅ CLI |
| Android | ✅ UniFFI + JNI bridge | ❌ | ❌ | ❌ |
| iOS | ✅ UniFFI + callback bridge | ❌ | ❌ | ❌ |
| HarmonyOS | ✅ callback bridge | ❌ | ❌ | ❌ |
| 审批桥接 | PlatformApprovalGate per platform | 仅桌面 | 仅桌面 | 仅桌面 |

**OneAI 在此维度是唯一有跨平台支持的**

### ⚠️ 差距

**[Low] Platform gates 可能是 stub**
**[Low] UniFFI bindings 可能未覆盖全部 API**

### 🔧 改进方案

- 验证并完成各平台 native approval gate 实现
- 确保 UniFFI bindings 覆盖完整 AppBuilder API

---

## 12. 项目指令

| 特性 | OneAI | Claude Code | Codex CLI | OpenCode |
|------|-------|-------------|-----------|----------|
| 项目指令文件 | ❌ 无 | ✅ CLAUDE.md | ❌ | ✅ AGENTS.md |
| 层级指令 | ❌ | ✅ 项目/子目录/用户目录 | ❌ | ✅ |
| 动态刷新 | ❌ | ✅ 检测文件变化 | ❌ | ✅ |

### ⚠️ 关键差距

**[Critical] 完全缺失项目指令读取**

### 🔧 改进方案

1. **添加 ProjectInstructionsSource** — 读取 ONEAI.md (兼容 CLAUDE.md/AGENTS.md)
2. **支持层级指令** — 项目根、子目录、用户 home
3. **OnceAtStart + OnChange 刷新策略**

---

## 13. Workflow / StateGraph

| 特性 | OneAI | Claude Code | Codex CLI | OpenCode |
|------|-------|-------------|-----------|----------|
| DAG | WorkflowDag ✅ | ❌ | ❌ | ❌ |
| 有环图 | StateGraph (LangGraph-inspired) ✅ | ❌ | ❌ | ❌ |
| 条件边 | EdgeCondition ✅ | ❌ | ❌ | ❌ |
| 中断点 | interrupt field ✅ | ❌ | ❌ | ❌ |
| 范式→图编译 | ❌ 未实现 | ❌ | ❌ | ❌ |

**OneAI 在此维度架构最先进**

### ⚠️ 差距

**[Medium] StateGraph executor 需验证条件边/中断点**
**[Low] ParadigmStrategy 未编译为 StateGraph**

---

## 14. 多 Provider

| 特性 | OneAI | Claude Code | Codex CLI | OpenCode |
|------|-------|-------------|-----------|----------|
| Anthropic | ✅ Messages API | ✅ (原生) | ❌ | ✅ |
| OpenAI | ✅ Chat Completions | ❌ | ✅ Responses API | ✅ |
| Ollama | ✅ | ❌ | ❌ | ✅ |
| Google | ❌ | ❌ | ❌ | ✅ (Gemini) |
| 自动检测 | ✅ ProviderFactory | ❌ | ❌ | ✅ |
| Provider 特性 | ❌ 通用接口 | ✅ caching/thinking | ✅ Responses | ✅ |

### ⚠️ 差距

**[High] 无 Responses API**
**[Medium] 无 Google/Gemini provider**
**[Medium] Anthropic streaming tool call 损坏**
**[Low] ModelCapability 不含 provider 特性 (caching/Responses)**

---

## 🔥 优先级总结

### Critical (必须立即解决)

| # | 差距 | 维度 | 工作量 | 关键文件 |
|---|------|------|--------|----------|
| 1 | DefaultSubAgentFactory todo!() | 子代理 | 高 | `sub_agent.rs:207` |
| 2 | 无真实沙箱 | 安全 | 高 | `tool_interfaces.rs:75-86` |
| 3 | MCP connect/call todo!() | MCP | 高 | `mcp_real.rs:124-147` |
| 4 | GrepTool/GlobTool shell out | 工具 | 中 | `tool_interfaces.rs:862-1026` |
| 5 | 无项目指令读取 | 上下文 | 中 | (新增文件) |
| 6 | take_snapshot() todo!() | 上下文 | 中 | `context_assembler.rs:265` |
| 7 | 无 WebFetch/WebSearch | 工具 | 中 | (新增文件) |

### High (短期应解决)

| # | 差距 | 维度 | 工作量 |
|---|------|------|--------|
| 8 | 无 StructuredOutput schema | 子代理 | 中 |
| 9 | 范式切换语义空洞 | Agent Loop | 中 |
| 10 | 无 git worktree 隔离 | 子代理 | 高 |
| 11 | 无 Context Epoch | 上下文 | 中 |
| 12 | 无 prompt caching | 上下文 | 低 |
| 13 | NotebookEdit placeholder | 工具 | 中 |
| 14 | 无 Responses API | 多Provider | 高 |
| 15 | 工具描述太简 | 工具 | 低 |

### Medium (中期改进)

| # | 差距 | 维度 | 工作量 |
|---|------|------|--------|
| 16 | RecoveryManager 未接线 | 错误恢复 | 中 |
| 17 | Anthropic streaming tool call 损坏 | 流式 | 中 |
| 18 | ProgressiveCheckpoint list todo!() | 记忆 | 低 |
| 19 | SqliteCheckpoint 未实现 | 记忆 | 中 |
| 20 | 仅 CodingPack | 域 | 中 |
| 21 | 无 Google/Gemini provider | 多Provider | 中 |

### 补充遗漏项 (来自深度审查修正)

| # | 差距 | 维度 | 工作量 | 说明 |
|---|------|------|--------|------|
| 22 | Anthropic streaming tool call args 始终为 `"{}"` | 流式/Provider | 中 | `anthropic.rs:395` input_json_delta 未累积 |
| 23 | IncrementalStreamParser 未被流式迭代使用 | 流式/Agent Loop | 中 | `agent_loop.rs:784-876` 手动累积绕过 parser |
| 24 | Provider tool calling 格式差异映射 | 多Provider | 低 | Anthropic tool_use vs OpenAI function_call 格式一致性 |
| 25 | Skill 系统实际可用性未验证 | Skill | 低 | SkillSelector 可能是 stub 或基础实现 |
| 26 | AppSession 端到端流程未验证 | App | 低 | AppBuilder→AgentLoop→Tool→Observer 全链路 |
| 27 | TUI Observer Bridge 回调连接完整性 | TUI/流式 | 低 | 13 个 Observer 回调是否全部正确连接 |
| 28 | 跨平台 FFI bindings API 覆盖完整性 | 跨平台 | 中 | UniFFI 是否覆盖 domain_pack/agent_loop/app_builder |
| 29 | hard_max_iterations 应保留为安全兜底 | Agent Loop | 低 | 保留但提高默认值到 200 或设为 Option |

---

## 🏗️ 重构建议 (整体架构层面)

### 1. 子代理系统重构 (Critical #1)

当前 `DefaultSubAgentFactory::create()` 是 todo!()，整个 Delegate 路径是死代码。重构方案：

```
DefaultSubAgentFactory::create(kind, budget)
  → 从 MergedDomainPack.paradigm_strategies 查找 SubAgentTypeDefinition
  → 创建 scoped ToolRegistry (仅 available_tools 中列出的工具)
  → 使用 agent_type.system_prompt 作为 system prompt
  → 创建独立 AgentLoop 实例 (scoped provider + scoped tools + scoped budget)
  → 运行 AgentLoop.run() → 提取 StructuredOutput → 转换为 SubAgentSummary
```

### 2. 沙箱架构重构 (Critical #2)

当前 SandboxMode enum 不做任何事。重构方案：

```
SandboxBackend trait:
  - SeatbeltSandbox (macOS: sandbox-exec profile)
  - DockerSandbox (Linux: Docker container isolation)  
  - AppContainerSandbox (Windows: restricted token/AppContainer)
  - NoSandbox (开发模式)

ShellTool::execute():
  1. 检查黑名单 (regex, 快速拒绝)
  2. 通过 SandboxBackend 执行命令 (实际隔离)
  3. 限制输出大小 (防止 context overflow)
```

### 3. MCP 实现重构 (Critical #3)

使用 rmcp crate 实现完整 MCP 协议：

```
McpConnection::connect_and_discover():
  1. 根据 McpTransport 创建 rmcp transport client
  2. 发送 InitializeRequest
  3. 发送 ListToolsRequest
  4. 解析响应 → 存储 McpToolInfo

McpConnection::call_tool():
  1. 发送 CallToolRequest
  2. 解析 CallToolResult → 转换为 ToolOutput
```

### 4. 工具系统重构 (Critical #4-7)

```
替换 GrepTool → RipgrepTool (使用 ripgrep crate, 原生 Rust)
替换 GlobTool → GlobNativeTool (使用 glob + walkdir crate)
新增 WebFetchTool (reqwest + HTML→markdown)
新增 WebSearchTool (搜索 API 集成)
新增 ProjectInstructionsSource (读取 ONEAI.md/CLAUDE.md)
实现 NotebookEditTool (解析 .ipynb JSON)
```

### 5. 流式架构修复 (补充遗漏 #22-23)

Anthropic provider 的流式 tool call 有 bug，且 IncrementalStreamParser 未被使用：

```
修复 anthropic.rs streaming tool call 累积:
  1. 为每个 tool call ID 维护独立的 args buffer (HashMap<String, String>)
  2. 在 input_json_delta 事件中累积参数片段到对应 buffer
  3. 在 content_block_stop 时组合完整 args JSON → 发出 ToolCall ContentBlock

重构 run_streaming_iteration_async():
  1. 使用 self.stream_parser.process_chunk(chunk) 替代手动 text_buffer/tool_call_buffers
  2. 处理 StreamEvent::ToolIntentDetected → observer.on_tool_calls()
  3. 处理 StreamEvent::ToolCallComplete → 累积到最终 ToolCall 列表
  4. 处理 StreamEvent::StreamComplete → 组装 InferenceResponse
```

### 6. 端到端验证与 Skill 系统 (补充遗漏 #25-27)

Skill 系统和 AppSession 的实际可用性需验证：

```
Skill 系统验证:
  1. 检查 SkillSelector 实现 (oneai-skill/src/selector.rs)
  2. 确认 skill 注册/选择/注入流程是否端到端可用
  3. 对比 OpenCode 的 Skill.md 注入机制

AppSession 端到端验证:
  1. 构建完整 App (AppBuilder → domain_pack → tools → provider)
  2. 创建 Session → 发送任务 → 触发 AgentLoop
  3. 验证 Observer 13 个回调是否全部传递到 App state
  4. 验证 TUI 渲染是否正确反映所有回调事件

跨平台 FFI API 覆盖验证:
  1. 检查 oneai-uniffi/src/lib.rs 导出的 API 列表
  2. 对比 AppBuilder 的全部方法 (provider/tool/domain_pack/approval/trace)
  3. 确认 Kotlin/Swift/C++/C# binding 都能调用 AgentLoop.run()
```

---

## ✅ 验证方案

### 端到端测试

1. **子代理验证**: 创建 Plan→Explore→Code→Reflect 流程，验证 Delegate 决策实际执行子代理并返回 StructuredOutput
2. **沙箱验证**: 在 ShellTool 中执行危险命令 (rm -rf), 确认沙箱拦截
3. **MCP 验证**: 连接 filesystem MCP server, 发现工具并调用
4. **上下文验证**: 首轮完整注入基线，后续仅注入 diff; CLAUDE.md 内容被注入
5. **流式验证**: Anthropic streaming tool call args 正确累积
6. **域切换验证**: coding_pack → research_pack 切换，工具/权限/system prompt 正确变化
7. **跨平台验证**: Android/iOS UniFFI binding 能构建 App 并执行工具调用

### 单元测试

- 每个 todo!() 替换为实际实现后，添加对应单元测试
- GrepTool/GlobTool 原生实现后，对比 shell-out 版本结果一致性
- 沙箱 backend 测试: 黑名单命中/miss, 超时, 输出截断
- Context Epoch 测试: 首轮基线注入, 后续仅 diff
- Anthropic streaming 测试: 验证 tool call args 在流式模式下正确累积 (不再为空 "{}")
- Skill 系统测试: SkillSelector 是否能实际选择和注入 skill
- AppSession 端到端测试: AppBuilder→AgentLoop→Tool→Observer→TUI 全链路运行
- TUI Observer Bridge 测试: 所有 13 个回调是否正确传递到 App state
- Provider 格式一致性测试: Anthropic 和 OpenAI provider 的 tool call 格式是否正确映射到同一 ContentBlock
- 跨平台 FFI 测试: UniFFI bindings 是否覆盖 domain_pack/agent_loop/app_builder 等高级 API
