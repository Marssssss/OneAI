//! JNI bridge for Android interaction flow.
//!
//! Provides a bridge between the Rust channel-based interaction gate
//! and the Android Kotlin/Java side. The Kotlin code polls for
//! pending tool-approval requests via JNI, shows AlertDialog, and
//! sends responses back.
//!
//! The bridge maintains a HashMap of pending response senders,
//! keyed by a unique request ID, so that the Kotlin side can
//! respond to specific requests asynchronously.

use std::collections::HashMap;
use std::sync::Mutex;

use oneai_core::{InteractionRequest, InteractionResponse};
use oneai_tool::InteractionPendingItem;

/// JNI-compatible interaction bridge that tracks pending items by ID.
///
/// The Kotlin-side `OneAIInteractionHandler` uses this to:
/// 1. Poll for pending items (returns JSON + request ID)
/// 2. Show AlertDialog for each tool-approval item
/// 3. Send response (by request ID)
pub struct JniInteractionBridge {
    /// Pending response senders, keyed by request ID.
    pending_senders: Mutex<HashMap<String, tokio::sync::oneshot::Sender<InteractionResponse>>>,
}

impl JniInteractionBridge {
    /// Create a new JNI bridge.
    pub fn new() -> Self {
        Self {
            pending_senders: Mutex::new(HashMap::new()),
        }
    }

    /// Register a pending interaction item.
    ///
    /// Returns the request as a JSON string, with the response sender
    /// stored internally keyed by a unique request ID. Only `ToolApproval`
    /// items carry a meaningful request payload; other points are
    /// auto-responded `Proceed` (the gate config disables them anyway).
    pub fn register_pending(&self, item: InteractionPendingItem) -> String {
        let approval = match &item.request {
            InteractionRequest::ToolApproval { approval } => approval,
            _ => {
                let _ = item.response_tx.send(InteractionResponse::Proceed);
                return String::new();
            }
        };

        let request_id = format!("{}_{}", approval.tool_name, uuid::Uuid::new_v4());

        let mut senders = self.pending_senders.lock().unwrap();
        senders.insert(request_id.clone(), item.response_tx);

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
    }

    /// Send a response for a pending request by ID.
    ///
    /// `response_json` shape: `{ "decision": "approve"|"deny"|"modify", "reason": "...", "args": <json> }`.
    pub fn send_response_by_id(&self, request_id: &str, response_json: &str) -> bool {
        let response = parse_response_json(response_json);

        let mut senders = self.pending_senders.lock().unwrap();
        if let Some(sender) = senders.remove(request_id) {
            sender.send(response).is_ok()
        } else {
            false
        }
    }

    /// Check if there are any pending requests.
    pub fn has_pending(&self) -> bool {
        !self.pending_senders.lock().unwrap().is_empty()
    }
}

impl Default for JniInteractionBridge {
    fn default() -> Self {
        Self::new()
    }
}

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
            let args = value.get("args").cloned().unwrap_or(serde_json::Value::Null);
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
