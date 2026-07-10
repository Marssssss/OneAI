//! Thin staticlib wrapper over `oneai-uniffi`.
//!
//! Its only job is to produce `liboneai.a` (the static archive the Apple
//! xcframework and the HarmonyOS NAPI module link against) — WITHOUT forcing
//! `cargo build` / `cargo test` (debug) to also emit that ~900 MB archive.
//! Keep `oneai-uniffi`'s crate-type as `["cdylib","lib"]`; build this crate
//! explicitly only when packaging a native lib (`scripts/build_apple.sh`,
//! `scripts/build_harmony.sh`).
//!
//! The re-export pulls the uniffi + c_facade `#[no_mangle] extern "C"` symbols
//! into the archive (exported symbols are retained in a staticlib). The Rust
//! crate name is `oneai` (oneai-uniffi's `[lib] name = "oneai"`).

pub use oneai::*;
