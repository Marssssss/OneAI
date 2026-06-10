//! Linux platform approval gate — CLI-based fallback implementation.
//!
//! The LinuxCliApprovalGate uses stdin/stdout for approval requests,
//! making it always available on Linux even without GTK or Qt.
//!
//! An optional GTK-based gate could be added behind a feature flag,
//! but the CLI version is the primary Linux implementation for now.

use std::sync::Mutex;
use std::io::{self, Write};

use async_trait::async_trait;
use oneai_core::{ApprovalRequest, ApprovalResponse, RiskLevel};
use oneai_core::error::Result;
use oneai_core::platform::PlatformApprovalGate;
use oneai_core::traits::ApprovalGate;
use oneai_tool::{ChannelApprovalGateWithThreshold, ApprovalPendingItem};

use crate::bridge_common::DesktopApprovalBridge;

use std::sync::Arc;

// ─── LinuxCliApprovalGate ──────────────────────────────────────

/// Linux CLI-based approval gate that prompts via stdin/stdout.
///
/// This gate wraps a ChannelApprovalGateWithThreshold internally.
/// The bridge runs a CLI loop that reads stdin for approval responses.
pub struct LinuxCliApprovalGate {
    inner: Arc<ChannelApprovalGateWithThreshold>,
}

impl LinuxCliApprovalGate {
    /// Create a new Linux CLI approval gate with auto-approve threshold.
    pub fn new(buffer_size: usize, threshold: RiskLevel) -> (Self, LinuxCliApprovalBridge) {
        let (gate, receiver) = ChannelApprovalGateWithThreshold::new(buffer_size, threshold);
        let inner = Arc::new(gate);
        let bridge = LinuxCliApprovalBridge::new(receiver);
        (Self { inner }, bridge)
    }

    /// Create a gate where all requests go through the channel.
    pub fn new_manual_only(buffer_size: usize) -> (Self, LinuxCliApprovalBridge) {
        let (gate, receiver) = ChannelApprovalGateWithThreshold::new_manual_only(buffer_size);
        let inner = Arc::new(gate);
        let bridge = LinuxCliApprovalBridge::new(receiver);
        (Self { inner }, bridge)
    }
}

#[async_trait]
impl ApprovalGate for LinuxCliApprovalGate {
    async fn request_approval(&self, request: ApprovalRequest) -> Result<ApprovalResponse> {
        self.inner.request_approval(request).await
    }
}

#[async_trait]
impl PlatformApprovalGate for LinuxCliApprovalGate {
    fn platform_name(&self) -> &'static str {
        "linux"
    }

    fn is_ui_available(&self) -> bool {
        // CLI is always available on Linux
        true
    }
}

// ─── LinuxCliApprovalBridge ──────────────────────────────────

/// Bridge that receives approval requests and prompts via CLI stdin.
pub struct LinuxCliApprovalBridge {
    inner: Mutex<DesktopApprovalBridge>,
}

impl LinuxCliApprovalBridge {
    fn new(receiver: tokio::sync::mpsc::Receiver<ApprovalPendingItem>) -> Self {
        Self {
            inner: Mutex::new(DesktopApprovalBridge::new(receiver)),
        }
    }

    /// Try to receive a pending approval item (non-blocking).
    pub fn try_recv(&self) -> Option<ApprovalPendingItem> {
        self.inner.lock().unwrap().try_recv()
    }

    /// Prompt for approval via stdin and return the response.
    pub fn prompt_cli_approval(request: &ApprovalRequest) -> ApprovalResponse {
        println!();
        println!("╔══════════════════════════════════════╗");
        println!("║   ⚠️  Approval Request               ║");
        println!("╚══════════════════════════════════════╝");
        println!("{}", DesktopApprovalBridge::format_request(request));
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
                return ApprovalResponse::Denied {
                    reason: "Failed to read input".to_string(),
                };
            }

            let choice = input.trim().to_uppercase();
            match choice.as_str() {
                "A" | "APPROVE" | "Y" | "YES" => {
                    return ApprovalResponse::Approved { modified_args: None };
                }
                "D" | "DENY" | "N" | "NO" => {
                    return ApprovalResponse::Denied {
                        reason: "User denied via CLI".to_string(),
                    };
                }
                "M" | "MODIFY" => {
                    print!("Enter modified args (JSON): ");
                    io::stdout().flush().ok();
                    let mut args_input = String::new();
                    if io::stdin().read_line(&mut args_input).is_err() {
                        return ApprovalResponse::Denied {
                            reason: "Failed to read modified args".to_string(),
                        };
                    }
                    let args_json = args_input.trim();
                    match serde_json::from_str(args_json) {
                        Ok(args) => return ApprovalResponse::Modified { args },
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

    /// Handle an item: prompt for CLI approval + send response.
    pub fn handle_item(&self, item: ApprovalPendingItem) {
        let response = Self::prompt_cli_approval(&item.request);
        DesktopApprovalBridge::send_response(item, response).ok();
    }

    /// Run a blocking loop that processes approval requests via CLI.
    ///
    /// This blocks the calling thread and reads stdin for each request.
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