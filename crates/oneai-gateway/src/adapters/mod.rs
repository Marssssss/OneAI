//! Platform adapters — implementations of [`crate::MessagePlatform`].
//!
//! - [`loopback`] — in-process, zero external deps, CI-testable. The default
//!   adapter for local dev and the end-to-end webhook smoke test.
//! - `feishu` (feature `feishu`) — Feishu/Lark bot.
//! - `wechat` (feature `wechat`) — WeChat Official Account.

pub mod loopback;

#[cfg(feature = "feishu")]
pub mod feishu;

#[cfg(feature = "feishu")]
pub mod feishu_pb;

#[cfg(feature = "feishu")]
pub mod feishu_ws;

#[cfg(feature = "wechat")]
pub mod wechat;

pub use loopback::LoopbackPlatform;
