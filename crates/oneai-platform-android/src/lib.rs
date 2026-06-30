//! # OneAI Platform — Android
//!
//! Android platform adapter for the OneAI framework.
//! Provides an interaction gate that bridges to Android AlertDialog
//! via JNI, allowing the Kotlin/Java side to show native dialogs
//! for high-risk tool approval.
//!
//! Usage:
//! ```ignore
//! let (gate, bridge) = AndroidInteractionGate::new(16, RiskLevel::Medium);
//! let app = AppBuilder::new()
//!     .interaction_gate(Arc::new(gate))
//!     .build()?;
//!
//! // In Kotlin: OneAIInteractionHandler polls the bridge and shows AlertDialog
//! ```

mod gate;
mod jni_bridge;


use oneai_core::RiskLevel;

pub use gate::{AndroidInteractionGate, AndroidInteractionBridge};
pub use jni_bridge::JniInteractionBridge;

/// Factory for creating Android interaction gates.
pub struct AndroidInteractionGateFactory;

impl AndroidInteractionGateFactory {
    /// Create an Android interaction gate with an auto-proceed threshold.
    pub fn create(buffer_size: usize, threshold: RiskLevel) -> (AndroidInteractionGate, AndroidInteractionBridge) {
        AndroidInteractionGate::new(buffer_size, threshold)
    }

    /// Create a gate where tool-approval requests go through the channel.
    pub fn create_manual_only(buffer_size: usize) -> (AndroidInteractionGate, AndroidInteractionBridge) {
        AndroidInteractionGate::new_manual_only(buffer_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::traits::InteractionGate;
    use oneai_core::{ApprovalRequest, InteractionRequest, InteractionResponse};

    #[tokio::test]
    async fn test_android_interaction_gate_auto_proceed_low_risk() {
        let (gate, _bridge) = AndroidInteractionGateFactory::create(16, RiskLevel::Medium);

        let request = ApprovalRequest {
            tool_name: "calculator".to_string(),
            args: serde_json::json!({"expression": "2+2"}),
            risk_level: RiskLevel::Low,
            permission_level: None,
            justification: "Simple calculation".to_string(),
        };

        let response = gate
            .request(InteractionRequest::ToolApproval { approval: request })
            .await
            .unwrap();
        assert!(matches!(response, InteractionResponse::Proceed));
    }

    #[test]
    fn test_android_bridge_poll_empty() {
        let (_, bridge) = AndroidInteractionGateFactory::create(16, RiskLevel::Medium);
        // No pending items yet
        assert!(bridge.poll_pending_json().is_none());
    }
}
