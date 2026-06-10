# OneAI

> 基于 Rust 的跨平台 AI Agent 框架 — 模块化、类型安全、评估就绪。

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE)
[![Crates: 18](https://img.shields.io/badge/Crates-18-orange.svg)]()
[![Tests: 211](https://img.shields.io/badge/Tests-211-green.svg)]()

---

## 什么是 OneAI？

OneAI 是一个使用 Rust 编写的全栈 Agent 框架。它提供了构建、运行和评估 AI Agent 所需的一切 —— 从 LLM Provider 抽象到工具执行、内存管理、工作流编排和轨迹日志，并通过 UniFFI 绑定实现跨平台支持。

**核心原则：**

- **模块化设计** — 18 个独立 crate，职责清晰。按需选用。
- **全链路类型安全** — 封闭枚举层级、trait 驱动抽象，无字符串配置。
- **跨平台** — 通过 UniFFI（Kotlin、Swift、C++、C#）支持 macOS、Windows、Linux、Android、iOS 和 HarmonyOS。
- **评估就绪** — 内置 OpenInference 兼容的轨迹日志，支持 Agent 评估（成功率、成本、延迟、容错性）。
- **人机协作** — 高风险工具操作通过原生 UI 弹窗的审批门控制。

---

## 架构总览

```
┌─────────────────────────────────────────────────────────────────────┐
│                        oneai-app（集成层）                            │
│  AppBuilder → App → AppSession                                       │
│  将所有模块组装在一起；应用程序入口                                     │
├──────────┬──────────┬──────────┬──────────┬──────────┬──────────────┤
│ oneai-   │ oneai-   │ oneai-   │ oneai-   │ oneai-   │ oneai-       │
│ agent    │ workflow │ memory   │ tool     │ rag      │ skill        │
│          │          │          │          │          │              │
│ ReAct    │ Config → │ STM +    │ Registry │ Document │ Selector     │
│ Plan     │ DAG →    │ LTM +    │ + MCP +  │ Index +  │ + Registry   │
│ Reflect  │ Compile →│ Compress │ Approval │ Retrieval│              │
│ Parallel │ Execute  │          │ Executor │          │              │
├──────────┴──────────┴──────────┴──────────┴──────────┴──────────────┤
│                     oneai-core（基础层）                              │
│  ContentBlock, Message, Conversation, ModelConfig, Traits            │
├──────────────────────────────┬──────────────────────────────────────┤
│     oneai-provider           │  oneai-parser                        │
│  OpenAI / Anthropic / Ollama │  三层解析防御                         │
├──────────────────────────────┼──────────────────────────────────────┤
│     oneai-persistence        │  oneai-scheduler                     │
│  文件级检查点持久化            │  内存任务调度                         │
├──────────────────────────────┼──────────────────────────────────────┤
│     oneai-trace              │  oneai-uniffi                        │
│  OpenInference 轨迹日志       │  Kotlin / Swift / C++ / C# 绑定     │
├──────────────────────────────┴──────────────────────────────────────┤
│                平台 Crate                                            │
│  oneai-platform-desktop / android / ios / harmony                    │
│  原生审批门（NSAlert / AlertDialog / UIAlertController）              │
└─────────────────────────────────────────────────────────────────────┘
```

---

## Crate 概览

| Crate | 说明 | 测试数 |
|-------|------|--------|
| `oneai-core` | 核心类型、trait 和抽象 | 28 |
| `oneai-provider` | LLM Provider（OpenAI、Anthropic、Ollama） | — |
| `oneai-parser` | 三层输出解析防御 | 12 |
| `oneai-memory` | 内存系统（STM、LTM、压缩、HNSW） | 20 |
| `oneai-tool` | 工具注册、MCP、审批门、执行器 | 32 |
| `oneai-skill` | Skill 系统，渐进披露 | — |
| `oneai-agent` | Agent 范式（ReAct、Plan、Reflection、Parallel） | 15 |
| `oneai-rag` | 检索增强生成 | 20 |
| `oneai-workflow` | 工作流编译、DAG、验证器、执行器 | 26 |
| `oneai-scheduler` | 内存任务调度 | 6 |
| `oneai-persistence` | 状态持久化和检查点管理 | 5 |
| `oneai-app` | 应用集成层（AppBuilder） | 7 |
| `oneai-trace` | OpenInference 兼容轨迹日志 | 14 |
| `oneai-uniffi` | UniFFI 绑定定义 | 20 |
| `oneai-platform-desktop` | 桌面平台（macOS/Windows/Linux） | 2 |
| `oneai-platform-android` | Android 平台 | 2 |
| `oneai-platform-ios` | iOS 平台 | 1 |
| `oneai-platform-harmony` | HarmonyOS 平台 | 1 |
| **合计** | | **211** |

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

#[tokio::main]
async fn main() {
    // 构建应用（自动审批模式，用于测试）
    let app = AppBuilder::new()
        .auto_approval_gate()
        .default_parser()
        .build()
        .expect("构建应用");

    // 注册工具
    app.register_tool(Arc::new(CalculatorTool::new())).await.unwrap();

    // 创建会话并执行
    let session = app.create_session();
    let result = session.execute_tool("calculator", serde_json::json!({"expression": "2+3"})).await.unwrap();
    println!("结果: {}", result.content); // → "5"
}
```

### 完整演示

```bash
cargo run -p oneai-cli-demo
```

演示完整流水线：工具、内存、RAG、工作流、检查点和轨迹日志。

---

## 核心概念

### LLM Provider

OneAI 通过 `LlmProvider` trait 抽象 LLM 推理：

```rust
#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn infer(&self, req: InferenceRequest) -> Result<InferenceResponse>;
    async fn infer_stream(&self, req: InferenceRequest) -> Result<Pin<Box<dyn Stream<Item = InferenceStreamChunk> + Send>>>;
    fn capabilities(&self) -> ModelCapability;
    fn config(&self) -> &ModelConfig;
}
```

内置三个 Provider：

- **OpenAI** — GPT-4、GPT-3.5 及所有 OpenAI 兼容 API
- **Anthropic** — Claude 模型，支持流式推理
- **Ollama** — 通过 Ollama 运行本地模型

### Agent 范式

| 范式 | 模式 | 适用场景 |
|------|------|----------|
| **ReAct** | 思考 → 行动 → 观察循环 | 通用工具调用任务 |
| **Plan** | 分解 → 排序步骤列表 | 复杂多步任务 |
| **Reflection** | 验证 → 建议修正 | 质量保证、自我检查 |
| **Parallel** | ScopeState 隔离 → 合并 | 独立子任务并行执行 |

所有 Agent 使用 `ScopeState` 实现安全的并行执行 —— 本地沙箱仅通过显式 `Reduction` 操作将结果合并回全局状态。

### 工具系统

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;
    fn risk_level(&self) -> RiskLevel;    // Low, Medium, High
    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput>;
}
```

内置工具：`CalculatorTool`、`ShellTool`、`FileReadTool`、`FileWriteTool`。

通过 `rmcp` crate 集成 MCP —— 可连接任何 MCP 兼容的工具服务器。

**审批门** 控制高风险工具的执行：

| 审批门 | 行为 |
|--------|------|
| `BlockingApprovalGate` | 总是拒绝（安全默认值） |
| `AutoApprovalGate` | 总是批准（仅用于测试） |
| `ChannelApprovalGate` | 发送到平台 UI 供人工审核 |
| `PlatformApprovalGate` | 原生弹窗（NSAlert / AlertDialog / UIAlertController） |

### 内存系统

- **短期记忆 (STM)** — 滑动窗口，可配置大小，自动溢出到长期记忆
- **长期记忆 (LTM)** — 内嵌 HNSW 向量存储 + 内容存储 + 混合评分（语义相似度 × 时间近度）
- **上下文压缩** — 当 token 数超阈值时自动摘要，保留最近轮次
- **MemoryManager** — 统一接口，协调 STM ↔ LTM ↔ 压缩

### 三层输出解析器

LLM 输出的格式可靠性是常见问题。OneAI 通过三层防御解决：

1. **约束解码** — BNF 语法引导模型输出格式
2. **模糊 JSON 修复** — 括号闭合、正则提取、内嵌 JSON 检测
3. **回退自我修正** — 重新提示模型修正自身输出

```rust
let parser = ThreeLayerParser::new();
let result: ParsingResult = parser.parse(raw_llm_output).await?;
```

### 工作流引擎

将工作流定义为声明式配置 → 编译为 DAG → 按层级执行，自动并行独立步骤：

```rust
let config = WorkflowConfig::new("data_pipeline", vec![
    StepConfig { id: "fetch", depends_on: vec![], tool: Some("http_get"), .. },
    StepConfig { id: "parse", depends_on: vec!["fetch"], tool: Some("json_parser"), .. },
    StepConfig { id: "store", depends_on: vec!["parse"], tool: Some("db_write"), .. },
]);

let result = session.execute_workflow(&config).await?;
```

功能：超时策略、重试策略、审批检查点、失败继续模式。

### RAG（检索增强生成）

```rust
let mut index = DocumentIndex::with_defaults(vector_store);
let mut doc = Document::with_id("guide", "Rust 是一门系统编程语言...");
doc.chunk(&ChunkingStrategy::SentenceBoundary { max_chunk_size: 200 });
index.add_document(doc)?;

let results = index.search_by_keyword("系统编程语言", 5);
```

分块策略：`SentenceBoundary`（句子边界）、`FixedSize`（固定大小）、`Paragraph`（段落）。

### 轨迹日志 (Trace)

OpenInference 兼容的轨迹日志，用于 Agent 评估：

```rust
let app = AppBuilder::new()
    .trace_in_memory()  // 或 .trace_to_file("/tmp/trace.json")
    .build()?;

// ... 运行 Agent 会话 ...

session.end_session(SpanStatus::Ok);
let tree = session.build_trace_tree();
println!("成功率: {:.1}%", tree.metrics.success_rate * 100.0);
println!("工具调用次数: {}", tree.metrics.tool_call_count);
println!("估算成本: ${:.4}", tree.metrics.estimated_cost_usd);
```

**追踪指标：** success_rate、total_tokens、estimated_cost_usd、avg_inference_latency_ms、tool_call_count、tool_success_rate、approval_denial_rate、parser_fallback_rate、total_retries、workflow_step_success_rate、avg_iterations、checkpoint_count、error_count。

**条件编译：** 禁用 `trace` feature 后，所有 trace 类型变为零开销 stub，编译时完全消除。

---

## 跨平台支持

OneAI 使用 UniFFI 从 Rust 类型生成外语绑定：

| 平台 | 绑定语言 | 审批门 |
|------|----------|--------|
| macOS / Windows / Linux | C++ / C# | NSAlert / MessageBox |
| Android | Kotlin | AlertDialog |
| iOS | Swift | UIAlertController |
| HarmonyOS | C++ | CommonDialog |

```bash
# 生成绑定
./scripts/generate_bindings.sh
```

`ProviderFactory` 和 `AppBuilder` 作为 UniFFI Object 导出 —— 外语代码通过工厂方法创建具体实例，无需 trait object。

---

## 持久化

基于文件的检查点管理，支持 Agent 状态恢复：

```rust
let persistence = Arc::new(FilePersistence::new("/tmp/checkpoints"));
let app = AppBuilder::new()
    .persistence(persistence)
    .build()?;

let checkpoint_id = session.save_checkpoint().await?;
// 后续：从检查点加载，恢复长时间运行的 Agent
```

---

## 项目结构

```
oneai/
├── crates/
│   ├── oneai-core/          # 基础层：类型、trait、错误、平台
│   ├── oneai-provider/      # LLM Provider（OpenAI、Anthropic、Ollama）
│   ├── oneai-parser/        # 三层解析防御
│   ├── oneai-memory/        # STM、LTM、压缩、HNSW、MemoryManager
│   ├── oneai-tool/          # 注册、本地/MCP 工具、审批、执行器
│   ├── oneai-skill/         # Skill 注册 + 选择器
│   ├── oneai-agent/         # ReAct、Plan、Reflection、Parallel、AgentRunner
│   ├── oneai-rag/           # 文档、索引、检索
│   ├── oneai-workflow/      # 配置、DAG、编译器、验证器、执行器
│   ├── oneai-scheduler/     # 内存调度器
│   ├── oneai-persistence/   # 文件持久化、检查点、状态
│   ├── oneai-app/           # AppBuilder、App、AppSession
│   ├── oneai-trace/         # OpenInference 轨迹日志
│   ├── oneai-uniffi/        # UniFFI 绑定定义
│   ├── oneai-platform-desktop/
│   ├── oneai-platform-android/
│   ├── oneai-platform-ios/
│   └── oneai-platform-harmony/
├── examples/
│   ├── cli/                 # 交互式 REPL 演示
│   ├── desktop-app/         # 桌面审批门演示
│   ├── rust/                # Channel 审批门演示
│   ├── android-app/         # Android 应用演示
│   └── ios-app/             # iOS 应用演示
├── bindings/                # 生成的 UniFFI 绑定（Kotlin、Swift、C++、C#）
├── scripts/                 # 构建和绑定生成脚本
├── tests/                   # 集成测试
└── Cargo.toml               # Workspace 根配置
```

---

## 开发阶段

| 阶段 | 重点 | 状态 |
|------|------|------|
| 1 | 核心类型、Provider、Parser | ✅ 完成 |
| 2 | Agent 范式（ReAct、Plan、Reflection、Parallel） | ✅ 完成 |
| 3 | Memory、Tools（MCP + 审批门）、RAG 基础 | ✅ 完成 |
| 4 | Workflow（Config + DAG + Executor）、Persistence、Scheduler | ✅ 完成 |
| 5 | AppBuilder + AppSession、UniFFI 绑定、168 测试 | ✅ 完成 |
| 6 | 平台 UI + 原生审批门 | ✅ 完成 |
| 7 | 轨迹日志（OpenInference）、211 测试 | ✅ 完成 |

---

## 许可证

Apache-2.0 — 详见 [LICENSE](LICENSE)。