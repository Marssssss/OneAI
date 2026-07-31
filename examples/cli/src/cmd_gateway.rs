//! Gateway command — launch the OneAI message gateway (Feishu / WeChat / loopback)
//! as a webhook HTTP server. Builds a real `App` and supplies a `GatewayRunner`
//! impl that drives `create_session_with_id` + `run_agent_silent` per inbound
//! message — mirroring `cmd_studio`'s `StudioRunner` pattern.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use oneai_app::AppBuilder;
use oneai_gateway::web::{serve as serve_web, WebConfig, WebhookState};
use oneai_gateway::{
    adapters::loopback::LoopbackPlatform, ChannelDirectory, Gateway, GatewayRunner,
    PlatformRegistry, ProfileRoute, ReplySink, TurnOutcome,
};
use oneai_tool::CalculatorTool;
use tokio::sync::{Mutex, RwLock};

use crate::cmd_pack::get_builtin_pack;
use crate::config::OneaiConfig;

/// List bound channels from the persisted directory.
pub fn cmd_gateway_channels() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        let dir = match oneai_gateway::ChannelDirectory::default_root().await {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Error loading channel directory: {}", e);
                std::process::exit(1);
            }
        };
        let bindings = dir.list().await;
        if bindings.is_empty() {
            println!("No bound channels yet.");
            return;
        }
        println!(
            "{:<8} {:<24} {:<36} last_seen",
            "platform", "channel", "session_id"
        );
        for b in bindings {
            println!(
                "{:<8} {:<24} {:<36} {}",
                b.platform, b.channel, b.session_id, b.last_seen
            );
        }
    });
}

/// Start the gateway webhook server.
pub fn cmd_gateway_serve(
    config: &OneaiConfig,
    bind: &str,
    domain_override: Option<&str>,
    model_override: Option<&str>,
    user: Option<&str>,
) {
    // Initialize tracing to stderr so webhook hits, signature failures, and
    // parse errors are visible (gateway runs headless — no TUI to take over
    // the terminal). Default: info + oneai_gateway=debug; RUST_LOG overrides.
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,oneai_gateway=debug"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    println!("🤖 OneAI Gateway — message-platform webhook server");
    println!("   Bind:   http://{}", bind);
    println!("   Routes: POST /gateway/{{platform}}   (Feishu/WeChat/loopback)");
    println!("           GET  /gateway/wechat          (WeChat handshake)");
    println!();

    let provider_config = config.to_model_config_with_overrides(model_override);

    let domain_name = config.default_domain_pack(domain_override);
    let domain_pack = match get_builtin_pack(&domain_name, ".") {
        Some(p) => p,
        None => {
            eprintln!(
                "⚠️  Unknown domain pack '{}'. Falling back to built-in 'coding'.",
                domain_name
            );
            oneai_domain::coding_pack(".")
        }
    };

    let addr: std::net::SocketAddr = match bind.parse() {
        Ok(a) => a,
        Err(_) => {
            eprintln!(
                "Invalid --bind '{}': expected host:port (e.g. 0.0.0.0:9090)",
                bind
            );
            std::process::exit(1);
        }
    };

    let pack_name = domain_pack.name.clone();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    if let Err(e) = rt.block_on(async {
        let (_gateway, handle) =
            run_gateway_task(config, addr, provider_config, &pack_name, user, None).await?;
        handle
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })??;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }) {
        eprintln!("Error starting gateway: {}", e);
        std::process::exit(1);
    }
}

/// Build the gateway — the lazily-built per-pack App factory (§3.1 tail #1),
/// platform adapters resolved from env, Feishu long-connection transports,
/// and the axum webhook state — and spawn the webhook server as a background
/// task. Returns the `Gateway` handle (for callers that want to attach more,
/// e.g. the supervisor's inlined gateway, Part E) and the webhook server's
/// `JoinHandle`. The caller decides whether to await the handle (foreground
/// `gateway serve`) or run it alongside another loop (`supervisor serve
/// --with-gateway`).
pub(crate) async fn run_gateway_task(
    config: &OneaiConfig,
    addr: std::net::SocketAddr,
    model_config: Option<oneai_core::ModelConfig>,
    pack_name: &str,
    user: Option<&str>,
    cron: Option<Arc<dyn oneai_core::traits::CronScheduler>>,
) -> Result<
    (
        Arc<Gateway>,
        tokio::task::JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>>,
    ),
    Box<dyn std::error::Error + Send + Sync>,
> {
    let gateway = build_gateway(config, model_config, pack_name, user, cron).await?;

    // ── Webhook state (webhook-push adapters) ──
    let mut state = WebhookState::new(gateway.clone())
        .with(Arc::new(oneai_gateway::web::LoopbackWebhookHandler));
    // Re-register any feishu/wechat adapters build_gateway wired (they're
    // already in the gateway's platform registry; the webhook state needs
    // them as WebhookHandlers too).
    // (build_gateway registered platforms on the gateway; for the webhook
    // handlers we re-resolve from env — they impl both MessagePlatform +
    // WebhookHandler.)
    if let Some(cfg) = oneai_gateway::adapters::feishu::FeishuConfig::from_env() {
        state = state.with(oneai_gateway::adapters::feishu::FeishuPlatform::arc(cfg));
    }
    if let Some(cfg) = oneai_gateway::adapters::wechat::WeChatConfig::from_env() {
        state = state.with(oneai_gateway::adapters::wechat::WeChatPlatform::arc(cfg));
    }

    println!("\nGateway listening on http://{addr}… press Ctrl+C to stop.\n");
    let handle = tokio::spawn(async move {
        serve_web(WebConfig { addr }, state)
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })
    });
    Ok((gateway, handle))
}

/// Build a `Gateway` (per-pack App factory + platform adapters resolved from
/// env + the long-connection transports) WITHOUT starting the webhook server
/// — shared by `gateway serve` (via [`run_gateway_task`]), `cron serve`, and
/// `cron fire` (one-shot delivery). The caller decides what to start on top.
pub(crate) async fn build_gateway(
    config: &OneaiConfig,
    model_config: Option<oneai_core::ModelConfig>,
    pack_name: &str,
    user: Option<&str>,
    cron: Option<Arc<dyn oneai_core::traits::CronScheduler>>,
) -> Result<Arc<Gateway>, Box<dyn std::error::Error + Send + Sync>> {
    let has_provider = model_config.is_some();
    if !has_provider {
        eprintln!("⚠️  No LLM provider configured (set ONEAI_API_KEY / ONEAI_BASE_URL).");
        eprintln!("   Inbound messages will be rejected with [oneai] 未能处理.\n");
    }

    // Runner: a lazily-built App per DomainPack (§3.1 tail #1). The gateway
    // core resolves the pack per channel and carries it via SESSION_SOURCE;
    // the runner reads it task-locally, builds the App on first contact, and
    // caches it. A per-pack Mutex serializes turns within a pack (the shared
    // MemoryManager.set_session_id would race across concurrent channels);
    // cross-pack turns run in parallel.
    let factory = Arc::new(GatewayAppFactory {
        config: config.clone(),
        model_config,
        user: user.map(String::from),
        cron,
    }) as Arc<dyn AppFactory>;
    let runner = Arc::new(AppGatewayRunner {
        factory,
        apps: RwLock::new(HashMap::new()),
        has_provider,
    });

    // ── Resolve platform adapters from env ──
    let feishu_platform = oneai_gateway::adapters::feishu::FeishuConfig::from_env().map(|cfg| {
        println!(
            "   Feishu adapter: ENABLED (app_id={}) + long-connection (no public URL needed)",
            cfg.app_id
        );
        oneai_gateway::adapters::feishu::FeishuPlatform::arc(cfg)
    });
    if feishu_platform.is_none() {
        println!(
            "   Feishu adapter: off (set FEISHU_APP_ID/FEISHU_APP_SECRET/FEISHU_VERIFY_TOKEN)"
        );
    }
    let wechat_platform = oneai_gateway::adapters::wechat::WeChatConfig::from_env().map(|cfg| {
        println!("   WeChat adapter: ENABLED (appid={})", cfg.app_id);
        oneai_gateway::adapters::wechat::WeChatPlatform::arc(cfg)
    });
    if wechat_platform.is_none() {
        println!("   WeChat adapter: off (set WECHAT_APPID/WECHAT_SECRET/WECHAT_TOKEN)");
    }

    // ── Platform registry (so handle_inbound can send replies) ──
    let mut platforms = PlatformRegistry::new();
    platforms.register(Arc::new(LoopbackPlatform::new())); // local dev + smoke
    if let Some(p) = &feishu_platform {
        platforms.register(p.clone());
    }
    if let Some(p) = &wechat_platform {
        platforms.register(p.clone());
    }

    // ── Gateway (holds the registry) ──
    let gateway = Arc::new(Gateway::new(
        runner,
        platforms,
        ChannelDirectory::default_root().await?,
        ProfileRoute::new(pack_name),
    ));

    // ── Start long-connection transports (outbound WSS, no public URL) ──
    if let Some(p) = &feishu_platform {
        let (cfg, http) = p.cfg_and_http();
        oneai_gateway::adapters::feishu_ws::start_long_connection(cfg, http, gateway.clone());
        println!("   Feishu long-connection: started (configure 长连接 mode in Feishu backend)");
    }
    Ok(gateway)
}

// ─── Per-pack App factory (§3.1 tail #1) ───────────────────────────────────

/// A lazily-built `App` keyed by DomainPack name. The runner asks for the App
/// for the channel's bound pack; the factory builds it on first contact.
#[async_trait]
trait AppFactory: Send + Sync {
    async fn build(&self, pack_name: &str) -> Option<oneai_app::App>;
}

struct GatewayAppFactory {
    config: OneaiConfig,
    model_config: Option<oneai_core::ModelConfig>,
    user: Option<String>,
    /// Optional cron scheduler — when set, the lazily-built App gets
    /// `.cron_provider(...)` so the `schedule` agent tool is registered
    /// (the user can say "每天9点..." in chat and the agent wires a cron job).
    cron: Option<Arc<dyn oneai_core::traits::CronScheduler>>,
}

#[async_trait]
impl AppFactory for GatewayAppFactory {
    async fn build(&self, pack_name: &str) -> Option<oneai_app::App> {
        let domain_pack = get_builtin_pack(pack_name, ".")?;
        let pack_tools = domain_pack.tools.clone();

        let mut builder = AppBuilder::new()
            .default_parser()
            .default_rate_limiter()
            .noop_interaction_gate() // headless bot → auto-approve all tools
            .trace_in_memory()
            .generation_config(self.config.generation.clone())
            .domain_pack(domain_pack)
            .sqlite_persistence(); // per-channel sessions resume the chat

        if let Some(mc) = &self.model_config {
            let provider = oneai_provider::ProviderFactory::create(mc.clone());
            builder = builder.provider(Arc::from(provider));
        }
        if let Some(uid) = &self.user {
            builder = builder.user_id(uid);
        }
        if let Some(cron) = &self.cron {
            builder = builder.cron_provider(cron.clone());
        }

        let app = builder.build().await.ok()?;

        // Register skills + domain tools + calculator (same set as cmd_run).
        let skills = oneai_skill::builtin::skills_for_domain(pack_name);
        let _ = app.skill_registry.register_builtin(skills).await;
        for tool in &pack_tools {
            let _ = app.register_tool(tool.clone()).await;
        }
        let _ = app.register_tool(Arc::new(CalculatorTool::new())).await;
        let _ = app.register_skill_tools().await;
        Some(app)
    }
}

// ─── GatewayRunner impl — drives create_session_with_id + run_agent_silent ──

/// One built `App` + the per-pack turn-serialization lock.
struct RunApp {
    app: oneai_app::App,
    lock: Mutex<()>,
}

struct AppGatewayRunner {
    factory: Arc<dyn AppFactory>,
    apps: RwLock<HashMap<String, Arc<RunApp>>>,
    has_provider: bool,
}

impl AppGatewayRunner {
    /// Resolve the (lazily-built, cached) App for `pack`, building it on
    /// first contact. Returns None if the pack isn't a known builtin.
    async fn get_or_build(&self, pack: &str) -> Option<Arc<RunApp>> {
        if let Some(a) = self.apps.read().await.get(pack) {
            return Some(a.clone());
        }
        let built = self.factory.build(pack).await?;
        let run_app = Arc::new(RunApp {
            app: built,
            lock: Mutex::new(()),
        });
        // Another task may have built the same pack concurrently; reuse the
        // winner via `or_insert` (Arc↔Arc — cheap).
        let mut apps = self.apps.write().await;
        Some(
            apps.entry(pack.to_string())
                .or_insert(run_app.clone())
                .clone(),
        )
    }

    /// The pack for the current turn — read from the gateway's task-local
    /// `SESSION_SOURCE` (set by `Gateway::handle_inbound`). Empty for legacy
    /// sessions bound before pack routing → falls back to the default pack
    /// in `ProfileRoute`.
    fn current_pack() -> String {
        oneai_gateway::SESSION_SOURCE.with(|s| s.pack.clone())
    }
}

#[async_trait]
impl GatewayRunner for AppGatewayRunner {
    async fn run_turn(&self, session_id: &str, task: &str) -> TurnOutcome {
        if !self.has_provider {
            return TurnOutcome::Rejected {
                reason: "no LLM provider configured".to_string(),
            };
        }
        let pack = Self::current_pack();
        let app = match self.get_or_build(&pack).await {
            Some(a) => a,
            None => {
                return TurnOutcome::Rejected {
                    reason: format!("no App built for pack '{pack}'"),
                }
            }
        };
        // Serialize turns within this pack to avoid the shared
        // MemoryManager.set_session_id race; cross-pack turns run in parallel.
        let _guard = app.lock.lock().await;
        let mut session = app.app.create_session_with_id(session_id).await;
        match session.run_agent_silent(task).await {
            Ok(r) => TurnOutcome::Done {
                final_answer: r.final_answer,
                completed: r.completed,
                iterations: r.iterations,
            },
            Err(e) => TurnOutcome::Error {
                message: e.to_string(),
            },
        }
    }

    /// Streaming reply path (§3.1 tail #3): wire a `StreamingRelayObserver`
    /// into `run_agent` so `on_stream_chunk` pushes assistant tokens to the
    /// gateway's `ReplySink` (coalesced → platform.send) as they're produced.
    async fn run_turn_streaming(
        &self,
        session_id: &str,
        task: &str,
        sink: Arc<dyn ReplySink>,
    ) -> TurnOutcome {
        if !self.has_provider {
            return TurnOutcome::Rejected {
                reason: "no LLM provider configured".to_string(),
            };
        }
        let pack = Self::current_pack();
        let app = match self.get_or_build(&pack).await {
            Some(a) => a,
            None => {
                return TurnOutcome::Rejected {
                    reason: format!("no App built for pack '{pack}'"),
                }
            }
        };
        let _guard = app.lock.lock().await;
        let mut session = app.app.create_session_with_id(session_id).await;
        let observer = StreamingRelayObserver { sink };
        let interrupt_slot: Arc<tokio::sync::Mutex<Option<oneai_agent::AgentLoop>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        match session.run_agent(task, &observer, interrupt_slot).await {
            Ok(r) => TurnOutcome::Done {
                final_answer: r.final_answer,
                completed: r.completed,
                iterations: r.iterations,
            },
            Err(e) => TurnOutcome::Error {
                message: e.to_string(),
            },
        }
    }

    fn supports_streaming(&self) -> bool {
        self.has_provider
    }
}

// ─── Streaming observer — wires on_stream_chunk to the gateway ReplySink ──

struct StreamingRelayObserver {
    sink: Arc<dyn ReplySink>,
}

impl oneai_agent::AgentLoopObserver for StreamingRelayObserver {
    fn on_iteration_start(&self, _: usize, _: oneai_agent::ParadigmKind) {}
    fn on_direct_answer(&self, _: &str) {}
    fn on_tool_calls(&self, _: &[oneai_agent::ToolCallRequest]) {}
    fn on_tool_result(&self, _: &str, _: &str, _: &oneai_core::ToolOutput) {}
    fn on_delegate(&self, _: &str, _: &oneai_agent::SubAgentKind) {}
    fn on_paradigm_switch(&self, _: oneai_agent::ParadigmKind) {}
    fn on_checkpoint(&self, _: usize) {}
    fn on_complete(&self, _: &oneai_agent::AgentLoopResult) {}
    fn on_stream_chunk(&self, text: &str) {
        self.sink.push(text);
    }
}

// ─── macOS LaunchAgent auto-start (§3.1 Part F) ─────────────────────────────
//
// The supervisor is the boot-time guardian that brings up the gateway (Part
// E). `oneai gateway autostart install` writes a LaunchAgent plist that runs
// `oneai supervisor serve --with-gateway` at login (RunAtLoad) and keeps it
// alive across crashes (KeepAlive). TUI + supervisor/gateway then share
// ~/.oneai/oneai.db isolated by WAL (Part A).

#[cfg(target_os = "macos")]
const LAUNCH_AGENT_LABEL: &str = "com.oneai.supervisor";

#[cfg(target_os = "macos")]
fn launch_agent_plist_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCH_AGENT_LABEL}.plist"))
}

#[cfg(target_os = "macos")]
fn default_log_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(home).join(".oneai/logs/supervisor.log")
}

/// Build the LaunchAgent plist XML. Pure (testable): `bin_path` is the
/// absolute path to the `oneai` binary (`ProgramArguments[0]`), `log_path`
/// is where stdout/stderr are redirected. Runs `supervisor serve
/// --with-gateway` so the gateway comes up on boot alongside the supervisor.
#[cfg(target_os = "macos")]
fn build_plist(bin_path: &str, log_path: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LAUNCH_AGENT_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{bin_path}</string>
    <string>supervisor</string>
    <string>serve</string>
    <string>--with-gateway</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{log_path}</string>
  <key>StandardErrorPath</key>
  <string>{log_path}</string>
</dict>
</plist>
"#
    )
}

#[cfg(target_os = "macos")]
pub fn cmd_gateway_autostart_install() {
    let bin = match std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
    {
        Some(p) => p.to_string_lossy().into_owned(),
        None => {
            eprintln!("Error: cannot resolve current executable path.");
            std::process::exit(1);
        }
    };
    let log = default_log_path();
    if let Some(parent) = log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let plist = build_plist(&bin, &log.to_string_lossy());
    let plist_path = launch_agent_plist_path();
    if let Some(parent) = plist_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&plist_path, &plist) {
        eprintln!("Error writing plist: {e}");
        std::process::exit(1);
    }
    println!("Wrote {}", plist_path.display());
    // Unload first (idempotent if not loaded), then load.
    let _ = std::process::Command::new("launchctl")
        .args(["unload", &plist_path.to_string_lossy()])
        .status();
    match std::process::Command::new("launchctl")
        .args(["load", &plist_path.to_string_lossy()])
        .status()
    {
        Ok(s) if s.success() => {
            println!(
                "LaunchAgent loaded — supervisor + gateway will start on login (logs: {}).",
                log.display()
            );
        }
        Ok(s) => {
            eprintln!("launchctl load failed (exit {:?}).", s.code());
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("launchctl not found: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(target_os = "macos")]
pub fn cmd_gateway_autostart_uninstall() {
    let plist_path = launch_agent_plist_path();
    let _ = std::process::Command::new("launchctl")
        .args(["unload", &plist_path.to_string_lossy()])
        .status();
    match std::fs::remove_file(&plist_path) {
        Ok(()) => println!("Removed {} (LaunchAgent unloaded).", plist_path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("No plist at {} (nothing to remove).", plist_path.display())
        }
        Err(e) => {
            eprintln!("Error removing plist: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(target_os = "macos")]
pub fn cmd_gateway_autostart_status() {
    match std::process::Command::new("launchctl")
        .args(["list"])
        .output()
    {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let mut found = false;
            for line in stdout.lines() {
                if line.contains(LAUNCH_AGENT_LABEL) {
                    println!("{}", line.trim());
                    found = true;
                }
            }
            if !found {
                println!("LaunchAgent '{LAUNCH_AGENT_LABEL}' is NOT loaded.");
            }
        }
        Err(e) => {
            eprintln!("launchctl not found: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn cmd_gateway_autostart_install() {
    eprintln!("autostart is macOS-only (LaunchAgent); Linux/Windows is a follow-up.");
}
#[cfg(not(target_os = "macos"))]
pub fn cmd_gateway_autostart_uninstall() {
    eprintln!("autostart is macOS-only.");
}
#[cfg(not(target_os = "macos"))]
pub fn cmd_gateway_autostart_status() {
    eprintln!("autostart is macOS-only.");
}

#[cfg(all(test, target_os = "macos"))]
mod autostart_tests {
    use super::*;

    #[test]
    fn plist_contains_required_keys() {
        let xml = build_plist("/usr/local/bin/oneai", "/tmp/oneai.log");
        assert!(xml.contains("<key>Label</key>"));
        assert!(xml.contains("com.oneai.supervisor"));
        assert!(xml.contains("<key>RunAtLoad</key>"));
        assert!(xml.contains("<true/>"));
        assert!(xml.contains("<key>KeepAlive</key>"));
        assert!(xml.contains("/usr/local/bin/oneai"));
        assert!(xml.contains("--with-gateway"));
        assert!(xml.contains("/tmp/oneai.log"));
    }
}
