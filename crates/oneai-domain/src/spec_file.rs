//! DomainPack SpecFile — standalone TOML/JSON/YAML config with validate→build pipeline.
//!
//! The SpecFile provides a convenient pipeline for loading, validating, and
//! building DomainPacks from configuration files:
//!
//! ```text
//! load(path) → DomainPackSpecFile → validate() → ValidationResult → build(project_dir) → DomainPack
//! ```
//!
//! This is the recommended entry point for DomainPack file processing.
//! It validates the config before attempting resolution, catching errors
//! early rather than silently skipping unknown tools/sources.
//!
//! **Usage**:
//! ```ignore
//! let spec_file = DomainPackSpecFile::load("ONEAI.domain.yaml")?;
//! let result = spec_file.validate();
//! if result.is_valid() {
//!     let pack = spec_file.build("/project")?;
//! } else {
//!     for issue in result.issues() {
//!         println!("{}", issue);
//!     }
//! }
//! ```

use std::path::Path;

use crate::config_parser::{DomainPackConfig, parse_yaml, parse_toml, resolve_config};
use crate::validator::{DomainPackValidator, ValidationResult};
use crate::domain_pack::DomainPack;

// ─── DomainPackSpecFile ──────────────────────────────────────────────────────────

/// A DomainPack configuration file with a validate→build pipeline.
///
/// Wraps a `DomainPackConfig` (from `config_parser.rs`) and provides:
/// - **load**: Read and parse from YAML/TOML/JSON file
/// - **validate**: Structural + semantic validation (via DomainPackValidator)
/// - **build**: Resolve into a DomainPack (only valid configs should be built)
///
/// The SpecFile is designed for sharing across language boundaries —
/// the underlying `DomainPackConfig` uses string references (not Arc<dyn Trait>)
/// so it can be serialized/deserialized in any language.
#[derive(Debug)]
pub struct DomainPackSpecFile {
    /// The parsed configuration.
    pub config: DomainPackConfig,
    /// The file path (if loaded from a file).
    pub source_path: Option<String>,
}

impl DomainPackSpecFile {
    /// Load a DomainPackSpecFile from a YAML or TOML file.
    ///
    /// Auto-detects the format from the file extension:
    /// - `.yaml` / `.yml` → YAML format
    /// - `.toml` → TOML format
    ///
    /// Returns an error if the file cannot be read or parsed.
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let extension = path.extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");

        let config = match extension {
            "yaml" | "yml" => parse_yaml(path)?,
            "toml" => parse_toml(path)?,
            other => return Err(format!(
                "Unknown domain spec file extension '{}' — expected .yaml, .yml, or .toml",
                other
            ).into()),
        };

        Ok(Self {
            config,
            source_path: Some(path.to_string_lossy().to_string()),
        })
    }

    /// Create a SpecFile from an existing DomainPackConfig.
    ///
    /// Useful when the config is created programmatically rather than
    /// loaded from a file.
    pub fn from_config(config: DomainPackConfig) -> Self {
        Self {
            config,
            source_path: None,
        }
    }

    /// Validate the configuration — structural + semantic checks.
    ///
    /// Returns a `ValidationResult` with all issues found.
    /// `is_valid()` is true if zero Error-level issues.
    pub fn validate(&self) -> ValidationResult {
        DomainPackValidator::validate(&self.config)
    }

    /// Build a DomainPack from the configuration.
    ///
    /// **Warning**: This method does not check validity first. It's the caller's
    /// responsibility to call `validate()` and decide whether to proceed.
    /// Invalid configs will still be resolved, but unknown tools/sources
    /// will be silently skipped.
    pub fn build(&self, project_dir: &str) -> DomainPack {
        resolve_config(&self.config, project_dir)
    }

    /// Validate and build — only builds if the config is valid.
    ///
    /// Returns `Ok(DomainPack)` if valid, `Err(ValidationResult)` if invalid.
    /// The `ValidationResult` in the error contains all issues found.
    pub fn validate_and_build(&self, project_dir: &str) -> Result<DomainPack, ValidationResult> {
        let result = self.validate();
        if result.is_valid() {
            Ok(self.build(project_dir))
        } else {
            Err(result)
        }
    }

    /// Search a project directory for DomainPack config files and load the first found.
    ///
    /// Search order (first found wins):
    /// 1. `ONEAI.domain.yaml`
    /// 2. `ONEAI.domain.yml`
    /// 3. `ONEAI.domain.toml`
    ///
    /// Returns `None` if no config file is found in the directory.
    pub fn search_dir(project_dir: &str) -> Option<Self> {
        let dir = Path::new(project_dir);

        for filename in &["ONEAI.domain.yaml", "ONEAI.domain.yml", "ONEAI.domain.toml"] {
            let path = dir.join(filename);
            if path.exists() {
                if let Ok(spec_file) = Self::load(&path) {
                    return Some(spec_file);
                }
            }
        }

        None
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_parser::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn make_valid_config() -> DomainPackConfig {
        DomainPackConfig {
            name: "test-domain".to_string(),
            description: "Test domain".to_string(),
            tools: vec!["read_file".to_string(), "calculator".to_string()],
            tool_decorators: HashMap::new(),
            context_sources: vec!["date".to_string()],
            permission_profile: PermissionProfileConfig {
                auto_approve: vec!["read_file".to_string(), "calculator".to_string()],
                require_confirmation: vec![],
                deny_by_default: vec![],
            },
            paradigm_strategies: Vec::new(),
            compression_template: CompressionTemplateConfig {
                name: "test".to_string(),
                preserve_fields: vec!["key_data".to_string()],
                truncate_rules: HashMap::new(),
            },
            system_prompt: "You are a test agent".to_string(),
        }
    }

    #[test]
    fn test_spec_file_from_config() {
        let config = make_valid_config();
        let spec_file = DomainPackSpecFile::from_config(config);
        assert!(spec_file.source_path.is_none());
        assert_eq!(spec_file.config.name, "test-domain");
    }

    #[test]
    fn test_spec_file_validate_valid() {
        let config = make_valid_config();
        let spec_file = DomainPackSpecFile::from_config(config);
        let result = spec_file.validate();
        assert!(result.is_valid());
    }

    #[test]
    fn test_spec_file_validate_and_build_valid() {
        let config = make_valid_config();
        let spec_file = DomainPackSpecFile::from_config(config);
        let pack_result = spec_file.validate_and_build("/tmp/test");
        assert!(pack_result.is_ok());
        let pack = pack_result.unwrap();
        assert_eq!(pack.name, "test-domain");
        assert_eq!(pack.tools.len(), 2); // read_file + calculator
    }

    #[test]
    fn test_spec_file_validate_and_build_invalid() {
        let mut config = make_valid_config();
        config.name = String::new(); // invalid
        let spec_file = DomainPackSpecFile::from_config(config);
        let pack_result = spec_file.validate_and_build("/tmp/test");
        assert!(pack_result.is_err());
        let result = pack_result.unwrap_err();
        assert!(!result.is_valid());
    }

    #[test]
    fn test_spec_file_load_yaml() {
        let tmp_dir = TempDir::new().unwrap();
        let yaml_path = tmp_dir.path().join("ONEAI.domain.yaml");

        let yaml_content = r#"
name: test-yaml
description: "Test YAML domain"
tools: [read_file, calculator]
context_sources: [date]
[permission_profile]
auto_approve = ["read_file", "calculator"]
require_confirmation = []
deny_by_default = []
system_prompt: "You are a test agent"
"#;

        // Note: YAML format needs nested objects for permission_profile
        let yaml_content = r#"
name: test-yaml
description: "Test YAML domain"
tools: [read_file, calculator]
context_sources: [date]
permission_profile:
  auto_approve: [read_file, calculator]
  require_confirmation: []
  deny_by_default: []
compression_template:
  name: test
  preserve_fields: [key_data]
system_prompt: "You are a test agent"
"#;

        std::fs::write(&yaml_path, yaml_content).unwrap();
        let spec_file = DomainPackSpecFile::load(&yaml_path).unwrap();
        assert_eq!(spec_file.config.name, "test-yaml");
        assert!(spec_file.source_path.is_some());
    }

    #[test]
    fn test_spec_file_load_toml() {
        let tmp_dir = TempDir::new().unwrap();
        let toml_path = tmp_dir.path().join("ONEAI.domain.toml");

        let toml_content = r#"
name = "test-toml"
description = "Test TOML domain"
tools = ["read_file", "calculator"]
context_sources = ["date"]
system_prompt = "You are a test agent"

[permission_profile]
auto_approve = ["read_file", "calculator"]
require_confirmation = []
deny_by_default = []

[compression_template]
name = "test"
preserve_fields = ["key_data"]
"#;

        std::fs::write(&toml_path, toml_content).unwrap();
        let spec_file = DomainPackSpecFile::load(&toml_path).unwrap();
        assert_eq!(spec_file.config.name, "test-toml");
        assert!(spec_file.source_path.is_some());
    }

    #[test]
    fn test_spec_file_load_unknown_extension() {
        let tmp_dir = TempDir::new().unwrap();
        let json_path = tmp_dir.path().join("ONEAI.domain.json");
        std::fs::write(&json_path, "{}").unwrap();

        let result = DomainPackSpecFile::load(&json_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Unknown domain spec file extension"));
    }

    #[test]
    fn test_spec_file_search_dir_finds_yaml() {
        let tmp_dir = TempDir::new().unwrap();

        let yaml_content = r#"
name: search-test
description: "Found by search"
tools: [read_file]
permission_profile:
  auto_approve: [read_file]
  require_confirmation: []
  deny_by_default: []
compression_template:
  name: search
  preserve_fields: [test]
system_prompt: "You are found"
"#;

        std::fs::write(tmp_dir.path().join("ONEAI.domain.yaml"), yaml_content).unwrap();

        let spec_file = DomainPackSpecFile::search_dir(tmp_dir.path().to_str().unwrap()).unwrap();
        assert_eq!(spec_file.config.name, "search-test");
    }

    #[test]
    fn test_spec_file_search_dir_finds_toml() {
        let tmp_dir = TempDir::new().unwrap();

        let toml_content = r#"
name = "search-toml"
description = "Found by search"
tools = ["read_file"]
system_prompt = "You are found"

[permission_profile]
auto_approve = ["read_file"]
require_confirmation = []
deny_by_default = []

[compression_template]
name = "search"
preserve_fields = ["test"]
"#;

        std::fs::write(tmp_dir.path().join("ONEAI.domain.toml"), toml_content).unwrap();

        let spec_file = DomainPackSpecFile::search_dir(tmp_dir.path().to_str().unwrap()).unwrap();
        assert_eq!(spec_file.config.name, "search-toml");
    }

    #[test]
    fn test_spec_file_search_dir_none_found() {
        let tmp_dir = TempDir::new().unwrap();
        let result = DomainPackSpecFile::search_dir(tmp_dir.path().to_str().unwrap());
        assert!(result.is_none());
    }
}
