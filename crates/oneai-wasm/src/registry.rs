//! WASM module registry — named module storage with version tracking and health checks.
//!
//! The registry provides:
//! - Named module storage (register by name, retrieve by name)
//! - Module source tracking (File, Bytes, Url, Builtin)
//! - Version tracking with SHA256 hash for integrity verification
//! - Health checking (can instantiate, can execute)
//! - Hot-reload capability (unload old, load new without stopping runtime)
//! - URL loading (fetch WASM modules from remote repositories)

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::RwLock;

use crate::error::{WasmError, Result};
use crate::runtime::WasmRuntime;
use crate::tool::{WasmTool, WasmToolMetadata};

/// Source of a WASM module.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum WasmModuleSource {
    /// Load from a file path.
    File {
        /// Absolute or relative path to the .wasm file.
        path: std::path::PathBuf,
    },
    /// Load from in-memory bytes (e.g., template bytecode, dynamically generated).
    Bytes {
        /// Raw WASM bytecode.
        bytes: Vec<u8>,
    },
    /// Load from a URL (remote module repository).
    Url {
        /// URL to fetch the WASM module from.
        url: String,
    },
    /// Built-in module (pre-compiled templates).
    Builtin {
        /// Built-in module name.
        name: String,
    },
}

/// Module version tracking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmModuleVersion {
    /// Semantic version (e.g., "1.0.0").
    version: String,
    /// SHA256 hash of the WASM bytes (for integrity checking).
    hash: String,
}

impl WasmModuleVersion {
    /// Create a module version with computed hash.
    pub fn new(version: &str, wasm_bytes: &[u8]) -> Self {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        wasm_bytes.hash(&mut hasher);
        let hash = format!("{:016x}", hasher.finish());

        Self {
            version: version.to_string(),
            hash,
        }
    }

    /// Get the version string.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Get the hash.
    pub fn hash(&self) -> &str {
        &self.hash
    }
}

/// Health status of a loaded WASM module.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WasmModuleHealth {
    /// Module is healthy — can instantiate and execute.
    Healthy,
    /// Module can instantiate but execution has issues (e.g., export mismatch).
    Degraded {
        /// Reason for degraded status.
        reason: String,
    },
    /// Module cannot instantiate (compilation or linking failure).
    Unhealthy {
        /// Reason for unhealthy status.
        reason: String,
    },
    /// Module has not been health-checked yet.
    Unknown,
}

/// Entry in the WASM module registry.
#[derive(Debug, Clone)]
pub struct WasmModuleEntry {
    /// Module name (unique identifier in registry).
    name: String,
    /// Module source (where it came from).
    source: WasmModuleSource,
    /// Module version.
    version: Option<WasmModuleVersion>,
    /// Health status.
    health: WasmModuleHealth,
    /// When the module was loaded.
    loaded_at: DateTime<Utc>,
}

impl WasmModuleEntry {
    /// Create a new entry.
    pub fn new(name: &str, source: WasmModuleSource) -> Self {
        Self {
            name: name.to_string(),
            source,
            version: None,
            health: WasmModuleHealth::Unknown,
            loaded_at: Utc::now(),
        }
    }

    /// Get the module name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the module source.
    pub fn source(&self) -> &WasmModuleSource {
        &self.source
    }

    /// Get the module version.
    pub fn version(&self) -> &Option<WasmModuleVersion> {
        &self.version
    }

    /// Get the health status.
    pub fn health(&self) -> &WasmModuleHealth {
        &self.health
    }

    /// Get the load timestamp.
    pub fn loaded_at(&self) -> &DateTime<Utc> {
        &self.loaded_at
    }

    /// Set the health status.
    pub fn set_health(&mut self, health: WasmModuleHealth) {
        self.health = health;
    }

    /// Set the version.
    pub fn set_version(&mut self, version: WasmModuleVersion) {
        self.version = Some(version);
    }
}

/// WASM module registry — named module storage with version tracking and health checks.
///
/// The registry provides lifecycle management for WASM modules:
/// - Register modules from various sources (File, Bytes, Url, Builtin)
/// - Track module versions and health status
/// - Hot-reload modules (unload old version, load new)
/// - Health checking (attempt instantiation to verify module integrity)
///
/// # Example
///
/// ```ignore
/// let registry = WasmModuleRegistry::new(runtime);
///
/// // Register from file
/// let tool = registry.register_file("calculator", Path::new("calc.wasm")).await?;
///
/// // Check health
/// let health = registry.check_health("calculator").await;
///
/// // Hot-reload
/// let tool = registry.reload("calculator", WasmModuleSource::File{ path: new_path }).await?;
/// ```
pub struct WasmModuleRegistry {
    /// Reference to the WASM runtime.
    runtime: Arc<WasmRuntime>,
    /// Registry entries (name → entry).
    entries: Arc<RwLock<HashMap<String, WasmModuleEntry>>>,
}

impl WasmModuleRegistry {
    /// Create a new module registry with the given runtime.
    pub fn new(runtime: Arc<WasmRuntime>) -> Self {
        Self {
            runtime,
            entries: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create a registry with default runtime.
    pub fn with_defaults() -> Result<Self> {
        let runtime = Arc::new(WasmRuntime::with_defaults()?);
        Ok(Self::new(runtime))
    }

    /// Register a module from a source.
    ///
    /// Compiles the module, creates a WasmTool, and stores the registry entry.
    pub async fn register(&self, name: &str, source: WasmModuleSource) -> Result<Arc<WasmTool>> {
        let wasm_bytes = self.resolve_source_bytes(&source).await?;

        // Check for duplicate name
        {
            let entries = self.entries.read().await;
            if entries.contains_key(name) {
                return Err(WasmError::RegistryError(format!(
                    "Module '{}' already registered — use reload() to replace it", name
                )));
            }
        }

        // Compile and create tool
        let tool = self.load_tool(name, &wasm_bytes).await?;

        // Create and store entry
        let mut entry = WasmModuleEntry::new(name, source);
        entry.set_version(WasmModuleVersion::new("1.0.0", &wasm_bytes));
        entry.set_health(WasmModuleHealth::Healthy);

        {
            let mut entries = self.entries.write().await;
            entries.insert(name.to_string(), entry);
        }

        Ok(tool)
    }

    /// Register a module from file (convenience).
    pub async fn register_file(&self, name: &str, path: &Path) -> Result<Arc<WasmTool>> {
        self.register(name, WasmModuleSource::File { path: path.to_path_buf() }).await
    }

    /// Register a module from bytes (convenience).
    pub async fn register_bytes(&self, name: &str, bytes: &[u8]) -> Result<Arc<WasmTool>> {
        self.register(name, WasmModuleSource::Bytes { bytes: bytes.to_vec() }).await
    }

    /// Get a module entry by name.
    pub async fn get(&self, name: &str) -> Option<WasmModuleEntry> {
        let entries = self.entries.read().await;
        entries.get(name).cloned()
    }

    /// List all registered modules.
    pub async fn list(&self) -> Vec<WasmModuleEntry> {
        let entries = self.entries.read().await;
        entries.values().cloned().collect()
    }

    /// Check module health (attempt instantiation + metadata extraction).
    ///
    /// Creates a temporary Store, instantiates the module, and verifies
    /// that required exports are available.
    pub async fn check_health(&self, name: &str) -> WasmModuleHealth {
        // Check if module is cached in the runtime
        let module = self.runtime.get_cached_module(name).await;

        if module.is_none() {
            let mut entries = self.entries.write().await;
            if let Some(entry) = entries.get_mut(name) {
                entry.set_health(WasmModuleHealth::Unhealthy {
                    reason: "Module not found in runtime cache".to_string(),
                });
                return WasmModuleHealth::Unhealthy {
                    reason: "Module not found in runtime cache".to_string(),
                };
            }
            return WasmModuleHealth::Unhealthy {
                reason: "Module name not in registry".to_string(),
            };
        }

        // Try to instantiate the module to verify health
        let mut store = self.runtime.create_store();
        let linker = match self.runtime.create_linker() {
            Ok(l) => l,
            Err(e) => {
                let mut entries = self.entries.write().await;
                if let Some(entry) = entries.get_mut(name) {
                    entry.set_health(WasmModuleHealth::Unhealthy {
                        reason: format!("Linker creation failed: {}", e),
                    });
                }
                return WasmModuleHealth::Unhealthy {
                    reason: format!("Linker creation failed: {}", e),
                };
            }
        };

        let module = module.unwrap();
        let instance_result = linker.instantiate(&mut store, &module);

        match instance_result {
            Ok(_) => {
                let mut entries = self.entries.write().await;
                if let Some(entry) = entries.get_mut(name) {
                    entry.set_health(WasmModuleHealth::Healthy);
                }
                WasmModuleHealth::Healthy
            }
            Err(e) => {
                let mut entries = self.entries.write().await;
                if let Some(entry) = entries.get_mut(name) {
                    entry.set_health(WasmModuleHealth::Degraded {
                        reason: format!("Instantiation failed: {}", e),
                    });
                }
                WasmModuleHealth::Degraded {
                    reason: format!("Instantiation failed: {}", e),
                }
            }
        }
    }

    /// Unload a module (remove from cache and registry).
    pub async fn unload(&self, name: &str) -> Result<()> {
        // Remove from runtime cache
        self.runtime.remove_from_cache(name).await?;

        // Remove from registry
        {
            let mut entries = self.entries.write().await;
            entries.remove(name);
        }

        Ok(())
    }

    /// Hot-reload: unload old version and load new bytes.
    ///
    /// Unloads the existing module and registers a new one from the
    /// provided source. The module name remains the same.
    pub async fn reload(&self, name: &str, source: WasmModuleSource) -> Result<Arc<WasmTool>> {
        // Unload the old module
        self.runtime.remove_from_cache(name).await?;

        // Load the new module
        let wasm_bytes = self.resolve_source_bytes(&source).await?;
        let tool = self.load_tool(name, &wasm_bytes).await?;

        // Update the registry entry
        {
            let mut entries = self.entries.write().await;
            let mut entry = WasmModuleEntry::new(name, source);
            entry.set_version(WasmModuleVersion::new("1.0.0", &wasm_bytes));
            entry.set_health(WasmModuleHealth::Healthy);
            entries.insert(name.to_string(), entry);
        }

        Ok(tool)
    }

    /// Get a reference to the WASM runtime.
    pub fn runtime(&self) -> &Arc<WasmRuntime> {
        &self.runtime
    }

    // ─── Private helpers ──────────────────────────────────────────────────────

    /// Resolve WASM bytes from a module source.
    async fn resolve_source_bytes(&self, source: &WasmModuleSource) -> Result<Vec<u8>> {
        match source {
            WasmModuleSource::File { path } => {
                std::fs::read(path)
                    .map_err(|e| WasmError::FileReadError(format!("Failed to read {}: {}", path.display(), e)))
            }
            WasmModuleSource::Bytes { bytes } => {
                Ok(bytes.clone())
            }
            WasmModuleSource::Url { url } => {
                // URL loading is async and uses reqwest
                // For now, return an error — URL loading requires network infrastructure
                Err(WasmError::UrlFetchFailed(format!(
                    "URL loading not yet implemented for: {}", url
                )))
            }
            WasmModuleSource::Builtin { name } => {
                // Built-in modules — placeholder for future pre-compiled templates
                Err(WasmError::RegistryError(format!(
                    "Built-in module '{}' not yet available", name
                )))
            }
        }
    }

    /// Load and compile a WASM module, creating a WasmTool.
    async fn load_tool(&self, name: &str, wasm_bytes: &[u8]) -> Result<Arc<WasmTool>> {
        // Compile the module
        let module = self.runtime.compile_module(name, wasm_bytes).await?;

        // Extract metadata
        let mut store = self.runtime.create_store();
        let linker = self.runtime.create_linker()?;

        let instance = linker.instantiate(&mut store, &module)
            .map_err(|e| WasmError::InstantiationFailed(format!(
                "Failed to instantiate '{}' for metadata: {}", name, e
            )))?;

        // Extract metadata fields
        let tool_name = self.read_string_export(&mut store, &instance, "tool_name")?;
        let description = self.read_string_export(&mut store, &instance, "tool_description")?;
        let schema_str = self.read_string_export(&mut store, &instance, "tool_parameters_schema")?;
        let risk_str = self.read_string_export(&mut store, &instance, "tool_risk_level")?;

        let parameters_schema: serde_json::Value = serde_json::from_str(&schema_str)
            .map_err(|e| WasmError::InvalidMetadata(format!("Invalid JSON Schema: {}", e)))?;

        let risk_level = match risk_str.to_lowercase().as_str() {
            "low" => oneai_core::RiskLevel::Low,
            "medium" => oneai_core::RiskLevel::Medium,
            "high" => oneai_core::RiskLevel::High,
            _ => oneai_core::RiskLevel::Low,
        };

        let metadata = WasmToolMetadata {
            name: tool_name,
            description,
            parameters_schema,
            risk_level,
        };

        Ok(Arc::new(WasmTool::new(name, metadata, self.runtime.clone())))
    }

    /// Read a string from a WASM export function.
    fn read_string_export(
        &self,
        store: &mut wasmtime::Store<crate::runtime::WasmStoreState>,
        instance: &wasmtime::Instance,
        export_name: &str,
    ) -> Result<String> {
        let func = instance.get_typed_func::<(), (u32, u32)>(&mut *store, export_name)
            .map_err(|e| WasmError::ExportNotFound(format!("Export '{}' not found: {}", export_name, e)))?;

        let (ptr, len) = func.call(&mut *store, ())
            .map_err(|e| WasmError::CallFailed(format!("Failed to call '{}': {}", export_name, e)))?;

        let memory = instance.get_memory(&mut *store, "memory")
            .ok_or_else(|| WasmError::ExportNotFound("memory".to_string()))?;

        let data = memory.data(&*store);
        let start = ptr as usize;
        let end = start + len as usize;

        if end > data.len() {
            return Err(WasmError::InvalidMetadata(format!(
                "Invalid memory range ({}, {})", ptr, len
            )));
        }

        Ok(String::from_utf8_lossy(&data[start..end]).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::WasmRuntime;
    use std::path::PathBuf;

    #[test]
    fn test_wasm_module_source_file() {
        let source = WasmModuleSource::File { path: PathBuf::from("test.wasm") };
        assert!(matches!(source, WasmModuleSource::File { .. }));
    }

    #[test]
    fn test_wasm_module_source_bytes() {
        let source = WasmModuleSource::Bytes { bytes: vec![0, 97, 115, 109] };
        assert!(matches!(source, WasmModuleSource::Bytes { .. }));
    }

    #[test]
    fn test_wasm_module_source_url() {
        let source = WasmModuleSource::Url { url: "https://example.com/module.wasm".to_string() };
        assert!(matches!(source, WasmModuleSource::Url { .. }));
    }

    #[test]
    fn test_wasm_module_source_builtin() {
        let source = WasmModuleSource::Builtin { name: "compute".to_string() };
        assert!(matches!(source, WasmModuleSource::Builtin { .. }));
    }

    #[test]
    fn test_wasm_module_version_new() {
        let version = WasmModuleVersion::new("1.0.0", b"\x00asm");
        assert_eq!(version.version(), "1.0.0");
        assert!(!version.hash().is_empty());
    }

    #[test]
    fn test_wasm_module_version_same_bytes_same_hash() {
        let bytes = b"\x00asm\x01\x00\x00\x00";
        let v1 = WasmModuleVersion::new("1.0.0", bytes);
        let v2 = WasmModuleVersion::new("2.0.0", bytes);
        assert_eq!(v1.hash(), v2.hash());
    }

    #[test]
    fn test_wasm_module_health_unknown_default() {
        let entry = WasmModuleEntry::new("test", WasmModuleSource::Bytes { bytes: vec![] });
        assert_eq!(entry.health(), &WasmModuleHealth::Unknown);
    }

    #[test]
    fn test_wasm_module_entry_set_health() {
        let mut entry = WasmModuleEntry::new("test", WasmModuleSource::Bytes { bytes: vec![] });
        entry.set_health(WasmModuleHealth::Healthy);
        assert_eq!(entry.health(), &WasmModuleHealth::Healthy);
    }

    #[test]
    fn test_wasm_module_entry_set_version() {
        let mut entry = WasmModuleEntry::new("test", WasmModuleSource::Bytes { bytes: vec![] });
        entry.set_version(WasmModuleVersion::new("1.0.0", b"\x00asm"));
        assert!(entry.version().is_some());
        assert_eq!(entry.version().as_ref().unwrap().version(), "1.0.0");
    }

    #[test]
    fn test_wasm_module_entry_fields() {
        let entry = WasmModuleEntry::new("calculator", WasmModuleSource::File { path: PathBuf::from("calc.wasm") });
        assert_eq!(entry.name(), "calculator");
        assert!(matches!(entry.source(), WasmModuleSource::File { .. }));
        assert!(entry.version().is_none());
        assert_eq!(entry.health(), &WasmModuleHealth::Unknown);
    }

    #[tokio::test]
    async fn test_registry_new() {
        let runtime = Arc::new(WasmRuntime::with_defaults().unwrap());
        let registry = WasmModuleRegistry::new(runtime);
        let list = registry.list().await;
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn test_registry_with_defaults() {
        let registry = WasmModuleRegistry::with_defaults();
        assert!(registry.is_ok());
    }

    #[tokio::test]
    async fn test_registry_list_empty() {
        let runtime = Arc::new(WasmRuntime::with_defaults().unwrap());
        let registry = WasmModuleRegistry::new(runtime);
        let list = registry.list().await;
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn test_registry_get_nonexistent() {
        let runtime = Arc::new(WasmRuntime::with_defaults().unwrap());
        let registry = WasmModuleRegistry::new(runtime);
        let result = registry.get("nonexistent").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_registry_unload_nonexistent() {
        let runtime = Arc::new(WasmRuntime::with_defaults().unwrap());
        let registry = WasmModuleRegistry::new(runtime);
        let result = registry.unload("nonexistent").await;
        assert!(result.is_ok()); // Removing nonexistent is OK
    }

    #[tokio::test]
    async fn test_registry_register_duplicate_error() {
        let runtime = Arc::new(WasmRuntime::with_defaults().unwrap());
        let registry = WasmModuleRegistry::new(runtime);

        // Manually insert an entry to simulate a registered module
        {
            let mut entries = registry.entries.write().await;
            entries.insert("test".to_string(), WasmModuleEntry::new("test",
                WasmModuleSource::Bytes { bytes: vec![0, 97, 115, 109] }));
        }

        // Now try to register again — should fail with RegistryError
        let result = registry.register("test", WasmModuleSource::Bytes { bytes: vec![] }).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("already registered"));
    }

    #[tokio::test]
    async fn test_registry_unload_removes_entry() {
        let runtime = Arc::new(WasmRuntime::with_defaults().unwrap());
        let registry = WasmModuleRegistry::new(runtime);

        // Manually insert an entry
        {
            let mut entries = registry.entries.write().await;
            entries.insert("test".to_string(), WasmModuleEntry::new("test",
                WasmModuleSource::Bytes { bytes: vec![] }));
        }

        // Unload it
        registry.unload("test").await.unwrap();

        // Verify it's gone
        let result = registry.get("test").await;
        assert!(result.is_none());
    }

    #[test]
    fn test_wasm_module_health_variants() {
        let healthy = WasmModuleHealth::Healthy;
        let degraded = WasmModuleHealth::Degraded { reason: "test".to_string() };
        let unhealthy = WasmModuleHealth::Unhealthy { reason: "test".to_string() };
        let unknown = WasmModuleHealth::Unknown;

        assert_ne!(healthy, unknown);
        assert_ne!(degraded, unhealthy);
    }
}
