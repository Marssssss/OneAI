//! # OneAI Domain
//!
//! Domain Pack system — pluggable, declarative, composable domain configuration for OneAI agents.
//!
//! A DomainPack encapsulates the 5 layers of domain-specific workflow embedding:
//! 1. **Tools + ToolDecorators**: Domain-specific tool set and description overrides
//! 2. **ContextSources**: Domain-specific environment sensing with refresh policies
//! 3. **PermissionProfile**: Domain-specific permission classification (deny/auto/confirm)
//! 4. **ParadigmStrategies**: Domain-specific task → paradigm mapping
//! 5. **CompressionTemplate**: Domain-specific context preservation priorities
//!
//! The key insight: "Coding Agent embeds workflow via 5-layer implicit configuration.
//! OneAI makes these 5 layers declarative, pluggable, and composable."
//!
//! Usage:
//! ```ignore
//! // From Rust code:
//! let app = AppBuilder::new()
//!     .provider(provider)
//!     .domain_pack(coding_pack("/project/dir"))  // ← one-line domain switch
//!     .build()?;
//!
//! // From YAML/TOML config file:
//! let app = AppBuilder::new()
//!     .provider(provider)
//!     .domain_pack(domain_pack_from_dir("/project/dir")?)  // ← auto-detect config
//!     .build()?;
//! ```

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


pub mod domain_pack;
pub mod context_source;
pub mod permission_profile;
pub mod paradigm_strategy;
pub mod compression_template;
pub mod tool_decorator;
pub mod merge;
pub mod coding_pack;
pub mod research_pack;
pub mod builtin_sources;
pub mod config_parser;
pub mod repo_map;
pub mod market;
pub mod spec;
pub mod validator;
pub mod spec_file;

pub use domain_pack::*;
pub use paradigm_strategy::{SubAgentTypeDefinition, SubAgentMergeStrategy};
pub use context_source::*;
pub use permission_profile::*;
pub use paradigm_strategy::*;
pub use compression_template::*;
pub use tool_decorator::*;
pub use merge::*;
pub use coding_pack::*;
pub use research_pack::*;
pub use builtin_sources::*;
pub use config_parser::*;
pub use repo_map::*;
pub use market::*;
pub use spec::*;
pub use validator::{ValidationSeverity, ValidationIssue, ValidationResult, DomainPackValidator};
pub use spec_file::DomainPackSpecFile;
