//! Tasks command — cross-session working-state management.
//!
//! `oneai tasks list`     — list open (unfinished) tasks for the current
//!                          user/project (reads the lightweight index).
//! `oneai tasks show <id>` — print a task's goal / steps / decisions / blockers.
//! `oneai tasks continue <id>` — start a NEW session bound to an existing
//!                          unfinished task (cross-session continuation). The
//!                          new session does NOT read the old session's
//!                          transcript — it rehydrates goal/steps/decisions/
//!                          blockers from the durable working-state event log.
//! `oneai tasks archive <id>` — mark a task done/archived (gzips its log).
//!
//! The working-state root defaults to `./.oneai` (in-repo, git-trackable for
//! coding domains); override with `--root`.

use std::path::PathBuf;
use std::sync::Arc;

use oneai_app::AppBuilder;
use oneai_core::traits::WorkingStateStore;
use oneai_persistence::FileWorkingStateStore;
use oneai_tool::CalculatorTool;

use crate::config::OneaiConfig;
use crate::cmd_pack::get_builtin_pack;

/// Default working-state root — in-repo so it's git-trackable.
const DEFAULT_ROOT: &str = "./.oneai";

/// Open the file working-state store at `root` (or the default).
fn open_store(root: Option<&str>) -> Arc<dyn WorkingStateStore> {
    let path = PathBuf::from(root.unwrap_or(DEFAULT_ROOT));
    Arc::new(FileWorkingStateStore::new(path))
}

/// The project scope = current working directory.
fn project_scope() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

/// `oneai tasks list`
pub fn cmd_tasks_list(user: Option<&str>, root: Option<&str>) {
    let store = open_store(root);
    let project = project_scope();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let briefs = rt
        .block_on(async { store.list_open_tasks(user.unwrap_or(""), &project).await })
        .unwrap_or_else(|e| {
            eprintln!("Error reading working-state index: {}", e);
            std::process::exit(1);
        });
    if briefs.is_empty() {
        println!("No unfinished tasks for user '{}/project '{}'.", user.unwrap_or(""), project);
        return;
    }
    println!("Unfinished tasks (user '{}', project '{}'):", user.unwrap_or(""), project);
    for b in &briefs {
        println!(
            "  • [{}] {} (步骤剩余 {}, 卡点 {}, 状态 {}, 更新 {})",
            b.task_id,
            b.goal,
            b.open_step_count,
            b.open_blocker_count,
            b.status.as_str(),
            b.last_event_ts,
        );
    }
    println!("\nContinue one with: oneai tasks continue <id>");
}

/// `oneai tasks show <id>`
pub fn cmd_tasks_show(id: &str, root: Option<&str>) {
    let store = open_store(root);
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let ws = rt
        .block_on(async { store.get_task(id).await })
        .unwrap_or_else(|e| {
            eprintln!("Error reading task '{}': {}", id, e);
            std::process::exit(1);
        });
    let ws = match ws {
        Some(ws) => ws,
        None => {
            eprintln!("Task '{}' not found.", id);
            std::process::exit(1);
        }
    };
    println!("Task:   {}", id);
    println!("Goal:   {}", ws.goal);
    if !ws.intent.is_empty() {
        println!("Intent: {}", ws.intent);
    }
    println!("Status: {}", ws.status.as_str());
    println!("\nSteps:");
    if ws.steps.is_empty() {
        println!("  (none)");
    } else {
        for s in &ws.steps {
            let icon = match s.status {
                oneai_core::StepStatus::Pending => "⏳",
                oneai_core::StepStatus::InProgress => "🔄",
                oneai_core::StepStatus::Completed => "✅",
                oneai_core::StepStatus::Failed => "✗",
            };
            println!("  {icon} [{}] {}", s.id, s.description);
        }
    }
    if !ws.decisions.is_empty() {
        println!("\nDecisions:");
        for d in &ws.decisions {
            println!("  • {} → {}", d.question, d.chosen);
            if !d.rationale.is_empty() {
                println!("      理由: {}", d.rationale);
            }
        }
    }
    let open: Vec<_> = ws.blockers.iter().filter(|b| b.status == oneai_core::BlockerStatus::Open).collect();
    if !open.is_empty() {
        println!("\nOpen blockers:");
        for b in &open {
            println!("  ⚠ {}: {}", b.id, b.description);
        }
    }
}

/// `oneai tasks archive <id>`
pub fn cmd_tasks_archive(id: &str, root: Option<&str>) {
    let store = open_store(root);
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    if let Err(e) = rt.block_on(async { store.archive_task(id).await }) {
        eprintln!("Error archiving task '{}': {}", id, e);
        std::process::exit(1);
    }
    println!("Task '{}' archived.", id);
}

/// `oneai tasks continue <id>` — start a new session bound to an existing task.
pub fn cmd_tasks_continue(
    id: &str,
    config: &OneaiConfig,
    domain_override: Option<&str>,
    model_override: Option<&str>,
    user: Option<&str>,
    root: Option<&str>,
) {
    tracing_subscriber::fmt::init();

    let provider_config = config.to_model_config_with_overrides(model_override);
    if provider_config.is_none() {
        eprintln!("Error: No LLM provider configured. Set ONEAI_API_KEY or ~/.oneai/config.toml.");
        std::process::exit(1);
    }
    let model_config = provider_config.unwrap();
    let domain_name = config.default_domain_pack(domain_override);
    let domain_pack = get_builtin_pack(&domain_name, ".");
    if domain_pack.is_none() {
        eprintln!("Error: Unknown domain pack '{}'.", domain_name);
        std::process::exit(1);
    }

    let root_path = PathBuf::from(root.unwrap_or(DEFAULT_ROOT));
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = rt.block_on(async move {
        let provider = oneai_provider::ProviderFactory::create(model_config);
        let mut builder = AppBuilder::new()
            .provider(Arc::from(provider))
            .noop_interaction_gate()
            .default_parser()
            .generation_config(config.generation.clone())
            .embedding_config(config.embedding.clone())
            .sqlite_persistence()
            .working_state(root_path.clone());
        if let Some(uid) = user {
            builder = builder.user_id(uid);
        }
        let app = builder.build().await.expect("App build failed");

        let skills = oneai_skill::builtin::skills_for_domain(&domain_name);
        app.skill_registry.register_builtin(skills).await.unwrap();
        let pack = domain_pack.unwrap();
        for tool in &pack.tools {
            app.register_tool(tool.clone()).await.unwrap();
        }
        app.register_tool(Arc::new(CalculatorTool::new())).await.unwrap();
        app.register_tool(Arc::new(oneai_agent::SkillTool::new(app.skill_registry.clone())))
            .await
            .unwrap();

        // Create a brand-new session (new session id, does NOT read the old
        // session's conversation) and bind it to the existing durable task.
        let mut session = app.create_session();
        session.continue_task(id);

        // The continuation prompt: the loop's hydrate_working_state has
        // already rehydrated goal/steps/decisions/blockers from the event log
        // into the pinned projection; ask the model to continue from the first
        // non-completed step.
        session
            .run_agent_silent("继续上次未完成的任务，从第一个未完成步骤开始，不要重复已完成的工作。")
            .await
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
                println!("{}", agent_result.final_answer);
            }
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}
