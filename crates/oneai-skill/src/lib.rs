//! # OneAI Skill
//!
//! SKILL system with progressive disclosure and lightweight selector.
//! Includes built-in preset skills for coding, research, and general domains.

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


pub mod builtin;
pub mod registry;
pub mod selector;

pub use builtin::*;
pub use registry::*;
pub use selector::*;