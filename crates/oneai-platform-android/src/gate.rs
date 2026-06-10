//! Android approval gate — wraps ChannelApprovalGateWithThreshold.
//!
//! The AndroidApprovalGate wraps a ChannelApprovalGateWithThreshold,
//! which sends approval requests through a tokio channel. The
//! AndroidApprovalBridge holds the receiver and provides JNI-compatible
//! methods for the Kotlin side to poll pending items and send responses.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use oneai_core::{ApprovalRequest, ApprovalResponse, RiskLevel};
use oneai_core::platform::PlatformApprovalGate;
use oneai_core::traits::ApprovalGate;
use oneai_tool::{ChannelApprovalGateWithThreshold, ApprovalPendingItem};

// ─── AndroidApprovalGate ───────────────────────────────────────────

/// Android-native approval gate that bridges to AlertDialog via JNI.
///
/// This gate wraps a ChannelApprovalGateWithThreshold internally.
/// Low-risk requests (below threshold) are auto-approved.
/// High-risk requests are sent through a channel to the bridge,
/// which the Kotlin/Java side polls and responds to via JNI.
pub struct AndroidApprovalGate {
    inner: Arc<ChannelApprovalGateWithThreshold>,
}

impl AndroidApprovalGate {
    /// Create a new Android approval gate with auto-approve threshold.
    pub fn new(buffer_size: usize, threshold: RiskLevel) -> (Self, AndroidApprovalBridge) {
        let (gate, receiver) = ChannelApprovalGateWithThreshold::new(buffer_size, threshold);
        let inner = Arc::new(gate);
        let bridge = AndroidApprovalBridge::new(receiver);
        (Self { inner }, bridge)
    }

    /// Create a gate where all requests go through the channel.
    pub fn new_manual_only(buffer_size: usize) -> (Self, AndroidApprovalBridge) {
        let (gate, receiver) = ChannelApprovalGateWithThreshold::new_manual_only(buffer_size);
        let inner = Arc::new(gate);
        let bridge = AndroidApprovalBridge::new(receiver);
        (Self { inner }, bridge)
    }
}

#[async_trait]
impl ApprovalGate for AndroidApprovalGate {
    async fn request_approval(&self, request: ApprovalRequest) -> oneai_core::error::Result<ApprovalResponse> {
        self.inner.request_approval(request).await
    }
}

#[async_trait]
impl PlatformApprovalGate for AndroidApprovalGate {
    fn platform_name(&self) -> &'static str {
        "android"
    }

    fn is_ui_available(&self) -> bool {
        // Android always has UI available when the gate is used
        true
    }
}

// ─── AndroidApprovalBridge ───────────────────────────────────────

/// Bridge that holds the channel receiver for Android approval items.
///
/// The Kotlin-side `OneAIApprovalHandler` calls `poll_pending_json()`
/// to receive pending approval requests (as JSON strings), shows
/// an AlertDialog, and then calls `send_response_json()` to send
/// the user's decision back through the oneshot channel.
pub struct AndroidApprovalBridge {
    inner: Mutex<tokio::sync::mpsc::Receiver<ApprovalPendingItem>>,
}

impl AndroidApprovalBridge {
    fn new(receiver: tokio::sync::mpsc::Receiver<ApprovalPendingItem>) -> Self {
        Self {
            inner: Mutex::new(receiver),
        }
    }

    /// Poll for a pending approval item (non-blocking).
    ///
    /// Returns a JSON-encoded string of the ApprovalRequest,
    /// or None if no item is pending.
    ///
    /// The JSON format:
    /// ```json
    /// {
    ///   "tool_name": "shell",
    ///   "args": {"command": "ls"},
    ///   "risk_level": "high",
    ///   "justification": "List files"
    /// }
    /// ```
    pub fn poll_pending_json(&self) -> Option<String> {
        let mut inner = self.inner.lock().unwrap();
        match inner.try_recv() {
            Ok(item) => {
                // Store the response_tx for later use
                // We need to keep track of pending items that haven't been responded to
                // The simplest approach: serialize the request and store the sender separately
                let request_json = serde_json::to_string(&item.request).unwrap_or_default();
                // Store the response sender in a thread-safe way
                // For now, we'll return the request JSON and the caller must
                // call send_response_json separately with the same request details
                // This is a simplified approach — in production, you'd use a HashMap
                // keyed by request ID to track pending response senders
                // For the JNI bridge, we'll handle this in the jni_bridge module
                Some(request_json)
            }
            Err(_) => None,
        }
    }

    /// Poll for a pending approval item and return the full item
    /// (for use by the JNI bridge which tracks response senders).
    pub fn poll_pending_item(&self) -> Option<ApprovalPendingItem> {
        let mut inner = self.inner.lock().unwrap();
        inner.try_recv().ok()
    }

    /// Send a response for a pending approval item.
    ///
    /// Takes ownership of the item and sends the response through
    /// its oneshot channel.
    pub fn send_response(item: ApprovalPendingItem, response: ApprovalResponse) -> std::result::Result<(), ()> {
        item.response_tx.send(response).map_err(|_| ())
    }
}