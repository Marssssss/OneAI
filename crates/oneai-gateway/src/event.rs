//! Inbound message + session-origin types shared across all platform adapters.
//!
//! These are platform-agnostic: every adapter (loopback / Feishu / WeChat)
//! parses its native event into a [`MessageEvent`], and the gateway core never
//! touches platform-specific shapes. [`SessionSource`] is carried through the
//! turn via the `task_local!` in [`crate::SESSION_SOURCE`] so downstream code
//! knows which channel originated the task.

use serde::{Deserialize, Serialize};

/// A channel identity: the platform name + its raw channel id on that platform.
///
/// `raw` is the platform-native receive id — Feishu `chat_id`, WeChat
/// `openid`/`unionid`, loopback test handle.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChannelId {
    /// Platform name as registered in [`crate::PlatformRegistry`] (e.g.
    /// `"feishu"`, `"wechat"`, `"loopback"`).
    pub platform: String,
    /// Platform-native channel/receive id.
    pub raw: String,
}

impl ChannelId {
    pub fn new(platform: impl Into<String>, raw: impl Into<String>) -> Self {
        Self {
            platform: platform.into(),
            raw: raw.into(),
        }
    }

    /// A stable composite key for maps (platform + NUL + raw).
    pub fn key(&self) -> String {
        format!("{}\u{0}{}", self.platform, self.raw)
    }
}

/// A sender on a platform — the user id as the platform reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sender {
    /// Platform-native user id (Feishu `open_id`/`user_id`, WeChat `openid`).
    pub id: String,
    /// Optional display name as reported by the platform.
    pub name: Option<String>,
}

impl Sender {
    pub fn anonymous(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: None,
        }
    }
}

/// A normalized inbound message event, platform-agnostic.
///
/// Produced by an adapter from the platform's native event shape; consumed by
/// [`crate::Gateway::handle_inbound`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessageEvent {
    /// Where the message arrived from (platform + channel id).
    pub channel: ChannelId,
    /// Who sent it on the platform.
    pub sender: Sender,
    /// The message text to hand to the agent.
    pub text: String,
    /// The platform-native event payload, kept verbatim for adapter reference
    /// (e.g. to extract a `msg_id` for reply threading). Defaults to null when
    /// omitted (e.g. a loopback test event).
    #[serde(default)]
    pub raw: serde_json::Value,
    /// The id of the message being replied to, if any.
    #[serde(default)]
    pub reply_to: Option<String>,
}

impl MessageEvent {
    pub fn new(channel: ChannelId, sender: Sender, text: impl Into<String>) -> Self {
        Self {
            channel,
            sender,
            text: text.into(),
            raw: serde_json::Value::Null,
            reply_to: None,
        }
    }

    /// Convenience for tests / loopback: anonymous sender on a channel.
    pub fn loopback(channel_raw: &str, text: impl Into<String>) -> Self {
        Self::new(
            ChannelId::new("loopback", channel_raw),
            Sender::anonymous(channel_raw),
            text,
        )
    }
}

/// The originating context of an inbound turn, carried via
/// [`crate::SESSION_SOURCE`] through the task tree.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSource {
    pub platform: String,
    pub channel: String,
    /// The internal OneAI session id this channel is bound to.
    pub session_id: String,
    /// The platform-native sender id.
    pub user_id: String,
    /// The DomainPack name this channel is bound to (§3.1 tail #1: per-channel
    /// whole-pack switching). Locked at first mint — the runner reads this
    /// task-local to pick the lazily-built App for that pack.
    #[serde(default)]
    pub pack: String,
}
