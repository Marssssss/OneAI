//! Linux platform interaction gate — CLI-based fallback implementation.
//!
//! The LinuxCliInteractionGate uses stdin/stdout for interaction requests,
//! making it always available on Linux even without GTK or Qt.
//!
//! An optional GTK-based gate could be added behind a feature flag,
//! but the CLI version is the primary Linux implementation for now.

use std::sync::{Arc, Mutex};
use std::io::{self, Write};

use async_trait::async_trait;
use oneai_core::{InteractionPoint, InteractionRequest, InteractionResponse, RiskLevel};
use oneai_core::error::Result;
use oneai_core::platform::PlatformInteractionGate;
use oneai_core::traits::InteractionGate;
use oneai_tool::{ChannelInteractionGate, InteractionGateConfig, InteractionPendingItem, ThresholdInteractionGate};

use crate::bridge_common::DesktopInteractionBridge;

// ─── LinuxCliInteractionGate ──────────────────────────────────────

/// Linux CLI-based interaction gate that prompts via stdin/stdout.
///
/// This gate wraps a `ChannelInteractionGate` (or `ThresholdInteractionGate`)
/// internally. The bridge runs a CLI loop that reads stdin for responses.
pub struct LinuxCliInteractionGate {
    inner: Arc<dyn InteractionGate>,
}

impl LinuxCliInteractionGate {
    /// Create a new Linux CLI interaction gate with an auto-proceed threshold.
    pub fn new(buffer_size: usize, threshold: RiskLevel) -> (Self, LinuxCliInteractionBridge) {
        let (gate, receiver) =
            ThresholdInteractionGate::new(buffer_size, threshold, InteractionGateConfig::default());
        let inner: Arc<dyn InteractionGate> = Arc::new(gate);
        let bridge = LinuxCliInteractionBridge::new(receiver);
        (Self { inner }, bridge)
    }

    /// Create a gate where all enabled points go through the channel.
    pub fn new_manual_only(buffer_size: usize) -> (Self, LinuxCliInteractionBridge) {
        let (gate, receiver) = ChannelInteractionGate::new(buffer_size);
        let inner: Arc<dyn InteractionGate> = Arc::new(gate);
        let bridge = LinuxCliInteractionBridge::new(receiver);
        (Self { inner }, bridge)
    }
}

#[async_trait]
impl InteractionGate for LinuxCliInteractionGate {
    async fn request(&self, req: InteractionRequest) -> Result<InteractionResponse> {
        self.inner.request(req).await
    }

    fn enabled(&self, point: InteractionPoint) -> bool {
        self.inner.enabled(point)
    }
}

#[async_trait]
impl PlatformInteractionGate for LinuxCliInteractionGate {
    fn platform_name(&self) -> &'static str {
        "linux"
    }

    fn is_ui_available(&self) -> bool {
        // CLI is always available on Linux
        true
    }
}

// ─── LinuxCliInteractionBridge ──────────────────────────────────

/// Bridge that receives interaction requests and prompts via CLI stdin.
pub struct LinuxCliInteractionBridge {
    inner: Mutex<DesktopInteractionBridge>,
}

impl LinuxCliInteractionBridge {
    fn new(receiver: tokio::sync::mpsc::Receiver<InteractionPendingItem>) -> Self {
        Self {
            inner: Mutex::new(DesktopInteractionBridge::new(receiver)),
        }
    }

    /// Try to receive a pending interaction item (non-blocking).
    pub fn try_recv(&self) -> Option<InteractionPendingItem> {
        self.inner.lock().unwrap().try_recv()
    }

    /// Prompt for a tool-approval decision via stdin and return the response.
    pub fn prompt_cli_approval(request: &oneai_core::ApprovalRequest) -> InteractionResponse {
        println!();
        println!("╔══════════════════════════════════════╗");
        println!("║   ⚠️  Approval Request               ║");
        println!("╚══════════════════════════════════════╝");
        println!("{}", DesktopInteractionBridge::format_request(request));
        println!();
        println!("Choose action:");
        println!("  [A] Approve — allow execution unchanged");
        println!("  [D] Deny    — block execution");
        println!("  [M] Modify  — approve with modified args (enter JSON)");
        println!();

        loop {
            print!("Your choice (A/D/M): ");
            io::stdout().flush().ok();

            let mut input = String::new();
            if io::stdin().read_line(&mut input).is_err() {
                return InteractionResponse::Abort {
                    reason: "Failed to read input".to_string(),
                };
            }

            let choice = input.trim().to_uppercase();
            match choice.as_str() {
                "A" | "APPROVE" | "Y" | "YES" => {
                    return InteractionResponse::Proceed;
                }
                "D" | "DENY" | "N" | "NO" => {
                    return InteractionResponse::Abort {
                        reason: "User denied via CLI".to_string(),
                    };
                }
                "M" | "MODIFY" => {
                    print!("Enter modified args (JSON): ");
                    io::stdout().flush().ok();
                    let mut args_input = String::new();
                    if io::stdin().read_line(&mut args_input).is_err() {
                        return InteractionResponse::Abort {
                            reason: "Failed to read modified args".to_string(),
                        };
                    }
                    let args_json = args_input.trim();
                    match serde_json::from_str(args_json) {
                        Ok(args) => {
                            return InteractionResponse::ProceedWith {
                                modification: oneai_core::InteractionModification::ReplaceToolArgs(args),
                            }
                        }
                        Err(_) => {
                            println!("Invalid JSON. Please try again.");
                            continue;
                        }
                    }
                }
                _ => {
                    println!("Invalid choice. Please enter A, D, or M.");
                    continue;
                }
            }
        }
    }

    /// Handle an item: prompt for CLI approval (tool-approval points) + send response.
    pub fn handle_item(&self, item: InteractionPendingItem) {
        let response = match &item.request {
            InteractionRequest::ToolApproval { approval } => Self::prompt_cli_approval(approval),
            // Other decision points auto-proceed on the CLI fallback.
            _ => InteractionResponse::Proceed,
        };
        DesktopInteractionBridge::send_response(item, response).ok();
    }

    /// Run a blocking loop that processes interaction requests via CLI.
    ///
    /// This blocks the calling thread and reads stdin for each request.
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
