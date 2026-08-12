//! # OneAI Platform — Android
//!
//! Android platform adapter for the OneAI framework. Provides a
//! `PlatformInteractionGate` (`AndroidInteractionGate`) for a non-bus uniffi
//! app: low-risk tool-approval requests auto-proceed, the rest land on a
//! channel the Kotlin side polls via `AndroidInteractionBridge` and resolves
//! with `AlertDialog`.
//!
//! P4 removed the separate `JniInteractionBridge` (id-keyed pending tracker)
//! — the in-process 3-symbol bus pump resolves approvals as
//! `EngineYield::ApprovalRequest` ↔ `Directive::Approve`, so the bus path no
//! longer needs this gate. It stays as a fallback shell for a non-bus app;
//! native bus consumers see `approval_request` yields off the pump.
//!
//! Usage (non-bus fallback):
//! ```ignore
//! let (gate, bridge) = AndroidInteractionGate::new(16, RiskLevel::Medium);
//! let app = AppBuilder::new()
//!     .interaction_gate(Arc::new(gate))
//!     .build()?;
//!
//! // In Kotlin: poll the bridge and show AlertDialog
//! ```

mod gate;

use oneai_core::RiskLevel;

pub use gate::{AndroidInteractionBridge, AndroidInteractionGate};

/// Factory for creating Android interaction gates.
pub struct AndroidInteractionGateFactory;

impl AndroidInteractionGateFactory {
    /// Create an Android interaction gate with an auto-proceed threshold.
    pub fn create(
        buffer_size: usize,
        threshold: RiskLevel,
    ) -> (AndroidInteractionGate, AndroidInteractionBridge) {
        AndroidInteractionGate::new(buffer_size, threshold)
    }

    /// Create a gate where tool-approval requests go through the channel.
    pub fn create_manual_only(
        buffer_size: usize,
    ) -> (AndroidInteractionGate, AndroidInteractionBridge) {
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
