# OneAI

[English](README_EN.md) | **简体中文**

> **One AI, Every Platform** — 跨平台 AI Agent 框架，基于 Rust 构建：模块化、类型安全、领域可插拔、可评测、多 Agent 原生，一套 Rust 内核打到六端。

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE)
[![Crates: 24](https://img.shields.io/badge/Crates-24-orange.svg)]()
[![Tests: 1378](https://img.shields.io/badge/Tests-1378-green.svg)]()
[![Version: 0.2.0](https://img.shields.io/badge/Version-0.2.0-blue.svg)]()
[![Rust: 1.74+](https://img.shields.io/badge/Rust-edition%202021-dea584.svg)]()
[![Platforms: 6](https://img.shields.io/badge/Platforms-macOS%20%7C%20Win%20%7C%20Linux%20%7C%20Android%20%7C%20iOS%20%7C%20HarmonyOS-blue.svg)]()

<p align="center">
  <img src="OneAI_icon.png" alt="OneAI" width="160">
</p>

<p align="center"><em>同一套 Rust 内核（<code>oneai-core</code>），通过 UniFFI 与手写 <code>extern "C"</code> facade，驱动 macOS / Windows / Linux / Android / iOS / HarmonyOS 六端原生 App。</em></p>

---

## 一图看懂

<p align="center">
  <img src="interview-opening-example.png" alt="macOS App — 场景化入口" width="780">
</p>

<p align="center"><em>macOS 原生 App ·「面试演练」场景入口 —— 启动前内联填写应聘岗位 / 目标公司 / 项目经历等背景字段，按<strong>成员可见性</strong>注入各自的系统提示（面试官看不到项目经历，指导员据此给项目级建议）。</em></p>

<p align="center">
  <img src="interview-chat-example.png" alt="macOS App — 多 Agent 群聊" width="780">
</p>

<p align="center"><em>macOS 原生 App · 多 Agent 群聊 —— 面试官（蓝，提问）与指导员（绿，点评）按脚本轮次发言：用户作答 → 指导员点评 → 面试官追问，流式逐字渲染 + 思考气泡。</em></p>

<p align="center">
  <img src="new-scenario-example.png" alt="macOS App — 可视化场景编辑器" width="780">
</p>

<p align="center"><em>macOS 原生 App · 可视化场景编辑器 —— 拖拽式编排角色阵容、背景字段、轮次策略（脚本/轮询/主持人）、开场白与复盘阶段，持久化到 <code>~/Library/Application Support</code>。</em></p>

<p align="center">
  <img src="oneai-tui-screenshot.jpg" alt="OneAI CLI — Plan 模式下执行复杂任务" width="880">
</p>

<p align="center"><em>交互式 CLI（<code>oneai-cli</code>）· Plan 模式下执行复杂任务 —— 思考气泡、计划清单面板、工具调用展示，以及 accept/reject 审批弹窗。</em></p>

**同一个引擎，两种前端**：左上的原生 App 是给「场景化多 Agent 对话」（面试陪练 / 语言伙伴 / 辩论 / 写作工坊 / 头脑风暴）准备的；下方的 CLI TUI 是给「通用 Agentic 编程 / 任务执行」准备的。两者背后是同一套 Rust 内核与同一个 `AgentLoop`。

---

## 快速上手（CLI，最快 30 秒）

### 1. 配置 Provider

OneAI 兼容任何 **OpenAI 兼容端点**（OpenAI、Anthropic、Gemini、Ollama，以及阿里百炼/DashScope、DeepSeek、vLLM 等自建网关）。通过环境变量或配置文件设置凭据——环境变量优先级更高。

```bash
# OpenAI 兼容端点 —— 适用于 OpenAI / DashScope / DeepSeek 等
export ONEAI_API_KEY="sk-..."
export ONEAI_BASE_URL="https://api.openai.com/v1"   # 或你的网关地址
export ONEAI_MODEL="gpt-4o"                          # 或 qwen-plus、deepseek-chat ...

# Ollama（本地，无需 key）
export ONEAI_BASE_URL="http://localhost:11434"
export ONEAI_MODEL="llama3"
```

…或写入 `~/.oneai/config.toml`：

```toml
[provider]
api_key = "sk-..."
base_url = "https://api.openai.com/v1"
model = "gpt-4o"

[domain]
default_pack = "coding"   # coding | research | general

[ui]
theme = "dark"
```

用 `oneai config create` 生成默认配置，`oneai config show` 查看。

### 2. 启动 TUI

```bash
cargo run -p oneai-cli-demo
# 或执行 cargo install --path examples/cli 后直接：oneai
```

进入交互式 Agent。输入任务即可看到完整管线实时运行：流式思考气泡、工具调用、计划清单、用量/Token 统计、轨迹日志。

**交互模式 —— 用 `Shift+Tab` 循环切换：**

| 模式 | 行为 |
|------|------|
| `Normal` | 默认 —— 高风险工具暂停等待审批 |
| `⚡ Auto` | 全部自动批准（快速迭代） |
| `📋 Plan` | 禁用工具执行 —— Agent 必须先给出计划；你在 accept/reject 弹窗中审阅后才开始执行 |

**按键：**

| 按键 | 动作 |
|------|------|
| `Enter` | 发送 · `Ctrl+Enter` 换行 |
| `Shift+Tab` | 循环模式（Normal → Auto → Plan） |
| `Tab` | 切换侧边栏 |
| `↑↓` / `Ctrl+↑↓` / `PgUp` / `PgDn` | 历史与聊天滚动 |
| 鼠标拖拽 | 选中文本复制 · 滚轮滚动 |
| `Esc` | Vim 模式 / 退出 |

**对话内斜杠命令**（输入 `/help` 查看完整列表）：`/skills` `/skill` `/tools` `/usage` `/context` `/session` `/domain` `/compact` `/wf` `/new` `/init` `/clear` `/quit`。

### 3. 非交互单次推理

```bash
oneai run "把 auth 模块重构为 async" --domain coding --model gpt-4o
```

### 4. 通过 CLI 体验各子系统

OneAI 把每个子系统都暴露为 CLI 子命令，无需写代码即可驱动：

```bash
oneai pack list                      # 浏览 DomainPack
oneai eval run coding-basic          # 运行评测套件
oneai eval swebench --dataset ./swe_bench_lite.jsonl --instances astropy__astropy-12907  # SWE-bench 三轴评测
oneai studio                         # 启动 Web UI（StateGraph 可视化 + Checkpoint 时间旅行）
oneai mcp serve                      # 作为 MCP 服务器运行（兼容 Claude Code/Cursor）
oneai provider status                # Provider 池健康与降级日志
oneai route                          # 查看 SmartRouter 最近的路由决策
oneai usage report                   # Token 用量报告（prompt/completion/total/calls，纯 token 维度）
oneai token --prompt "..."           # 统计 Token、检查上下文窗口是否装得下
oneai team run code-review           # 多 Agent 团队协作
oneai swarm run --task "..."         # 群体编排
oneai session list / resume <id>     # 持久化会话（SQLite）
oneai wasm list / run <name>         # WASM 沙箱模块
oneai embed generate "text"          # 生成向量 embedding
oneai a2a serve                       # 通过 A2A 协议暴露 Agent
oneai init [--format oneai|agents|claude] [--force] [--no-llm]  # 生成项目指令文件（已配置 LLM 时由模型综合生成，否则启发式）
```

### 5. 最简 Rust 程序

```rust
use oneai_app::AppBuilder;
use oneai_domain::coding_pack;

#[tokio::main]
async fn main() {
    let app = AppBuilder::new()
        .noop_interaction_gate()
        .default_parser()
        .domain_pack(coding_pack("/project/dir"))  // ← 一行领域切换
        .build()
        .expect("App 构建成功");

    let session = app.create_session();
    let result = session
        .execute_tool("calculator", serde_json::json!({"expression": "2+3"}))
        .await
        .unwrap();
    println!("结果: {}", result.content); // → "5"
}
```

> **给 AI Agent 读者的提示**：本项目是 `cargo` workspace，集成入口是 `crates/oneai-app/src/builder.rs` 的 `AppBuilder`——每个子系统都是可选的、通过 builder 方法插装。要理解架构，读 [CLAUDE.md](CLAUDE.md)（crate 分层、DomainPack 7 层、AgentLoop 决策、权限模型）。要程序化驱动，从上面的「最简 Rust 程序」与 `examples/cli` 开始。

---

## OneAI 是什么？

OneAI 是一个用 Rust 编写的全栈 Agent 框架。它提供了构建、运行和评测 AI Agent 所需的一切——从 LLM Provider 抽象到工具执行、记忆管理、工作流编排、领域专属配置、多 Agent 协作和轨迹日志——全部支持通过 UniFFI bindings 实现跨平台。**LLM Provider 是可选的**——纯工具或纯工作流的使用无需 Provider。

**核心原则：**

- **模块化设计** — 24 个独立 crate，各司其职，按需使用。
- **类型安全** — 密封枚举层级（每个公开枚举都加了 `#[non_exhaustive]`）、trait 驱动抽象，无字符串配置。
- **领域可插拔** — DomainPack 系统让领域知识声明式、可组合、一行切换；可对照 JSON Schema 校验，并通过 pack 市场共享。
- **多 Agent 原生** — SubAgent、Team 协作（Coordinate/Route/Collaborate/Debate）、Handoff 协议、Swarm 群体编排（能力驱动路由）；以及引擎级 **GroupChat 原语**驱动场景化多角色对话。
- **生产级基础设施** — ProviderPool 降级链、SmartRouter 多因子路由、用量统计、限流、熔断、Token 感知的上下文管理。
- **跨平台** — 通过 UniFFI + 手写 `extern "C"` facade 支持 macOS、Windows、Linux、Android、iOS 和 HarmonyOS（Kotlin、Swift、C++、C#、ArkTS），同一套 Rust 内核。
- **可评测** — 内置 OpenInference 兼容轨迹日志器 + 独立评测框架（6 指标、3 套件 + SWE-bench 三轴）。
- **人机协作** — 高风险工具通过原生 UI 对话框审批；执行前的 Plan 模式审批门。
- **动态 Agentic Loop** — 不是固定管线；每轮迭代动态决策（直接回答/工具调用/委托子 Agent/切换范式）。

---

## 两种使用方式

OneAI 暴露两个面向用户的前端，背后是同一套内核。

### A. CLI / TUI —— 通用 Agentic 执行

面向编程、任务执行、子系统探索。`examples/cli`（bin `oneai-cli`）是基于 ratatui+crossterm 的交互式 TUI，跑完整管线：流式思考气泡、工具调用展示、Plan 模式审批门、用量/Token 统计、轨迹日志。每个子系统都暴露为 clap 子命令（见上方「体验各子系统」）。

### B. 原生桌面 / 移动 App —— 场景化多 Agent 对话

面向「多角色协作对话」场景。以 macOS App 为参考实现（其他端镜像同一设计），核心是 **GroupChat 原语 + 场景系统**：

- **场景（Scenario）= 角色阵容 + 轮次策略 + 背景字段 + 复盘阶段**。一个 Agent 是一个 AI 人格（用户是隐式额外参与者）。
- **轮次策略**：`scripted`（脚本式，固定顺序）、`roundRobin`（轮询）、`moderator`（主持人选下一位）。
- **背景字段 + 成员可见性**：启动前内联表单收集背景（如「应聘岗位」「项目经历」），按 `visibleTo` 注入各自系统提示——例如面试官看不到候选人的项目经历，但指导员能据此给项目级建议。
- **复盘阶段（Debrief）**：如面试结束按钮一键切换到指导员全场总结 + 后续 Q&A，其他成员退场。
- **审改循环（ReviewLoop）**：如写作工坊的「写手起草 → 编辑审阅 → 写手修改 → 编辑复审」，编辑发出 `定稿` 标记或达 `maxRounds` 即止。
- **内置 5 预设**：`面试演练` / `语言伙伴` / `辩论赛` / `写作工坊` / `头脑风暴`，开箱即用、可编辑、可自建。
- 流式 20fps 合并渲染、语音输入（Speech 框架）、Markdown 渲染、暗色跟随系统、命令面板（`⌘K`）、产物画布。

各端的编译运行见下文 [跨平台：桌面与移动端](#跨平台桌面与移动端)。

---

## 架构

```
┌─────────────────────────────────────────────────────────────────────┐
│                        oneai-app（集成层）                           │
│  AppBuilder → App → AppSession（唯一的组装入口）                      │
├──────────┬──────────┬──────────┬──────────┬──────────┬──────────────┤
│ oneai-   │ oneai-   │ oneai-   │ oneai-   │ oneai-   │ oneai-       │
│ agent    │ workflow │ memory   │ tool     │ rag      │ skill        │
│ AgentLoop│ DAG +    │ STM +    │ Registry │ Document │ Selector     │
│ +SubAgent│ StateGrph│ LTM +    │ + MCP +  │ Index +  │ + Registry   │
│ +ReAct   │ 编译→执行│ Compress │ Interact │ Embedding│ + Skills     │
│ +Plan    │          │ +SQLite  │ +12工具   │ Retrieval│              │
│ +Reflect │          │ 持久化    │          │          │              │
├──────────┴──────────┴──────────┴──────────┴──────────┴──────────────┤
│ oneai-domain（7 层 DomainPack + 市场 + 规范校验器）                     │
│ oneai-a2a   oneai-wasm   oneai-eval   oneai-studio   oneai-mcp        │
│ A2A SDK     Wasmtime     评测套件      Web UI        MCP 服务/宿主      │
├──────────────────────────────────────────────────────────────────────┤
│ oneai-provider：OpenAI/Anthropic/Gemini/Ollama + ProviderPool +     │
│                 SmartRouter + 429 重试                                │
│ oneai-parser（3 层）· oneai-persistence · oneai-trace · oneai-       │
│   scheduler · oneai-uniffi · oneai-platform-{desktop,android,ios,  │
│   harmony}                                                          │
├──────────────────────────────────────────────────────────────────────┤
│                     oneai-core（基础层）                             │
│  ContentBlock, Message, Conversation, PermissionLevel, Budget,       │
│  ContextBudgetManager, PlatformCapabilities, ModelContextResolver,   │
│  全部核心 trait (LlmProvider, Tool, InteractionGate, EmbeddingService,│
│   UsageTracker, RateLimiter, CircuitBreaker, TokenCounter)           │
└─────────────────────────────────────────────────────────────────────┘
```

---

## Crate 总览

| Crate | 说明 | 测试数 |
|-------|------|--------|
| `oneai-core` | 核心类型、trait、PermissionLevel、Budget、PlatformCapabilities | 259 |
| `oneai-provider` | LLM Provider（OpenAI/Anthropic/Gemini/Ollama）+ ProviderPool + SmartRouter | 95 |
| `oneai-parser` | 3 层输出解析防御 | 12 |
| `oneai-memory` | 记忆系统（STM、LTM、压缩、HNSW、MemoryManager + 持久化） | 59 |
| `oneai-tool` | 工具注册、MCP 客户端、InteractionGate、执行器、12 工具 | 56 |
| `oneai-skill` | 技能选择器 + 注册 + 内置领域技能 | 8 |
| `oneai-domain` | DomainPack 系统（7 层）、CodingPack、市场、规范校验器 | 127 |
| `oneai-agent` | AgentLoop + SubAgent + ReAct/Plan/Reflect + StreamParser + ContextAssembler + Team/Handoff/Swarm + GroupChat | 194 |
| `oneai-rag` | RAG + EmbeddingService（OpenAI/Anthropic/Voyage/Ollama/FastEmbed） | 61 |
| `oneai-workflow` | Workflow DAG + StateGraph + 编译器 + 执行器 | 44 |
| `oneai-scheduler` | 内存任务调度 | 6 |
| `oneai-persistence` | 渐进式 Checkpoint + SQLite（会话/用量）后端 | 40 |
| `oneai-a2a` | A2A 协议 SDK — 客户端 + 服务端宿主 + DomainPack→AgentCard | 88 |
| `oneai-wasm` | WASM 沙箱引擎 — Wasmtime + WasmTool + 模块注册 | 95 |
| `oneai-eval` | 评测框架 — 用例/指标/Runner/3 套件 + SWE-bench 三轴 | 86 |
| `oneai-studio` | Studio Web UI — axum HTTP+WS + D3.js StateGraph 可视化 + Checkpoint 时间旅行 | 34 |
| `oneai-mcp` | MCP 服务生态 — 宿主 + 插件注册 + 配置 | 57 |
| `oneai-app` | 应用集成层（AppBuilder） | 19 |
| `oneai-trace` | OpenInference 兼容轨迹日志器 | 14 |
| `oneai-uniffi` | UniFFI 绑定定义 + 手写 `extern "C"` facade（C#/Windows、C++/HarmonyOS 复用） | 18 |
| `oneai-platform-desktop` | 桌面平台（macOS/Windows/Linux） | 2 |
| `oneai-platform-android` | Android 平台 | 2 |
| `oneai-platform-ios` | iOS 平台 | 1 |
| `oneai-platform-harmony` | HarmonyOS 平台 | 1 |
| **总计** | | **1378** |

> 另有 `oneai-staticlib`（crate-type=staticlib 的打包 crate，仅 Apple/Windows 构建脚本构建，排除在 `default-members` 之外，故不计入 24）。

---

## 核心概念

### DomainPack 系统（领域配置包）

DomainPack 是 OneAI 的关键架构创新——它让领域知识变为**声明式、可插拔、可组合**，而非硬编码。一个 DomainPack 封装 7 层领域专属配置：

| 层级 | 组件 | 作用 |
|------|------|------|
| 1 | **工具 + ToolDecorator** | 领域专属工具集与描述覆写 |
| 2 | **ContextSource** | 领域专属环境感知（含刷新策略） |
| 3 | **PermissionProfile** | 领域专属权限分类（拒绝/自动/确认） |
| 4 | **ParadigmStrategy** | 领域专属任务→范式映射 |
| 5 | **CompressionTemplate** | 领域专属上下文保留优先级 |
| 6 | **Workflow + StateGraph** | 领域预定义工作流与循环图 |
| 7 | **MemoryProfile** | 领域专属记忆策略(抽取 schema/召回/core 预算/自管理工具/跨会话习惯) |

```rust
let app = AppBuilder::new()
    .provider(provider)
    .domain_pack(coding_pack("/project/dir"))  // ← 一行领域切换
    .build()?;
```

DomainPack 可**合并**用于多领域 Agent（coding + research）——权限"严格优先"、上下文源按优先级合并。Pack 可对照 JSON Schema（`DomainPackSpec`）做结构 + 语义**校验**，可从路径或 git URL **安装**，并通过**市场**（`PackSource` + `PackRegistry` + 内置索引）共享。

```bash
oneai pack list                  # 浏览内置 pack
oneai pack validate spec.toml   # 对照规范校验
oneai pack install ./my-pack     # 从本地路径安装
```

#### CodingPack（内置）

参照 Claude Code 的工作流嵌入机制：9 个工具（FileRead、FileEdit、Shell、Grep、Glob、FileList、NotebookEdit、Environment、WebFetch）、8 个工具装饰器、6 个带刷新策略的上下文源、权限配置（自动审批读取、确认编辑/Shell、拒绝 `rm -rf`/`mkfs`）、4 个范式策略、3 个子 Agent 类型（searcher / coder / reviewer）。

### Agentic Loop（动态循环）

核心执行引擎是 **动态循环**——而非固定管线。每轮迭代模型动态决定下一步：

| 决策类型 | 行动 |
|----------|------|
| **DirectAnswer** | 模型给出最终答案 → 循环结束 |
| **ToolCalls** | 模型调用工具 → 执行并回填结果 |
| **Delegate** | 模型委托子任务给专门的子 Agent |
| **SwitchParadigm** | 模型切换范式（Plan/Reflect/Explore）——会改 system prompt + 工具过滤 |

迭代上限由 **TokenBudget** 约束（而非硬编码 `max_iterations`）。`delegate` / `switch_paradigm` 由 `meta_tool.rs` 注入为模型可调用的 meta-tool——模型即可主动委托子 Agent 或切换范式，`apply_paradigm_switch` + `AgentLoopGraphActionExecutor` 内联升级范式（system prompt + 工具过滤）。内置生命周期钩子（`PreToolUse`/`PostToolUse` 等）、中断/恢复（`CancellationToken`）、结构化输出。

### Agent 范式

| 范式 | 模式 | 适用场景 |
|------|------|----------|
| **ReAct** | 推理 → 行动 → 观察 循环 | 通用工具调用任务 |
| **Plan** | 分解 → 有序步骤列表 | 复杂多步任务 |
| **Reflection** | 验证 → 建议修正 | 质量保证、自检 |
| **Parallel** | ScopeState 隔离 → 合并 | 独立子任务 |
| **Explore** | 搜索 → 理解 → 概括 | 代码库/搜索探索 |

范式是**模型/工作流驱动**的——模型调用 `switch_paradigm`，或 StateGraph 节点发出 `GraphDecision::SwitchParadigm`，`apply_paradigm_switch` 随即改变 system prompt + 决策提示 + 工具过滤。用户侧的执行策略则是独立的 **InteractionMode**（Normal/Auto/Plan，`Shift+Tab` 切换）。

### 权限模型

三级权限：`Read`（自动审批）、`Standard`（视策略而定）、`Full`（需审批）。解析顺序：`deny_by_default` → `permission_overrides` → `auto_approve` → `require_confirmation` → 工具自身 `risk_level()`。人机交互由统一的 **`InteractionGate`** 把守 5 个决策点：`PreInfer`（推理前可改写请求/跳过）、`PostInfer`（推理后可校验/替换）、`ToolApproval`（高风险工具放行，对接原生对话框）、`PlanDecision`（规划权衡选择）、`PlanReview`（最终计划 accept/reject/Revise）。内置实现：`NoopInteractionGate`（全点零延迟放行，等价自动批准）、`ChannelInteractionGate`（mpsc+oneshot 桥到 UI 线程，按点可配）、`ThresholdInteractionGate`（低风险自动放行、其余走通道）、`DenyAllInteractionGate`（全拒）。平台侧 `PlatformInteractionGate` 在 macOS/Windows/Linux/Android/iOS/HarmonyOS 用原生 NSAlert/MessageBox/AlertDialog/UIController/CommonDialog 处理 `ToolApproval`。旧的 `ApprovalGate` / `on_plan_submitted` 已移除。

### LLM Provider 与路由

内置 Provider：**OpenAI、Anthropic、Gemini、Ollama**，统一在 `LlmProvider` trait（`infer` + `infer_stream`）之下。其上是两个生产级层：

- **ProviderPool** — Provider 降级链，每个 Provider 自带熔断器、限流器和降级规则（如 Anthropic→OpenAI→本地）。自动处理 429/重试，解析 `Retry-After`。
- **SmartRouter** — 多因子路由（延迟/质量/均衡/自定义），给 Provider 打分后挑最优，集成熔断/限流/上下文约束。每次决策都记录日志可供查看。

```rust
let app = AppBuilder::new()
    .default_provider_pool_anthropic()   // Anthropic → OpenAI → Ollama 降级
    .default_smart_router_balanced()     // 多因子路由
    .build()?;
```

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
pub trait PermissionAwareTool: Tool { fn permission_level(&self) -> PermissionLevel; }
```

**内置 12 工具：** ShellTool（安全黑名单+沙箱）、FileReadTool（offset+limit 分页）、FileEditTool、FileWriteTool、FileListTool、GrepTool、GlobTool、EnvironmentTool、NotebookEditTool、FileDeleteTool、CalculatorTool、WebFetchTool。MCP 客户端通过 `rmcp` 集成（stdio/SSE/streamable-http）；**MCP 服务端**模式让 OneAI 自身向 Claude Code/Cursor 暴露工具（`oneai mcp serve`）。

### 多 Agent 协作

| 模式 | 机制 |
|------|------|
| **GroupChat（场景）** | 引擎级 `GroupChatSession` 原语 + 场景系统（角色阵容/轮次策略/背景字段/复盘/审改循环），驱动 macOS/Windows 等端的多角色对话 |
| **SubAgent** | 分层委托给专门的子 Agent（Plan/Explore/Code/Review/Custom），可选 worktree 隔离 |
| **Team** | `TeamCoordinator` 4 策略——Coordinate/Route/Collaborate/Debate——加 4 预设（`code_review`/`research_route`/`dev_pipeline`/`arch_debate`） |
| **Handoff** | `HandoffTool`（handoff-as-tool-call）+ `HandoffManager` + 3 预设 |
| **Swarm** | 动态 Agent 池，3 路由策略（BestFit/LoadBalanced/Fastest），任务分解 + 质量校验 + 重试 |

### 记忆系统

- **三层记忆（Letta 式）** — recall log(`Conversation`)/core(常驻、有 token 预算、agent 自管理)/archival(全量事实向量库,按需召回)。`Conversation` 是唯一原始日志,core 只存策展过的原子事实,不再冗余副本。
- **MemoryProfile(DomainPack 第 7 层)** — 声明领域级「抽取 schema(记什么)+ 召回策略 + core 预算 + 是否暴露自管理工具 + 习惯事实类型(跨会话)」,与 `CompressionTemplate`/`ContextSource` 同级可合并。`CodingPack`/`ResearchPack` 内置默认 profile。
- **压缩→归档增量抽取** — `ContextCompressor` 丢弃旧轮次前,按领域 schema 用 `FactExtractor` 抽取原子事实,经 `MemoryFactStore` 的 Mem0 式冲突更新(同 subject+predicate 更新而非追加)归档,堵住"压缩即丢失"。
- **抗压缩注入** — `CoreMemorySource`(实现 `ContextSource`,`EveryIteration`)每轮注入 core 块 + 召回上下文,压缩后自动重注入。
- **自管理记忆工具(领域 opt-in)** — `memory_search` / `core_memory_edit` / `archival_memory_insert`,让 agent 主动策展记忆 → "越用越好用"。
- **双命名空间 + 持久化** — `user_id`(跨会话习惯)+ `session_id`(本会话 episodic);统一 `memories` 表持久化,`oneai memory search/list --user`。`--user` 标志命名空间化跨会话记忆。
- **短期记忆** — 滑动窗口,自动驱逐到长期记忆。
- **长期记忆** — HNSW 向量存储 + 内容存储 + 混合评分;通过配置的 `EmbeddingService` **自动 embedding**。
- **STM↔LTM 闭环** — `MemoryReflection` + `inject_ltm_context` + `RecallStrategy`。
- **上下文压缩** — Token 超限自动摘要,保留近期轮次;`ContextBudgetManager` 按比例分配每轮预算。
- **持久化** — `SqliteSessionStore` 持久化会话/LTM/事实;`AppSession` 每次运行后自动保存。`oneai session list / resume <id> / delete / info`、`oneai memory search <kw> --user <id> / list --user <id>`。

### 用量与可靠性

- **用量记录（纯 token 维度）** — `UsageTracker` trait + `UsageRecord`，内置 `InMemoryUsageTracker` 与持久化用的 `SqliteUsageTracker`（`oneai-persistence`）。每次推理后由 AgentLoop 记录 prompt/completion/total tokens 与调用次数，**不追踪任何 USD 金额或预算**（USD 成本/预算管理已移除）—— `oneai usage report / session <id> / export`。
- **RateLimiter**（`TokenWindowRateLimiter`）+ **CircuitBreaker**（`ThresholdCircuitBreaker`，Closed/Open/HalfOpen）—— 在 AgentLoop 内强制执行。
- **Token 计数** — `HeuristicTokenCounter`（按 Provider、CJK 感知）+ `ContextWindowProfile` + 4 种裁剪策略 + 装得下检查 —— `oneai token`。

### 3 层输出解析器

LLM 输出经 3 层防御：约束解码 → 模糊 JSON 修复（括号补全、正则提取、嵌入式 JSON 检测）→ 回退自纠重提示。请复用它，而非直接解析模型输出。

### 工作流引擎

- **WorkflowDag** — 声明式 DAG，用于并行步骤编排。
- **StateGraph** — 有环有向图，用于需要迭代的 Agent 流程（ReAct 循环、条件路由、中断点）。StateGraph 与 AgentLoop 形成闭环：图节点可发出 `GraphDecision::SwitchParadigm`/`Delegate`/`ToolCalls`。

### RAG

`EmbeddingService` trait，含 OpenAI/Anthropic/Voyage/Ollama/FastEmbed（本地 ONNX）实现，`EmbeddingServiceRegistry`（缓存+降级），`AutoEmbeddingDocumentIndex` 在 `add_document()` 时自动 embedding。分块：SentenceBoundary/FixedSize/Paragraph。

### A2A 协议、WASM 沙箱、评测、Studio、MCP

- **A2A**（`oneai-a2a`）— Agent 间协议 SDK：客户端 + axum JSON-RPC 服务端宿主 + DomainPack→AgentCard 自动暴露。`oneai a2a serve / discover / list / send`。
- **WASM**（`oneai-wasm`）— Wasmtime 沙箱执行不可信代码：`WasmTool`、`WasmModuleRegistry`、资源监控、WASI 受限访问、Native↔Wasm 执行模式。`oneai wasm list / load / run / health / stats`。
- **Eval**（`oneai-eval`）— `EvalCase`/`ExpectedOutput`/`EvalMetric`/`EvalRunner` + 6 内置指标 + 3 套件。`oneai eval run <suite>` / `eval score`。另含 **SWE-bench 三轴评测**（能力 resolved × 用量 usage × 效率 efficiency），见[下文专节](#swe-bench-评测能力用量效率三轴)。
- **Studio**（`oneai-studio`）— axum HTTP+WebSocket 服务、REST API、实时事件推送、D3.js SVG StateGraph 可视化、Checkpoint 时间旅行。`oneai studio`。
- **MCP 生态**（`oneai-mcp`）— `McpServerHost`（JSON-RPC 服务端）+ `McpPluginRegistry`（发现/配置/连接）+ TOML 配置 + stdio 传输。`oneai mcp serve / list / add / remove / connect`。

### 轨迹日志（Trace）

OpenInference 兼容轨迹用于 Agent 评测，外加 OTEL 导出器（`OtlpCollector` + `OtelMetricsProvider`）：

```rust
let app = AppBuilder::new().trace_in_memory().build()?;
session.end_session(SpanStatus::Ok);
let tree = session.build_trace_tree();
println!("成功率: {:.1}%", tree.metrics.success_rate * 100.0);
```

---

## SWE-bench 评测（能力×用量×效率三轴）

OneAI 接入 [SWE-bench Lite](https://www.swebench.com/)（300 实例）做 coding agent 评测，按 **能力（resolved）× 用量（usage）× 效率（efficiency）** 三轴采集：

- **能力轴** ← SWE-bench 外部 harness 判定（`resolved` true/false），由 `SwebenchJudge` 调 Python subprocess 得到。
- **用量轴** ← `UsageTracker.session_usage()`（api_calls + prompt/completion/total token 拆分，纯 token 维度，无 USD）。
- **效率轴** ← `TraceMetrics`（total_tokens / tool_call_count / avg_iterations）+ 各阶段 wall-clock 耗时拆解。

每条实例：`git clone <repo>` → `git checkout <base_commit>` → 用 `problem_statement` 驱动 agent（CodingPack 提供 read_file/edit_file/grep/glob/shell）→ `git diff` 收 patch → 外部 harness 判 `resolved`，三轴全部写入 `EvalResult`。

### 前置准备

```bash
# 1) 建 venv 并装 datasets + swebench + modal（一个 venv 两用：导数据 + 判定）
#    macOS Homebrew Python 受 PEP 668 限制不能系统级装包，必须走 venv
python3 -m venv ~/.venvs/swebench
~/.venvs/swebench/bin/pip install datasets swebench modal httpx[socks]
~/.venvs/swebench/bin/modal token new        # 登录 Modal

# 2) 用该 venv 的 python 导出数据集 JSONL 到本地（Rust 端不做 HF 网络）
~/.venvs/swebench/bin/python scripts/swebench/export_dataset.py
# → 生成 swe_bench_lite.jsonl（300 行）

# 3) LLM Provider（agent 真调 API = 真花钱）
export ONEAI_API_KEY=sk-...
```

> 也可用 Verified（500 实例）：`export_dataset.py --dataset princeton-nlp/SWE-bench_Verified --out swe_bench_verified.jsonl`。
> `scripts/swebench/` 另有 `fetch_instance.py`（拉单个实例元数据）和 `make_prediction.py`（手工 git diff→JSONL），是阶段一的手工通路，阶段二已被下面的 CLI 命令替代。
> 判定器默认找 `~/.venvs/swebench/bin/python`（即上面建的 venv），用 `--python <path>` 可覆盖。

### 测试某一个实例（先这样冒烟，确认闭环）

```bash
cargo run -p oneai-cli-demo -- eval swebench \
    --dataset ./swe_bench_lite.jsonl \
    --instances astropy__astropy-12907 \
    --workspace ./swebench-workspace \
    --run-id oneai-smoke
```

### 测试一批实例

```bash
# 限定 N 条（从数据集顺序取前 N，避免一次烧太多 API）
cargo run -p oneai-cli-demo -- eval swebench \
    --dataset ./swe_bench_lite.jsonl \
    --limit 10 \
    --workspace ./swebench-workspace \
    --run-id oneai-batch

# 或指定多个 instance id
cargo run -p oneai-cli-demo -- eval swebench \
    --dataset ./swe_bench_lite.jsonl \
    --instances astropy__astropy-12907,django__django-11099 \
    --workspace ./swebench-workspace \
    --run-id oneai-batch
```

### 测试全量 300 个实例

```bash
cargo run -p oneai-cli-demo -- eval swebench \
    --dataset ./swe_bench_lite.jsonl \
    --workspace ./swebench-workspace \
    --run-id oneai-full-$(date +%Y%m%d) \
    --format json
```

> 全量 300 实例会真实克隆 300 个仓库、跑 300 轮 agent，API 费用和时间都不小——建议先单条冒烟再上批量。

### 产物与可选项

每次运行在 `--workspace` 下产出：

| 文件 | 内容 |
|---|---|
| `predictions.jsonl` | agent 产出的 patch（可重跑/提交 harness） |
| `leaderboard.json` | swebench.com 提交 schema：`instance_calls` / `resolved_count` / `total_instances` / `resolution_rate` / `per_instance:[{instance_id, api_calls, resolved}]`（USD 成本字段已移除，可比口径是 `api_calls`） |
| `evaluation_results/<run_id>/` | swebench harness 自己写的判定明细 |

stdout 按下式输出报告：

| `--format` | 输出 |
|---|---|
| `markdown`（默认） | 人类可读报告（含每实例能力/用量/效率三轴） |
| `json` | 完整 `EvalReport` JSON |
| `compact` | CI 友好的一行摘要 |

常用选项：`--python <path>` 指定判定器解释器（默认 `~/.venvs/swebench/bin/python`）；`--modal false` 改本地 docker（Apple Silicon 需 `--namespace ''`，慢）；`--dataset-name princeton-nlp/SWE-bench_Lite` 传给 harness；`--run-id` 控制 `evaluation_results/` 子目录名。

---

## 跨平台：桌面与移动端

一套 Rust 内核（`oneai-core` + `oneai-app`），通过两条 FFI 通路打到六端原生 App：

- **UniFFI 绑定**（Kotlin / Swift / Python）—— Android、Apple 平台。
- **手写 `extern "C"` JSON facade**（`crates/oneai-uniffi/src/c_facade.rs`，头文件 `bindings/c/oneai_c.h`）—— 因 `uniffi-bindgen` 0.32 无 C#/ArkTS 生成器，Windows（C# P/Invoke `oneai.dll`）与 HarmonyOS（NAPI 包裹）复用此 facade。所有字符串以 UTF-8 过界，CJK 正确往返。

| 平台 | 技术 | 绑定语言 | 原生 InteractionGate |
|------|------|----------|----------------------|
| macOS | SwiftUI（`swiftc`，无需 Xcode） | Swift（UniFFI） | NSAlert |
| Windows | WinUI 3 / C# | C#（P/Invoke facade） | MessageBox |
| Linux | 桌面平台 crate | C++（facade） | MessageBox |
| Android | Jetpack Compose / Kotlin | Kotlin（UniFFI） | AlertDialog |
| iOS | SwiftUI / Swift | Swift（UniFFI xcframework） | UIAlertController |
| HarmonyOS | ArkTS / ArkUI + NAPI | C++（NAPI 包裹 facade） | CommonDialog |

各端共享同一设计：场景化多 Agent 群聊（5 内置预设）、流式 20fps 合并渲染、Markdown、暗色跟随系统、命令面板、产物画布。**macOS App 是参考实现，其他端镜像之。**

### macOS（参考实现，无需 Xcode）

```bash
# 前置：Command Line Tools（swiftc）+ Apple Rust targets
rustup target add aarch64-apple-darwin x86_64-apple-darwin

# 1) 生成 Swift 绑定 + 交叉编译 universal 静态库到 platforms/apple/
./scripts/generate_bindings.sh swift
./scripts/build_apple.sh

# 2) swiftc 编译 SwiftUI 源码 → OneAI.app
./platforms/macos/build_macos.sh        # 加 --debug 走 debug profile

# 3) 运行
open platforms/macos/build/OneAI.app
```

源码在 `platforms/macos/Sources/`（11 个 Swift 文件，~3.3k 行）：`OneAIApp`（入口/主题）、`Views`（聊天屏/侧边栏）、`ChatViewModel`（会话生命周期/流式合并）、`AgentStore`（场景 CRUD + 5 预设）、`Models`（Scenario/Agent/TurnPolicy）、`ScenarioEditor`、`CommandPalette`（⌘K）、`ArtifactCanvas`、`SpeechInput`、`Markdown`。

### Windows（WinUI 3 / C#）

```powershell
# 前置：Visual Studio + WindowsAppSDK workload
rustup target add x86_64-pc-windows-msvc

# 1) 交叉编译 oneai.dll（cdylib，同时导出 uniffi 符号与 c_facade extern "C" 符号）
pwsh ./scripts/build_windows.ps1

# 2) 构建 App（VS 打开 OneAI.sln，或 dotnet）
dotnet build platforms\windows\OneAI.sln -c Debug

# 3) 运行（unpackaged）
dotnet run --project platforms\windows\OneAI\OneAI.csproj -c Debug
```

源码在 `platforms/windows/OneAI/`（Native/ViewModels/Services/Views），README 见 `platforms/windows/README.md`。

### Android（Jetpack Compose / Kotlin）

```bash
# 前置：Android NDK + cargo-ndk
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android i686-linux-android
cargo install cargo-ndk

# 1) 4 ABI 交叉编译 + 暂存 liboneai.so 与 Kotlin 绑定
./scripts/build_android.sh

# 2) 构建 & 安装
cd platforms/android
./gradlew assembleDebug
./gradlew installDebug
```

README 见 `platforms/android/README.md`。Ollama on host from emulator：`kind=ollama, model=llama3, base_url=http://10.0.2.2:11434`。

### iOS（SwiftUI / Swift）

```bash
# 前置：Xcode（Command Line Tools 不足以构建 iOS / xcframework）
rustup target add aarch64-apple-ios aarch64-apple-ios-sim

./scripts/build_apple.sh   # 检测到 xcodebuild 即额外产出 OneAI.xcframework（iOS+macOS slice）
```

`build_apple.sh` 无 Xcode 时只产出 macOS 静态库（跳过 iOS/xcframework 并提示）。iOS App 工程待补；绑定与 xcframework 由该脚本就绪。

### HarmonyOS（ArkTS / ArkUI + NAPI）

```bash
# 前置：DevEco Studio + HarmonyOS Native SDK
rustup target add aarch64-linux-ohos x86_64-linux-ohos
export OHOS_NDK_HOME=/path/to/harmony/native   # 含 llvm/bin/clang

# 1) 交叉编译 liboneai.so 并暂存到 CMake 能找到的位置
./scripts/build_harmony.sh

# 2) DevEco Studio 打开 platforms/harmony → hvigorw assembleHap
#    CMake 构建 liboneai_napi.so（包裹 oneai_* C 符号）并链接 liboneai.so
```

README 见 `platforms/harmony/README.md`。流式回调经 `napi_threadsafe_function` 在 tokio worker 线程触发、ArkTS 线程派发。

---

## 项目结构

```
oneai/
├── crates/
│   ├── oneai-core/          # 基础：类型、trait、PermissionLevel、Budget
│   ├── oneai-provider/      # OpenAI/Anthropic/Gemini/Ollama + ProviderPool + SmartRouter
│   ├── oneai-parser/        # 3 层输出解析
│   ├── oneai-memory/        # STM、LTM、压缩、HNSW、MemoryManager + 持久化
│   ├── oneai-tool/          # 注册、12 工具、MCP 客户端、审批、执行器
│   ├── oneai-skill/         # 技能注册 + 选择器 + 内置领域技能
│   ├── oneai-domain/        # DomainPack（7 层）、CodingPack、市场、规范校验器
│   ├── oneai-agent/         # AgentLoop、SubAgent、范式、Team/Handoff/Swarm/GroupChat、StreamParser
│   ├── oneai-rag/           # Document、Index、EmbeddingService、Retrieval
│   ├── oneai-workflow/      # DAG、StateGraph、编译器、验证器、执行器
│   ├── oneai-scheduler/     # InMemoryScheduler
│   ├── oneai-persistence/   # Checkpoint + SQLite 会话/用量后端
│   ├── oneai-a2a/           # A2A 协议 SDK（客户端 + 服务端宿主）
│   ├── oneai-wasm/          # Wasmtime 沙箱 + WasmTool + 模块注册
│   ├── oneai-eval/          # 评测用例/指标/Runner/套件 + SWE-bench 三轴
│   ├── oneai-studio/        # Studio Web UI（axum + WS + D3 可视化）
│   ├── oneai-mcp/           # MCP 服务端宿主 + 插件注册
│   ├── oneai-app/           # AppBuilder、App、AppSession
│   ├── oneai-trace/         # OpenInference 轨迹 + OTEL 导出器
│   ├── oneai-uniffi/        # UniFFI 绑定定义 + 手写 extern "C" facade
│   └── oneai-platform-{desktop,android,ios,harmony}/
├── platforms/               # 六端原生 App（SwiftUI / WinUI / Compose / ArkTS）
│   ├── macos/               # 参考实现（SwiftUI，swiftc 构建）
│   ├── windows/             # WinUI 3 / C#
│   ├── android/             # Jetpack Compose / Kotlin
│   ├── ios/                 # SwiftUI（Xcode xcframework）
│   ├── harmony/             # ArkTS / ArkUI + NAPI
│   └── apple/               # 交叉编译暂存：liboneai.a + headers + binding
├── examples/
│   ├── cli/                 # 交互式 TUI 演示（ratatui + crossterm）— bin: oneai-cli
│   ├── desktop-app/         # 桌面审批门演示
│   └── rust/                # Channel 审批门演示
├── bindings/                # 生成的绑定（cpp/csharp/kotlin/swift + 手写 c/）
├── scripts/                 # generate_bindings.sh + build_{apple,windows,android,harmony}.sh + swebench/
└── Cargo.toml               # Workspace 根配置（resolver = "2"，edition 2021，v0.2.0）
```

---

## 构建、测试、运行

```bash
cargo build                      # 构建整个 workspace
cargo test                       # 全部 1378 测试（24 个 crate）
cargo test -p oneai-agent        # 单个 crate 的测试
cargo test -p oneai-agent plan   # 单个测试/模块
cargo clippy --workspace --all-targets   # 保持 lint 干净
cargo run -p oneai-cli-demo      # 启动交互式 TUI（bin: oneai-cli）
```

Workspace 使用 `resolver = "2"`、`edition = "2021"`、共享版本 `0.2.0`（来自 `[workspace.package]`），所有共享依赖在 `[workspace.dependencies]` 中锁定。公开枚举均加 `#[non_exhaustive]`，作为 v0.2.0 API 稳定性承诺的一部分。

### 网络代理

所有出站 HTTP（LLM Provider API、`web_search`/`web_fetch`、A2A 客户端、embedding 服务、MCP HTTP 传输）都走 `reqwest::Client`，故代理基于环境变量、全端统一：

- `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` —— 代理 URL（reqwest 在每个 client build 时自动探测，无需代码 opt-in）。
- `NO_PROXY` —— 逗号分隔排除列表。
- SOCKS5：`ALL_PROXY=socks5://host:port`（reqwest `socks` feature 已开）。
- macOS/Windows 上 `system-proxy` feature 还读 OS GUI 代理设置；环境变量始终优先。

---

## 开发路线

| 阶段 | 重点 | 状态 |
|------|------|------|
| 1–11 | 核心、Provider、Parser、范式、记忆、工具、工作流、持久化、AppBuilder、UniFFI、平台 UI、轨迹、DomainPack、TUI | ✅ 完成 |
| P2-1 | SubAgent + Worktree 隔离 + 并行执行 | ✅ 完成 |
| P2-2 | StateGraph ↔ AgentLoop 闭环执行 | ✅ 完成 |
| P2-3/4 | OTEL 可观测性 + STM↔LTM 闭环 | ✅ 完成 |
| P2-5 | A2A 协议 SDK | ✅ 完成 |
| P2-6 | WASM 沙箱引擎 | ✅ 完成 |
| P3-1 | API 稳定化（`#[non_exhaustive]`、v0.2.0） | ✅ 完成 |
| P3-2/3 | DomainPack 市场 + CLI 打磨（clap 子命令 + 配置） | ✅ 完成 |
| P3-4/5 | 评测框架 + Studio Web UI | ✅ 完成 |
| P3-6 | MCP 服务生态 | ✅ 完成 |
| P4-1/2 | A2A 服务端宿主 + MCP 客户端增强 | ✅ 完成 |
| P4-3/4 | DomainPack 规范校验器 + WASM 运行时增强 | ✅ 完成 |
| P5-1/2/3 | SQLite 持久化 + Embedding 服务 + 用量（token）管理 | ✅ 完成 |
| P6-1/2/3 | ProviderPool + SmartRouter + Token 计数/上下文管理 | ✅ 完成 |
| P7-1/2/3 | Team 协作 + Handoff 协议 + Swarm 群体编排 | ✅ 完成 |
| Native | 六端原生 App：macOS/Windows/Android/iOS/HarmonyOS + 场景化多 Agent 群聊 | ✅ macOS/Windows/Android/Harmony · 🚧 iOS |
| TUI | 工具展示、Plan 模式审批门、技能披露、滚动性能、鼠标选中 | ✅ 完成 |

---

## 许可证

Apache-2.0 — 详情见 [LICENSE](LICENSE)。
