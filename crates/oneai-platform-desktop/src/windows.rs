//! Windows platform interaction gate — MessageBox implementation.
//!
//! The WindowsInteractionGate wraps a Channel/Threshold interaction gate and
//! bridges interaction requests to Windows MessageBox dialogs.
//!
//! MessageBox supports Yes/No/Cancel which maps to Proceed/Abort/ProceedWith.
//! For more sophisticated UI (argument modification), a custom dialog
//! would be needed (beyond Phase 6 scope).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use oneai_core::{InteractionPoint, InteractionRequest, InteractionResponse, RiskLevel};
use oneai_core::error::Result;
use oneai_core::platform::PlatformInteractionGate;
use oneai_core::traits::InteractionGate;
use oneai_tool::{ChannelInteractionGate, InteractionGateConfig, InteractionPendingItem, ThresholdInteractionGate};

use crate::bridge_common::DesktopInteractionBridge;

// ─── WindowsInteractionGate ────────────────────────────────────────

/// Windows-native interaction gate using MessageBox dialogs.
///
/// This gate wraps a `ChannelInteractionGate` (or `ThresholdInteractionGate`)
/// internally. Low-risk requests (below threshold, when a threshold is set) are
/// auto-proceeded; the rest go through a channel to the UI thread, where a
/// MessageBox dialog is shown.
pub struct WindowsInteractionGate {
    inner: Arc<dyn InteractionGate>,
}

impl WindowsInteractionGate {
    /// Create a new Windows interaction gate with an auto-proceed threshold.
    pub fn new(buffer_size: usize, threshold: RiskLevel) -> (Self, WindowsInteractionBridge) {
        let (gate, receiver) =
            ThresholdInteractionGate::new(buffer_size, threshold, InteractionGateConfig::default());
        let inner: Arc<dyn InteractionGate> = Arc::new(gate);
        let bridge = WindowsInteractionBridge::new(receiver);
        (Self { inner }, bridge)
    }

    /// Create a gate where all enabled points go through the channel.
    pub fn new_manual_only(buffer_size: usize) -> (Self, WindowsInteractionBridge) {
        let (gate, receiver) = ChannelInteractionGate::new(buffer_size);
        let inner: Arc<dyn InteractionGate> = Arc::new(gate);
        let bridge = WindowsInteractionBridge::new(receiver);
        (Self { inner }, bridge)
    }
}

#[async_trait]
impl InteractionGate for WindowsInteractionGate {
    async fn request(&self, req: InteractionRequest) -> Result<InteractionResponse> {
        self.inner.request(req).await
    }

    fn enabled(&self, point: InteractionPoint) -> bool {
        self.inner.enabled(point)
    }
}

#[async_trait]
impl PlatformInteractionGate for WindowsInteractionGate {
    fn platform_name(&self) -> &'static str {
        "windows"
    }

    fn is_ui_available(&self) -> bool {
        true
    }
}

// ─── WindowsInteractionBridge ──────────────────────────────────────

/// Bridge that receives interaction requests and shows MessageBox dialogs.
pub struct WindowsInteractionBridge {
    inner: Mutex<DesktopInteractionBridge>,
}

impl WindowsInteractionBridge {
    fn new(receiver: tokio::sync::mpsc::Receiver<InteractionPendingItem>) -> Self {
        Self {
            inner: Mutex::new(DesktopInteractionBridge::new(receiver)),
        }
    }

    /// Try to receive a pending interaction item (non-blocking).
    pub fn try_recv(&self) -> Option<InteractionPendingItem> {
        self.inner.lock().unwrap().try_recv()
    }

    /// Show a MessageBox for an interaction request.
    #[cfg(target_os = "windows")]
    pub fn show_messagebox(&self, item: &InteractionPendingItem) -> InteractionResponse {
        show_messagebox_for_request(&item.request)
    }

    /// Handle an item: show dialog + send response.
    #[cfg(target_os = "windows")]
    pub fn handle_item(&self, item: InteractionPendingItem) {
        let response = show_messagebox_for_request(&item.request);
        DesktopInteractionBridge::send_response(item, response).ok();
    }

    /// Run a blocking loop that processes interaction requests via MessageBox.
    #[cfg(target_os = "windows")]
    pub fn run_loop(&self) {
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
}

// ─── MessageBox implementation ──────────────────────────────────────

#[cfg(target_os = "windows")]
fn show_messagebox_for_request(request: &InteractionRequest) -> InteractionResponse {
    // MessageBox is meaningful for tool approval; other points auto-proceed.
    let approval = match request {
        InteractionRequest::ToolApproval { approval } => approval,
        _ => return InteractionResponse::Proceed,
    };

    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::core::HSTRING;

    let title = HSTRING::from(format!("OneAI: Approve '{}'", approval.tool_name));
    let message = HSTRING::from(DesktopInteractionBridge::format_request(approval));

    // MB_YESNOCANCEL: Yes=Proceed, No=Abort, Cancel=ProceedWith (unchanged args)
    let result = MessageBoxW(
        None,  // No parent window
        &message,
        &title,
        MB_YESNOCANCEL | MB_ICONQUESTION | MB_DEFBUTTON2,  // Default to "No" (deny)
    );

    match result {
        IDYES => InteractionResponse::Proceed,
        IDNO => InteractionResponse::Abort {
            reason: "User denied via MessageBox".to_string(),
        },
        IDCANCEL => InteractionResponse::Proceed,
        _ => InteractionResponse::Abort {
            reason: "Unknown dialog response".to_string(),
        },
    }
}
