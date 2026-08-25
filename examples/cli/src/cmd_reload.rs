//! Reload command — trigger a runtime data-layer reload (Phase 3.4).
//!
//! Re-reads the DomainPack data layer (discovered skills, MCP tool
//! registrations) without restarting, by invoking the app's
//! `DataLayerReloader`. This is the CLI equivalent of the model calling the
//! `reload` tool — useful after skills are added/edited on disk or an MCP
//! server's tool set changes, so the next agent run sees them.

use std::sync::Arc;

use oneai_app::AppBuilder;
use oneai_tool::CalculatorTool;

use crate::cmd_pack::get_builtin_pack;
use crate::config::OneaiConfig;

/// Build the app (same shape as `cmd_run`, so the reload exercises the real
/// configured registries — skills + MCP), then trigger the data-layer reload
/// and print the (re-)loaded item names.
pub fn cmd_reload(
    config: &OneaiConfig,
    domain_override: Option<&str>,
    model_override: Option<&str>,
    user: Option<&str>,
) {
    tracing_subscriber::fmt::init();

    let provider_config = config.to_model_config_with_overrides(model_override);
    let domain_name = config.default_domain_pack(domain_override);
    let domain_pack = get_builtin_pack(&domain_name, ".");
    if domain_pack.is_none() {
        eprintln!(
            "Error: Unknown domain pack '{}'. Available: coding, research, general",
            domain_name
        );
        std::process::exit(1);
    }

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = rt.block_on(async {
        let builder = AppBuilder::new()
            .noop_interaction_gate()
            .default_parser()
            .generation_config(config.generation.clone())
            .embedding_config(config.embedding.clone());
        let builder = if let Some(uid) = user {
            builder.user_id(uid)
        } else {
            builder
        };
        // Provider is optional for a reload — the data layer (skills / MCP)
        // is independent of the LLM. Only attach one if configured, so a
        // reload works even without API credentials.
        let builder = if let Some(mc) = provider_config {
            let provider = oneai_provider::ProviderFactory::create(mc);
            builder.provider(Arc::from(provider))
        } else {
            builder
        };

        let app = builder.build().await.expect("App build failed");

        let pack = domain_pack.unwrap();
        for tool in &pack.tools {
            app.register_tool(tool.clone()).await.unwrap();
        }
        app.register_tool(Arc::new(CalculatorTool::new()))
            .await
            .unwrap();
        // Builtin skills + skill tools are wired by `AppBuilder::build()` (#38).

        let reloader = match app.data_layer_reloader() {
            Some(r) => r.clone(),
            None => {
                eprintln!("No data-layer reloader is configured for this app.");
                std::process::exit(1);
            }
        };
        reloader.reload_data_layer().await
    });

    match result {
        Ok(names) => {
            if names.is_empty() {
                println!("Data layer reloaded; nothing new found.");
            } else {
                println!(
                    "Data layer reloaded. {} item(s) now available:",
                    names.len()
                );
                for name in &names {
                    println!("- {name}");
                }
            }
        }
        Err(e) => {
            eprintln!("Reload failed: {e}");
            std::process::exit(1);
        }
    }
}
