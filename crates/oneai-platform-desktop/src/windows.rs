//! Windows platform approval gate — MessageBox implementation.
//!
//! The WindowsApprovalGate wraps a ChannelApprovalGateWithThreshold and
//! bridges approval requests to Windows MessageBox dialogs.
//!
//! MessageBox supports Yes/No/Cancel which maps to Approve/Deny/Modify.
//! For more sophisticated UI (argument modification), a custom dialog
//! would be needed (beyond Phase 6 scope).

use std::sync::Mutex;

use async_trait::async_trait;
use oneai_core::{ApprovalRequest, ApprovalResponse, RiskLevel};
use oneai_core::error::Result;
use oneai_core::platform::PlatformApprovalGate;
use oneai_core::traits::ApprovalGate;
use oneai_tool::{ChannelApprovalGateWithThreshold, ApprovalPendingItem};

use crate::bridge_common::DesktopApprovalBridge;

// ─── WindowsApprovalGate ──────────────────────────────────────────

/// Windows-native approval gate using MessageBox dialogs.
///
/// This gate wraps a ChannelApprovalGateWithThreshold internally.
/// Low-risk requests (below threshold) are auto-approved.
/// High-risk requests are sent through a channel to the UI thread,
/// where a MessageBox dialog is shown.
pub struct WindowsApprovalGate {
    inner: Arc<ChannelApprovalGateWithThreshold>,
}

use std::sync::Arc;

impl WindowsApprovalGate {
    /// Create a new Windows approval gate with auto-approve threshold.
    pub fn new(buffer_size: usize, threshold: RiskLevel) -> (Self, WindowsApprovalBridge) {
        let (gate, receiver) = ChannelApprovalGateWithThreshold::new(buffer_size, threshold);
        let inner = Arc::new(gate);
        let bridge = WindowsApprovalBridge::new(receiver);
        (Self { inner }, bridge)
    }

    /// Create a gate where all requests go through the channel.
    pub fn new_manual_only(buffer_size: usize) -> (Self, WindowsApprovalBridge) {
        let (gate, receiver) = ChannelApprovalGateWithThreshold::new_manual_only(buffer_size);
        let inner = Arc::new(gate);
        let bridge = WindowsApprovalBridge::new(receiver);
        (Self { inner }, bridge)
    }
}

#[async_trait]
impl ApprovalGate for WindowsApprovalGate {
    async fn request_approval(&self, request: ApprovalRequest) -> Result<ApprovalResponse> {
        self.inner.request_approval(request).await
    }
}

#[async_trait]
impl PlatformApprovalGate for WindowsApprovalGate {
    fn platform_name(&self) -> &'static str {
        "windows"
    }

    fn is_ui_available(&self) -> bool {
        true
    }
}

// ─── WindowsApprovalBridge ──────────────────────────────────────

/// Bridge that receives approval requests and shows MessageBox dialogs.
pub struct WindowsApprovalBridge {
    inner: Mutex<DesktopApprovalBridge>,
}

impl WindowsApprovalBridge {
    fn new(receiver: tokio::sync::mpsc::Receiver<ApprovalPendingItem>) -> Self {
        Self {
            inner: Mutex::new(DesktopApprovalBridge::new(receiver)),
        }
    }

    /// Try to receive a pending approval item (non-blocking).
    pub fn try_recv(&self) -> Option<ApprovalPendingItem> {
        self.inner.lock().unwrap().try_recv()
    }

    /// Show a MessageBox for an approval request.
    #[cfg(target_os = "windows")]
    pub fn show_messagebox(&self, item: &ApprovalPendingItem) -> ApprovalResponse {
        show_messagebox_for_approval(&item.request)
    }

    /// Handle an item: show dialog + send response.
    #[cfg(target_os = "windows")]
    pub fn handle_item(&self, item: ApprovalPendingItem) {
        let response = show_messagebox_for_approval(&item.request);
        DesktopApprovalBridge::send_response(item, response).ok();
    }

    /// Run a blocking loop that processes approval requests via MessageBox.
    #[cfg(target_os = "windows")]
    pub fn run_loop(&self) {
        loop {
            let item = {
                let mut inner = self.inner.lock().unwrap();
                inner.recv_blocking()
            };
            if let Some(item) = item {
                self.handle_item(&item);
            } else {
                break;
            }
        }
    }
}

// ─── MessageBox implementation ──────────────────────────────────────

#[cfg(target_os = "windows")]
fn show_messagebox_for_approval(request: &ApprovalRequest) -> ApprovalResponse {
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::core::HSTRING;

    let title = HSTRING::from(format!("OneAI: Approve '{}'", request.tool_name));
    let message = HSTRING::from(DesktopApprovalBridge::format_request(request));

    // MB_YESNOCANCEL: Yes=Approve, No=Deny, Cancel=Modify
    let result = MessageBoxW(
        None,  // No parent window
        &message,
        &title,
        MB_YESNOCANCEL | MB_ICONQUESTION | MB_DEFBUTTON2,  // Default to "No" (deny)
    );

    match result {
        IDYES => ApprovalResponse::Approved { modified_args: None },
        IDNO => ApprovalResponse::Denied {
            reason: "User denied via MessageBox".to_string(),
        },
        IDCANCEL => ApprovalResponse::Modified {
            args: request.args.clone(),
        },
        _ => ApprovalResponse::Denied {
            reason: "Unknown dialog response".to_string(),
        },
    }
}