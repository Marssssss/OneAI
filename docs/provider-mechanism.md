# OneAI Provider / 路由 / 解析器机制

> LLM Provider 抽象 + 降级池 + 多因子路由 + 3 层输出解析防御，应对「provider 会挂、模型会胡说」。

## 职责

把多家 LLM 厂商统一到一个 `LlmProvider` trait 之上；在其上叠两层韧性（ProviderPool 降级链、SmartRouter 多因子路由）；再用 3 层解析器防御不可靠的模型输出。**LLM Provider 是可选的**——纯工具 / 纯工作流用法无需 Provider。

## LlmProvider

`infer` + `infer_stream`。内置 OpenAI / Anthropic / Gemini / Ollama，外加任意 OpenAI 兼容网关（DashScope / DeepSeek / vLLM 等）。所有出站 HTTP 走 `reqwest::Client`，代理靠环境变量全端统一（见 [CLAUDE.md — Network proxy](../CLAUDE.md)）。

## 其上两层

- **ProviderPool** — 降级链，每 Provider 自带熔断器 + 限流器 + 降级规则（如 Anthropic→OpenAI→本地），自动 429 重试并解析 `Retry-After`。
- **SmartRouter** — 多因子路由（延迟 / 质量 / 均衡 / 自定义）打分选优，集成熔断 / 限流 / 上下文约束，每次决策留日志。

```rust
let app = AppBuilder::new()
    .default_provider_pool_anthropic()   // Anthropic → OpenAI → Ollama 降级
    .default_smart_router_balanced()     // 多因子路由
    .build()?;
```

## 3 层输出解析器（`oneai-parser`）

复用它而非直接解析模型输出：

1. **约束解码** — 流式增量结构约束
2. **模糊 JSON 修复** — 括号补全 / 正则提取 / 嵌入式 JSON 检测
3. **回退自纠重提示** — 失败时构造自纠提示让模型重生

## 关键类型与文件

| 项 | 位置 |
|---|---|
| OpenAI / Anthropic / Gemini / Ollama | `crates/oneai-provider/src/{openai,anthropic,gemini,ollama}.rs` |
| `ProviderPool` / 降级规则 | `crates/oneai-provider/src/provider_pool.rs` |
| `SmartRouter` | `crates/oneai-provider/src/model_router.rs` |
| 工厂 / 兼容网关 | `crates/oneai-provider/src/provider_factory.rs`、`compat.rs` |
| 3 层解析器 | `crates/oneai-parser/src/{three_layer,fuzzy,constrained,fallback}.rs` |
| `LlmProvider` / `TokenCounter` trait | `crates/oneai-core/src/traits.rs` |

## 相关 CLI

[`provider status / fallback-log / test / route / route-log / route-config`](cli-reference.md#provider-池与智能路由)。

## 深入阅读

- [CLAUDE.md — Provider / parser 章节](../CLAUDE.md)
- 模型上下文三层解析见 [context-management 机制](context-management-mechanism.md)
