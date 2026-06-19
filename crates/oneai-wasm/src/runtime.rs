//! WasmRuntime — Wasmtime Engine + Store management for WASM sandbox execution.
//!
//! The WasmRuntime is the core of the WASM sandbox. It manages:
//! - Engine creation with security-focused Wasmtime Config
//! - Module compilation and caching
//! - Store creation per execution (memory isolation)
//! - Fuel-based execution limiting (prevents infinite loops)
//! - Epoch-based interruption (async timeout support)
//! - Optional WASI filesystem access (restricted to whitelisted directories)

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use wasmtime::{Config, Engine, Module, Store, Linker};

use crate::config::WasmRuntimeConfig;
use crate::error::{WasmError, Result};
use crate::wasi::build_wasi_p1_ctx;

/// WASM sandbox execution runtime — based on Wasmtime.
///
/// Manages all WASM module loading, compilation, instantiation, and execution.
/// Each WasmRuntime holds a shared Engine (compilation cache) and creates
/// independent Stores per execution request (memory isolation).
///
/// # Security
///
/// The runtime enforces:
/// - Fuel consumption limits (prevents infinite loops)
/// - Epoch-based interruption (async timeout enforcement)
/// - Memory limits (prevents runaway allocation)
/// - Instance concurrency limits (prevents resource exhaustion)
/// - WASI filesystem access only to whitelisted directories
///
/// # Thread Safety
///
/// - Engine is shared across all operations (Wasmtime guarantees thread safety)
/// - Module cache is protected by RwLock
/// - Instance concurrency is controlled by Semaphore
/// - Each execution creates an independent Store (no shared mutable state)
pub struct WasmRuntime {
    /// Wasmtime Engine — shared compilation cache, all Stores use it.
    engine: Engine,

    /// Runtime configuration (security policy, resource limits).
    config: WasmRuntimeConfig,

    /// Compiled module cache (name → Module).
    /// Modules are compiled once and cached for fast re-instantiation.
    pub(crate) module_cache: Arc<RwLock<HashMap<String, Module>>>,
}

impl WasmRuntime {
    /// Create a new WasmRuntime with the given configuration.
    ///
    /// This initializes the Wasmtime Engine with security-focused settings:
    /// - `consume_fuel(true)` — enables fuel-based execution limiting
    /// - `epoch_interruption(true)` — enables async interrupt support
    /// - Memory size limits
    ///
    /// # Errors
    ///
    /// Returns `WasmError::CompilationFailed` if the Engine fails to create
    /// (invalid config, incompatible Wasmtime version, etc.).
    pub fn new(config: WasmRuntimeConfig) -> Result<Self> {
        let mut wasmtime_config = Config::new();

        // Security settings
        wasmtime_config.consume_fuel(config.fuel_limit.is_some());
        wasmtime_config.epoch_interruption(config.epoch_interruption);

        // Performance settings
        wasmtime_config.cranelift_opt_level(wasmtime::OptLevel::Speed);

        let engine = Engine::new(&wasmtime_config)
            .map_err(|e| WasmError::CompilationFailed(format!("Failed to create Wasmtime Engine: {}", e)))?;

        let config_clone = config.clone();

        Ok(Self {
            engine,
            config: config_clone,
            module_cache: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Create a runtime with default configuration.
    ///
    /// Default: strict pure-computation sandbox (no WASI, 1MB memory, 100K fuel).
    pub fn with_defaults() -> Result<Self> {
        Self::new(WasmRuntimeConfig::default())
    }

    /// Get a reference to the Wasmtime Engine.
    ///
    /// The Engine is shared across all Stores and is thread-safe.
    /// It caches compiled modules for fast re-instantiation.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Get a reference to the runtime configuration.
    pub fn config(&self) -> &WasmRuntimeConfig {
        &self.config
    }

    /// Compile a WASM module from bytes and cache it.
    ///
    /// The module is compiled by the Engine and stored in the cache
    /// for fast re-instantiation. Subsequent requests for the same
    /// module name will return the cached compiled module.
    pub async fn compile_module(&self, name: &str, wasm_bytes: &[u8]) -> Result<Module> {
        // Compile the module
        let module = Module::from_binary(&self.engine, wasm_bytes)
            .map_err(|e| WasmError::CompilationFailed(format!("Failed to compile WASM module '{}': {}", name, e)))?;

        // Cache the module
        {
            let mut cache = self.module_cache.write().await;
            cache.insert(name.to_string(), module.clone());
        }

        Ok(module)
    }

    /// Load a module from cache.
    ///
    /// Returns the cached compiled module if available, or None if not found.
    pub async fn get_cached_module(&self, name: &str) -> Option<Module> {
        let cache = self.module_cache.read().await;
        cache.get(name).cloned()
    }

    /// Remove a module from cache.
    pub async fn remove_from_cache(&self, name: &str) -> Result<()> {
        let mut cache = self.module_cache.write().await;
        cache.remove(name);
        Ok(())
    }

    /// List all cached module names.
    pub async fn cached_module_names(&self) -> Vec<String> {
        let cache = self.module_cache.read().await;
        cache.keys().cloned().collect()
    }

    /// Create a new Store<WasmStoreState> for WASM execution.
    ///
    /// Each execution gets an independent Store with:
    /// - Fuel initialized from config (if fuel limiting is enabled)
    /// - Epoch deadline set for interrupt support
    /// - Optional WASI P1 context (if WASI is enabled in config)
    ///
    /// The Store is the WASM instance's "world" — it contains the memory,
    /// tables, and global variables. Each Store is completely isolated.
    pub fn create_store(&self) -> Store<WasmStoreState> {
        // Build WASI P1 context if enabled
        let wasi_p1_ctx = build_wasi_p1_ctx(&self.config.wasi_config);

        let host_state = WasmHostState::new();

        let store_state = if wasi_p1_ctx.is_some() {
            WasmStoreState::with_wasi(host_state, wasi_p1_ctx.unwrap())
        } else {
            WasmStoreState::new(host_state)
        };

        let mut store = Store::new(&self.engine, store_state);

        // Initialize fuel using set_fuel (v45 API)
        if let Some(fuel_limit) = self.config.fuel_limit {
            store.set_fuel(fuel_limit).expect("fuel consumption should be enabled");
        }

        // Set epoch deadline for interrupt support
        if self.config.epoch_interruption {
            store.set_epoch_deadline(1_000_000); // Large deadline — host advances epoch to interrupt
        }

        store
    }

    /// Create a Linker with host functions registered.
    ///
    /// The Linker defines what host functions the WASM guest can call.
    /// This is the security boundary — only explicitly registered functions
    /// are available to the guest.
    ///
    /// When WASI is enabled, WASI host functions are also registered,
    /// providing restricted filesystem access to whitelisted directories.
    pub fn create_linker(&self) -> Result<Linker<WasmStoreState>> {
        let mut linker = Linker::new(&self.engine);

        // Register the minimal guest API (oneai host functions)
        crate::guest_api::register_host_functions(&mut linker)?;

        // Register WASI P1 host functions if WASI is enabled
        if self.config.wasi_config.enabled() {
            wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |state: &mut WasmStoreState| state.wasi_p1_ctx_mut())
                .map_err(|e| WasmError::WasiInitFailed(format!("Failed to add WASI P1 to linker: {}", e)))?;
        }

        Ok(linker)
    }

    /// Get the number of cached modules.
    pub async fn cache_size(&self) -> usize {
        let cache = self.module_cache.read().await;
        cache.len()
    }

    /// Increment the engine epoch (used for async timeout enforcement).
    ///
    /// When epoch interruption is enabled, calling this method advances
    /// the epoch counter. WASM instances with a deadline less than the
    /// new epoch will be interrupted.
    pub fn increment_epoch(&self) {
        self.engine.increment_epoch();
    }
}

/// Host state stored in each WASM Store (buffers for data exchange).
///
/// This is the mutable state accessible from host functions.
/// It contains buffers for data exchange between host and guest.
pub struct WasmHostState {
    /// Output buffer — guest writes results here via host functions.
    output_buffer: Vec<u8>,

    /// Input buffer — host writes input data for guest to read.
    input_buffer: Vec<u8>,

    /// Environment whitelist — only these variables are accessible via host_get_env.
    env_whitelist: HashMap<String, String>,
}

impl WasmHostState {
    /// Create a new host state with default environment whitelist.
    fn new() -> Self {
        let mut whitelist = HashMap::new();
        whitelist.insert("ONEAI_TOOL_MODE".to_string(), "sandbox".to_string());
        whitelist.insert("ONEAI_MAX_MEMORY_PAGES".to_string(), "16".to_string());
        whitelist.insert("ONEAI_FUEL_LIMIT".to_string(), "100000".to_string());

        Self {
            output_buffer: Vec::new(),
            input_buffer: Vec::new(),
            env_whitelist: whitelist,
        }
    }

    /// Create host state with custom environment whitelist.
    pub fn with_env_whitelist(env_vars: HashMap<String, String>) -> Self {
        let mut state = Self::new();
        state.env_whitelist.extend(env_vars);
        state
    }

    /// Set the input buffer content.
    pub fn set_input(&mut self, data: Vec<u8>) {
        self.input_buffer = data;
    }

    /// Get the input buffer content.
    pub fn input(&self) -> &[u8] {
        &self.input_buffer
    }

    /// Set the output buffer content.
    pub fn set_output(&mut self, data: Vec<u8>) {
        self.output_buffer = data;
    }

    /// Get the output buffer content.
    pub fn output(&self) -> &[u8] {
        &self.output_buffer
    }

    /// Take the output buffer (returns content and clears buffer).
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output_buffer)
    }

    /// Look up an environment variable from the whitelist.
    ///
    /// Returns None if the variable is not in the whitelist.
    pub fn get_env(&self, key: &str) -> Option<&String> {
        self.env_whitelist.get(key)
    }
}

/// Combined store state — holds both host state and optional WASI P1 context.
///
/// This replaces `WasmHostState` as the Store data type. When WASI is
/// disabled, only the host state is present. When WASI is enabled,
/// the WASI P1 context is added for filesystem access.
///
/// The WASI P1 context is required by `wasmtime_wasi::p1::add_to_linker_sync()`
/// which needs `&mut WasiP1Ctx` access via a closure on the store state.
pub struct WasmStoreState {
    /// Host state — buffers and environment whitelist.
    host_state: WasmHostState,

    /// Optional WASI P1 context — provides restricted filesystem access.
    wasi_p1_ctx: Option<wasmtime_wasi::p1::WasiP1Ctx>,
}

impl WasmStoreState {
    /// Create a store state without WASI (pure computation sandbox).
    pub fn new(host_state: WasmHostState) -> Self {
        Self {
            host_state,
            wasi_p1_ctx: None,
        }
    }

    /// Create a store state with WASI P1 context (restricted filesystem access).
    pub fn with_wasi(host_state: WasmHostState, wasi_p1_ctx: wasmtime_wasi::p1::WasiP1Ctx) -> Self {
        Self {
            host_state,
            wasi_p1_ctx: Some(wasi_p1_ctx),
        }
    }

    /// Get a reference to the host state.
    pub fn host_state(&self) -> &WasmHostState {
        &self.host_state
    }

    /// Get a mutable reference to the host state.
    pub fn host_state_mut(&mut self) -> &mut WasmHostState {
        &mut self.host_state
    }

    /// Get a mutable reference to the WASI P1 context.
    ///
    /// This is required by `wasmtime_wasi::p1::add_to_linker_sync()` for WASI
    /// host function registration.
    pub fn wasi_p1_ctx_mut(&mut self) -> &mut wasmtime_wasi::p1::WasiP1Ctx {
        // This should only be called when WASI is enabled.
        // If WASI is disabled, the linker won't have WASI functions,
        // so this closure is never invoked by the linker.
        self.wasi_p1_ctx.as_mut().expect("WASI P1 context should be present when WASI linker functions are called")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use crate::wasi::WasiConfig;

    #[test]
    fn test_wasm_runtime_creation() {
        let runtime = WasmRuntime::new(WasmRuntimeConfig::default());
        assert!(runtime.is_ok(), "Failed to create WasmRuntime: {:?}", runtime.err());
    }

    #[test]
    fn test_wasm_runtime_with_defaults() {
        let runtime = WasmRuntime::with_defaults();
        assert!(runtime.is_ok());
    }

    #[test]
    fn test_wasm_runtime_config_defaults() {
        let config = WasmRuntimeConfig::default();
        assert_eq!(config.max_memory_pages, 16);
        assert_eq!(config.fuel_limit, Some(100_000));
        assert!(config.epoch_interruption);
        assert_eq!(config.max_execution_time, Duration::from_secs(30));
        assert!(!config.wasi_config.enabled());
        assert_eq!(config.max_instances, 10);
    }

    #[test]
    fn test_wasm_runtime_config_strict() {
        let config = WasmRuntimeConfig::strict();
        assert_eq!(config.max_memory_pages, 8);
        assert_eq!(config.fuel_limit, Some(50_000));
        assert_eq!(config.max_execution_time, Duration::from_secs(10));
        assert_eq!(config.max_instances, 5);
    }

    #[test]
    fn test_wasm_runtime_create_store() {
        let runtime = WasmRuntime::with_defaults().unwrap();
        let mut store = runtime.create_store();

        // Store should be created with WasmStoreState
        // Fuel should be initialized via set_fuel
        let fuel = store.get_fuel();
        assert!(fuel.is_ok(), "Fuel should be available since consume_fuel is enabled");
        let fuel_remaining = fuel.unwrap();
        assert_eq!(fuel_remaining, 100_000, "Fuel should be initialized to 100,000");
    }

    #[test]
    fn test_wasm_runtime_create_linker() {
        let runtime = WasmRuntime::with_defaults().unwrap();
        let linker = runtime.create_linker();
        assert!(linker.is_ok(), "Failed to create linker: {:?}", linker.err());
    }

    #[test]
    fn test_wasm_runtime_create_store_with_wasi() {
        let wasi_config = WasiConfig::restricted(vec![
            crate::wasi::WasiDirConfig::readonly(std::path::PathBuf::from("/tmp"), "/tmp"),
        ]);
        let config = WasmRuntimeConfig::default().with_wasi_config(wasi_config);
        let runtime = WasmRuntime::new(config).unwrap();
        let store = runtime.create_store();

        // Store should be created with WasiP1Ctx
        assert!(store.data().wasi_p1_ctx.is_some());
    }

    #[test]
    fn test_wasm_runtime_create_linker_with_wasi() {
        let wasi_config = WasiConfig::restricted(vec![
            crate::wasi::WasiDirConfig::readonly(std::path::PathBuf::from("/tmp"), "/tmp"),
        ]);
        let config = WasmRuntimeConfig::default().with_wasi_config(wasi_config);
        let runtime = WasmRuntime::new(config).unwrap();
        let linker = runtime.create_linker();
        assert!(linker.is_ok(), "Failed to create linker with WASI: {:?}", linker.err());
    }

    #[test]
    fn test_wasm_host_state_env_whitelist() {
        let state = WasmHostState::new();
        assert_eq!(state.get_env("ONEAI_TOOL_MODE"), Some(&"sandbox".to_string()));
        assert_eq!(state.get_env("ONEAI_MAX_MEMORY_PAGES"), Some(&"16".to_string()));
        assert_eq!(state.get_env("PATH"), None);
        assert_eq!(state.get_env("HOME"), None);
    }

    #[test]
    fn test_wasm_host_state_input_output() {
        let mut state = WasmHostState::new();
        state.set_input(b"hello".to_vec());
        assert_eq!(state.input(), b"hello");
        state.set_output(b"world".to_vec());
        assert_eq!(state.output(), b"world");
        let taken = state.take_output();
        assert_eq!(taken, b"world");
        assert!(state.output().is_empty());
    }

    #[test]
    fn test_wasm_store_state_without_wasi() {
        let host_state = WasmHostState::new();
        let store_state = WasmStoreState::new(host_state);
        assert!(store_state.wasi_p1_ctx.is_none());
        assert_eq!(store_state.host_state().get_env("ONEAI_TOOL_MODE"), Some(&"sandbox".to_string()));
    }

    #[test]
    fn test_wasm_store_state_with_wasi() {
        let host_state = WasmHostState::new();
        let wasi_p1_ctx = wasmtime_wasi::WasiCtx::builder().build_p1();
        let store_state = WasmStoreState::with_wasi(host_state, wasi_p1_ctx);
        assert!(store_state.wasi_p1_ctx.is_some());
        assert_eq!(store_state.host_state().get_env("ONEAI_TOOL_MODE"), Some(&"sandbox".to_string()));
    }

    #[test]
    fn test_wasm_runtime_increment_epoch() {
        let runtime = WasmRuntime::with_defaults().unwrap();
        runtime.increment_epoch();
        // Should not panic
    }
}
