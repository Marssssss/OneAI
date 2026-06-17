//! WasmModuleManager — WASM module loading, caching, and lifecycle management.
//!
//! Supports three sources of WASM modules:
//! 1. File path loading (.wasm files)
//! 2. Memory bytes loading (dynamic code-as-action)
//! 3. URL loading (remote WASM module repository — future)
//!
//! Modules are compiled once by the WasmRuntime Engine and cached
//! for fast re-instantiation. WasmModuleManager also handles
//! module metadata extraction (tool_name, description, schema).

use std::path::Path;
use std::sync::Arc;

use crate::config::WasmRuntimeConfig;
use crate::error::{WasmError, Result};
use crate::runtime::WasmRuntime;
use crate::runtime::WasmHostState;
use crate::tool::{WasmTool, WasmToolMetadata};

/// WASM module manager — loading, caching, and lifecycle.
///
/// Each WasmModuleManager is associated with a WasmRuntime.
/// It handles:
/// - Loading WASM modules from files or bytes
/// - Compiling and caching modules via the Engine
/// - Extracting tool metadata (name, description, schema, risk_level)
/// - Creating WasmTool instances from loaded modules
///
/// # Example
///
/// ```ignore
/// let runtime = Arc::new(WasmRuntime::new(WasmRuntimeConfig::default())?);
/// let manager = WasmModuleManager::new(runtime);
///
/// // Load a WASM tool from file
/// let tool = manager.load_from_file("calculator", Path::new("calc.wasm")).await?;
///
/// // Register in ToolRegistry
/// registry.register(tool).await?;
/// ```
pub struct WasmModuleManager {
    /// Reference to the WASM runtime.
    runtime: Arc<WasmRuntime>,
}

impl WasmModuleManager {
    /// Create a new module manager with the given runtime.
    pub fn new(runtime: Arc<WasmRuntime>) -> Self {
        Self { runtime }
    }

    /// Create a module manager with default runtime configuration.
    pub fn with_defaults() -> Result<Self> {
        let runtime = Arc::new(WasmRuntime::with_defaults()?);
        Ok(Self { runtime })
    }

    /// Create a module manager with custom runtime configuration.
    pub fn with_config(config: WasmRuntimeConfig) -> Result<Self> {
        let runtime = Arc::new(WasmRuntime::new(config)?);
        Ok(Self { runtime })
    }

    /// Load a WASM module from a file path and create a WasmTool.
    ///
    /// The file must be a valid .wasm binary. The module is compiled
    /// by the Engine, cached, and instantiated to extract metadata.
    ///
    /// # Steps
    /// 1. Read the .wasm file bytes
    /// 2. Compile via WasmRuntime.compile_module()
    /// 3. Instantiate to extract metadata (tool_name, description, etc.)
    /// 4. Create WasmTool with extracted metadata
    ///
    /// # Errors
    /// - `WasmError::FileReadError` — file doesn't exist or can't be read
    /// - `WasmError::CompilationFailed` — invalid WASM bytecode
    /// - `WasmError::ExportNotFound` — missing required guest exports
    /// - `WasmError::InvalidMetadata` — metadata values are invalid
    pub async fn load_from_file(&self, name: &str, path: &Path) -> Result<Arc<WasmTool>> {
        // Read the file bytes
        let wasm_bytes = std::fs::read(path)
            .map_err(|e| WasmError::FileReadError(format!("Failed to read {}: {}", path.display(), e)))?;

        self.load_from_bytes(name, &wasm_bytes).await
    }

    /// Load a WASM module from in-memory bytes and create a WasmTool.
    ///
    /// This is the primary method for code-as-action: the agent generates
    /// WASM bytecode (or uses a template) and it's loaded directly.
    ///
    /// # Steps
    /// 1. Compile via WasmRuntime.compile_module()
    /// 2. Instantiate to extract metadata
    /// 3. Create WasmTool
    pub async fn load_from_bytes(&self, name: &str, wasm_bytes: &[u8]) -> Result<Arc<WasmTool>> {
        // Compile the module
        let module = self.runtime.compile_module(name, wasm_bytes).await?;

        // Extract metadata by instantiating the module
        let metadata = self.extract_metadata(&module)?;

        // Create the WasmTool
        let tool = WasmTool::new(name, metadata, self.runtime.clone());

        Ok(Arc::new(tool))
    }

    /// Extract tool metadata from a compiled WASM module.
    ///
    /// Creates a temporary Store and Instance, calls the guest's
    /// metadata export functions to get:
    /// - tool_name
    /// - tool_description
    /// - tool_parameters_schema
    /// - tool_risk_level
    ///
    /// # Required Guest Exports
    ///
    /// The WASM module must export these functions:
    /// - `tool_name() -> (ptr: u32, len: u32)` — returns the tool name string
    /// - `tool_description() -> (ptr: u32, len: u32)` — returns the description
    /// - `tool_parameters_schema() -> (ptr: u32, len: u32)` — returns JSON Schema
    /// - `tool_risk_level() -> (ptr: u32, len: u32)` — returns "low"/"medium"/"high"
    fn extract_metadata(&self, module: &wasmtime::Module) -> Result<WasmToolMetadata> {
        // Create a temporary Store for metadata extraction
        let mut store = self.runtime.create_store();
        let linker = self.runtime.create_linker()?;

        // Instantiate the module
        let instance = linker.instantiate(&mut store, module)
            .map_err(|e| WasmError::InstantiationFailed(format!("Failed to instantiate for metadata extraction: {}", e)))?;

        // Extract each metadata field
        let name = self.read_string_export(&mut store, &instance, "tool_name")?;
        let description = self.read_string_export(&mut store, &instance, "tool_description")?;
        let schema_str = self.read_string_export(&mut store, &instance, "tool_parameters_schema")?;
        let risk_str = self.read_string_export(&mut store, &instance, "tool_risk_level")?;

        // Parse the JSON Schema
        let parameters_schema: serde_json::Value = serde_json::from_str(&schema_str)
            .map_err(|e| WasmError::InvalidMetadata(format!("Invalid JSON Schema from tool_parameters_schema: {}", e)))?;

        // Parse the risk level
        let risk_level = match risk_str.to_lowercase().as_str() {
            "low" => oneai_core::RiskLevel::Low,
            "medium" => oneai_core::RiskLevel::Medium,
            "high" => oneai_core::RiskLevel::High,
            _ => oneai_core::RiskLevel::Low, // Default to Low for WASM sandbox safety
        };

        Ok(WasmToolMetadata {
            name,
            description,
            parameters_schema,
            risk_level,
        })
    }

    /// Read a string from a WASM export function that returns (ptr, len).
    ///
    /// The function must return two u32 values: pointer and length
    /// of the string in the guest's linear memory.
    fn read_string_export(
        &self,
        store: &mut wasmtime::Store<WasmHostState>,
        instance: &wasmtime::Instance,
        export_name: &str,
    ) -> Result<String> {
        // Get the exported function
        let func = instance.get_typed_func::<(), (u32, u32)>(&mut *store, export_name)
            .map_err(|e| WasmError::ExportNotFound(format!("Export '{}' not found: {}", export_name, e)))?;

        // Call the function
        let (ptr, len) = func.call(&mut *store, ())
            .map_err(|e| WasmError::CallFailed(format!("Failed to call '{}': {}", export_name, e)))?;

        // Read the string from guest memory (need AsContextMut for get_memory in v45)
        let memory = instance.get_memory(&mut *store, "memory")
            .ok_or_else(|| WasmError::ExportNotFound("memory export not found".to_string()))?;

        let data = memory.data(&*store);
        let start = ptr as usize;
        let end = start + len as usize;

        if end > data.len() {
            return Err(WasmError::InvalidMetadata(format!(
                "String export '{}' returned invalid memory range ({}, {})",
                export_name, ptr, len
            )));
        }

        let bytes = &data[start..end];
        let string = String::from_utf8_lossy(bytes).into_owned();

        Ok(string)
    }

    /// Get a cached module tool (if previously loaded).
    ///
    /// Returns None if the module name is not in the cache.
    pub async fn get_cached(&self, name: &str) -> Option<Arc<WasmTool>> {
        // Check if module is cached in runtime
        if self.runtime.get_cached_module(name).await.is_some() {
            // Module bytes are cached — but WasmTool needs re-creation
            // For simplicity, return None (caller must re-load)
            None
        } else {
            None
        }
    }

    /// Unload a module from the runtime cache.
    pub async fn unload(&self, name: &str) -> Result<()> {
        self.runtime.remove_from_cache(name).await
    }

    /// List all cached module names.
    pub async fn list_loaded(&self) -> Vec<String> {
        self.runtime.cached_module_names().await
    }

    /// Get a reference to the WASM runtime.
    pub fn runtime(&self) -> &Arc<WasmRuntime> {
        &self.runtime
    }

    /// Get the runtime configuration.
    pub fn config(&self) -> &WasmRuntimeConfig {
        self.runtime.config()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_module_manager_with_defaults() {
        let manager = WasmModuleManager::with_defaults();
        assert!(manager.is_ok());
    }

    #[test]
    fn test_module_manager_with_config() {
        let config = WasmRuntimeConfig::strict();
        let manager = WasmModuleManager::with_config(config);
        assert!(manager.is_ok());
    }

    #[tokio::test]
    async fn test_module_manager_list_loaded_empty() {
        let manager = WasmModuleManager::with_defaults().unwrap();
        let names = manager.list_loaded().await;
        assert!(names.is_empty());
    }

    #[tokio::test]
    async fn test_module_manager_unload_nonexistent() {
        let manager = WasmModuleManager::with_defaults().unwrap();
        let result = manager.unload("nonexistent").await;
        assert!(result.is_ok()); // Removing nonexistent is OK
    }
}
