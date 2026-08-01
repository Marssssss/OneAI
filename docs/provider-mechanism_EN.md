# OneAI Provider / Routing / Parser Mechanism

> LLM provider abstraction + fallback pool + multi-factor routing + a 3-layer output parser defense — for "providers fail, models hallucinate".

## Responsibility

Unify multiple LLM vendors under one `LlmProvider` trait; layer two resilience tiers on top (ProviderPool fallback chain, SmartRouter multi-factor routing); and defend unreliable model output with a 3-layer parser. **The LLM provider is optional** — tool-only or workflow-only usage needs no provider.

## LlmProvider

`infer` + `infer_stream`. Built-in OpenAI / Anthropic / Gemini / Ollama, plus any OpenAI-compatible gateway (DashScope / DeepSeek / vLLM …). All outbound HTTP goes through `reqwest::Client`, so proxy support is env-var based and uniform across targets (see [CLAUDE.md — Network proxy](../CLAUDE.md)).

## Two layers on top

- **ProviderPool** — fallback chain; each provider carries its own circuit breaker + rate limiter + degradation rule (e.g. Anthropic→OpenAI→local), with automatic 429 retry and `Retry-After` parsing.
- **SmartRouter** — multi-factor routing (latency / quality / balanced / custom) scoring to pick the best, integrating circuit breaking / rate limiting / context constraints, logging every decision.

```rust
let app = AppBuilder::new()
    .default_provider_pool_anthropic()   // Anthropic → OpenAI → Ollama fallback
    .default_smart_router_balanced()     // multi-factor routing
    .build()?;
```

## 3-layer output parser (`oneai-parser`)

Reuse it rather than parsing model output directly:

1. **Constrained decoding** — streaming incremental structure constraint
2. **Fuzzy JSON repair** — bracket completion / regex extraction / embedded-JSON detection
3. **Fallback self-correction re-prompt** — on failure, build a corrective prompt for the model to regenerate

## Key types & files

| Item | Location |
|---|---|
| OpenAI / Anthropic / Gemini / Ollama | `crates/oneai-provider/src/{openai,anthropic,gemini,ollama}.rs` |
| `ProviderPool` / degradation rules | `crates/oneai-provider/src/provider_pool.rs` |
| `SmartRouter` | `crates/oneai-provider/src/model_router.rs` |
| Factory / compat gateway | `crates/oneai-provider/src/provider_factory.rs`, `compat.rs` |
| 3-layer parser | `crates/oneai-parser/src/{three_layer,fuzzy,constrained,fallback}.rs` |
| `LlmProvider` / `TokenCounter` trait | `crates/oneai-core/src/traits.rs` |

## Related CLI

[`provider status / fallback-log / test / route / route-log / route-config`](cli-reference_EN.md#provider-pool-and-smart-routing).

## Further reading

- [CLAUDE.md — Provider / parser](../CLAUDE.md)
- Model-context 3-layer resolution — see [Context-management mechanism](context-management-mechanism_EN.md)
