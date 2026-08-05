# OneAI DomainPack 机制

> 7 层声明式领域配置包——工具+装饰器 / ContextSource / PermissionProfile / ParadigmStrategy / CompressionTemplate / Workflow+StateGraph / MemoryProfile：把"领域知识"从硬编码里抽出来，变成声明式、可合并（strictest-wins）、可校验（JSON Schema）、一行切换、可从市场安装共享的配置包。

## 1. 概述（是什么）

DomainPack 是 OneAI 的中心扩展机制。一个 coding agent 与一个 research agent 的差别，不在引擎，而在领域知识——用什么工具、感知什么环境、哪些操作要审批、什么任务走什么范式、压缩时保留什么、跑什么工作流、记忆怎么管。OneAI 把这七类领域知识显式拆成七层配置，封装进一个 `DomainPack`，于是同一套引擎在不同领域间用一行 `AppBuilder::domain_pack(...)` 切换，无需改代码。

关键洞察来自参考实现 CodingPack：一个编码 agent 隐式地通过五层配置 embed 它的工作流（工具集、环境感知、权限、范式、压缩优先级）。OneAI 把这五层显式化、再加 Workflow 与 Memory 两层凑成七层，让它们声明式、可插拔、可组合。多 DomainPack 可合并以构建多领域 Agent——合并规则明确：权限取最严（strictest-wins，安全优先），上下文源按优先级合并，core 记忆预算取 min、工具取 OR。这一层横切所有特性层，`oneai-domain` 不属于某一层级，而是声明式配置层。

## 2. 职责与能力（做什么）

| 层 | 组件 | 作用 |
|---|---|---|
| ① | Tools + ToolDecorator | 领域专属工具集 + 工具描述覆写（不改工具实现，只改模型看到的 description）|
| ② | ContextSource | 领域专属环境感知，带 `RefreshPolicy`（每轮/OnChange/OnceAtStart/OnResume/Periodic）|
| ③ | PermissionProfile | 权限分类：`deny_by_default`/`auto_approve`/`require_confirmation`/`permission_overrides` |
| ④ | ParadigmStrategy | 任务→范式映射，声明何时进 Plan/ReAct/Reflect/Explore + SubAgent 定义 |
| ⑤ | CompressionTemplate | 压缩保留优先级：`preserve_fields`/`template`/`truncate_rules` |
| ⑥ | Workflow + StateGraph | 领域预定义工作流与有环图（react/plan/reflect/explore）|
| ⑦ | MemoryProfile | 记忆策略：抽取 schema/召回配置/core 预算/自管理工具/跨会话习惯/working-state/衰减 |

**配套能力：** `DomainPackBuilder` 链式构造七层；`merge` 模块做多 pack 合并（strictest-wins）；`DomainPackSpec` 产出 JSON Schema（draft-2020-12，跨语言可校验）；`DomainPackValidator` 做结构 + 语义校验；`market`（`PackSource`/`PackRegistry`）从本地/git 安装与索引；`CodingPack`/`ResearchPack` 是内置参考实现；`ContainerizedCodingPack` 是 Gondolin 模式（VM/容器即安全边界）。

**显式不做什么**：不实现工具执行（归 `oneai-tool`）；不实现 LLM 推理（归 provider）；不持久化运行时状态（pack 是静态配置）；不定义引擎行为（只声明式配置引擎）；`MemoryProfile` 承载策略但不实现记忆机制本体（归 `oneai-memory`）。

## 3. 设计动机（为什么这样实现）

| 决策 | 理由 | 否决的替代方案 |
|---|---|---|
| 七层显式拆分而非整体配置 | 编码 agent 隐式 embed 的五类领域知识是真实存在的关切分离点；显式拆层让每层独立演进、独立校验、独立合并 | 单一大配置对象 → 改一处牵全身、无法分层合并 |
| 声明式而非代码 | 领域配置要可存盘、可从 git 安装、可跨 session 复用、可被非 Rust 用户（pack 作者）编写 | 代码定义领域 → 不可声明、不可共享、需重编译 |
| `AppBuilder::domain_pack(...)` 一行切换 | 切换领域是高频操作（编码→研究→通用），应是一行而非散落多处的接线 | 多处分别配工具/权限/范式 → 易漏配、易漂移 |
| 多 pack 合并 strictest-wins（权限）| 多领域 Agent 叠加时，安全策略必须取最严——research pack 放行 web_fetch、coding pack 要求确认 shell，叠加应取确认 | 取宽松 → 安全降级；取 OR → 总是放行 |
| `ContextSource` 带 `RefreshPolicy` | 环境信息变化频率不同（git status 每轮变、项目配置不变），统一每轮刷新浪费 token，OnChange/OnceAtStart/OnResume/Periodic 按需 | 全部每轮刷 → token 浪费；全部一次性 → 信息过时 |
| `OnResume` 独立策略 | 跨 session 续接时要做一次性 ground-truth 对账（§8.2），既非每轮也非一次性，是 resume 时刻触发一次 | 用 EveryIteration/Once → 续接对账无处安放 |
| JSON Schema 校验 + 语义校验 | pack 作者会写错配置（自指依赖、孤儿节点、缺审批 gate）；结构校验查语法、语义校验查跨层一致性，两层才够 | 仅结构校验 → 语义错漏到运行时才暴露 |
| `ToolDecorator` 覆写 description 而非改实现 | 同一工具在不同领域需要不同描述（shell 在 coding 是"跑构建"、在 ops 是"管服务"）；只改模型看到的描述，不改执行 | 每领域重写工具 → 重复 |
| `ContainerizedCodingPack` drop-in 替换 | Gondolin 模式把同名工具换成 VM 后端实现，VM 即安全边界不砍权限（戒律#1），pack 级 drop-in 不动引擎 | 引擎级改 → 跨领域污染 |

## 4. 架构与核心抽象

```mermaid
flowchart TB
    DP["DomainPack（7 层静态配置）"]
    B["AppBuilder.domain_pack(dp)"]
    Engine["oneai-agent AgentLoop + oneai-tool/oneai-memory/oneai-workflow"]

    L1["①Tools+Decorator"] ::: DP
    L2["②ContextSource"]
    L3["③PermissionProfile"]
    L4["④ParadigmStrategy"]
    L5["⑤CompressionTemplate"]
    L6["⑥Workflow+StateGraph"]
    L7["⑦MemoryProfile"]

    DP --> B
    B --> Engine
    DP --> L1 & L2 & L3 & L4 & L5 & L6 & L7
    L1 -. 横切注入 .-> Engine
    L3 -. 横切注入 .-> Engine
    L7 -. 横切注入 .-> Engine
```

`DomainPack` 是七层字段的聚合，由 `DomainPackBuilder` 链式构造：

```rust
pub struct DomainPack {
    pub name: String, pub description: String, pub system_prompt: Option<String>,
    // ①
    pub tools: Vec<Arc<dyn Tool>>, pub tool_decorators: Vec<ToolDecorator>,
    pub context_sources: Vec<Arc<dyn ContextSource>>,   // ②
    pub permission_profile: PermissionProfile,           // ③
    pub paradigm_strategies: Vec<ParadigmStrategy>,      // ④
    pub compression_template: Option<CompressionTemplate>, // ⑤
    pub workflows: Vec<WorkflowConfig>, pub state_graphs: Vec<StateGraph>, // ⑥
    pub memory_profile: MemoryProfile,                  // ⑦
    pub sub_agent_definitions: Vec<SubAgentTypeDefinition>,
}
```

`ContextSource` 的刷新策略是这层的关键抽象：

```rust
#[non_exhaustive]
pub enum RefreshPolicy {
    EveryIteration,   // 每轮（git status）
    OnChange,         // 检测 diff 才产出新 token（OpenCode Context Epoch）
    OnceAtStart,      // 启动一次（项目配置）
    OnResume,         // resume 时一次（ground-truth 对账，take 模式）
    Periodic(Duration),
}
```

## 5. 参与的流程

**装配期（AppBuilder）：** `AppBuilder::domain_pack(dp)` 把 pack 的七层注入引擎各处——①工具注册进 `ToolRegistry`、②ContextSource 注册进 `ContextAssembler`、③PermissionProfile 转 `PermissionResolver` 注入 `ToolExecutor`、④ParadigmStrategy 注入 AgentLoop 范式路由、⑤CompressionTemplate 注入 `ContextCompressor`、⑥Workflow/StateGraph 注册进 `StateGraphExecutor`、⑦MemoryProfile 注入 `MemoryManager` + working-state store。一行切换即改全套。

**运行期（每轮迭代）：** AgentLoop 每轮调 `ContextAssembler` 装配上下文时，按各 ContextSource 的 `refresh_policy` 决定是否 `load()`——`EveryIteration` 每轮、`OnChange` 检测 diff、`OnResume` 在续接首轮 take 一次。`build_tool_definitions_for_paradigm` 按④ParadigmStrategy 的 `tool_filter` 过滤工具集，按③PermissionProfile 经 `PermissionResolver` 解析权限。压缩时 `ContextCompressor` 按⑤CompressionTemplate 的 `preserve_fields` 保留关键字段、按 `truncate_rules` 截断、按 `template` 渲染摘要 prompt。

**合并期（多领域）：** `DomainPack::merge(packs)` 按 strictest-wins 合权限（`require_confirmation` 胜 `auto_approve`）、按优先级合上下文源、core 预算取 min、工具取 OR、MemoryProfile 按 `MemoryProfile::merge` 合并 schema/habits。

## 6. 依赖关系

| 方向 | 谁 | 内容 |
|---|---|---|
| 上游 | `oneai-core` | `Tool`/`ContextSource`(trait 在 domain)？实际 trait 在 domain；core 提供 `PermissionLevel`/`MemoryFact` 等共享类型 |
| 上游 | `oneai-workflow` | `WorkflowConfig`/`StateGraph` 类型（第⑥层引用）|
| 上游 | `serde`/`serde_json`/`regex` | 配置序列化、JSON Schema 生成、deny 模式正则 |
| 下游 | `oneai-app` | `AppBuilder::domain_pack(...)` 唯一装配入口 |
| 下游 | `oneai-agent` | AgentLoop 消费范式/上下文/工具过滤 |
| 下游 | `oneai-tool` | `PermissionProfile` 转 `PermissionResolver` 注入 `ToolExecutor` |
| 下游 | `oneai-memory` | `MemoryProfile` 注入 `MemoryManager` |
| 横切接入 | 引擎各层 | DomainPack 不是某一级，是横切所有特性层的声明式配置层 |

## 7. 关键类型与文件

| 项 | 位置 |
|---|---|
| `DomainPack`（7 层聚合）+ `DomainPackBuilder` | `crates/oneai-domain/src/domain_pack.rs:50,198` |
| `ContextSource` trait + `RefreshPolicy`（5 变体）| `crates/oneai-domain/src/context_source.rs:73,31` |
| `PermissionProfile` + `DenyPattern` | `crates/oneai-domain/src/permission_profile.rs:118,37` |
| `ParadigmStrategy` + `SubAgentTypeDefinition` + `SubAgentMergeStrategy` | `crates/oneai-domain/src/paradigm_strategy.rs:314,88,280` |
| `CompressionTemplate` | `crates/oneai-domain/src/compression_template.rs:44` |
| `MemoryProfile` + `WorkingStatePolicy` + `CompactionConfig` | `crates/oneai-domain/src/memory_profile.rs` |
| `merge`（strictest-wins + 优先级 + core 取 min）| `crates/oneai-domain/src/merge.rs:99` |
| `DomainPackSpec`（JSON Schema draft-2020-12）| `crates/oneai-domain/src/spec.rs:33`（`schema():43`）|
| `DomainPackSpecFile`（validate→build）| `crates/oneai-domain/src/spec_file.rs` |
| `DomainPackValidator` + `ValidationIssue`/`Result`/`Severity`（结构+语义）| `crates/oneai-domain/src/validator.rs` |
| `PackSource`/`PackIndexEntry`/`PackRegistry`（市场）| `crates/oneai-domain/src/market.rs:35,55,81` |
| `CodingPack` 参考实现 | `crates/oneai-domain/src/coding_pack.rs` |
| `ResearchPack` | `crates/oneai-domain/src/research_pack.rs` |
| `ContainerizedCodingPack`（Gondolin VM 模式）| `crates/oneai-domain/src/containerized_pack.rs` |
| `config_parser`（YAML/TOML→pack）| `crates/oneai-domain/src/config_parser.rs` |
| `builtin_sources` + `repo_map` + `project_info` | `crates/oneai-domain/src/{builtin_sources,repo_map,project_info}.rs` |

## 8. 与业界对比

| 系统 | 模型 | OneAI 取舍 |
|---|---|---|
| **Claude Code** | 隐式 coding agent 配置（工具/沙箱/权限散在代码）| OneAI 把隐式配置显式成 7 层声明式 pack，可切换、可合并、可校验、可共享；Claude Code 无 pack 概念 |
| **Cursor / Cline** | 领域配置靠 prompt + rules 文件 | OneAI 不只是 prompt——七层里 ContextSource/Permission/Workflow/Memory 是引擎级接线，prompt 只是①+④的一部分 |
| **OpenAI Custom GPTs** | 单一 system prompt + tools 配置 | OneAI 多了权限分级、范式映射、压缩模板、工作流、记忆策略五层，且 strictest-wins 合并让多领域叠加安全 |
| **LangChain Hub prompts** | prompt 模板共享 | OneAI pack 是完整领域配置包（含工具/权限/工作流），经 JSON Schema 校验，从市场安装 |
| **AutoGen AgentConfig** | agent 级配置 | OneAI 是 domain 级（一个 pack 跨多个 agent 复用），且七层覆盖了 AutoGen 不涉及的压缩/记忆/工作流 |

OneAI 独特点：**七层声明式 + strictest-wins 合并**——多领域 Agent 的安全策略不会因叠加而降级，且整包可对照 JSON Schema 校验、从市场安装，是少数把"领域知识"做成一等公民可共享资产的框架。

## 9. 扩展点与配置

- **切领域**：`AppBuilder::domain_pack(coding_pack("/dir"))` 一行，或 `domain_pack_from_dir` 自动探测配置文件。
- **写自定义 pack**：`DomainPackBuilder::new(name)` 链式构造七层；或写 YAML/TOML 经 `config_parser` 转 pack。
- **多领域合并**：注册多个 pack，`merge` 自动 strictest-wins。
- **校验 pack**：`DomainPackSpec::schema()` 出 JSON Schema 用任意校验器；`DomainPackValidator` 做结构+语义校验。
- **市场安装**：`PackRegistry` 从 `PackSource::Git`/本地安装，索引缓存于 `~/.oneai/packs`。
- **Gondolin 模式**：`ContainerizedCodingPack` drop-in 替换 CodingPack，同名工具接 VM 后端。
- **CLI**：`oneai pack list/show/install/validate/spec/check`（详见 [cli-reference](cli-reference.md)）。

## 10. 深入阅读

- [memory-mechanism.md](memory-mechanism.md) —— 第⑦层 `MemoryProfile` 的下游机制本体
- [permission-mechanism.md](permission-mechanism.md) —— 第③层 `PermissionProfile` → `PermissionResolver`
- [context-management-mechanism.md](context-management-mechanism.md) —— 第②层 ContextSource + 第⑤层 CompressionTemplate
- [workflow-mechanism.md](workflow-mechanism.md) —— 第⑥层 Workflow+StateGraph
- [multi-agent-mechanism.md](multi-agent-mechanism.md) —— 第④层 ParadigmStrategy
- [tool-mechanism.md](tool-mechanism.md) —— 第①层 Tools+Decorator + Footprint ladder
- [CLAUDE.md — DomainPack 章节](../CLAUDE.md)
- 源码：`crates/oneai-domain/src/`（20 文件 / ~12.4K LOC）
