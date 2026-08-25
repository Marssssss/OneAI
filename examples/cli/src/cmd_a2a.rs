//! A2A protocol management commands.
//!
//! Subcommands for discovering, connecting to, and serving as A2A agents.
//!
//! `a2a serve` (§3.5) starts a real axum HTTP server that serves the
//! AgentCard at `/.well-known/agent-card` and runs the full AgentLoop on
//! `tasks/send` (+ SSE streaming on `tasks/sendSubscribe`). `POST /` is
//! gated by a shared-secret Bearer (`ONEAI_A2A_SECRET`).

use async_trait::async_trait;
use oneai_a2a::{A2AClient, A2ARunner, A2AServerHost, A2ASseSink, TaskOutcome};
use oneai_app::AppBuilder;
use oneai_tool::CalculatorTool;
use std::sync::Arc;

use crate::cmd_pack::get_builtin_pack;
use crate::config::OneaiConfig;

/// Start the A2A server — serve OneAI agent capabilities over a real axum
/// HTTP server (§3.5).
///
/// Builds a real `App` for the given domain pack, wraps it in [`AppA2ARunner`]
/// (the seam that drives `create_session_with_id` + `run_agent_silent` /
/// `run_agent` on each incoming `tasks/send`), and serves:
/// - `GET /.well-known/agent-card` (discovery, no auth)
/// - `POST /` (JSON-RPC + SSE streaming, `ONEAI_A2A_SECRET` Bearer-gated)
pub fn cmd_a2a_serve(domain: Option<&str>, port: u16) {
    tracing_subscriber::fmt::init();

    let config = OneaiConfig::load_or_default();
    let domain_name = config.default_domain_pack(domain);
    let domain_pack = match get_builtin_pack(&domain_name, ".") {
        Some(p) => p,
        None => {
            eprintln!(
                "Error: Unknown domain pack '{}'. Available: coding, research, general",
                domain_name
            );
            std::process::exit(1);
        }
    };
    let pack_name = domain_pack.name.clone();
    let pack_tools = domain_pack.tools.clone();

    // Provider is optional: without it the server still starts (discovery
    // works) but `tasks/send` rejects with "no LLM provider configured".
    let provider_config = config.to_model_config_with_overrides(None);
    let has_provider = provider_config.is_some();

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        let mut builder = AppBuilder::new()
            .default_parser()
            .default_rate_limiter()
            .noop_interaction_gate() // headless server → auto-approve tools
            .trace_in_memory()
            // gap P2 #13 — real BPE token counting for budget/compression.
            .default_token_counter()
            .generation_config(config.generation.clone())
            .embedding_config(config.embedding.clone())
            .domain_pack(domain_pack)
            .sqlite_persistence(); // per-session A2A tasks resume the chat

        if let Some(mc) = &provider_config {
            let provider = oneai_provider::ProviderFactory::create(mc.clone());
            builder = builder.provider(Arc::from(provider));
        }
        // gap P1 #9 — permission-decision audit trail when configured.
        if let Some(l) = config.permission_audit_log_sink() {
            builder = builder.permission_audit_log(l);
        }

        let app = builder.build().await.expect("App build failed");

        // Register domain tools + calculator (same set as cmd_run). Builtin
        // skills + skill tools are wired by `AppBuilder::build()` (#38).
        for tool in &pack_tools {
            let _ = app.register_tool(tool.clone()).await;
        }
        let _ = app.register_tool(Arc::new(CalculatorTool::new())).await;

        let runner = Arc::new(AppA2ARunner {
            app,
            has_provider,
            lock: tokio::sync::Mutex::new(()),
        }) as Arc<dyn A2ARunner>;

        let url = format!("http://localhost:{port}");
        let host = Arc::new(
            A2AServerHost::from_domain_pack(&get_builtin_pack(&pack_name, ".").unwrap(), &url)
                .with_runner(runner),
        );

        println!("🤖 A2A Server starting...");
        println!(
            "   Agent: {} ({})",
            host.agent_card().name,
            host.agent_card().url
        );
        println!("   Skills: {}", host.agent_card().skills.len());
        println!(
            "   Capabilities: streaming={}, push_notifications={}, state_history={}",
            host.agent_card().capabilities.streaming,
            host.agent_card().capabilities.push_notifications,
            host.agent_card().capabilities.state_transition_history,
        );
        println!(
            "   Provider: {}",
            if has_provider {
                "configured"
            } else {
                "NONE — tasks/send will reject"
            }
        );
        println!("\n📋 Agent Card (/.well-known/agent-card):");
        if let Ok(card_json) = host.well_known_card_json() {
            println!("{}", card_json);
        }
        println!("\n   External task: POST /  (Authorization: Bearer $ONEAI_A2A_SECRET)");
        if oneai_a2a::secret_from_env().is_none() {
            eprintln!("⚠️  ONEAI_A2A_SECRET unset — server will refuse to start.");
            eprintln!(
                "   Set it (e.g. export ONEAI_A2A_SECRET=$(openssl rand -hex 32)) to enable."
            );
            std::process::exit(1);
        }
        println!("\nPress Ctrl+C to stop the server.");

        if let Err(e) = host.run(port).await {
            eprintln!("❌ A2A server error: {}", e);
            std::process::exit(1);
        }
    });
}

// ─── AppA2ARunner — drives create_session_with_id + run_agent_silent ────────

/// The CLI's [`A2ARunner`] impl — holds one built `App` and a per-turn
/// serialization lock (so concurrent A2A tasks on the same App don't race
/// the shared `MemoryManager.set_session_id`). Mirrors `AppGatewayRunner`.
struct AppA2ARunner {
    app: oneai_app::App,
    has_provider: bool,
    lock: tokio::sync::Mutex<()>,
}

#[async_trait]
impl A2ARunner for AppA2ARunner {
    async fn run_task(&self, session_id: &str, message_text: &str) -> TaskOutcome {
        if !self.has_provider {
            return TaskOutcome::Rejected {
                reason: "no LLM provider configured".to_string(),
            };
        }
        // Serialize turns so the shared MemoryManager session binding doesn't
        // race across concurrent A2A tasks.
        let _guard = self.lock.lock().await;
        let mut session = self.app.create_session_with_id(session_id).await;
        match session.run_agent_silent(message_text).await {
            Ok(r) => TaskOutcome::Done {
                final_answer: r.final_answer,
                completed: r.completed,
                iterations: r.iterations,
            },
            Err(e) => TaskOutcome::Error {
                message: e.to_string(),
            },
        }
    }

    /// Streaming reply path: wire a [`A2AStreamingObserver`] into `run_agent`
    /// so `on_stream_chunk` pushes assistant tokens to the A2A SSE sink.
    async fn run_task_streaming(
        &self,
        session_id: &str,
        message_text: &str,
        sink: Arc<dyn A2ASseSink>,
    ) -> TaskOutcome {
        if !self.has_provider {
            return TaskOutcome::Rejected {
                reason: "no LLM provider configured".to_string(),
            };
        }
        let _guard = self.lock.lock().await;
        let mut session = self.app.create_session_with_id(session_id).await;
        let observer = A2AStreamingObserver { sink };
        let interrupt_slot: Arc<tokio::sync::Mutex<Option<oneai_agent::AgentLoop>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        match session
            .run_agent(message_text, &observer, interrupt_slot)
            .await
        {
            Ok(r) => TaskOutcome::Done {
                final_answer: r.final_answer,
                completed: r.completed,
                iterations: r.iterations,
            },
            Err(e) => TaskOutcome::Error {
                message: e.to_string(),
            },
        }
    }

    fn supports_streaming(&self) -> bool {
        self.has_provider
    }

    /// gap P0 #4 — record the inbound W3C `traceparent` on this App's trace
    /// context (attribute on the active span, when one is open) so the
    /// distributed trace link survives into the local trajectory, then drive
    /// the usual streaming / non-streaming turn.
    async fn run_task_with_trace(
        &self,
        session_id: &str,
        message_text: &str,
        traceparent: Option<&str>,
        sink: Option<Arc<dyn A2ASseSink>>,
    ) -> TaskOutcome {
        if let Some(tp) = traceparent {
            tracing::info!(
                "A2A task on session '{}' continuing inbound trace: {}",
                session_id,
                tp
            );
            if let Some(ctx) = self.app.trace_context.as_ref() {
                ctx.set_attribute("a2a.inbound_traceparent", serde_json::json!(tp));
            }
        }
        match sink {
            Some(s) if self.supports_streaming() => {
                self.run_task_streaming(session_id, message_text, s).await
            }
            _ => self.run_task(session_id, message_text).await,
        }
    }
}

/// Observer that relays assistant stream chunks to the A2A SSE sink. Mirrors
/// `cmd_gateway::StreamingRelayObserver` (wired to `ReplySink` there).
struct A2AStreamingObserver {
    sink: Arc<dyn A2ASseSink>,
}

impl oneai_agent::AgentLoopObserver for A2AStreamingObserver {
    fn on_iteration_start(&self, _: usize, _: oneai_agent::ParadigmKind) {}
    fn on_direct_answer(&self, _: &str) {}
    fn on_tool_calls(&self, _: &[oneai_agent::ToolCallRequest]) {}
    fn on_tool_result(&self, _: &str, _: &str, _: &oneai_core::ToolOutput) {}
    fn on_delegate(&self, _: &str, _: &str, _: &oneai_agent::SubAgentKind) {}
    fn on_paradigm_switch(&self, _: oneai_agent::ParadigmKind) {}
    fn on_checkpoint(&self, _: usize) {}
    fn on_complete(&self, _: &oneai_agent::AgentLoopResult) {}
    fn on_stream_chunk(&self, text: &str) {
        self.sink.push_chunk(text);
    }
}

/// Discover a remote A2A agent's capabilities.
///
/// Connects to the remote agent and fetches its AgentCard.
pub fn cmd_a2a_discover(url: &str) {
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");

    rt.block_on(async {
        let mut client = A2AClient::new(url);

        println!("🔍 Discovering A2A agent at: {}", url);

        match client.discover().await {
            Ok(card) => {
                println!("✅ Agent discovered!");
                println!("   Name: {}", card.name);
                println!("   Description: {}", card.description);
                println!("   URL: {}", card.url);
                println!("   Version: {}", card.version.unwrap_or_default());
                if let Some(provider) = &card.provider {
                    println!(
                        "   Provider: {} ({})",
                        provider.organization,
                        provider.url.as_deref().unwrap_or("")
                    );
                }
                println!("   Skills:");
                for skill in &card.skills {
                    println!(
                        "     • {} [{}]: {}",
                        skill.name, skill.id, skill.description
                    );
                    if !skill.examples.is_empty() {
                        println!("       Examples: {}", skill.examples.join(", "));
                    }
                }
                println!("   Capabilities:");
                println!("     Streaming: {}", card.capabilities.streaming);
                println!(
                    "     Push notifications: {}",
                    card.capabilities.push_notifications
                );
                println!(
                    "     State history: {}",
                    card.capabilities.state_transition_history
                );
                println!(
                    "   Authentication: {} schemes",
                    card.authentication.schemes.len()
                );
                for scheme in &card.authentication.schemes {
                    println!("     • {}", scheme);
                }
            }
            Err(e) => {
                eprintln!("❌ Discovery failed: {}", e);
            }
        }
    });
}

/// List configured A2A endpoints.
///
/// Reads from the A2A client configuration (placeholder for future).
pub fn cmd_a2a_list() {
    println!("📋 A2A Endpoints\n");
    println!("  No configured endpoints yet.");
    println!("  Use: oneai a2a discover <url> to find remote agents");
    println!("  Use: oneai a2a serve to start as an A2A server");
}

/// Send a task to a remote A2A agent.
///
/// Creates a task with a text message and sends it to the remote agent.
pub fn cmd_a2a_send(url: &str, message: &str) {
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");

    rt.block_on(async {
        let mut client = A2AClient::new(url);
        let task_id = format!("oneai-task-{}", uuid::Uuid::new_v4());

        println!("📤 Sending task to: {}", url);
        println!("   Message: {}", message);
        println!("   Task ID: {}", task_id);

        match client
            .send_task(&task_id, oneai_a2a::Message::user_text(message), None)
            .await
        {
            Ok(task) => {
                println!("✅ Task created!");
                println!("   ID: {}", task.id);
                println!("   State: {}", task.status.state);
                if let Some(session_id) = &task.session_id {
                    println!("   Session: {}", session_id);
                }
            }
            Err(e) => {
                eprintln!("❌ Send failed: {}", e);
            }
        }
    });
}
