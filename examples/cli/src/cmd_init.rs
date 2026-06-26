//! `oneai init` — generate a project-instruction file (ONEAI.md / AGENTS.md / CLAUDE.md).
//!
//! Mirrors Claude Code's `/init` (→ CLAUDE.md) and OpenCode's `/init` (→ AGENTS.md):
//! a fresh checkout can bootstrap its own agent context. The generated file is
//! picked up automatically by `ProjectInstructionsSource` at session start.
//!
//! When an LLM provider is configured, the file is **LLM-synthesized** from a deep
//! codebase probe (README, crate docs + module map, manifest facts, conventions,
//! git context) following the `init` skill principles — link-don't-embed, minimal,
//! every line guides behavior. With `--no-llm` (or no provider), a deterministic
//! heuristic doc is written instead.

use std::path::PathBuf;

use oneai_domain::project_info::{
    generate_project_info, generate_project_info_with_llm, ProjectInfoFormat, ProjectInfoOptions,
};

use crate::config::OneaiConfig;

/// Entry point for `oneai init`.
///
/// `format` is the lowercase identifier (`oneai` / `agents` / `claude`); defaults
/// to `oneai`. `path` overrides the target project directory (defaults to `.`).
/// `force` overwrites an existing instruction file. `no_llm` forces the heuristic
/// composer even when a provider is configured.
pub fn cmd_init(
    config: &OneaiConfig,
    format: Option<&str>,
    path: Option<&str>,
    force: bool,
    no_llm: bool,
) {
    let format = match format {
        Some(f) => match ProjectInfoFormat::from_name(f) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("✗ {}", e);
                eprintln!("  Valid formats: oneai, agents, claude");
                return;
            }
        },
        None => ProjectInfoFormat::default(),
    };

    let project_dir = PathBuf::from(path.unwrap_or("."));
    let abs_dir = match std::fs::canonicalize(&project_dir) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("✗ Cannot resolve project directory '{}': {}", project_dir.display(), e);
            return;
        }
    };

    let opts = ProjectInfoOptions {
        format,
        force,
        ..ProjectInfoOptions::default()
    };

    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");

    // Prefer LLM synthesis when a provider is configured and not disabled.
    let use_llm = !no_llm && config.to_model_config().is_some();

    let result = if use_llm {
        println!("🔍 Probing project and synthesizing {} with the LLM…", format.filename());
        let model_config = config.to_model_config().expect("provider config checked above");
        let provider = oneai_provider::ProviderFactory::create(model_config);
        rt.block_on(generate_project_info_with_llm(&abs_dir, &opts, &*provider))
    } else {
        if no_llm {
            println!("⚙  Generating {} heuristically (--no-llm)…", format.filename());
        } else {
            println!("⚙  No LLM provider configured — generating {} heuristically.", format.filename());
            println!("   Configure ONEAI_API_KEY / ~/.oneai/config.toml for an LLM-synthesized doc.");
        }
        rt.block_on(generate_project_info(&abs_dir, &opts))
    };

    let res = match result {
        Ok(r) => r,
        Err(e) => {
            eprintln!("✗ Failed to generate {}: {}", opts.format.filename(), e);
            return;
        }
    };

    if res.skipped {
        println!("⊘ {} already exists — left untouched.", res.path.display());
        println!("  Re-run with --force to overwrite, or edit it directly:");
        println!("    {}", res.path.display());
        return;
    }

    if res.overwritten {
        println!("↻ Overwrote {}", res.path.display());
    } else {
        println!("✅ Created {}", res.path.display());
    }
    let mode = if res.llm_generated { "LLM-synthesized" } else { "heuristic" };
    println!();
    println!("Format: {} ({}) — {}", opts.format.label(), res.filename, mode);
    if use_llm && !res.llm_generated {
        println!("⚠  LLM synthesis failed (provider error) — wrote a heuristic doc instead.");
        println!("   Check ONEAI_API_KEY / base_url, or pass --no-llm to silence this.");
    }
    println!("It will be loaded automatically into agent context on the next session.");
    println!("Review and edit it to add project-specific conventions, constraints, and team preferences.");
}
