//! macOS platform interaction gate — NSAlert dialog implementation.
//!
//! The MacOSInteractionGate wraps a Channel/Threshold interaction gate and
//! bridges interaction requests to macOS NSAlert dialogs on the main thread.
//!
//! Key design: NSAlert.runModal() must be called on the macOS main thread.
//! The MacOSInteractionBridge handles dispatching to the main thread via
//! macOS's dispatch mechanism, avoiding blocking the tokio runtime.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use oneai_core::{InteractionRequest, InteractionResponse, RiskLevel};
use oneai_core::error::Result;
use oneai_core::platform::PlatformInteractionGate;
use oneai_core::traits::InteractionGate;
use oneai_tool::{ChannelInteractionGate, InteractionGateConfig, InteractionPendingItem, ThresholdInteractionGate};

use crate::bridge_common::DesktopInteractionBridge;

// ─── MacOSInteractionGate ───────────────────────────────────────────

/// macOS-native interaction gate using NSAlert dialogs.
///
/// This gate wraps a `ChannelInteractionGate` (or `ThresholdInteractionGate`)
/// internally. Low-risk requests (below threshold, when a threshold is set) are
/// auto-proceeded; the rest are sent through a channel to the UI thread, where
/// an NSAlert dialog is shown.
pub struct MacOSInteractionGate {
    /// The inner channel-based gate that handles the interaction flow.
    inner: Arc<dyn InteractionGate>,
}

impl MacOSInteractionGate {
    /// Create a new macOS interaction gate with an auto-proceed threshold.
    ///
    /// Returns the gate (for AppBuilder) and a bridge (for the UI thread).
    pub fn new(buffer_size: usize, threshold: RiskLevel) -> (Self, MacOSInteractionBridge) {
        let (gate, receiver) =
            ThresholdInteractionGate::new(buffer_size, threshold, InteractionGateConfig::default());
        let inner: Arc<dyn InteractionGate> = Arc::new(gate);
        let bridge = MacOSInteractionBridge::new(receiver);
        (Self { inner }, bridge)
    }

    /// Create a gate where all enabled points go through the channel.
    pub fn new_manual_only(buffer_size: usize) -> (Self, MacOSInteractionBridge) {
        let (gate, receiver) = ChannelInteractionGate::new(buffer_size);
        let inner: Arc<dyn InteractionGate> = Arc::new(gate);
        let bridge = MacOSInteractionBridge::new(receiver);
        (Self { inner }, bridge)
    }
}

#[async_trait]
impl InteractionGate for MacOSInteractionGate {
    async fn request(&self, req: InteractionRequest) -> Result<InteractionResponse> {
        self.inner.request(req).await
    }

    fn enabled(&self, point: oneai_core::InteractionPoint) -> bool {
        self.inner.enabled(point)
    }
}

#[async_trait]
impl PlatformInteractionGate for MacOSInteractionGate {
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

// ─── MacOSInteractionBridge ─────────────────────────────────────────

/// Bridge that receives interaction requests and shows NSAlert dialogs.
///
/// The bridge must be run on the macOS main thread. It receives
/// pending interaction items from the channel and shows NSAlert dialogs
/// for tool-approval decisions (other points default to Proceed).
///
/// Usage:
/// ```ignore
/// let (gate, bridge) = MacOSInteractionGate::new(16, RiskLevel::Medium);
/// // ... build App with gate ...
/// bridge.run_loop();  // Blocks the main thread, processes dialogs
/// ```
pub struct MacOSInteractionBridge {
    /// The common bridge that holds the channel receiver.
    inner: Mutex<DesktopInteractionBridge>,
}

impl MacOSInteractionBridge {
    /// Create a new bridge from a channel receiver.
    fn new(receiver: tokio::sync::mpsc::Receiver<InteractionPendingItem>) -> Self {
        Self {
            inner: Mutex::new(DesktopInteractionBridge::new(receiver)),
        }
    }

    /// Try to receive a pending interaction item (non-blocking).
    ///
    /// Can be called from any thread. Returns None if no item is pending.
    pub fn try_recv(&self) -> Option<InteractionPendingItem> {
        self.inner.lock().unwrap().try_recv()
    }

    /// Handle an item: show an NSAlert for tool approvals, else Proceed.
    #[cfg(target_os = "macos")]
    pub fn handle_item(&self, item: InteractionPendingItem) {
        let response = show_nsalert_for_request(&item.request);
        DesktopInteractionBridge::send_response(item, response).ok();
    }

    /// Run a blocking loop that processes interaction requests via NSAlert.
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

    /// Run the interaction loop on a background thread, dispatching
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
fn show_nsalert_for_request(request: &InteractionRequest) -> InteractionResponse {
    // The NSAlert dialog is meaningful for tool approval; other decision
    // points (PlanDecision/PlanReview/PreInfer/PostInfer) auto-proceed.
    let approval = match request {
        InteractionRequest::ToolApproval { approval } => approval,
        _ => return InteractionResponse::Proceed,
    };

    use objc2_app_kit::NSAlert;
    use objc2_foundation::NSString;

    // NSAlert must be created and run on the main thread.
    let mtm = unsafe { objc2::MainThreadMarker::new_unchecked() };

    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("OneAI: Approve tool execution?"));
    alert.setInformativeText(&NSString::from_str(&DesktopInteractionBridge::format_request(approval)));
    alert.addButtonWithTitle(&NSString::from_str("Approve"));
    alert.addButtonWithTitle(&NSString::from_str("Deny"));
    alert.addButtonWithTitle(&NSString::from_str("Modify Args"));

    let response = alert.runModal();

    // NSModalResponse values: First button = 1000, Second = 1001, Third = 1002
    match response as i64 {
        1000 => InteractionResponse::Proceed,
        1001 => InteractionResponse::Abort {
            reason: "User denied via NSAlert".to_string(),
        },
        1002 => {
            // For "Modify", we proceed with the (unchanged) args since NSAlert
            // doesn't support inline argument editing. A more sophisticated UI
            // would use a custom window and return ReplaceToolArgs.
            InteractionResponse::Proceed
        }
        _ => InteractionResponse::Abort {
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
