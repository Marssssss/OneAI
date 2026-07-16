//! Studio command — launch the OneAI Studio Web UI as an **interactive** playground.
//!
//! Builds a real `App` + `AppSession` (provider + domain tools + skills, same as
//! `cmd_run`/the TUI) and wires it into the Studio via a `StudioRunner` impl, so
//! a prompt typed in the browser drives an actual agent turn — iterations, tool
//! calls, streaming chunks, and the final answer stream live over the WebSocket.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use oneai_agent::AgentLoop;
use oneai_app::AppBuilder;
use oneai_persistence::FilePersistence;
use oneai_studio::{
    RunOutcome, RunnerStatus, StudioConfig, StudioRunner, StudioState,
    serve_with_state,
};
use oneai_tool::CalculatorTool;
use oneai_trace::{InMemoryCollector, TraceContext};
use tokio::sync::Mutex;

use crate::config::OneaiConfig;
use crate::cmd_pack::get_builtin_pack;

/// Launch the Studio Web UI server with an interactive agent attached.
pub fn cmd_studio(
    config: &OneaiConfig,
    port: u16,
    domain_override: Option<&str>,
    model_override: Option<&str>,
    user: Option<&str>,
) {
    println!("🤖 OneAI Studio — interactive Playground/Studio Web UI");
    println!("   Port: {}", port);
    println!("   Open: http://127.0.0.1:{}  (bypass any system proxy, e.g. --noproxy '*')", port);
    if let Some(d) = domain_override {
        println!("   Domain: {}", d);
    }
    println!();

    let provider_config = config.to_model_config_with_overrides(model_override);
    let has_provider = provider_config.is_some();
    if !has_provider {
        eprintln!("⚠️  No LLM provider configured (set ONEAI_API_KEY / ONEAI_BASE_URL).");
        eprintln!("   Studio will still serve, but sending a prompt will return 503.\n");
    }
    let model_config = provider_config;

    // Resolve the domain pack up-front (mirrors cmd_run). Fall back to the
    // built-in coding pack if the requested name isn't a builtin.
    let domain_name = config.default_domain_pack(domain_override);
    let domain_pack = match get_builtin_pack(&domain_name, ".") {
        Some(p) => p,
        None => {
            eprintln!("⚠️  Unknown domain pack '{}'. Falling back to built-in 'coding'.", domain_name);
            oneai_domain::coding_pack(".")
        }
    };

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    if let Err(e) = rt.block_on(serve(config, port, model_config, domain_pack, user, has_provider)) {
        eprintln!("Error starting Studio server: {}", e);
        std::process::exit(1);
    }
}

/// Build the App + session + StudioState and serve.
async fn serve(
    config: &OneaiConfig,
    port: u16,
    model_config: Option<oneai_core::ModelConfig>,
    domain_pack: oneai_domain::DomainPack,
    user: Option<&str>,
    has_provider: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // ── Build the App (provider optional, auto-approve, in-memory trace) ──
    let mut builder = AppBuilder::new()
        .default_parser()
        .default_rate_limiter()
        .noop_interaction_gate()   // headless web → auto-approve all tools
        .trace_in_memory()         // so /api/.../trace + metrics reflect the agent
        .generation_config(config.generation.clone());

    if let Some(mc) = model_config {
        let provider = oneai_provider::ProviderFactory::create(mc);
        builder = builder.provider(Arc::from(provider));
    }
    if let Some(uid) = user {
        builder = builder.user_id(uid);
    }

    let app = builder.build().await.expect("App build failed");

    // Register built-in skills (domain + general) so the `skill` tool menu is
    // populated and the model can invoke them.
    let skills = oneai_skill::builtin::skills_for_domain(&domain_pack.name);
    app.skill_registry.register_builtin(skills).await.unwrap();

    // Register domain tools + a calculator + the skill tool (progressive
    // disclosure Tier2/Tier3) — same set as `cmd_run`.
    for tool in &domain_pack.tools {
        app.register_tool(tool.clone()).await.unwrap();
    }
    app.register_tool(Arc::new(CalculatorTool::new())).await.unwrap();
    app.register_tool(Arc::new(oneai_agent::SkillTool::new(app.skill_registry.clone())))
        .await
        .unwrap();

    // ── Build a session (multi-turn conversation lives here) ──
    let session = app.create_session();
    let session_id = session.session_id().to_string();
    let session = Arc::new(Mutex::new(session));

    // Standalone interrupt slot (so a future "stop" button could cancel a run
    // without the session lock — mirrors the TUI).
    let interrupt_slot: Arc<Mutex<Option<AgentLoop>>> = Arc::new(Mutex::new(None));

    // ── Build StudioState from the App's REAL resources so /api/tools,
    // /api/.../trace and metrics reflect the agent. TraceContext is
    // Arc-backed and Clone — the session's loop writes spans into the same
    // collector this state reads.
    let trace_ctx = app.trace_context.clone()
        .unwrap_or_else(|| TraceContext::new(Arc::new(InMemoryCollector::new())));
    let persistence = app.persistence.clone()
        .unwrap_or_else(|| Arc::new(FilePersistence::new("/tmp/oneai-studio-checkpoints")));
    let tool_registry = app.tool_registry.clone();

    let studio_state = Arc::new(StudioState::new(trace_ctx, persistence, tool_registry));

    // Register the session so the header shows it.
    studio_state
        .register_session(oneai_studio::SessionView {
            id: session_id,
            paradigm: "react".to_string(),
            iteration: 0,
            running: false,
            total_tokens: 0,
        })
        .await;

    // Attach the runner — `/api/run` calls it; events stream via `studio_state`.
    let runner = Arc::new(StudioRunnerImpl {
        session: session.clone(),
        interrupt_slot,
        busy: Arc::new(AtomicBool::new(false)),
        has_provider,
    });
    studio_state.set_runner(Some(runner as Arc<dyn StudioRunner>)).await;

    // ── Serve ──
    let cfg = StudioConfig::with_port(port);
    serve_with_state(cfg, studio_state).await
}

// ─── StudioRunner impl — drives the AgentLoop from /api/run ──────────

struct StudioRunnerImpl {
    session: Arc<Mutex<oneai_app::AppSession>>,
    interrupt_slot: Arc<Mutex<Option<AgentLoop>>>,
    busy: Arc<AtomicBool>,
    has_provider: bool,
}

#[async_trait::async_trait]
impl StudioRunner for StudioRunnerImpl {
    fn status(&self) -> RunnerStatus {
        RunnerStatus {
            has_provider: self.has_provider,
            busy: self.busy.load(Ordering::Acquire),
        }
    }

    async fn run_task(&self, task: &str, observer: Arc<StudioState>) -> RunOutcome {
        // Authoritative single-flight guard (defense beyond the handler check).
        if self.busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return RunOutcome::Rejected { reason: "already running".to_string() };
        }

        let mut session = self.session.lock().await;
        let result = session
            .run_agent(task, observer.as_ref(), self.interrupt_slot.clone())
            .await;
        drop(session);

        self.busy.store(false, Ordering::Release);

        match result {
            Ok(agent_result) => {
                // on_complete already broadcast LoopComplete to the WS.
                RunOutcome::Done {
                    completed: agent_result.completed,
                    iterations: agent_result.iterations,
                }
            }
            Err(e) => {
                let msg = e.to_string();
                observer.broadcast(oneai_studio::StudioEvent::Error { message: msg.clone() });
                RunOutcome::Error { message: msg }
            }
        }
    }
}
