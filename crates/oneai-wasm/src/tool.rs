//! WasmTool — Tool trait implementation wrapping WASM modules.
//!
//! WasmTool makes WASM modules indistinguishable from native tools in the
//! OneAI framework. The AgentLoop calls WasmTool.execute() through the Tool
//! trait, and the execution happens inside the WASM sandbox.

use std::sync::Arc;

use async_trait::async_trait;
use oneai_core::{RiskLevel, ToolOutput};
use oneai_core::error::Result;
use oneai_core::traits::Tool;

use crate::error::WasmError;
use crate::runtime::{WasmRuntime, WasmStoreState};

/// Pre-extracted metadata for a WASM tool.
#[derive(Debug, Clone)]
pub struct WasmToolMetadata {
    /// The tool's unique name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema for tool parameters.
    pub parameters_schema: serde_json::Value,
    /// Risk level — WASM tools default to Low (sandboxed).
    pub risk_level: RiskLevel,
}

/// WASM tool — wraps a WASM module as an OneAI Tool.
///
/// WASM tools always execute inside the WASM sandbox with zero I/O access.
/// Because of this sandboxing, they are always Low-risk by default.
pub struct WasmTool {
    /// Module name in the runtime's module cache.
    module_name: String,
    /// Pre-extracted tool metadata.
    metadata: WasmToolMetadata,
    /// Reference to the WASM runtime.
    runtime: Arc<WasmRuntime>,
}

impl std::fmt::Debug for WasmTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmTool")
            .field("module_name", &self.module_name)
            .field("metadata", &self.metadata)
            .finish()
    }
}

impl WasmTool {
    /// Create a new WasmTool.
    pub fn new(module_name: &str, metadata: WasmToolMetadata, runtime: Arc<WasmRuntime>) -> Self {
        Self {
            module_name: module_name.to_string(),
            metadata,
            runtime,
        }
    }

    /// Get the module name.
    pub fn module_name(&self) -> &str {
        &self.module_name
    }

    /// Get the tool metadata.
    pub fn metadata(&self) -> &WasmToolMetadata {
        &self.metadata
    }

    /// Parse the WASM tool's output string into a ToolOutput.
    fn parse_tool_output(output_str: &str) -> std::result::Result<ToolOutput, WasmError> {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(output_str) {
            let success = parsed.get("success")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let content = parsed.get("content")
                .and_then(|v| v.as_str())
                .unwrap_or(output_str)
                .to_string();
            let error = parsed.get("error")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Ok(ToolOutput { success, content, error })
        } else {
            Ok(ToolOutput {
                success: true,
                content: output_str.to_string(),
                error: None,
            })
        }
    }
}

#[async_trait]
impl Tool for WasmTool {
    fn name(&self) -> &str {
        &self.metadata.name
    }

    fn description(&self) -> &str {
        &self.metadata.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.metadata.parameters_schema.clone()
    }

    fn risk_level(&self) -> RiskLevel {
        self.metadata.risk_level
    }

    /// Execute the WASM tool in a sandboxed environment.
    ///
    /// Synchronous WASM execution is wrapped in spawn_blocking
    /// to avoid blocking the tokio runtime.
    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let module_name = self.module_name.clone();
        let runtime = self.runtime.clone();
        let fuel_limit = runtime.config().fuel_limit;

        let result = tokio::task::spawn_blocking(move || {
            // Get the cached module
            let module = {
                let cache = &runtime.module_cache;
                let guard = cache.try_read();
                match guard {
                    Ok(g) => g.get(&module_name).cloned()
                        .ok_or_else(|| WasmError::ModuleNotFound(module_name.clone()))?,
                    Err(_) => return Err(WasmError::ModuleNotFound(format!(
                        "{} (cache lock contested)", module_name
                    ))),
                }
            };

            // Create a new Store for this execution
            let mut store = runtime.create_store();

            // Create Linker
            let linker = runtime.create_linker()?;

            // Instantiate
            let instance = linker.instantiate(&mut store, &module)
                .map_err(|e| WasmError::InstantiationFailed(format!(
                    "Failed to instantiate '{}': {}", module_name, e
                )))?;

            // Get memory and execute function exports
            let memory = instance.get_memory(&mut store, "memory")
                .ok_or_else(|| WasmError::ExportNotFound("memory".to_string()))?;

            let execute_func = instance.get_typed_func::<(u32, u32), (u32, u32)>(&mut store, "execute")
                .map_err(|e| WasmError::ExportNotFound(format!("execute: {}", e)))?;

            // Serialize args
            let input_json = serde_json::to_string(&args)
                .map_err(|e| WasmError::JsonError(format!("serialize: {}", e)))?;
            let input_bytes = input_json.as_bytes();

            // Ensure memory is large enough for input
            let current_size = memory.data(&store).len();
            if current_size < input_bytes.len() + 16 {
                let extra_pages: u64 = ((input_bytes.len() + 16) as u64) / (64 * 1024) + 1;
                memory.grow(&mut store, extra_pages)
                    .map_err(|_| WasmError::MemoryExceeded(extra_pages as u32, runtime.config().max_memory_pages))?;
            }

            // Write input at a fixed offset (8 bytes from start)
            let input_offset = 8u32;
            {
                let data_mut = memory.data_mut(&mut store);
                let start = input_offset as usize;
                let end = start + input_bytes.len();
                if end <= data_mut.len() {
                    data_mut[start..end].copy_from_slice(input_bytes);
                } else {
                    return Err(WasmError::MemoryExceeded(
                        (input_bytes.len() as u32) / (64 * 1024) + 1,
                        runtime.config().max_memory_pages,
                    ));
                }
            }

            // Call execute
            let (output_ptr, output_len) = execute_func.call(&mut store, (input_offset, input_bytes.len() as u32))
                .map_err(|e| {
                    // Check if fuel was exhausted
                    let fuel = store.get_fuel().ok();
                    if fuel.map_or(false, |f| f == 0) {
                        WasmError::FuelExceeded(fuel_limit.unwrap_or(0))
                    } else {
                        WasmError::CallFailed(format!("execute failed: {}", e))
                    }
                })?;

            // Read output from memory
            let output_data = memory.data(&store);
            let start = output_ptr as usize;
            let end = start + output_len as usize;
            if end > output_data.len() {
                return Err(WasmError::InvalidMetadata(format!(
                    "Invalid output range ({}, {})", output_ptr, output_len
                )));
            }
            let output_bytes = output_data[start..end].to_vec();
            let output_str = String::from_utf8_lossy(&output_bytes).into_owned();

            Self::parse_tool_output(&output_str)
        }).await;

        match result {
            Ok(inner) => inner.map_err(|e| oneai_core::error::OneAIError::Wasm(e.to_string())),
            Err(e) => Err(oneai_core::error::OneAIError::Wasm(format!("spawn_blocking failed: {}", e))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wasm_tool_metadata_creation() {
        let metadata = WasmToolMetadata {
            name: "calculator".to_string(),
            description: "A WASM calculator".to_string(),
            parameters_schema: serde_json::json!({"type": "object"}),
            risk_level: RiskLevel::Low,
        };
        assert_eq!(metadata.name, "calculator");
        assert_eq!(metadata.risk_level, RiskLevel::Low);
    }

    #[test]
    fn test_wasm_tool_creation() {
        let runtime = Arc::new(WasmRuntime::with_defaults().unwrap());
        let metadata = WasmToolMetadata {
            name: "test".to_string(),
            description: "test".to_string(),
            parameters_schema: serde_json::json!({"type": "object"}),
            risk_level: RiskLevel::Low,
        };
        let tool = WasmTool::new("test", metadata, runtime);
        assert_eq!(tool.name(), "test");
        assert_eq!(tool.risk_level(), RiskLevel::Low);
    }

    #[test]
    fn test_parse_tool_output_json() {
        let output = WasmTool::parse_tool_output("{\"success\":true,\"content\":\"5\",\"error\":null}");
        assert!(output.is_ok());
        let result = output.unwrap();
        assert!(result.success);
        assert_eq!(result.content, "5");
        assert!(result.error.is_none());
    }

    #[test]
    fn test_parse_tool_output_plain_text() {
        let output = WasmTool::parse_tool_output("just text");
        assert!(output.is_ok());
        let result = output.unwrap();
        assert!(result.success);
        assert_eq!(result.content, "just text");
    }

    #[test]
    fn test_parse_tool_output_error() {
        let output = WasmTool::parse_tool_output("{\"success\":false,\"content\":\"\",\"error\":\"division by zero\"}");
        assert!(output.is_ok());
        let result = output.unwrap();
        assert!(!result.success);
        assert_eq!(result.error, Some("division by zero".to_string()));
    }
}
