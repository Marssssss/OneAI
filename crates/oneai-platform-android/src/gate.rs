//! Android interaction gate — wraps ChannelInteractionGate.
//!
//! The AndroidInteractionGate wraps a ChannelInteractionGate
//! (tool-approval-only config), which sends tool-approval requests through
//! a tokio channel. The AndroidInteractionBridge holds the receiver and
//! provides JNI-compatible methods for the Kotlin side to poll pending
//! items and send responses.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use oneai_core::{InteractionPoint, InteractionRequest, InteractionResponse, RiskLevel};
use oneai_core::error::Result;
use oneai_core::platform::PlatformInteractionGate;
use oneai_core::traits::InteractionGate;
use oneai_tool::{ChannelInteractionGate, InteractionGateConfig, InteractionPendingItem, ThresholdInteractionGate};

// ─── AndroidInteractionGate ───────────────────────────────────────

/// Android-native interaction gate that bridges to AlertDialog via JNI.
///
/// This gate wraps a `ChannelInteractionGate` (or `ThresholdInteractionGate`)
/// internally. Low-risk requests (below threshold, when a threshold is set) are
/// auto-proceeded; the rest go through a channel to the bridge, which the
/// Kotlin/Java side polls and responds to via JNI.
pub struct AndroidInteractionGate {
    inner: Arc<dyn InteractionGate>,
}

/// Tool-approval-only gate config (see `oneai-platform-ios/src/gate.rs`).
fn mobile_config() -> InteractionGateConfig {
    InteractionGateConfig {
        preinfer: false,
        postinfer: false,
        tool_approval: true,
        plan_decision: false,
        plan_review: false,
    }
}

impl AndroidInteractionGate {
    /// Create a new Android interaction gate with an auto-proceed threshold.
    pub fn new(buffer_size: usize, threshold: RiskLevel) -> (Self, AndroidInteractionBridge) {
        let (gate, receiver) =
            ThresholdInteractionGate::new(buffer_size, threshold, mobile_config());
        let inner: Arc<dyn InteractionGate> = Arc::new(gate);
        let bridge = AndroidInteractionBridge::new(receiver);
        (Self { inner }, bridge)
    }

    /// Create a gate where tool-approval requests go through the channel.
    pub fn new_manual_only(buffer_size: usize) -> (Self, AndroidInteractionBridge) {
        let (gate, receiver) = ChannelInteractionGate::with_config(buffer_size, mobile_config());
        let inner: Arc<dyn InteractionGate> = Arc::new(gate);
        let bridge = AndroidInteractionBridge::new(receiver);
        (Self { inner }, bridge)
    }
}

#[async_trait]
impl InteractionGate for AndroidInteractionGate {
    async fn request(&self, req: InteractionRequest) -> Result<InteractionResponse> {
        self.inner.request(req).await
    }

    fn enabled(&self, point: InteractionPoint) -> bool {
        self.inner.enabled(point)
    }
}

#[async_trait]
impl PlatformInteractionGate for AndroidInteractionGate {
    fn platform_name(&self) -> &'static str {
        "android"
    }

    fn is_ui_available(&self) -> bool {
        // Android always has UI available when the gate is used
        true
    }
}

// ─── AndroidInteractionBridge ───────────────────────────────────────

/// Bridge that holds the channel receiver for Android interaction items.
///
/// The Kotlin-side `OneAIInteractionHandler` calls `poll_pending_json()`
/// to receive pending tool-approval requests (as JSON strings), shows
/// an AlertDialog, and then calls `send_response_json()` to send
/// the user's decision back through the oneshot channel.
pub struct AndroidInteractionBridge {
    inner: Mutex<tokio::sync::mpsc::Receiver<InteractionPendingItem>>,
}

impl AndroidInteractionBridge {
    fn new(receiver: tokio::sync::mpsc::Receiver<InteractionPendingItem>) -> Self {
        Self {
            inner: Mutex::new(receiver),
        }
    }

    /// Poll for a pending tool-approval request (non-blocking).
    ///
    /// Returns a JSON-encoded string of the request, or None if no item is
    /// pending or the item is not a ToolApproval (auto-proceeded). The
    /// response sender is dropped — use the `JniInteractionBridge` to track
    /// pending senders by ID and reply.
    pub fn poll_pending_json(&self) -> Option<String> {
        let mut inner = self.inner.lock().unwrap();
        match inner.try_recv() {
            Ok(item) => {
                let approval = match &item.request {
                    InteractionRequest::ToolApproval { approval } => approval,
                    _ => {
                        let _ = item.response_tx.send(InteractionResponse::Proceed);
                        return None;
                    }
                };
                Some(
                    serde_json::json!({
                        "tool_name": approval.tool_name,
                        "args": approval.args,
                        "risk_level": approval.risk_level,
                        "justification": approval.justification,
                    })
                    .to_string(),
                )
            }
            Err(_) => None,
        }
    }

    /// Poll for a pending interaction item and return the full item
    /// (for use by the JNI bridge which tracks response senders).
    pub fn poll_pending_item(&self) -> Option<InteractionPendingItem> {
        let mut inner = self.inner.lock().unwrap();
        inner.try_recv().ok()
    }

    /// Send a response for a pending interaction item.
    pub fn send_response(
        item: InteractionPendingItem,
        response: InteractionResponse,
    ) -> std::result::Result<(), ()> {
        item.response_tx.send(response).map_err(|_| ())
    }
}
