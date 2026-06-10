//! Approval gate — human-machine collaboration for high-risk tool execution.
//!
//! The approval gate is the mechanism by which high-risk tool calls are
//! suspended until a human user approves or denies them.
//!
//! Implementations:
//! - `BlockingApprovalGate`: Placeholder that always denies (for testing/Phase 6)
//! - `ChannelApprovalGateWithThreshold`: Channel-based approval that allows external UI
//!   to respond to approval requests asynchronously
//! - `AutoApprovalGate`: Automatically approves all requests (for testing/low-risk environments)

use tokio::sync::{mpsc, oneshot};
use oneai_core::{ApprovalRequest, ApprovalResponse, RiskLevel};
use oneai_core::error::{ApprovalError, OneAIError, Result};
use oneai_core::traits::ApprovalGate;

// ─── BlockingApprovalGate ────────────────────────────────────────────────

/// Blocking approval gate that always denies.
///
/// This is a placeholder implementation for testing and development.
/// Platform-specific implementations (Qt dialog, Android popup, iOS UIAlertController)
/// will be provided in Phase 6.
pub struct BlockingApprovalGate;

#[async_trait::async_trait]
impl ApprovalGate for BlockingApprovalGate {
    async fn request_approval(&self, _request: ApprovalRequest) -> Result<ApprovalResponse> {
        Ok(ApprovalResponse::Denied {
            reason: "BlockingApprovalGate placeholder — no UI configured".to_string(),
        })
    }
}

// ─── ChannelApprovalGate ────────────────────────────────────────────────

/// A pending approval request with a response channel.
///
/// When a high-risk tool needs approval, an `ApprovalPendingItem` is created
/// and sent through the channel. The UI (or test code) receives this item,
/// makes a decision, and sends the response back via the `response_tx` oneshot channel.
pub struct ApprovalPendingItem {
    /// The approval request details.
    pub request: ApprovalRequest,
    /// The oneshot channel to send the response back.
    pub response_tx: oneshot::Sender<ApprovalResponse>,
}

/// Channel-based approval gate for interactive approval flow.
///
/// Uses an mpsc channel to send approval requests to an external handler
/// (typically a UI thread), and a oneshot channel for each response.
///
/// This design avoids callback hell and allows the approval flow to be
/// handled by any external system (CLI prompt, native dialog, web UI, etc.)
///
/// Usage:
/// ```ignore
/// let (gate, receiver) = ChannelApprovalGateWithThreshold::new_manual_only(16);
///
/// // In the agent loop:
/// let response = gate.request_approval(request).await?;
///
/// // In the UI thread:
/// while let Some(item) = receiver.recv().await {
///     let response = show_dialog(&item.request);
///     item.response_tx.send(response).unwrap();
/// }
/// ```
pub struct ChannelApprovalGate {
    /// Channel to send approval requests to the external handler.
    pending_tx: mpsc::Sender<ApprovalPendingItem>,
}

impl ChannelApprovalGate {
    /// Create a new channel-based approval gate (all requests go through channel).
    ///
    /// Returns the gate (for use in the agent) and a receiver (for the UI handler).
    /// The `buffer_size` controls how many approval requests can be queued
    /// before the agent blocks waiting for the UI to process them.
    pub fn new(buffer_size: usize) -> (Self, mpsc::Receiver<ApprovalPendingItem>) {
        let (pending_tx, pending_rx) = mpsc::channel(buffer_size);
        (Self { pending_tx }, pending_rx)
    }
}

#[async_trait::async_trait]
impl ApprovalGate for ChannelApprovalGate {
    async fn request_approval(&self, request: ApprovalRequest) -> Result<ApprovalResponse> {
        // Send request through channel and wait for response
        let (response_tx, response_rx) = oneshot::channel();

        self.pending_tx.send(ApprovalPendingItem {
            request,
            response_tx,
        }).await.map_err(|_| {
            OneAIError::Approval(ApprovalError::NotConfigured)
        })?;

        // Wait for the UI handler's response
        let response = response_rx.await.map_err(|_| {
            OneAIError::Approval(ApprovalError::Denied {
                reason: "Approval response channel dropped".to_string(),
            })
        })?;

        Ok(response)
    }
}

// ─── ChannelApprovalGateWithAutoApprove ────────────────────────────────

/// Channel-based approval gate with configurable auto-approve threshold.
///
/// Requests with risk levels below the threshold are automatically approved.
/// Requests at or above the threshold are sent through the channel for human review.
pub struct ChannelApprovalGateWithThreshold {
    /// Channel to send approval requests to the external handler.
    pending_tx: mpsc::Sender<ApprovalPendingItem>,
    /// Risk level threshold for auto-approval.
    /// None means all requests go through the channel.
    auto_approve_threshold: Option<RiskLevel>,
}

impl ChannelApprovalGateWithThreshold {
    /// Create a new gate with auto-approve threshold.
    pub fn new(
        buffer_size: usize,
        auto_approve_threshold: RiskLevel,
    ) -> (Self, mpsc::Receiver<ApprovalPendingItem>) {
        let (pending_tx, pending_rx) = mpsc::channel(buffer_size);
        (Self {
            pending_tx,
            auto_approve_threshold: Some(auto_approve_threshold),
        }, pending_rx)
    }

    /// Create a gate where all requests go through the channel (no auto-approve).
    pub fn new_manual_only(buffer_size: usize) -> (Self, mpsc::Receiver<ApprovalPendingItem>) {
        let (pending_tx, pending_rx) = mpsc::channel(buffer_size);
        (Self {
            pending_tx,
            auto_approve_threshold: None,
        }, pending_rx)
    }
}

#[async_trait::async_trait]
impl ApprovalGate for ChannelApprovalGateWithThreshold {
    async fn request_approval(&self, request: ApprovalRequest) -> Result<ApprovalResponse> {
        // Check if the request should be auto-approved
        if let Some(threshold) = &self.auto_approve_threshold {
            if should_auto_approve(&request.risk_level, threshold) {
                tracing::info!(
                    "Auto-approving tool '{}' with risk level {:?} (below threshold {:?})",
                    request.tool_name, request.risk_level, threshold
                );
                return Ok(ApprovalResponse::Approved { modified_args: None });
            }
        }

        // Send request through channel and wait for response
        let (response_tx, response_rx) = oneshot::channel();

        self.pending_tx.send(ApprovalPendingItem {
            request,
            response_tx,
        }).await.map_err(|_| {
            oneai_core::error::OneAIError::Approval(ApprovalError::NotConfigured)
        })?;

        // Wait for the UI handler's response
        let response = response_rx.await.map_err(|_| {
            oneai_core::error::OneAIError::Approval(ApprovalError::Denied {
                reason: "Approval response channel dropped".to_string(),
            })
        })?;

        Ok(response)
    }
}

/// Check if a risk level should be auto-approved given the threshold.
///
/// Auto-approves if the request's risk level is strictly below the threshold.
/// Risk level ordering: Low < Medium < High
fn should_auto_approve(request_level: &RiskLevel, threshold: &RiskLevel) -> bool {
    match (request_level, threshold) {
        (RiskLevel::Low, RiskLevel::Low) => true,
        (RiskLevel::Low, RiskLevel::Medium) => true,
        (RiskLevel::Low, RiskLevel::High) => true,
        (RiskLevel::Medium, RiskLevel::Low) => false,
        (RiskLevel::Medium, RiskLevel::Medium) => true,
        (RiskLevel::Medium, RiskLevel::High) => true,
        (RiskLevel::High, RiskLevel::Low) => false,
        (RiskLevel::High, RiskLevel::Medium) => false,
        (RiskLevel::High, RiskLevel::High) => true,
    }
}

// ─── AutoApprovalGate ────────────────────────────────────────────────

/// Auto-approval gate that always approves requests.
///
/// Useful for testing and low-risk environments where no human
/// intervention is needed.
pub struct AutoApprovalGate;

#[async_trait::async_trait]
impl ApprovalGate for AutoApprovalGate {
    async fn request_approval(&self, _request: ApprovalRequest) -> Result<ApprovalResponse> {
        Ok(ApprovalResponse::Approved { modified_args: None })
    }
}

// ─── ApprovalDecision ────────────────────────────────────────────────

/// Helper for creating approval responses in UI handler code.
pub struct ApprovalDecision;

impl ApprovalDecision {
    /// Create an approval response (allow execution unchanged).
    pub fn approve() -> ApprovalResponse {
        ApprovalResponse::Approved { modified_args: None }
    }

    /// Create an approval response with modified arguments.
    pub fn approve_with_modifications(args: serde_json::Value) -> ApprovalResponse {
        ApprovalResponse::Approved { modified_args: Some(args) }
    }

    /// Create a denial response with a reason.
    pub fn deny(reason: impl Into<String>) -> ApprovalResponse {
        ApprovalResponse::Denied { reason: reason.into() }
    }

    /// Create a modification response (allow execution with different args).
    pub fn modify(args: serde_json::Value) -> ApprovalResponse {
        ApprovalResponse::Modified { args }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::traits::ApprovalGate;

    #[tokio::test]
    async fn test_blocking_approval_gate() {
        let gate = BlockingApprovalGate;
        let request = ApprovalRequest {
            tool_name: "shell".to_string(),
            args: serde_json::json!({"command": "rm -rf /"}),
            risk_level: RiskLevel::High,
            justification: "Delete everything".to_string(),
        };

        let response = gate.request_approval(request).await.unwrap();
        match response {
            ApprovalResponse::Denied { reason } => {
                assert!(reason.contains("placeholder"));
            }
            _ => panic!("Expected Denied response"),
        }
    }

    #[tokio::test]
    async fn test_auto_approval_gate() {
        let gate = AutoApprovalGate;
        let request = ApprovalRequest {
            tool_name: "calculator".to_string(),
            args: serde_json::json!({"expression": "2+2"}),
            risk_level: RiskLevel::Low,
            justification: "Simple calculation".to_string(),
        };

        let response = gate.request_approval(request).await.unwrap();
        match response {
            ApprovalResponse::Approved { modified_args } => {
                assert!(modified_args.is_none());
            }
            _ => panic!("Expected Approved response"),
        }
    }

    #[tokio::test]
    async fn test_channel_approval_gate_approve() {
        let (gate, mut receiver) = ChannelApprovalGateWithThreshold::new_manual_only(16);

        // Spawn a task that processes approval requests
        tokio::spawn(async move {
            while let Some(item) = receiver.recv().await {
                // Approve the request
                item.response_tx.send(ApprovalDecision::approve()).unwrap();
            }
        });

        let request = ApprovalRequest {
            tool_name: "shell".to_string(),
            args: serde_json::json!({"command": "ls"}),
            risk_level: RiskLevel::High,
            justification: "List files".to_string(),
        };

        let response = gate.request_approval(request).await.unwrap();
        match response {
            ApprovalResponse::Approved { modified_args } => {
                assert!(modified_args.is_none());
            }
            _ => panic!("Expected Approved response"),
        }
    }

    #[tokio::test]
    async fn test_channel_approval_gate_deny() {
        let (gate, mut receiver) = ChannelApprovalGateWithThreshold::new_manual_only(16);

        // Spawn a task that denies all requests
        tokio::spawn(async move {
            while let Some(item) = receiver.recv().await {
                item.response_tx.send(ApprovalDecision::deny("Security policy")).unwrap();
            }
        });

        let request = ApprovalRequest {
            tool_name: "shell".to_string(),
            args: serde_json::json!({"command": "rm -rf /"}),
            risk_level: RiskLevel::High,
            justification: "Delete everything".to_string(),
        };

        let response = gate.request_approval(request).await.unwrap();
        match response {
            ApprovalResponse::Denied { reason } => {
                assert_eq!(reason, "Security policy");
            }
            _ => panic!("Expected Denied response"),
        }
    }

    #[tokio::test]
    async fn test_channel_approval_gate_auto_approve_low() {
        // Threshold: Medium — Low risk tools are auto-approved, Medium and High go through channel
        let (gate, mut receiver) = ChannelApprovalGateWithThreshold::new(16, RiskLevel::Medium);

        // Spawn a task that would approve if reached
        tokio::spawn(async move {
            while let Some(item) = receiver.recv().await {
                item.response_tx.send(ApprovalDecision::approve()).unwrap();
            }
        });

        // Low risk should be auto-approved (no channel interaction)
        let request = ApprovalRequest {
            tool_name: "calculator".to_string(),
            args: serde_json::json!({"expression": "2+2"}),
            risk_level: RiskLevel::Low,
            justification: "Simple calculation".to_string(),
        };

        let response = gate.request_approval(request).await.unwrap();
        match response {
            ApprovalResponse::Approved { .. } => {}
            _ => panic!("Expected Approved response for low-risk auto-approve"),
        }
    }

    #[tokio::test]
    async fn test_channel_approval_gate_modify() {
        let (gate, mut receiver) = ChannelApprovalGateWithThreshold::new_manual_only(16);

        // Spawn a task that modifies the request args
        tokio::spawn(async move {
            while let Some(item) = receiver.recv().await {
                // Modify the command to a safer version
                item.response_tx.send(ApprovalDecision::modify(
                    serde_json::json!({"command": "ls /home"})
                )).unwrap();
            }
        });

        let request = ApprovalRequest {
            tool_name: "shell".to_string(),
            args: serde_json::json!({"command": "rm -rf /"}),
            risk_level: RiskLevel::High,
            justification: "Delete everything".to_string(),
        };

        let response = gate.request_approval(request).await.unwrap();
        match response {
            ApprovalResponse::Modified { args } => {
                assert_eq!(args["command"], "ls /home");
            }
            _ => panic!("Expected Modified response"),
        }
    }

    #[test]
    fn test_should_auto_approve() {
        // Low threshold: only Low auto-approves
        assert!(should_auto_approve(&RiskLevel::Low, &RiskLevel::Low));
        assert!(!should_auto_approve(&RiskLevel::Medium, &RiskLevel::Low));
        assert!(!should_auto_approve(&RiskLevel::High, &RiskLevel::Low));

        // Medium threshold: Low and Medium auto-approve
        assert!(should_auto_approve(&RiskLevel::Low, &RiskLevel::Medium));
        assert!(should_auto_approve(&RiskLevel::Medium, &RiskLevel::Medium));
        assert!(!should_auto_approve(&RiskLevel::High, &RiskLevel::Medium));

        // High threshold: all auto-approve
        assert!(should_auto_approve(&RiskLevel::Low, &RiskLevel::High));
        assert!(should_auto_approve(&RiskLevel::Medium, &RiskLevel::High));
        assert!(should_auto_approve(&RiskLevel::High, &RiskLevel::High));
    }

    #[test]
    fn test_approval_decision_helpers() {
        let approve = ApprovalDecision::approve();
        assert!(matches!(approve, ApprovalResponse::Approved { modified_args: None }));

        let approve_mod = ApprovalDecision::approve_with_modifications(serde_json::json!({"a": 1}));
        assert!(matches!(approve_mod, ApprovalResponse::Approved { modified_args: Some(_) }));

        let deny = ApprovalDecision::deny("test");
        assert!(matches!(deny, ApprovalResponse::Denied { reason: _ }));

        let modify = ApprovalDecision::modify(serde_json::json!({"b": 2}));
        assert!(matches!(modify, ApprovalResponse::Modified { args: _ }));
    }
}