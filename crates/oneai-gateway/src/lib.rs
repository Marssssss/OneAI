//! # OneAI Gateway — message-platform bridge
//!
//! OneAI's native apps are single-process UI clients. The gateway turns OneAI
//! into a *reachable* agent: a Feishu bot, a WeChat Official Account, or any
//! platform that pushes events to an HTTP webhook. Inbound messages drive a
//! real `AgentLoop` turn; the agent's `final_answer` is sent back over the
//! platform's REST API.
//!
//! ## Architecture
//!
//! `oneai-gateway` is a **pure protocol crate** with **no `oneai-*` deps** —
//! it sits *below* `oneai-app` exactly like `oneai-studio` / `oneai-supervisor`.
//! It cannot hold an `App`/`AppSession` or call `run_agent` directly, so it
//! defines the [`GatewayRunner`] trait; the CLI (`examples/cli/cmd_gateway`)
//! builds a real `App`, calls `create_session_with_id` + `run_agent_silent`,
//! and supplies the impl. Per inbound message the gateway resolves (or mints)
//! the channel's bound session id via [`ChannelDirectory`], hands the task to
//! the runner, and relays the reply via the [`MessagePlatform`] adapter.
//!
//! Adapters are platform implementations of [`MessagePlatform`]:
//! - [`adapters::loopback`] — in-process, zero deps, CI-testable.
//! - `adapters::feishu` (feature `feishu`) — Feishu/Lark event webhook + send.
//! - `adapters::wechat` (feature `wechat`) — WeChat OA signature handshake + send.
//!
//! The HTTP webhook surface lives behind the default `axum-webhook` feature,
//! reused by Phase 3.2 (cron) and 3.5 (A2A server axum).
//!
//! ## Stability
//!
//! Public enums carry `#[non_exhaustive]` per the v0.2.0+ stability commitment.

pub mod directory;
pub mod error;
pub mod event;
pub mod gateway;
pub mod platform;
pub mod profile;
pub mod runner;

#[cfg(feature = "axum-webhook")]
pub mod web;

pub mod adapters;

pub use directory::ChannelDirectory;
pub use error::{GatewayError, Result};
pub use event::{ChannelId, MessageEvent, Sender, SessionSource};
pub use gateway::Gateway;
pub use platform::{MessagePlatform, PlatformRegistry};
pub use profile::{ProfileRoute, RouteEntry};
pub use runner::{GatewayRunner, ReplySink, TurnOutcome};

tokio::task_local! {
    /// The originating channel/session context for the current task tree.
    /// Set by [`Gateway::handle_inbound`] so downstream code (hooks, tools)
    /// can read which platform/channel/user originated the turn — preventing
    /// cross-channel context bleed under concurrent multiplexing.
    pub static SESSION_SOURCE: SessionSource;
}
