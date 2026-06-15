//! # OneAI Skill
//!
//! SKILL system with progressive disclosure and lightweight selector.
//! Includes built-in preset skills for coding, research, and general domains.

pub mod builtin;
pub mod registry;
pub mod selector;

pub use builtin::*;
pub use registry::*;
pub use selector::*;