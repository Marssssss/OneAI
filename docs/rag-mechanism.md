# OneAI RAG / Embedding 机制

> `EmbeddingService` trait + adapter 注册表 + auto 探测 + 默认检索栈，长期记忆与文档检索的向量层。

## 职责

为长期记忆的语义召回与 RAG 文档检索提供向量能力。默认**零配置**：不填任何 embedding 字段即走 `auto` 探测，按顺序挑第一个可用 provider；全无可用时降级为关键词召回，不报错。

## EmbeddingService 与 auto 探测链

`LlmProvider` trait 无 embed 方法——LLM 与 embedding 是两项独立能力，**embedding key 与主模型 key 相互独立**（中转站大概率无 embedding 端点，故 auto 不复用主模型 key；Anthropic 无原生 embedding API，真实路径是 Voyage）。

auto 探测顺序：

1. `openai-compat` — 需同时设 `ONEAI_EMBEDDING_API_KEY` + `ONEAI_EMBEDDING_BASE_URL`（中转站）
2. `voyage` — 设 `VOYAGE_API_KEY`
3. `openai` — 设 `OPENAI_API_KEY`
4. `ollama` — 本地 `localhost:11434` 可达且装了 embedding 模型（如 `nomic-embed-text`）
5. `fastembed` — 本地 ONNX（`AllMiniLML6V2`，无 key；首次使用一次性下载 ~22MB，之后离线）。代理网络下 hf-hub 下载客户端可能忽略代理 → 跑 `./scripts/download_fastembed_models.sh`（用 curl，尊重代理）预拉取。
6. 全无可用 → 关键词召回

显式覆盖三处均可：CLI `--provider` / `~/.oneai/config.toml` `[embedding]` / 各平台 App 设置面板。模型名为自由字符串，未知名称运行时探测维度。`fallback` 字段在主 provider 失败时自动切换（构建期 + 运行期，共享一套 `should_continue` 错误分类：429/5xx/传输错/缺 key 降级，其它报错）。超长输入按 UTF-8 字节二分自动切分（CJK 不截断）。

## 默认检索栈（`oneai-vector`）

`StandardRetrievalPipeline` = BM25 + dense → RRF(k=60) → 可选 rerank，由 `InMemoryVectorBackend` / `SqliteVecBackend` / `UsearchBackend` + `TantivyBm25Backend` + `BgeM3Embedder` / `BgeRerankerOnnx` 组合。`AppBuilder::default_retrieval_stack()` 一行启用（内存后端；生产可显式 `retrieval_backend()` 接 Qdrant 等）。

## 关键类型与文件

| 项 | 位置 |
|---|---|
| `EmbeddingService` trait | `crates/oneai-core/src/traits.rs` |
| adapter 注册表 / `EmbeddingResolver` | `crates/oneai-rag/src/provider_adapter.rs` |
| embedding 实现 | `crates/oneai-rag/src/embedding.rs` |
| `DocumentIndex` 双后端 + `AutoEmbeddingDocumentIndex` | `crates/oneai-rag/src/index.rs` |
| 混合检索 / 召回 | `crates/oneai-rag/src/retrieval.rs` |
| 输入切分 / `ChunkingStrategy` | `crates/oneai-rag/src/chunk_split.rs` |
| 默认检索栈 | `crates/oneai-vector/src/pipeline.rs` 等 |

## 相关 CLI

[`embed generate / batch / list / health / dimension`](cli-reference.md#embedding-服务)。

## 深入阅读

- [CLAUDE.md — RAG / Network proxy 章节](../CLAUDE.md)
- 与记忆系统的衔接见 [记忆机制](memory-mechanism.md)
