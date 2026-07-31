//! Cron command — durable NL/cron/ISO scheduling + external one-shot triggers
//! (Phase 3.2). Mirrors the gateway: the scheduler sits below `oneai-app`,
//! so this command builds a real `App` + the gateway (for `deliver=origin`
//! delivery) and supplies a `CronRunner` impl that routes fired jobs into
//! `Gateway::deliver_scheduled`.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use oneai_gateway::{ChannelId, Gateway};
use oneai_scheduler::error::Result as CronResult;
use oneai_scheduler::orchestrator::CronSchedulerImpl;
use oneai_scheduler::runner::{CronRunner, DeliveryOutcome};
use oneai_scheduler::store::{default_root, FileJobStore, JobStore};
use oneai_scheduler::{parse_schedule, CronJob, CronScheduler, DeliverMode};

use crate::cmd_gateway::build_gateway;
use crate::config::OneaiConfig;

// ─── add / list / rm ──────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub fn cmd_cron_add(
    name: &str,
    schedule: &str,
    task: &str,
    platform: &str,
    channel: Option<&str>,
    session: Option<&str>,
    pack: Option<&str>,
    deliver: &str,
) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        let store = match FileJobStore::new(default_root()).await {
            Ok(s) => Arc::new(s) as Arc<dyn JobStore>,
            Err(e) => {
                eprintln!("Error opening cron store: {e}");
                std::process::exit(1);
            }
        };
        let schedule = match parse_schedule(schedule) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Error parsing schedule: {e}");
                std::process::exit(1);
            }
        };
        let deliver_mode = match deliver.to_ascii_lowercase().as_str() {
            "origin" => DeliverMode::Origin,
            "silent" => DeliverMode::Silent,
            other => {
                eprintln!("--deliver must be origin|silent, got '{other}'");
                std::process::exit(1);
            }
        };
        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();
        let session_id = session
            .map(String::from)
            .unwrap_or_else(|| format!("cron-{}", &id[..8]));
        let channel = channel
            .map(String::from)
            .unwrap_or_else(|| format!("cron-{}", &id[..8]));
        let mut job = CronJob::new(id.clone(), name.to_string(), schedule);
        job.task = task.to_string();
        job.platform = platform.to_string();
        job.channel = channel;
        job.session_id = session_id;
        job.pack = pack.unwrap_or("coding").to_string();
        job.deliver = deliver_mode;
        if let Err(e) = oneai_scheduler::add_job(&store, job, now).await {
            eprintln!("Error adding job: {e}");
            std::process::exit(1);
        }
        println!("Added cron job '{name}' (id={id}).");
        println!("Run `oneai cron serve` to start the orchestrator + /cron/fire receiver.");
    });
}

pub fn cmd_cron_list() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        let store = match FileJobStore::new(default_root()).await {
            Ok(s) => Arc::new(s) as Arc<dyn JobStore>,
            Err(e) => {
                eprintln!("Error opening cron store: {e}");
                std::process::exit(1);
            }
        };
        let jobs = match store.list().await {
            Ok(j) => j,
            Err(e) => {
                eprintln!("Error listing jobs: {e}");
                std::process::exit(1);
            }
        };
        if jobs.is_empty() {
            println!("No cron jobs. Add one: oneai cron add --name .. --schedule 30m --task ..");
            return;
        }
        println!(
            "{:<38} {:<16} {:<18} {:<8} {:<20} next",
            "id", "name", "schedule", "deliver", "channel"
        );
        for j in jobs {
            println!(
                "{:<38} {:<16} {:<18} {:<8} {:<20} {}",
                j.id,
                j.name,
                schedule_str(&j),
                if j.deliver == DeliverMode::Origin {
                    "origin"
                } else {
                    "silent"
                },
                j.channel,
                j.next_fire_at
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_else(|| "—".to_string()),
            );
        }
    });
}

pub fn cmd_cron_rm(id: &str) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        let store = match FileJobStore::new(default_root()).await {
            Ok(s) => Arc::new(s) as Arc<dyn JobStore>,
            Err(e) => {
                eprintln!("Error opening cron store: {e}");
                std::process::exit(1);
            }
        };
        match store.remove(id).await {
            Ok(true) => println!("Removed cron job '{id}'."),
            Ok(false) => println!("No cron job '{id}' (nothing removed)."),
            Err(e) => {
                eprintln!("Error removing job: {e}");
                std::process::exit(1);
            }
        }
    });
}

fn schedule_str(j: &CronJob) -> String {
    match &j.schedule {
        oneai_scheduler::Schedule::Interval { interval } => {
            format!("every {}s", interval.as_secs())
        }
        oneai_scheduler::Schedule::OneShot { at } => at.to_rfc3339(),
        oneai_scheduler::Schedule::Cron { expr } => expr.clone(),
        _ => "<unknown>".to_string(),
    }
}

// ─── fire (manual one-shot) ──────────────────────────────────────────────────

pub fn cmd_cron_fire(id: &str) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        let store = match FileJobStore::new(default_root()).await {
            Ok(s) => Arc::new(s) as Arc<dyn JobStore>,
            Err(e) => {
                eprintln!("Error opening cron store: {e}");
                std::process::exit(1);
            }
        };
        let job = match store.get(id).await {
            Ok(Some(j)) => j,
            Ok(None) => {
                eprintln!("No cron job '{id}'.");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("Error loading job: {e}");
                std::process::exit(1);
            }
        };
        println!("Firing cron job '{}' (id={})…", job.name, job.id);

        let config = OneaiConfig::load_or_default();
        let model_config = config.to_model_config_with_overrides(None);
        let pack = if job.pack.is_empty() {
            "coding"
        } else {
            job.pack.as_str()
        };
        let gateway = match build_gateway(&config, model_config, pack, None, None).await {
            Ok(g) => g,
            Err(e) => {
                eprintln!("Error building gateway: {e}");
                std::process::exit(1);
            }
        };
        let runner = GatewayCronRunner::new();
        // Fire is a one-shot CLI — set the cell then deliver.
        let _ = runner.gateway_cell.set(gateway);
        match runner.deliver(&job).await {
            Ok(DeliveryOutcome::Done { reply, iterations }) => {
                println!("✅ delivered ({} iterations).", iterations);
                if !reply.is_empty() {
                    println!("reply: {reply}");
                }
            }
            Ok(DeliveryOutcome::Rejected { reason }) => {
                eprintln!("⚠️  rejected: {reason}");
                std::process::exit(1);
            }
            Ok(DeliveryOutcome::Error { message }) => {
                eprintln!("❌ error: {message}");
                std::process::exit(1);
            }
            Ok(_) => {
                eprintln!("❌ unknown delivery outcome");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("❌ delivery failed: {e}");
                std::process::exit(1);
            }
        }
    });
}

// ─── serve ───────────────────────────────────────────────────────────────────

pub fn cmd_cron_serve(
    config: &OneaiConfig,
    cron_bind: &str,
    gateway_bind: &str,
    domain: Option<&str>,
    model: Option<&str>,
    user: Option<&str>,
) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,oneai_scheduler=debug,oneai_gateway=debug"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    println!("⏰ OneAI Cron — durable scheduler + /cron/fire receiver");
    println!("   Cron receiver: http://{}", cron_bind);
    println!(
        "   Gateway:       http://{} (delivery + inbound)",
        gateway_bind
    );
    println!("   External fire: POST /cron/fire  (Authorization: Bearer $ONEAI_CRON_SECRET)");
    if std::env::var("ONEAI_CRON_SECRET")
        .map(|s| s.is_empty())
        .unwrap_or(true)
    {
        eprintln!("⚠️  ONEAI_CRON_SECRET unset — /cron/fire receiver will refuse all requests.");
        eprintln!("   Set it (e.g. export ONEAI_CRON_SECRET=$(openssl rand -hex 32)) to enable.");
    }
    println!();

    let model_config = config.to_model_config_with_overrides(model);
    let pack_name = config.default_domain_pack(domain);

    let cron_addr: std::net::SocketAddr = match cron_bind.parse() {
        Ok(a) => a,
        Err(_) => {
            eprintln!("Invalid --cron-bind '{cron_bind}'");
            std::process::exit(1);
        }
    };
    let gw_addr: std::net::SocketAddr = match gateway_bind.parse() {
        Ok(a) => a,
        Err(_) => {
            eprintln!("Invalid --gateway-bind '{gateway_bind}'");
            std::process::exit(1);
        }
    };

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    if let Err(e) = rt.block_on(async {
        // ── Cron store + runner + orchestrator (built FIRST so the gateway's
        //    lazily-built App factory can inject `.cron_provider(sched)` →
        //    the `schedule` agent tool appears in gateway chats) ──
        let store: Arc<dyn JobStore> = Arc::new(FileJobStore::new(default_root()).await?);
        // Build the concrete runner FIRST so we can set its gateway cell after
        // the gateway exists; the scheduler gets an `Arc<dyn CronRunner>` clone
        // of the *same* `GatewayCronRunner` (Arc clone — cell write is visible
        // to the scheduler's copy).
        let gw_runner = Arc::new(GatewayCronRunner::new());
        let runner_dyn: Arc<dyn CronRunner> = gw_runner.clone();
        let sched = Arc::new(CronSchedulerImpl::new(store.clone(), runner_dyn));
        let cron_dyn: Arc<dyn CronScheduler> = sched.clone();

        // ── Gateway (delivery + inbound) — factory gets the scheduler so the
        //    `schedule` tool is registered; the runner's gateway cell is set
        //    once the gateway exists (chicken-and-egg via OnceLock) ──
        let (gateway, gw_handle) = crate::cmd_gateway::run_gateway_task(
            config,
            gw_addr,
            model_config,
            &pack_name,
            user,
            Some(cron_dyn),
        )
        .await?;
        let _ = gw_runner.gateway_cell.set(gateway.clone());

        // ── Start the orchestrator ticker (after the gateway cell is set so
        //    the first tick can deliver) ──
        sched.start().await.map_err(|e| {
            Box::new(std::io::Error::other(format!("cron start: {e}")))
                as Box<dyn std::error::Error + Send + Sync>
        })?;
        println!("Cron orchestrator ticker started (30s interval).");

        // ── Start the /cron/fire receiver ──
        let secret = oneai_scheduler::oneshot::secret_from_env().unwrap_or_default();
        let fire_state = oneai_scheduler::oneshot::FireState {
            scheduler: sched.clone(),
            secret,
        };
        let fire_handle = tokio::spawn(async move {
            if let Err(e) = oneai_scheduler::oneshot::serve(
                oneai_scheduler::oneshot::FireServerConfig { addr: cron_addr },
                fire_state,
            )
            .await
            {
                eprintln!("[cron] /cron/fire receiver stopped: {e}");
            }
        });

        println!("\nCron scheduler ready. Press Ctrl+C to stop.\n");

        // Await both servers; exit when either stops.
        tokio::select! {
            r = gw_handle => {
                eprintln!("[cron] gateway stopped: {:?}", r);
            }
            r = fire_handle => {
                eprintln!("[cron] receiver stopped: {:?}", r);
            }
        }
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }) {
        eprintln!("Error starting cron: {e}");
        std::process::exit(1);
    }
}

// ─── CronRunner impl — routes fired jobs into the gateway ────────────────────

/// Holds the gateway behind a `OnceLock` so the scheduler can be constructed
/// *before* the gateway exists (the gateway's lazily-built App factory needs
/// the scheduler to register the `schedule` tool → the scheduler's runner
/// needs the gateway → chicken-and-egg). The cell is set once the gateway is
/// built; `deliver` reads it (cron never fires before `serve` starts the
/// ticker, which is after the gateway is up).
struct GatewayCronRunner {
    gateway_cell: Arc<std::sync::OnceLock<Arc<Gateway>>>,
}

impl GatewayCronRunner {
    fn new() -> Self {
        Self {
            gateway_cell: Arc::new(std::sync::OnceLock::new()),
        }
    }

    fn gateway(&self) -> &Arc<Gateway> {
        self.gateway_cell
            .get()
            .expect("gateway not set before cron delivery")
    }
}

#[async_trait]
impl CronRunner for GatewayCronRunner {
    async fn deliver(&self, job: &CronJob) -> CronResult<DeliveryOutcome> {
        use oneai_scheduler::DeliverMode;
        let gateway = self.gateway();
        if job.deliver == DeliverMode::Silent {
            // Run the turn but don't relay a reply — call the gateway runner
            // directly (no platform send).
            let outcome = gateway.runner().run_turn(&job.session_id, &job.task).await;
            return Ok(map_outcome(outcome));
        }
        // Origin: deliver into the bound channel session, reply relayed by the
        // gateway over the originating platform.
        let channel = ChannelId::new(&job.platform, &job.channel);
        let session_id = if job.session_id.is_empty() {
            format!("cron-{}", uuid::Uuid::new_v4())
        } else {
            job.session_id.clone()
        };
        let pack = if job.pack.is_empty() {
            "coding".to_string()
        } else {
            job.pack.clone()
        };
        match gateway
            .deliver_scheduled(channel, session_id, pack, job.user_id.clone(), &job.task)
            .await
        {
            Ok(()) => Ok(DeliveryOutcome::Done {
                reply: String::new(),
                iterations: 0,
            }),
            Err(e) => Ok(DeliveryOutcome::Error {
                message: e.to_string(),
            }),
        }
    }
}

fn map_outcome(outcome: oneai_gateway::TurnOutcome) -> DeliveryOutcome {
    match outcome {
        oneai_gateway::TurnOutcome::Done {
            final_answer,
            iterations,
            ..
        } => DeliveryOutcome::Done {
            reply: final_answer,
            iterations,
        },
        oneai_gateway::TurnOutcome::Rejected { reason } => DeliveryOutcome::Rejected { reason },
        oneai_gateway::TurnOutcome::Error { message } => DeliveryOutcome::Error { message },
        _ => DeliveryOutcome::Error {
            message: "unknown outcome".to_string(),
        },
    }
}

// Keep the CronScheduler trait import used (referenced in doc comments / future
// agent-tool wiring).
#[allow(unused_imports)]
use oneai_scheduler::CronScheduler as _CronSchedulerTrait;
