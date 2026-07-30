//! The gateway host — multiplexes inbound [`MessageEvent`]s across platforms.
//!
//! [`Gateway::handle_inbound`] is the platform-agnostic core: resolve (or
//! mint) the channel's session id, set the `task_local` [`SessionSource`], hand
//! the task to the [`GatewayRunner`], and relay the reply via the
//! [`MessagePlatform`] adapter (segmenting long replies at the platform's
//! `max_text_length`). The HTTP webhook surface ([`crate::web`]) and the
//! adapters ([`crate::adapters`]) both funnel through this method.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tracing::{debug, warn};

use crate::directory::ChannelDirectory;
use crate::error::{GatewayError, Result};
use crate::event::{ChannelId, MessageEvent, SessionSource};
use crate::platform::{MessagePlatform, PlatformRegistry};
use crate::profile::ProfileRoute;
use crate::runner::{GatewayRunner, ReplySink, TurnOutcome};
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

        // Profile routing — resolve the pack for this channel (§3.1 tail #1:
        // per-channel whole-pack switching).
        let pack = self.profile.resolve(&event.channel);

        // Resolve / mint the session id; the pack is locked into the binding
        // at first mint (see resolve_or_mint).
        let session_id = self
            .directory
            .resolve_or_mint(&event.channel, Some(&user_id), &pack)
            .await?;

        // Effective pack = the binding's locked pack (empty for legacy
        // bindings created before the `pack` field existed), falling back to
        // the freshly resolved one.
        let effective_pack = self
            .directory
            .get(&event.channel)
            .await
            .and_then(|b| {
                if b.pack.is_empty() {
                    None
                } else {
                    Some(b.pack)
                }
            })
            .unwrap_or_else(|| pack.clone());

        debug!(
            platform = %platform_name, channel = %channel_raw,
            session_id = %session_id, pack = %effective_pack,
            "inbound message routed"
        );

        let platform = self
            .platforms
            .get(&platform_name)
            .ok_or_else(|| GatewayError::UnknownPlatform(platform_name.clone()))?
            .clone();

        let source = SessionSource {
            platform: platform_name,
            channel: channel_raw,
            session_id,
            user_id,
            pack: effective_pack,
        };
        self.run_and_reply(source, platform, event.channel, event.text)
            .await
    }

    /// Deliver a scheduled cron job's task into a *known* channel session
    /// (§3.2 `deliver=origin`) — no mint (the session id is the cron job's
    /// bound origin). Sets `SESSION_SOURCE`, runs the turn (streaming when
    /// supported), and relays the reply over the platform adapter, segmented
    /// to the platform's `max_text_length`. The scheduler's `CronRunner`
    /// impl (CLI) calls this so a cron fire reuses the gateway's `send()`
    /// exactly — one code path for inbound messages and scheduled jobs.
    pub async fn deliver_scheduled(
        &self,
        channel: ChannelId,
        session_id: String,
        pack: String,
        user_id: String,
        task: &str,
    ) -> Result<()> {
        let platform_name = channel.platform.clone();
        let channel_raw = channel.raw.clone();
        let platform = self
            .platforms
            .get(&platform_name)
            .ok_or_else(|| GatewayError::UnknownPlatform(platform_name.clone()))?
            .clone();
        let source = SessionSource {
            platform: platform_name,
            channel: channel_raw,
            session_id,
            user_id,
            pack,
        };
        self.run_and_reply(source, platform, channel, task.to_string())
            .await
    }

    /// Run a turn under `SESSION_SOURCE` and relay the reply to `channel` via
    /// `platform` (segmented; streaming when both sides support it, with a
    /// dedup skip of the final send when chunks were already streamed).
    /// Shared by [`handle_inbound`] (inbound message) and [`deliver_scheduled`]
    /// (cron fire) so the two paths can't diverge.
    async fn run_and_reply(
        &self,
        source: SessionSource,
        platform: Arc<dyn MessagePlatform>,
        channel: ChannelId,
        task_text: String,
    ) -> Result<()> {
        let platform_name = source.platform.clone();
        let session_id = source.session_id.clone();
        let runner = self.runner.clone();

        // Streaming reply (§3.1 tail #3): if the platform accepts streamed
        // chunks and the runner can stream, hand a sink to the runner so the
        // observer's `on_stream_chunk` pushes incremental text to the platform
        // via a background coalescer. The coalescer flushes on its interval;
        // `finalize` drains the tail after the turn ends.
        let should_stream = platform.supports_streaming_reply()
            && runner.supports_streaming()
            && platform.supports_reply();

        let (outcome, streamed) = if should_stream {
            let sink: Arc<dyn ReplySink> =
                StreamingReplySink::new(platform.clone(), channel.clone()).await;
            let sink_for_runner = sink.clone();
            let o = SESSION_SOURCE
                .scope(source, async move {
                    runner
                        .run_turn_streaming(&session_id, &task_text, sink_for_runner)
                        .await
                })
                .await;
            sink.finalize().await;
            (o, sink.did_stream())
        } else {
            let o = SESSION_SOURCE
                .scope(source, async move {
                    runner.run_turn(&session_id, &task_text).await
                })
                .await;
            (o, false)
        };

        // If the reply was streamed, the chunks are already delivered — skip
        // the final segment-send (dedup). Falls through to the normal path
        // when the turn produced no streamed chunks (e.g. rejected/errored).
        if streamed {
            debug!(platform = %platform_name, "reply streamed — skipping final send");
            return Ok(());
        }

        let reply = match &outcome {
            TurnOutcome::Done { final_answer, .. } => final_answer.clone(),
            TurnOutcome::Rejected { reason } => {
                warn!(platform = %platform_name, reason = %reason, "turn rejected");
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

        if !platform.supports_reply() {
            debug!(platform = %platform_name, "platform can't reply — dropping");
            return Ok(());
        }

        let max_len = platform.max_text_length().max(1);
        for chunk in segment_text(&reply, max_len) {
            platform.send(&channel, chunk).await?;
        }
        Ok(())
    }
}

// ─── Streaming reply sink ───────────────────────────────────────────────────

/// A [`ReplySink`] backed by a [`MessagePlatform`] — accumulates chunks pushed
/// by the streaming observer and flushes them to the platform from a
/// background coalescer. The coalescer **does not flush on a time tick**: most
/// platforms (Feishu REST, WeChat) create a *new message bubble* per `send`,
/// so a per-tick flush would split one reply into many bubbles. Instead it
/// flushes only when the buffer exceeds the platform's `max_text_length`
/// (unavoidable segmentation for long replies) and once at `finalize` (sender
/// dropped = turn done) — so a short reply is a single bubble, and a long one
/// is split only as the platform's per-message cap forces.
pub struct StreamingReplySink {
    tx: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>,
    join: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    streamed: Arc<AtomicBool>,
}

impl StreamingReplySink {
    /// Construct + spawn the background coalescer. The coalescer owns the
    /// platform/channel, accumulates pushed chunks, flushes only when the
    /// buffer exceeds `max_text_length`, and drains the remainder once at
    /// finalize (sender drop = turn done).
    pub async fn new(platform: Arc<dyn MessagePlatform>, channel: ChannelId) -> Arc<Self> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let streamed = Arc::new(AtomicBool::new(false));
        let sink = Arc::new(Self {
            tx: std::sync::Mutex::new(Some(tx)),
            join: std::sync::Mutex::new(None),
            streamed: streamed.clone(),
        });

        let max_len = platform.max_text_length().max(1);
        let handle = tokio::spawn(async move {
            let mut buf = String::new();
            while let Some(text) = rx.recv().await {
                streamed.store(true, Ordering::Relaxed);
                buf.push_str(&text);
                // Flush only when the buffer exceeds the platform's
                // per-message cap — otherwise hold everything and flush once
                // at finalize so a short reply is a single bubble.
                if buf.len() >= max_len {
                    flush_send(&platform, &channel, &mut buf).await;
                }
            }
            // Sender dropped (turn done) — drain the remainder in one send.
            if !buf.is_empty() {
                flush_send(&platform, &channel, &mut buf).await;
            }
        });
        *sink.join.lock().unwrap() = Some(handle);
        sink
    }
}

/// Segment `buf` at the platform's `max_text_length` (CJK-safe) and send each
/// chunk, then clear the buffer.
async fn flush_send(platform: &Arc<dyn MessagePlatform>, channel: &ChannelId, buf: &mut String) {
    let max_len = platform.max_text_length().max(1);
    for chunk in segment_text(buf, max_len) {
        if let Err(e) = platform.send(channel, chunk).await {
            warn!(error = %e, "streaming reply: send chunk failed");
            break;
        }
    }
    buf.clear();
}

#[async_trait]
impl ReplySink for StreamingReplySink {
    fn push(&self, text: &str) {
        // Sync (called from the observer's `on_stream_chunk`); std Mutex held
        // only for the try_send, never across an await.
        if let Some(tx) = self.tx.lock().unwrap().as_ref() {
            let _ = tx.send(text.to_string());
        }
    }

    fn did_stream(&self) -> bool {
        self.streamed.load(Ordering::Relaxed)
    }

    async fn finalize(&self) {
        // Drop the sender so the coalescer's rx.recv() returns None, it drains
        // the tail, and exits. Await the join so trailing chunks land before
        // the gateway considers the reply done. Guards are released before the
        // await so the future stays `Send` (std MutexGuard isn't Send).
        self.tx.lock().unwrap().take();
        let handle = self.join.lock().unwrap().take();
        if let Some(h) = handle {
            let _ = h.await;
        }
    }
}

/// Segment `text` into chunks ≤ `max_len`, preferring paragraph (`\n\n`) and
/// newline boundaries, falling back to hard cut. Never returns an empty chunk.
///
/// CJK-aware: `max_len` is a byte budget, and a hard cut at an arbitrary byte
/// lands inside a multi-byte codepoint → panic. So the byte cap is floored to
/// the nearest char boundary before slicing, and the final cut is advanced to a
/// boundary if needed.
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
        // Floor the byte cap to a char boundary (CJK: a 3-byte char may straddle
        // max_len). `is_char_boundary` is stable since 1.9.
        let mut cap = max_len;
        while cap > 0 && !rest.is_char_boundary(cap) {
            cap -= 1;
        }
        // Try to cut at the last paragraph / newline break within the window.
        let window = &rest[..cap];
        let cut = window
            .rfind("\n\n")
            .map(|i| i + 2)
            .or_else(|| window.rfind('\n').map(|i| i + 1))
            .unwrap_or(cap)
            .max(1)
            .min(rest.len());
        // Advance cut to a char boundary (rfind results land on ASCII newlines,
        // so they're already boundaries; this guards the hard-cut fallback).
        let mut cut = cut;
        while cut < rest.len() && !rest.is_char_boundary(cut) {
            cut += 1;
        }
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
        fn supports_streaming_reply(&self) -> bool {
            true
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
    async fn deliver_scheduled_runs_turn_and_replies() {
        // §3.2 cron fire → gateway.deliver_scheduled (known session, no mint)
        // reuses run_and_reply so the reply is relayed to the origin channel.
        let (gw, plat) = gateway_with(4000);
        gw.deliver_scheduled(
            ChannelId::new("loopback", "cron-chan"),
            "sess-123".to_string(),
            "coding".to_string(),
            "u-cron".to_string(),
            "standup time",
        )
        .await
        .unwrap();
        assert_eq!(plat.take(), vec!["echo: standup time"]);
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

    #[test]
    fn segment_text_cjk_no_panic_on_mid_char_boundary() {
        // Reproduces the production panic: a 3-byte CJK char straddling the
        // byte cap. "工" is 3 bytes (E5 B7 A5). Build a string where byte
        // offset `cap` lands inside a 工 char, and verify segment_text floors to
        // a char boundary instead of panicking.
        // 2998 bytes of 'a' + "工工工" → byte 3000 (cap=3000) is inside the
        // first 工 (bytes 2998..3001).
        let prefix = "a".repeat(2998);
        let suffix = "工工工";
        let text = format!("{prefix}{suffix}");
        assert_eq!(text.len(), 2998 + 9);
        let chunks = segment_text(&text, 3000);
        // No panic, every chunk is valid UTF-8 and ≤ cap, concat reproduces.
        for c in &chunks {
            assert!(c.len() <= 3000);
            assert!(std::str::from_utf8(c.as_bytes()).is_ok());
        }
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn segment_text_cjk_prefers_newline_boundary() {
        // 2400 bytes of CJK, then a newline, then ASCII — the newline sits
        // inside the cap window, so the cut should land on it (not mid-char).
        let cjk = "你好世界".repeat(200); // 4 chars * 3 bytes * 200 = 2400 bytes
        let text = format!("{cjk}\n{}", "x".repeat(700)); // 2400+1+700 = 3101 > 3000
        let chunks = segment_text(&text, 3000);
        assert!(chunks.len() >= 2);
        assert!(
            chunks[0].ends_with('\n'),
            "first cut should land on the newline"
        );
        for c in &chunks {
            assert!(c.len() <= 3000);
            assert!(std::str::from_utf8(c.as_bytes()).is_ok());
        }
        assert_eq!(chunks.concat(), text);
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

    #[tokio::test]
    async fn streaming_reply_pushes_chunks_and_dedups_final() {
        // A runner that streams chunks via the sink (mirrors a CLI runner
        // wiring on_stream_chunk → sink.push).
        struct StreamingRunner;
        #[async_trait]
        impl GatewayRunner for StreamingRunner {
            async fn run_turn(&self, _: &str, _: &str) -> TurnOutcome {
                unreachable!("should_stream path must call run_turn_streaming")
            }
            async fn run_turn_streaming(
                &self,
                _: &str,
                _: &str,
                sink: Arc<dyn ReplySink>,
            ) -> TurnOutcome {
                sink.push("hello ");
                sink.push("world");
                TurnOutcome::Done {
                    final_answer: "hello world".into(),
                    completed: true,
                    iterations: 1,
                }
            }
            fn supports_streaming(&self) -> bool {
                true
            }
        }

        let plat = Arc::new(CapturePlatform::new(4000));
        let mut reg = PlatformRegistry::new();
        reg.register(plat.clone());
        let gw = Gateway::new(
            Arc::new(StreamingRunner),
            reg,
            ChannelDirectory::in_memory(),
            ProfileRoute::new("coding"),
        );
        gw.handle_inbound(MessageEvent::new(
            ChannelId::new("loopback", "c"),
            Sender::anonymous("u"),
            "hi",
        ))
        .await
        .unwrap();
        let sent = plat.take();
        // Two pushed chunks coalesce into a SINGLE platform send (one bubble)
        // — only drained at finalize. A per-tick flush would have split this
        // into multiple bubbles (the bug this test pins).
        assert_eq!(sent.len(), 1, "expected one bubble, got {sent:?}");
        assert_eq!(sent[0], "hello world");
    }

    #[tokio::test]
    async fn non_streaming_runner_falls_back_to_final_send() {
        struct EchoRunner;
        #[async_trait]
        impl GatewayRunner for EchoRunner {
            async fn run_turn(&self, _sid: &str, task: &str) -> TurnOutcome {
                TurnOutcome::Done {
                    final_answer: format!("echo: {task}"),
                    completed: true,
                    iterations: 1,
                }
            }
        }
        let plat = Arc::new(CapturePlatform::new(4000));
        let mut reg = PlatformRegistry::new();
        reg.register(plat.clone());
        let gw = Gateway::new(
            Arc::new(EchoRunner),
            reg,
            ChannelDirectory::in_memory(),
            ProfileRoute::new("coding"),
        );
        gw.handle_inbound(MessageEvent::new(
            ChannelId::new("loopback", "c"),
            Sender::anonymous("u"),
            "hi",
        ))
        .await
        .unwrap();
        assert_eq!(plat.take().as_slice(), ["echo: hi"]);
    }

    #[tokio::test]
    async fn pack_locked_per_channel_from_profile_route() {
        // §3.1 tail #1: ProfileRoute resolves a pack per channel, locked into
        // the binding at first mint and carried through SESSION_SOURCE so the
        // runner can pick the right lazily-built App.
        use crate::profile::RouteEntry;
        use std::sync::OnceLock;
        static SEEN: OnceLock<Mutex<Option<SessionSource>>> = OnceLock::new();
        let _ = SEEN.set(Mutex::new(None));
        struct PeekRunner;
        #[async_trait]
        impl GatewayRunner for PeekRunner {
            async fn run_turn(&self, _: &str, _: &str) -> TurnOutcome {
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
        let profile = ProfileRoute::new("default_pack").with(RouteEntry {
            platform: None,
            guild: None,
            channel: Some("cA".into()),
            thread: None,
            pack: "packA".into(),
        });
        let gw = Gateway::new(
            Arc::new(PeekRunner),
            reg,
            ChannelDirectory::in_memory(),
            profile,
        );

        // channel cA → entry packA
        gw.handle_inbound(MessageEvent::new(
            ChannelId::new("loopback", "cA"),
            Sender::anonymous("u"),
            "x",
        ))
        .await
        .unwrap();
        let s = SEEN.get().unwrap().lock().unwrap().clone().unwrap();
        assert_eq!(s.pack, "packA");

        // channel cB (no entry) → default
        SEEN.get().unwrap().lock().unwrap().take();
        gw.handle_inbound(MessageEvent::new(
            ChannelId::new("loopback", "cB"),
            Sender::anonymous("u"),
            "y",
        ))
        .await
        .unwrap();
        let s = SEEN.get().unwrap().lock().unwrap().clone().unwrap();
        assert_eq!(s.pack, "default_pack");
    }
}
