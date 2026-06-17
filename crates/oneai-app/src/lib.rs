//! # OneAI App
//!
//! Application integration layer — wires all modules together.
//!
//! The oneai-app crate provides:
//! - `AppBuilder`: Assembly point for all components (provider, tools, memory, RAG, etc.)
//! - `App`: Fully wired application with shared resources
//! - `AppSession`: Active conversation session with isolated memory

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


pub mod builder;
pub mod session;

pub use builder::*;
pub use session::*;

#[cfg(test)]
mod e2e_tests;