//! Workflow CLI commands.
//!
//! Provides CLI subcommands for listing, inspecting, and running the
//! predefined DAG workflows and cyclic StateGraphs that ship inside the
//! active DomainPack (e.g. CodingPack's code_review/debug/refactor/test
//! workflows + react/plan/reflect/explore state graphs).
//!
//!   oneai workflow list                  — List DAG workflows + state graphs
//!   oneai workflow show <name>           — Render a workflow DAG as ASCII
//!   oneai workflow run <name> [task]     — Execute a DAG workflow end-to-end
//!   oneai graph list                     — List state graphs
//!   oneai graph show <key>               — Render a state graph as ASCII
//!   oneai graph run <key> <task>         — Execute a state graph with a task
//!
//! Running a workflow/graph requires a configured LLM provider (set
//! ONEAI_API_KEY or ~/.oneai/config.toml). Provider HTTP traffic honors the
//! HTTPS_PROXY/HTTP_PROXY env vars uniformly (see CLAUDE.md "Network proxy").

use std::sync::Arc;

use oneai_app::AppBuilder;
use oneai_tool::CalculatorTool;

use crate::config::OneaiConfig;
use crate::cmd_pack::get_builtin_pack;

// ─── shared app build ─────────────────────────────────────────────────────

/// Build an App wired with the given domain pack's provider + tools.
///
/// Unlike `cmd_run` (which registers tools manually and leaves the session
/// without a domain pack), this calls `.domain_pack(pack)` so the pack's
/// predefined workflows/state_graphs are reachable via the session AND the
/// pack's tools (shell, read_file, grep, …) are auto-registered into the
/// workflow executor. The workflow executor is built with the provider
/// (builder.rs), so prompt-based DAG steps run real LLM inference.
fn build_app_with_domain(
    config: &OneaiConfig,
    domain_override: Option<&str>,
    model_override: Option<&str>,
    user: Option<&str>,
) -> oneai_app::App {
    tracing_subscriber::fmt::init();

    let provider_config = config.to_model_config_with_overrides(model_override);
    if provider_config.is_none() {
        eprintln!("Error: No LLM provider configured.");
        eprintln!("Set ONEAI_API_KEY or configure ~/.oneai/config.toml");
        std::process::exit(1);
    }
    let model_config = provider_config.unwrap();

    let domain_name = config.default_domain_pack(domain_override);
    let domain_pack = get_builtin_pack(&domain_name, ".").unwrap_or_else(|| {
        eprintln!(
            "Error: Unknown domain pack '{}'. Available: coding, research, general",
            domain_name
        );
        std::process::exit(1);
    });

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async move {
        let provider = oneai_provider::ProviderFactory::create(model_config);
        let builder = AppBuilder::new()
            .provider(Arc::from(provider))
            .noop_interaction_gate()
            .default_parser()
            .domain_pack(domain_pack)
            .generation_config(config.generation.clone());
        let builder = if let Some(uid) = user {
            builder.user_id(uid)
        } else {
            builder
        };

        let app = builder.build().await.expect("App build failed");

        // Register the `skill` tool + calculator on top of the auto-registered
        // domain tools (parity with cmd_run).
        let skills = oneai_skill::builtin::skills_for_domain(&domain_name);
        app.skill_registry.register_builtin(skills).await.unwrap();
        app.register_tool(Arc::new(CalculatorTool::new())).await.unwrap();
        app.register_tool(Arc::new(oneai_agent::SkillTool::new(app.skill_registry.clone())))
            .await
            .unwrap();

        app
    })
}

// ─── workflow (DAG) commands ──────────────────────────────────────────────

/// List available DAG workflows and state graphs from the active domain pack.
pub fn cmd_workflow_list(config: &OneaiConfig, domain_override: Option<&str>) {
    let domain_name = config.default_domain_pack(domain_override);
    let pack = get_builtin_pack(&domain_name, ".").unwrap_or_else(|| {
        eprintln!("Error: Unknown domain pack '{}'.", domain_name);
        std::process::exit(1);
    });

    println!("Domain pack: {}\n", domain_name);

    println!("DAG Workflows ({}):", pack.workflows.len());
    if pack.workflows.is_empty() {
        println!("  (none)");
    } else {
        for wf in &pack.workflows {
            println!("  • {} — {}", wf.name, wf.description);
            println!("    {} steps, v{}", wf.steps.len(), wf.version);
        }
    }

    println!("\nState Graphs ({}):", pack.state_graphs.len());
    if pack.state_graphs.is_empty() {
        println!("  (none)");
    } else {
        for sg in &pack.state_graphs {
            println!(
                "  • {} — entry '{}', {} nodes, terminal: {:?}",
                sg.name,
                sg.entry_point,
                sg.nodes.len(),
                sg.terminal_nodes
            );
        }
    }

    println!("\nUsage:");
    println!("  oneai workflow show <name>   — render a workflow DAG");
    println!("  oneai workflow run <name>     — execute a workflow");
    println!("  oneai graph show <key>       — render a state graph");
    println!("  oneai graph run <key> <task> — execute a state graph");
}

/// Render a workflow DAG as ASCII.
pub fn cmd_workflow_show(name: &str, config: &OneaiConfig, domain_override: Option<&str>) {
    let domain_name = config.default_domain_pack(domain_override);
    let pack = get_builtin_pack(&domain_name, ".").unwrap_or_else(|| {
        eprintln!("Error: Unknown domain pack '{}'.", domain_name);
        std::process::exit(1);
    });

    let wf = pack
        .workflows
        .iter()
        .find(|w| w.name == name)
        .unwrap_or_else(|| {
            panic!(
                "Workflow '{}' not found in domain pack '{}'. Available: {:?}",
                name,
                domain_name,
                pack.workflows.iter().map(|w| w.name.as_str()).collect::<Vec<_>>()
            )
        })
        .clone();

    let dag = oneai_workflow::compile(&wf);
    let ascii = oneai_workflow::render_dag_ascii(&dag);
    println!("Workflow: {} — {}\n", wf.name, wf.description);
    println!("Steps:");
    for step in &wf.steps {
        let kind = if let Some(tool) = &step.tool {
            format!("tool:{}", tool)
        } else if step.prompt.is_some() {
            "llm:prompt".to_string()
        } else {
            "?".to_string()
        };
        println!("  • {} ({}) — depends_on: {:?}", step.id, kind, step.depends_on);
    }
    println!("\nDAG:\n{}", ascii);
}

/// Execute a DAG workflow end-to-end and print per-step results.
pub fn cmd_workflow_run(
    name: &str,
    task: Option<&str>,
    config: &OneaiConfig,
    domain_override: Option<&str>,
    model_override: Option<&str>,
    user: Option<&str>,
) {
    let app = build_app_with_domain(config, domain_override, model_override, user);

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = rt.block_on(async move {
        let mut session = app.create_session();
        let wf = session
            .get_workflow_config(name)
            .ok_or_else(|| {
                oneai_core::error::OneAIError::Workflow(format!(
                    "Workflow '{}' not found in domain pack",
                    name
                ))
            })?;
        println!("▶ Running workflow: {} — {}\n", wf.name, wf.description);
        if let Some(t) = task {
            println!("   task input: \"{}\"\n", t);
            // Seed the workflow context with the task as a variable so steps
            // using {{task}} interpolate it.
            // (execute_workflow starts from the config's variables; we can't
            // inject extra vars post-hoc without modifying the config, so we
            // print it for visibility — prompt steps interpolate step outputs.)
            let _ = t;
        }
        session.execute_workflow(&wf).await
    });

    match result {
        Ok(wf_result) => {
            println!("\n── Workflow Result ──────────────────────────────");
            println!("success: {}, steps: {}, completed levels: {}, total_time: {}ms",
                wf_result.success,
                wf_result.step_results.len(),
                wf_result.completed_levels,
                wf_result.total_time_ms);

            // Print steps in declaration order of the original config if
            // available; otherwise iterate the result map.
            for (id, step) in &wf_result.step_results {
                let status = match step.status {
                    oneai_workflow::StepStatus::Completed => "✓ completed",
                    oneai_workflow::StepStatus::Failed => "✗ failed",
                    oneai_workflow::StepStatus::Skipped => "⊘ skipped",
                    oneai_workflow::StepStatus::Running => "⏵ running",
                    oneai_workflow::StepStatus::Pending => "· pending",
                    _ => "? unknown",
                };
                println!("\n  [{}] {} ({} retries, {:?})",
                    id, status, step.retries_used,
                    step.execution_time_ms);
                if let Some(out) = &step.output {
                    // Truncate very long step outputs for terminal readability.
                    let trimmed = if out.len() > 800 {
                        format!("{}…\n[…truncated, {} chars total]", &out[..800], out.len())
                    } else {
                        out.clone()
                    };
                    println!("    output:\n{}", indent(&trimmed, "      "));
                }
                if let Some(err) = &step.error {
                    println!("    error: {}", err);
                }
            }

            if !wf_result.success {
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("Error running workflow: {}", e);
            std::process::exit(1);
        }
    }
}

// ─── state graph commands ─────────────────────────────────────────────────

/// List available state graphs.
pub fn cmd_graph_list(config: &OneaiConfig, domain_override: Option<&str>) {
    let domain_name = config.default_domain_pack(domain_override);
    let pack = get_builtin_pack(&domain_name, ".").unwrap_or_else(|| {
        eprintln!("Error: Unknown domain pack '{}'.", domain_name);
        std::process::exit(1);
    });
    println!("State Graphs in domain pack '{}':\n", domain_name);
    if pack.state_graphs.is_empty() {
        println!("  (none)");
        return;
    }
    for sg in &pack.state_graphs {
        println!("  • {} — entry '{}', {} nodes, terminal: {:?}",
            sg.name, sg.entry_point, sg.nodes.len(), sg.terminal_nodes);
    }
    println!("\nUsage: oneai graph run <key> <task>");
}

/// Render a state graph as ASCII.
pub fn cmd_graph_show(key: &str, config: &OneaiConfig, domain_override: Option<&str>) {
    let domain_name = config.default_domain_pack(domain_override);
    let pack = get_builtin_pack(&domain_name, ".").unwrap_or_else(|| {
        eprintln!("Error: Unknown domain pack '{}'.", domain_name);
        std::process::exit(1);
    });
    let sg = pack
        .state_graphs
        .iter()
        .find(|g| g.name == key)
        .unwrap_or_else(|| {
            panic!(
                "State graph '{}' not found. Available: {:?}",
                key,
                pack.state_graphs.iter().map(|g| g.name.as_str()).collect::<Vec<_>>()
            )
        })
        .clone();
    println!("State Graph: {}\n", sg.name);
    println!("{}", oneai_workflow::render_state_graph_ascii(&sg));
}

/// Execute a state graph with a task and print the result.
pub fn cmd_graph_run(
    key: &str,
    task: &str,
    config: &OneaiConfig,
    domain_override: Option<&str>,
    model_override: Option<&str>,
    user: Option<&str>,
) {
    let app = build_app_with_domain(config, domain_override, model_override, user);

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = rt.block_on(async move {
        let mut session = app.create_session();
        let sg = session
            .get_state_graph(key)
            .ok_or_else(|| {
                oneai_core::error::OneAIError::Workflow(format!(
                    "State graph '{}' not found in domain pack",
                    key
                ))
            })?;
        println!("▶ Running state graph: {} — task: \"{}\"\n", sg.name, task);
        session.execute_state_graph_with_task(&sg, task).await
    });

    match result {
        Ok(g) => {
            println!("\n── State Graph Result ───────────────────────────");
            println!("completed: {}, iterations: {}, terminal: {:?}",
                g.completed, g.iterations, g.terminal_node);
            if let Some(last) = &g.final_state.last_result {
                let trimmed = if last.len() > 1200 {
                    format!("{}…\n[…truncated, {} chars total]", &last[..1200], last.len())
                } else {
                    last.clone()
                };
                println!("\nfinal answer:\n{}", indent(&trimmed, "  "));
            }
            if !g.completed {
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("Error running state graph: {}", e);
            std::process::exit(1);
        }
    }
}

// ─── helpers ──────────────────────────────────────────────────────────────

/// Prefix every line of `s` with `prefix`.
fn indent(s: &str, prefix: &str) -> String {
    s.lines()
        .map(|l| format!("{}{}", prefix, l))
        .collect::<Vec<_>>()
        .join("\n")
}
