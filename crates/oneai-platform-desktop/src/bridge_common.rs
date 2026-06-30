//! Common bridge infrastructure shared across desktop platforms.
//!
//! The DesktopInteractionBridge wraps the mpsc::Receiver and provides
//! a common interface for receiving pending interaction items and
//! sending responses back.

use tokio::sync::mpsc;
use oneai_core::InteractionResponse;
use oneai_tool::InteractionPendingItem;

/// A shared bridge that holds the channel receiver for desktop interaction items.
///
/// This is the platform-independent base that each platform-specific bridge
/// wraps. It provides the core channel receive/send mechanism.
pub struct DesktopInteractionBridge {
    /// Channel receiver for pending interaction items.
    pending_rx: mpsc::Receiver<InteractionPendingItem>,
}

impl DesktopInteractionBridge {
    /// Create a new bridge from a channel receiver.
    pub fn new(pending_rx: mpsc::Receiver<InteractionPendingItem>) -> Self {
        Self { pending_rx }
    }

    /// Try to receive a pending interaction item (non-blocking).
    ///
    /// Returns None if no item is available right now.
    pub fn try_recv(&mut self) -> Option<InteractionPendingItem> {
        self.pending_rx.try_recv().ok()
    }

    /// Receive a pending interaction item (blocking, async).
    ///
    /// Waits until an item is available or the channel is closed.
    pub async fn recv(&mut self) -> Option<InteractionPendingItem> {
        self.pending_rx.recv().await
    }

    /// Send an interaction response for a pending item.
    ///
    /// This sends the response back through the oneshot channel
    /// embedded in the pending item, unblocking the agent's
    /// interaction request. Takes ownership of the item.
    pub fn send_response(item: InteractionPendingItem, response: InteractionResponse) -> Result<(), ()> {
        item.response_tx.send(response).map_err(|_| ())
    }

    /// Format a tool-approval request for display in a native dialog.
    ///
    /// Returns a human-readable string summarizing the request. Used for the
    /// `InteractionRequest::ToolApproval` point (which carries an `ApprovalRequest`).
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
    pub fn recv_blocking(&mut self) -> Option<InteractionPendingItem> {
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

// Helper for creating InteractionResponse from dialog choices
/// Helper for creating interaction responses from native dialog choices.
#[allow(dead_code)]
pub struct DesktopInteractionDecision;

#[allow(dead_code)]
impl DesktopInteractionDecision {
    /// Proceed with the request unchanged (approve).
    pub fn approve() -> InteractionResponse {
        InteractionResponse::Proceed
    }

    /// Abort the request with a reason (deny).
    pub fn deny(reason: impl Into<String>) -> InteractionResponse {
        InteractionResponse::Abort { reason: reason.into() }
    }

    /// Proceed with replaced tool arguments (approve with modification).
    pub fn modify(args: serde_json::Value) -> InteractionResponse {
        InteractionResponse::ProceedWith {
            modification: oneai_core::InteractionModification::ReplaceToolArgs(args),
        }
    }
}
