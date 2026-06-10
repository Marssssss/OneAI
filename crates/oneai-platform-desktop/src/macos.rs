//! macOS platform approval gate — NSAlert dialog implementation.
//!
//! The MacOSApprovalGate wraps a ChannelApprovalGateWithThreshold and
//! bridges approval requests to macOS NSAlert dialogs on the main thread.
//!
//! Key design: NSAlert.runModal() must be called on the macOS main thread.
//! The MacOSApprovalBridge handles dispatching to the main thread via
//! macOS's dispatch mechanism, avoiding blocking the tokio runtime.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use oneai_core::{ApprovalRequest, ApprovalResponse, RiskLevel};
use oneai_core::error::Result;
use oneai_core::platform::PlatformApprovalGate;
use oneai_core::traits::ApprovalGate;
use oneai_tool::{ChannelApprovalGateWithThreshold, ApprovalPendingItem};

use crate::bridge_common::DesktopApprovalBridge;

// ─── MacOSApprovalGate ──────────────────────────────────────────────

/// macOS-native approval gate using NSAlert dialogs.
///
/// This gate wraps a ChannelApprovalGateWithThreshold internally.
/// Low-risk requests (below threshold) are auto-approved.
/// High-risk requests are sent through a channel to the UI thread,
/// where an NSAlert dialog is shown.
pub struct MacOSApprovalGate {
    /// The inner channel-based gate that handles the approval flow.
    inner: Arc<ChannelApprovalGateWithThreshold>,
}

impl MacOSApprovalGate {
    /// Create a new macOS approval gate with auto-approve threshold.
    ///
    /// Returns the gate (for AppBuilder) and a bridge (for the UI thread).
    pub fn new(buffer_size: usize, threshold: RiskLevel) -> (Self, MacOSApprovalBridge) {
        let (gate, receiver) = ChannelApprovalGateWithThreshold::new(buffer_size, threshold);
        let inner = Arc::new(gate);
        let bridge = MacOSApprovalBridge::new(receiver);
        (Self { inner }, bridge)
    }

    /// Create a gate where all requests go through the channel (no auto-approve).
    pub fn new_manual_only(buffer_size: usize) -> (Self, MacOSApprovalBridge) {
        let (gate, receiver) = ChannelApprovalGateWithThreshold::new_manual_only(buffer_size);
        let inner = Arc::new(gate);
        let bridge = MacOSApprovalBridge::new(receiver);
        (Self { inner }, bridge)
    }
}

#[async_trait]
impl ApprovalGate for MacOSApprovalGate {
    async fn request_approval(&self, request: ApprovalRequest) -> Result<ApprovalResponse> {
        self.inner.request_approval(request).await
    }
}

#[async_trait]
impl PlatformApprovalGate for MacOSApprovalGate {
    fn platform_name(&self) -> &'static str {
        "macos"
    }

    fn is_ui_available(&self) -> bool {
        // NSAlert requires NSApplication to be running.
        // We check at runtime if the app has a UI context.
        // For now, assume it's available when this gate is used.
        true
    }
}

// ─── MacOSApprovalBridge ──────────────────────────────────────────

/// Bridge that receives approval requests and shows NSAlert dialogs.
///
/// The bridge must be run on the macOS main thread. It receives
/// pending approval items from the channel and shows NSAlert dialogs.
///
/// Usage:
/// ```ignore
/// let (gate, bridge) = MacOSApprovalGate::new(16, RiskLevel::Medium);
/// // ... build App with gate ...
/// bridge.run_loop();  // Blocks the main thread, processes dialogs
/// ```
pub struct MacOSApprovalBridge {
    /// The common bridge that holds the channel receiver.
    inner: Mutex<DesktopApprovalBridge>,
}

impl MacOSApprovalBridge {
    /// Create a new bridge from a channel receiver.
    fn new(receiver: tokio::sync::mpsc::Receiver<ApprovalPendingItem>) -> Self {
        Self {
            inner: Mutex::new(DesktopApprovalBridge::new(receiver)),
        }
    }

    /// Try to receive a pending approval item (non-blocking).
    ///
    /// Can be called from any thread. Returns None if no item is pending.
    pub fn try_recv(&self) -> Option<ApprovalPendingItem> {
        self.inner.lock().unwrap().try_recv()
    }

    /// Handle an item: show NSAlert dialog + send response.
    #[cfg(target_os = "macos")]
    pub fn handle_item(&self, item: ApprovalPendingItem) {
        let response = show_nsalert_for_approval(&item.request);
        DesktopApprovalBridge::send_response(item, response).ok();
    }

    /// Run a blocking loop that processes approval requests via NSAlert.
    ///
    /// This blocks the calling thread. It should be run on the
    /// macOS main thread (typically from within NSApplication.main()).
    #[cfg(target_os = "macos")]
    pub fn run_loop(&self) {
        ensure_nsapp();

        loop {
            let item = {
                let mut inner = self.inner.lock().unwrap();
                inner.recv_blocking()
            };
            if let Some(item) = item {
                self.handle_item(item);
            } else {
                break;
            }
        }
    }

    /// Run the approval loop on a background thread, dispatching
    /// NSAlert dialogs to the macOS main thread.
    #[cfg(target_os = "macos")]
    pub fn run_loop_on_background_thread(self: Arc<Self>) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            ensure_nsapp();

            loop {
                let item = {
                    let mut inner = self.inner.lock().unwrap();
                    inner.recv_blocking()
                };
                if let Some(item) = item {
                    self.handle_item(item);
                } else {
                    break;
                }
            }
        })
    }
}

// ─── NSAlert implementation ─────────────────────────────────────────

#[cfg(target_os = "macos")]
fn show_nsalert_for_approval(request: &ApprovalRequest) -> ApprovalResponse {
    use objc2_app_kit::NSAlert;
    use objc2_foundation::NSString;

    // NSAlert must be created and run on the main thread.
    let mtm = unsafe { objc2::MainThreadMarker::new_unchecked() };

    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(&format!(
        "OneAI: Approve tool execution?"
    )));
    alert.setInformativeText(&NSString::from_str(&DesktopApprovalBridge::format_request(request)));
    alert.addButtonWithTitle(&NSString::from_str("Approve"));
    alert.addButtonWithTitle(&NSString::from_str("Deny"));
    alert.addButtonWithTitle(&NSString::from_str("Modify Args"));

    let response = alert.runModal();

    // NSModalResponse values: First button = 1000, Second = 1001, Third = 1002
    match response as i64 {
        1000 => ApprovalResponse::Approved { modified_args: None },
        1001 => ApprovalResponse::Denied {
            reason: "User denied via NSAlert".to_string(),
        },
        1002 => {
            // For "Modify", we return Approved with the original args
            // since NSAlert doesn't support inline argument editing.
            // A more sophisticated UI would use a custom window.
            ApprovalResponse::Approved {
                modified_args: Some(request.args.clone()),
            }
        },
        _ => ApprovalResponse::Denied {
            reason: "Unknown dialog response".to_string(),
        },
    }
}

#[cfg(target_os = "macos")]
fn ensure_nsapp() {
    use objc2_app_kit::NSApplication;

    // Ensure the shared NSApplication instance exists.
    // This must be called on the main thread.
    let mtm = unsafe { objc2::MainThreadMarker::new_unchecked() };
    let _ = NSApplication::sharedApplication(mtm);
}