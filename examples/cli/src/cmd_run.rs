//! Run command — single-shot non-interactive inference.
//!
//! This command runs a single inference without the TUI, suitable for
//! scripting, CI pipelines, and batch processing. Output goes to stdout.

use oneai_app::AppBuilder;
use oneai_tool::CalculatorTool;
use std::sync::Arc;

use crate::cmd_pack::get_builtin_pack;
use crate::config::OneaiConfig;

/// Run a single inference and output the result to stdout.
///
/// Uses AutoApprovalGate (no human approval needed) and silent execution
/// (no observer callbacks).
pub fn cmd_run(
    prompt: &str,
    config: &OneaiConfig,
    domain_override: Option<&str>,
    model_override: Option<&str>,
    user: Option<&str>,
) {
    tracing_subscriber::fmt::init();

    // Build ModelConfig
    let provider_config = config.to_model_config_with_overrides(model_override);
    if provider_config.is_none() {
        eprintln!("Error: No LLM provider configured.");
        eprintln!("Set ONEAI_API_KEY or configure ~/.oneai/config.toml");
        std::process::exit(1);
    }
    let model_config = provider_config.unwrap();

    // Get domain pack
    let domain_name = config.default_domain_pack(domain_override);
    let domain_pack = get_builtin_pack(&domain_name, ".");
    if domain_pack.is_none() {
        eprintln!(
            "Error: Unknown domain pack '{}'. Available: coding, research, general",
            domain_name
        );
        std::process::exit(1);
    }

    // Build App — auto-approve all tools for non-interactive mode
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = rt.block_on(async {
        let provider = oneai_provider::ProviderFactory::create(model_config);
        let builder = AppBuilder::new()
            .provider(Arc::from(provider))
            .noop_interaction_gate()
            .default_parser()
            // gap P2 #13 — real BPE token counting for budget/compression.
            .default_token_counter()
            .generation_config(config.generation.clone())
            .embedding_config(config.embedding.clone());
        // gap P1 #9 — permission-decision audit trail when configured.
        let builder = match config.permission_audit_log_sink() {
            Some(l) => builder.permission_audit_log(l),
            None => builder,
        };
        let builder = if let Some(uid) = user {
            builder.user_id(uid)
        } else {
            builder
        };

        let app = builder.build().await.expect("App build failed");

        // Register domain tools
        let pack = domain_pack.unwrap();
        for tool in &pack.tools {
            app.register_tool(tool.clone()).await.unwrap();
        }
        app.register_tool(Arc::new(CalculatorTool::new()))
            .await
            .unwrap();
        // Builtin skills + the skill tools are wired by `AppBuilder::build()`
        // (issue #38) — no per-command registration needed.

        let mut session = app.create_session();

        // Run agent loop silently (no observer callbacks)
        session.run_agent_silent(prompt).await
    });

    match result {
        Ok(agent_result) => {
            if agent_result.completed {
                println!("{}", agent_result.final_answer);
            } else {
                eprintln!(
                    "Agent did not reach a final answer after {} iterations.",
                    agent_result.iterations
                );
                // Still output the best answer we have
                println!("{}", agent_result.final_answer);
            }
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}
