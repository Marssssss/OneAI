//! # OneAI Platform — Android
//!
//! Android platform adapter for the OneAI framework.
//! Provides an approval gate that bridges to Android AlertDialog
//! via JNI, allowing the Kotlin/Java side to show native dialogs
//! for high-risk tool approval.
//!
//! Usage:
//! ```ignore
//! let (gate, bridge) = AndroidApprovalGate::new(16, RiskLevel::Medium);
//! let app = AppBuilder::new()
//!     .platform_approval_gate(Arc::new(gate))
//!     .build()?;
//!
//! // In Kotlin: OneAIApprovalHandler polls the bridge and shows AlertDialog
//! ```

mod gate;
mod jni_bridge;

use std::sync::Arc;

use oneai_core::RiskLevel;

pub use gate::{AndroidApprovalGate, AndroidApprovalBridge};
pub use jni_bridge::JniApprovalBridge;

/// Factory for creating Android approval gates.
pub struct AndroidApprovalGateFactory;

impl AndroidApprovalGateFactory {
    /// Create an Android approval gate with auto-approve threshold.
    pub fn create(buffer_size: usize, threshold: RiskLevel) -> (AndroidApprovalGate, AndroidApprovalBridge) {
        AndroidApprovalGate::new(buffer_size, threshold)
    }

    /// Create a gate where all requests go through the channel (no auto-approve).
    pub fn create_manual_only(buffer_size: usize) -> (AndroidApprovalGate, AndroidApprovalBridge) {
        AndroidApprovalGate::new_manual_only(buffer_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::traits::ApprovalGate;
    use oneai_core::{ApprovalRequest, ApprovalResponse};

    #[tokio::test]
    async fn test_android_approval_gate_auto_approve() {
        let (gate, _bridge) = AndroidApprovalGateFactory::create(16, RiskLevel::Medium);

        let request = ApprovalRequest {
            tool_name: "calculator".to_string(),
            args: serde_json::json!({"expression": "2+2"}),
            risk_level: RiskLevel::Low,
            permission_level: None,
            justification: "Simple calculation".to_string(),
        };

        let response = gate.request_approval(request).await.unwrap();
        assert!(matches!(response, ApprovalResponse::Approved { .. }));
    }

    #[test]
    fn test_android_bridge_poll_empty() {
        let (_, bridge) = AndroidApprovalGateFactory::create(16, RiskLevel::Medium);
        // No pending items yet
        assert!(bridge.poll_pending_json().is_none());
    }
}