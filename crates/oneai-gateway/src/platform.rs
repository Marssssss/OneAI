//! The platform-adapter trait + registry.
//!
//! [`MessagePlatform`] is the seam a platform (Feishu / WeChat / loopback)
//! implements. Capability flags are **default trait methods** — the key
//! portability lesson: a platform that can't do something returns `false` and
//! the gateway core adapts (e.g. skips media, segments long text) instead of
//! the adapter having to implement every method.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::Result;
use crate::event::ChannelId;

/// A message-platform adapter: receive-native → [`crate::MessageEvent`],
/// and send OneAI's reply back over the platform's REST API.
///
/// Inbound *reception* (webhook push or polling) is the adapter's own concern —
/// the gateway core only calls [`MessagePlatform::send`] for replies and reads
/// capability flags to shape the reply (segment long text, skip media).
#[async_trait]
pub trait MessagePlatform: Send + Sync {
    /// Registered platform name (`"feishu"` / `"wechat"` / `"loopback"`).
    fn name(&self) -> &str;

    // ─── Capability flags (default trait methods — portability seam) ──────

    /// Whether the platform pushes events to us over a webhook (vs. the
    /// adapter needing to long-poll). Default `false`.
    fn supports_inbound_push(&self) -> bool {
        false
    }

    /// Whether the adapter can post replies back. Default `true`.
    fn supports_reply(&self) -> bool {
        true
    }

    /// Whether the platform accepts media (images/files) in replies. Default
    /// `false` — most-first-cut adapters are text-only.
    fn supports_media(&self) -> bool {
        false
    }

    /// Max text length per message the platform accepts. The gateway segments
    /// replies that exceed this. Feishu ≈ 3072, WeChat ≈ 600 (OA customer
    /// service message). Default conservative `4000`.
    fn max_text_length(&self) -> usize {
        4000
    }

    // ─── Outbound ─────────────────────────────────────────────────────────

    /// Send `text` to the channel. Called by the gateway after each turn.
    async fn send(&self, channel: &ChannelId, text: &str) -> Result<()>;

    /// Connect / start polling, for adapters that long-poll rather than
    /// receive webhook pushes. Default no-op (Feishu/WeChat use webhooks).
    async fn connect(&self) -> Result<()> {
        Ok(())
    }
}

/// Lazy-loaded registry of platform adapters, keyed by [`MessagePlatform::name`].
#[derive(Default)]
pub struct PlatformRegistry {
    map: HashMap<String, Arc<dyn MessagePlatform>>,
}

impl PlatformRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an adapter. Replaces an existing adapter with the same name.
    pub fn register(&mut self, platform: Arc<dyn MessagePlatform>) {
        self.map.insert(platform.name().to_string(), platform);
    }

    /// Look up an adapter by platform name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn MessagePlatform>> {
        self.map.get(name)
    }

    /// Whether any adapter is registered for `name`.
    pub fn contains(&self, name: &str) -> bool {
        self.map.contains_key(name)
    }

    /// Names of all registered platforms.
    pub fn names(&self) -> Vec<String> {
        self.map.keys().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }
}
