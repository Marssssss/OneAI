//! MCP Server Config File — TOML-based configuration for MCP server plugins.
//!
//! Stores MCP server configurations in `~/.oneai/mcp_servers.toml`.
//! Format is designed to be human-readable and easy to edit manually.
//!
//! Example config:
//! ```toml
//! [servers.filesystem]
//! description = "MCP filesystem server"
//! transport = "stdio"
//! command = "npx"
//! args = ["-y", "@modelcontextprotocol/server-filesystem"]
//! enabled = false
//!
//! [servers.web_search]
//! description = "Anthropic MCP web search"
//! transport = "sse"
//! url = "http://localhost:8080/sse"
//! headers = { Authorization = "Bearer $ANTHROPIC_API_KEY" }
//! enabled = false
//! requires_api_key = true
//! api_key_env = "ANTHROPIC_API_KEY"
//! ```

use std::path::PathBuf;

use crate::plugin::McpPluginEntry;

/// MCP server configuration file.
///
/// Contains a list of MCP server entries that are loaded from
/// `~/.oneai/mcp_servers.toml`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct McpServerConfigFile {
    /// MCP server entries.
    #[serde(default)]
    pub servers: Vec<McpPluginEntry>,
}

impl McpServerConfigFile {
    /// Load from the default path: `~/.oneai/mcp_servers.toml`
    pub fn load_default() -> Result<Self, ConfigLoadError> {
        let path = Self::default_path();
        if path.exists() {
            Self::load_from(&path)
        } else {
            Ok(Self::default())
        }
    }

    /// Load from a specific path.
    pub fn load_from(path: &PathBuf) -> Result<Self, ConfigLoadError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| ConfigLoadError::ReadFailed(path.clone(), e.to_string()))?;

        // Try TOML first (primary format)
        let config: Result<Self, toml::de::Error> = toml::from_str(&content);
        match config {
            Ok(c) => Ok(c),
            Err(toml_err) => {
                // Try JSON as fallback
                let json_config: Result<Self, serde_json::Error> = serde_json::from_str(&content);
                match json_config {
                    Ok(c) => Ok(c),
                    Err(_) => Err(ConfigLoadError::ParseFailed(
                        path.clone(),
                        toml_err.to_string(),
                    )),
                }
            }
        }
    }

    /// Save to the default path.
    pub fn save_default(&self) -> Result<PathBuf, ConfigSaveError> {
        let path = Self::default_path();
        self.save_to(&path)?;
        Ok(path)
    }

    /// Save to a specific path.
    pub fn save_to(&self, path: &PathBuf) -> Result<(), ConfigSaveError> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ConfigSaveError::WriteFailed(path.clone(), e.to_string()))?;
        }

        // Determine format from extension
        let content = if path.extension().and_then(|e| e.to_str()) == Some("json") {
            serde_json::to_string_pretty(self)
                .map_err(|e| ConfigSaveError::SerializeFailed(e.to_string()))?
        } else {
            toml::to_string_pretty(self)
                .map_err(|e| ConfigSaveError::SerializeFailed(e.to_string()))?
        };

        std::fs::write(path, content)
            .map_err(|e| ConfigSaveError::WriteFailed(path.clone(), e.to_string()))?;

        Ok(())
    }

    /// Get the default config file path.
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".oneai")
            .join("mcp_servers.toml")
    }

    /// Create default config with builtin entries.
    pub fn default_config() -> Self {
        Self {
            servers: vec![
                McpPluginEntry {
                    name: "filesystem".to_string(),
                    description: "MCP filesystem server — read and write files".to_string(),
                    source: crate::plugin::McpPluginSource::Stdio {
                        command: "npx".to_string(),
                        args: vec!["-y".to_string(), "@modelcontextprotocol/server-filesystem".to_string()],
                        env: std::collections::HashMap::new(),
                    },
                    enabled: false,
                    requires_api_key: false,
                    api_key_env: None,
                    tags: vec!["filesystem".to_string(), "files".to_string()],
                },
                McpPluginEntry {
                    name: "web_search".to_string(),
                    description: "Anthropic MCP web search — search the web via API".to_string(),
                    source: crate::plugin::McpPluginSource::Stdio {
                        command: "npx".to_string(),
                        args: vec!["-y".to_string(), "@anthropic-ai/mcp-web-search".to_string()],
                        env: std::collections::HashMap::new(),
                    },
                    enabled: false,
                    requires_api_key: true,
                    api_key_env: Some("ANTHROPIC_API_KEY".to_string()),
                    tags: vec!["web".to_string(), "search".to_string()],
                },
            ],
        }
    }
}

impl Default for McpServerConfigFile {
    fn default() -> Self {
        Self { servers: Vec::new() }
    }
}

// ─── Error types ────────────────────────────────────────────────────────────

/// Error during config file loading.
#[derive(Debug)]
#[non_exhaustive]
pub enum ConfigLoadError {
    /// Failed to read the file.
    ReadFailed(PathBuf, String),
    /// Failed to parse the file (TOML or JSON).
    ParseFailed(PathBuf, String),
}

impl std::fmt::Display for ConfigLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadFailed(path, msg) => write!(f, "Failed to read {}: {}", path.display(), msg),
            Self::ParseFailed(path, msg) => write!(f, "Failed to parse {}: {}", path.display(), msg),
        }
    }
}

impl std::error::Error for ConfigLoadError {}

/// Error during config file saving.
#[derive(Debug)]
#[non_exhaustive]
pub enum ConfigSaveError {
    /// Failed to serialize the config.
    SerializeFailed(String),
    /// Failed to write the file.
    WriteFailed(PathBuf, String),
}

impl std::fmt::Display for ConfigSaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SerializeFailed(msg) => write!(f, "Failed to serialize config: {}", msg),
            Self::WriteFailed(path, msg) => write!(f, "Failed to write {}: {}", path.display(), msg),
        }
    }
}

impl std::error::Error for ConfigSaveError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::McpPluginSource;
    use std::collections::HashMap;

    #[test]
    fn test_config_file_default() {
        let config = McpServerConfigFile::default();
        assert!(config.servers.is_empty());
    }

    #[test]
    fn test_config_file_default_config() {
        let config = McpServerConfigFile::default_config();
        assert_eq!(config.servers.len(), 2);
        assert_eq!(config.servers[0].name, "filesystem");
        assert_eq!(config.servers[1].name, "web_search");
    }

    #[test]
    fn test_config_file_save_and_load_toml() {
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let path = tmp_dir.path().join("mcp_servers.toml");

        let config = McpServerConfigFile {
            servers: vec![
                McpPluginEntry {
                    name: "filesystem".to_string(),
                    description: "FS server".to_string(),
                    source: McpPluginSource::Stdio {
                        command: "npx".to_string(),
                        args: vec!["-y".to_string(), "@mcp/fs".to_string()],
                        env: HashMap::new(),
                    },
                    enabled: true,
                    requires_api_key: false,
                    api_key_env: None,
                    tags: vec!["filesystem".to_string()],
                },
            ],
        };

        config.save_to(&path).unwrap();
        let loaded = McpServerConfigFile::load_from(&path).unwrap();
        assert_eq!(loaded.servers.len(), 1);
        assert_eq!(loaded.servers[0].name, "filesystem");
        assert!(matches!(loaded.servers[0].source, McpPluginSource::Stdio { .. }));
    }

    #[test]
    fn test_config_file_save_and_load_json() {
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let path = tmp_dir.path().join("mcp_servers.json");

        let config = McpServerConfigFile {
            servers: vec![
                McpPluginEntry {
                    name: "web_search".to_string(),
                    description: "Web search".to_string(),
                    source: McpPluginSource::Sse {
                        url: "http://localhost:8080/sse".to_string(),
                        headers: HashMap::new(),
                    },
                    enabled: false,
                    requires_api_key: true,
                    api_key_env: Some("API_KEY".to_string()),
                    tags: vec!["search".to_string()],
                },
            ],
        };

        config.save_to(&path).unwrap();
        let loaded = McpServerConfigFile::load_from(&path).unwrap();
        assert_eq!(loaded.servers.len(), 1);
        assert_eq!(loaded.servers[0].name, "web_search");
        assert!(matches!(loaded.servers[0].source, McpPluginSource::Sse { .. }));
    }

    #[test]
    fn test_config_file_load_missing() {
        let path = PathBuf::from("/tmp/nonexistent_mcp_config.toml");
        let result = McpServerConfigFile::load_from(&path);
        assert!(result.is_err());
    }

    #[test]
    fn test_config_load_error_display() {
        let err = ConfigLoadError::ReadFailed(
            PathBuf::from("/tmp/test.toml"),
            "permission denied".to_string(),
        );
        assert!(err.to_string().contains("permission denied"));
    }

    #[test]
    fn test_config_save_error_display() {
        let err = ConfigSaveError::SerializeFailed("bad data".to_string());
        assert!(err.to_string().contains("bad data"));
    }
}
