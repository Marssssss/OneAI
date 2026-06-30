//! HarmonyOS interaction gate — wraps ChannelInteractionGate.
//!
//! The HarmonyInteractionGate wraps a ChannelInteractionGate
//! (tool-approval-only config), which sends tool-approval requests through
//! a tokio channel. The HarmonyInteractionBridge holds the receiver and
//! provides a C callback mechanism for the ArkTS-side code to receive
//! requests and send responses.

use std::sync::Arc;

use async_trait::async_trait;
use oneai_core::{InteractionPoint, InteractionRequest, InteractionResponse, RiskLevel};
use oneai_core::error::Result;
use oneai_core::platform::PlatformInteractionGate;
use oneai_core::traits::InteractionGate;
use oneai_tool::{ChannelInteractionGate, InteractionGateConfig, ThresholdInteractionGate};

use crate::callback_bridge::CallbackInteractionBridge;

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

/// HarmonyOS-native interaction gate that bridges to CommonDialog via C callback.
pub struct HarmonyInteractionGate {
    inner: Arc<dyn InteractionGate>,
}

impl HarmonyInteractionGate {
    /// Create a new HarmonyOS interaction gate with an auto-proceed threshold.
    pub fn new(buffer_size: usize, threshold: RiskLevel) -> (Self, HarmonyInteractionBridge) {
        let (gate, receiver) =
            ThresholdInteractionGate::new(buffer_size, threshold, mobile_config());
        let inner: Arc<dyn InteractionGate> = Arc::new(gate);
        let callback_bridge = CallbackInteractionBridge::new(receiver);
        let bridge = HarmonyInteractionBridge { inner: callback_bridge };
        (Self { inner }, bridge)
    }

    /// Create a gate where tool-approval requests go through the channel.
    pub fn new_manual_only(buffer_size: usize) -> (Self, HarmonyInteractionBridge) {
        let (gate, receiver) = ChannelInteractionGate::with_config(buffer_size, mobile_config());
        let inner: Arc<dyn InteractionGate> = Arc::new(gate);
        let callback_bridge = CallbackInteractionBridge::new(receiver);
        let bridge = HarmonyInteractionBridge { inner: callback_bridge };
        (Self { inner }, bridge)
    }
}

#[async_trait]
impl InteractionGate for HarmonyInteractionGate {
    async fn request(&self, req: InteractionRequest) -> Result<InteractionResponse> {
        self.inner.request(req).await
    }

    fn enabled(&self, point: InteractionPoint) -> bool {
        self.inner.enabled(point)
    }
}

#[async_trait]
impl PlatformInteractionGate for HarmonyInteractionGate {
    fn platform_name(&self) -> &'static str {
        "harmony"
    }

    fn is_ui_available(&self) -> bool {
        true
    }
}

/// HarmonyOS interaction bridge that wraps a CallbackInteractionBridge.
///
/// The ArkTS-side `InteractionDialogManager` registers a C callback
/// at init, receives JSON-encoded tool-approval request strings, shows
/// a CommonDialog, and calls the response callback.
pub struct HarmonyInteractionBridge {
    inner: CallbackInteractionBridge,
}

impl HarmonyInteractionBridge {
    /// Access the inner callback bridge for registering callbacks.
    pub fn callback_bridge(&self) -> &CallbackInteractionBridge {
        &self.inner
    }
}
