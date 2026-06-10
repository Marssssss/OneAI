//! Callback-based approval bridge for iOS and HarmonyOS.
//!
//! This bridge uses a C ABI callback mechanism for bridging
//! approval requests to platform-native UI threads. The pattern:
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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use oneai_core::ApprovalResponse;
use oneai_tool::ApprovalPendingItem;

/// C callback types for the approval bridge.
///
/// These function pointer types define the C ABI that platform
/// code must implement and register.
pub type RequestCallback = extern "C" fn(request_json: *const std::ffi::c_char);
pub type ResponseCallback = extern "C" fn(response_json: *const std::ffi::c_char);

/// Callback-based approval bridge shared by iOS and HarmonyOS.
///
/// The bridge maintains:
/// - A channel receiver for pending approval items
/// - A registered request callback (set by the platform code)
/// - A HashMap of pending response senders keyed by request ID
pub struct CallbackApprovalBridge {
    /// Channel receiver for pending items.
    pending_rx: Mutex<tokio::sync::mpsc::Receiver<ApprovalPendingItem>>,
    /// Registered request callback (set by platform code at init).
    request_callback: Mutex<Option<RequestCallback>>,
    /// Pending response senders, keyed by request ID.
    response_senders: Mutex<HashMap<String, tokio::sync::oneshot::Sender<ApprovalResponse>>>,
}

impl CallbackApprovalBridge {
    /// Create a new callback bridge from a channel receiver.
    pub fn new(receiver: tokio::sync::mpsc::Receiver<ApprovalPendingItem>) -> Self {
        Self {
            pending_rx: Mutex::new(receiver),
            request_callback: Mutex::new(None),
            response_senders: Mutex::new(HashMap::new()),
        }
    }

    /// Register a request callback.
    ///
    /// The platform code calls this at init to register the function
    /// that will be called when a new approval request arrives.
    /// This callback receives a C string (JSON-encoded ApprovalRequest).
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
    ///    both the request ID and the approval request details
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

            // Store the response sender
            {
                let mut senders = self.response_senders.lock().unwrap();
                senders.insert(request_id.clone(), item.response_tx);
            }

            // Build the notification JSON
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
    /// `response_json` should be a JSON-encoded ApprovalResponse.
    ///
    /// Returns true if the response was successfully sent.
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