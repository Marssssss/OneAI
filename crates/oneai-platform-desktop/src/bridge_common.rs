//! Common bridge infrastructure shared across desktop platforms.
//!
//! The DesktopApprovalBridge wraps the mpsc::Receiver and provides
//! a common interface for receiving pending approval items and
//! sending responses back.

use tokio::sync::mpsc;
use oneai_core::ApprovalResponse;
use oneai_tool::ApprovalPendingItem;

/// A shared bridge that holds the channel receiver for desktop approval items.
///
/// This is the platform-independent base that each platform-specific bridge
/// wraps. It provides the core channel receive/send mechanism.
pub struct DesktopApprovalBridge {
    /// Channel receiver for pending approval items.
    pending_rx: mpsc::Receiver<ApprovalPendingItem>,
}

impl DesktopApprovalBridge {
    /// Create a new bridge from a channel receiver.
    pub fn new(pending_rx: mpsc::Receiver<ApprovalPendingItem>) -> Self {
        Self { pending_rx }
    }

    /// Try to receive a pending approval item (non-blocking).
    ///
    /// Returns None if no item is available right now.
    pub fn try_recv(&mut self) -> Option<ApprovalPendingItem> {
        self.pending_rx.try_recv().ok()
    }

    /// Receive a pending approval item (blocking, async).
    ///
    /// Waits until an item is available or the channel is closed.
    pub async fn recv(&mut self) -> Option<ApprovalPendingItem> {
        self.pending_rx.recv().await
    }

    /// Send an approval response for a pending item.
    ///
    /// This sends the response back through the oneshot channel
    /// embedded in the pending item, unblocking the agent's
    /// approval request. Takes ownership of the item.
    pub fn send_response(item: ApprovalPendingItem, response: ApprovalResponse) -> Result<(), ()> {
        item.response_tx.send(response).map_err(|_| ())
    }

    /// Format an approval request for display in a native dialog.
    ///
    /// Returns a human-readable string summarizing the request.
    pub fn format_request(request: &oneai_core::ApprovalRequest) -> String {
        let risk_label = match request.risk_level {
            oneai_core::RiskLevel::Low => "Low",
            oneai_core::RiskLevel::Medium => "Medium",
            oneai_core::RiskLevel::High => "High ⚠️",
        };

        format!(
            "Tool: {}\nRisk Level: {}\nArguments: {}\n\nJustification: {}",
            request.tool_name,
            risk_label,
            serde_json::to_string_pretty(&request.args).unwrap_or_else(|_| request.args.to_string()),
            request.justification
        )
    }

    /// Blocking receive — waits for an item without requiring a tokio runtime.
    ///
    /// Uses a spin loop with short sleep intervals. This is used in
    /// platform UI thread contexts where tokio async receive is not available.
    pub fn recv_blocking(&mut self) -> Option<ApprovalPendingItem> {
        loop {
            match self.try_recv() {
                Some(item) => return Some(item),
                None => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        }
    }
}

// Helper for creating ApprovalResponse from dialog choices
/// Helper for creating approval responses from native dialog choices.
pub struct DesktopApprovalDecision;

impl DesktopApprovalDecision {
    /// Approve the request (allow execution unchanged).
    pub fn approve() -> ApprovalResponse {
        ApprovalResponse::Approved { modified_args: None }
    }

    /// Deny the request with a reason.
    pub fn deny(reason: impl Into<String>) -> ApprovalResponse {
        ApprovalResponse::Denied { reason: reason.into() }
    }

    /// Approve with modified arguments.
    pub fn modify(args: serde_json::Value) -> ApprovalResponse {
        ApprovalResponse::Modified { args }
    }
}