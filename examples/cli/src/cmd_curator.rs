//! `oneai curator` — skill lifecycle stewardship (Phase 2.1 Stage B).
//!
//! Runs the closed-loop skill curator over the live skill registry:
//!
//! - `oneai curator status` — per-skill state / use_count / pinned / author.
//! - `oneai curator run`     — apply automatic `Active → Stale → Archived`
//!   transitions (writes a backup before any retirement), then print a report.
//! - `oneai curator pin <name>`    — pin a skill (exempt from auto-retirement).
//! - `oneai curator unpin <name>`  — unpin.
//! - `oneai curator archive <name>`— manually retire (reversible; backup first).
//! - `oneai curator restore <name>`— restore an archived skill.
//! - `oneai curator backup`  — write a restorable snapshot of every skill.
//! - `oneai curator backups` — list available backup snapshot ids.
//! - `oneai curator rollback <id>` — restore a backup snapshot (skills + metadata).
//! - `oneai curator consolidate` — LLM consolidation pass (Stage C, default-off
//!   / opt-in). Merge narrow one-session skills into class-level umbrella
//!   skills. Unlike the other actions this needs a configured LLM provider.
//!
//! Mirrors the `pack` subcommand pattern. The pure-function actions build a
//! provider-less App (the curator needs no LLM — only the SkillRegistry +
//! SkillMetadataStore + the pack's `skill_lifecycle` policy). Discovered +
//! builtin skills are loaded so the curator sees the same library the agent
//! sees.

use oneai_app::AppBuilder;
use oneai_domain::coding_pack;

use crate::cmd_pack::get_builtin_pack;
use crate::config::OneaiConfig;

/// Build a provider-less App with the skill curator wired (domain pack
/// supplies the `skill_lifecycle` policy; discovered + builtin skills loaded).
async fn build_curator_app(config: &OneaiConfig, domain_override: Option<&str>) -> oneai_app::App {
    let domain_name = config.default_domain_pack(domain_override);
    let pack = get_builtin_pack(&domain_name, ".").unwrap_or_else(|| coding_pack("."));
    // Pass the pack through `.domain_pack(...)` so its `skill_lifecycle`
    // policy drives the store/curator (coding → 30d/90d, assistant → 60d/180d).
    let app = AppBuilder::new()
        .noop_interaction_gate()
        .default_parser()
        .generation_config(config.generation.clone())
        .domain_pack(pack)
        .build()
        .await
        .expect("curator App build failed");
    // Load the same skill library the agent sees: discovered convention-dir
    // skills + builtin domain skills.
    let skills = oneai_skill::builtin::skills_for_domain(&domain_name);
    app.skill_registry.register_builtin(skills).await.unwrap();
    app
}

/// `oneai curator status` — per-skill lifecycle table.
pub async fn cmd_curator_status(config: &OneaiConfig, domain: Option<&str>) {
    let app = build_curator_app(config, domain).await;
    let curator = match app.skill_curator.as_ref() {
        Some(c) => c,
        None => {
            eprintln!("curator unavailable (no skill_lifecycle policy).");
            return;
        }
    };
    let rows = curator.status().await;
    if rows.is_empty() {
        println!("🧩 No skills registered.");
        return;
    }
    println!("🧩 Skill lifecycle ({})\n", rows.len());
    println!(
        "  {:<22} {:<9} {:>9} {:<6} {:<8}",
        "skill", "state", "use_count", "pinned", "author"
    );
    for (s, m) in rows {
        println!(
            "  {:<22} {:<9} {:>9} {:<6} {:<8}",
            s.name,
            format!("{:?}", m.state).to_lowercase(),
            m.use_count,
            if m.pinned { "yes" } else { "no" },
            format!("{:?}", m.created_by).to_lowercase(),
        );
    }
    println!("\nStorage: {}", curator.store().root().display());
}

/// `oneai curator run` — apply automatic transitions + report.
pub async fn cmd_curator_run(config: &OneaiConfig, domain: Option<&str>) {
    let app = build_curator_app(config, domain).await;
    let curator = match app.skill_curator.as_ref() {
        Some(c) => c,
        None => {
            eprintln!("curator unavailable (no skill_lifecycle policy).");
            return;
        }
    };
    let report = curator.run(std::time::SystemTime::now()).await;
    println!("🧹 Curator run complete.\n");
    if report.backup_written {
        println!("  ✅ backup snapshot written (restorable)");
    }
    if !report.gone_stale.is_empty() {
        println!("  🟡 gone stale: {}", report.gone_stale.join(", "));
    }
    if !report.archived.is_empty() {
        println!(
            "  📦 archived: {} (restorable via `oneai curator restore <name>`)",
            report.archived.join(", ")
        );
    }
    if report.gone_stale.is_empty() && report.archived.is_empty() {
        println!("  (no skills aged this run)");
    }
}

/// `oneai curator pin <name>`.
pub async fn cmd_curator_pin(config: &OneaiConfig, domain: Option<&str>, name: &str, pin: bool) {
    let app = build_curator_app(config, domain).await;
    let curator = match app.skill_curator.as_ref() {
        Some(c) => c,
        None => {
            eprintln!("curator unavailable.");
            return;
        }
    };
    if curator.registry().find_by_name(name).await.is_none() {
        eprintln!("skill '{name}' not registered. Use `oneai curator status` to list.");
        std::process::exit(1);
    }
    let m = if pin {
        curator.pin(name).await
    } else {
        curator.unpin(name).await
    };
    println!(
        "📌 skill `{name}` pinned={} (state={:?})",
        m.pinned, m.state
    );
}

/// `oneai curator archive <name>` — manual retirement (reversible).
pub async fn cmd_curator_archive(config: &OneaiConfig, domain: Option<&str>, name: &str) {
    let app = build_curator_app(config, domain).await;
    let curator = match app.skill_curator.as_ref() {
        Some(c) => c,
        None => {
            eprintln!("curator unavailable.");
            return;
        }
    };
    if curator.registry().find_by_name(name).await.is_none() {
        eprintln!("skill '{name}' not registered.");
        std::process::exit(1);
    }
    let m = curator.archive(name).await;
    println!(
        "📦 skill `{name}` archived (state={:?}). Restore with `oneai curator restore {name}`.",
        m.state
    );
}

/// `oneai curator restore <name>`.
pub async fn cmd_curator_restore(config: &OneaiConfig, domain: Option<&str>, name: &str) {
    let app = build_curator_app(config, domain).await;
    let curator = match app.skill_curator.as_ref() {
        Some(c) => c,
        None => {
            eprintln!("curator unavailable.");
            return;
        }
    };
    let m = curator.restore(name).await;
    println!("✅ skill `{name}` restored (state={:?}).", m.state);
}

/// `oneai curator backup` — write a restorable snapshot.
pub async fn cmd_curator_backup(config: &OneaiConfig, domain: Option<&str>) {
    let app = build_curator_app(config, domain).await;
    let curator = match app.skill_curator.as_ref() {
        Some(c) => c,
        None => {
            eprintln!("curator unavailable.");
            return;
        }
    };
    let id = curator.backup().await;
    println!("💾 backup snapshot written (id={id}).");
    println!("   restore with `oneai curator rollback {id}`.");
}

/// `oneai curator backups` — list available snapshot ids.
pub async fn cmd_curator_backups(config: &OneaiConfig, domain: Option<&str>) {
    let app = build_curator_app(config, domain).await;
    let curator = match app.skill_curator.as_ref() {
        Some(c) => c,
        None => {
            eprintln!("curator unavailable.");
            return;
        }
    };
    let ids = curator.list_backups();
    if ids.is_empty() {
        println!("(no backups yet — run `oneai curator backup` or `... run`)");
        return;
    }
    println!("💾 backup snapshots (newest first):");
    for id in ids {
        println!("  {id}");
    }
}

/// `oneai curator rollback <id>` — restore a backup snapshot.
pub async fn cmd_curator_rollback(config: &OneaiConfig, domain: Option<&str>, id: u64) {
    let app = build_curator_app(config, domain).await;
    let curator = match app.skill_curator.as_ref() {
        Some(c) => c,
        None => {
            eprintln!("curator unavailable.");
            return;
        }
    };
    // Restore skill files into the global skills dir so discovery re-finds them.
    let restore_dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join(".oneai")
        .join("skills");
    match curator.rollback(id, &restore_dir).await {
        Ok(n) => println!("✅ restored {n} skill(s) + metadata from snapshot {id}."),
        Err(e) => {
            eprintln!("rollback failed: {e}");
            std::process::exit(1);
        }
    }
}

/// `oneai curator consolidate` — LLM consolidation pass (Stage C, default-off
/// / opt-in). Unlike the other curator actions, this needs a configured LLM
/// provider (the model proposes the umbrella merges). Builds a provider-backed
/// App — mirroring `cmd_run`'s provider wiring — over the same skill library
/// the agent sees, then runs [`oneai_agent::run_consolidation`].
pub async fn cmd_curator_consolidate(config: &OneaiConfig, domain: Option<&str>) {
    let domain_name = config.default_domain_pack(domain);

    // Build the model config (needs a provider, unlike the other curator
    // actions). Exit cleanly if no provider is configured.
    let provider_config = config.to_model_config_with_overrides(None);
    let Some(model_config) = provider_config else {
        eprintln!("Error: `curator consolidate` needs an LLM provider.");
        eprintln!("Set ONEAI_API_KEY or configure ~/.oneai/config.toml (model).");
        std::process::exit(1);
    };

    let pack = get_builtin_pack(&domain_name, ".").unwrap_or_else(|| coding_pack("."));
    let provider = oneai_provider::ProviderFactory::create(model_config);
    let app = AppBuilder::new()
        .provider(std::sync::Arc::from(provider))
        .noop_interaction_gate()
        .default_parser()
        .generation_config(config.generation.clone())
        .domain_pack(pack)
        .build()
        .await
        .expect("curator consolidate App build failed");

    // Load the same skill library the agent sees.
    let skills = oneai_skill::builtin::skills_for_domain(&domain_name);
    app.skill_registry.register_builtin(skills).await.unwrap();
    app.register_skill_tools().await.unwrap();

    let curator = match app.skill_curator.as_ref() {
        Some(c) => c,
        None => {
            eprintln!("curator unavailable (no skill_lifecycle policy).");
            std::process::exit(1);
        }
    };
    let provider = app.provider.as_ref().expect("provider wired above");

    // Write umbrella SKILL.md into the global skills dir so discovery
    // re-finds them next session (same dir `rollback` restores into).
    let skills_dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join(".oneai")
        .join("skills");

    match oneai_agent::run_consolidation(
        provider.as_ref(),
        curator,
        &skills_dir,
        &config.generation,
    )
    .await
    {
        Ok(report) => {
            if report.empty {
                println!("🧩 Nothing to consolidate — library already tidy (<2 candidates).");
                return;
            }
            println!("🧠 Consolidation pass complete.\n");
            for mr in &report.proposals_applied {
                println!(
                    "  ✅ umbrella `{}` ← merged {} (archived: {})",
                    mr.umbrella_name,
                    mr.members_archived.len(),
                    mr.members_archived.join(", ")
                );
                if !mr.members_skipped.is_empty() {
                    println!(
                        "     ⚠️  skipped (referenced by active pack): {}",
                        mr.members_skipped.join(", ")
                    );
                }
                println!("     ↩️  undo: `oneai curator rollback {}`", mr.backup_id);
            }
            if !report.proposals_skipped.is_empty() {
                println!("\n  ⏭️  proposals skipped:");
                for s in &report.proposals_skipped {
                    println!("     - {s}");
                }
            }
        }
        Err(e) => {
            eprintln!("consolidation failed: {e}");
            std::process::exit(1);
        }
    }
}
