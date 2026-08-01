# OneAI RAG / Embedding Mechanism

> `EmbeddingService` trait + adapter registry + auto-probe + a default retrieval stack — the vector layer for long-term-memory recall and document retrieval.

## Responsibility

Provide vector capabilities for long-term-memory semantic recall and RAG document retrieval. **Zero-config by default**: leave every embedding field blank to use `auto`-probe, which picks the first available provider in order; if none is available it falls back to keyword recall without error.

## EmbeddingService & the auto-probe chain

The `LlmProvider` trait has no embed method — LLM and embedding are two independent capabilities, and **the embedding key is independent of the main-model key** (a relay station usually has no embedding endpoint, so auto does not reuse the main-model key; Anthropic has no native embedding API — the real path is Voyage).

auto-probe order:

1. `openai-compat` — requires both `ONEAI_EMBEDDING_API_KEY` + `ONEAI_EMBEDDING_BASE_URL` (relay)
2. `voyage` — set `VOYAGE_API_KEY`
3. `openai` — set `OPENAI_API_KEY`
4. `ollama` — local `localhost:11434` reachable and an embedding model installed (e.g. `nomic-embed-text`)
5. `fastembed` — local ONNX (`AllMiniLML6V2`, no key; first use downloads ~22MB once, then offline). Under a proxied network the hf-hub download client may ignore the proxy → run `./scripts/download_fastembed_models.sh` (uses curl, respects the proxy) to pre-fetch.
6. none available → keyword recall

Explicit overrides work in three places: CLI `--provider` / `~/.oneai/config.toml` `[embedding]` / each platform app's settings panel. The model name is a free string; unknown names are probed for dimension at runtime. The `fallback` field auto-switches on main-provider failure (build time + runtime, sharing one `should_continue` error classifier: 429/5xx/transport/missing-key degrade, other errors raise). Overlong input is auto-split on UTF-8 byte boundaries (CJK not truncated).

## Default retrieval stack (`oneai-vector`)

`StandardRetrievalPipeline` = BM25 + dense → RRF(k=60) → optional rerank, composed of `InMemoryVectorBackend` / `SqliteVecBackend` / `UsearchBackend` + `TantivyBm25Backend` + `BgeM3Embedder` / `BgeRerankerOnnx`. `AppBuilder::default_retrieval_stack()` enables it in one line (in-memory backend; production can explicitly `retrieval_backend()` to Qdrant etc.).

## Key types & files

| Item | Location |
|---|---|
| `EmbeddingService` trait | `crates/oneai-core/src/traits.rs` |
| adapter registry / `EmbeddingResolver` | `crates/oneai-rag/src/provider_adapter.rs` |
| embedding impls | `crates/oneai-rag/src/embedding.rs` |
| `DocumentIndex` dual-backend + `AutoEmbeddingDocumentIndex` | `crates/oneai-rag/src/index.rs` |
| hybrid retrieval / recall | `crates/oneai-rag/src/retrieval.rs` |
| input split / `ChunkingStrategy` | `crates/oneai-rag/src/chunk_split.rs` |
| default retrieval stack | `crates/oneai-vector/src/pipeline.rs` etc. |

## Related CLI

[`embed generate / batch / list / health / dimension`](cli-reference_EN.md#embedding-service).

## Further reading

- [CLAUDE.md — RAG / Network proxy](../CLAUDE.md)
- Integration with the memory system — see [Memory mechanism](memory-mechanism_EN.md)
