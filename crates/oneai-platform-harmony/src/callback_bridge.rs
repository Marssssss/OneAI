//! Callback-based interaction bridge for HarmonyOS.
//!
//! Same implementation as iOS — uses C ABI callback mechanism.
//! See `oneai-platform-ios/src/callback_bridge.rs` for detailed docs.

use std::collections::HashMap;
use std::sync::Mutex;

use oneai_core::{InteractionRequest, InteractionResponse};
use oneai_tool::InteractionPendingItem;

/// C callback types for the interaction bridge.
pub type RequestCallback = extern "C" fn(request_json: *const std::ffi::c_char);
#[allow(dead_code)]
pub type ResponseCallback = extern "C" fn(response_json: *const std::ffi::c_char);

/// Callback-based interaction bridge for HarmonyOS.
///
/// Same pattern as iOS: the ArkTS-side `InteractionDialogManager`
/// registers a C callback, receives JSON-encoded tool-approval requests,
/// shows CommonDialog, and calls the response callback.
pub struct CallbackInteractionBridge {
    pending_rx: Mutex<tokio::sync::mpsc::Receiver<InteractionPendingItem>>,
    request_callback: Mutex<Option<RequestCallback>>,
    response_senders: Mutex<HashMap<String, tokio::sync::oneshot::Sender<InteractionResponse>>>,
}

impl CallbackInteractionBridge {
    pub fn new(receiver: tokio::sync::mpsc::Receiver<InteractionPendingItem>) -> Self {
        Self {
            pending_rx: Mutex::new(receiver),
            request_callback: Mutex::new(None),
            response_senders: Mutex::new(HashMap::new()),
        }
    }

    pub fn register_request_callback(&self, callback: RequestCallback) {
        let mut cb = self.request_callback.lock().unwrap();
        *cb = Some(callback);
    }

    pub fn poll_and_notify(&self) -> bool {
        let callback = {
            let cb = self.request_callback.lock().unwrap();
            *cb
        };

        if callback.is_none() {
            return false;
        }

        let item = {
            let mut rx = self.pending_rx.lock().unwrap();
            rx.try_recv().ok()
        };

        if let Some(item) = item {
            let request_id = uuid::Uuid::new_v4().to_string();

            let approval = match &item.request {
                InteractionRequest::ToolApproval { approval } => approval,
                _ => {
                    let _ = item.response_tx.send(InteractionResponse::Proceed);
                    return false;
                }
            };

            {
                let mut senders = self.response_senders.lock().unwrap();
                senders.insert(request_id.clone(), item.response_tx);
            }

            let notification = serde_json::json!({
                "request_id": request_id,
                "request": {
                    "tool_name": approval.tool_name,
                    "args": approval.args,
                    "risk_level": approval.risk_level,
                    "justification": approval.justification,
                },
            });
            let notification_json = notification.to_string();

            let c_string = std::ffi::CString::new(notification_json).unwrap();
            if let Some(cb) = callback {
                cb(c_string.as_ptr());
                return true;
            }
        }

        false
    }

    /// Send a response for a pending request by ID.
    ///
    /// `response_json` shape: `{ "decision": "approve"|"deny"|"modify", "reason": "...", "args": <json> }`.
    pub fn send_response_by_id(&self, request_id: &str, response_json: &str) -> bool {
        let response = parse_response_json(response_json);

        let mut senders = self.response_senders.lock().unwrap();
        if let Some(sender) = senders.remove(request_id) {
            sender.send(response).is_ok()
        } else {
            false
        }
    }

    pub fn has_pending(&self) -> bool {
        !self.response_senders.lock().unwrap().is_empty()
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
