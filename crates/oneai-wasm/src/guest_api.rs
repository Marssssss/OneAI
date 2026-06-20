//! Guest API — host functions exposed to WASM guest modules.
//!
//! This module defines the minimal host API available to WASM guests.
//! Following the principle of least privilege, only 3 functions are exposed:
//!
//! 1. `host_log(level, msg_ptr, msg_len)` — print debug/audit log messages
//! 2. `host_get_env(key_ptr, key_len) -> (found: u32, val_len: u32)` — read whitelisted env vars
//! 3. `host_abort(msg_ptr, msg_len)` — guest-initiated termination
//!
//! All other operations (filesystem, network, process creation) are NOT available.

use std::collections::HashMap;

use wasmtime::Linker;

use crate::error::{WasmError, Result};
use crate::runtime::WasmStoreState;

/// Host function types available to WASM guests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasmHostFunction {
    /// Print a log message (trace/info/warn/error).
    Log,
    /// Read a whitelisted environment variable.
    GetEnv,
    /// Abort execution with a message (triggers a trap).
    Abort,
}

impl WasmHostFunction {
    /// Get all available host functions.
    pub fn all() -> Vec<WasmHostFunction> {
        vec![WasmHostFunction::Log, WasmHostFunction::GetEnv, WasmHostFunction::Abort]
    }

    /// Get the Wasmtime function name for this host function.
    pub fn wasmtime_name(&self) -> &str {
        match self {
            WasmHostFunction::Log => "oneai_host_log",
            WasmHostFunction::GetEnv => "oneai_host_get_env",
            WasmHostFunction::Abort => "oneai_host_abort",
        }
    }
}

/// Guest API manager — controls which host functions are available.
pub struct WasmGuestApi {
    /// Which host functions are enabled.
    enabled_functions: Vec<WasmHostFunction>,
    /// Custom environment whitelist (extends the default whitelist).
    #[allow(dead_code)]
    custom_env_vars: HashMap<String, String>,
}

impl WasmGuestApi {
    /// Create a guest API with all functions enabled.
    pub fn full() -> Self {
        Self {
            enabled_functions: WasmHostFunction::all(),
            custom_env_vars: HashMap::new(),
        }
    }

    /// Create a minimal guest API (only log and abort).
    pub fn minimal() -> Self {
        Self {
            enabled_functions: vec![WasmHostFunction::Log, WasmHostFunction::Abort],
            custom_env_vars: HashMap::new(),
        }
    }

    /// Create a strict guest API (only abort).
    pub fn strict() -> Self {
        Self {
            enabled_functions: vec![WasmHostFunction::Abort],
            custom_env_vars: HashMap::new(),
        }
    }

    /// Create a guest API with custom environment variables.
    pub fn with_env_vars(env_vars: HashMap<String, String>) -> Self {
        Self {
            enabled_functions: WasmHostFunction::all(),
            custom_env_vars: env_vars,
        }
    }

    /// Get the enabled functions.
    pub fn enabled_functions(&self) -> &[WasmHostFunction] {
        &self.enabled_functions
    }
}

impl Default for WasmGuestApi {
    fn default() -> Self {
        Self::full()
    }
}

// ─── Host function implementations ──────────────────────────────────────────

/// Read a string from WASM linear memory using ptr + len.
/// Takes a mutable caller to allow access to exports.
fn read_string_from_memory(
    caller: &mut wasmtime::Caller<'_, WasmStoreState>,
    ptr: u32,
    len: u32,
) -> Option<String> {
    let memory = match caller.get_export("memory") {
        Some(wasmtime::Extern::Memory(mem)) => mem,
        _ => return None,
    };

    let data = memory.data(&*caller);
    let start = ptr as usize;
    let end = start + len as usize;

    if end > data.len() {
        return None;
    }

    Some(String::from_utf8_lossy(&data[start..end]).into_owned())
}

/// Register host functions into a Wasmtime Linker.
///
/// Each host function reads from the guest's linear memory using
/// the standard ptr + len WASM protocol.
pub fn register_host_functions(linker: &mut Linker<WasmStoreState>) -> Result<()> {
    // ─── host_log(level: u32, msg_ptr: u32, msg_len: u32) → void ────
    linker.func_wrap(
        "oneai",
        "host_log",
        |mut caller: wasmtime::Caller<'_, WasmStoreState>, level: u32, msg_ptr: u32, msg_len: u32| {
            let msg = match read_string_from_memory(&mut caller, msg_ptr, msg_len) {
                Some(s) => s,
                None => return,
            };

            match level {
                0 => tracing::trace!("WASM guest: {}", msg),
                1 => tracing::info!("WASM guest: {}", msg),
                2 => tracing::warn!("WASM guest: {}", msg),
                3 => tracing::error!("WASM guest: {}", msg),
                _ => tracing::info!("WASM guest (level {}): {}", level, msg),
            }
        },
    ).map_err(|e| WasmError::InstantiationFailed(format!("Failed to register host_log: {}", e)))?;

    // ─── host_get_env(key_ptr: u32, key_len: u32) → (found: u32, val_len: u32) ────
    //
    // Returns (0, 0) if not found/in whitelist.
    // If found, stores value in host state output buffer and returns (1, val_len).
    linker.func_wrap(
        "oneai",
        "host_get_env",
        |mut caller: wasmtime::Caller<'_, WasmStoreState>, key_ptr: u32, key_len: u32| -> (u32, u32) {
            let key = match read_string_from_memory(&mut caller, key_ptr, key_len) {
                Some(s) => s,
                None => return (0, 0),
            };

            // Look up in the whitelist and clone value before mutation
            let val = caller.data().host_state().get_env(&key).cloned();
            match val {
                Some(val_str) => {
                    let val_len = val_str.len() as u32;
                    caller.data_mut().host_state_mut().set_output(val_str.into_bytes());
                    (1, val_len)
                }
                None => (0, 0),
            }
        },
    ).map_err(|e| WasmError::InstantiationFailed(format!("Failed to register host_get_env: {}", e)))?;

    // ─── host_abort(msg_ptr: u32, msg_len: u32) → void ────
    linker.func_wrap(
        "oneai",
        "host_abort",
        |mut caller: wasmtime::Caller<'_, WasmStoreState>, msg_ptr: u32, msg_len: u32| {
            let msg = match read_string_from_memory(&mut caller, msg_ptr, msg_len) {
                Some(s) => s,
                None => "unknown abort reason".to_string(),
            };

            tracing::warn!("WASM guest aborted: {}", msg);
            caller.data_mut().host_state_mut().set_output(format!("Guest aborted: {}", msg).into_bytes());
        },
    ).map_err(|e| WasmError::InstantiationFailed(format!("Failed to register host_abort: {}", e)))?;

    Ok(())
}

/// Register a subset of host functions based on the WasmGuestApi configuration.
pub fn register_host_functions_with_api(
    linker: &mut Linker<WasmStoreState>,
    api: &WasmGuestApi,
) -> Result<()> {
    // Always register abort — it's the guest's only error mechanism
    linker.func_wrap(
        "oneai",
        "host_abort",
        |mut caller: wasmtime::Caller<'_, WasmStoreState>, msg_ptr: u32, msg_len: u32| {
            let msg = match read_string_from_memory(&mut caller, msg_ptr, msg_len) {
                Some(s) => s,
                None => "unknown abort reason".to_string(),
            };
            tracing::warn!("WASM guest aborted: {}", msg);
            caller.data_mut().host_state_mut().set_output(format!("Guest aborted: {}", msg).into_bytes());
        },
    ).map_err(|e| WasmError::InstantiationFailed(format!("Failed to register host_abort: {}", e)))?;

    if api.enabled_functions.contains(&WasmHostFunction::Log) {
        linker.func_wrap(
            "oneai",
            "host_log",
            |mut caller: wasmtime::Caller<'_, WasmStoreState>, level: u32, msg_ptr: u32, msg_len: u32| {
                let msg = match read_string_from_memory(&mut caller, msg_ptr, msg_len) {
                    Some(s) => s,
                    None => return,
                };
                match level {
                    0 => tracing::trace!("WASM guest: {}", msg),
                    1 => tracing::info!("WASM guest: {}", msg),
                    2 => tracing::warn!("WASM guest: {}", msg),
                    3 => tracing::error!("WASM guest: {}", msg),
                    _ => tracing::info!("WASM guest (level {}): {}", level, msg),
                }
            },
        ).map_err(|e| WasmError::InstantiationFailed(format!("Failed to register host_log: {}", e)))?;
    }

    if api.enabled_functions.contains(&WasmHostFunction::GetEnv) {
        linker.func_wrap(
            "oneai",
            "host_get_env",
            |mut caller: wasmtime::Caller<'_, WasmStoreState>, key_ptr: u32, key_len: u32| -> (u32, u32) {
                let key = match read_string_from_memory(&mut caller, key_ptr, key_len) {
                    Some(s) => s,
                    None => return (0, 0),
                };
                let val = caller.data().host_state().get_env(&key).cloned();
                match val {
                    Some(val_str) => {
                        let val_len = val_str.len() as u32;
                        caller.data_mut().host_state_mut().set_output(val_str.into_bytes());
                        (1, val_len)
                    }
                    None => (0, 0),
                }
            },
        ).map_err(|e| WasmError::InstantiationFailed(format!("Failed to register host_get_env: {}", e)))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::WasmRuntime;
    use crate::config::WasmRuntimeConfig;

    #[test]
    fn test_guest_api_default_is_full() {
        let api = WasmGuestApi::default();
        assert_eq!(api.enabled_functions().len(), 3);
    }

    #[test]
    fn test_guest_api_minimal() {
        let api = WasmGuestApi::minimal();
        assert_eq!(api.enabled_functions().len(), 2);
    }

    #[test]
    fn test_guest_api_strict() {
        let api = WasmGuestApi::strict();
        assert_eq!(api.enabled_functions().len(), 1);
    }

    #[test]
    fn test_register_host_functions() {
        let runtime = WasmRuntime::new(WasmRuntimeConfig::default()).unwrap();
        let mut linker = Linker::new(runtime.engine());
        let result = register_host_functions(&mut linker);
        assert!(result.is_ok());
    }

    #[test]
    fn test_register_host_functions_with_api() {
        let runtime = WasmRuntime::new(WasmRuntimeConfig::default()).unwrap();
        let mut linker = Linker::new(runtime.engine());
        let api = WasmGuestApi::minimal();
        let result = register_host_functions_with_api(&mut linker, &api);
        assert!(result.is_ok());
    }

    #[test]
    fn test_host_function_names() {
        assert_eq!(WasmHostFunction::Log.wasmtime_name(), "oneai_host_log");
        assert_eq!(WasmHostFunction::GetEnv.wasmtime_name(), "oneai_host_get_env");
        assert_eq!(WasmHostFunction::Abort.wasmtime_name(), "oneai_host_abort");
    }
}
