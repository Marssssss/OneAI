//! # OneAI Provider
//!
//! LLM provider implementations (OpenAI-compatible, Anthropic Claude, Google Gemini, Ollama)
//! and cost-based model routing.

pub mod openai;
pub mod anthropic;
pub mod gemini;
pub mod ollama;
pub mod provider_factory;
pub mod model_router;

pub use openai::OpenAIProvider;
pub use anthropic::AnthropicProvider;
pub use gemini::GeminiProvider;
pub use ollama::OllamaProvider;
pub use provider_factory::ProviderFactory;
pub use model_router::{ModelRouter, RouteRule, RouteDecision, RouteProviderKind};