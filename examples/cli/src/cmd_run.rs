//! Run command — single-shot non-interactive inference.
//!
//! This command runs a single inference without the TUI, suitable for
//! scripting, CI pipelines, and batch processing. Output goes to stdout.

use std::sync::Arc;
use oneai_app::AppBuilder;
use oneai_tool::CalculatorTool;

use crate::config::OneaiConfig;
use crate::cmd_pack::get_builtin_pack;

/// Run a single inference and output the result to stdout.
///
/// Uses AutoApprovalGate (no human approval needed) and silent execution
/// (no observer callbacks).
pub fn cmd_run(
    prompt: &str,
    config: &OneaiConfig,
    domain_override: Option<&str>,
    model_override: Option<&str>,
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
        eprintln!("Error: Unknown domain pack '{}'. Available: coding, research, general", domain_name);
        std::process::exit(1);
    }

    // Build App — auto-approve all tools for non-interactive mode
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = rt.block_on(async {
        let provider = oneai_provider::ProviderFactory::create(model_config);
        let builder = AppBuilder::new()
            .provider(Arc::from(provider))
            .auto_approval_gate()
            .default_parser();

        let app = builder.build().await.expect("App build failed");

        // Register built-in skills (domain + general) so the `skill` tool menu
        // is populated and the model can invoke them.
        let skills = oneai_skill::builtin::skills_for_domain(&domain_name);
        app.skill_registry.register_builtin(skills).await.unwrap();

        // Register domain tools
        let pack = domain_pack.unwrap();
        for tool in &pack.tools {
            app.register_tool(tool.clone()).await.unwrap();
        }
        app.register_tool(Arc::new(CalculatorTool::new())).await.unwrap();
        // Register the `skill` tool (progressive disclosure Tier2/Tier3).
        app.register_tool(Arc::new(oneai_agent::SkillTool::new(app.skill_registry.clone())))
            .await
            .unwrap();

        let mut session = app.create_session();

        // Run agent loop silently (no observer callbacks)
        session.run_agent_silent(prompt).await
    });

    match result {
        Ok(agent_result) => {
            if agent_result.completed {
                println!("{}", agent_result.final_answer);
            } else {
                eprintln!("Agent did not reach a final answer after {} iterations.", agent_result.iterations);
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
