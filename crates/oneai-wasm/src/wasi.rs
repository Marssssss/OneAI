//! WASI restricted filesystem access for WASM sandbox execution.
//!
//! WASI (WebAssembly System Interface) provides controlled filesystem access
//! to WASM guest modules. The OneAI WASM sandbox uses a restricted WASI
//! configuration that only allows access to explicitly whitelisted directories.
//!
//! ## Security Model
//!
//! - Only explicitly whitelisted directories are accessible
//! - Each directory can be read-only or read-write
//! - Environment variables are whitelist-only (never inherited by default)
//! - Stdio is not inherited by default (guest logs via host_log)
//! - No network access (WASI does not provide sockets by default in our config)
//! - No process spawning (WASI does not provide proc_spawn)

use std::collections::HashMap;
use std::path::PathBuf;

use wasmtime_wasi::DirPerms;
use wasmtime_wasi::FilePerms;

/// WASI filesystem access mode.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WasiAccessMode {
    /// Read-only access (guest can read but not write/delete).
    ReadOnly,
    /// Read-write access (guest can read, write, create, delete).
    ReadWrite,
}

/// Configuration for a single WASI-allowed directory.
///
/// Maps a host filesystem path to a guest-visible path with
/// a specified access mode.
#[derive(Debug, Clone)]
pub struct WasiDirConfig {
    /// Host path (absolute path on the host filesystem).
    host_path: PathBuf,
    /// Guest path (path as seen by the WASM module, e.g. "/data").
    guest_path: String,
    /// Access mode (read-only or read-write).
    access_mode: WasiAccessMode,
}

impl WasiDirConfig {
    /// Create a read-only directory configuration.
    ///
    /// The guest can read files from `host_path` but cannot modify them.
    /// The directory is visible to the guest as `guest_path`.
    pub fn readonly(host_path: PathBuf, guest_path: &str) -> Self {
        Self {
            host_path,
            guest_path: guest_path.to_string(),
            access_mode: WasiAccessMode::ReadOnly,
        }
    }

    /// Create a read-write directory configuration.
    ///
    /// The guest can read, write, create, and delete files in `host_path`.
    /// The directory is visible to the guest as `guest_path`.
    pub fn readwrite(host_path: PathBuf, guest_path: &str) -> Self {
        Self {
            host_path,
            guest_path: guest_path.to_string(),
            access_mode: WasiAccessMode::ReadWrite,
        }
    }

    /// Get the host filesystem path.
    pub fn host_path(&self) -> &PathBuf {
        &self.host_path
    }

    /// Get the guest-visible path.
    pub fn guest_path(&self) -> &str {
        &self.guest_path
    }

    /// Get the access mode.
    pub fn access_mode(&self) -> &WasiAccessMode {
        &self.access_mode
    }
}

/// WASI configuration for the runtime.
///
/// Controls filesystem access, environment variables, and stdio inheritance
/// for WASM guest modules. Default configuration is maximally restricted:
/// no filesystem access, no env vars, no stdio inheritance.
///
/// ## Security Guarantees
///
/// - Only explicitly whitelisted directories are preopened for the guest
/// - No environment variables are inherited from the host (whitelist-only)
/// - Stdio is not connected by default (guest logs via host_log host function)
/// - WASI preview1 is used (no sockets, no process spawning)
#[derive(Debug, Clone)]
pub struct WasiConfig {
    /// Whether WASI is enabled.
    enabled: bool,
    /// Allowed directories with access mode.
    allowed_dirs: Vec<WasiDirConfig>,
    /// Whether to inherit host environment variables (default: false, security-first).
    inherit_env: bool,
    /// Whether to inherit stdin/stdout/stderr (default: false).
    inherit_stdio: bool,
    /// Custom environment variables to provide to guest (whitelist approach).
    env_vars: HashMap<String, String>,
}

impl WasiConfig {
    /// Create a disabled WASI configuration (no filesystem access).
    ///
    /// This is the default — WASM modules execute in a pure computation
    /// sandbox with zero I/O access.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            allowed_dirs: Vec::new(),
            inherit_env: false,
            inherit_stdio: false,
            env_vars: HashMap::new(),
        }
    }

    /// Create a restricted WASI configuration with whitelisted directories.
    ///
    /// WASI is enabled, but:
    /// - Only the specified directories are accessible
    /// - No environment variables are inherited from host
    /// - Stdio is not connected (guest logs via host_log)
    pub fn restricted(dirs: Vec<WasiDirConfig>) -> Self {
        Self {
            enabled: true,
            allowed_dirs: dirs,
            inherit_env: false,
            inherit_stdio: false,
            env_vars: HashMap::new(),
        }
    }

    /// Create a restricted WASI configuration with custom environment variables.
    ///
    /// WASI is enabled with whitelisted directories plus specific
    /// environment variables provided to the guest. Host environment
    /// variables are NOT inherited.
    pub fn restricted_with_env(dirs: Vec<WasiDirConfig>, env_vars: HashMap<String, String>) -> Self {
        Self {
            enabled: true,
            allowed_dirs: dirs,
            inherit_env: false,
            inherit_stdio: false,
            env_vars,
        }
    }

    /// Whether WASI is enabled.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Get the allowed directories.
    pub fn allowed_dirs(&self) -> &[WasiDirConfig] {
        &self.allowed_dirs
    }

    /// Whether host environment variables are inherited.
    pub fn inherit_env(&self) -> bool {
        self.inherit_env
    }

    /// Whether stdio is inherited.
    pub fn inherit_stdio(&self) -> bool {
        self.inherit_stdio
    }

    /// Get custom environment variables for the guest.
    pub fn env_vars(&self) -> &HashMap<String, String> {
        &self.env_vars
    }
}

impl Default for WasiConfig {
    fn default() -> Self {
        Self::disabled()
    }
}

/// Build a WASI P1 context from configuration.
///
/// Creates a `wasmtime_wasi::WasiP1Ctx` with:
/// - Preopened directories from `allowed_dirs` (read-only or read-write)
/// - Custom environment variables (if specified)
/// - No inherited host env vars (unless explicitly configured)
/// - No inherited stdio (unless explicitly configured)
///
/// Returns `None` if WASI is disabled.
pub fn build_wasi_p1_ctx(config: &WasiConfig) -> Option<wasmtime_wasi::p1::WasiP1Ctx> {
    if !config.enabled {
        return None;
    }

    let mut builder = wasmtime_wasi::WasiCtx::builder();

    // Add preopened directories
    for dir_config in config.allowed_dirs() {
        let host_path = dir_config.host_path();
        let guest_path = dir_config.guest_path();

        // Verify host path exists
        if !host_path.exists() {
            // Skip non-existent paths — WASI will not provide them to guest
            continue;
        }

        let (dir_perms, file_perms) = match dir_config.access_mode() {
            WasiAccessMode::ReadOnly => (DirPerms::READ, FilePerms::READ),
            WasiAccessMode::ReadWrite => (DirPerms::all(), FilePerms::all()),
        };

        // Preopen the directory
        if builder.preopened_dir(host_path, guest_path, dir_perms, file_perms).is_err() {
            // Skip if cannot open — WASI will not provide it to guest
            continue;
        }
    }

    // Add custom environment variables (whitelist-only)
    for (key, value) in config.env_vars() {
        builder.env(key, value);
    }

    // Inherit host environment if explicitly configured
    if config.inherit_env() {
        builder.inherit_env();
    }

    // Inherit stdio if explicitly configured
    if config.inherit_stdio() {
        builder.inherit_stdin();
        builder.inherit_stdout();
        builder.inherit_stderr();
    }

    // Build P1 context (includes WasiCtx internally)
    Some(builder.build_p1())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wasi_config_disabled() {
        let config = WasiConfig::disabled();
        assert!(!config.enabled());
        assert!(config.allowed_dirs().is_empty());
        assert!(!config.inherit_env());
        assert!(!config.inherit_stdio());
        assert!(config.env_vars().is_empty());
    }

    #[test]
    fn test_wasi_config_default_is_disabled() {
        let config = WasiConfig::default();
        assert!(!config.enabled());
    }

    #[test]
    fn test_wasi_config_restricted() {
        let dirs = vec![
            WasiDirConfig::readonly(PathBuf::from("/tmp/data"), "/data"),
        ];
        let config = WasiConfig::restricted(dirs);
        assert!(config.enabled());
        assert_eq!(config.allowed_dirs().len(), 1);
        assert!(!config.inherit_env());
        assert!(!config.inherit_stdio());
    }

    #[test]
    fn test_wasi_config_restricted_with_env() {
        let dirs = vec![
            WasiDirConfig::readwrite(PathBuf::from("/tmp/output"), "/output"),
        ];
        let env_vars = HashMap::from([
            ("ONEAI_MODE".to_string(), "production".to_string()),
        ]);
        let config = WasiConfig::restricted_with_env(dirs, env_vars);
        assert!(config.enabled());
        assert_eq!(config.allowed_dirs().len(), 1);
        assert_eq!(config.env_vars().len(), 1);
        assert!(!config.inherit_env());
    }

    #[test]
    fn test_wasi_dir_config_readonly() {
        let config = WasiDirConfig::readonly(PathBuf::from("/tmp/data"), "/data");
        assert_eq!(config.host_path(), &PathBuf::from("/tmp/data"));
        assert_eq!(config.guest_path(), "/data");
        assert_eq!(config.access_mode(), &WasiAccessMode::ReadOnly);
    }

    #[test]
    fn test_wasi_dir_config_readwrite() {
        let config = WasiDirConfig::readwrite(PathBuf::from("/tmp/output"), "/output");
        assert_eq!(config.host_path(), &PathBuf::from("/tmp/output"));
        assert_eq!(config.guest_path(), "/output");
        assert_eq!(config.access_mode(), &WasiAccessMode::ReadWrite);
    }

    #[test]
    fn test_build_wasi_p1_ctx_disabled() {
        let config = WasiConfig::disabled();
        let ctx = build_wasi_p1_ctx(&config);
        assert!(ctx.is_none());
    }

    #[test]
    fn test_build_wasi_p1_ctx_enabled() {
        let config = WasiConfig::restricted(vec![]);
        let ctx = build_wasi_p1_ctx(&config);
        assert!(ctx.is_some());
    }

    #[test]
    fn test_wasi_access_mode_variants() {
        assert_ne!(WasiAccessMode::ReadOnly, WasiAccessMode::ReadWrite);
    }
}
