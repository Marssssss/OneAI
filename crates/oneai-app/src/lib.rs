//! # OneAI App
//!
//! Application integration layer — wires all modules together.
//!
//! The oneai-app crate provides:
//! - `AppBuilder`: Assembly point for all components (provider, tools, memory, RAG, etc.)
//! - `App`: Fully wired application with shared resources
//! - `AppSession`: Active conversation session with isolated memory

pub mod builder;
pub mod session;

pub use builder::*;
pub use session::*;

#[cfg(test)]
mod e2e_tests;