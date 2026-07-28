//! # OneAI Provider
//!
//! LLM provider implementations (OpenAI-compatible, Anthropic Claude, Google Gemini, Ollama)
//! and cost-based model routing.

//! # Stability
//!
//! This crate follows the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/).
//! All public enums are annotated `#[non_exhaustive]` where appropriate to prevent
//! downstream breakage when new variants are added. Structs use constructor methods
//! for creation — direct struct literal construction is supported within this crate
//! but may be restricted in future versions via `#[non_exhaustive]`.
//!
//! Breaking changes will be signaled by a minor version bump (0.x → 0.y).
//! Patch versions (0.x.y → 0.x.z) are always backward-compatible.

pub mod anthropic;
pub mod gemini;
pub mod model_router;
pub mod ollama;
pub mod openai;
pub mod provider_factory;
pub mod provider_pool;
pub mod retry;
pub mod smart_router;

pub use anthropic::AnthropicProvider;
pub use gemini::GeminiProvider;
pub use model_router::{ModelRouter, RouteDecision, RouteProviderKind, RouteRule};
pub use ollama::OllamaProvider;
pub use oneai_core::ContextFitResult;
pub use oneai_core::ContextManager;
pub use oneai_core::ContextManagerConfig;
pub use oneai_core::ContextTrimmingStrategy;
pub use oneai_core::ContextWindowProfile;
pub use oneai_core::HeuristicTokenCounter;
pub use oneai_core::TokenCounter;
pub use openai::OpenAIProvider;
pub use provider_factory::ProviderFactory;
pub use provider_pool::{ProviderEntry, ProviderPool};
pub use retry::ProviderRetryConfig;
pub use smart_router::SmartRouter;
