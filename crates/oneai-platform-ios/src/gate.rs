//! iOS approval gate — wraps ChannelApprovalGateWithThreshold.
//!
//! The IOSApprovalGate wraps a ChannelApprovalGateWithThreshold,
//! which sends approval requests through a tokio channel. The
//! IOSApprovalBridge holds the receiver and provides a C callback
//! mechanism for the Swift-side code to receive requests and
//! send responses.

use std::sync::Arc;

use async_trait::async_trait;
use oneai_core::{ApprovalRequest, ApprovalResponse, RiskLevel};
use oneai_core::platform::PlatformApprovalGate;
use oneai_core::traits::ApprovalGate;
use oneai_tool::ChannelApprovalGateWithThreshold;

use crate::callback_bridge::CallbackApprovalBridge;

/// iOS-native approval gate that bridges to UIAlertController via C callback.
pub struct IOSApprovalGate {
    inner: Arc<ChannelApprovalGateWithThreshold>,
}

impl IOSApprovalGate {
    /// Create a new iOS approval gate with auto-approve threshold.
    pub fn new(buffer_size: usize, threshold: RiskLevel) -> (Self, IOSApprovalBridge) {
        let (gate, receiver) = ChannelApprovalGateWithThreshold::new(buffer_size, threshold);
        let inner = Arc::new(gate);
        let callback_bridge = CallbackApprovalBridge::new(receiver);
        let bridge = IOSApprovalBridge { inner: callback_bridge };
        (Self { inner }, bridge)
    }

    /// Create a gate where all requests go through the channel.
    pub fn new_manual_only(buffer_size: usize) -> (Self, IOSApprovalBridge) {
        let (gate, receiver) = ChannelApprovalGateWithThreshold::new_manual_only(buffer_size);
        let inner = Arc::new(gate);
        let callback_bridge = CallbackApprovalBridge::new(receiver);
        let bridge = IOSApprovalBridge { inner: callback_bridge };
        (Self { inner }, bridge)
    }
}

#[async_trait]
impl ApprovalGate for IOSApprovalGate {
    async fn request_approval(&self, request: ApprovalRequest) -> oneai_core::error::Result<ApprovalResponse> {
        self.inner.request_approval(request).await
    }
}

#[async_trait]
impl PlatformApprovalGate for IOSApprovalGate {
    fn platform_name(&self) -> &'static str {
        "ios"
    }

    fn is_ui_available(&self) -> bool {
        true
    }
}

/// iOS approval bridge that wraps a CallbackApprovalBridge.
///
/// The Swift-side `OneAIApprovalHandler` class registers a C callback
/// at init, receives JSON-encoded ApprovalRequest strings, presents
/// a UIAlertController, and calls the response callback.
pub struct IOSApprovalBridge {
    inner: CallbackApprovalBridge,
}

impl IOSApprovalBridge {
    /// Access the inner callback bridge for registering callbacks.
    pub fn callback_bridge(&self) -> &CallbackApprovalBridge {
        &self.inner
    }
}