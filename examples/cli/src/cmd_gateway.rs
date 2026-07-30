//! Gateway command — launch the OneAI message gateway (Feishu / WeChat / loopback)
//! as a webhook HTTP server. Builds a real `App` and supplies a `GatewayRunner`
//! impl that drives `create_session_with_id` + `run_agent_silent` per inbound
//! message — mirroring `cmd_studio`'s `StudioRunner` pattern.

use std::sync::Arc;

use async_trait::async_trait;
use oneai_app::AppBuilder;
use oneai_domain::DomainPack;
use oneai_gateway::web::{serve as serve_web, WebConfig, WebhookState};
use oneai_gateway::{
    adapters::loopback::LoopbackPlatform, ChannelDirectory, Gateway, GatewayRunner,
    PlatformRegistry, ProfileRoute, TurnOutcome,
};
use oneai_tool::CalculatorTool;
use tokio::sync::Mutex;

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
    println!("🤖 OneAI Gateway — message-platform webhook server");
    println!("   Bind:   http://{}", bind);
    println!("   Routes: POST /gateway/{{platform}}   (Feishu/WeChat/loopback)");
    println!("           GET  /gateway/wechat          (WeChat handshake)");
    println!();

    let provider_config = config.to_model_config_with_overrides(model_override);
    let has_provider = provider_config.is_some();
    if !has_provider {
        eprintln!("⚠️  No LLM provider configured (set ONEAI_API_KEY / ONEAI_BASE_URL).");
        eprintln!("   Inbound messages will be rejected with [oneai] 未能处理.\n");
    }

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

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    if let Err(e) = rt.block_on(serve(
        config,
        addr,
        provider_config,
        domain_pack,
        user,
        has_provider,
    )) {
        eprintln!("Error starting gateway: {}", e);
        std::process::exit(1);
    }
}

async fn serve(
    config: &OneaiConfig,
    addr: std::net::SocketAddr,
    model_config: Option<oneai_core::ModelConfig>,
    domain_pack: DomainPack,
    user: Option<&str>,
    has_provider: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // ── Build the App (provider optional, auto-approve, in-memory trace) ──
    let pack_name = domain_pack.name.clone();
    let pack_tools = domain_pack.tools.clone();

    let mut builder = AppBuilder::new()
        .default_parser()
        .default_rate_limiter()
        .noop_interaction_gate() // headless bot → auto-approve all tools
        .trace_in_memory()
        .generation_config(config.generation.clone())
        .domain_pack(domain_pack);

    // Persist per-channel sessions so a follow-up message resumes the chat.
    builder = builder.sqlite_persistence();

    if let Some(mc) = model_config {
        let provider = oneai_provider::ProviderFactory::create(mc);
        builder = builder.provider(Arc::from(provider));
    }
    if let Some(uid) = user {
        builder = builder.user_id(uid);
    }

    let app = builder.build().await.expect("App build failed");

    // Register skills + domain tools + calculator (same set as cmd_run/studio).
    let skills = oneai_skill::builtin::skills_for_domain(&pack_name);
    app.skill_registry.register_builtin(skills).await.unwrap();
    for tool in &pack_tools {
        app.register_tool(tool.clone()).await.unwrap();
    }
    app.register_tool(Arc::new(CalculatorTool::new()))
        .await
        .unwrap();
    app.register_skill_tools().await.unwrap();

    // ── Runner impl: per inbound message, resolve session + run a turn ──
    // A single Mutex serializes turns: the shared MemoryManager.set_session_id
    // would otherwise race across concurrent channels. Per-channel concurrency
    // (per-session memory isolation) is a follow-up.
    let runner = Arc::new(AppGatewayRunner {
        app,
        lock: Mutex::new(()),
        has_provider,
    });

    // ── Platform registry + webhook state ──
    let mut platforms = PlatformRegistry::new();

    // Loopback always on (local dev + smoke).
    platforms.register(Arc::new(LoopbackPlatform::new()));
    let mut state = WebhookState::new(Arc::new(Gateway::new(
        runner.clone(),
        platforms,
        ChannelDirectory::default_root().await?,
        ProfileRoute::new(pack_name),
    )))
    .with(Arc::new(oneai_gateway::web::LoopbackWebhookHandler));

    // Feishu adapter (if env credentials present).
    if let Some(cfg) = oneai_gateway::adapters::feishu::FeishuConfig::from_env() {
        println!("   Feishu adapter: ENABLED (app_id={})", cfg.app_id);
        state = state.with(oneai_gateway::adapters::feishu::FeishuPlatform::arc(cfg));
    } else {
        println!(
            "   Feishu adapter: off (set FEISHU_APP_ID/FEISHU_APP_SECRET/FEISHU_VERIFY_TOKEN)"
        );
    }

    // WeChat adapter (if env credentials present).
    if let Some(cfg) = oneai_gateway::adapters::wechat::WeChatConfig::from_env() {
        println!("   WeChat adapter: ENABLED (appid={})", cfg.app_id);
        state = state.with(oneai_gateway::adapters::wechat::WeChatPlatform::arc(cfg));
    } else {
        println!("   WeChat adapter: off (set WECHAT_APPID/WECHAT_SECRET/WECHAT_TOKEN)");
    }

    println!("\nListening… press Ctrl+C to stop.\n");
    serve_web(WebConfig { addr }, state).await?;
    Ok(())
}

// ─── GatewayRunner impl — drives create_session_with_id + run_agent_silent ──

struct AppGatewayRunner {
    app: oneai_app::App,
    lock: Mutex<()>,
    has_provider: bool,
}

#[async_trait]
impl GatewayRunner for AppGatewayRunner {
    async fn run_turn(&self, session_id: &str, task: &str) -> TurnOutcome {
        if !self.has_provider {
            return TurnOutcome::Rejected {
                reason: "no LLM provider configured".to_string(),
            };
        }
        // Serialize turns to avoid the shared MemoryManager session_id race.
        let _guard = self.lock.lock().await;
        let mut session = self.app.create_session_with_id(session_id).await;
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
}
