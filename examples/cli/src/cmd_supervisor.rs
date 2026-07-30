//! Supervisor command — run the headless supervisor daemon, or drive it as a
//! client (`list/spawn/stop/status/rpc/rpc-stream`).
//!
//! `serve` builds a `SupervisorRunnerImpl` (one `App` + `AppSession` per
//! spawned instance, mirroring `cmd_studio`) and hands it to
//! `oneai_supervisor::serve`, which binds the IPC socket and serves forever.
//! The client subcommands connect to a running daemon over the same socket.

use std::sync::Arc;

use oneai_agent::AgentLoop;
use oneai_app::AppBuilder;
use oneai_core::InterruptReason;
use oneai_supervisor::{
    default_socket_path, Event, InstanceHandle, InstanceSpec, InstanceStatus, SupervisorClient,
    SupervisorConfig, SupervisorRunner, TurnSummary,
};
use oneai_tool::CalculatorTool;
use tokio::sync::Mutex;
use tokio_stream::StreamExt;

use crate::cmd_pack::get_builtin_pack;
use crate::config::OneaiConfig;

/// Resolve the socket path: CLI `--socket` or the default `~/.oneai/server.sock`.
fn socket_or_default(socket: Option<&str>) -> std::path::PathBuf {
    socket
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_socket_path)
}

// ─── serve (daemon) ─────────────────────────────────────────────────────────

pub fn cmd_supervisor_serve(
    config: &OneaiConfig,
    socket: Option<&str>,
    domain: Option<&str>,
    model: Option<&str>,
    user: Option<&str>,
) {
    let socket_path = socket_or_default(socket);
    let root_dir = socket_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(oneai_supervisor::default_server_dir)
        .join("server");

    println!("🤖 OneAI Supervisor — headless agent daemon");
    println!("   Socket: {}", socket_path.display());
    println!("   Registry: {}", root_dir.display());
    if let Some(d) = domain {
        println!("   Default domain: {}", d);
    }
    println!();

    let provider_config = config.to_model_config_with_overrides(model);
    let has_provider = provider_config.is_some();
    if !has_provider {
        eprintln!("⚠️  No LLM provider configured (set ONEAI_API_KEY / ONEAI_BASE_URL).");
        eprintln!("   The daemon will start, but `spawn`/`rpc` will reject turns.\n");
    }

    let runner = Arc::new(SupervisorRunnerImpl {
        config: config.clone(),
        has_provider,
        default_user: user.map(String::from),
        default_domain: domain.map(String::from),
    }) as Arc<dyn SupervisorRunner>;

    let sup_config = SupervisorConfig {
        socket_path,
        root_dir,
    };

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    if let Err(e) = rt.block_on(oneai_supervisor::serve(sup_config, runner)) {
        eprintln!("Error running supervisor daemon: {}", e);
        std::process::exit(1);
    }
}

// ─── client subcommands ─────────────────────────────────────────────────────

pub fn cmd_supervisor_list(socket: Option<&str>) {
    let path = socket_or_default(socket);
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    match rt.block_on(async { SupervisorClient::connect(&path).await?.list().await }) {
        Ok(list) if list.is_empty() => println!("No supervised instances."),
        Ok(list) => {
            println!("{:<20} {:<10} {:<10} LAST ANSWER", "ID", "DOMAIN", "STATUS");
            for info in list {
                let status = status_str(&info.status);
                let last = info
                    .last_turn
                    .as_ref()
                    .map(|t| truncate(&t.final_answer, 40))
                    .unwrap_or_default();
                println!(
                    "{:<20} {:<10} {:<10} {}",
                    info.spec.id, info.spec.domain, status, last
                );
            }
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

pub fn cmd_supervisor_spawn(
    socket: Option<&str>,
    id: &str,
    domain: Option<&str>,
    model: Option<&str>,
    user: Option<&str>,
) {
    let path = socket_or_default(socket);
    let spec = InstanceSpec {
        id: id.to_string(),
        domain: domain.unwrap_or("coding").to_string(),
        model: model.map(String::from),
        user: user.map(String::from),
        created_at: chrono::Utc::now(),
    };
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    match rt.block_on(async {
        let client = SupervisorClient::connect(&path).await?;
        client.spawn(&spec).await
    }) {
        Ok(inst_id) => println!("Spawned instance: {}", inst_id),
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

pub fn cmd_supervisor_stop(socket: Option<&str>, id: &str) {
    let path = socket_or_default(socket);
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    match rt.block_on(async {
        let client = SupervisorClient::connect(&path).await?;
        client.stop(id).await
    }) {
        Ok(()) => println!("Stopped instance: {}", id),
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

pub fn cmd_supervisor_status(socket: Option<&str>, id: &str) {
    let path = socket_or_default(socket);
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    match rt.block_on(async {
        let client = SupervisorClient::connect(&path).await?;
        client.status(id).await
    }) {
        Ok(info) => {
            println!("ID:       {}", info.spec.id);
            println!("Domain:   {}", info.spec.domain);
            println!(
                "Model:    {}",
                info.spec.model.as_deref().unwrap_or("(default)")
            );
            println!(
                "User:     {}",
                info.spec.user.as_deref().unwrap_or("(none)")
            );
            println!("Status:   {}", status_str(&info.status));
            println!("Updated:  {}", info.updated_at);
            if let Some(t) = &info.last_turn {
                println!(
                    "Last turn: {} (iter {}, paradigm {})",
                    truncate(&t.final_answer, 60),
                    t.iterations,
                    t.active_paradigm
                );
            }
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

pub fn cmd_supervisor_rpc(socket: Option<&str>, id: &str, message: &str) {
    let path = socket_or_default(socket);
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    match rt.block_on(async {
        let client = SupervisorClient::connect(&path).await?;
        client.rpc(id, message).await
    }) {
        Ok(summary) => {
            println!("{}", summary.final_answer);
            eprintln!(
                "[{} iterations, paradigm {}, completed={}]",
                summary.iterations, summary.active_paradigm, summary.completed
            );
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

pub fn cmd_supervisor_rpc_stream(socket: Option<&str>, id: &str, message: &str) {
    let path = socket_or_default(socket);
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async move {
        let client = match SupervisorClient::connect(&path).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        };
        let mut stream = client.rpc_stream(id, message);
        while let Some(result) = stream.next().await {
            match result {
                Ok(event) => print_event(&event),
                Err(e) => eprintln!("[error] {}", e),
            }
        }
    });
}

fn print_event(event: &Event) {
    match event {
        Event::IterationStart {
            iteration,
            paradigm,
        } => {
            eprintln!("[iter {} | paradigm {}]", iteration, paradigm);
        }
        Event::StreamChunk { text } => {
            print!("{}", text);
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
        Event::DirectAnswer { text } => {
            println!("\n[answer] {}", text);
        }
        Event::ToolCalls { calls } => {
            for c in calls {
                eprintln!("[tool] {} ({})", c.tool_name, c.id);
            }
        }
        Event::ToolResult {
            tool_name,
            success,
            output_summary,
            ..
        } => {
            eprintln!(
                "[tool→{}] {} = {}",
                tool_name,
                if *success { "ok" } else { "fail" },
                output_summary
            );
        }
        Event::ParadigmSwitch { paradigm } => {
            eprintln!("[switch → {}]", paradigm);
        }
        Event::Thinking { text } => {
            eprintln!("[think] {}", text);
        }
        Event::LoopComplete { result_summary } => {
            eprintln!("[done] {}", result_summary);
        }
        Event::Delegate { task, agent_type } => {
            eprintln!("[delegate → {}] {}", agent_type, task);
        }
        Event::CheckpointSaved {
            iteration,
            checkpoint_id,
        } => {
            eprintln!("[ckpt @ {} {}]", iteration, checkpoint_id);
        }
        Event::TraceEvent { kind, name, .. } => {
            eprintln!("[trace {} {}]", kind, name);
        }
        Event::Error { message } => {
            eprintln!("[error] {}", message);
        }
        _ => {}
    }
}

fn status_str(s: &InstanceStatus) -> String {
    match s {
        InstanceStatus::Idle => "idle".to_string(),
        InstanceStatus::Running => "running".to_string(),
        InstanceStatus::Stopping => "stopping".to_string(),
        InstanceStatus::Stopped => "stopped".to_string(),
        InstanceStatus::Crashed(r) => format!("crashed({})", r),
        _ => "unknown".to_string(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{}…", t)
    }
}

// ─── SupervisorRunnerImpl — builds App+AppSession per spawn ──────────────────

struct SupervisorRunnerImpl {
    config: OneaiConfig,
    has_provider: bool,
    default_user: Option<String>,
    default_domain: Option<String>,
}

struct SupervisorInstanceHandle {
    session: Arc<Mutex<oneai_app::AppSession>>,
    interrupt_slot: Arc<Mutex<Option<AgentLoop>>>,
}

#[async_trait::async_trait]
impl SupervisorRunner for SupervisorRunnerImpl {
    fn has_provider(&self) -> bool {
        self.has_provider
    }

    async fn spawn(
        &self,
        spec: &InstanceSpec,
    ) -> Result<Arc<dyn InstanceHandle>, oneai_supervisor::SupervisorError> {
        // Resolve the domain pack for this instance (fall back to the daemon
        // default, then to coding).
        let domain_name = if spec.domain.is_empty() {
            self.default_domain
                .clone()
                .unwrap_or_else(|| "coding".to_string())
        } else {
            spec.domain.clone()
        };
        let domain_pack = match get_builtin_pack(&domain_name, ".") {
            Some(p) => p,
            None => oneai_domain::coding_pack("."),
        };

        let model_config = self
            .config
            .to_model_config_with_overrides(spec.model.as_deref());

        let mut builder = AppBuilder::new()
            .default_parser()
            .default_rate_limiter()
            .noop_interaction_gate() // headless daemon → auto-approve
            .trace_in_memory()
            .generation_config(self.config.generation.clone());

        if let Some(mc) = model_config {
            let provider = oneai_provider::ProviderFactory::create(mc);
            builder = builder.provider(Arc::from(provider));
        }
        if let Some(uid) = spec.user.as_ref().or(self.default_user.as_ref()) {
            builder = builder.user_id(uid);
        }

        let app = builder.build().await.map_err(|e| {
            oneai_supervisor::SupervisorError::Instance(format!("app build failed: {e}"))
        })?;

        let skills = oneai_skill::builtin::skills_for_domain(&domain_pack.name);
        let _ = app.skill_registry.register_builtin(skills).await;
        for tool in &domain_pack.tools {
            let _ = app.register_tool(tool.clone()).await;
        }
        let _ = app.register_tool(Arc::new(CalculatorTool::new())).await;
        let _ = app.register_skill_tools().await;

        let session = app.create_session();
        Ok(Arc::new(SupervisorInstanceHandle {
            session: Arc::new(Mutex::new(session)),
            interrupt_slot: Arc::new(Mutex::new(None)),
        }))
    }
}

#[async_trait::async_trait]
impl InstanceHandle for SupervisorInstanceHandle {
    fn status(&self) -> InstanceStatus {
        InstanceStatus::Idle
    }

    async fn run_turn(
        &self,
        task: &str,
        observer: Arc<dyn oneai_agent::AgentLoopObserver>,
    ) -> Result<TurnSummary, oneai_supervisor::SupervisorError> {
        let mut session = self.session.lock().await;
        let result = session
            .run_agent(task, observer.as_ref(), self.interrupt_slot.clone())
            .await
            .map_err(|e| oneai_supervisor::SupervisorError::Instance(e.to_string()))?;
        drop(session);
        Ok(TurnSummary::from(&result))
    }

    async fn stop(&self) {
        let slot = self.interrupt_slot.lock().await;
        if let Some(agent_loop) = slot.as_ref() {
            agent_loop.request_interrupt(InterruptReason::Custom {
                reason: "supervisor_stop".to_string(),
            });
        }
    }
}
