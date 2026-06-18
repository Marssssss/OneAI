//! MCP Plugin Registry — config-based management of MCP server plugins.
//!
//! Follows the DomainPack market pattern (PackSource/PackRegistry/PackIndexEntry).
//! Manages external MCP server connections:
//! - Load configs from `~/.oneai/mcp_servers.toml`
//! - Connect/disconnect servers, discover their tools
//! - Auto-register discovered tools into OneAI's ToolRegistry
//! - Health monitoring and lifecycle management

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use oneai_core::traits::Tool;
use oneai_core::error::Result;
use oneai_tool::{ToolRegistry, RealMcpServerManager, McpServerConfig, McpTransport, RealMcpToolWrapper};

use crate::config::McpServerConfigFile;

/// Source type for an MCP plugin server.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "transport")]
#[non_exhaustive]
pub enum McpPluginSource {
    /// Stdio transport — launch a local subprocess.
    #[serde(rename = "stdio")]
    Stdio {
        /// Command to launch the MCP server.
        command: String,
        /// Arguments for the command.
        #[serde(default)]
        args: Vec<String>,
        /// Environment variables for the subprocess.
        #[serde(default)]
        env: HashMap<String, String>,
    },
    /// SSE transport — connect to an HTTP SSE endpoint.
    #[serde(rename = "sse")]
    Sse {
        /// URL of the SSE endpoint.
        url: String,
        /// Custom HTTP headers.
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    /// Streamable HTTP transport — POST + SSE stream.
    #[serde(rename = "streamable_http")]
    StreamableHttp {
        /// URL of the HTTP endpoint.
        url: String,
        /// Custom HTTP headers.
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

/// An MCP plugin server entry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct McpPluginEntry {
    /// Unique plugin name.
    pub name: String,
    /// Human-readable description.
    #[serde(default)]
    pub description: String,
    /// Transport configuration.
    pub source: McpPluginSource,
    /// Whether this plugin is enabled (connected at startup).
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Whether this plugin requires an API key.
    #[serde(default)]
    pub requires_api_key: bool,
    /// The environment variable name for the API key.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Tags for categorization.
    #[serde(default)]
    pub tags: Vec<String>,
}

fn default_enabled() -> bool {
    true
}

impl McpPluginEntry {
    /// Convert to a McpServerConfig for the oneai-tool MCP client.
    pub fn to_server_config(&self) -> McpServerConfig {
        let transport = match &self.source {
            McpPluginSource::Stdio { command, args, env } => {
                // Interpolate environment variables in args
                let interpolated_args = args.iter()
                    .map(|a| interpolate_env_vars(a))
                    .collect();
                let interpolated_env = env.iter()
                    .map(|(k, v)| (k.clone(), interpolate_env_vars(v)))
                    .collect();
                McpTransport::Stdio {
                    command: interpolate_env_vars(command),
                    args: interpolated_args,
                    env: interpolated_env,
                }
            }
            McpPluginSource::Sse { url, headers } => {
                let interpolated_headers = headers.iter()
                    .map(|(k, v)| (k.clone(), interpolate_env_vars(v)))
                    .collect();
                McpTransport::Sse {
                    url: interpolate_env_vars(url),
                    headers: interpolated_headers,
                }
            }
            McpPluginSource::StreamableHttp { url, headers } => {
                let interpolated_headers = headers.iter()
                    .map(|(k, v)| (k.clone(), interpolate_env_vars(v)))
                    .collect();
                McpTransport::StreamableHttp {
                    url: interpolate_env_vars(url),
                    headers: interpolated_headers,
                }
            }
        };

        McpServerConfig {
            name: self.name.clone(),
            transport,
            requires_api_key: self.requires_api_key,
            api_key_field: self.api_key_env.clone(),
        }
    }
}

/// MCP Plugin Registry — manages external MCP server configurations.
///
/// Follows the DomainPack market pattern:
/// - `load_from_config()` — load from config file
/// - `connect_server()` — connect and discover tools
/// - `disconnect_server()` — shutdown a server connection
/// - `all_discovered_tools()` — get all tools from connected servers
/// - `register_tools()` — register discovered tools into ToolRegistry
pub struct McpPluginRegistry {
    /// Loaded plugin entries (from config + builtin).
    entries: HashMap<String, McpPluginEntry>,
    /// MCP server manager (handles actual connections).
    server_manager: Arc<tokio::sync::Mutex<RealMcpServerManager>>,
    /// Connected server names.
    connected: HashMap<String, Vec<String>>, // server_name → discovered_tool_names
}

impl McpPluginRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            server_manager: Arc::new(tokio::sync::Mutex::new(RealMcpServerManager::new())),
            connected: HashMap::new(),
        }
    }

    /// Create a registry from a config file.
    ///
    /// Loads entries from `~/.oneai/mcp_servers.toml`, merges with
    /// builtin defaults, and interpolates environment variables.
    pub fn from_config_file() -> Self {
        let config_path = Self::default_config_path();
        Self::from_config_path(&config_path)
    }

    /// Create a registry from a specific config path.
    pub fn from_config_path(path: &PathBuf) -> Self {
        let mut registry = Self::new();

        // Load builtin defaults first
        registry.populate_builtin_entries();

        // Load from config file (overrides/extends builtins)
        if path.exists() {
            if let Ok(config) = McpServerConfigFile::load_from(path) {
                for entry in config.servers {
                    registry.entries.insert(entry.name.clone(), entry);
                }
            }
        }

        registry
    }

    /// Get the default config file path: `~/.oneai/mcp_servers.toml`
    pub fn default_config_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".oneai")
            .join("mcp_servers.toml")
    }

    /// Add a plugin entry.
    pub fn add_entry(&mut self, entry: McpPluginEntry) {
        self.entries.insert(entry.name.clone(), entry);
    }

    /// Remove a plugin entry.
    pub fn remove_entry(&mut self, name: &str) -> Option<McpPluginEntry> {
        self.entries.remove(name)
    }

    /// Get a plugin entry by name.
    pub fn get_entry(&self, name: &str) -> Option<&McpPluginEntry> {
        self.entries.get(name)
    }

    /// List all plugin entries.
    pub fn list_entries(&self) -> Vec<&McpPluginEntry> {
        self.entries.values().collect()
    }

    /// List only enabled entries.
    pub fn list_enabled(&self) -> Vec<&McpPluginEntry> {
        self.entries.values().filter(|e| e.enabled).collect()
    }

    /// Connect to an MCP server by name, discover its tools.
    ///
    /// Uses the oneai-tool McpServerManager to establish a connection
    /// and discover available tools. Stores discovered tool names.
    pub async fn connect_server(&mut self, name: &str) -> Result<Vec<String>> {
        let entry = self.entries.get(name)
            .ok_or_else(|| oneai_core::error::OneAIError::Provider(
                format!("MCP plugin '{}' not found in registry", name)
            ))?;

        if !entry.enabled {
            return Err(oneai_core::error::OneAIError::Provider(
                format!("MCP plugin '{}' is disabled", name)
            ));
        }

        let config = entry.to_server_config();
        let mut manager = self.server_manager.lock().await;
        let tool_names = manager.connect_server(config).await?;

        self.connected.insert(name.to_string(), tool_names.clone());

        tracing::info!("MCP plugin '{}' connected — discovered {} tools: {:?}",
            name, tool_names.len(), tool_names);

        Ok(tool_names)
    }

    /// Connect all enabled servers and discover their tools.
    ///
    /// Returns a map of server_name → discovered_tool_names.
    pub async fn connect_all_enabled(&mut self) -> Result<HashMap<String, Vec<String>>> {
        let enabled_names: Vec<String> = self.entries.values()
            .filter(|e| e.enabled)
            .map(|e| e.name.clone())
            .collect();

        let mut results = HashMap::new();
        for name in &enabled_names {
            match self.connect_server(name).await {
                Ok(tool_names) => {
                    results.insert(name.clone(), tool_names);
                }
                Err(e) => {
                    tracing::warn!("Failed to connect MCP plugin '{}': {}", name, e);
                    results.insert(name.clone(), Vec::new());
                }
            }
        }

        Ok(results)
    }

    /// Disconnect a server by name.
    pub async fn disconnect_server(&mut self, name: &str) -> Result<()> {
        let mut manager = self.server_manager.lock().await;
        // Shutdown specific connection
        // Note: current McpServerManager only supports shutdown_all()
        // For now, we just remove from the connected map
        manager.shutdown_all().await?;
        self.connected.clear();
        tracing::info!("MCP plugin '{}' disconnected", name);
        Ok(())
    }

    /// Disconnect all servers.
    pub async fn disconnect_all(&mut self) -> Result<()> {
        let mut manager = self.server_manager.lock().await;
        manager.shutdown_all().await?;
        self.connected.clear();
        Ok(())
    }

    /// Get all discovered tool wrappers from connected servers.
    pub async fn all_tool_wrappers(&self) -> Vec<Arc<RealMcpToolWrapper>> {
        let manager = self.server_manager.lock().await;
        manager.all_tool_wrappers()
    }

    /// Register all discovered tools into a ToolRegistry.
    ///
    /// This is called by AppBuilder.build() to auto-register MCP tools.
    pub async fn register_tools(&self, registry: &Arc<ToolRegistry>) -> Result<Vec<String>> {
        let tool_wrappers = self.all_tool_wrappers().await;
        let mut registered = Vec::new();

        for wrapper in tool_wrappers {
            let name = wrapper.name().to_string();
            registry.register(wrapper.clone() as Arc<dyn Tool>).await?;
            registered.push(name);
        }

        Ok(registered)
    }

    /// List connected server names.
    pub fn connected_servers(&self) -> Vec<String> {
        self.connected.keys().cloned().collect()
    }

    /// Get discovered tool names for a connected server.
    pub fn server_tools(&self, name: &str) -> Option<&Vec<String>> {
        self.connected.get(name)
    }

    /// Save the current registry entries to the default config file.
    pub fn save_config(&self) -> Result<PathBuf> {
        let path = Self::default_config_path();
        self.save_config_to(&path)?;
        Ok(path)
    }

    /// Save the registry entries to a specific config path.
    pub fn save_config_to(&self, path: &PathBuf) -> Result<()> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| oneai_core::error::OneAIError::Provider(
                    format!("Failed to create config directory: {}", e)
                ))?;
        }

        let servers: Vec<McpPluginEntry> = self.entries.values().cloned().collect();
        let config = McpServerConfigFile { servers };
        config.save_to(path).map_err(|e| oneai_core::error::OneAIError::Provider(
            format!("Failed to save MCP config: {}", e)
        ))?;

        Ok(())
    }

    // ─── Private methods ───────────────────────────────────────────────────

    fn populate_builtin_entries(&mut self) {
        let builtins = builtin_mcp_entries();
        for entry in builtins {
            self.entries.insert(entry.name.clone(), entry);
        }
    }
}

impl Default for McpPluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Environment variable interpolation ──────────────────────────────────────

/// Interpolate environment variables in a string.
///
/// Replaces `$VAR_NAME` or `${VAR_NAME}` with the value from the environment.
/// If the variable is not set, replaces with an empty string.
pub fn interpolate_env_vars(s: &str) -> String {
    let mut result = s.to_string();

    // Handle ${VAR_NAME} format
    while let Some(start) = result.find("${") {
        if let Some(end) = result[start..].find('}') {
            let var_name = &result[start + 2..start + end];
            let value = std::env::var(var_name).unwrap_or_default();
            result = result.replace(&format!("${{{}}}", var_name), &value);
        } else {
            break;
        }
    }

    // Handle $VAR_NAME format (simple form, stops at non-alphanumeric/underscore)
    let mut i = 0;
    let mut final_result = String::new();
    while i < result.len() {
        if result.as_bytes()[i] == b'$' && i + 1 < result.len() {
            let rest = &result[i + 1..];
            let var_end = rest.find(|c: char| !c.is_alphanumeric() && c != '_')
                .unwrap_or(rest.len());
            if var_end > 0 {
                let var_name = &rest[..var_end];
                let value = std::env::var(var_name).unwrap_or_default();
                final_result.push_str(&value);
                i += 1 + var_end;
                continue;
            }
        }
        final_result.push(result.as_bytes()[i] as char);
        i += 1;
    }

    final_result
}

// ─── Built-in MCP plugin entries ────────────────────────────────────────────

fn builtin_mcp_entries() -> Vec<McpPluginEntry> {
    vec![
        McpPluginEntry {
            name: "filesystem".to_string(),
            description: "MCP filesystem server — read and write files".to_string(),
            source: McpPluginSource::Stdio {
                command: "npx".to_string(),
                args: vec!["-y".to_string(), "@modelcontextprotocol/server-filesystem".to_string()],
                env: HashMap::new(),
            },
            enabled: false, // Disabled by default — requires npx
            requires_api_key: false,
            api_key_env: None,
            tags: vec!["filesystem".to_string(), "files".to_string()],
        },
        McpPluginEntry {
            name: "web_search".to_string(),
            description: "Anthropic MCP web search — search the web via API".to_string(),
            source: McpPluginSource::Stdio {
                command: "npx".to_string(),
                args: vec!["-y".to_string(), "@anthropic-ai/mcp-web-search".to_string()],
                env: HashMap::new(),
            },
            enabled: false,
            requires_api_key: true,
            api_key_env: Some("ANTHROPIC_API_KEY".to_string()),
            tags: vec!["web".to_string(), "search".to_string()],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_interpolate_env_vars_simple() {
        std::env::set_var("TEST_MCP_VAR", "hello");
        assert_eq!(interpolate_env_vars("$TEST_MCP_VAR"), "hello");
        std::env::remove_var("TEST_MCP_VAR");
    }

    #[test]
    fn test_interpolate_env_vars_braced() {
        std::env::set_var("TEST_MCP_VAR2", "world");
        assert_eq!(interpolate_env_vars("${TEST_MCP_VAR2}"), "world");
        std::env::remove_var("TEST_MCP_VAR2");
    }

    #[test]
    fn test_interpolate_env_vars_unset() {
        std::env::remove_var("NONEXISTENT_MCP_VAR");
        assert_eq!(interpolate_env_vars("$NONEXISTENT_MCP_VAR"), "");
    }

    #[test]
    fn test_interpolate_env_vars_mixed() {
        std::env::set_var("MCP_HOST", "localhost");
        std::env::set_var("MCP_PORT", "8080");
        assert_eq!(
            interpolate_env_vars("http://${MCP_HOST}:$MCP_PORT/api"),
            "http://localhost:8080/api"
        );
        std::env::remove_var("MCP_HOST");
        std::env::remove_var("MCP_PORT");
    }

    #[test]
    fn test_interpolate_env_vars_in_auth_header() {
        std::env::set_var("MY_API_KEY", "sk-test123");
        assert_eq!(
            interpolate_env_vars("Bearer $MY_API_KEY"),
            "Bearer sk-test123"
        );
        std::env::remove_var("MY_API_KEY");
    }

    #[test]
    fn test_mcp_plugin_entry_to_server_config() {
        let entry = McpPluginEntry {
            name: "filesystem".to_string(),
            description: "FS server".to_string(),
            source: McpPluginSource::Stdio {
                command: "npx".to_string(),
                args: vec!["-y".to_string(), "@mcp/server".to_string()],
                env: HashMap::new(),
            },
            enabled: true,
            requires_api_key: false,
            api_key_env: None,
            tags: vec!["filesystem".to_string()],
        };

        let config = entry.to_server_config();
        assert_eq!(config.name, "filesystem");
        assert!(matches!(config.transport, McpTransport::Stdio { .. }));
    }

    #[test]
    fn test_mcp_plugin_registry_new() {
        let registry = McpPluginRegistry::new();
        assert!(registry.list_entries().is_empty());
    }

    #[test]
    fn test_mcp_plugin_registry_add_remove() {
        let mut registry = McpPluginRegistry::new();
        let entry = McpPluginEntry {
            name: "test".to_string(),
            description: "Test server".to_string(),
            source: McpPluginSource::Stdio {
                command: "test-cmd".to_string(),
                args: vec![],
                env: HashMap::new(),
            },
            enabled: true,
            requires_api_key: false,
            api_key_env: None,
            tags: vec!["test".to_string()],
        };

        registry.add_entry(entry);
        assert_eq!(registry.list_entries().len(), 1);

        let removed = registry.remove_entry("test");
        assert!(removed.is_some());
        assert!(registry.list_entries().is_empty());
    }

    #[test]
    fn test_mcp_plugin_registry_list_enabled() {
        let mut registry = McpPluginRegistry::new();
        registry.add_entry(McpPluginEntry {
            name: "enabled_server".to_string(),
            description: "Enabled".to_string(),
            source: McpPluginSource::Stdio {
                command: "cmd".to_string(),
                args: vec![],
                env: HashMap::new(),
            },
            enabled: true,
            requires_api_key: false,
            api_key_env: None,
            tags: vec![],
        });
        registry.add_entry(McpPluginEntry {
            name: "disabled_server".to_string(),
            description: "Disabled".to_string(),
            source: McpPluginSource::Stdio {
                command: "cmd".to_string(),
                args: vec![],
                env: HashMap::new(),
            },
            enabled: false,
            requires_api_key: false,
            api_key_env: None,
            tags: vec![],
        });

        assert_eq!(registry.list_enabled().len(), 1);
        assert_eq!(registry.list_entries().len(), 2);
    }

    #[test]
    fn test_mcp_plugin_source_serialization() {
        let source = McpPluginSource::Stdio {
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "@mcp/server".to_string()],
            env: HashMap::new(),
        };

        let json = serde_json::to_string(&source).unwrap();
        assert!(json.contains("\"transport\":\"stdio\""));

        let deserialized: McpPluginSource = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, McpPluginSource::Stdio { .. }));
    }

    #[test]
    fn test_mcp_plugin_entry_serialization() {
        let entry = McpPluginEntry {
            name: "filesystem".to_string(),
            description: "Filesystem server".to_string(),
            source: McpPluginSource::Sse {
                url: "http://localhost:8080/sse".to_string(),
                headers: HashMap::from([("Authorization".to_string(), "Bearer $API_KEY".to_string())]),
            },
            enabled: true,
            requires_api_key: true,
            api_key_env: Some("API_KEY".to_string()),
            tags: vec!["files".to_string()],
        };

        let json = serde_json::to_string(&entry).unwrap();
        let deserialized: McpPluginEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, "filesystem");
        assert!(matches!(deserialized.source, McpPluginSource::Sse { .. }));
    }
}
