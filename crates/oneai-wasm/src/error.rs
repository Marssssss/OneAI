//! WASM error types.

use thiserror::Error;

/// WASM execution errors.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum WasmError {
    /// Module compilation failed.
    #[error("WASM module compilation failed: {0}")]
    CompilationFailed(String),

    /// Module instantiation failed.
    #[error("WASM module instantiation failed: {0}")]
    InstantiationFailed(String),

    /// Required export function not found in module.
    #[error("Required export '{0}' not found in WASM module")]
    ExportNotFound(String),

    /// Function call failed (trap or runtime error).
    #[error("WASM function call failed: {0}")]
    CallFailed(String),

    /// Execution exceeded fuel limit (prevents infinite loops).
    #[error("WASM execution exceeded fuel limit ({0} fuel consumed)")]
    FuelExceeded(u64),

    /// Execution exceeded time limit.
    #[error("WASM execution exceeded time limit ({0}s)")]
    TimeoutExceeded(u64),

    /// Memory allocation exceeded limit.
    #[error("WASM memory exceeded limit ({0} pages requested, max {1})")]
    MemoryExceeded(u32, u32),

    /// Invalid JSON input/output for tool execution.
    #[error("WASM tool JSON error: {0}")]
    JsonError(String),

    /// Module not found in cache.
    #[error("WASM module '{0}' not found in cache")]
    ModuleNotFound(String),

    /// File I/O error when loading WASM module.
    #[error("Failed to read WASM file: {0}")]
    FileReadError(String),

    /// Guest module produced invalid metadata.
    #[error("Invalid WASM tool metadata: {0}")]
    InvalidMetadata(String),

    /// Maximum concurrent instances exceeded.
    #[error("Maximum concurrent WASM instances exceeded (limit: {0})")]
    MaxInstancesExceeded(usize),

    /// Guest called abort with a message.
    #[error("WASM guest aborted: {0}")]
    GuestAbort(String),

    /// WASI initialization failed.
    #[error("WASI initialization failed: {0}")]
    WasiInitFailed(String),

    /// WASI directory access denied (path not in whitelist).
    #[error("WASI directory access denied: '{0}' not in allowed_dirs")]
    WasiAccessDenied(String),

    /// Module registry error (duplicate name, not found, etc.).
    #[error("WASM module registry error: {0}")]
    RegistryError(String),

    /// Module URL fetch failed.
    #[error("Failed to fetch WASM module from URL: {0}")]
    UrlFetchFailed(String),

    /// Module health check failed.
    #[error("WASM module health check failed for '{0}': {1}")]
    HealthCheckFailed(String, String),
}

/// Result type for WASM operations.
pub type Result<T> = std::result::Result<T, WasmError>;
