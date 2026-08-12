//! # oneai-bus — the unified engine↔frontend protocol.
//!
//! Every frontend (TUI, web/Studio, native macOS/Windows/iOS/Android, IDE
//! plugin, test harness) is a *Directive writer + Yield reader* over a single
//! [`EngineBus`]. This collapses the three parallel wires OneAI had
//! (`oneai-studio` WS broadcast, `oneai-a2a` JSON-RPC+SSE, `oneai-supervisor`
//! newline-JSON IPC) plus the in-process TUI direct-drive into one protocol.
//!
//! ## Naming (intentionally not codex's Op/Event)
//!
//! codex uses `Op`/`Event` + `submission`/`event` queues. This crate uses:
//!
//! | concept | name | rationale |
//! |---|---|---|
//! | inbound unit (frontend → engine) | [`Directive`] | a control-flow instruction the engine must act on; avoids `Command` (CLI), `Request` (`InteractionRequest`), `Intent` (Android) |
//! | outbound unit (engine → frontend) | [`EngineYield`] | control-flow pair to Directive — what the engine yields back; avoids `Event` (codex + `TaskEvent`), `Signal` (unix), `Emission` (Chinese "排放物" connotation). Note: `yield` is a Rust reserved-for-future keyword, so the enum type is `EngineYield` while the channel/fields stay `yield` |
//! | inbound channel | `directive` (bounded 512) | codex's submission |
//! | outbound channel | `yield` (broadcast 1024) | codex's event |
//!
//! ## Approval correlation
//!
//! An [`EngineYield::ApprovalRequest`] carries a `request_id`; the matching
//! [`Directive::Approve`] with the same id resolves the blocked
//! [`EngineBus::request_approval`] call. This unifies approval onto the two
//! channels — no separate approval mpsc (replaces `ChannelInteractionGate`'s
//! ad-hoc per-request oneshot surface for bus consumers).
//!
//! ## Stability
//!
//! Both enums are `#[non_exhaustive]`; new variants may be added in a minor
//! version without breaking consumers (per the v0.2.0 / 1.x API-stability
//! commitment, P3-1). Wire consumers must handle unknown variants gracefully.

#![forbid(unsafe_code)]

pub mod bus;
pub mod protocol;
pub mod wire;

pub use bus::{EngineBus, InProcessBus};
pub use protocol::{
    BusParadigmKind, BusSubAgent, BusSubAgentKind, BusToolCall, BusTurnSummary, BusUsageRecord,
    Directive, EngineYield,
};
pub use wire::{parse_directive, parse_yield, serialize_directive, serialize_yield};

use thiserror::Error;

/// Crate-local result alias.
pub type Result<T> = std::result::Result<T, BusError>;

/// Errors raised by the engine bus.
#[derive(Debug, Error)]
pub enum BusError {
    /// The bus was shut down — channels closed.
    #[error("engine bus closed")]
    Closed,
    /// A directive was submitted that the bus does not accept in the current
    /// state (e.g. an `Approve` for an unknown / already-resolved request_id).
    #[error("directive not acceptable in current state: {0}")]
    NotAcceptable(String),
    /// A serialized frame did not decode.
    #[error("wire codec error: {0}")]
    Codec(String),
}
