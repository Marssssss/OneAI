# OneAI

> 跨平台 AI Agent 框架，基于 Rust 构建 — 模块化、类型安全、领域可插拔、可评测。

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE)
[![Crates: 19](https://img.shields.io/badge/Crates-19-orange.svg)]()
[![Tests: 257](https://img.shields.io/badge/Tests-257-green.svg)]()

---

## OneAI 是什么？

OneAI 是一个用 Rust 编写的全栈 Agent 框架。它提供了构建、运行和评测 AI Agent 所需的一切——从 LLM Provider 抽象到工具执行、记忆管理、工作流编排、领域专属配置和轨迹日志——全部支持通过 UniFFI bindings 实现跨平台。

**核心原则：**

- **模块化设计** — 19 个独立 crate，各司其职，按需使用。
- **类型安全** — 密封枚举层级、trait 驱动抽象，无字符串配置。
- **领域可插拔** — DomainPack 系统让领域知识声明式、可组合、一行切换。
- **跨平台** — 通过 UniFFI 支持 macOS、Windows、Linux、Android、iOS 和 HarmonyOS（Kotlin、Swift、C++、C#）。
- **可评测** — 内置 OpenInference 兼容的轨迹日志器，支持成功率、成本、延迟、容错等评测。
- **人机协作** — 高风险工具操作通过原生 UI 对话框审批。
- **动态 Agentic Loop** — 不是固定管线；每轮迭代动态决策（直接回答/工具调用/委托子 Agent/切换范式）。

---

## 架构

```
┌─────────────────────────────────────────────────────────────────────┐
│                        oneai-app (集成层)                            │
│  AppBuilder → App → AppSession                                      │
│  将所有模块组装在一起；应用的入口点                                    │
├──────────┬──────────┬──────────┬──────────┬──────────┬──────────────┤
│ oneai-   │ oneai-   │ oneai-   │ oneai-   │ oneai-   │ oneai-       │
│ agent    │ workflow │ memory   │ tool     │ rag      │ skill        │
│          │          │          │          │          │              │
│ AgentLoop│ Config → │ STM +    │ Registry │ Document │ Selector     │
│ +SubAgent│ DAG +    │ LTM +    │ + MCP +  │ Index +  │ + Registry   │
│ +ReAct   │ StateGrph│ Compress │ Approval │ Embedding│ + 内置领域   │
│ +Plan    │ Compile →│          │ Executor │ Retrieval│   Skills     │
│ +Reflect │ Execute  │          │ +12工具   │          │              │
│ +Stream  │          │          │          │          │              │
│ +CtxAsmbl│          │          │          │          │              │
├──────────┴──────────┴──────────┴──────────┴──────────┴──────────────┤
│                     oneai-domain (领域配置层)                         │
│  DomainPack (5层)、CodingPack、ToolDecorator、ContextSource、        │
│  PermissionProfile、ParadigmStrategy、CompressionTemplate、Merge    │
├──────────────────────────────────────────────────────────────────────┤
│                     oneai-core (基础层)                              │
│  ContentBlock, Message, Conversation, PermissionLevel, Budget,     │
│  ContextBudgetManager, PlatformCapabilities, Traits                  │
├──────────────────────────────┬──────────────────────────────────────┤
│     oneai-provider           │  oneai-parser                        │
│  OpenAI / Anthropic / Ollama │  3层解析防御                          │
├──────────────────────────────┼──────────────────────────────────────┤
│     oneai-persistence        │  oneai-scheduler                     │
│  渐进式Checkpoint +          │  内存任务调度                         │
│  Memory/SQLite/Postgres      │                                      │
├──────────────────────────────┼──────────────────────────────────────┤
│     oneai-trace              │  oneai-uniffi                        │
│  OpenInference 轨迹日志       │  Kotlin / Swift / C++ / C# 绑定     │
├──────────────────────────────┴──────────────────────────────────────┤
│                平台 Crate                                           │
│  oneai-platform-desktop / android / ios / harmony                   │
│  原生审批门 + PlatformCapabilities                                  │
└─────────────────────────────────────────────────────────────────────┘
```

---

## Crate 总览

| Crate | 说明 | 测试数 |
|-------|------|--------|
| `oneai-core` | 核心类型、trait、PermissionLevel、ContextBudgetManager、PlatformCapabilities | 28 |
| `oneai-provider` | LLM Provider（OpenAI、Anthropic、Ollama） | 6 |
| `oneai-parser` | 3层输出解析防御 | 12 |
| `oneai-memory` | 记忆系统（STM、LTM、压缩、HNSW） | 20 |
| `oneai-tool` | 工具注册、MCP、审批门、执行器、12工具 | 32 |
| `oneai-skill` | 技能选择器 + 注册 + 16内置领域技能 | — |
| `oneai-domain` | DomainPack 系统（5层配置）、CodingPack、合并 | 40 |
| `oneai-agent` | AgentLoop + SubAgent + ReAct/Plan/Reflect + StreamParser + ContextAssembler | 15 |
| `oneai-rag` | RAG（含 EmbeddingService：FastEmbed/Ollama/OpenAI） | 20 |
| `oneai-workflow` | Workflow DAG + StateGraph + 执行器 | 26 |
| `oneai-scheduler` | 内存任务调度 | 6 |
| `oneai-persistence` | 渐进式Checkpoint + 后端（Memory/SQLite/Postgres） | 5 |
| `oneai-app` | 应用集成层（AppBuilder） | 7 |
| `oneai-trace` | OpenInference 兼容轨迹日志器 | 14 |
| `oneai-uniffi` | UniFFI 绑定定义 | 20 |
| `oneai-platform-desktop` | 桌面平台（macOS/Windows/Linux） | 2 |
| `oneai-platform-android` | Android 平台 | 2 |
| `oneai-platform-ios` | iOS 平台 | 1 |
| `oneai-platform-harmony` | HarmonyOS 平台 | 1 |
| **总计** | | **257** |

---

## 快速开始

### 构建

```bash
# 克隆仓库
git clone https://github.com/oneai-project/oneai.git
cd oneai

# 构建所有 crate
cargo build

# 运行所有测试
cargo test
```

### 最简示例

```rust
use std::sync::Arc;
use oneai_app::AppBuilder;
use oneai_tool::CalculatorTool;
use oneai_domain::coding_pack;

#[tokio::main]
async fn main() {
    // 构建一个带 Coding DomainPack 的 App（一行领域切换）
    let app = AppBuilder::new()
        .auto_approval_gate()
        .default_parser()
        .domain_pack(coding_pack("/project/dir"))  // ← 领域切换
        .build()
        .expect("App 构建成功");

    // 创建会话并执行
    let session = app.create_session();
    let result = session.execute_tool("calculator", serde_json::json!({"expression": "2+3"})).await.unwrap();
    println!("结果: {}", result.content); // → "5"
}
```

### 完整演示（TUI）

```bash
cargo run -p oneai-cli
```

启动交互式 TUI（ratatui + crossterm），演示完整管线：工具、记忆、RAG、工作流、Checkpoint、轨迹日志、领域配置。

---

## 核心概念

### DomainPack 系统（领域配置包）

DomainPack 是 OneAI 的关键架构创新——它让领域知识变为**声明式、可插拔、可组合**，而非硬编码。

> "Coding Agent 通过 5 层隐式配置嵌入工作流。OneAI 让这 5 层变为声明式、可插拔、可组合。"

DomainPack 封装了 5 层领域专属配置：

| 层级 | 组件 | 作用 |
|------|------|------|
| 1 | **工具 + ToolDecorator** | 领域专属工具集与描述覆写 |
| 2 | **ContextSource** | 领域专属环境感知（含刷新策略） |
| 3 | **PermissionProfile** | 领域专属权限分类（拒绝/自动/确认） |
| 4 | **ParadigmStrategy** | 领域专属任务→范式映射 |
| 5 | **CompressionTemplate** | 领域专属上下文保留优先级 |

一行切换领域：

```rust
let app = AppBuilder::new()
    .provider(provider)
    .domain_pack(coding_pack("/project/dir"))  // ← 一行领域切换
    .build()?;
```

DomainPack 可以**合并**用于多领域 Agent（如 coding + research），权限使用"严格优先"规则合并，上下文源按优先级合并。

#### CodingPack（内置）

第一个具体 DomainPack 实现，参照 Claude Code 的工作流嵌入机制：

- **9 工具**：FileRead、FileEdit、Shell、Grep、Glob、FileList、NotebookEdit、Environment、WebFetch
- **8 工具装饰器**：领域专属描述（如 Shell 描述为"编译/测试/运行脚本"，而非通用命令执行）
- **6 上下文源**：ProjectInstructions、GitStatus、FileTree、ProjectConfig、Date、EnvironmentInfo
- **权限配置**：自动审批读取、确认编辑/Shell、拒绝危险命令（`rm -rf`、`mkfs`）
- **4 范式策略**：重构 → Plan+ReAct+Reflect、修 bug → Plan+ReAct、搜索 → Explore、实现 → ReAct
- **3 子 Agent 类型**：searcher（read+grep+glob）、coder（edit+shell）、reviewer（read+grep）
- **压缩模板**：保留文件路径、进度状态、关键决策

#### ToolDecorator（工具装饰器）

为领域覆写工具描述——ShellTool 在编程领域的描述从通用变为"执行 Shell 命令用于编译、测试和运行脚本"。

#### ContextSource（上下文源）

可插拔的环境感知，独立刷新策略：

| 策略 | 行为 |
|------|------|
| `EveryIteration` | 每轮迭代刷新（git status） |
| `OnChange` | 仅当检测到差异时刷新（文件树） |
| `OnceAtStart` | 启动时加载一次，不再刷新（项目配置） |
| `Periodic(Duration)` | 固定间隔刷新（日期时间） |

#### PermissionProfile（权限配置）

领域级权限覆写，解析优先级：

1. `deny_by_default` → 总是阻止匹配模式（最高优先级）
2. `permission_overrides` → 覆写工具默认 PermissionLevel
3. `auto_approve` → 跳过审批门
4. `require_confirmation` → 总是路由到审批门
5. 回退到工具自身的 `risk_level()`

### Agentic Loop（动态循环）

核心执行引擎是 **动态循环**——而非固定管线。每轮迭代，模型动态决定下一步：

| 决策类型 | 行动 |
|----------|------|
| **DirectAnswer** | 模型给出最终答案 → 循环结束 |
| **ToolCalls** | 模型调用工具 → 执行并回填结果 |
| **Delegate** | 模型委托子任务给专门的子 Agent |
| **SwitchParadigm** | 模型切换范式（Plan/Reflect/Explore） |

迭代上限由 **TokenBudget** 约束（而非硬编码 `max_iterations`），预算不足时循环自动终止。

### 增量流解析器（StreamParser）

流式模式下实时检测 tool_use 块——UI 可以在参数完全生成之前就显示 Agent 的意图。参照 Claude Code 的增量解析方法，取代旧的"先收集完整流再处理"模式。

### 上下文组装器（ContextAssembler）

为每轮迭代构造对话上下文，并自动进行**环境差异检测**——检测迭代间的变化（新文件、git 修改、目录变化）并注入为上下文，即使没有工具显式报告这些变化。

### 子 Agent 系统

分层委托：主 Agent 将复杂子任务委托给专门的子 Agent。DomainPack 通过 `SubAgentTypeDefinition` 定义自定义子 Agent 类型：

```rust
pub enum SubAgentKind { Plan, Explore, Code, Review, Custom(String) }
```

自定义子 Agent（在 DomainPack 中定义）拥有领域专属的系统提示、工具子集和权限阈值。

### 权限分级（PermissionLevel）

替代旧的 `RiskLevel`，三级权限体系：

| 等级 | 范围 | 自动审批？ |
|------|------|------------|
| **Read** | 文件读取、搜索、环境感知 | 是 |
| **Standard** | 文件编辑、MCP 交互 | 视策略而定 |
| **Full** | Shell 执行、文件删除、系统命令 | 需审批 |

### LLM Provider

```rust
#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn infer(&self, req: InferenceRequest) -> Result<InferenceResponse>;
    async fn infer_stream(&self, req: InferenceRequest) -> Result<Pin<Box<dyn Stream<Item = InferenceStreamChunk> + Send>>>;
    fn capabilities(&self) -> ModelCapability;
    fn config(&self) -> &ModelConfig;
}
```

内置三个 Provider：**OpenAI**、**Anthropic** 和 **Ollama**。

### Agent 范式

| 范式 | 模式 | 适用场景 |
|------|------|----------|
| **ReAct** | 推理 → 行动 → 观察 循环 | 通用工具调用任务 |
| **Plan** | 分解 → 有序步骤列表 | 复杂多步任务 |
| **Reflection** | 验证 → 建议修正 | 质量保证、自检 |
| **Parallel** | ScopeState 隔离 → 合并 | 独立子任务 |
| **Explore** | 搜索 → 理解 → 概括 | 代码库/搜索探索 |

所有 Agent 使用 `ScopeState` 实现安全的并行执行。

### 工具系统

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;
    fn risk_level(&self) -> RiskLevel;
    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput>;
}

pub trait PermissionAwareTool: Tool {
    fn permission_level(&self) -> PermissionLevel;
}
```

**内置工具（12）：** ShellTool（安全黑名单+沙箱）、FileReadTool（offset+limit分页）、FileEditTool、FileWriteTool、FileListTool、GrepTool、GlobTool、EnvironmentTool、NotebookEditTool、FileDeleteTool、CalculatorTool、WebFetchTool。

通过 `rmcp` crate 实现 MCP 集成——支持 stdio、SSE、streamable-http 传输协议连接任何 MCP 兼容工具服务器。

**审批门** 控制高风险工具执行：

| 审批门 | 行为 |
|--------|------|
| `BlockingApprovalGate` | 总是拒绝（安全默认） |
| `AutoApprovalGate` | 总是批准（仅用于测试） |
| `ChannelApprovalGate` | 发送到平台 UI 由人审核 |
| `PlatformApprovalGate` | 原生对话框（NSAlert / AlertDialog / UIAlertController） |

### 技能系统（Skill）

渐进式揭示 Agent 能力的技能机制。内置技能按领域组织：

| 领域 | 技能 |
|------|------|
| **编程**（8） | project-planning、code-review、debug-analysis、refactoring、test-strategy、documentation、git-workflow、dependency-analysis |
| **研究**（5） | deep-research、academic-search、data-extraction、citation-management、fact-verification |
| **通用**（3） | summarization、translation、creative-writing |

技能通过 `SkillSelector` 使用触发关键词或 embedding 匹配，激活时注入 Agent 上下文。

### 记忆系统

- **短期记忆** — 可配置大小的滑动窗口，自动驱逐到长期记忆
- **长期记忆** — 嵌入式 HNSW 向量存储 + 内容存储 + 混合评分
- **上下文压缩** — Token 超限时自动摘要，保留近期轮次
- **ContextBudgetManager** — 每轮自动压缩，按比例分配上下文预算

### 3层输出解析器

LLM 输出不可靠，OneAI 通过 3 层防御：

1. **约束解码** — BNF 语法引导模型输出格式
2. **模糊 JSON 修复** — 括号补全、正则提取、嵌入式 JSON 检测
3. **回退自纠** — 重新提示模型修正输出

### 工作流引擎

- **WorkflowDag** — 声明式 DAG，用于并行步骤编排
- **StateGraph** — 有环有向图，用于需要迭代的 Agent 流程（ReAct 循环、条件路由、中断点）

### RAG（检索增强生成）

- **EmbeddingService** — FastEmbed（本地 ONNX）、Ollama 或 OpenAI embedding
- **DocumentIndex** — `add_document()` 时自动生成 embedding
- **分块策略** — SentenceBoundary、FixedSize、Paragraph

### 错误恢复

超越 LLM 自判断的系统化错误恢复：

| 策略 | 说明 |
|------|------|
| **Retry** | 可配置重试策略 |
| **ConditionalFallback** | 错误 → 修正路径 |
| **Rollback** | 从 Checkpoint 回滚状态 |
| **Assertion** | 约束 Hook 拦截 |
| **ExternalFeedback** | 测试结果、编译、API 状态码 |

### 渐进式 Checkpoint

每轮迭代自动保存，支持多种后端：

| 后端 | 适用场景 |
|------|----------|
| **MemoryCheckpointBackend** | 开发/测试 |
| **SqliteCheckpointBackend** | 单设备生产 |
| **PostgresCheckpointBackend** | 服务端生产 |

自动保存策略：EveryStep、EveryNSteps、CriticalNodes。支持中断、回放和从任意检查点 fork。

### 轨迹日志（Trace）

OpenInference 兼容的轨迹日志器，用于 Agent 评测：

```rust
let app = AppBuilder::new()
    .trace_in_memory()  // 或 .trace_to_file("/tmp/trace.json")
    .build()?;

session.end_session(SpanStatus::Ok);
let tree = session.build_trace_tree();
println!("成功率: {:.1}%", tree.metrics.success_rate * 100.0);
```

---

## 跨平台支持

OneAI 使用 UniFFI 生成外语绑定：

| 平台 | 绑定语言 | 审批门 | PlatformCapabilities |
|------|----------|--------|----------------------|
| macOS / Windows / Linux | C++ / C# | NSAlert / MessageBox | 截屏、文件沙箱、通知 |
| Android | Kotlin | AlertDialog | 相机、截屏、网络 |
| iOS | Swift | UIAlertController | 相机（受限）、截屏 |
| HarmonyOS | C++ | CommonDialog | 相机、App沙箱 |

---

## 项目结构

```
oneai/
├── crates/
│   ├── oneai-core/          # 基础：类型、trait、PermissionLevel、Budget、PlatformCapabilities
│   ├── oneai-provider/      # LLM Provider（OpenAI、Anthropic、Ollama）
│   ├── oneai-parser/        # 3层输出解析
│   ├── oneai-memory/        # STM、LTM、压缩、HNSW、MemoryManager
│   ├── oneai-tool/          # 注册、12本地工具、MCP、审批、执行器
│   ├── oneai-skill/         # 技能注册 + 选择器 + 16内置领域技能
│   ├── oneai-domain/        # DomainPack系统（5层）、CodingPack、合并
│   ├── oneai-agent/         # AgentLoop、SubAgent、ReAct、Plan、Reflect、StreamParser、ContextAssembler
│   ├── oneai-rag/           # Document、Index、EmbeddingService、Retrieval
│   ├── oneai-workflow/      # DAG、StateGraph、编译器、验证器、执行器
│   ├── oneai-scheduler/     # InMemoryScheduler
│   ├── oneai-persistence/   # 渐进式Checkpoint、Memory/SQLite/Postgres 后端
│   ├── oneai-app/           # AppBuilder、App、AppSession
│   ├── oneai-trace/         # OpenInference 轨迹日志器
│   ├── oneai-uniffi/        # UniFFI 绑定定义
│   ├── oneai-platform-desktop/
│   ├── oneai-platform-android/
│   ├── oneai-platform-ios/
│   └── oneai-platform-harmony/
├── examples/
│   ├── cli/                 # 交互式 TUI 演示（ratatui + crossterm）
│   ├── desktop-app/         # 桌面审批门演示
│   ├── rust/                # Channel 审批门演示
│   ├── android-app/         # Android 演示（Kotlin）
│   └── ios-app/             # iOS 演示（Swift）
├── bindings/                # 生成的 UniFFI 绑定
├── scripts/                 # 构建和绑定生成脚本
└── Cargo.toml               # Workspace 根配置
```

---

## 开发阶段

| 阶段 | 重点 | 状态 |
|------|------|------|
| 1 | 核心类型、Provider、Parser | ✅ 完成 |
| 2 | Agent 范式（ReAct、Plan、Reflection、Parallel、Explore） | ✅ 完成 |
| 3 | 记忆、工具（MCP + 审批）、RAG 基础 | ✅ 完成 |
| 4 | 工作流（Config + DAG + Executor）、持久化、调度器 | ✅ 完成 |
| 5 | AppBuilder + AppSession、UniFFI 绑定 | ✅ 完成 |
| 6 | 平台 UI + 原生审批门 | ✅ 完成 |
| 7 | 轨迹日志器（OpenInference） | ✅ 完成 |
| 8 | Agentic Loop、SubAgent、StateGraph、Budget、PermissionLevel | ✅ 完成 |
| 9 | 12工具、ShellTool安全、MCP真实实现、EmbeddingService、WebFetchTool | ✅ 完成 |
| 10 | 渐进式Checkpoint、ErrorRecovery、PromptTemplates、PlatformCapabilities | ✅ 完成 |
| 11 | DomainPack系统（5层）、CodingPack、Skill内置、StreamParser、ContextAssembler、TUI | ✅ 完成 |

---

## 许可证

Apache-2.0 — 详情见 [LICENSE](LICENSE)。
