//! iOS interaction gate — wraps `ChannelInteractionGate` / `ThresholdInteractionGate`.
//!
//! P4 slimmed this to a fallback shell: the bus path uses a `BusInteractionGate`
//! (an active group round / single-agent `run_turn_via_bus` resolves tool
//! approvals as `EngineYield::ApprovalRequest` ↔ `Directive::Approve`), so the
//! C-callback `CallbackInteractionBridge` (which ferried approval items to a
//! foreign function pointer) is gone. What remains is a plain
//! `IOSInteractionGate` (`PlatformInteractionGate` for a non-bus uniffi app)
//! and an `IOSInteractionBridge` that holds the gate's pending-item channel so
//! a Swift caller can poll requests and send responses by id — the same shape
//! the Android bridge has always had, with no C callback machinery.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use oneai_core::error::Result;
use oneai_core::platform::PlatformInteractionGate;
use oneai_core::traits::InteractionGate;
use oneai_core::{InteractionPoint, InteractionRequest, InteractionResponse, RiskLevel};
use oneai_tool::{
    ChannelInteractionGate, InteractionGateConfig, InteractionPendingItem, ThresholdInteractionGate,
};

/// The tool-approval-only gate config used by mobile gates: only
/// `ToolApproval` is enabled, so the bridge only ever sees tool-approval
/// items (matching the legacy `ApprovalGate` behaviour).
fn mobile_config() -> InteractionGateConfig {
    InteractionGateConfig {
        preinfer: false,
        postinfer: false,
        tool_approval: true,
        plan_decision: false,
        plan_review: false,
        network_approval: false,
    }
}

/// iOS-native interaction gate. Low-risk requests (below `threshold`, when
/// set) are auto-proceeded; the rest go through a channel to the bridge.
pub struct IOSInteractionGate {
    inner: Arc<dyn InteractionGate>,
}

impl IOSInteractionGate {
    /// Create a new iOS interaction gate with an auto-proceed threshold.
    pub fn new(buffer_size: usize, threshold: RiskLevel) -> (Self, IOSInteractionBridge) {
        let (gate, receiver) =
            ThresholdInteractionGate::new(buffer_size, threshold, mobile_config());
        let inner: Arc<dyn InteractionGate> = Arc::new(gate);
        let bridge = IOSInteractionBridge::new(receiver);
        (Self { inner }, bridge)
    }

    /// Create a gate where tool-approval requests go through the channel.
    pub fn new_manual_only(buffer_size: usize) -> (Self, IOSInteractionBridge) {
        let (gate, receiver) = ChannelInteractionGate::with_config(buffer_size, mobile_config());
        let inner: Arc<dyn InteractionGate> = Arc::new(gate);
        let bridge = IOSInteractionBridge::new(receiver);
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

/// Bridge that holds the channel receiver for iOS interaction items.
///
/// P4 removed the C-callback poll layer (the bus path resolves approvals as
/// `EngineYield::ApprovalRequest` ↔ `Directive::Approve`). This shell remains
/// for a non-bus uniffi app: the Swift side polls `poll_pending_json()` (gets
/// a request + id), presents a `UIAlertController`, and calls
/// `send_response_by_id()` to resolve it.
pub struct IOSInteractionBridge {
    inner: Mutex<tokio::sync::mpsc::Receiver<InteractionPendingItem>>,
    /// Pending response senders, keyed by request id (so the foreign side can
    /// reply asynchronously).
    response_senders: Mutex<HashMap<String, tokio::sync::oneshot::Sender<InteractionResponse>>>,
}

impl IOSInteractionBridge {
    fn new(receiver: tokio::sync::mpsc::Receiver<InteractionPendingItem>) -> Self {
        Self {
            inner: Mutex::new(receiver),
            response_senders: Mutex::new(HashMap::new()),
        }
    }

    /// Poll for a pending tool-approval request (non-blocking). Returns a
    /// JSON object `{request_id, request}` the foreign side renders, or `None`
    /// if nothing is pending. Items that aren't `ToolApproval` (shouldn't
    /// arrive under the tool-approval-only config) are auto-proceeded.
    pub fn poll_pending_json(&self) -> Option<String> {
        let item = self.inner.lock().unwrap().try_recv().ok()?;
        let approval = match &item.request {
            InteractionRequest::ToolApproval { approval } => approval,
            _ => {
                let _ = item.response_tx.send(InteractionResponse::Proceed);
                return None;
            }
        };
        let request_id = format!("{}_{}", approval.tool_name, uuid::Uuid::new_v4());
        self.response_senders
            .lock()
            .unwrap()
            .insert(request_id.clone(), item.response_tx);
        serde_json::json!({
            "request_id": request_id,
            "request": {
                "tool_name": approval.tool_name,
                "args": approval.args,
                "risk_level": approval.risk_level,
                "justification": approval.justification,
            },
        })
        .to_string()
        .into()
    }

    /// Send a response for a pending request by id. `response_json` shape:
    /// `{ "decision": "approve"|"deny"|"modify", "reason": "...", "args": <json> }`.
    pub fn send_response_by_id(&self, request_id: &str, response_json: &str) -> bool {
        let response = parse_response_json(response_json);
        let mut senders = self.response_senders.lock().unwrap();
        if let Some(sender) = senders.remove(request_id) {
            sender.send(response).is_ok()
        } else {
            false
        }
    }

    /// Check if there are any pending requests.
    pub fn has_pending(&self) -> bool {
        !self.response_senders.lock().unwrap().is_empty()
    }
}

/// Parse a platform-supplied response JSON into an [`InteractionResponse`].
/// Unknown / malformed input defaults to `Abort` (deny) for safety.
fn parse_response_json(response_json: &str) -> InteractionResponse {
    let value: serde_json::Value = match serde_json::from_str(response_json) {
        Ok(v) => v,
        Err(_) => {
            return InteractionResponse::Abort {
                reason: "Failed to parse response JSON".to_string(),
            }
        }
    };
    let decision = value
        .get("decision")
        .and_then(|v| v.as_str())
        .unwrap_or("deny");
    match decision {
        "approve" => InteractionResponse::Proceed,
        "modify" => {
            let args = value
                .get("args")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            InteractionResponse::ProceedWith {
                modification: oneai_core::InteractionModification::ReplaceToolArgs(args),
            }
        }
        _ => InteractionResponse::Abort {
            reason: value
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("User denied via platform dialog")
                .to_string(),
        },
    }
}
