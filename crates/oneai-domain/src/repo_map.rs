//! RepoMap — structural codebase summary as a ContextSource.
//!
//! Provides a compact hierarchical summary of the project's code structure,
//! extracting key symbols (functions, structs, classes, enums) from source
//! files. This is the OneAI equivalent of Claude Code's repo map feature.
//!
//! Uses regex-based extraction for maximum portability (no C/tree-sitter deps
//! needed, works on Android/iOS/HarmonyOS cross-platform targets).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use oneai_core::error::Result;
use regex::Regex;
use tokio::sync::RwLock;

use crate::context_source::{ContextSource, RefreshPolicy};

// ─── Language Detection ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Language {
    Rust, Python, JavaScript, TypeScript, Go, Java,
    C, Cpp, Kotlin, Swift, Ruby, Php, Shell, Unknown,
}

impl Language {
    fn from_ext(ext: &str) -> Self {
        match ext {
            "rs" => Self::Rust,
            "py" | "pyi" => Self::Python,
            "js" | "jsx" | "mjs" => Self::JavaScript,
            "ts" | "tsx" => Self::TypeScript,
            "go" => Self::Go,
            "java" => Self::Java,
            "c" | "h" => Self::C,
            "cpp" | "hpp" => Self::Cpp,
            "kt" => Self::Kotlin,
            "swift" => Self::Swift,
            "rb" => Self::Ruby,
            "php" => Self::Php,
            "sh" | "bash" => Self::Shell,
            _ => Self::Unknown,
        }
    }
}

// ─── Symbol Types ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Symbol {
    kind: SymKind,
    name: String,
    sig: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SymKind {
    Fn, Struct, Enum, Class, Interface, Trait,
    Module, Import, Const, Type, Method, Field,
}

impl SymKind {
    fn tag(&self) -> &'static str {
        match self {
            Self::Fn => "fn",
            Self::Method => "fn",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Class => "class",
            Self::Interface => "interface",
            Self::Trait => "trait",
            Self::Module => "mod",
            Self::Import => "import",
            Self::Const => "const",
            Self::Type => "type",
            Self::Field => "field",
        }
    }
}

// ─── Config ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RepoMapConfig {
    pub max_chars: usize,
    pub max_files: usize,
    pub max_syms_per_file: usize,
    pub skip_dirs: Vec<String>,
    pub include_imports: bool,
}

impl Default for RepoMapConfig {
    fn default() -> Self {
        Self {
            max_chars: 8000,
            max_files: 200,
            max_syms_per_file: 30,
            skip_dirs: vec![
                "target".into(), "node_modules".into(), ".git".into(), "dist".into(), "build".into(),
                "__pycache__".into(), ".venv".into(), "vendor".into(), "out".into(), ".next".into(),
                ".cache".into(), ".cargo".into(), "debug".into(), "release".into(),
            ],
            include_imports: true,
        }
    }
}

// ─── Regex Symbol Extraction ────────────────────────────────────────────────

fn extract_syms(content: &str, lang: Language) -> Vec<Symbol> {
    match lang {
        Language::Rust => rust_syms(content),
        Language::Python => python_syms(content),
        Language::JavaScript | Language::TypeScript => js_syms(content),
        Language::Go => go_syms(content),
        Language::Java => java_syms(content),
        Language::C | Language::Cpp => c_syms(content),
        Language::Kotlin => kotlin_syms(content),
        Language::Swift => swift_syms(content),
        Language::Ruby => ruby_syms(content),
        Language::Php => php_syms(content),
        Language::Shell => shell_syms(content),
        Language::Unknown => vec![],
    }
}

fn rust_syms(code: &str) -> Vec<Symbol> {
    let mut out = Vec::new();
    let fn_r = Regex::new(r"fn\s+(\w+)").unwrap();
    let st_r = Regex::new(r"struct\s+(\w+)").unwrap();
    let en_r = Regex::new(r"enum\s+(\w+)").unwrap();
    let tr_r = Regex::new(r"trait\s+(\w+)").unwrap();
    let mo_r = Regex::new(r"mod\s+(\w+)").unwrap();
    let cn_r = Regex::new(r"const\s+(\w+)").unwrap();
    let ty_r = Regex::new(r"type\s+(\w+)").unwrap();

    for line in code.lines() {
        let t = line.trim();
        if t.starts_with("//") || t.is_empty() { continue; }
        if let Some(c) = fn_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Fn, name: n.into(), sig: fmt_sig("fn", n) });
        } else if let Some(c) = st_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Struct, name: n.into(), sig: fmt_sig("struct", n) });
        } else if let Some(c) = en_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Enum, name: n.into(), sig: fmt_sig("enum", n) });
        } else if let Some(c) = tr_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Trait, name: n.into(), sig: fmt_sig("trait", n) });
        } else if let Some(c) = mo_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Module, name: n.into(), sig: fmt_sig("mod", n) });
        } else if let Some(c) = cn_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Const, name: n.into(), sig: fmt_sig("const", n) });
        } else if let Some(c) = ty_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Type, name: n.into(), sig: fmt_sig("type", n) });
        }
    }
    out
}

fn python_syms(code: &str) -> Vec<Symbol> {
    let mut out = Vec::new();
    let cl_r = Regex::new(r"class\s+(\w+)").unwrap();
    let fn_r = Regex::new(r"def\s+(\w+)").unwrap();

    for line in code.lines() {
        let t = line.trim();
        if t.starts_with('#') || t.is_empty() { continue; }
        if let Some(c) = cl_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Class, name: n.into(), sig: fmt_sig("class", n) });
        } else if let Some(c) = fn_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Fn, name: n.into(), sig: fmt_sig("def", n) });
        }
    }
    out
}

fn js_syms(code: &str) -> Vec<Symbol> {
    let mut out = Vec::new();
    let fn_r = Regex::new(r"function\s+(\w+)").unwrap();
    let cl_r = Regex::new(r"class\s+(\w+)").unwrap();
    let if_r = Regex::new(r"interface\s+(\w+)").unwrap();
    let en_r = Regex::new(r"enum\s+(\w+)").unwrap();
    let ar_r = Regex::new(r"const\s+(\w+)\s*=").unwrap();

    for line in code.lines() {
        let t = line.trim();
        if t.starts_with("//") || t.is_empty() { continue; }
        if let Some(c) = fn_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Fn, name: n.into(), sig: fmt_sig("function", n) });
        } else if let Some(c) = cl_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Class, name: n.into(), sig: fmt_sig("class", n) });
        } else if let Some(c) = if_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Interface, name: n.into(), sig: fmt_sig("interface", n) });
        } else if let Some(c) = en_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Enum, name: n.into(), sig: fmt_sig("enum", n) });
        } else if let Some(c) = ar_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Fn, name: n.into(), sig: fmt_sig("const", n) });
        }
    }
    out
}

fn go_syms(code: &str) -> Vec<Symbol> {
    let mut out = Vec::new();
    let fn_r = Regex::new(r"func\s+(\w+)").unwrap();
    let st_r = Regex::new(r"type\s+(\w+)\s+struct").unwrap();
    let if_r = Regex::new(r"type\s+(\w+)\s+interface").unwrap();
    let pk_r = Regex::new(r"package\s+(\w+)").unwrap();

    for line in code.lines() {
        let t = line.trim();
        if t.starts_with("//") || t.is_empty() { continue; }
        if let Some(c) = fn_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Fn, name: n.into(), sig: fmt_sig("func", n) });
        } else if let Some(c) = st_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Struct, name: n.into(), sig: fmt_sig("type struct", n) });
        } else if let Some(c) = if_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Interface, name: n.into(), sig: fmt_sig("type interface", n) });
        } else if let Some(c) = pk_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Module, name: n.into(), sig: fmt_sig("package", n) });
        }
    }
    out
}

fn java_syms(code: &str) -> Vec<Symbol> {
    let mut out = Vec::new();
    let cl_r = Regex::new(r"class\s+(\w+)").unwrap();
    let if_r = Regex::new(r"interface\s+(\w+)").unwrap();
    let en_r = Regex::new(r"enum\s+(\w+)").unwrap();

    for line in code.lines() {
        let t = line.trim();
        if t.starts_with("//") || t.is_empty() { continue; }
        if let Some(c) = cl_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Class, name: n.into(), sig: fmt_sig("class", n) });
        } else if let Some(c) = if_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Interface, name: n.into(), sig: fmt_sig("interface", n) });
        } else if let Some(c) = en_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Enum, name: n.into(), sig: fmt_sig("enum", n) });
        }
    }
    out
}

fn c_syms(code: &str) -> Vec<Symbol> {
    let mut out = Vec::new();
    let st_r = Regex::new(r"struct\s+(\w+)").unwrap();
    let en_r = Regex::new(r"enum\s+(\w+)").unwrap();
    let ma_r = Regex::new(r"#define\s+(\w+)").unwrap();

    for line in code.lines() {
        let t = line.trim();
        if t.starts_with("//") || t.is_empty() { continue; }
        if let Some(c) = st_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Struct, name: n.into(), sig: fmt_sig("struct", n) });
        } else if let Some(c) = en_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Enum, name: n.into(), sig: fmt_sig("enum", n) });
        } else if let Some(c) = ma_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Const, name: n.into(), sig: fmt_sig("#define", n) });
        }
    }
    out
}

fn kotlin_syms(code: &str) -> Vec<Symbol> {
    let mut out = Vec::new();
    let fn_r = Regex::new(r"fun\s+(\w+)").unwrap();
    let cl_r = Regex::new(r"class\s+(\w+)").unwrap();
    let ob_r = Regex::new(r"object\s+(\w+)").unwrap();

    for line in code.lines() {
        let t = line.trim();
        if t.starts_with("//") || t.is_empty() { continue; }
        if let Some(c) = fn_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Fn, name: n.into(), sig: fmt_sig("fun", n) });
        } else if let Some(c) = cl_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Class, name: n.into(), sig: fmt_sig("class", n) });
        } else if let Some(c) = ob_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Struct, name: n.into(), sig: fmt_sig("object", n) });
        }
    }
    out
}

fn swift_syms(code: &str) -> Vec<Symbol> {
    let mut out = Vec::new();
    let fn_r = Regex::new(r"func\s+(\w+)").unwrap();
    let cl_r = Regex::new(r"class\s+(\w+)").unwrap();
    let st_r = Regex::new(r"struct\s+(\w+)").unwrap();
    let pr_r = Regex::new(r"protocol\s+(\w+)").unwrap();

    for line in code.lines() {
        let t = line.trim();
        if t.starts_with("//") || t.is_empty() { continue; }
        if let Some(c) = fn_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Fn, name: n.into(), sig: fmt_sig("func", n) });
        } else if let Some(c) = cl_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Class, name: n.into(), sig: fmt_sig("class", n) });
        } else if let Some(c) = st_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Struct, name: n.into(), sig: fmt_sig("struct", n) });
        } else if let Some(c) = pr_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Trait, name: n.into(), sig: fmt_sig("protocol", n) });
        }
    }
    out
}

fn ruby_syms(code: &str) -> Vec<Symbol> {
    let mut out = Vec::new();
    let cl_r = Regex::new(r"class\s+(\w+)").unwrap();
    let mo_r = Regex::new(r"module\s+(\w+)").unwrap();
    let fn_r = Regex::new(r"def\s+(\w+)").unwrap();

    for line in code.lines() {
        let t = line.trim();
        if t.starts_with('#') || t.is_empty() { continue; }
        if let Some(c) = cl_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Class, name: n.into(), sig: fmt_sig("class", n) });
        } else if let Some(c) = mo_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Module, name: n.into(), sig: fmt_sig("module", n) });
        } else if let Some(c) = fn_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Method, name: n.into(), sig: fmt_sig("def", n) });
        }
    }
    out
}

fn php_syms(code: &str) -> Vec<Symbol> {
    let mut out = Vec::new();
    let cl_r = Regex::new(r"class\s+(\w+)").unwrap();
    let fn_r = Regex::new(r"function\s+(\w+)").unwrap();

    for line in code.lines() {
        let t = line.trim();
        if t.starts_with("//") || t.is_empty() { continue; }
        if let Some(c) = cl_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Class, name: n.into(), sig: fmt_sig("class", n) });
        } else if let Some(c) = fn_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Fn, name: n.into(), sig: fmt_sig("function", n) });
        }
    }
    out
}

fn shell_syms(code: &str) -> Vec<Symbol> {
    let mut out = Vec::new();
    let fn_r = Regex::new(r"function\s+(\w+)").unwrap();

    for line in code.lines() {
        let t = line.trim();
        if t.starts_with('#') || t.is_empty() { continue; }
        if let Some(c) = fn_r.captures(t) {
            let n = c.get(1).unwrap().as_str();
            out.push(Symbol { kind: SymKind::Fn, name: n.into(), sig: fmt_sig("function", n) });
        }
    }
    out
}

fn fmt_sig(prefix: &str, name: &str) -> String {
    format!("{} {}", prefix, name)
}

// ─── RepoMap Builder ────────────────────────────────────────────────────────

async fn build_repo_map(project_dir: &Path, config: &RepoMapConfig) -> String {
    let mut entries: Vec<(String, Vec<Symbol>)> = Vec::new();
    let mut total = 0;

    let files = collect_source_files(project_dir, &config.skip_dirs);

    for path in &files {
        if entries.len() >= config.max_files { break; }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let lang = Language::from_ext(ext);
        if lang == Language::Unknown { continue; }

        if let Ok(content) = tokio::fs::read_to_string(path).await {
            let syms: Vec<Symbol> = extract_syms(&content, lang)
                .into_iter()
                .filter(|s| config.include_imports || s.kind != SymKind::Import)
                .take(config.max_syms_per_file)
                .collect();
            if syms.is_empty() { continue; }

            let rel = path.strip_prefix(project_dir)
                .unwrap_or(path).to_string_lossy().to_string();
            let est = rel.len() + syms.len() * 30 + 20;
            if total + est > config.max_chars { break; }
            total += est;
            entries.push((rel, syms));
        }
    }
    format_map(&entries)
}

fn format_map(entries: &[(String, Vec<Symbol>)]) -> String {
    let mut out = String::from("RepoMap - project code structure:\n\n");
    for (path, syms) in entries {
        out.push_str(&format!("{}:\n", path));
        for s in syms {
            out.push_str(&format!("  {} {}\n", s.kind.tag(), s.sig));
        }
        out.push_str("\n");
    }
    out
}

fn collect_source_files(dir: &Path, skip: &[String]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    walk_dir(dir, dir, skip, &mut files);
    files.sort_by_key(|f| f.to_string_lossy().to_string());
    files
}

fn walk_dir(base: &Path, cur: &Path, skip: &[String], out: &mut Vec<PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(cur) {
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') { continue; }
            if p.is_dir() {
                if skip.iter().any(|s| name == *s) { continue; }
                walk_dir(base, &p, skip, out);
            } else if p.is_file() {
                let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
                if Language::from_ext(ext) != Language::Unknown {
                    out.push(p);
                }
            }
        }
    }
}

// ─── RepoMapSource ──────────────────────────────────────────────────────────

pub struct RepoMapSource {
    project_dir: PathBuf,
    config: RepoMapConfig,
    last_content: Arc<RwLock<Option<String>>>,
}

impl RepoMapSource {
    pub fn new(project_dir: &str) -> Self {
        Self {
            project_dir: PathBuf::from(project_dir),
            config: RepoMapConfig::default(),
            last_content: Arc::new(RwLock::new(None)),
        }
    }

    pub fn with_config(project_dir: &str, config: RepoMapConfig) -> Self {
        Self {
            project_dir: PathBuf::from(project_dir),
            config,
            last_content: Arc::new(RwLock::new(None)),
        }
    }
}

#[async_trait]
impl ContextSource for RepoMapSource {
    fn key(&self) -> &str { "repo_map" }
    async fn load(&self) -> Result<String> {
        let content = build_repo_map(&self.project_dir, &self.config).await;
        *self.last_content.write().await = Some(content.clone());
        Ok(content)
    }
    fn refresh_policy(&self) -> RefreshPolicy { RefreshPolicy::OnChange }
    fn priority(&self) -> u32 { 8 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_language_detection() {
        assert_eq!(Language::from_ext("rs"), Language::Rust);
        assert_eq!(Language::from_ext("py"), Language::Python);
        assert_eq!(Language::from_ext("ts"), Language::TypeScript);
        assert_eq!(Language::from_ext("go"), Language::Go);
        assert_eq!(Language::from_ext("txt"), Language::Unknown);
    }

    #[test]
    fn test_rust_syms() {
        let code = "pub struct AppConfig { }\npub enum DecisionKind { }\nfn run_agent() { }\nmod cli;\nconst MAX: usize = 50;";
        let syms = rust_syms(code);
        assert!(syms.iter().any(|s| s.name == "AppConfig" && s.kind == SymKind::Struct));
        assert!(syms.iter().any(|s| s.name == "DecisionKind" && s.kind == SymKind::Enum));
        assert!(syms.iter().any(|s| s.name == "run_agent" && s.kind == SymKind::Fn));
        assert!(syms.iter().any(|s| s.name == "cli" && s.kind == SymKind::Module));
        assert!(syms.iter().any(|s| s.name == "MAX" && s.kind == SymKind::Const));
    }

    #[test]
    fn test_python_syms() {
        let code = "class AgentConfig:\n    def __init__(self):\n        pass";
        let syms = python_syms(code);
        assert!(syms.iter().any(|s| s.name == "AgentConfig" && s.kind == SymKind::Class));
        assert!(syms.iter().any(|s| s.name == "__init__" && s.kind == SymKind::Fn));
    }

    #[test]
    fn test_js_syms() {
        let code = "class AgentLoop { }\nfunction createAgent() { }\nconst runTask = ...";
        let syms = js_syms(code);
        assert!(syms.iter().any(|s| s.name == "AgentLoop" && s.kind == SymKind::Class));
        assert!(syms.iter().any(|s| s.name == "createAgent" && s.kind == SymKind::Fn));
    }

    #[test]
    fn test_go_syms() {
        let code = "package agent\nfunc RunLoop() { }\ntype Config struct { }";
        let syms = go_syms(code);
        assert!(syms.iter().any(|s| s.name == "RunLoop" && s.kind == SymKind::Fn));
        assert!(syms.iter().any(|s| s.name == "Config" && s.kind == SymKind::Struct));
    }

    #[test]
    fn test_java_syms() {
        let code = "class AgentLoop { }\ninterface Provider { }";
        let syms = java_syms(code);
        assert!(syms.iter().any(|s| s.name == "AgentLoop" && s.kind == SymKind::Class));
        assert!(syms.iter().any(|s| s.name == "Provider" && s.kind == SymKind::Interface));
    }

    #[test]
    fn test_format_map() {
        let entries = vec![
            ("src/main.rs".into(), vec![
                Symbol { kind: SymKind::Fn, name: "main".into(), sig: "fn main".into() },
                Symbol { kind: SymKind::Struct, name: "Config".into(), sig: "struct Config".into() },
            ]),
        ];
        let out = format_map(&entries);
        assert!(out.contains("src/main.rs"));
        assert!(out.contains("fn main"));
        assert!(out.contains("struct Config"));
    }

    #[tokio::test]
    async fn test_repo_map_source() {
        let src = RepoMapSource::new("/Users/maxf/github/new/OneAI");
        assert_eq!(src.key(), "repo_map");
        assert_eq!(src.priority(), 8);
        assert_eq!(src.refresh_policy(), RefreshPolicy::OnChange);

        let content = src.load().await.unwrap();
        assert!(content.contains("RepoMap"));
    }

    #[test]
    fn test_config_defaults() {
        let cfg = RepoMapConfig::default();
        assert_eq!(cfg.max_chars, 8000);
        assert_eq!(cfg.max_files, 200);
        assert!(cfg.skip_dirs.contains(&"target".to_string()));
    }
}
