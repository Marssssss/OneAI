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


pub mod openai;
pub mod anthropic;
pub mod gemini;
pub mod ollama;
pub mod provider_factory;
pub mod model_router;
pub mod provider_pool;
pub mod smart_router;

pub use openai::OpenAIProvider;
pub use anthropic::AnthropicProvider;
pub use gemini::GeminiProvider;
pub use ollama::OllamaProvider;
pub use provider_factory::ProviderFactory;
pub use model_router::{ModelRouter, RouteRule, RouteDecision, RouteProviderKind};
pub use provider_pool::{ProviderPool, ProviderEntry};
pub use smart_router::SmartRouter;
pub use oneai_core::TokenCounter;
pub use oneai_core::HeuristicTokenCounter;
pub use oneai_core::ContextManager;
pub use oneai_core::ContextManagerConfig;
pub use oneai_core::ContextTrimmingStrategy;
pub use oneai_core::ContextWindowProfile;
pub use oneai_core::ContextFitResult;
pub use oneai_core::TeamStrategy;
pub use oneai_core::TeamConfig;
pub use oneai_core::TeamResult;
pub use oneai_core::AgentRole;
pub use oneai_core::AgentResultEntry;
pub use oneai_core::SubAgentKindProxy;
pub use oneai_core::TokenBudgetProxy;
pub use oneai_core::TeamCoordinationLog;
pub use oneai_core::InMemoryTeamCoordinationLog;
pub use oneai_core::TeamCoordinationEvent;
pub use oneai_core::TeamCoordinationEventKind;
pub use oneai_core::TeamPresets;
pub use oneai_core::HandoffConfig;
pub use oneai_core::HandoffTarget;
pub use oneai_core::HandoffEvent;
pub use oneai_core::HandoffResult;
pub use oneai_core::HandoffChainEntry;
pub use oneai_core::HandoffLog;
pub use oneai_core::InMemoryHandoffLog;
pub use oneai_core::HandoffPresets;
pub use oneai_core::AgentCapability;
pub use oneai_core::SwarmConfig;
pub use oneai_core::SwarmRouting;
pub use oneai_core::SwarmTask;
pub use oneai_core::SwarmResult;
pub use oneai_core::SwarmTaskResult;
pub use oneai_core::SwarmAgentEntry;
pub use oneai_core::SwarmCoordinationLog;
pub use oneai_core::InMemorySwarmCoordinationLog;
pub use oneai_core::SwarmCoordinationEvent;
pub use oneai_core::SwarmCoordinationEventKind;
pub use oneai_core::SwarmPresets;