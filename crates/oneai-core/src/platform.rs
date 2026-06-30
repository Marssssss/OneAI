//! Platform adaptation traits — define how platform-specific features plug in.
//!
//! The OneAI framework is designed to be cross-platform. Each platform
//! (Android, iOS, Desktop, HarmonyOS) provides its own implementations
//! of these traits through the platform adaptation layer.
//!
//! In the core Rust layer, we provide:
//! - Trait definitions that platform code must implement
//! - Default/stub implementations for testing on any host
//! - Factory methods that select the correct platform implementation

use std::sync::Arc;

use async_trait::async_trait;
use crate::{InteractionRequest, InteractionResponse};
use crate::error::Result;
use crate::traits::InteractionGate;

// ─── PlatformInteractionGate ───────────────────────────────────────────

/// Platform-specific interaction gate that uses native UI dialogs.
///
/// The `InteractionGate` equivalent of the deprecated `PlatformApprovalGate`:
/// each platform implements this to surface every decision point
/// (PreInfer/PostInfer/ToolApproval/PlanDecision/PlanReview) through native UI.
/// A platform gate is wired into the app via `AppBuilder::interaction_gate(...)`.
#[async_trait]
pub trait PlatformInteractionGate: InteractionGate {
    /// Get the platform name this gate is designed for.
    fn platform_name(&self) -> &'static str;

    /// Whether the platform UI is available.
    fn is_ui_available(&self) -> bool;
}

/// Stub implementation for development/testing — always proceeds and disables
/// every point (zero-latency, like `NoopInteractionGate`).
pub struct StubPlatformInteractionGate {
    #[allow(dead_code)]
    platform_name: String,
}

impl StubPlatformInteractionGate {
    /// Create a new stub interaction gate for the given platform name.
    pub fn new(platform_name: impl Into<String>) -> Self {
        Self {
            platform_name: platform_name.into(),
        }
    }

    /// Create a macOS stub.
    pub fn macos() -> Self {
        Self::new("macos")
    }

    /// Create a Windows stub.
    pub fn windows() -> Self {
        Self::new("windows")
    }

    /// Create an Android stub.
    pub fn android() -> Self {
        Self::new("android")
    }

    /// Create an iOS stub.
    pub fn ios() -> Self {
        Self::new("ios")
    }

    /// Create a HarmonyOS stub.
    pub fn harmony() -> Self {
        Self::new("harmony")
    }
}

#[async_trait]
impl InteractionGate for StubPlatformInteractionGate {
    async fn request(&self, _req: InteractionRequest) -> Result<InteractionResponse> {
        Ok(InteractionResponse::Proceed)
    }

    fn enabled(&self, _point: crate::InteractionPoint) -> bool {
        false
    }
}

#[async_trait]
impl PlatformInteractionGate for StubPlatformInteractionGate {
    fn platform_name(&self) -> &'static str {
        "stub"
    }

    fn is_ui_available(&self) -> bool {
        false
    }
}

// ─── Platform ──────────────────────────────────────────────────────────

/// The platform enum — identifies which platform the app is running on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Platform {
    Macos,
    Windows,
    Linux,
    Android,
    Ios,
    Harmony,
    Unknown,
}

impl Platform {
    /// Detect the current platform based on compile-time configuration.
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Macos
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "android") {
            Self::Android
        } else if cfg!(target_os = "ios") {
            Self::Ios
        } else {
            Self::Unknown
        }
    }

    /// Get a human-readable name for the platform.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Macos => "macOS",
            Self::Windows => "Windows",
            Self::Linux => "Linux",
            Self::Android => "Android",
            Self::Ios => "iOS",
            Self::Harmony => "HarmonyOS",
            Self::Unknown => "Unknown",
        }
    }

    /// Whether this platform is a mobile device.
    pub fn is_mobile(&self) -> bool {
        matches!(self, Self::Android | Self::Ios | Self::Harmony)
    }

    /// Whether this platform is a desktop.
    pub fn is_desktop(&self) -> bool {
        matches!(self, Self::Macos | Self::Windows | Self::Linux)
    }
}

// ─── PlatformAdapter ──────────────────────────────────────────────────

/// Bundle of platform-specific adapters.
///
/// Each platform provides its own implementation of this struct
/// through the platform adaptation layer. The default implementation
/// uses stubs suitable for testing on any host.
pub struct PlatformAdapter {
    /// The detected platform.
    pub platform: Platform,
    /// The interaction gate (native dialog).
    pub interaction_gate: Arc<dyn PlatformInteractionGate>,
}

impl PlatformAdapter {
    /// Create a default adapter with stub implementations.
    pub fn default_stub() -> Self {
        Self {
            platform: Platform::current(),
            interaction_gate: Arc::new(StubPlatformInteractionGate::new(
                Platform::current().name().to_lowercase()
            )),
        }
    }

    /// Create an adapter for macOS with stub interaction.
    pub fn macos_stub() -> Self {
        Self {
            platform: Platform::Macos,
            interaction_gate: Arc::new(StubPlatformInteractionGate::macos()),
        }
    }

    /// Create an adapter for Android with stub interaction.
    pub fn android_stub() -> Self {
        Self {
            platform: Platform::Android,
            interaction_gate: Arc::new(StubPlatformInteractionGate::android()),
        }
    }

    /// Create an adapter for iOS with stub interaction.
    pub fn ios_stub() -> Self {
        Self {
            platform: Platform::Ios,
            interaction_gate: Arc::new(StubPlatformInteractionGate::ios()),
        }
    }

    /// Create an adapter for HarmonyOS with stub interaction.
    pub fn harmony_stub() -> Self {
        Self {
            platform: Platform::Harmony,
            interaction_gate: Arc::new(StubPlatformInteractionGate::harmony()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_detection() {
        let platform = Platform::current();
        // On macOS, should detect macOS
        assert!(matches!(platform, Platform::Macos | Platform::Linux | Platform::Windows));
    }

    #[test]
    fn test_platform_names() {
        assert_eq!(Platform::Macos.name(), "macOS");
        assert_eq!(Platform::Android.name(), "Android");
        assert_eq!(Platform::Ios.name(), "iOS");
        assert_eq!(Platform::Harmony.name(), "HarmonyOS");
    }

    #[test]
    fn test_platform_categories() {
        assert!(Platform::Android.is_mobile());
        assert!(Platform::Ios.is_mobile());
        assert!(Platform::Harmony.is_mobile());
        assert!(Platform::Macos.is_desktop());
        assert!(Platform::Windows.is_desktop());
        assert!(Platform::Linux.is_desktop());
    }

    #[tokio::test]
    async fn test_stub_interaction_gate() {
        let gate = StubPlatformInteractionGate::macos();
        let resp = gate
            .request(InteractionRequest::PlanReview {
                plan: "p".to_string(),
                steps: vec![],
            })
            .await
            .unwrap();
        assert!(matches!(resp, InteractionResponse::Proceed));
        assert!(!gate.enabled(crate::InteractionPoint::PlanReview));
    }

    #[test]
    fn test_platform_adapter_stub() {
        let adapter = PlatformAdapter::default_stub();
        assert!(!adapter.interaction_gate.is_ui_available());
    }

    #[test]
    fn test_platform_adapter_android() {
        let adapter = PlatformAdapter::android_stub();
        assert_eq!(adapter.platform, Platform::Android);
        assert!(!adapter.interaction_gate.is_ui_available());
    }

    #[test]
    fn test_platform_adapter_harmony() {
        let adapter = PlatformAdapter::harmony_stub();
        assert_eq!(adapter.platform, Platform::Harmony);
    }
}