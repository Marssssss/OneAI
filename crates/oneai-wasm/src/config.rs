//! WasmRuntimeConfig — security policy and resource limits for WASM execution.

use std::time::Duration;

use crate::wasi::{WasiConfig, WasiDirConfig};

/// Configuration for the WasmRuntime.
///
/// Controls security policies, resource limits, and WASI access.
/// Default configuration provides a strict pure-computation sandbox:
/// - No filesystem access (WASI disabled)
/// - No network access
/// - Limited memory (16 pages = 1MB)
/// - Fuel-based execution limit (prevents infinite loops)
/// - Epoch-based interrupt (async timeout support)
/// - Maximum execution time of 30 seconds
///
/// These defaults follow the principle of "configuration most flexible,
/// execution strongest" — the WASM sandbox is maximally restricted by default,
/// and users must explicitly opt into less restrictive settings.
#[derive(Debug, Clone)]
pub struct WasmRuntimeConfig {
    /// Maximum memory pages for WASM instances.
    ///
    /// Each WASM memory page is 64KB. Default: 16 pages = 1MB.
    /// This limits the amount of memory a WASM module can allocate,
    /// preventing runaway memory consumption.
    pub max_memory_pages: u32,

    /// Fuel limit for WASM execution.
    ///
    /// Each WASM instruction consumes fuel. When the limit is reached,
    /// execution is trapped with a FuelExceeded error.
    /// This prevents infinite loops in agent-generated code.
    ///
    /// Default: 100,000 fuel units (roughly equivalent to ~10 seconds
    /// of simple computation).
    pub fuel_limit: Option<u64>,

    /// Whether to enable epoch-based interruption.
    ///
    /// Epoch interruption allows the host to asynchronously interrupt
    /// WASM execution by advancing the epoch. This enables tokio-based
    /// timeout enforcement for WASM execution.
    ///
    /// Default: enabled with 10ms check interval.
    pub epoch_interruption: bool,

    /// Maximum execution time for WASM calls.
    ///
    /// When epoch interruption is enabled, this timeout is enforced
    /// via tokio::timeout + epoch advancement.
    ///
    /// Default: 30 seconds.
    pub max_execution_time: Duration,

    /// WASI configuration for filesystem access.
    ///
    /// Default: disabled (pure computation sandbox with zero I/O access).
    /// Use `with_wasi_config()` to enable restricted filesystem access
    /// with explicitly whitelisted directories.
    pub wasi_config: WasiConfig,

    /// Maximum concurrent WASM instances.
    ///
    /// Limits how many WASM instances can be running simultaneously.
    /// This prevents resource exhaustion from parallel WASM execution.
    ///
    /// Default: 10 instances.
    pub max_instances: usize,
}

impl Default for WasmRuntimeConfig {
    fn default() -> Self {
        Self {
            max_memory_pages: 16,                    // 16 * 64KB = 1MB
            fuel_limit: Some(100_000),               // ~10s of computation
            epoch_interruption: true,                 // enable async timeout
            max_execution_time: Duration::from_secs(30), // 30s max
            wasi_config: WasiConfig::disabled(),     // pure computation
            max_instances: 10,                        // 10 concurrent
        }
    }
}

impl WasmRuntimeConfig {
    /// Create a new config with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a strict sandbox config (pure computation, minimal resources).
    ///
    /// This is the safest configuration:
    /// - 8 memory pages (512KB)
    /// - 50,000 fuel units (~5s)
    /// - No WASI
    /// - 5 max instances
    pub fn strict() -> Self {
        Self {
            max_memory_pages: 8,
            fuel_limit: Some(50_000),
            epoch_interruption: true,
            max_execution_time: Duration::from_secs(10),
            wasi_config: WasiConfig::disabled(),
            max_instances: 5,
        }
    }

    /// Create a permissive config for trusted modules with WASI access.
    ///
    /// **WARNING**: This enables WASI filesystem access.
    /// Only use for trusted WASM modules.
    pub fn permissive_with_wasi(allowed_dirs: Vec<WasiDirConfig>) -> Self {
        Self {
            max_memory_pages: 256,                    // 256 * 64KB = 16MB
            fuel_limit: Some(1_000_000),              // ~100s of computation
            epoch_interruption: true,
            max_execution_time: Duration::from_secs(120),
            wasi_config: WasiConfig::restricted(allowed_dirs),
            max_instances: 20,
        }
    }

    /// Set the fuel limit.
    pub fn with_fuel_limit(mut self, limit: u64) -> Self {
        self.fuel_limit = Some(limit);
        self
    }

    /// Disable fuel limit (unsafe — allows infinite execution).
    pub fn without_fuel_limit(mut self) -> Self {
        self.fuel_limit = None;
        self
    }

    /// Set the maximum execution time.
    pub fn with_max_execution_time(mut self, timeout: Duration) -> Self {
        self.max_execution_time = timeout;
        self
    }

    /// Set the maximum memory pages.
    pub fn with_max_memory_pages(mut self, pages: u32) -> Self {
        self.max_memory_pages = pages;
        self
    }

    /// Set the WASI configuration.
    ///
    /// Use `WasiConfig::restricted()` or `WasiConfig::restricted_with_env()`
    /// to enable WASI with whitelisted directory access.
    pub fn with_wasi_config(mut self, wasi_config: WasiConfig) -> Self {
        self.wasi_config = wasi_config;
        self
    }

    /// Enable WASI with allowed directories (convenience method).
    ///
    /// Creates a `WasiConfig::restricted()` with the specified directories.
    pub fn with_wasi(mut self, allowed_dirs: Vec<WasiDirConfig>) -> Self {
        self.wasi_config = WasiConfig::restricted(allowed_dirs);
        self
    }
}
