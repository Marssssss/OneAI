//! # OneAI Platform — Desktop
//!
//! Desktop platform adapter for the OneAI framework.
//! Provides native UI approval gates for macOS (NSAlert), Windows (MessageBox),
//! and Linux (CLI fallback / optional GTK).
//!
//! Each platform gate wraps a `ChannelApprovalGateWithThreshold` and bridges
//! the channel communication to the platform's native UI thread.
//!
//! Usage:
//! ```ignore
//! let (gate, bridge) = DesktopApprovalGateFactory::create(16, RiskLevel::Medium);
//! let app = AppBuilder::new()
//!     .platform_approval_gate(Arc::new(gate))
//!     .build()?;
//!
//! // In the UI thread:
//! bridge.run_loop();  // macOS/Windows — blocks and processes dialogs
//! // or for Linux CLI:
//! bridge.run_cli_loop();  // stdin-based approval
//! ```

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
mod linux;

// Common bridge infrastructure (always available)
mod bridge_common;

use oneai_core::RiskLevel;
use oneai_core::platform::Platform;

pub use bridge_common::DesktopApprovalBridge;

// ─── Platform-specific gate types ──────────────────────────────────────

#[cfg(target_os = "macos")]
pub use macos::{MacOSApprovalGate, MacOSApprovalBridge};

#[cfg(target_os = "windows")]
pub use windows::{WindowsApprovalGate, WindowsApprovalBridge};

#[cfg(target_os = "linux")]
pub use linux::{LinuxCliApprovalGate, LinuxCliApprovalBridge};

// ─── DesktopApprovalGateFactory ─────────────────────────────────────────

/// Factory for creating the correct desktop approval gate based on the current platform.
///
/// Returns a tuple of (gate, bridge) — the gate is passed to AppBuilder,
/// and the bridge is held by the UI thread to receive and respond to approval requests.
pub struct DesktopApprovalGateFactory;

impl DesktopApprovalGateFactory {
    /// Create a desktop approval gate with auto-approve threshold.
    ///
    /// On macOS: returns MacOSApprovalGate + MacOSApprovalBridge
    /// On Windows: returns WindowsApprovalGate + WindowsApprovalBridge
    /// On Linux: returns LinuxCliApprovalGate + LinuxCliApprovalBridge
    #[cfg(target_os = "macos")]
    pub fn create(buffer_size: usize, threshold: RiskLevel) -> (MacOSApprovalGate, MacOSApprovalBridge) {
        MacOSApprovalGate::new(buffer_size, threshold)
    }

    #[cfg(target_os = "windows")]
    pub fn create(buffer_size: usize, threshold: RiskLevel) -> (WindowsApprovalGate, WindowsApprovalBridge) {
        WindowsApprovalGate::new(buffer_size, threshold)
    }

    #[cfg(target_os = "linux")]
    pub fn create(buffer_size: usize, threshold: RiskLevel) -> (LinuxCliApprovalGate, LinuxCliApprovalBridge) {
        LinuxCliApprovalGate::new(buffer_size, threshold)
    }

    /// Create a desktop approval gate where all requests go through the channel (no auto-approve).
    #[cfg(target_os = "macos")]
    pub fn create_manual_only(buffer_size: usize) -> (MacOSApprovalGate, MacOSApprovalBridge) {
        MacOSApprovalGate::new_manual_only(buffer_size)
    }

    #[cfg(target_os = "windows")]
    pub fn create_manual_only(buffer_size: usize) -> (WindowsApprovalGate, WindowsApprovalBridge) {
        WindowsApprovalGate::new_manual_only(buffer_size)
    }

    #[cfg(target_os = "linux")]
    pub fn create_manual_only(buffer_size: usize) -> (LinuxCliApprovalGate, LinuxCliApprovalBridge) {
        LinuxCliApprovalGate::new_manual_only(buffer_size)
    }

    /// Get the current desktop platform.
    pub fn current_platform() -> Platform {
        Platform::current()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::traits::ApprovalGate;
    use oneai_core::{ApprovalRequest, ApprovalResponse};

    #[test]
    fn test_factory_current_platform() {
        let platform = DesktopApprovalGateFactory::current_platform();
        // On macOS, should detect macOS
        assert!(matches!(platform, Platform::Macos | Platform::Linux | Platform::Windows));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn test_macos_approval_gate_auto_approve() {
        // Test that low-risk requests are auto-approved
        let (gate, _bridge) = DesktopApprovalGateFactory::create(16, RiskLevel::Medium);

        let request = ApprovalRequest {
            tool_name: "calculator".to_string(),
            args: serde_json::json!({"expression": "2+2"}),
            risk_level: RiskLevel::Low,
            permission_level: None,
            justification: "Simple calculation".to_string(),
        };

        let response = gate.request_approval(request).await.unwrap();
        assert!(matches!(response, ApprovalResponse::Approved { .. }));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_linux_cli_approval_gate_auto_approve() {
        let (gate, _bridge) = DesktopApprovalGateFactory::create(16, RiskLevel::Medium);

        let request = ApprovalRequest {
            tool_name: "calculator".to_string(),
            args: serde_json::json!({"expression": "2+2"}),
            risk_level: RiskLevel::Low,
            permission_level: None,
            justification: "Simple calculation".to_string(),
        };

        let response = gate.request_approval(request).await.unwrap();
        assert!(matches!(response, ApprovalResponse::Approved { .. }));
    }
}