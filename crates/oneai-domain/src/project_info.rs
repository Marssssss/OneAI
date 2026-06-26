//! Project info generator — the writer symmetric to `ProjectInstructionsSource` (reader).
//!
//! `ProjectInstructionsSource` reads ONEAI.md / CLAUDE.md / AGENTS.md into agent
//! context (inspired by Claude Code's CLAUDE.md and OpenCode's AGENTS.md). This
//! module *generates* those files so a fresh checkout can bootstrap its own
//! agent context with `oneai init` (CLI) or `/init` (TUI).
//!
//! Two generation paths, mirroring how Claude Code's `/init` and OpenCode's
//! `/init` actually work:
//!
//! 1. **LLM synthesis** ([`generate_project_info_with_llm`]) — preferred when a
//!    provider is configured. A deep probe gathers real signals (full README,
//!    crate-level docs + `pub mod` module map, manifest facts, existing
//!    instruction file, docs inventory, git context) and the model composes a
//!    concise, actionable doc following the `init` skill's principles:
//!    *link-don't-embed*, *minimal-by-default*, *every line guides behavior*.
//! 2. **Heuristic** ([`generate_project_info`]) — deterministic, provider-free
//!    fallback. Emits a structured doc from the same deep probe.
//!
//! Everything is async (tokio::fs / tokio::process) and cross-platform, matching
//! the existing `builtin_sources` implementations.

use std::path::{Path, PathBuf};

use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::LlmProvider;
use oneai_core::types::{
    Conversation, InferenceRequest, Message,
};

/// Over-collect dependencies during probing; the composer / LLM trims for display.
const PROBE_MAX_DEPS: usize = 60;
/// Soft char caps for the LLM-bound context block (keeps the prompt bounded).
const README_CAP: usize = 6000;
const CRATE_DOC_CAP: usize = 800;
const CONTEXT_BLOCK_CAP: usize = 14000;

// ─── Output format ───────────────────────────────────────────────────────────

/// Which project-instruction file format to generate.
///
/// OneAI reads all three formats (ONEAI.md, CLAUDE.md, AGENTS.md) with the same
/// priority, so any choice is picked up automatically by `ProjectInstructionsSource`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProjectInfoFormat {
    /// OneAI's native format → `ONEAI.md`.
    #[default]
    Oneai,
    /// OpenCode-compatible format → `AGENTS.md`.
    Agents,
    /// Claude Code-compatible format → `CLAUDE.md`.
    Claude,
}

impl ProjectInfoFormat {
    /// Create from a lowercase string identifier (`oneai` / `agents` / `claude`).
    pub fn from_name(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "oneai" => Ok(Self::Oneai),
            "agents" | "agent" => Ok(Self::Agents),
            "claude" | "claudemd" | "claude.md" => Ok(Self::Claude),
            other => Err(OneAIError::Config(format!(
                "unknown project-info format '{}'; expected one of: oneai, agents, claude",
                other
            ))),
        }
    }

    /// Target filename for this format.
    pub fn filename(&self) -> &'static str {
        match self {
            Self::Oneai => "ONEAI.md",
            Self::Agents => "AGENTS.md",
            Self::Claude => "CLAUDE.md",
        }
    }

    /// Human-readable label for the format (used in the generated header).
    pub fn label(&self) -> &'static str {
        match self {
            Self::Oneai => "OneAI",
            Self::Agents => "OpenCode (AGENTS.md)",
            Self::Claude => "Claude Code (CLAUDE.md)",
        }
    }
}

// ─── Options & result ────────────────────────────────────────────────────────

/// Options controlling project-info generation.
#[derive(Debug, Clone)]
pub struct ProjectInfoOptions {
    /// Output format / filename. Defaults to [`ProjectInfoFormat::Oneai`].
    pub format: ProjectInfoFormat,
    /// Overwrite an existing instruction file. When `false` (default) and the
    /// target file exists, generation is skipped and the existing file is kept.
    pub force: bool,
    /// Include the top-level file-tree / module-map section in heuristic output.
    pub include_file_tree: bool,
    /// Cap on file-tree lines (heuristic output).
    pub max_structure_lines: usize,
    /// Cap on key-dependency entries listed (heuristic output).
    pub max_dependencies: usize,
}

impl Default for ProjectInfoOptions {
    fn default() -> Self {
        Self {
            format: ProjectInfoFormat::Oneai,
            force: false,
            include_file_tree: true,
            max_structure_lines: 120,
            max_dependencies: 40,
        }
    }
}

/// The outcome of a generation call.
#[derive(Debug, Clone)]
pub struct GeneratedProjectInfo {
    /// Absolute path of the (would-be) instruction file.
    pub path: PathBuf,
    /// Filename written (`ONEAI.md` / `AGENTS.md` / `CLAUDE.md`).
    pub filename: String,
    /// Composed markdown content.
    pub content: String,
    /// True if an existing file was overwritten (`force` was set).
    pub overwritten: bool,
    /// True if generation was skipped because the file already existed and
    /// `force` was not set. `content` still holds the freshly composed doc.
    pub skipped: bool,
    /// Whether the content came from LLM synthesis (`true`) or the heuristic
    /// fallback (`false`).
    pub llm_generated: bool,
}

// ─── Probe: gathered project facts ───────────────────────────────────────────

/// Detected build-system family. `None` (in the probe) means unrecognized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BuildSystem {
    RustCargo,
    NodePackage,
    Python,
    GoModule,
    Make,
    Cmake,
}

impl BuildSystem {
    fn label(&self) -> &'static str {
        match self {
            Self::RustCargo => "Rust / Cargo",
            Self::NodePackage => "Node.js / npm",
            Self::Python => "Python",
            Self::GoModule => "Go modules",
            Self::Make => "Make",
            Self::Cmake => "CMake",
        }
    }
}

/// A crate/module discovered in the module map.
#[derive(Debug, Clone)]
pub struct ModuleEntry {
    /// Crate / package name (manifest `name`).
    pub name: String,
    /// Relative path from project root.
    pub path: String,
    /// One-line description (manifest `description` or first crate-doc line).
    pub description: String,
    /// Crate-level doc comment (`//!`) excerpt.
    pub doc: String,
    /// `pub mod` declarations in the entry-point file.
    pub modules: Vec<String>,
}

/// Facts gathered about the project by the probe.
#[derive(Debug, Default)]
pub struct ProjectProbe {
    pub project_dir: PathBuf,
    pub name: String,
    pub version: String,
    pub description: String,
    pub build_system: Option<BuildSystem>,
    pub language: String,
    /// (label, command) pairs.
    pub commands: Vec<(String, String)>,
    /// (name, version) pairs.
    pub key_dependencies: Vec<(String, String)>,
    pub conventions: Vec<String>,
    /// Workspace members / crates with docs + module declarations.
    pub module_map: Vec<ModuleEntry>,
    /// Entry-point file excerpts (lib.rs / main.rs / index.ts) — first ~40 doc lines.
    pub entry_points: Vec<(String, String)>,
    /// Full README content (capped).
    pub readme: String,
    /// Existing instruction file content (ONEAI.md/CLAUDE.md/AGENTS.md), if any.
    pub existing_instructions: Option<(String, String)>, // (filename, content)
    /// Paths of supplementary docs to link (ARCHITECTURE.md, CONTRIBUTING.md, docs/*).
    pub doc_links: Vec<String>,
    /// Raw top-level file tree (capped).
    pub file_tree: String,
    pub git_branch: String,
    pub git_commits: String,
}

impl ProjectProbe {
    /// Run the deep probe against `project_dir`.
    pub async fn probe(project_dir: &Path) -> Self {
        let mut p = Self {
            project_dir: project_dir.to_path_buf(),
            ..Self::default()
        };

        // Detect build system + manifest facts.
        if project_dir.join("Cargo.toml").exists() {
            p.build_system = Some(BuildSystem::RustCargo);
            p.language = "Rust".to_string();
            probe_cargo(project_dir, &mut p).await;
        } else if project_dir.join("package.json").exists() {
            p.build_system = Some(BuildSystem::NodePackage);
            p.language = "JavaScript/TypeScript".to_string();
            probe_node(project_dir, &mut p).await;
        } else if project_dir.join("pyproject.toml").exists()
            || project_dir.join("setup.py").exists()
            || project_dir.join("requirements.txt").exists()
        {
            p.build_system = Some(BuildSystem::Python);
            p.language = "Python".to_string();
            probe_python(project_dir, &mut p).await;
        } else if project_dir.join("go.mod").exists() {
            p.build_system = Some(BuildSystem::GoModule);
            p.language = "Go".to_string();
            probe_go(project_dir, &mut p).await;
        } else if project_dir.join("Makefile").exists()
            || project_dir.join("makefile").exists()
        {
            p.build_system = Some(BuildSystem::Make);
            probe_make(project_dir, &mut p).await;
        } else if project_dir.join("CMakeLists.txt").exists() {
            p.build_system = Some(BuildSystem::Cmake);
            p.language = "C/C++".to_string();
            probe_cmake(project_dir, &mut p).await;
        }

        detect_conventions(project_dir, &mut p).await;

        if p.name.is_empty() {
            p.name = project_dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "project".to_string());
        }

        // Deep, non-manifest signals.
        p.module_map = build_module_map(project_dir, p.build_system).await;
        p.entry_points = read_entry_points(project_dir, p.build_system).await;
        p.readme = read_readme(project_dir).await;
        p.existing_instructions = read_existing_instructions(project_dir).await;
        p.doc_links = inventory_docs(project_dir).await;
        p.file_tree = scan_file_tree(project_dir).await;
        let (branch, commits) = scan_git(project_dir).await;
        p.git_branch = branch;
        p.git_commits = commits;

        p
    }

    /// Format the probe as a single bounded text block for an LLM prompt.
    pub fn context_block(&self) -> String {
        let mut s = String::new();
        let mut push = |label: &str, body: String| {
            if body.trim().is_empty() {
                return;
            }
            s.push_str(&format!("## {}\n{}\n\n", label, body));
        };

        push("Project", format!(
            "name: {}\nversion: {}\nlanguage: {}\nbuild_system: {}\ndescription: {}\nroot: {}",
            self.name,
            self.version,
            self.language,
            self.build_system.as_ref().map(|b| b.label()).unwrap_or("unrecognized"),
            self.description,
            self.project_dir.display()
        ));

        if !self.commands.is_empty() {
            push("Commands", self.commands.iter()
                .map(|(l, c)| format!("- {}: {}", l, c))
                .collect::<Vec<_>>().join("\n"));
        }

        if !self.module_map.is_empty() {
            let mut mm = String::new();
            for m in &self.module_map {
                mm.push_str(&format!("- {} ({})", m.name, m.path));
                if !m.description.is_empty() {
                    mm.push_str(&format!(" — {}", m.description));
                }
                mm.push('\n');
                if !m.doc.is_empty() {
                    for line in m.doc.lines().take(8) {
                        mm.push_str(&format!("    {}\n", line));
                    }
                }
                if !m.modules.is_empty() {
                    mm.push_str(&format!("    modules: {}\n", m.modules.join(", ")));
                }
            }
            push("Module map (architecture)", mm);
        }

        if !self.entry_points.is_empty() {
            push("Entry-point file excerpts", self.entry_points.iter()
                .map(|(f, body)| format!("// {} \n{}", f, body))
                .collect::<Vec<_>>().join("\n\n"));
        }

        if !self.key_dependencies.is_empty() {
            push("Key dependencies", self.key_dependencies.iter()
                .map(|(n, v)| if v.is_empty() { format!("- {}", n) } else { format!("- {}: {}", n, v) })
                .collect::<Vec<_>>().join("\n"));
        }

        if !self.conventions.is_empty() {
            push("Conventions", self.conventions.iter()
                .map(|c| format!("- {}", c))
                .collect::<Vec<_>>().join("\n"));
        }

        if !self.readme.is_empty() {
            push("README", truncate_chars(&self.readme, README_CAP));
        }

        if let Some((fname, content)) = &self.existing_instructions {
            push(&format!("Existing instruction file ({})", fname),
                 truncate_chars(content, 4000));
        }

        if !self.doc_links.is_empty() {
            push("Supplementary docs (link, don't embed)",
                 self.doc_links.iter().map(|d| format!("- {}", d)).collect::<Vec<_>>().join("\n"));
        }

        if !self.file_tree.is_empty() {
            push("File tree (top levels)", truncate_chars(&self.file_tree, 3000));
        }

        push("Git", format!(
            "branch: {}\nrecent commits:\n{}",
            self.git_branch, self.git_commits
        ));

        truncate_chars(&s, CONTEXT_BLOCK_CAP)
    }

    /// Build (system, user) prompt messages for LLM synthesis, modeled on the
    /// Claude Code / Copilot `init` skill principles.
    pub fn build_init_prompt(&self, format: ProjectInfoFormat) -> (String, String) {
        let system = format!(
            "You generate a {}-compatible project-instruction file ({}) for an AI coding \
agent running in the OneAI framework. The file is auto-loaded into agent context at \
session start, so it must make the agent immediately productive.\n\n\
Principles (follow strictly):\n\
1. Link, don't embed — reference existing docs (README.md, ARCHITECTURE.md, \
CONTRIBUTING.md, docs/*) with Markdown links instead of copying their content.\n\
2. Minimal by default — include only what an agent cannot easily discover itself; \
omit the obvious.\n\
3. Concise and actionable — every line should guide behavior (commands, constraints, \
do/don't rules, gotchas).\n\
4. Ground every claim in the provided project facts; if something is uncertain, omit \
it rather than guess.\n\n\
Output ONLY the raw markdown file content. Do not wrap it in a code fence, do not add \
preamble or commentary, do not mention that you were asked to generate a file.",
            format.label(), format.filename()
        );

        let user = format!(
            "Project root: {}\n\n\
Here are facts gathered from the project:\n\n\
{}\n\n\
Write the full content of {}. Start with a `# <Project name>` H1. Include these \
sections (skip any that lack grounding facts):\n\
- Overview (name, purpose, language, build system — 2-3 sentences)\n\
- Architecture & key design (component boundaries, layering, data/control flow, key \
abstractions and where they live — ground in the module map and entry points)\n\
- Build, test, run (exact commands from the facts)\n\
- Conventions (formatting, linting, commit style, patterns that differ from defaults)\n\
- Key files & directories (pointers to the most important files an agent must read)\n\
- Pitfalls / notes (environment setup, common mistakes, do/don't rules)\n\n\
Reference README.md / ARCHITECTURE.md / CONTRIBUTING.md via links where they hold \
detail. Keep it tight — prefer a sharp 60-line file over a vague 200-line one.",
            self.project_dir.display(),
            self.context_block(),
            format.filename()
        );

        (system, user)
    }
}

// ─── Per-build-system probes ─────────────────────────────────────────────────

async fn probe_cargo(dir: &Path, p: &mut ProjectProbe) {
    let Ok(text) = tokio::fs::read_to_string(dir.join("Cargo.toml")).await else {
        return;
    };
    let Ok(parsed) = text.parse::<toml::Value>() else {
        p.commands.push(("build".into(), "cargo build".into()));
        p.commands.push(("test".into(), "cargo test".into()));
        p.commands.push(("lint".into(), "cargo clippy --workspace --all-targets".into()));
        return;
    };

    if let Some(pkg) = parsed.get("package").and_then(|v| v.as_table()) {
        p.name = pkg.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        p.version = pkg.get("version").and_then(|v| v.as_str()).unwrap_or("").to_string();
        p.description = pkg.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
    } else if parsed.get("workspace").is_some() {
        p.name = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "workspace".into());
    }

    let is_workspace = parsed.get("workspace").is_some();
    let test = if is_workspace { "cargo test --workspace" } else { "cargo test" };
    let lint = "cargo clippy --workspace --all-targets";
    p.commands.push(("build".into(), "cargo build".into()));
    p.commands.push(("test".into(), test.into()));
    p.commands.push(("lint".into(), lint.into()));
    p.commands.push(("format".into(), "cargo fmt".into()));
    p.commands.push(("run".into(), "cargo run --bin <bin>".into()));

    let deps_table = parsed
        .get(if is_workspace { "workspace" } else { "package" })
        .and_then(|v| v.as_table())
        .and_then(|t| t.get("dependencies"))
        .and_then(|v| v.as_table())
        .or_else(|| parsed.get("dependencies").and_then(|v| v.as_table()));
    if let Some(deps) = deps_table {
        for (name, val) in deps.iter() {
            if p.key_dependencies.len() >= PROBE_MAX_DEPS {
                break;
            }
            p.key_dependencies.push((name.clone(), dep_version(val)));
        }
    }
}

/// Best-effort version string from a toml dependency value.
fn dep_version(val: &toml::Value) -> String {
    match val {
        toml::Value::String(s) => s.clone(),
        toml::Value::Table(t) => {
            if let Some(s) = t.get("version").and_then(|v| v.as_str()) {
                return s.to_string();
            }
            if t.get("workspace").and_then(|v| v.as_bool()).unwrap_or(false) {
                return "{workspace}".to_string();
            }
            String::new()
        }
        _ => String::new(),
    }
}

async fn probe_node(dir: &Path, p: &mut ProjectProbe) {
    let Ok(text) = tokio::fs::read_to_string(dir.join("package.json")).await else {
        return;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    p.name = json.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
    p.version = json.get("version").and_then(|v| v.as_str()).unwrap_or("").to_string();
    p.description = json.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();

    let pm = if dir.join("pnpm-lock.yaml").exists() {
        "pnpm"
    } else if dir.join("yarn.lock").exists() {
        "yarn"
    } else {
        "npm"
    };
    p.commands.push(("install".into(), format!("{pm} install")));
    p.commands.push(("build".into(), format!("{pm} run build")));
    p.commands.push(("test".into(), format!("{pm} test")));
    p.commands.push(("lint".into(), format!("{pm} run lint")));
    p.commands.push(("run".into(), format!("{pm} start")));

    if let Some(deps) = json.get("dependencies").and_then(|v| v.as_object()) {
        for (name, val) in deps.iter() {
            if p.key_dependencies.len() >= PROBE_MAX_DEPS {
                break;
            }
            p.key_dependencies.push((name.clone(), val.as_str().unwrap_or("").to_string()));
        }
    }
}

async fn probe_python(dir: &Path, p: &mut ProjectProbe) {
    if let Ok(text) = tokio::fs::read_to_string(dir.join("pyproject.toml")).await {
        if let Ok(parsed) = text.parse::<toml::Value>() {
            if let Some(proj) = parsed.get("project").and_then(|v| v.as_table()) {
                p.name = proj.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                p.version = proj.get("version").and_then(|v| v.as_str()).unwrap_or("").to_string();
                p.description = proj.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if let Some(deps) = proj.get("dependencies").and_then(|v| v.as_array()) {
                    for d in deps {
                        if p.key_dependencies.len() >= PROBE_MAX_DEPS {
                            break;
                        }
                        if let Some(s) = d.as_str() {
                            let (name, ver) = split_pep508(s);
                            p.key_dependencies.push((name, ver));
                        }
                    }
                }
            } else if let Some(poetry) = parsed.get("tool").and_then(|t| t.get("poetry")).and_then(|v| v.as_table()) {
                p.name = poetry.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                p.version = poetry.get("version").and_then(|v| v.as_str()).unwrap_or("").to_string();
                p.description = poetry.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if let Some(deps) = poetry.get("dependencies").and_then(|v| v.as_table()) {
                    for (name, val) in deps.iter() {
                        if p.key_dependencies.len() >= PROBE_MAX_DEPS {
                            break;
                        }
                        if name == "python" {
                            continue;
                        }
                        p.key_dependencies.push((name.clone(), dep_version(val)));
                    }
                }
            }
        }
    }
    p.commands.push(("install".into(), "pip install -e .".into()));
    p.commands.push(("test".into(), "pytest".into()));
    p.commands.push(("build".into(), "python -m build".into()));
    p.commands.push(("lint".into(), "ruff check".into()));
}

/// Split a PEP 508 requirement string into (name, version-spec).
fn split_pep508(s: &str) -> (String, String) {
    let mut name = String::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
            name.push(c);
            chars.next();
        } else {
            break;
        }
    }
    let rest = s[name.len()..].trim();
    (name, rest.to_string())
}

async fn probe_go(dir: &Path, p: &mut ProjectProbe) {
    if let Ok(text) = tokio::fs::read_to_string(dir.join("go.mod")).await {
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("module ") {
                p.name = rest.trim().to_string();
            } else if line.starts_with("go ") {
                p.version = line.replace("go ", "").trim().to_string();
            }
        }
    }
    p.commands.push(("build".into(), "go build ./...".into()));
    p.commands.push(("test".into(), "go test ./...".into()));
    p.commands.push(("run".into(), "go run .".into()));
    p.commands.push(("lint".into(), "go vet ./...".into()));
}

async fn probe_make(dir: &Path, p: &mut ProjectProbe) {
    let path = if dir.join("Makefile").exists() {
        dir.join("Makefile")
    } else {
        dir.join("makefile")
    };
    let Ok(text) = tokio::fs::read_to_string(&path).await else {
        return;
    };
    if let Ok(re) = regex::Regex::new(r"(?m)^([a-zA-Z][a-zA-Z0-9_.-]*):") {
        for cap in re.captures_iter(&text) {
            if let Some(target) = cap.get(1).map(|m| m.as_str()) {
                p.commands.push((target.to_string(), format!("make {target}")));
            }
        }
    }
}

async fn probe_cmake(dir: &Path, p: &mut ProjectProbe) {
    if let Ok(text) = tokio::fs::read_to_string(dir.join("CMakeLists.txt")).await {
        if let Ok(re) = regex::Regex::new(r#"project\s*\(\s*([A-Za-z0-9_.-]+)"#) {
            if let Some(cap) = re.captures(&text) {
                if let Some(name) = cap.get(1).map(|m| m.as_str()) {
                    p.name = name.to_string();
                }
            }
        }
    }
    p.commands.push(("configure".into(), "cmake -B build".into()));
    p.commands.push(("build".into(), "cmake --build build".into()));
    p.commands.push(("test".into(), "ctest --test-dir build".into()));
}

// ─── Conventions detection ───────────────────────────────────────────────────

async fn detect_conventions(dir: &Path, p: &mut ProjectProbe) {
    let checks: &[(&str, &str)] = &[
        ("rustfmt.toml",         "uses rustfmt with a custom config"),
        (".rustfmt.toml",        "uses rustfmt with a custom config"),
        ("clippy.toml",          "uses clippy with project-specific lints"),
        (".cargo/config.toml",   "has a .cargo/config.toml (custom build flags)"),
        (".eslintrc",            "uses ESLint"),
        (".eslintrc.json",       "uses ESLint"),
        (".eslintrc.js",         "uses ESLint"),
        (".eslintrc.cjs",        "uses ESLint"),
        (".eslintrc.yml",        "uses ESLint"),
        (".prettierrc",          "uses Prettier"),
        (".prettierrc.json",     "uses Prettier"),
        ("prettier.config.js",   "uses Prettier"),
        ("tsconfig.json",        "uses TypeScript (tsconfig.json present)"),
        (".ruff.toml",           "uses Ruff (lint/format)"),
        ("ruff.toml",            "uses Ruff (lint/format)"),
        (".flake8",              "uses flake8"),
        ("mypy.ini",             "uses mypy for type checking"),
        (".pre-commit-config.yaml", "uses pre-commit hooks"),
        ("pytest.ini",           "uses pytest (pytest.ini present)"),
        ("tox.ini",              "uses tox"),
        (".editorconfig",        "ships an .editorconfig (editor consistency)"),
        (".gitattributes",       "ships .gitattributes (line-ending / diff normalization)"),
        (".gitignore",           "ships a .gitignore"),
        ("CONTRIBUTING.md",      "has CONTRIBUTING.md (commit/PR conventions)"),
    ];
    for (file, note) in checks {
        if dir.join(file).exists() {
            p.conventions.push((*note).to_string());
        }
    }
    if dir.join("pyproject.toml").exists() {
        if let Ok(text) = tokio::fs::read_to_string(dir.join("pyproject.toml")).await {
            if text.contains("[tool.ruff") {
                p.conventions.push("uses Ruff (config in pyproject.toml)".into());
            }
            if text.contains("[tool.mypy") {
                p.conventions.push("uses mypy (config in pyproject.toml)".into());
            }
            if text.contains("[tool.pytest") || text.contains("[tool.pytest]") {
                p.conventions.push("uses pytest (config in pyproject.toml)".into());
            }
        }
    }
    if p.build_system == Some(BuildSystem::RustCargo) {
        if !p.conventions.iter().any(|c| c.contains("rustfmt")) {
            p.conventions.push("cargo fmt is the canonical formatter".into());
        }
        if !p.conventions.iter().any(|c| c.contains("clippy")) {
            p.conventions.push("cargo clippy is the canonical linter".into());
        }
    }
}

// ─── Module map / entry points / docs ────────────────────────────────────────

/// Build a module map: for Rust workspaces, each member crate with its doc +
/// `pub mod` declarations; for single Rust crates, the root lib.rs/main.rs modules.
async fn build_module_map(dir: &Path, build_system: Option<BuildSystem>) -> Vec<ModuleEntry> {
    let mut entries = Vec::new();

    if build_system != Some(BuildSystem::RustCargo) {
        return entries;
    }

    let members = workspace_members(dir).await;
    if members.is_empty() {
        // Single crate — treat the project itself as the one module.
        if let Some(e) = rust_module_entry(dir, dir).await {
            entries.push(e);
        }
        return entries;
    }

    for member_dir in members {
        if let Some(e) = rust_module_entry(dir, &member_dir).await {
            entries.push(e);
        }
    }
    entries
}

/// Resolve workspace `members` (supporting simple `crates/*` globs) to absolute dirs.
async fn workspace_members(dir: &Path) -> Vec<PathBuf> {
    let Ok(text) = tokio::fs::read_to_string(dir.join("Cargo.toml")).await else {
        return Vec::new();
    };
    let Ok(parsed) = text.parse::<toml::Value>() else {
        return Vec::new();
    };
    let Some(members) = parsed.get("workspace").and_then(|w| w.get("members")).and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for m in members {
        let Some(pat) = m.as_str() else { continue };
        for resolved in glob_expand(dir, pat) {
            if !out.contains(&resolved) {
                out.push(resolved);
            }
        }
    }
    out
}

/// Expand a simple glob pattern (`crates/*`, `crates/foo`) relative to `root`.
fn glob_expand(root: &Path, pat: &str) -> Vec<PathBuf> {
    let pat = pat.trim_matches('/');
    if !pat.contains('*') {
        let p = root.join(pat);
        return if p.is_dir() { vec![p] } else { vec![] };
    }
    // Handle a single `*` in one segment (e.g. `crates/*`).
    let parts: Vec<&str> = pat.split('/').collect();
    let star_idx = parts.iter().position(|s| s.contains('*'));
    let Some(idx) = star_idx else { return vec![] };

    let prefix: PathBuf = root.join(parts[..idx].iter().collect::<PathBuf>());
    let star_seg = parts[idx];
    let segs_after = &parts[idx + 1..];

    let Ok(rd) = std::fs::read_dir(&prefix) else { return vec![] };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !glob_match(star_seg, &name) {
            continue;
        }
        let mut p = entry.path();
        if !segs_after.is_empty() {
            p = p.join(segs_after.iter().collect::<PathBuf>());
        }
        if p.is_dir() {
            out.push(p);
        }
    }
    out
}

/// Match a single-segment glob with one `*` (e.g. `*`, `oneai-*`).
fn glob_match(pattern: &str, name: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(star) = pattern.find('*') {
        let prefix = &pattern[..star];
        let suffix = &pattern[star + 1..];
        return name.starts_with(prefix) && name.ends_with(suffix) && name.len() >= prefix.len() + suffix.len();
    }
    name == pattern
}

/// Build a [`ModuleEntry`] for a Rust crate dir, reading its Cargo.toml
/// description and `src/lib.rs` crate-doc + `pub mod` declarations.
async fn rust_module_entry(root: &Path, crate_dir: &Path) -> Option<ModuleEntry> {
    let cargo_path = crate_dir.join("Cargo.toml");
    let (name, description) = if cargo_path.exists() {
        let text = tokio::fs::read_to_string(&cargo_path).await.ok()?;
        let parsed = text.parse::<toml::Value>().ok()?;
        let name = parsed.get("package").and_then(|p| p.get("name")).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let desc = parsed.get("package").and_then(|p| p.get("description")).and_then(|v| v.as_str()).unwrap_or("").to_string();
        (name, desc)
    } else {
        (crate_dir.file_name()?.to_string_lossy().to_string(), String::new())
    };

    let lib_path = crate_dir.join("src").join("lib.rs");
    let (doc, modules) = if lib_path.exists() {
        parse_rust_entry(&lib_path).await.unwrap_or_default()
    } else {
        (String::new(), Vec::new())
    };

    let rel_path = crate_dir.strip_prefix(root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| crate_dir.to_string_lossy().to_string());

    Some(ModuleEntry {
        name,
        path: rel_path,
        description,
        doc: truncate_chars(&doc, CRATE_DOC_CAP),
        modules,
    })
}

/// Parse a Rust source file for its `//!` crate-doc block and `pub mod` declarations.
async fn parse_rust_entry(path: &Path) -> Option<(String, Vec<String>)> {
    let text = tokio::fs::read_to_string(path).await.ok()?;
    let mut doc_lines = Vec::new();
    let mut mods = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("//!") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            doc_lines.push(rest.to_string());
            if doc_lines.len().ge(&60) {
                break;
            }
            continue;
        }
        // pub mod foo; / pub(crate) mod foo;
        if let Some(m) = parse_pub_mod(trimmed) {
            mods.push(m);
        }
    }
    Some((doc_lines.join("\n"), mods))
}

/// Extract the module name from a `pub mod x;` / `pub(crate) mod x;` line.
fn parse_pub_mod(line: &str) -> Option<String> {
    let re = regex::Regex::new(r"^pub(?:\([^)]*\))?\s+mod\s+([a-zA-Z_][a-zA-Z0-9_]*)").ok()?;
    re.captures(line).and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
}

/// Read entry-point file doc excerpts (lib.rs / main.rs / index.ts).
async fn read_entry_points(dir: &Path, build_system: Option<BuildSystem>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let candidates: Vec<(&str, PathBuf)> = match build_system {
        Some(BuildSystem::RustCargo) => vec![
            ("src/lib.rs", dir.join("src").join("lib.rs")),
            ("src/main.rs", dir.join("src").join("main.rs")),
        ],
        Some(BuildSystem::NodePackage) => vec![
            ("src/index.ts", dir.join("src").join("index.ts")),
            ("src/index.js", dir.join("src").join("index.js")),
        ],
        Some(BuildSystem::Python) => vec![
            ("__init__.py", dir.join("__init__.py")),
            ("src/__init__.py", dir.join("src").join("__init__.py")),
        ],
        _ => vec![],
    };
    for (label, path) in candidates {
        if !path.exists() {
            continue;
        }
        if let Ok(text) = tokio::fs::read_to_string(&path).await {
            // First ~40 non-empty lines, trimmed.
            let body: String = text.lines()
                .filter(|l| !l.trim().is_empty())
                .take(40)
                .collect::<Vec<_>>()
                .join("\n");
            out.push((label.to_string(), body));
        }
    }
    out
}

async fn read_readme(dir: &Path) -> String {
    for name in &["README.md", "README", "README.rst", "README.txt", "readme.md"] {
        let path = dir.join(name);
        if !path.exists() {
            continue;
        }
        if let Ok(text) = tokio::fs::read_to_string(&path).await {
            return truncate_chars(&text, README_CAP);
        }
    }
    String::new()
}

/// If an existing ONEAI.md / CLAUDE.md / AGENTS.md is present, return (filename, content)
/// so the LLM can preserve valuable content and merge.
async fn read_existing_instructions(dir: &Path) -> Option<(String, String)> {
    for name in &["ONEAI.md", "CLAUDE.md", "AGENTS.md"] {
        let path = dir.join(name);
        if !path.exists() {
            continue;
        }
        if let Ok(text) = tokio::fs::read_to_string(&path).await {
            if !text.trim().is_empty() {
                return Some((name.to_string(), text));
            }
        }
    }
    None
}

/// Inventory supplementary doc files to *link* (not embed): ARCHITECTURE.md,
/// CONTRIBUTING.md, docs/*.md.
async fn inventory_docs(dir: &Path) -> Vec<String> {
    let mut links = Vec::new();
    for name in &["ARCHITECTURE.md", "CONTRIBUTING.md", "CHANGELOG.md", "DESIGN.md"] {
        if dir.join(name).exists() {
            links.push(name.to_string());
        }
    }
    let docs_dir = dir.join("docs");
    if docs_dir.is_dir() {
        if let Ok(rd) = std::fs::read_dir(&docs_dir) {
            for entry in rd.flatten() {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("md") {
                    if let Ok(rel) = p.strip_prefix(dir) {
                        links.push(rel.to_string_lossy().to_string());
                    }
                }
            }
        }
    }
    links.sort();
    links
}

// ─── File tree / git scans ───────────────────────────────────────────────────

async fn scan_file_tree(dir: &Path) -> String {
    let dir_str = dir.to_string_lossy();
    let (shell, shell_arg) = if cfg!(target_os = "windows") {
        ("powershell", "-Command")
    } else {
        ("sh", "-c")
    };
    let cmd = format!(
        "cd {} && find . -maxdepth 3 \
         -not -path '*/.*' -not -path '*/target/*' -not -path '*/node_modules/*' \
         -not -path '*/.venv/*' -not -path '*/__pycache__/*' -not -path '*/dist/*' \
         -not -path '*/build/*' -not -path '*/.git/*' | head -300",
        dir_str
    );
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new(shell).arg(shell_arg).arg(cmd).output(),
    )
    .await;
    match out {
        Ok(Ok(o)) => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() {
                "(file tree unavailable)".to_string()
            } else {
                s
            }
        }
        _ => "(file tree unavailable)".to_string(),
    }
}

async fn scan_git(dir: &Path) -> (String, String) {
    let dir_str = dir.to_string_lossy();
    let (shell, shell_arg) = if cfg!(target_os = "windows") {
        ("powershell", "-Command")
    } else {
        ("sh", "-c")
    };
    let branch = git_run(&dir_str, shell, shell_arg, "git branch --show-current 2>/dev/null").await;
    let commits = git_run(&dir_str, shell, shell_arg, "git log --oneline -10 2>/dev/null").await;
    let branch = if branch.is_empty() {
        "(not a git repo or no branch)".to_string()
    } else {
        branch
    };
    let commits = if commits.is_empty() {
        "(no commits)".to_string()
    } else {
        commits
    };
    (branch, commits)
}

async fn git_run(dir_str: &str, shell: &str, shell_arg: &str, arg: &str) -> String {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new(shell)
            .arg(shell_arg)
            .arg(format!("cd {dir_str} && {arg}"))
            .output(),
    );
    match out.await {
        Ok(Ok(o)) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Truncate to `max` chars on a UTF-8 char boundary, appending an ellipsis.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let end = s.char_indices()
        .take_while(|(i, _)| *i < max)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    format!("{}… [truncated]", &s[..end])
}

// ─── Composition: heuristic fallback ─────────────────────────────────────────

/// Compose the heuristic markdown document (provider-free fallback).
fn compose_heuristic(probe: &ProjectProbe, opts: &ProjectInfoOptions) -> String {
    let mut md = String::new();
    let title = if probe.name.is_empty() {
        "Project".to_string()
    } else {
        probe.name.clone()
    };

    md.push_str(&format!("# {}\n\n", title));
    md.push_str(&format!(
        "> Generated by `oneai init` — {}-compatible project-instruction file.\n",
        opts.format.label()
    ));
    md.push_str("> Read automatically into agent context by `ProjectInstructionsSource`.\n");
    md.push_str("> Tip: re-run `oneai init --force` with a configured LLM provider for an LLM-synthesized doc, or edit this file by hand.\n\n");

    md.push_str("## Overview\n\n");
    md.push_str(&format!("- **Name**: {}\n", probe.name));
    if !probe.version.is_empty() {
        md.push_str(&format!("- **Version**: {}\n", probe.version));
    }
    md.push_str(&format!("- **Language**: {}\n", if probe.language.is_empty() { "unknown".into() } else { probe.language.clone() }));
    md.push_str(&format!(
        "- **Build system**: {}\n",
        probe.build_system.as_ref().map(|b| b.label()).unwrap_or("unrecognized")
    ));
    if !probe.description.is_empty() {
        md.push_str(&format!("- **Description**: {}\n", probe.description));
    }
    md.push('\n');

    if !probe.commands.is_empty() {
        md.push_str("## Build, test, run\n\n```bash\n");
        for (label, cmd) in &probe.commands {
            md.push_str(&format!("{:<10} # {}\n", cmd, label));
        }
        md.push_str("```\n\n");
    }

    if !probe.module_map.is_empty() {
        md.push_str("## Architecture — module map\n\n");
        md.push_str("Grounded in each crate's `src/lib.rs` doc comment and `pub mod` declarations.\n\n");
        for m in &probe.module_map {
            md.push_str(&format!("### {} (`{}`)\n", m.name, m.path));
            if !m.description.is_empty() {
                md.push_str(&format!("{}\n", m.description));
            }
            if !m.doc.is_empty() {
                md.push('\n');
                for line in m.doc.lines().take(12) {
                    md.push_str(&format!("> {}\n", line));
                }
            }
            if !m.modules.is_empty() {
                md.push_str(&format!("\nModules: {}\n", m.modules.join(", ")));
            }
            md.push('\n');
        }
    } else if opts.include_file_tree && !probe.file_tree.is_empty() {
        md.push_str("## Project structure\n\n```\n");
        let lines: Vec<&str> = probe.file_tree.lines().take(opts.max_structure_lines).collect();
        md.push_str(&lines.join("\n"));
        if probe.file_tree.lines().count() > opts.max_structure_lines {
            md.push_str("\n... [truncated]");
        }
        md.push_str("\n```\n\n");
    }

    if !probe.entry_points.is_empty() {
        md.push_str("## Entry-point excerpts\n\n");
        for (label, body) in &probe.entry_points {
            md.push_str(&format!("`{}` (first lines):\n\n```\n{}\n```\n\n", label, body));
        }
    }

    if !probe.key_dependencies.is_empty() {
        md.push_str("## Key dependencies\n\n");
        for (name, ver) in probe.key_dependencies.iter().take(opts.max_dependencies) {
            if ver.is_empty() {
                md.push_str(&format!("- {}\n", name));
            } else {
                md.push_str(&format!("- {}: {}\n", name, ver));
            }
        }
        md.push('\n');
    }

    if !probe.conventions.is_empty() {
        md.push_str("## Conventions\n\n");
        for note in &probe.conventions {
            md.push_str(&format!("- {}\n", note));
        }
        md.push('\n');
    }

    if !probe.doc_links.is_empty() {
        md.push_str("## Further reading\n\n");
        for d in &probe.doc_links {
            md.push_str(&format!("- [{}]({})\n", d, d));
        }
        md.push('\n');
    }

    md.push_str("## Git context\n\n");
    md.push_str(&format!("- **Branch**: {}\n", probe.git_branch));
    if !probe.git_commits.is_empty() {
        md.push_str("- **Recent commits**:\n");
        for line in probe.git_commits.lines() {
            md.push_str(&format!("  {}\n", line));
        }
    }
    md.push('\n');

    md.push_str("## Notes\n\n");
    if !probe.readme.is_empty() {
        md.push_str("README (excerpt):\n\n");
        md.push_str(&probe.readme);
        md.push_str("\n\n");
    }
    md.push_str("<!-- Add project-specific guidance: coding standards, commit message\n");
    md.push_str("     conventions, deployment norms, do/don't rules the agent must follow. -->\n");

    md
}

// ─── Public entry points ─────────────────────────────────────────────────────

/// Run the deep probe and return the gathered facts (no file is written).
pub async fn probe_project(project_dir: &Path) -> Result<ProjectProbe> {
    if !project_dir.exists() {
        return Err(OneAIError::Config(format!(
            "project directory does not exist: {}",
            project_dir.display()
        )));
    }
    Ok(ProjectProbe::probe(project_dir).await)
}

/// Generate a project-instruction file using the **heuristic** composer
/// (provider-free). See [`generate_project_info_with_llm`] for the preferred
/// LLM-synthesized path.
pub async fn generate_project_info(
    project_dir: &Path,
    opts: &ProjectInfoOptions,
) -> Result<GeneratedProjectInfo> {
    let probe = ProjectProbe::probe(project_dir).await;
    let content = compose_heuristic(&probe, opts);
    write_content(project_dir, opts, content, false).await
}

/// Generate a project-instruction file via **LLM synthesis** — the model composes
/// a concise, actionable doc from the deep probe, following the `init` skill
/// principles (link-don't-embed, minimal-by-default, every line guides behavior).
///
/// Falls back to the heuristic composer if the provider returns no usable text,
/// so callers always get a file. `llm_generated` on the result indicates which
/// path produced the content.
pub async fn generate_project_info_with_llm(
    project_dir: &Path,
    opts: &ProjectInfoOptions,
    provider: &dyn LlmProvider,
) -> Result<GeneratedProjectInfo> {
    let probe = ProjectProbe::probe(project_dir).await;
    let (system, user) = probe.build_init_prompt(opts.format);

    let mut conv = Conversation::new();
    conv.add_message(Message::system(system));
    conv.add_message(Message::user(user));

    let req = InferenceRequest {
        conversation: conv,
        tools: Vec::new(),
        max_tokens: Some(4096),
        temperature: Some(0.3),
        top_p: None,
        stop_sequences: Vec::new(),
        constrained_output: None,
        thinking_budget: None,
        metadata: std::collections::HashMap::new(),
    };

    let llm_text = match tokio::time::timeout(
        std::time::Duration::from_secs(60),
        provider.infer(req),
    ).await {
        Ok(Ok(resp)) => {
            let text = resp.message.text_content();
            strip_code_fence(&text).trim().to_string()
        }
        Ok(Err(e)) => {
            tracing::warn!("project_info LLM synthesis failed, falling back to heuristic: {e}");
            String::new()
        }
        Err(_) => {
            tracing::warn!("project_info LLM synthesis timed out after 60s, falling back to heuristic");
            String::new()
        }
    };

    if llm_text.is_empty() {
        let content = compose_heuristic(&probe, opts);
        return write_content(project_dir, opts, content, false).await;
    }

    write_content(project_dir, opts, llm_text, true).await
}

/// Shared writer: respects `force` / skip semantics.
async fn write_content(
    project_dir: &Path,
    opts: &ProjectInfoOptions,
    content: String,
    llm_generated: bool,
) -> Result<GeneratedProjectInfo> {
    let filename = opts.format.filename().to_string();
    let target = project_dir.join(&filename);

    let (overwritten, skipped) = if target.exists() {
        if !opts.force {
            (false, true)
        } else {
            tokio::fs::write(&target, &content)
                .await
                .map_err(|e| OneAIError::Config(format!("failed to write {}: {}", target.display(), e)))?;
            (true, false)
        }
    } else {
        tokio::fs::write(&target, &content)
            .await
            .map_err(|e| OneAIError::Config(format!("failed to write {}: {}", target.display(), e)))?;
        (false, false)
    };

    Ok(GeneratedProjectInfo {
        path: target,
        filename,
        content,
        overwritten,
        skipped,
        llm_generated,
    })
}

/// Strip a wrapping ``` fenced code block if the model added one despite instructions.
fn strip_code_fence(text: &str) -> String {
    let t = text.trim();
    if !t.starts_with("```") {
        return t.to_string();
    }
    let after_open = t.trim_start_matches("```");
    // Skip optional language tag on the opening line.
    let after_open = if let Some(nl) = after_open.find('\n') {
        &after_open[nl + 1..]
    } else {
        after_open
    };
    let inner = after_open.trim_end_matches("```").trim_end();
    inner.to_string()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_filenames() {
        assert_eq!(ProjectInfoFormat::Oneai.filename(), "ONEAI.md");
        assert_eq!(ProjectInfoFormat::Agents.filename(), "AGENTS.md");
        assert_eq!(ProjectInfoFormat::Claude.filename(), "CLAUDE.md");
    }

    #[test]
    fn format_from_name_roundtrip() {
        assert_eq!(ProjectInfoFormat::from_name("oneai").unwrap(), ProjectInfoFormat::Oneai);
        assert_eq!(ProjectInfoFormat::from_name("AGENTS").unwrap(), ProjectInfoFormat::Agents);
        assert_eq!(ProjectInfoFormat::from_name("Claude").unwrap(), ProjectInfoFormat::Claude);
        assert!(ProjectInfoFormat::from_name("nope").is_err());
    }

    #[test]
    fn pep508_split() {
        assert_eq!(split_pep508("requests>=2.0"), ("requests".into(), ">=2.0".into()));
        assert_eq!(split_pep508("numpy"), ("numpy".into(), "".into()));
        assert_eq!(split_pep508("foo-bar>=1; extra == \"x\""), ("foo-bar".into(), ">=1; extra == \"x\"".into()));
    }

    #[test]
    fn truncate_is_char_boundary_safe() {
        let s = "中文测试中".repeat(20);
        let t = truncate_chars(&s, 10);
        assert!(t.ends_with("… [truncated]"));
        // Valid UTF-8 (does not panic on decode).
        let _ = t.chars().count();
    }

    #[test]
    fn strip_fence_removes_wrapping() {
        assert_eq!(strip_code_fence("```markdown\n# hi\nbody\n```"), "# hi\nbody");
        assert_eq!(strip_code_fence("# plain"), "# plain");
        assert_eq!(strip_code_fence("```\nbody\n```"), "body");
    }

    #[test]
    fn parse_pub_mod_extracts_name() {
        assert_eq!(parse_pub_mod("pub mod foo;"), Some("foo".into()));
        assert_eq!(parse_pub_mod("pub(crate) mod bar;"), Some("bar".into()));
        assert_eq!(parse_pub_mod("fn main() {}"), None);
    }

    #[test]
    fn glob_expand_crates_star() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("crates/one")).unwrap();
        std::fs::create_dir_all(dir.path().join("crates/two")).unwrap();
        std::fs::create_dir_all(dir.path().join("crates/notacrate")).unwrap();
        let got = glob_expand(dir.path(), "crates/*");
        assert_eq!(got.len(), 3);
        assert!(got.iter().any(|p| p.ends_with("one")));
    }

    #[tokio::test]
    async fn probe_cargo_workspace_with_module_map() {
        let dir = tempfile::tempdir().unwrap();
        let root_cargo = r#"
[workspace]
members = ["crates/*"]
resolver = "2"

[workspace.dependencies]
serde = "1.0"
"#;
        tokio::fs::write(dir.path().join("Cargo.toml"), root_cargo).await.unwrap();
        tokio::fs::create_dir_all(dir.path().join("crates/foo/src")).await.unwrap();
        tokio::fs::write(
            dir.path().join("crates/foo/Cargo.toml"),
            r#"[package]
name = "foo"
version = "0.1.0"
description = "The foo crate"
"#,
        ).await.unwrap();
        tokio::fs::write(
            dir.path().join("crates/foo/src/lib.rs"),
            "//! Foo does the thing.\n//! It is important.\npub mod bar;\npub mod baz;\n",
        ).await.unwrap();

        let probe = ProjectProbe::probe(dir.path()).await;
        assert_eq!(probe.build_system, Some(BuildSystem::RustCargo));
        assert!(probe.commands.iter().any(|(_, c)| c.contains("cargo test --workspace")));
        assert_eq!(probe.module_map.len(), 1);
        let m = &probe.module_map[0];
        assert_eq!(m.name, "foo");
        assert_eq!(m.description, "The foo crate");
        assert!(m.doc.contains("Foo does the thing"));
        assert_eq!(m.modules, vec!["bar".to_string(), "baz".to_string()]);
    }

    #[tokio::test]
    async fn probe_reads_readme_and_existing_instructions() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\nversion=\"0\"\n").await.unwrap();
        tokio::fs::write(dir.path().join("README.md"), "# X\n\nA project.\n").await.unwrap();
        tokio::fs::write(dir.path().join("CLAUDE.md"), "manual rules").await.unwrap();

        let probe = ProjectProbe::probe(dir.path()).await;
        assert!(probe.readme.contains("A project."));
        let (fname, content) = probe.existing_instructions.expect("existing instructions");
        assert_eq!(fname, "CLAUDE.md");
        assert_eq!(content, "manual rules");
    }

    #[tokio::test]
    async fn probe_inventories_docs() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\nversion=\"0\"\n").await.unwrap();
        tokio::fs::write(dir.path().join("ARCHITECTURE.md"), "design").await.unwrap();
        tokio::fs::create_dir_all(dir.path().join("docs")).await.unwrap();
        tokio::fs::write(dir.path().join("docs/guide.md"), "guide").await.unwrap();
        let probe = ProjectProbe::probe(dir.path()).await;
        assert!(probe.doc_links.contains(&"ARCHITECTURE.md".to_string()));
        assert!(probe.doc_links.contains(&"docs/guide.md".to_string()));
    }

    #[tokio::test]
    async fn context_block_is_bounded_and_labelled() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\nversion=\"0\"\ndescription=\"d\"\n").await.unwrap();
        let probe = ProjectProbe::probe(dir.path()).await;
        let block = probe.context_block();
        assert!(block.contains("## Project"));
        assert!(block.contains("name: x"));
        assert!(block.len() <= CONTEXT_BLOCK_CAP + 64);
    }

    #[tokio::test]
    async fn build_init_prompt_mentions_principles() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\nversion=\"0\"\n").await.unwrap();
        let probe = ProjectProbe::probe(dir.path()).await;
        let (system, user) = probe.build_init_prompt(ProjectInfoFormat::Oneai);
        assert!(system.contains("Link, don't embed"));
        assert!(system.contains("ONEAI.md"));
        assert!(user.contains("Architecture & key design"));
    }

    #[tokio::test]
    async fn heuristic_generate_into_temp_cargo_project() {
        let dir = tempfile::tempdir().unwrap();
        let cargo = r#"
[package]
name = "demo-pkg"
version = "0.1.0"
description = "A demo project"

[dependencies]
serde = "1.0"
tokio = { version = "1", features = ["full"] }
"#;
        tokio::fs::write(dir.path().join("Cargo.toml"), cargo).await.unwrap();

        let opts = ProjectInfoOptions::default();
        let res = generate_project_info(dir.path(), &opts).await.unwrap();

        assert_eq!(res.filename, "ONEAI.md");
        assert!(!res.skipped);
        assert!(!res.overwritten);
        assert!(!res.llm_generated);
        assert!(res.content.contains("demo-pkg"));
        assert!(res.content.contains("cargo build"));
        assert!(res.content.contains("serde"));
        assert!(res.content.contains("Build, test, run"));
        let on_disk = tokio::fs::read_to_string(dir.path().join("ONEAI.md")).await.unwrap();
        assert_eq!(on_disk, res.content);
    }

    #[tokio::test]
    async fn skip_when_exists_unless_force() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\nversion=\"0\"\n").await.unwrap();
        tokio::fs::write(dir.path().join("ONEAI.md"), "manual content").await.unwrap();

        let res = generate_project_info(dir.path(), &ProjectInfoOptions::default()).await.unwrap();
        assert!(res.skipped);
        let on_disk = tokio::fs::read_to_string(dir.path().join("ONEAI.md")).await.unwrap();
        assert_eq!(on_disk, "manual content");

        let opts = ProjectInfoOptions { force: true, ..Default::default() };
        let res = generate_project_info(dir.path(), &opts).await.unwrap();
        assert!(!res.skipped);
        assert!(res.overwritten);
        let on_disk = tokio::fs::read_to_string(dir.path().join("ONEAI.md")).await.unwrap();
        assert!(on_disk.contains("cargo build"));
    }

    #[tokio::test]
    async fn agents_and_claude_filenames() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("package.json"), r#"{"name":"jsproj","version":"1.0.0","dependencies":{"react":"^18"}}"#).await.unwrap();

        let opts = ProjectInfoOptions { format: ProjectInfoFormat::Agents, ..Default::default() };
        let res = generate_project_info(dir.path(), &opts).await.unwrap();
        assert_eq!(res.filename, "AGENTS.md");
        assert!(res.content.contains("jsproj"));
        assert!(res.content.contains("react"));

        let opts = ProjectInfoOptions { format: ProjectInfoFormat::Claude, ..Default::default() };
        let res = generate_project_info(dir.path(), &opts).await.unwrap();
        assert_eq!(res.filename, "CLAUDE.md");
    }

    #[tokio::test]
    async fn unknown_project_still_emits() {
        let dir = tempfile::tempdir().unwrap();
        let res = generate_project_info(dir.path(), &ProjectInfoOptions::default()).await.unwrap();
        assert!(res.content.contains("Overview"));
        assert!(res.content.contains("unrecognized"));
    }

    // ─── LLM-synthesis path (mock provider) ──────────────────────────────────

    use async_trait::async_trait;
    use futures::stream;
    use futures::Stream;
    use std::pin::Pin;
    use oneai_core::error::OneAIError;
    use oneai_core::types::{InferenceResponse, InferenceStreamChunk, ModelCapability, ModelConfig, TokenUsage};

    /// A minimal LlmProvider that returns a fixed markdown string for every infer().
    struct StubProvider {
        model_config: ModelConfig,
        reply: String,
    }

    #[async_trait]
    impl LlmProvider for StubProvider {
        async fn infer(&self, _req: InferenceRequest) -> std::result::Result<InferenceResponse, OneAIError> {
            Ok(InferenceResponse {
                message: Message::assistant(self.reply.clone()),
                usage: TokenUsage { prompt_tokens: 0, completion_tokens: 0, total_tokens: 0 },
                model: "stub".to_string(),
                metadata: std::collections::HashMap::new(),
            })
        }
        async fn infer_stream(
            &self,
            _req: InferenceRequest,
        ) -> std::result::Result<Pin<Box<dyn Stream<Item = InferenceStreamChunk> + Send>>, OneAIError> {
            Ok(Box::pin(stream::empty()))
        }
        fn capabilities(&self) -> ModelCapability { ModelCapability::gpt4_class() }
        fn config(&self) -> &ModelConfig { &self.model_config }
    }

    fn stub(reply: &str) -> StubProvider {
        StubProvider {
            model_config: ModelConfig::openai_compatible("k".into(), "http://x".into(), "stub-model".into()),
            reply: reply.to_string(),
        }
    }

    #[tokio::test]
    async fn llm_path_uses_model_output() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\nversion=\"0\"\n").await.unwrap();
        let provider = stub("# X\n\nSynthesized by the model.\n\n## Build\n\n```bash\ncargo build\n```\n");
        let opts = ProjectInfoOptions::default();
        let res = generate_project_info_with_llm(dir.path(), &opts, &provider).await.unwrap();
        assert!(res.llm_generated);
        assert!(!res.skipped);
        assert!(res.content.contains("Synthesized by the model"));
        // Code fence stripped (no leading ```markdown).
        assert!(!res.content.starts_with("```"));
        let on_disk = tokio::fs::read_to_string(dir.path().join("ONEAI.md")).await.unwrap();
        assert_eq!(on_disk, res.content);
    }

    #[tokio::test]
    async fn llm_path_strips_wrapping_fence() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\nversion=\"0\"\n").await.unwrap();
        let provider = stub("```markdown\n# X\nbody line\n```");
        let res = generate_project_info_with_llm(dir.path(), &opts_default(), &provider).await.unwrap();
        assert!(res.llm_generated);
        assert!(res.content.contains("body line"));
        assert!(!res.content.contains("```markdown"));
    }

    #[tokio::test]
    async fn llm_path_falls_back_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\nversion=\"0\"\n").await.unwrap();
        let provider = stub("   "); // whitespace-only → falls back to heuristic
        let res = generate_project_info_with_llm(dir.path(), &opts_default(), &provider).await.unwrap();
        assert!(!res.llm_generated);
        assert!(res.content.contains("cargo build")); // heuristic output
    }

    fn opts_default() -> ProjectInfoOptions { ProjectInfoOptions::default() }
}
