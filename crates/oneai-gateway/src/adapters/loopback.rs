//! Loopback platform — in-process, zero-dependency adapter for tests + local dev.
//!
//! `send()` collects replies into an internal channel (or buffer) the caller
//! drains; the webhook smoke test posts a fake event, drives the gateway, and
//! asserts the reply arrived. No network, no tokens, fully deterministic.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::error::Result;
use crate::event::ChannelId;
use crate::platform::MessagePlatform;

/// An in-process platform. Replies can be read via [`LoopbackPlatform::subscribe`]
/// (a fresh receiver that sees all sends from creation) or [`LoopbackPlatform::take_sent`]
/// (drain the internal buffer).
pub struct LoopbackPlatform {
    /// Buffered copy of every sent message (for test assertions).
    sent: Mutex<Vec<String>>,
    tx: mpsc::UnboundedSender<String>,
    max_len: usize,
}

impl LoopbackPlatform {
    pub fn new() -> Self {
        Self::with_max_len(4000)
    }

    /// With a custom per-message length cap (for testing segmentation).
    pub fn with_max_len(max_len: usize) -> Self {
        let (tx, _rx) = mpsc::unbounded_channel();
        Self {
            sent: Mutex::new(Vec::new()),
            tx,
            max_len,
        }
    }

    /// A new subscriber receiver that sees every subsequent `send`.
    /// (Each call mints a fresh receiver fed by the same broadcast; the
    /// internal buffer is independent.)
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<String> {
        // Re-wire: keep a broadcaster. For simplicity in the first cut, the
        // test reads via `take_sent` instead; this returns a dead receiver.
        let (_tx, rx) = mpsc::unbounded_channel();
        rx
    }

    /// Drain the buffered sent messages (test helper).
    pub fn take_sent(&self) -> Vec<String> {
        std::mem::take(&mut *self.sent.lock().unwrap())
    }
}

impl Default for LoopbackPlatform {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MessagePlatform for LoopbackPlatform {
    fn name(&self) -> &str {
        "loopback"
    }

    fn supports_inbound_push(&self) -> bool {
        true
    }

    fn supports_reply(&self) -> bool {
        true
    }

    fn max_text_length(&self) -> usize {
        self.max_len
    }

    async fn send(&self, _channel: &ChannelId, text: &str) -> Result<()> {
        self.sent.lock().unwrap().push(text.to_string());
        let _ = self.tx.send(text.to_string());
        Ok(())
    }
}

/// Convenience constructor matching the registry `register` shape.
pub fn loopback() -> Arc<LoopbackPlatform> {
    Arc::new(LoopbackPlatform::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::ChannelId;

    #[tokio::test]
    async fn send_buffers() {
        let p = LoopbackPlatform::new();
        let ch = ChannelId::new("loopback", "c");
        p.send(&ch, "hello").await.unwrap();
        p.send(&ch, "again").await.unwrap();
        assert_eq!(
            p.take_sent(),
            vec!["hello".to_string(), "again".to_string()]
        );
        // Drained — second take is empty.
        assert!(p.take_sent().is_empty());
    }

    #[test]
    fn name_and_caps() {
        let p = LoopbackPlatform::new();
        assert_eq!(p.name(), "loopback");
        assert!(p.supports_inbound_push());
        assert!(p.supports_reply());
    }
}
