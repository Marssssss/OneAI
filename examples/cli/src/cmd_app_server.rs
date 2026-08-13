//! `oneai app-server` — JSON-RPC 2.0 frontend protocol layer (the unified
//! non-Rust-frontend entry point).
//!
//! Spawns an `AppSession` driven by the unified `EngineBus` (the same bus the
//! TUI consumes in-process) and exposes it over JSON-RPC 2.0 on any of
//! stdio / unix-socket / named-pipe / WebSocket via `oneai_app_server::serve_all`.
//! A non-Rust frontend (IDE plugin spawn / web / macOS-Swift / Windows-C#) is a
//! JSON-RPC client — `turn/run`, `approval/respond`, … ↔ `event` notifications.
//!
//! Differs from `oneai serve` (newline-JSON passthrough sidecar, retained as an
//! escape hatch): `app-server` speaks the operation-oriented JSON-RPC frontend
//! schema (one schema for all non-Rust frontends, IDE/MCP tool-chain friendly).
//! See `docs/app-server-mechanism.md`.

use std::sync::Arc;

use oneai_agent::AgentLoop;
use oneai_app::{App, AppBuilder, AppSession, DirectiveRuntime};
use oneai_app_server::{
    default_scenarios_path, serve_all, AppServerError, FileScenarioStore, ListenSpec,
    SharedScenarioStore,
};
use oneai_core::error::Result;
use oneai_core::{traits::LlmProvider, Message};
use oneai_provider::ProviderFactory;
use oneai_tool::CalculatorTool;
use tokio::sync::Mutex;

use crate::cmd_pack::get_builtin_pack;
use crate::config::OneaiConfig;

/// Default `--listen` if none given: an IPC socket at
/// `~/.oneai/app-server.sock` (separate from `serve.sock` / `server.sock`).
fn default_listen() -> Vec<String> {
    vec![format!(
        "ipc://{}",
        oneai_app_server::default_ipc_socket().display()
    )]
}

/// Parse + resolve `--listen` specs (apply default if empty).
fn parse_specs(listen: &[String]) -> oneai_app_server::Result<Vec<ListenSpec>> {
    let raw = if listen.is_empty() {
        default_listen()
    } else {
        listen.to_vec()
    };
    raw.iter()
        .map(|s| ListenSpec::parse(s))
        .collect::<oneai_app_server::Result<Vec<_>>>()
}

// ─── AppServerRuntime — headless DirectiveRuntime over AppSession ────────────
//
// Mirrors `cmd_serve`'s `SidecarRuntime` verbatim — the engine driver is the
// same whether the wire is newline-JSON passthrough (`serve`) or JSON-RPC
// (`app-server`); only the frontend-facing adapter differs.

struct AppServerRuntime {
    app: Arc<App>,
    session: AppSession,
}

#[async_trait::async_trait]
impl DirectiveRuntime for AppServerRuntime {
    async fn run_turn(
        &mut self,
        task: &str,
        slot: Arc<Mutex<Option<AgentLoop>>>,
    ) -> Result<oneai_bus::BusTurnSummary> {
        self.session.run_turn_via_bus(task, slot).await
    }

    async fn set_paradigm(
        &mut self,
        to: oneai_agent::ParadigmKind,
    ) -> Option<oneai_agent::ParadigmKind> {
        self.session.set_paradigm(to)
    }

    async fn set_plan_mode(&mut self, on: bool) {
        self.session.set_plan_mode(on);
    }

    async fn compact(&mut self, keep_recent_turns: usize) -> Result<oneai_app::CompactOutcome> {
        self.session.compact(keep_recent_turns).await
    }

    fn provider(&self) -> Option<Arc<dyn LlmProvider>> {
        self.session.provider().cloned()
    }

    async fn create_session(&mut self, id: Option<String>) -> String {
        let new = match id {
            Some(wanted) => self.app.create_session_with_id(&wanted).await,
            None => self.app.create_session(),
        };
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

    async fn session_id(&mut self) -> String {
        self.session.session_id().to_string()
    }
}

// ─── cmd_app_server ──────────────────────────────────────────────────────────

pub fn cmd_app_server(
    config: &OneaiConfig,
    listen: &[String],
    domain: Option<&str>,
    model: Option<&str>,
    user: Option<&str>,
) {
    let specs = match parse_specs(listen) {
        Ok(s) => s,
        Err(AppServerError::InvalidSpec(e)) => {
            eprintln!("Error: invalid --listen spec: {e}");
            std::process::exit(2);
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    // Banners go to stderr: stdout is the JSON-RPC message stream for the
    // `stdio` and `native-messaging` transports (an IDE spawn / a browser
    // native-messaging host read stdout as framed messages — any banner there
    // corrupts the protocol). LSP convention: log to stderr.
    eprintln!("🤖 OneAI app-server — JSON-RPC 2.0 over the engine bus");
    for s in &specs {
        eprintln!("   listen: {s:?}");
    }
    if let Some(d) = domain {
        eprintln!("   Domain: {d}");
    }
    eprintln!();

    let provider_config = config.to_model_config_with_overrides(model);
    let has_provider = provider_config.is_some();
    if !has_provider {
        eprintln!("⚠️  No LLM provider configured (set ONEAI_API_KEY / ONEAI_BASE_URL).");
        eprintln!("   The app-server will start, but turns will reject.\n");
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
        let skills = oneai_skill::builtin::skills_for_domain(domain_pack_name);
        let _ = app.skill_registry.register_builtin(skills).await;
        let _ = app.register_tool(Arc::new(CalculatorTool::new())).await;
        let _ = app.register_skill_tools().await;

        let session = app.create_session();
        let app = Arc::new(app);
        let bus = app
            .engine_bus
            .clone()
            .expect("engine_bus() was called on the builder");

        let runtime = Arc::new(Mutex::new(AppServerRuntime {
            app: app.clone(),
            session,
        }));
        let interrupt_slot: Arc<Mutex<Option<AgentLoop>>> = Arc::new(Mutex::new(None));

        // Engine driver — the shared pump. Drains Directive::UserMessage →
        // run_turn_via_bus; Shutdown stops it.
        let _pump =
            oneai_app::spawn_directive_pump(directive_rx, runtime, interrupt_slot, bus.clone());

        // Shared scenario library — the `scenario/*` methods back every
        // frontend's editor off one store + one validator. File-backed at
        // `~/.oneai/scenarios.json` (seeded with builtin presets on first run).
        let scenario_store: SharedScenarioStore = {
            let path = default_scenarios_path();
            match FileScenarioStore::new(path.clone()).await {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    eprintln!("⚠️  scenario store at {} unavailable: {e}", path.display());
                    eprintln!("   scenario/* methods will fail; other transports unaffected.");
                    // An empty in-memory store so serve_all still binds; the
                    // scenario/* methods return internal errors rather than
                    // panicking. Keeps a bad scenarios.json from taking down
                    // the whole app-server.
                    Arc::new(oneai_app_server::InMemoryScenarioStore::new())
                }
            }
        };

        // Multi-transport JSON-RPC server. Binds all `--listen` specs
        // concurrently against the one bus.
        let server = serve_all(specs, bus, scenario_store).await?;

        eprintln!("✅ Listening. Connect a JSON-RPC frontend.");
        eprintln!("   Methods: turn/run, turn/cancel, approval/respond, session/*, …");
        eprintln!("   Outbound: `event` notifications (one per EngineYield).");
        eprintln!("   Ctrl-C to stop.");

        tokio::select! {
            _ = server => {
                eprintln!("\n app-server: all listeners exited.");
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("\n Interrupted — shutting down.");
            }
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }) {
        eprintln!("Error running app-server: {}", e);
        std::process::exit(1);
    }
}
