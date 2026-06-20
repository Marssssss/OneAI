//! Callback-based approval bridge for HarmonyOS.
//!
//! Same implementation as iOS — uses C ABI callback mechanism.
//! See `oneai-platform-ios/src/callback_bridge.rs` for detailed docs.

use std::collections::HashMap;
use std::sync::Mutex;

use oneai_core::ApprovalResponse;
use oneai_tool::ApprovalPendingItem;

/// C callback types for the approval bridge.
pub type RequestCallback = extern "C" fn(request_json: *const std::ffi::c_char);
#[allow(dead_code)]
pub type ResponseCallback = extern "C" fn(response_json: *const std::ffi::c_char);

/// Callback-based approval bridge for HarmonyOS.
///
/// Same pattern as iOS: the ArkTS-side `ApprovalDialogManager`
/// registers a C callback, receives JSON-encoded requests, shows
/// CommonDialog, and calls the response callback.
pub struct CallbackApprovalBridge {
    pending_rx: Mutex<tokio::sync::mpsc::Receiver<ApprovalPendingItem>>,
    request_callback: Mutex<Option<RequestCallback>>,
    response_senders: Mutex<HashMap<String, tokio::sync::oneshot::Sender<ApprovalResponse>>>,
}

impl CallbackApprovalBridge {
    pub fn new(receiver: tokio::sync::mpsc::Receiver<ApprovalPendingItem>) -> Self {
        Self {
            pending_rx: Mutex::new(receiver),
            request_callback: Mutex::new(None),
            response_senders: Mutex::new(HashMap::new()),
        }
    }

    /// Register a request callback (set by ArkTS code at init).
    pub fn register_request_callback(&self, callback: RequestCallback) {
        let mut cb = self.request_callback.lock().unwrap();
        *cb = Some(callback);
    }

    /// Poll for a pending item and call the registered callback.
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

            {
                let mut senders = self.response_senders.lock().unwrap();
                senders.insert(request_id.clone(), item.response_tx);
            }

            let notification = serde_json::json!({
                "request_id": request_id,
                "request": {
                    "tool_name": item.request.tool_name,
                    "args": item.request.args,
                    "risk_level": item.request.risk_level,
                    "justification": item.request.justification,
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
    pub fn send_response_by_id(&self, request_id: &str, response_json: &str) -> bool {
        let response: ApprovalResponse = serde_json::from_str(response_json)
            .unwrap_or(ApprovalResponse::Denied {
                reason: "Failed to parse response JSON".to_string(),
            });

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