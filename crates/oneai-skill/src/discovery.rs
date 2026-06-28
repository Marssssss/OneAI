//! Skill discovery — convention-directory discovery of `SKILL.md` skills,
//! compatible with the Claude Code / OpenCode ecosystem.
//!
//! There is no install command and no remote download. Skills are plain files
//! dropped into well-known directories; OneAI discovers them on session start,
//! mirroring OpenCode's model:
//!
//! - **Project** dirs (walked up from cwd to the git worktree root):
//!   `.opencode/skills/`, `.oneai/skills/`, `.claude/skills/`, `.agents/skills/`
//! - **Global** dirs (under the home directory):
//!   `~/.config/opencode/skills/`, `~/.oneai/skills/`, `~/.claude/skills/`,
//!   `~/.agents/skills/`
//!
//! Each skill is a subdirectory `<name>/SKILL.md` (or `skill.yaml` / `skill.toml`
//! for OneAI-native packs). A bare skill file placed directly in a skills dir is
//! also accepted. Project-level skills override global ones with the same name.
//!
//! ## SKILL.md format
//!
//! YAML frontmatter (`name`, `description`, optional `trigger_keywords`) + a
//! markdown body that becomes `prompt_template`. `skill.yaml`/`skill.toml` mirror
//! [`SkillDescriptor`](oneai_core::SkillDescriptor) directly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use oneai_core::SkillDescriptor;
use serde::{Deserialize, Serialize};

// ─── Candidate skill files ──────────────────────────────────────────────────

/// File names probed inside a skill subdirectory, in priority order.
/// `SKILL.md` first — the ecosystem convention (Claude Code / OpenCode).
pub const SKILL_FILE_CANDIDATES: &[&str] = &[
    "SKILL.md",
    "skill.yaml",
    "skill.yml",
    "ONEAI.skill.yaml",
    "ONEAI.skill.yml",
    "skill.toml",
];

/// Project-level skill directories (relative to a project root). Walked up from
/// cwd to the git worktree root.
const PROJECT_SKILL_DIRS: &[&str] = &[
    ".opencode/skills",
    ".oneai/skills",
    ".claude/skills",
    ".agents/skills",
];

/// Global skill directories (relative to the user's home directory).
const GLOBAL_SKILL_DIRS: &[&str] = &[
    ".config/opencode/skills",
    ".oneai/skills",
    ".claude/skills",
    ".agents/skills",
];

// ─── SkillConfig ────────────────────────────────────────────────────────────

/// The on-disk skill config (mirrors [`SkillDescriptor`] minus the computed embedding).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SkillConfig {
    /// Unique skill name. May be empty in the file; falls back to the file/dir name.
    #[serde(default)]
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub prompt_template: String,
    #[serde(default)]
    pub trigger_keywords: Vec<String>,
}

impl From<SkillConfig> for SkillDescriptor {
    fn from(c: SkillConfig) -> Self {
        SkillDescriptor {
            name: c.name,
            description: c.description,
            prompt_template: c.prompt_template,
            trigger_keywords: c.trigger_keywords,
            embedding: None,
        }
    }
}

/// Parse a skill descriptor from raw file content.
///
/// `inferred_name` is used as a fallback when the file declares no name.
/// `ext` selects the parser: `yaml`/`yml`, `toml`, or `md` (SKILL.md frontmatter).
pub fn parse_skill_descriptor(inferred_name: &str, ext: &str, content: &str) -> Result<SkillDescriptor, SkillDiscoveryError> {
    let mut cfg = match ext {
        "yaml" | "yml" => serde_yaml::from_str::<SkillConfig>(content)
            .map_err(|e| SkillDiscoveryError::ParseFailed(format!("YAML parse error: {e}")))?,
        "toml" => toml::from_str::<SkillConfig>(content)
            .map_err(|e| SkillDiscoveryError::ParseFailed(format!("TOML parse error: {e}")))?,
        "md" => parse_skill_md(content)?,
        other => return Err(SkillDiscoveryError::ParseFailed(format!("unsupported skill file extension: {other}"))),
    };
    if cfg.name.trim().is_empty() {
        cfg.name = inferred_name.to_string();
    }
    Ok(cfg.into())
}

/// Parse an Anthropic-style `SKILL.md`: optional YAML frontmatter (delimited by
/// `---`) for metadata, with the body (after frontmatter) used as `prompt_template`.
fn parse_skill_md(content: &str) -> Result<SkillConfig, SkillDiscoveryError> {
    let trimmed = content.trim_start();
    let (frontmatter, body) = if trimmed.starts_with("---") {
        let after = &trimmed[3..];
        if let Some(end) = after.find("\n---") {
            let fm = &after[..end];
            let body_start = end + 4; // skip "\n---"
            let body = if body_start < after.len() { &after[body_start..] } else { "" };
            (Some(fm), body.trim_start())
        } else {
            (None, content)
        }
    } else {
        (None, content)
    };

    let mut cfg: SkillConfig = if let Some(fm) = frontmatter {
        serde_yaml::from_str(fm)
            .map_err(|e| SkillDiscoveryError::ParseFailed(format!("SKILL.md frontmatter parse error: {e}")))?
    } else {
        SkillConfig { name: String::new(), description: String::new(), prompt_template: String::new(), trigger_keywords: vec![] }
    };
    if cfg.prompt_template.is_empty() {
        cfg.prompt_template = body.to_string();
    }
    Ok(cfg)
}

/// Extension (without dot) for a skill file name, or `None` if unrecognized.
fn skill_file_ext(file_name: &str) -> Option<&'static str> {
    let lower = file_name.to_ascii_lowercase();
    if lower.ends_with(".md") { Some("md") }
    else if lower.ends_with(".yaml") { Some("yaml") }
    else if lower.ends_with(".yml") { Some("yml") }
    else if lower.ends_with(".toml") { Some("toml") }
    else { None }
}

// ─── Directory discovery ────────────────────────────────────────────────────

/// Default OneAI skills cache directory: `~/.oneai/skills/`.
///
/// Kept as one of the global discovery dirs for OneAI-native packs.
pub fn skills_dir() -> PathBuf {
    home_dir().join(".oneai").join("skills")
}

/// The user's home directory (falls back to `/tmp`).
fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Discover all skills from convention directories: global dirs first, then
/// project dirs walked up from cwd to the git worktree root. Project-level
/// skills override global ones with the same name.
pub fn discover_skills() -> Vec<SkillDescriptor> {
    let mut by_name: HashMap<String, SkillDescriptor> = HashMap::new();

    // Global first (lower precedence).
    let home = home_dir();
    for sub in GLOBAL_SKILL_DIRS {
        for skill in scan_skills_dir(&home.join(sub)) {
            by_name.insert(skill.name.clone(), skill);
        }
    }

    // Project-level (higher precedence — overrides global on name clash).
    for ancestor in project_ancestors() {
        for sub in PROJECT_SKILL_DIRS {
            for skill in scan_skills_dir(&ancestor.join(sub)) {
                by_name.insert(skill.name.clone(), skill);
            }
        }
    }

    let mut skills: Vec<SkillDescriptor> = by_name.into_values().collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// Find a single skill by name across the convention directories.
pub fn find_skill(name: &str) -> Option<SkillDescriptor> {
    discover_skills().into_iter().find(|s| s.name == name)
}

/// Walk from cwd up to (and including) the first ancestor containing `.git`,
/// collecting each directory along the way. Falls back to just cwd if cwd is
/// unreadable. Mirrors OpenCode's "walk up to the git worktree" behavior.
fn project_ancestors() -> Vec<PathBuf> {
    let mut dirs = vec![];
    let mut cwd = std::env::current_dir().ok();
    while let Some(d) = cwd {
        dirs.push(d.clone());
        if d.join(".git").exists() {
            break;
        }
        cwd = d.parent().map(|p| p.to_path_buf());
    }
    dirs
}

/// Scan a single skills directory: each subdirectory `<name>/SKILL.md` (or
/// `skill.yaml`/`.toml`) becomes a skill; a bare skill file placed directly in
/// the dir is also accepted. Unparseable entries are logged and skipped.
fn scan_skills_dir(dir: &Path) -> Vec<SkillDescriptor> {
    let mut out = vec![];
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            for cand in SKILL_FILE_CANDIDATES {
                let p = path.join(cand);
                if p.exists() {
                    if let Some(skill) = read_parse(&p, cand, &file_name) {
                        out.push(skill);
                    }
                    break;
                }
            }
        } else if let Some(ext) = skill_file_ext(&file_name) {
            if let Some(skill) = read_parse(&path, &file_name, &file_name) {
                let _ = ext; // ext already applied inside read_parse via cand
                out.push(skill);
            }
        }
    }
    out
}

/// Read a skill file and parse it. `cand` is the candidate name (to infer the
/// extension); `inferred_name` is the fallback skill name (dir/file name).
fn read_parse(path: &Path, cand: &str, inferred_name: &str) -> Option<SkillDescriptor> {
    let ext = skill_file_ext(cand)?;
    let content = std::fs::read_to_string(path).map_err(|e| {
        tracing::warn!("failed to read {}: {e}", path.display());
        e
    }).ok()?;
    let name = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| inferred_name.to_string());
    match parse_skill_descriptor(&name, ext, &content) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("failed to parse {}: {e}", path.display());
            None
        }
    }
}

// ─── Errors ─────────────────────────────────────────────────────────────────

/// Error during skill discovery/parsing.
#[derive(Debug)]
#[non_exhaustive]
pub enum SkillDiscoveryError {
    /// Skill file could not be parsed.
    ParseFailed(String),
}

impl std::fmt::Display for SkillDiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ParseFailed(s) => write!(f, "Parse failed: {s}"),
        }
    }
}

impl std::error::Error for SkillDiscoveryError {}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_yaml_skill() {
        let yaml = "name: pdf\ndescription: d\nprompt_template: hi\ntrigger_keywords: [a]\n";
        let s = parse_skill_descriptor("fb", "yaml", yaml).unwrap();
        assert_eq!(s.name, "pdf");
        assert_eq!(s.trigger_keywords, vec!["a"]);
    }

    #[test]
    fn parse_skill_md_frontmatter() {
        let md = "---\nname: coder\ndescription: A coding skill\ntrigger_keywords: [code]\n---\nYou are a coding expert.\nDo good work.\n";
        let s = parse_skill_descriptor("fb", "md", md).unwrap();
        assert_eq!(s.name, "coder");
        assert!(s.prompt_template.contains("You are a coding expert."));
        assert!(s.prompt_template.contains("Do good work."));
    }

    #[test]
    fn parse_skill_md_no_frontmatter() {
        let md = "Just a body with no frontmatter.";
        let s = parse_skill_descriptor("fromfile", "md", md).unwrap();
        assert_eq!(s.name, "fromfile");
        assert_eq!(s.prompt_template, "Just a body with no frontmatter.");
    }

    #[test]
    fn scan_dir_discovers_subdir_skill() {
        let tmp = tmp_dir();
        let skill_dir = tmp.join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: demo\n---\nBody.\n",
        ).unwrap();
        let found = scan_skills_dir(&tmp);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "my-skill");
        assert_eq!(found[0].description, "demo");
        assert!(found[0].prompt_template.contains("Body."));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn scan_dir_discovers_bare_file() {
        let tmp = tmp_dir();
        std::fs::write(
            tmp.join("standalone.yaml"),
            "name: standalone\ndescription: x\nprompt_template: y\n",
        ).unwrap();
        let found = scan_skills_dir(&tmp);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "standalone");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn project_overrides_global_on_name_clash() {
        // Both a global and a project skill named "dup"; project wins.
        let tmp = tmp_dir();
        let g = tmp.join("global/.claude/skills/dup");
        let p = tmp.join("proj/.claude/skills/dup");
        std::fs::create_dir_all(&g).unwrap();
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(g.join("SKILL.md"), "---\nname: dup\ndescription: GLOBAL\n---\ng\n").unwrap();
        std::fs::write(p.join("SKILL.md"), "---\nname: dup\ndescription: PROJECT\n---\np\n").unwrap();
        // Build a merged map the same way discover_skills does (sans home/cwd).
        let mut map = HashMap::new();
        for s in scan_skills_dir(&tmp.join("global/.claude/skills")) { map.insert(s.name.clone(), s); }
        for s in scan_skills_dir(&tmp.join("proj/.claude/skills")) { map.insert(s.name.clone(), s); }
        assert_eq!(map.get("dup").unwrap().description, "PROJECT");
        std::fs::remove_dir_all(&tmp).ok();
    }

    fn tmp_dir() -> PathBuf {
        let name = std::thread::current().name().unwrap_or("test").to_string();
        let mut h: u64 = 1469598103934665603;
        for b in name.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(1099511628211);
        }
        let p = std::env::temp_dir().join(format!("oneai-skill-disc-{h:x}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
