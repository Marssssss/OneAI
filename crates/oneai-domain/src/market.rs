//! DomainPack market — discovery, installation, and loading of domain packs.
//!
//! This module implements the local-first DomainPack distribution system:
//! - **Built-in packs**: coding, research, general — available without installation
//! - **Local files**: ONEAI.domain.yaml / .toml in project directories
//! - **Git repos**: clone from git URLs into ~/.oneai/packs/
//! - **Registry**: future remote pack index (oneai.dev/packs)
//!
//! Usage:
//! ```ignore
//! let registry = PackRegistry::new("~/.oneai/packs");
//!
//! // Search for packs
//! let results = registry.search("research");
//!
//! // Install from a git URL
//! registry.install(&PackSource::Git {
//!     repo_url: "https://github.com/oneai-project/oneai-pack-research.git".to_string(),
//!     ref: None,
//! })?;
//!
//! // Load an installed pack
//! let pack = registry.load_installed("research", ".")?;
//! ```

use std::collections::HashMap;
use std::path::PathBuf;

use crate::DomainPack;

// ─── PackSource ────────────────────────────────────────────────────────────

/// Where a DomainPack comes from.
#[derive(Debug, Clone)]
pub enum PackSource {
    /// Local path (ONEAI.domain.yaml / .toml).
    Local {
        path: PathBuf,
    },
    /// Git repository.
    Git {
        repo_url: String,
        /// Optional branch/tag/commit ref.
        ref_: Option<String>,
    },
    /// Official registry (oneai.dev/packs) — future feature.
    Registry {
        name: String,
        version: Option<String>,
    },
}

// ─── PackIndexEntry ────────────────────────────────────────────────────────

/// Metadata about a known domain pack.
#[derive(Debug, Clone)]
pub struct PackIndexEntry {
    /// Unique pack name (e.g., "coding", "research").
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Version string (for registry packs).
    pub version: String,
    /// Author or maintainer.
    pub author: String,
    /// Tags for search (e.g., "coding", "software", "development").
    pub tags: Vec<String>,
    /// Where to get this pack.
    pub source: PackSource,
    /// Whether this pack is available without installation.
    pub is_builtin: bool,
}

// ─── PackRegistry ──────────────────────────────────────────────────────────

/// DomainPack registry — manages installed packs and the builtin index.
///
/// The registry provides:
/// - Search: find packs by name, description, or tags
/// - Install: download and cache packs from git URLs or local paths
/// - Load: load installed packs into DomainPack structs
/// - List: show all available packs (builtin + installed)
pub struct PackRegistry {
    /// Local cache directory for installed packs.
    cache_dir: PathBuf,
    /// Index of known packs (builtin + discovered).
    index: HashMap<String, PackIndexEntry>,
}

impl PackRegistry {
    /// Create a new registry with the given cache directory.
    ///
    /// The cache_dir is typically `~/.oneai/packs/`.
    /// Creates the directory if it doesn't exist.
    pub fn new(cache_dir: impl Into<PathBuf>) -> Self {
        let cache_dir = cache_dir.into();
        if !cache_dir.exists() {
            let _ = std::fs::create_dir_all(&cache_dir);
        }

        let mut registry = Self {
            cache_dir,
            index: HashMap::new(),
        };

        // Populate with builtin pack entries
        registry.populate_builtin_index();
        registry.discover_installed_packs();

        registry
    }

    /// Create a registry using the default path: ~/.oneai/packs/.
    pub fn default_path() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        Self::new(home.join(".oneai").join("packs"))
    }

    /// Search for packs matching a query.
    ///
    /// Matches against name, description, and tags.
    pub fn search(&self, query: &str) -> Vec<&PackIndexEntry> {
        let query_lower = query.to_lowercase();
        self.index.values()
            .filter(|entry| {
                entry.name.to_lowercase().contains(&query_lower)
                    || entry.description.to_lowercase().contains(&query_lower)
                    || entry.tags.iter().any(|t| t.to_lowercase().contains(&query_lower))
            })
            .collect()
    }

    /// Get a specific pack entry by name.
    pub fn get_entry(&self, name: &str) -> Option<&PackIndexEntry> {
        self.index.get(name)
    }

    /// List all known pack entries (builtin + installed).
    pub fn list_all(&self) -> Vec<&PackIndexEntry> {
        self.index.values().collect()
    }

    /// List only builtin pack entries.
    pub fn list_builtin(&self) -> Vec<&PackIndexEntry> {
        self.index.values().filter(|e| e.is_builtin).collect()
    }

    /// List only installed pack entries (not builtin).
    pub fn list_installed(&self) -> Vec<&PackIndexEntry> {
        self.index.values().filter(|e| !e.is_builtin).collect()
    }

    /// Install a pack from a source.
    ///
    /// Returns the installed pack name on success.
    pub fn install(&self, source: &PackSource) -> Result<String, PackInstallError> {
        match source {
            PackSource::Local { path } => self.install_local(path),
            PackSource::Git { repo_url, ref_ } => self.install_git(repo_url, ref_.clone()),
            PackSource::Registry { name, version } => {
                // Registry installation is not yet implemented
                Err(PackInstallError::NotImplemented(
                    format!("Registry installation for '{}' v{} is not yet implemented. Use Git or Local source.", name, version.clone().unwrap_or_else(|| "latest".to_string()))
                ))
            }
        }
    }

    /// Load an installed or builtin pack by name.
    ///
    /// For builtin packs, uses the hardcoded constructors.
    /// For installed packs, loads from the cached directory.
    pub fn load_installed(&self, name: &str, project_dir: &str) -> Result<DomainPack, PackLoadError> {
        // Try builtin first
        if name == "coding" {
            return Ok(crate::coding_pack(project_dir));
        }
        if name == "research" {
            return Ok(crate::research_pack(project_dir));
        }
        // "general" pack is in CLI, not here — handled by caller

        // Try installed pack directory
        let pack_dir = self.cache_dir.join(name);
        if pack_dir.exists() {
            return crate::domain_pack_from_dir(&pack_dir.to_string_lossy())
                .map_err(|e| PackLoadError::LoadFailed(name.to_string(), e.to_string()));
        }

        Err(PackLoadError::NotFound(name.to_string()))
    }

    // ─── Private methods ───────────────────────────────────────────────────

    fn populate_builtin_index(&mut self) {
        let builtins = builtin_pack_entries();
        for entry in builtins {
            self.index.insert(entry.name.clone(), entry);
        }
    }

    fn discover_installed_packs(&mut self) {
        if !self.cache_dir.exists() {
            return;
        }
        if let Ok(entries) = std::fs::read_dir(&self.cache_dir) {
            for entry in entries.flatten() {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    let name = entry.file_name().to_string_lossy().to_string();
                    // Skip if already in index as builtin
                    if !self.index.contains_key(&name) {
                        let pack_dir = entry.path();
                        let description = self.read_pack_description(&pack_dir);
                        self.index.insert(name.clone(), PackIndexEntry {
                            name: name.clone(),
                            description,
                            version: "local".to_string(),
                            author: "unknown".to_string(),
                            tags: vec![name.clone()],
                            source: PackSource::Local { path: pack_dir },
                            is_builtin: false,
                        });
                    }
                }
            }
        }
    }

    fn read_pack_description(&self, pack_dir: &PathBuf) -> String {
        // Try to read description from config file
        for file in &["ONEAI.domain.yaml", "ONEAI.domain.yml", "ONEAI.domain.toml"] {
            let config_path = pack_dir.join(file);
            if config_path.exists() {
                if let Ok(config) = crate::domain_pack_from_file(&config_path, "") {
                    return config.description;
                }
            }
        }
        "Installed domain pack".to_string()
    }

    fn install_local(&self, path: &PathBuf) -> Result<String, PackInstallError> {
        if !path.exists() {
            return Err(PackInstallError::SourceNotFound(path.display().to_string()));
        }

        // Determine pack name from directory or config
        let pack_name = if path.is_dir() {
            crate::domain_pack_from_dir(&path.to_string_lossy())
                .map(|p| p.name.clone())
                .unwrap_or_else(|_| path.file_name().unwrap().to_string_lossy().to_string())
        } else {
            path.file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or("custom".to_string())
        };

        let dest = self.cache_dir.join(&pack_name);
        if dest.exists() {
            return Err(PackInstallError::AlreadyInstalled(pack_name));
        }

        // Copy
        if path.is_dir() {
            copy_dir_recursive(path, &dest)
                .map_err(|e| PackInstallError::CopyFailed(e.to_string()))?;
        } else {
            std::fs::create_dir_all(&dest)
                .map_err(|e| PackInstallError::CopyFailed(e.to_string()))?;
            std::fs::copy(path, dest.join(path.file_name().unwrap()))
                .map_err(|e| PackInstallError::CopyFailed(e.to_string()))?;
        }

        Ok(pack_name)
    }

    fn install_git(&self, repo_url: &str, ref_: Option<String>) -> Result<String, PackInstallError> {
        let pack_name = extract_pack_name_from_git_url(repo_url);
        let dest = self.cache_dir.join(&pack_name);

        if dest.exists() {
            return Err(PackInstallError::AlreadyInstalled(pack_name));
        }

        let dest_str = dest.to_string_lossy().into_owned();

        let output = if let Some(ref_str) = &ref_ {
            std::process::Command::new("git")
                .args(["clone", "--branch", ref_str, repo_url, &dest_str])
                .output()
                .map_err(|e| PackInstallError::GitFailed(e.to_string()))?
        } else {
            std::process::Command::new("git")
                .args(["clone", repo_url, &dest_str])
                .output()
                .map_err(|e| PackInstallError::GitFailed(e.to_string()))?
        };

        if !output.status.success() {
            return Err(PackInstallError::GitFailed(
                String::from_utf8_lossy(&output.stderr).to_string()
            ));
        }

        Ok(pack_name)
    }
}

// ─── Helper functions ──────────────────────────────────────────────────────

fn extract_pack_name_from_git_url(url: &str) -> String {
    let url = url.trim_end_matches(".git");
    url.rsplit('/')
        .next()
        .unwrap_or("custom-pack")
        .to_string()
}

fn copy_dir_recursive(src: &PathBuf, dst: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

// ─── Built-in pack entries ─────────────────────────────────────────────────

/// The list of known built-in domain packs.
fn builtin_pack_entries() -> Vec<PackIndexEntry> {
    vec![
        PackIndexEntry {
            name: "coding".to_string(),
            description: "Coding domain pack — software development tools, context, and strategies".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            author: "OneAI".to_string(),
            tags: vec!["coding".to_string(), "software".to_string(), "development".to_string(),
                        "editing".to_string(), "shell".to_string(), "git".to_string()],
            source: PackSource::Local { path: PathBuf::from("builtin://coding") },
            is_builtin: true,
        },
        PackIndexEntry {
            name: "research".to_string(),
            description: "Research domain pack — web search, analysis, and synthesis tools".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            author: "OneAI".to_string(),
            tags: vec!["research".to_string(), "search".to_string(), "analysis".to_string(),
                        "web".to_string(), "synthesis".to_string(), "citation".to_string()],
            source: PackSource::Local { path: PathBuf::from("builtin://research") },
            is_builtin: true,
        },
        PackIndexEntry {
            name: "general".to_string(),
            description: "General-purpose domain pack — minimal tool set for basic tasks".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            author: "OneAI".to_string(),
            tags: vec!["general".to_string(), "chat".to_string(), "calculator".to_string()],
            source: PackSource::Local { path: PathBuf::from("builtin://general") },
            is_builtin: true,
        },
        PackIndexEntry {
            name: "writing".to_string(),
            description: "Content creation and editing domain pack — coming soon".to_string(),
            version: "planned".to_string(),
            author: "OneAI".to_string(),
            tags: vec!["writing".to_string(), "content".to_string(), "editing".to_string(),
                        "creative".to_string()],
            source: PackSource::Registry { name: "writing".to_string(), version: None },
            is_builtin: false,
        },
        PackIndexEntry {
            name: "data".to_string(),
            description: "Data analysis and visualization domain pack — coming soon".to_string(),
            version: "planned".to_string(),
            author: "OneAI".to_string(),
            tags: vec!["data".to_string(), "analysis".to_string(), "visualization".to_string(),
                        "query".to_string(), "statistics".to_string()],
            source: PackSource::Registry { name: "data".to_string(), version: None },
            is_builtin: false,
        },
        PackIndexEntry {
            name: "devops".to_string(),
            description: "Infrastructure and deployment domain pack — coming soon".to_string(),
            version: "planned".to_string(),
            author: "OneAI".to_string(),
            tags: vec!["devops".to_string(), "deploy".to_string(), "monitor".to_string(),
                        "infrastructure".to_string(), "ci".to_string()],
            source: PackSource::Registry { name: "devops".to_string(), version: None },
            is_builtin: false,
        },
    ]
}

// ─── Error types ───────────────────────────────────────────────────────────

/// Error during pack installation.
#[derive(Debug)]
pub enum PackInstallError {
    /// The source path or URL does not exist or is inaccessible.
    SourceNotFound(String),
    /// The pack is already installed.
    AlreadyInstalled(String),
    /// Failed to copy files.
    CopyFailed(String),
    /// Failed to clone git repository.
    GitFailed(String),
    /// Feature not yet implemented.
    NotImplemented(String),
}

impl std::fmt::Display for PackInstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceNotFound(path) => write!(f, "Source not found: {}", path),
            Self::AlreadyInstalled(name) => write!(f, "Pack '{}' already installed", name),
            Self::CopyFailed(msg) => write!(f, "Copy failed: {}", msg),
            Self::GitFailed(msg) => write!(f, "Git clone failed: {}", msg),
            Self::NotImplemented(msg) => write!(f, "Not implemented: {}", msg),
        }
    }
}

impl std::error::Error for PackInstallError {}

/// Error during pack loading.
#[derive(Debug)]
pub enum PackLoadError {
    /// The requested pack was not found.
    NotFound(String),
    /// The pack was found but could not be loaded.
    LoadFailed(String, String),
}

impl std::fmt::Display for PackLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(name) => write!(f, "Pack '{}' not found", name),
            Self::LoadFailed(name, msg) => write!(f, "Failed to load pack '{}': {}", name, msg),
        }
    }
}

impl std::error::Error for PackLoadError {}
