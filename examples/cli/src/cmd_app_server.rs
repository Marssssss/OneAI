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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use oneai::group_chat::{OneAiGroupChatSession, ScenarioSpecView};
use oneai_agent::group_chat::{GroupChatSession, TurnPolicy};
use oneai_agent::{AgentLoop, GroupChatBusObserver};
use oneai_app::{App, AppBuilder, AppSession, DirectiveRuntime};
use oneai_app_server::{
    default_scenarios_path, serve_all, AppServerError, FileScenarioStore, ListenSpec,
    SharedScenarioStore,
};
use oneai_bus::{EngineBus, EngineYield};
use oneai_core::error::Result;
use oneai_core::{traits::LlmProvider, Message};
use oneai_provider::ProviderFactory;
use oneai_tool::CalculatorTool;
use tokio::sync::Mutex;

use crate::cmd_pack::get_builtin_pack;
use crate::config::OneaiConfig;

/// Init a `tracing_subscriber` that writes to stderr. The macOS app redirects
/// the sidecar's stderr to `~/.oneai/app-server-sidecar.log`, so this surfaces
/// engine activity (iterations, tool calls, approvals, errors) there for
/// debugging a stuck turn. `RUST_LOG` overrides the default filter.
fn init_stderr_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info,oneai=info,oneai_agent=info,oneai_provider=info,oneai_app_server=info")
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

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
    /// Active group-chat session when a `StartGroupChat` has displaced single-
    /// agent turns. `group/start`·`group/run` drive it; a new single-agent
    /// conversation (`session/create`) leaves it stale but unused (the next
    /// `StartGroupChat` rebuilds it). Mirrors `CFacadeRuntime.group`/`group_slot`.
    group: Option<Arc<GroupChatSession>>,
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

    // ── Group chat (Phase D Slice 2) ────────────────────────────────────
    //
    // The trait's default group methods error out ("group chat not active on
    // this runtime") — they exist so non-group runtimes typecheck unchanged.
    // The JSON-RPC sidecar needs group chat, so we override them here, mirroring
    // `CFacadeRuntime` (crates/oneai-uniffi/src/c_facade.rs): build a
    // `GroupChatSession` off the shared `App` resources via the uniffi
    // `OneAiGroupChatSession::build`, then drive each round through a
    // `GroupChatBusObserver` over `app.engine_bus` (the same bus single-agent
    // turns yield to). Speaker-tagged fragments + `SpeakerTurn` flow to the
    // frontend; a single round-level `TurnComplete` is emitted on success so an
    // out-of-process frontend (which can't observe the `await` returning) can
    // clear its `running` flag — the `GroupChatBusObserver` deliberately
    // no-ops `on_complete` so N members don't each emit one.

    async fn start_group(&mut self, scenario: oneai_bus::BusGroupScenario) -> Result<()> {
        // BusGroupScenario → the uniffi ScenarioSpecView (same field shape by
        // design) → per-member providers + shared app resources.
        let spec = ScenarioSpecView::from(&scenario);
        let gs = OneAiGroupChatSession::build(spec, &self.app)
            .map_err(|e| oneai_core::error::OneAIError::Config(format!("{e:?}")))?;
        self.group = Some(gs.inner_session());
        Ok(())
    }

    async fn group_start(&mut self) -> Result<()> {
        let group = self
            .group
            .clone()
            .ok_or_else(|| oneai_core::error::OneAIError::Agent("group chat not active".into()))?;
        let turn_id = group_turn_id();
        let observer = GroupChatBusObserver::new(self.engine_bus()?, turn_id.clone());
        let res = group.start(&observer).await;
        if res.is_ok() {
            let _ = self.emit_turn_complete(turn_id);
        }
        res
    }

    async fn group_run_task(&mut self, user_input: &str) -> Result<()> {
        let group = self
            .group
            .clone()
            .ok_or_else(|| oneai_core::error::OneAIError::Agent("group chat not active".into()))?;
        let turn_id = group_turn_id();
        let observer = GroupChatBusObserver::new(self.engine_bus()?, turn_id.clone());
        let res = group.run_task(user_input, &observer).await;
        if res.is_ok() {
            let _ = self.emit_turn_complete(turn_id);
        }
        res
    }

    async fn group_set_scripted_order(&mut self, order: Vec<String>) {
        if let Some(group) = &self.group {
            group.set_turn_policy(TurnPolicy::Scripted { order }).await;
        }
    }
}

// ─── AppServerRuntime group helpers ──────────────────────────────────────────

impl AppServerRuntime {
    /// The bus the pump drives; `engine_bus()` was called at startup so it's
    /// always `Some` here. Returned as `Arc<dyn EngineBus>` for the observer.
    fn engine_bus(&self) -> Result<Arc<dyn EngineBus>> {
        let bus = self.app.engine_bus.clone().ok_or_else(|| {
            oneai_core::error::OneAIError::Agent(
                "engine_bus not configured; call AppBuilder::engine_bus() before group chat".into(),
            )
        })?;
        Ok(bus)
    }

    /// Emit the single round-level `TurnComplete` after a group round succeeds.
    /// `final_answer` is empty — the members' answers rode `DirectAnswer`
    /// yields, and the frontend's `turn_complete` handler leaves the last
    /// member's bubble intact behind its `if !final.is_empty()` guard.
    fn emit_turn_complete(&self, turn_id: String) -> Result<()> {
        let bus = self.engine_bus()?;
        let _ = bus.emit(EngineYield::TurnComplete {
            turn_id,
            summary: oneai_bus::BusTurnSummary {
                final_answer: String::new(),
                iterations: 0,
                completed: true,
                active_paradigm: oneai_bus::BusParadigmKind::ReAct,
            },
        });
        Ok(())
    }
}

/// Monotonic group-round id (mirrors `c_facade::group_turn_id`'s uuid scheme
/// without pulling uuid into the CLI — a unique-enough bracketing key).
static GROUP_TURN_SEQ: AtomicU64 = AtomicU64::new(0);
fn group_turn_id() -> String {
    format!("group_{}", GROUP_TURN_SEQ.fetch_add(1, Ordering::Relaxed))
}

// ─── ConversationStore impl ──────────────────────────────────────────────────
//
// Backs the `session/list` JSON-RPC method (synchronous CRUD — no bus). Wraps
// the same `Arc<App>` the runtime drives, so the sidecar frontend's sidebar
// reads exactly the conversations this process persists. `App::
// list_conversations` swallows backend errors (unwrap_or_default), so this
// returns the list directly — a failing backend surfaces as an empty list,
// never a panic.

struct AppConversationStore {
    app: Arc<App>,
}

#[async_trait::async_trait]
impl oneai_app_server::ConversationStore for AppConversationStore {
    async fn list(&self) -> Vec<oneai_core::SessionInfo> {
        self.app.list_conversations().await
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
    // Init tracing to stderr BEFORE anything else. The macOS app spawns this
    // process as a sidecar and redirects its stderr to
    // ~/.oneai/app-server-sidecar.log — so engine spans (iterations, tool
    // calls, approvals, errors) are captured there for debugging. Without a
    // subscriber the engine's `tracing::*` calls are no-ops and a stuck turn
    // is invisible. `RUST_LOG` overrides the default (info for the engine +
    // provider crates). Stdio transport keeps stdout as the message stream.
    init_stderr_logging();

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
        // SQLite persistence. Default path is ~/.oneai/oneai.db, but when a
        // host (the macOS app's sidecar spawn) sets ONEAI_DB_PATH, persist at
        // that path instead — so the sidecar shares the SAME DB the in-process
        // FFI engine writes (~/Library/Application Support/oneai.db on macOS).
        // Switching transports then never loses history: the sidebar reads the
        // rows whichever engine is active. SQLite WAL + busy_timeout make the
        // cross-process sharing safe (only one engine is ever active at once —
        // the FFI app isn't built in sidecar mode, and vice versa).
        if let Some(db_path) = std::env::var("ONEAI_DB_PATH")
            .ok()
            .filter(|s| !s.is_empty())
        {
            eprintln!("   SQLite DB (shared): {db_path}");
            builder = builder.sqlite_persistence_at(&db_path);
        } else {
            builder = builder.sqlite_persistence();
        }
        builder = builder.working_state("./.oneai");

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

        // Conversation listing handle for `session/list` — wraps the same
        // `Arc<App>` the runtime drives, so a sidecar frontend's sidebar
        // reads the conversations this very process persists (and, when the
        // macOS app points `ONEAI_DB_PATH` at its Application Support DB,
        // the SAME rows the in-process FFI engine wrote — switching transports
        // never loses history).
        let conversation_store: oneai_app_server::SharedConversationStore =
            Arc::new(AppConversationStore { app: app.clone() });

        let runtime = Arc::new(Mutex::new(AppServerRuntime {
            app: app.clone(),
            session,
            group: None,
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
        let server = serve_all(specs, bus, scenario_store, conversation_store).await?;

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
