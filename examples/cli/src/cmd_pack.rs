//! DomainPack management commands.
//!
//! Subcommands for listing, inspecting, and installing domain packs.

use std::sync::Arc;
use oneai_domain::{DomainPack, coding_pack, research_pack};
use oneai_core::traits::Tool;

/// Builtin domain pack names and their descriptions.
pub const BUILTIN_PACKS: &[(&str, &str, &str)] = &[
    ("coding", "Software development — read, edit, search, shell tools", "8 tools: read_file, edit_file, shell, grep, glob, list_directory, notebook_edit, environment"),
    ("research", "Research & analysis — web search, read-only tools", "7 tools: web_search, web_fetch, read_file, grep, glob, list_directory, environment"),
    ("general", "General-purpose — minimal tool set", "1 tool: calculator"),
    ("writing", "Content creation & editing — coming soon", "planned: write, edit, review tools"),
    ("data", "Data analysis & visualization — coming soon", "planned: query, transform, plot tools"),
    ("devops", "Infrastructure & deployment — coming soon", "planned: deploy, monitor, rollback tools"),
];

/// Get a builtin DomainPack by name.
pub fn get_builtin_pack(name: &str, project_dir: &str) -> Option<DomainPack> {
    match name {
        "coding" => Some(coding_pack(project_dir)),
        "research" => Some(research_pack(project_dir)),
        "general" => Some(general_pack(project_dir)),
        _ => None,
    }
}

/// General-purpose domain pack — minimal tool set (just calculator).
fn general_pack(project_dir: &str) -> DomainPack {
    use std::collections::HashSet;
    use oneai_tool::CalculatorTool;
    use oneai_domain::permission_profile::PermissionProfile;
    use oneai_domain::compression_template::CompressionTemplate;
    use oneai_domain::paradigm_strategy::ParadigmStrategy;

    let mut profile = PermissionProfile::new("general");
    profile.auto_approve = HashSet::from(["calculator".to_string()]);

    let mut compression = CompressionTemplate::new("general");
    compression.preserve_fields = vec![
        "task_description".to_string(),
        "key_facts".to_string(),
        "user_preferences".to_string(),
    ];

    DomainPack {
        name: "general".to_string(),
        description: "General-purpose domain — minimal tool set for basic tasks".to_string(),
        tools: vec![Arc::new(CalculatorTool::new())],
        tool_decorators: vec![],
        context_sources: vec![
            Arc::new(oneai_domain::builtin_sources::DateSource::new()),
            Arc::new(oneai_domain::builtin_sources::EnvironmentInfoSource::new()),
        ],
        permission_profile: profile,
        paradigm_strategies: vec![
            ParadigmStrategy {
                trigger_pattern: "chat|help|answer".to_string(),
                paradigm_sequence: vec![oneai_domain::paradigm_strategy::DomainParadigmKind::ReAct],
                sub_agent_types: vec![],
                description: "General chat — answer questions directly".to_string(),
            },
        ],
        compression_template: compression,
        system_prompt_template: "You are a helpful general-purpose assistant. You have access to basic tools like a calculator. Answer questions directly and concisely.".to_string(),
        workflows: vec![],
        state_graphs: vec![],
        sub_agent_definitions: vec![],
    }
}

/// List all available domain packs (builtin + installed + project-level).
pub fn cmd_pack_list() {
    println!("📋 Available Domain Packs\n");

    // Builtin packs
    println!("  Built-in packs:");
    for (name, desc, tools) in BUILTIN_PACKS {
        let icon = if *name == "coding" || *name == "research" || *name == "general" {
            "✅"
        } else {
            "🔜"
        };
        println!("  {} {} — {}", icon, name, desc);
        println!("     {}", tools);
    }

    // Installed packs (from ~/.oneai/packs/)
    let packs_dir = super::config::OneaiConfig::packs_dir();
    if packs_dir.exists() {
        println!("\n  Installed packs:");
        if let Ok(entries) = std::fs::read_dir(&packs_dir) {
            for entry in entries.flatten() {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    let name = entry.file_name().to_string_lossy().to_string();
                    println!("  ✅ {} (installed)", name);
                }
            }
        }
    }

    // Project-level pack
    let project_files = ["ONEAI.domain.yaml", "ONEAI.domain.yml", "ONEAI.domain.toml"];
    for file in &project_files {
        if std::path::Path::new(file).exists() {
            println!("\n  Project-level pack:");
            println!("  ✅ {} (found in current directory)", file);
        }
    }

    println!("\nUse: oneai pack show <name> for details, oneai pack install <path|url> to install");
}

/// Show details of a specific domain pack.
pub fn cmd_pack_show(name: &str) {
    // Try builtin first
    if let Some(pack) = get_builtin_pack(name, ".") {
        print_pack_details(&pack);
        return;
    }

    // Try installed pack
    let packs_dir = super::config::OneaiConfig::packs_dir();
    let pack_dir = packs_dir.join(name);
    if pack_dir.exists() {
        // Try loading from file
        for file in &["ONEAI.domain.yaml", "ONEAI.domain.yml", "ONEAI.domain.toml"] {
            let config_path = pack_dir.join(file);
            if config_path.exists() {
                match oneai_domain::domain_pack_from_file(&config_path, ".") {
                    Ok(pack) => {
                        print_pack_details(&pack);
                        return;
                    }
                    Err(e) => {
                        eprintln!("Error loading pack from {}: {}", config_path.display(), e);
                        return;
                    }
                }
            }
        }
        eprintln!("Pack '{}' directory exists but no config file found.", name);
        return;
    }

    // Try project-level
    for file in &["ONEAI.domain.yaml", "ONEAI.domain.yml", "ONEAI.domain.toml"] {
        if std::path::Path::new(file).exists() {
            match oneai_domain::domain_pack_from_file(&std::path::PathBuf::from(file), ".") {
                Ok(pack) if pack.name == name => {
                    print_pack_details(&pack);
                    return;
                }
                _ => continue,
            }
        }
    }

    eprintln!("Pack '{}' not found. Use 'oneai pack list' to see available packs.", name);
}

/// Print detailed information about a DomainPack.
fn print_pack_details(pack: &DomainPack) {
    println!("📦 Domain Pack: {}\n", pack.name);
    println!("  Description: {}", pack.description);
    println!("  System prompt: \"{}...\"", &pack.system_prompt_template[..pack.system_prompt_template.len().min(200)]);
    println!();

    // Tools
    println!("  Tools ({}):", pack.tools.len());
    for tool in &pack.tools {
        println!("    • {} [risk: {:?}] — {}", tool.name(), tool.risk_level(), tool.description().chars().take(80).collect::<String>());
    }
    println!();

    // Permission profile
    println!("  Permission profile:");
    println!("    Auto-approve: {} tools", pack.permission_profile.auto_approve.len());
    println!("    Require confirmation: {} tools", pack.permission_profile.require_confirmation.len());
    println!("    Deny by default: {} patterns", pack.permission_profile.deny_by_default.len());
    println!();

    // Context sources
    println!("  Context sources ({}):", pack.context_sources.len());
    for source in &pack.context_sources {
        println!("    • {} (refresh: {:?})", source.key(), source.refresh_policy());
    }
    println!();

    // Paradigm strategies
    println!("  Paradigm strategies ({}):", pack.paradigm_strategies.len());
    for strategy in &pack.paradigm_strategies {
        println!("    • trigger: \"{}\" → {:?}", strategy.trigger_pattern, strategy.paradigm_sequence);
    }
    println!();

    // Compression template
    println!("  Compression template: {}", pack.compression_template.name);
    println!("    Preserve fields: {}", pack.compression_template.preserve_fields.join(", "));
    println!();

    // Workflows
    if !pack.workflows.is_empty() {
        println!("  Workflows ({}):", pack.workflows.len());
        for wf in &pack.workflows {
            println!("    • {} — {} ({} steps)", wf.name, wf.description, wf.steps.len());
        }
        println!();
    }

    // State graphs
    if !pack.state_graphs.is_empty() {
        println!("  StateGraphs ({}):", pack.state_graphs.len());
        for sg in &pack.state_graphs {
            println!("    • {} — {} nodes, {} edges", sg.name, sg.nodes.len(), sg.edges.len());
        }
    }
}

/// Install a domain pack from a local path or git URL.
pub fn cmd_pack_install(source: &str) {
    let packs_dir = super::config::OneaiConfig::packs_dir();

    // Ensure packs directory exists
    if let Err(e) = std::fs::create_dir_all(&packs_dir) {
        eprintln!("Error creating packs directory: {}", e);
        return;
    }

    // Check if source is a git URL
    if source.starts_with("https://") || source.starts_with("git@") || source.ends_with(".git") {
        // Extract pack name from git URL
        let pack_name = extract_pack_name_from_git_url(source);
        let dest = packs_dir.join(&pack_name);

        if dest.exists() {
            eprintln!("Pack '{}' already installed. Remove it first: rm -rf {}", pack_name, dest.display());
            return;
        }

        println!("Installing pack from git: {}...", source);
        let output = std::process::Command::new("git")
            .args(["clone", source, &dest.to_string_lossy()])
            .output();

        match output {
            Ok(output) if output.status.success() => {
                println!("✅ Pack '{}' installed from git.", pack_name);
                println!("   Location: {}", dest.display());
                println!("   Use: oneai chat --domain {}", pack_name);
            }
            Ok(output) => {
                eprintln!("❌ Git clone failed: {}", String::from_utf8_lossy(&output.stderr));
            }
            Err(e) => {
                eprintln!("❌ Failed to run git clone: {}", e);
            }
        }
        return;
    }

    // Local path
    let source_path = std::path::Path::new(source);
    if !source_path.exists() {
        eprintln!("Source path does not exist: {}", source);
        return;
    }

    // Determine pack name from directory name or config file
    let pack_name = if source_path.is_dir() {
        // Try loading config to get the pack name
        let config_result = oneai_domain::domain_pack_from_dir(&source_path.to_string_lossy());
        config_result.map(|p| p.name.clone())
            .unwrap_or_else(|_| source_path.file_name().unwrap().to_string_lossy().to_string())
    } else {
        // Single config file — use parent dir name or filename
        source_path.file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or("custom".to_string())
    };

    let dest = packs_dir.join(&pack_name);
    if dest.exists() {
        eprintln!("Pack '{}' already installed. Remove it first.", pack_name);
        return;
    }

    // Copy the directory or file to packs dir
    if source_path.is_dir() {
        if let Err(e) = copy_dir_recursive(source_path, &dest) {
            eprintln!("Error copying pack: {}", e);
            return;
        }
    } else {
        // Single file — create a directory and copy the file into it
        if let Err(e) = std::fs::create_dir_all(&dest) {
            eprintln!("Error creating pack directory: {}", e);
            return;
        }
        if let Err(e) = std::fs::copy(source_path, dest.join(source_path.file_name().unwrap())) {
            eprintln!("Error copying config file: {}", e);
            return;
        }
    }

    println!("✅ Pack '{}' installed from local path.", pack_name);
    println!("   Location: {}", dest.display());
    println!("   Use: oneai chat --domain {}", pack_name);
}

/// Extract a pack name from a git URL.
fn extract_pack_name_from_git_url(url: &str) -> String {
    // Extract the last segment of the URL path, removing .git suffix
    let url = url.trim_end_matches(".git");
    let path = url.rsplit('/').next().unwrap_or("custom-pack");
    path.to_string()
}

/// Recursively copy a directory.
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
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
