//! HarmonyOS approval gate — wraps ChannelApprovalGateWithThreshold.
//!
//! The HarmonyApprovalGate wraps a ChannelApprovalGateWithThreshold,
//! which sends approval requests through a tokio channel. The
//! HarmonyApprovalBridge holds the receiver and provides a C callback
//! mechanism for the ArkTS-side code to receive requests and send responses.

use std::sync::Arc;

use async_trait::async_trait;
use oneai_core::{ApprovalRequest, ApprovalResponse, RiskLevel};
use oneai_core::platform::PlatformApprovalGate;
use oneai_core::traits::ApprovalGate;
use oneai_tool::{ChannelApprovalGateWithThreshold, ApprovalPendingItem};

use crate::callback_bridge::CallbackApprovalBridge;

/// HarmonyOS-native approval gate that bridges to CommonDialog via C callback.
pub struct HarmonyApprovalGate {
    inner: Arc<ChannelApprovalGateWithThreshold>,
}

impl HarmonyApprovalGate {
    /// Create a new HarmonyOS approval gate with auto-approve threshold.
    pub fn new(buffer_size: usize, threshold: RiskLevel) -> (Self, HarmonyApprovalBridge) {
        let (gate, receiver) = ChannelApprovalGateWithThreshold::new(buffer_size, threshold);
        let inner = Arc::new(gate);
        let callback_bridge = CallbackApprovalBridge::new(receiver);
        let bridge = HarmonyApprovalBridge { inner: callback_bridge };
        (Self { inner }, bridge)
    }

    /// Create a gate where all requests go through the channel.
    pub fn new_manual_only(buffer_size: usize) -> (Self, HarmonyApprovalBridge) {
        let (gate, receiver) = ChannelApprovalGateWithThreshold::new_manual_only(buffer_size);
        let inner = Arc::new(gate);
        let callback_bridge = CallbackApprovalBridge::new(receiver);
        let bridge = HarmonyApprovalBridge { inner: callback_bridge };
        (Self { inner }, bridge)
    }
}

#[async_trait]
impl ApprovalGate for HarmonyApprovalGate {
    async fn request_approval(&self, request: ApprovalRequest) -> oneai_core::error::Result<ApprovalResponse> {
        self.inner.request_approval(request).await
    }
}

#[async_trait]
impl PlatformApprovalGate for HarmonyApprovalGate {
    fn platform_name(&self) -> &'static str {
        "harmony"
    }

    fn is_ui_available(&self) -> bool {
        true
    }
}

/// HarmonyOS approval bridge that wraps a CallbackApprovalBridge.
///
/// The ArkTS-side `ApprovalDialogManager` registers a C callback
/// at init, receives JSON-encoded ApprovalRequest strings, shows
/// a CommonDialog, and calls the response callback.
pub struct HarmonyApprovalBridge {
    inner: CallbackApprovalBridge,
}

impl HarmonyApprovalBridge {
    /// Access the inner callback bridge for registering callbacks.
    pub fn callback_bridge(&self) -> &CallbackApprovalBridge {
        &self.inner
    }
}