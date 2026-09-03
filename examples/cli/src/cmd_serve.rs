//! `oneai serve` — the engine-bus sidecar (Shape B).
//!
//! Spawns an `AppSession` driven by the unified `EngineBus` (the same bus the
//! TUI consumes in-process — the shared `spawn_directive_pump` drives it) and
//! exposes it over UDS (Unix) / named pipe (Windows) via the newline-JSON
//! `Directive`/`EngineYield` codec. A native frontend (macOS Swift / Windows
//! C#) is a Directive writer + Yield reader socket client — see
//! `examples/native/{macos,windows}/OneAIBusClient.*`.
//!
//! Differs from `oneai supervisor serve`: the supervisor is an instance-registry
//! RPC daemon (request/response `spawn/list/stop/rpc`); the sidecar is a
//! bidirectional concurrent bus (arbitrary-time directives ↔ arbitrary-time
//! yields, approval `request_id` correlation). It uses a separate socket
//! (`~/.oneai/serve.sock`) so both can coexist.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use oneai_agent::{AgentLoop, ParadigmKind};
use oneai_app::{App, AppBuilder, AppSession, CompactOutcome, DirectiveRuntime};
use oneai_bus::bridge_connection;
use oneai_core::error::Result;
use oneai_core::{traits::LlmProvider, Message};
use oneai_provider::ProviderFactory;
use oneai_supervisor::IpcListener;
use oneai_tool::CalculatorTool;
use tokio::sync::Mutex;

use crate::cmd_pack::get_builtin_pack;
use crate::config::OneaiConfig;

/// Default sidecar socket — `~/.oneai/serve.sock` (separate from the
/// supervisor's `server.sock`).
fn default_serve_socket() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".oneai")
        .join("serve.sock")
}

fn socket_or_default(socket: Option<&str>) -> PathBuf {
    socket
        .map(PathBuf::from)
        .unwrap_or_else(default_serve_socket)
}

// ─── SidecarRuntime — headless DirectiveRuntime over AppSession ──────────────

/// The sidecar's engine driver state — mirrors `SessionState` but without TUI
/// rendering state (no chat history co-located; session-lifecycle directives
/// just swap the `AppSession`).
struct SidecarRuntime {
    app: Arc<App>,
    session: AppSession,
}

#[async_trait]
impl DirectiveRuntime for SidecarRuntime {
    async fn run_turn(
        &mut self,
        task: &str,
        slot: Arc<Mutex<Option<AgentLoop>>>,
    ) -> Result<oneai_bus::BusTurnSummary> {
        self.session.run_turn_via_bus(task, slot).await
    }

    async fn set_paradigm(&mut self, to: ParadigmKind) -> Option<ParadigmKind> {
        self.session.set_paradigm(to)
    }

    async fn set_plan_mode(&mut self, on: bool) {
        self.session.set_plan_mode(on);
    }

    async fn compact(&mut self, keep_recent_turns: usize) -> Result<CompactOutcome> {
        self.session.compact(keep_recent_turns).await
    }

    fn provider(&self) -> Option<Arc<dyn LlmProvider>> {
        self.session.provider().cloned()
    }

    async fn create_session(&mut self, id: Option<String>, workspace: Option<String>) -> String {
        let mut new = match id {
            Some(wanted) => self.app.create_session_with_id(&wanted).await,
            None => self.app.create_session(),
        };
        new.set_workspace(workspace.as_deref());
        let nid = new.session_id().to_string();
        self.session = new;
        nid
    }

    async fn load_session(&mut self, id: String) -> (String, Vec<Message>) {
        let sessions = self.app.list_conversations().await;
        let resolved = if sessions.iter().any(|s| s.id == id) {
            id.clone()
        } else {
            let matches: Vec<_> = sessions.iter().filter(|s| s.id.starts_with(&id)).collect();
            match matches.len() {
                1 => matches[0].id.clone(),
                _ => id.clone(),
            }
        };
        let new = self.app.create_session_with_id(&resolved).await;
        let msgs = new.conversation().messages.clone();
        self.session = new;
        (resolved, msgs)
    }

    async fn reset_session(&mut self) -> String {
        let new = self.app.create_session();
        self.session = new;
        self.session.session_id().to_string()
    }

    async fn delete_session(&mut self, id: String) -> Result<()> {
        self.app.delete_conversation(&id).await
    }

    async fn list_sessions(&mut self) -> Vec<oneai_core::SessionInfo> {
        self.app.list_conversations().await
    }

    async fn session_id(&mut self) -> String {
        self.session.session_id().to_string()
    }
}

// ─── cmd_serve ───────────────────────────────────────────────────────────────

pub fn cmd_serve(
    config: &OneaiConfig,
    socket: Option<&str>,
    domain: Option<&str>,
    model: Option<&str>,
    user: Option<&str>,
) {
    let socket_path = socket_or_default(socket);

    println!("🤖 OneAI sidecar — engine bus over IPC");
    println!("   Socket: {}", socket_path.display());
    if let Some(d) = domain {
        println!("   Domain: {}", d);
    }
    println!();

    let provider_config = config.to_model_config_with_overrides(model);
    let has_provider = provider_config.is_some();
    if !has_provider {
        eprintln!("⚠️  No LLM provider configured (set ONEAI_API_KEY / ONEAI_BASE_URL).");
        eprintln!("   The sidecar will start, but turns will reject.\n");
    }

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    if let Err(e) = rt.block_on(async move {
        // Engine bus — wires the InProcessBus + BusInteractionGate (approvals
        // surface as EngineYield::ApprovalRequest ↔ Directive::Approve).
        let (builder, directive_rx) = AppBuilder::new()
            .default_parser()
            .default_rate_limiter()
            .engine_bus();
        let mut builder = builder.generation_config(config.generation.clone());
        if let Some(mc) = provider_config {
            let provider = ProviderFactory::create(mc);
            builder = builder.provider(Arc::from(provider));
        }
        if let Some(uid) = user {
            builder = builder.user_id(uid);
        }
        builder = builder.sqlite_persistence().working_state("./.oneai");

        let domain_pack_name = domain.unwrap_or("coding");
        let domain_pack = get_builtin_pack(domain_pack_name, ".")
            .unwrap_or_else(|| oneai_domain::coding_pack("."));
        builder = builder.domain_pack(domain_pack);

        let app = builder.build().await?;
        let _ = app.register_tool(Arc::new(CalculatorTool::new())).await;
        // Builtin skills + skill tools are wired by `AppBuilder::build()` (#38).

        let session = app.create_session();
        let app = Arc::new(app);
        let bus = app
            .engine_bus
            .clone()
            .expect("engine_bus() was called on the builder");

        let runtime = Arc::new(Mutex::new(SidecarRuntime {
            app: app.clone(),
            session,
        }));
        let interrupt_slot: Arc<Mutex<Option<AgentLoop>>> = Arc::new(Mutex::new(None));

        // Engine driver — the shared pump. Drains Directive::UserMessage →
        // run_turn_via_bus; Shutdown stops it.
        let _pump =
            oneai_app::spawn_directive_pump(directive_rx, runtime, interrupt_slot, bus.clone());

        // Bind + accept loop. Each connection gets a bridge (yield forwarder +
        // directive reader) over the same bus — multiple frontends share one
        // bus (broadcast yields fan out; directives serialize through one pump).
        let mut listener = IpcListener::bind(&socket_path).await?;
        println!("✅ Listening. Connect a frontend (Directive writer + Yield reader).");
        println!("   Ctrl-C to stop.");
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    eprintln!("\n Interrupted — shutting down.");
                    break;
                }
                res = listener.accept() => {
                    let stream = res?;
                    let bus = bus.clone();
                    tokio::spawn(async move {
                        if let Err(e) = bridge_connection(stream, bus).await {
                            tracing::warn!(error = %e, "sidecar: connection ended");
                        }
                    });
                }
            }
        }
        // Drop the listener to unlink the socket on clean exit (Unix).
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }) {
        eprintln!("Error running sidecar: {}", e);
        std::process::exit(1);
    }
}
