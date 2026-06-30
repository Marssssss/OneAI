//! Callback-based interaction bridge for iOS and HarmonyOS.
//!
//! This bridge uses a C ABI callback mechanism for bridging
//! interaction requests to platform-native UI threads. The pattern:
//!
//! 1. The Rust side polls for pending items from the channel
//! 2. When an item arrives, it calls the registered C callback
//!    with a JSON-encoded request string
//! 3. The platform UI code (Swift/ArkTS) receives the callback,
//!    shows a native dialog, and calls the response callback
//! 4. The response is sent back through the oneshot channel
//!
//! This avoids needing full Swift/Rust or ArkTS/Rust FFI bindings —
//! a simple C ABI callback is easy to wire up from any platform.
//!
//! Only the `ToolApproval` decision point is bridged to the native
//! dialog (the gate is configured with `tool_approval` as the sole
//! enabled point); other points are short-circuited by the gate.

use std::collections::HashMap;
use std::sync::Mutex;

use oneai_core::{InteractionRequest, InteractionResponse};
use oneai_tool::InteractionPendingItem;

/// C callback types for the interaction bridge.
///
/// These function pointer types define the C ABI that platform
/// code must implement and register.
pub type RequestCallback = extern "C" fn(request_json: *const std::ffi::c_char);
#[allow(dead_code)]
pub type ResponseCallback = extern "C" fn(response_json: *const std::ffi::c_char);

/// Callback-based interaction bridge shared by iOS and HarmonyOS.
///
/// The bridge maintains:
/// - A channel receiver for pending interaction items
/// - A registered request callback (set by the platform code)
/// - A HashMap of pending response senders keyed by request ID
pub struct CallbackInteractionBridge {
    /// Channel receiver for pending items.
    pending_rx: Mutex<tokio::sync::mpsc::Receiver<InteractionPendingItem>>,
    /// Registered request callback (set by platform code at init).
    request_callback: Mutex<Option<RequestCallback>>,
    /// Pending response senders, keyed by request ID.
    response_senders: Mutex<HashMap<String, tokio::sync::oneshot::Sender<InteractionResponse>>>,
}

impl CallbackInteractionBridge {
    /// Create a new callback bridge from a channel receiver.
    pub fn new(receiver: tokio::sync::mpsc::Receiver<InteractionPendingItem>) -> Self {
        Self {
            pending_rx: Mutex::new(receiver),
            request_callback: Mutex::new(None),
            response_senders: Mutex::new(HashMap::new()),
        }
    }

    /// Register a request callback.
    ///
    /// The platform code calls this at init to register the function
    /// that will be called when a new tool-approval request arrives.
    /// This callback receives a C string (JSON-encoded request).
    pub fn register_request_callback(&self, callback: RequestCallback) {
        let mut cb = self.request_callback.lock().unwrap();
        *cb = Some(callback);
    }

    /// Poll for a pending item and call the registered callback.
    ///
    /// If a request callback is registered and an item is pending,
    /// this method:
    /// 1. Takes the item from the channel
    /// 2. Generates a unique request ID
    /// 3. Stores the response sender keyed by the ID
    /// 4. Calls the request callback with a JSON string containing
    ///    both the request ID and the tool-approval request details
    ///
    /// Items that are not `ToolApproval` (shouldn't arrive under the
    /// tool-approval-only gate config) are auto-responded `Proceed`.
    ///
    /// Returns true if a callback was invoked, false otherwise.
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

            // Only ToolApproval is bridged to the native dialog; anything
            // else is auto-proceeded.
            let approval = match &item.request {
                InteractionRequest::ToolApproval { approval } => approval,
                _ => {
                    let _ = item.response_tx.send(InteractionResponse::Proceed);
                    return false;
                }
            };

            // Store the response sender
            {
                let mut senders = self.response_senders.lock().unwrap();
                senders.insert(request_id.clone(), item.response_tx);
            }

            // Build the notification JSON
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

            // Call the registered callback with the C string
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
    /// The platform code calls this after the user responds to the dialog.
    /// `response_json` should be a JSON object of the form:
    /// `{ "decision": "approve" | "deny" | "modify", "reason": "...", "args": <json> }`
    /// which maps to `Proceed` / `Abort` / `ProceedWith{ReplaceToolArgs}`.
    ///
    /// Returns true if the response was successfully sent.
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
///
/// Accepted shape: `{ "decision": "approve"|"deny"|"modify", "reason": "...", "args": <json> }`.
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
