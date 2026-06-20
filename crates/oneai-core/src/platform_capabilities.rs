//! Platform capabilities — extended platform adaptation beyond just approval gates.
//!
//! This addresses Issue #4: the PlatformAdapter currently only contains
//! an approval gate. Real cross-platform agents need access to platform
//! capabilities like:
//! - Screenshot capture (for visual context)
//! - Camera stream (for mobile agents)
//! - File system sandbox (for safe file operations)
//! - Notifications (for user alerts)
//! - Network status (for connectivity awareness)
//!
//! Each platform (Desktop, iOS, Android, HarmonyOS) implements these
//! capabilities through the PlatformCapabilities trait.

use async_trait::async_trait;

use crate::error::Result;

// ─── ScreenshotResult ───────────────────────────────────────────────────────

/// Result of a screenshot capture.
pub struct ScreenshotResult {
    /// The screenshot image data.
    pub data: Vec<u8>,
    /// The image MIME type (e.g., "image/png").
    pub mime_type: String,
    /// The image width in pixels.
    pub width: u32,
    /// The image height in pixels.
    pub height: u32,
}

// ─── CameraStreamHandle ─────────────────────────────────────────────────────

/// Handle to an active camera stream.
///
/// On mobile platforms (iOS, Android, HarmonyOS), the camera can be
/// accessed as a continuous stream of frames for real-time visual context.
pub struct CameraStreamHandle {
    /// Unique stream identifier.
    pub stream_id: String,
    /// The camera being used (front/back).
    pub camera_position: CameraPosition,
}

/// Camera position (front-facing vs back-facing).
pub enum CameraPosition {
    /// Front-facing camera (selfie).
    Front,
    /// Back-facing camera (main camera).
    Back,
}

// ─── NetworkStatus ──────────────────────────────────────────────────────────

/// Current network connectivity status.
pub struct NetworkStatus {
    /// Whether the device has internet connectivity.
    pub is_connected: bool,
    /// The connection type (WiFi, Cellular, Ethernet, None).
    pub connection_type: ConnectionType,
    /// Whether the connection is metered (cellular with data limits).
    pub is_metered: bool,
}

/// Network connection type.
pub enum ConnectionType {
    WiFi,
    Cellular,
    Ethernet,
    Offline,
}

// ─── FilesystemSandbox ──────────────────────────────────────────────────────

/// File system sandbox — restricts file operations to safe boundaries.
///
/// Each platform provides its own sandbox implementation:
/// - Desktop: restricts operations to the project directory
/// - iOS: uses app sandbox (already enforced by iOS)
/// - Android: uses app private storage
/// - HarmonyOS: uses app sandbox
pub trait FilesystemSandbox: Send + Sync {
    /// Get the allowed root directory for file operations.
    fn allowed_root(&self) -> &std::path::Path;

    /// Check if a path is within the allowed sandbox.
    fn is_path_allowed(&self, path: &std::path::Path) -> bool;

    /// Get the sandbox type.
    fn sandbox_type(&self) -> SandboxType;

    /// Resolve a relative path to an absolute path within the sandbox.
    fn resolve_path(&self, relative: &std::path::Path) -> std::path::PathBuf;
}

/// Type of filesystem sandbox.
pub enum SandboxType {
    /// Project directory sandbox (Desktop).
    ProjectDirectory,
    /// App sandbox (iOS/Android/HarmonyOS — OS-enforced).
    AppSandbox,
    /// Custom sandbox with user-defined boundaries.
    Custom,
}

// ─── PlatformCapabilities ───────────────────────────────────────────────────

/// Platform capabilities trait — defines what a platform can do.
///
/// Each platform crate implements this trait with capabilities
/// appropriate for the platform:
/// - Desktop: screenshot, filesystem sandbox, network status, notifications
/// - iOS: camera stream, screenshot (limited), app sandbox
/// - Android: camera stream, screenshot, network status, notifications
/// - HarmonyOS: camera stream, app sandbox, network status
///
/// The `supports_*()` methods allow the agent to check capabilities
/// before attempting to use them (important for cross-platform compatibility).
#[async_trait]
pub trait PlatformCapabilities: Send + Sync {
    /// Capture a screenshot of the current screen.
    ///
    /// Returns the screenshot image data. Not all platforms support this
    /// (e.g., iOS restricts screenshot capture in background).
    async fn screenshot(&self) -> Result<ScreenshotResult>;

    /// Start a camera stream for real-time visual context.
    ///
    /// Available on mobile platforms. Returns a handle that can be used
    /// to read individual frames from the camera.
    async fn camera_stream(&self, position: CameraPosition) -> Result<CameraStreamHandle>;

    /// Get the filesystem sandbox for this platform.
    fn filesystem_sandbox(&self) -> &dyn FilesystemSandbox;

    /// Send a notification to the user.
    ///
    /// Useful for long-running agent tasks that need to notify
    /// the user when they complete or encounter issues.
    async fn send_notification(&self, title: &str, body: &str) -> Result<()>;

    /// Get the current network connectivity status.
    ///
    /// Important for agents that need to decide whether to attempt
    /// network-dependent operations (API calls, web searches, etc.)
    async fn network_status(&self) -> Result<NetworkStatus>;

    /// Whether this platform supports screenshot capture.
    fn supports_screenshot(&self) -> bool;

    /// Whether this platform supports camera streaming.
    fn supports_camera(&self) -> bool;

    /// Whether this platform supports notifications.
    fn supports_notifications(&self) -> bool;

    /// Get the platform name.
    fn platform_name(&self) -> &'static str;
}

// ─── StubPlatformCapabilities ───────────────────────────────────────────────

/// Stub implementation for development/testing.
pub struct StubPlatformCapabilities {
    #[allow(dead_code)]
    platform_name: String,
}

impl StubPlatformCapabilities {
    pub fn new(platform_name: impl Into<String>) -> Self {
        Self { platform_name: platform_name.into() }
    }
}

#[async_trait]
impl PlatformCapabilities for StubPlatformCapabilities {
    async fn screenshot(&self) -> Result<ScreenshotResult> {
        Err(crate::error::OneAIError::Platform("Screenshot not available in stub".to_string()))
    }

    async fn camera_stream(&self, _position: CameraPosition) -> Result<CameraStreamHandle> {
        Err(crate::error::OneAIError::Platform("Camera not available in stub".to_string()))
    }

    fn filesystem_sandbox(&self) -> &dyn FilesystemSandbox {
        &StubFilesystemSandbox
    }

    async fn send_notification(&self, _title: &str, _body: &str) -> Result<()> {
        Ok(())
    }

    async fn network_status(&self) -> Result<NetworkStatus> {
        Ok(NetworkStatus {
            is_connected: true,
            connection_type: ConnectionType::WiFi,
            is_metered: false,
        })
    }

    fn supports_screenshot(&self) -> bool { false }
    fn supports_camera(&self) -> bool { false }
    fn supports_notifications(&self) -> bool { false }
    fn platform_name(&self) -> &'static str { "stub" }
}

/// Stub filesystem sandbox — allows all paths (for testing/development).
struct StubFilesystemSandbox;

impl FilesystemSandbox for StubFilesystemSandbox {
    fn allowed_root(&self) -> &std::path::Path {
        std::path::Path::new("/")
    }

    fn is_path_allowed(&self, _path: &std::path::Path) -> bool {
        true // Stub allows everything
    }

    fn sandbox_type(&self) -> SandboxType {
        SandboxType::Custom
    }

    fn resolve_path(&self, relative: &std::path::Path) -> std::path::PathBuf {
        relative.to_path_buf()
    }
}