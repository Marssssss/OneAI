# OneAI DomainPack 机制

> 把「领域知识」从硬编码里抽出来，变成声明式、可合并、可校验、一行切换的配置包。

## 职责

DomainPack 让同一套引擎在不同领域（编程 / 研究 / 通用）间无缝切换，而无需改代码。一个 pack 封装 7 层领域配置，可多 pack 合并（多领域 Agent），可对照 JSON Schema 校验，可从路径或 git 安装、通过市场共享。

## 7 层结构

| 层 | 组件 | 作用 |
|---|---|---|
| 1 | 工具 + ToolDecorator | 领域专属工具集与描述覆写 |
| 2 | ContextSource | 领域专属环境感知（含刷新策略） |
| 3 | PermissionProfile | 领域专属权限分类（拒绝 / 自动 / 确认） |
| 4 | ParadigmStrategy | 领域专属任务→范式映射 |
| 5 | CompressionTemplate | 领域专属上下文保留优先级 |
| 6 | Workflow + StateGraph | 领域预定义工作流与循环图 |
| 7 | MemoryProfile | 领域专属记忆策略（抽取 schema / 召回 / core 预算 / 自管理工具 / 跨会话习惯） |

`MemoryProfile` 同时承载 Working-State 策略与记忆衰减策略（见 [记忆机制](memory-mechanism.md)、[Working-State 机制](working-state-mechanism.md)）。

## 关键类型与文件

| 项 | 位置 |
|---|---|
| `DomainPack` 7 层定义 | `crates/oneai-domain/src/domain_pack.rs` |
| `CodingPack` 参考实现 | `crates/oneai-domain/src/coding_pack.rs` |
| `ContainerizedCodingPack`（VM/容器即边界） | `crates/oneai-domain/src/containerized_pack.rs` |
| Pack 市场（`PackSource`/`PackRegistry`） | `crates/oneai-domain/src/market.rs` |
| `DomainPackSpec` + 校验器 | `crates/oneai-domain/src/config_parser.rs` |
| `ContextSource` + 刷新策略 | `crates/oneai-domain/src/context_source.rs` |
| 压缩模板 | `crates/oneai-domain/src/compression_template.rs` |

## 核心流程

```rust
let app = AppBuilder::new()
    .provider(provider)
    .domain_pack(coding_pack("/project/dir"))  // ← 一行领域切换
    .build()?;
```

合并用于多领域 Agent（coding + research）：权限「严格优先」、上下文源按优先级合并。pack 可对照 `DomainPackSpec`（JSON Schema）做结构 + 语义校验，可 `pack install` 从本地或 git 安装。

## 相关 CLI

[`pack list / show / install / validate / spec / check`](cli-reference.md#domainpack领域配置包)。

## 深入阅读

- [CLAUDE.md — DomainPack 章节](../CLAUDE.md)（7 层定义、合并规则、Footprint ladder）
- 参考实现：[CodingPack](../crates/oneai-domain/src/coding_pack.rs)
