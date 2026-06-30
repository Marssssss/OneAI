//! # OneAI Platform — Desktop
//!
//! Desktop platform adapter for the OneAI framework.
//! Provides native UI interaction gates for macOS (NSAlert), Windows (MessageBox),
//! and Linux (CLI fallback / optional GTK).
//!
//! Each platform gate wraps a `ChannelInteractionGate` (or `ThresholdInteractionGate`)
//! and bridges the channel communication to the platform's native UI thread.
//!
//! Usage:
//! ```ignore
//! let (gate, bridge) = DesktopInteractionGateFactory::create(16, RiskLevel::Medium);
//! let app = AppBuilder::new()
//!     .interaction_gate(Arc::new(gate))
//!     .build()?;
//!
//! // In the UI thread:
//! bridge.run_loop();  // macOS/Windows — blocks and processes dialogs
//! // or for Linux CLI:
//! bridge.run_loop();  // stdin-based interaction
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

pub use bridge_common::{DesktopInteractionBridge, DesktopInteractionDecision};

// ─── Platform-specific gate types ──────────────────────────────────────

#[cfg(target_os = "macos")]
pub use macos::{MacOSInteractionGate, MacOSInteractionBridge};

#[cfg(target_os = "windows")]
pub use windows::{WindowsInteractionGate, WindowsInteractionBridge};

#[cfg(target_os = "linux")]
pub use linux::{LinuxCliInteractionGate, LinuxCliInteractionBridge};

// ─── DesktopInteractionGateFactory ──────────────────────────────────────

/// Factory for creating the correct desktop interaction gate based on the current platform.
///
/// Returns a tuple of (gate, bridge) — the gate is passed to AppBuilder,
/// and the bridge is held by the UI thread to receive and respond to interaction requests.
pub struct DesktopInteractionGateFactory;

impl DesktopInteractionGateFactory {
    /// Create a desktop interaction gate with an auto-proceed threshold.
    ///
    /// On macOS: returns MacOSInteractionGate + MacOSInteractionBridge
    /// On Windows: returns WindowsInteractionGate + WindowsInteractionBridge
    /// On Linux: returns LinuxCliInteractionGate + LinuxCliInteractionBridge
    #[cfg(target_os = "macos")]
    pub fn create(buffer_size: usize, threshold: RiskLevel) -> (MacOSInteractionGate, MacOSInteractionBridge) {
        MacOSInteractionGate::new(buffer_size, threshold)
    }

    #[cfg(target_os = "windows")]
    pub fn create(buffer_size: usize, threshold: RiskLevel) -> (WindowsInteractionGate, WindowsInteractionBridge) {
        WindowsInteractionGate::new(buffer_size, threshold)
    }

    #[cfg(target_os = "linux")]
    pub fn create(buffer_size: usize, threshold: RiskLevel) -> (LinuxCliInteractionGate, LinuxCliInteractionBridge) {
        LinuxCliInteractionGate::new(buffer_size, threshold)
    }

    /// Create a desktop interaction gate where all enabled points go through the channel.
    #[cfg(target_os = "macos")]
    pub fn create_manual_only(buffer_size: usize) -> (MacOSInteractionGate, MacOSInteractionBridge) {
        MacOSInteractionGate::new_manual_only(buffer_size)
    }

    #[cfg(target_os = "windows")]
    pub fn create_manual_only(buffer_size: usize) -> (WindowsInteractionGate, WindowsInteractionBridge) {
        WindowsInteractionGate::new_manual_only(buffer_size)
    }

    #[cfg(target_os = "linux")]
    pub fn create_manual_only(buffer_size: usize) -> (LinuxCliInteractionGate, LinuxCliInteractionBridge) {
        LinuxCliInteractionGate::new_manual_only(buffer_size)
    }

    /// Get the current desktop platform.
    pub fn current_platform() -> Platform {
        Platform::current()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::traits::InteractionGate;
    use oneai_core::{ApprovalRequest, InteractionRequest, InteractionResponse};

    #[test]
    fn test_factory_current_platform() {
        let platform = DesktopInteractionGateFactory::current_platform();
        // On macOS, should detect macOS
        assert!(matches!(platform, Platform::Macos | Platform::Linux | Platform::Windows));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn test_macos_interaction_gate_auto_proceed_low_risk() {
        // Low-risk requests auto-proceed under the threshold gate.
        let (gate, _bridge) = DesktopInteractionGateFactory::create(16, RiskLevel::Medium);

        let request = ApprovalRequest {
            tool_name: "calculator".to_string(),
            args: serde_json::json!({"expression": "2+2"}),
            risk_level: RiskLevel::Low,
            permission_level: None,
            justification: "Simple calculation".to_string(),
        };

        let response = gate
            .request(InteractionRequest::ToolApproval { approval: request })
            .await
            .unwrap();
        assert!(matches!(response, InteractionResponse::Proceed));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_linux_cli_interaction_gate_auto_proceed_low_risk() {
        let (gate, _bridge) = DesktopInteractionGateFactory::create(16, RiskLevel::Medium);

        let request = ApprovalRequest {
            tool_name: "calculator".to_string(),
            args: serde_json::json!({"expression": "2+2"}),
            risk_level: RiskLevel::Low,
            permission_level: None,
            justification: "Simple calculation".to_string(),
        };

        let response = gate
            .request(InteractionRequest::ToolApproval { approval: request })
            .await
            .unwrap();
        assert!(matches!(response, InteractionResponse::Proceed));
    }
}
