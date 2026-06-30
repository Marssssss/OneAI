//! iOS interaction gate — wraps ChannelInteractionGate.
//!
//! The IOSInteractionGate wraps a ChannelInteractionGate (tool-approval-only
//! config), which sends tool-approval requests through a tokio channel. The
//! IOSInteractionBridge holds the receiver and provides a C callback
//! mechanism for the Swift-side code to receive requests and send responses.

use std::sync::Arc;

use async_trait::async_trait;
use oneai_core::{InteractionPoint, InteractionRequest, InteractionResponse, RiskLevel};
use oneai_core::error::Result;
use oneai_core::platform::PlatformInteractionGate;
use oneai_core::traits::InteractionGate;
use oneai_tool::{ChannelInteractionGate, InteractionGateConfig, ThresholdInteractionGate};

use crate::callback_bridge::CallbackInteractionBridge;

/// The tool-approval-only gate config used by mobile gates: only
/// `ToolApproval` is enabled, so the bridge only ever sees tool-approval
/// items (matching the legacy ApprovalGate behaviour).
fn mobile_config() -> InteractionGateConfig {
    InteractionGateConfig {
        preinfer: false,
        postinfer: false,
        tool_approval: true,
        plan_decision: false,
        plan_review: false,
    }
}

/// iOS-native interaction gate that bridges to UIAlertController via C callback.
pub struct IOSInteractionGate {
    inner: Arc<dyn InteractionGate>,
}

impl IOSInteractionGate {
    /// Create a new iOS interaction gate with an auto-proceed threshold.
    pub fn new(buffer_size: usize, threshold: RiskLevel) -> (Self, IOSInteractionBridge) {
        let (gate, receiver) =
            ThresholdInteractionGate::new(buffer_size, threshold, mobile_config());
        let inner: Arc<dyn InteractionGate> = Arc::new(gate);
        let callback_bridge = CallbackInteractionBridge::new(receiver);
        let bridge = IOSInteractionBridge { inner: callback_bridge };
        (Self { inner }, bridge)
    }

    /// Create a gate where tool-approval requests go through the channel.
    pub fn new_manual_only(buffer_size: usize) -> (Self, IOSInteractionBridge) {
        let (gate, receiver) = ChannelInteractionGate::with_config(buffer_size, mobile_config());
        let inner: Arc<dyn InteractionGate> = Arc::new(gate);
        let callback_bridge = CallbackInteractionBridge::new(receiver);
        let bridge = IOSInteractionBridge { inner: callback_bridge };
        (Self { inner }, bridge)
    }
}

#[async_trait]
impl InteractionGate for IOSInteractionGate {
    async fn request(&self, req: InteractionRequest) -> Result<InteractionResponse> {
        self.inner.request(req).await
    }

    fn enabled(&self, point: InteractionPoint) -> bool {
        self.inner.enabled(point)
    }
}

#[async_trait]
impl PlatformInteractionGate for IOSInteractionGate {
    fn platform_name(&self) -> &'static str {
        "ios"
    }

    fn is_ui_available(&self) -> bool {
        true
    }
}

/// iOS interaction bridge that wraps a CallbackInteractionBridge.
///
/// The Swift-side `OneAIInteractionHandler` class registers a C callback
/// at init, receives JSON-encoded tool-approval request strings, presents
/// a UIAlertController, and calls the response callback.
pub struct IOSInteractionBridge {
    inner: CallbackInteractionBridge,
}

impl IOSInteractionBridge {
    /// Access the inner callback bridge for registering callbacks.
    pub fn callback_bridge(&self) -> &CallbackInteractionBridge {
        &self.inner
    }
}
