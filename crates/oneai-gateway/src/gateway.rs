//! The gateway host — multiplexes inbound [`MessageEvent`]s across platforms.
//!
//! [`Gateway::handle_inbound`] is the platform-agnostic core: resolve (or
//! mint) the channel's session id, set the `task_local` [`SessionSource`], hand
//! the task to the [`GatewayRunner`], and relay the reply via the
//! [`MessagePlatform`] adapter (segmenting long replies at the platform's
//! `max_text_length`). The HTTP webhook surface ([`crate::web`]) and the
//! adapters ([`crate::adapters`]) both funnel through this method.

use std::sync::Arc;

use tracing::{debug, warn};

use crate::directory::ChannelDirectory;
use crate::error::{GatewayError, Result};
use crate::event::{MessageEvent, SessionSource};
use crate::platform::PlatformRegistry;
use crate::profile::ProfileRoute;
use crate::runner::{GatewayRunner, TurnOutcome};
use crate::SESSION_SOURCE;

/// The gateway host. Held by the CLI (and the axum server state); constructed
/// once with a runner impl, a platform registry, a channel directory, and a
/// profile router.
pub struct Gateway {
    runner: Arc<dyn GatewayRunner>,
    platforms: PlatformRegistry,
    directory: ChannelDirectory,
    profile: ProfileRoute,
}

impl Gateway {
    pub fn new(
        runner: Arc<dyn GatewayRunner>,
        platforms: PlatformRegistry,
        directory: ChannelDirectory,
        profile: ProfileRoute,
    ) -> Self {
        Self {
            runner,
            platforms,
            directory,
            profile,
        }
    }

    /// Read-only access for the CLI / webhook server state.
    pub fn platforms(&self) -> &PlatformRegistry {
        &self.platforms
    }

    pub fn directory(&self) -> &ChannelDirectory {
        &self.directory
    }

    pub fn profile(&self) -> &ProfileRoute {
        &self.profile
    }

    pub fn runner(&self) -> &Arc<dyn GatewayRunner> {
        &self.runner
    }

    /// Process one inbound message end-to-end.
    ///
    /// 1. Resolve (mint) the channel's session id.
    /// 2. Record the pack the profile routes to (logged only — first cut uses
    ///    the single configured pack; see [`crate::profile`]).
    /// 3. Set `SESSION_SOURCE` task-local for the turn.
    /// 4. Hand the task to the runner; relay the reply via the adapter,
    ///    segmented to the platform's `max_text_length`.
    pub async fn handle_inbound(&self, event: MessageEvent) -> Result<()> {
        let platform_name = event.channel.platform.clone();
        let channel_raw = event.channel.raw.clone();
        let user_id = event.sender.id.clone();

        // Resolve / mint the session id for this channel.
        let session_id = self
            .directory
            .resolve_or_mint(&event.channel, Some(&user_id))
            .await?;

        // Profile routing — first cut: log the resolved pack name. The runner
        // uses the single configured pack; per-channel pack switching is a
        // follow-up (evolution-plan §3.1).
        let pack = self.profile.resolve(&event.channel);
        debug!(
            platform = %platform_name, channel = %channel_raw,
            session_id = %session_id, pack = %pack,
            "inbound message routed"
        );

        let source = SessionSource {
            platform: platform_name.clone(),
            channel: channel_raw.clone(),
            session_id: session_id.clone(),
            user_id: user_id.clone(),
        };
        let task_text = event.text.clone();
        let runner = self.runner.clone();

        // Drive the turn under the task-local session source so downstream
        // code (hooks/tools) can read the originating channel.
        let outcome = SESSION_SOURCE
            .scope(source, async move {
                runner.run_turn(&session_id, &task_text).await
            })
            .await;

        let reply = match &outcome {
            TurnOutcome::Done { final_answer, .. } => final_answer.clone(),
            TurnOutcome::Rejected { reason } => {
                warn!(platform = %platform_name, reason = %reason, "turn rejected");
                // Surface a friendly note so the user isn't left without a reply.
                format!("[oneai] 未能处理: {}", reason)
            }
            TurnOutcome::Error { message } => {
                warn!(platform = %platform_name, message = %message, "turn error");
                format!("[oneai] 处理出错: {}", message)
            }
        };

        if reply.is_empty() {
            debug!(platform = %platform_name, "empty reply — not sending");
            return Ok(());
        }

        // Relay via the platform adapter.
        let platform = self
            .platforms
            .get(&platform_name)
            .ok_or_else(|| GatewayError::UnknownPlatform(platform_name.clone()))?;

        if !platform.supports_reply() {
            debug!(platform = %platform_name, "platform can't reply — dropping");
            return Ok(());
        }

        let max_len = platform.max_text_length().max(1);
        for chunk in segment_text(&reply, max_len) {
            platform.send(&event.channel, chunk).await?;
        }
        Ok(())
    }
}

/// Segment `text` into chunks ≤ `max_len`, preferring paragraph (`\n\n`) and
/// newline boundaries, falling back to hard cut. Never returns an empty chunk.
fn segment_text(text: &str, max_len: usize) -> Vec<&str> {
    if text.len() <= max_len {
        return vec![text];
    }
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if rest.len() <= max_len {
            out.push(rest);
            break;
        }
        // Try to cut at the last paragraph break within the window.
        let window = &rest[..max_len];
        let cut = window
            .rfind("\n\n")
            .map(|i| i + 2)
            .or_else(|| window.rfind('\n').map(|i| i + 1))
            .unwrap_or(max_len);
        // Guard against pathological 0-cut (e.g. window starts with \n\n).
        let cut = cut.max(1).min(rest.len());
        out.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{ChannelId, MessageEvent, Sender};
    use crate::platform::{MessagePlatform, PlatformRegistry};
    use crate::profile::ProfileRoute;
    use crate::runner::{GatewayRunner, TurnOutcome};
    use async_trait::async_trait;
    use std::sync::Mutex;

    // ─── Loopback runner that echoes the task back ──────────────────────────
    struct EchoRunner;
    #[async_trait]
    impl GatewayRunner for EchoRunner {
        async fn run_turn(&self, _session_id: &str, task: &str) -> TurnOutcome {
            TurnOutcome::Done {
                final_answer: format!("echo: {}", task),
                completed: true,
                iterations: 1,
            }
        }
    }

    // ─── Capturing loopback platform ─────────────────────────────────────────
    struct CapturePlatform {
        sent: Mutex<Vec<String>>,
        max_len: usize,
    }
    impl CapturePlatform {
        fn new(max_len: usize) -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                max_len,
            }
        }
        fn take(&self) -> Vec<String> {
            std::mem::take(&mut *self.sent.lock().unwrap())
        }
    }
    #[async_trait]
    impl MessagePlatform for CapturePlatform {
        fn name(&self) -> &str {
            "loopback"
        }
        fn max_text_length(&self) -> usize {
            self.max_len
        }
        async fn send(&self, _channel: &ChannelId, text: &str) -> Result<()> {
            self.sent.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

    fn gateway_with(max_len: usize) -> (Gateway, Arc<CapturePlatform>) {
        let plat = Arc::new(CapturePlatform::new(max_len));
        let mut reg = PlatformRegistry::new();
        reg.register(plat.clone());
        let gw = Gateway::new(
            Arc::new(EchoRunner),
            reg,
            ChannelDirectory::in_memory(),
            ProfileRoute::new("coding"),
        );
        (gw, plat)
    }

    #[tokio::test]
    async fn handle_inbound_relays_reply() {
        let (gw, plat) = gateway_with(4000);
        let ev = MessageEvent::new(
            ChannelId::new("loopback", "c1"),
            Sender::anonymous("u1"),
            "hello",
        );
        gw.handle_inbound(ev).await.unwrap();
        assert_eq!(plat.take(), vec!["echo: hello"]);
    }

    #[tokio::test]
    async fn session_id_stable_across_messages() {
        // First message mints a session; second resolves to the same one.
        let (gw, _plat) = gateway_with(4000);
        let ch = ChannelId::new("loopback", "stable");
        gw.handle_inbound(MessageEvent::new(ch.clone(), Sender::anonymous("u"), "a"))
            .await
            .unwrap();
        let b1 = gw.directory().get(&ch).await.unwrap();
        gw.handle_inbound(MessageEvent::new(ch.clone(), Sender::anonymous("u"), "b"))
            .await
            .unwrap();
        let b2 = gw.directory().get(&ch).await.unwrap();
        assert_eq!(b1.session_id, b2.session_id);
    }

    #[tokio::test]
    async fn long_reply_segmented() {
        let (gw, plat) = gateway_with(20);
        let long = "echo: ".to_string() + &"a".repeat(50);
        let ev = MessageEvent::new(
            ChannelId::new("loopback", "c2"),
            Sender::anonymous("u"),
            "a".repeat(50),
        );
        gw.handle_inbound(ev).await.unwrap();
        let sent = plat.take();
        // Every chunk ≤ max_len, and concatenation reproduces the reply.
        for c in &sent {
            assert!(c.len() <= 20, "chunk over limit: {} bytes", c.len());
        }
        assert_eq!(sent.concat(), long);
    }

    #[tokio::test]
    async fn session_source_visible_in_runner() {
        // The runner reads the task-local SESSION_SOURCE to verify the
        // originating channel propagated.
        use std::sync::OnceLock;
        static SEEN: OnceLock<Mutex<Option<SessionSource>>> = OnceLock::new();
        let _ = SEEN.set(Mutex::new(None));
        struct PeekingRunner;
        #[async_trait]
        impl GatewayRunner for PeekingRunner {
            async fn run_turn(&self, _sid: &str, _task: &str) -> TurnOutcome {
                let s = SESSION_SOURCE.with(|src| src.clone());
                SEEN.get().unwrap().lock().unwrap().replace(s);
                TurnOutcome::Done {
                    final_answer: "ok".into(),
                    completed: true,
                    iterations: 1,
                }
            }
        }
        let plat = Arc::new(CapturePlatform::new(4000));
        let mut reg = PlatformRegistry::new();
        reg.register(plat);
        let gw = Gateway::new(
            Arc::new(PeekingRunner),
            reg,
            ChannelDirectory::in_memory(),
            ProfileRoute::new("coding"),
        );
        gw.handle_inbound(MessageEvent::new(
            ChannelId::new("loopback", "peek"),
            Sender::anonymous("userX"),
            "hi",
        ))
        .await
        .unwrap();
        let seen = SEEN.get().unwrap().lock().unwrap().clone().unwrap();
        assert_eq!(seen.platform, "loopback");
        assert_eq!(seen.channel, "peek");
        assert_eq!(seen.user_id, "userX");
    }
}
