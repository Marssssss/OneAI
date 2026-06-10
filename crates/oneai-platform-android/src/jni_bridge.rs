//! JNI bridge for Android approval flow.
//!
//! Provides a bridge between the Rust channel-based approval gate
//! and the Android Kotlin/Java side. The Kotlin code polls for
//! pending approval requests via JNI, shows AlertDialog, and
//! sends responses back.
//!
//! The bridge maintains a HashMap of pending response senders,
//! keyed by a unique request ID, so that the Kotlin side can
//! respond to specific requests asynchronously.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use oneai_core::ApprovalResponse;
use oneai_tool::ApprovalPendingItem;

/// JNI-compatible approval bridge that tracks pending items by ID.
///
/// The Kotlin-side `OneAIApprovalHandler` uses this to:
/// 1. Poll for pending items (returns JSON + request ID)
/// 2. Show AlertDialog for each item
/// 3. Send response (by request ID)
pub struct JniApprovalBridge {
    /// Pending response senders, keyed by request ID.
    pending_senders: Mutex<HashMap<String, tokio::sync::oneshot::Sender<ApprovalResponse>>>,
}

impl JniApprovalBridge {
    /// Create a new JNI bridge.
    pub fn new() -> Self {
        Self {
            pending_senders: Mutex::new(HashMap::new()),
        }
    }

    /// Register a pending approval item.
    ///
    /// Returns the request as a JSON string, with the response sender
    /// stored internally keyed by a unique request ID.
    pub fn register_pending(&self, item: ApprovalPendingItem) -> String {
        let request_id = format!("{}_{}", item.request.tool_name, uuid::Uuid::new_v4());
        let request_json = serde_json::to_string(&item.request).unwrap_or_default();

        let mut senders = self.pending_senders.lock().unwrap();
        senders.insert(request_id.clone(), item.response_tx);

        // Return both the ID and the request JSON
        serde_json::json!({
            "request_id": request_id,
            "request": item.request,
        }).to_string()
    }

    /// Send a response for a pending approval request by ID.
    ///
    /// The Kotlin side calls this after the user responds to the AlertDialog.
    /// Returns true if the response was successfully sent.
    pub fn send_response_by_id(&self, request_id: &str, response_json: &str) -> bool {
        let response: ApprovalResponse = serde_json::from_str(response_json)
            .unwrap_or(ApprovalResponse::Denied {
                reason: "Failed to parse response JSON".to_string(),
            });

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

impl Default for JniApprovalBridge {
    fn default() -> Self {
        Self::new()
    }
}