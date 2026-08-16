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
    default_scenarios_path, serve_all, AppConfigSnapshot, AppProbe, AppServerError, ConfigFileView,
    DomainPackInfo, DomainPackList, FileScenarioStore, ListenSpec, ProviderEntryDto, ProviderInfo,
    ProviderOpResult, SharedAppProbe, SharedScenarioStore, SkillInfo, SkillOpResult,
};
use oneai_bus::{EngineBus, EngineYield, InProcessBus};
use oneai_core::error::Result;
use oneai_core::ProviderPoolConfig;
use oneai_core::{traits::LlmProvider, Message};
use oneai_core::{CloudProviderKind, ModelConfig, ProviderType, SkillDescriptor};
use oneai_domain::{MergedDomainPack, PackRegistry};
use oneai_provider::{ProviderEntry, ProviderFactory, ProviderPool};
use oneai_skill::{SkillAuthor, SkillMetadata, SkillState};
use oneai_tool::CalculatorTool;
use tokio::sync::Mutex;

use crate::cmd_pack::get_builtin_pack;
use crate::config::{OneaiConfig, ProviderEntryConfig};

/// Init a `tracing_subscriber` that writes to stderr. The macOS app redirects
/// the sidecar's stderr to `~/.oneai/app-server-sidecar.log`, so this surfaces
/// engine activity (iterations, tool calls, approvals, errors) there for
/// debugging a stuck turn. `RUST_LOG` overrides the default filter.
pub(crate) fn init_stderr_logging() {
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
    /// The app-server's configured provider defaults (from env / `--model`),
    /// captured at startup so `start_group` can inject them into scenario
    /// members that don't bring their own provider config. Group-chat members
    /// build standalone providers off the spec (`build_member_provider`) —
    /// unlike single-agent turns, they do NOT inherit the app's provider. A
    /// web/IDE frontend can't bake its provider config into the spec (it
    /// doesn't own the server-side env), so the server injects the defaults
    /// for "inherit" members (no api_key + no base_url + empty model),
    /// mirroring the macOS app's client-side `buildGroupScenarioJSON` bake.
    provider_config: Option<ModelConfig>,
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

    async fn create_session(&mut self, id: Option<String>, workspace: Option<String>) -> String {
        let mut new = match id {
            Some(wanted) => self.app.create_session_with_id(&wanted).await,
            None => self.app.create_session(),
        };
        // Persist the workspace the frontend bound this session to (a fresh
        // chat created under a chosen working directory). Only acts when Some;
        // re-opening an existing session keeps its stored workspace.
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
        // No provider configured for the app-server at all → group members
        // can't infer one (they build off the spec, not the app's provider).
        // Surface a clear, actionable error instead of a confusing network
        // failure to api.openai.com mid-turn.
        let config = match &self.provider_config {
            Some(c) => c.clone(),
            None => {
                return Err(oneai_core::error::OneAIError::Config(
                    "no LLM provider configured for group chat; start the app-server with \
                     ONEAI_API_KEY / ONEAI_BASE_URL / ONEAI_MODEL (or --model)"
                        .into(),
                ));
            }
        };
        // BusGroupScenario → the uniffi ScenarioSpecView (same field shape by
        // design) → per-member providers + shared app resources.
        let mut spec = ScenarioSpecView::from(&scenario);
        inject_provider_defaults(&mut spec, &config);
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

/// Inject the app-server's configured provider defaults into scenario members
/// that don't bring their own. A member is an "inherit" member (full inherit
/// of the app's provider, including `kind`) when it carries no api_key, no
/// base_url, and an empty model — the shape of every preset. Members that
/// bring a partial override (e.g. their own api_key) keep their `kind` and
/// only fill the missing fields. Maps `CloudProviderKind` → the
/// `build_member_provider` kind string; Gemini group chat isn't wired there
/// yet, so for Gemini we leave `kind` alone (best-effort) rather than crash
/// the build.
fn inject_provider_defaults(spec: &mut ScenarioSpecView, config: &ModelConfig) {
    let app_kind = match config.cloud_kind {
        Some(CloudProviderKind::OpenAI) => Some("openai"),
        Some(CloudProviderKind::Anthropic) => Some("anthropic"),
        // Gemini has no build_member_provider arm; leave the member's kind.
        Some(CloudProviderKind::Gemini) | None => None,
    };
    for m in &mut spec.members {
        let full_inherit = m.api_key.is_none() && m.base_url.is_none() && m.model.is_empty();
        if full_inherit {
            if let Some(kind) = app_kind {
                m.kind = kind.to_string();
            }
        }
        if m.api_key.is_none() {
            m.api_key = config.api_key.clone();
        }
        if m.base_url.is_none() {
            m.base_url = config.base_url.clone();
        }
        if m.model.is_empty() {
            if let Some(name) = &config.model_name {
                m.model = name.clone();
            }
        }
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

// ─── FeedbackStore impl ─────────────────────────────────────────────────────
//
// Backs the `feedback/submit` + `feedback/list` JSON-RPC methods (sync CRUD —
// no bus). Same shape as `AppConversationStore`: wraps the same `Arc<App>` the
// runtime drives, delegating to `App::record_feedback` / `list_feedback`,
// which in turn hit the shared SQLite store (`ONEAI_DB_PATH`). `App` swallows
// backend errors, so `list` returns an empty vec on failure and `record` is a
// silent no-op — never a panic, never a turn failure.

struct AppFeedbackStore {
    app: Arc<App>,
}

#[async_trait::async_trait]
impl oneai_app_server::FeedbackStore for AppFeedbackStore {
    async fn record(
        &self,
        session_id: &str,
        turn_id: &str,
        message_role: &str,
        kind: &str,
        text: Option<&str>,
    ) {
        self.app
            .record_feedback(session_id, turn_id, message_role, kind, text)
            .await;
    }

    async fn list(&self, session_id: &str) -> Vec<oneai_core::FeedbackEntry> {
        self.app.list_feedback(session_id).await
    }
}

// ─── AppProbeImpl — backs the `config/*` / `provider/*` / `domainpack/*` /
//     `skill/*` JSON-RPC methods. A read-only view of the running config +
//     the genuinely hot-switchable skill lifecycle (pin/unpin/archive/restore
//     via `App.skill_curator`). DomainPack/provider changes restart the
//     app-server (`--domain` / `--model`) — there's no live hot-swap path
//     (`App.domain_pack` / `App.provider` are immutable `Arc`s), so those are
//     NOT exposed as mutable ops here. ──────────────────────────────────────

struct AppProbeImpl {
    app: Arc<App>,
    /// Launch-time `--domain` pack name (cleaner than the merged-pack
    /// concatenated name for display).
    domain_pack_name: Option<String>,
    /// Launch-time provider config (env / `--model`) — kept for group-chat
    /// member injection + the config snapshot's provider fields.
    provider_config: Option<ModelConfig>,
    /// The live provider pool (`App.provider` is this `Arc<dyn LlmProvider>`).
    /// `provider/add`/`delete`/`set_active` mutate this live; the pool routes
    /// each inference to its `active_index` entry, so a live switch takes
    /// effect on the next turn with no `App.provider` swap.
    pool: Option<Arc<ProviderPool>>,
}

/// Render a `ModelConfig` as a kind string — the cloud provider kind
/// (openai/anthropic/ollama/gemini) when set, else the provider_type
/// (cloud/local/...).
fn provider_kind_str(mc: &ModelConfig) -> Option<String> {
    if let Some(kind) = mc.cloud_kind {
        return Some(cloud_kind_str(kind).to_string());
    }
    Some(provider_type_str(mc.provider_type).to_string())
}

fn cloud_kind_str(kind: CloudProviderKind) -> &'static str {
    match kind {
        CloudProviderKind::OpenAI => "openai",
        CloudProviderKind::Anthropic => "anthropic",
        CloudProviderKind::Gemini => "gemini",
    }
}

fn provider_type_str(t: ProviderType) -> &'static str {
    match t {
        ProviderType::Cloud => "cloud",
        ProviderType::Local => "local",
        ProviderType::Transformers => "transformers",
    }
}

/// Map a `SkillState` to its wire string (matches serde `snake_case`).
fn skill_state_str(s: SkillState) -> &'static str {
    match s {
        SkillState::Active => "active",
        SkillState::Stale => "stale",
        SkillState::Archived => "archived",
        _ => "unknown",
    }
}

fn skill_author_str(a: SkillAuthor) -> &'static str {
    match a {
        SkillAuthor::User => "user",
        SkillAuthor::Agent => "agent",
        SkillAuthor::Bundled => "bundled",
        _ => "unknown",
    }
}

/// Build a `SkillInfo` from a descriptor + optional metadata.
fn skill_info(desc: &SkillDescriptor, meta: Option<&SkillMetadata>) -> SkillInfo {
    let m = meta.cloned().unwrap_or_default();
    SkillInfo {
        name: desc.name.clone(),
        description: if desc.description.is_empty() {
            None
        } else {
            Some(desc.description.clone())
        },
        use_count: m.use_count,
        pinned: m.pinned,
        state: skill_state_str(m.state).to_string(),
        origin: Some(skill_author_str(m.created_by).to_string()),
    }
}

#[async_trait::async_trait]
impl AppProbe for AppProbeImpl {
    async fn config(&self) -> AppConfigSnapshot {
        let permission_profile = self
            .app
            .domain_pack()
            .map(|p: &Arc<MergedDomainPack>| p.permission_profile.name.clone());
        let (kind, model, base_url) = match &self.provider_config {
            Some(mc) => (
                provider_kind_str(mc),
                mc.model_name.clone(),
                mc.base_url.clone(),
            ),
            None => (None, None, None),
        };
        AppConfigSnapshot {
            domain_pack: self.domain_pack_name.clone(),
            provider_kind: kind,
            provider_model: model,
            base_url,
            plan_mode: false,
            permission_profile,
        }
    }

    async fn providers(&self) -> Vec<ProviderInfo> {
        // The live pool is the source of truth — it reflects `provider/add`/
        // `delete` mutations done at runtime. Each entry's `active` flag is
        // whether the pool currently routes to it (`active_index`).
        self.provider_list()
    }

    async fn domainpacks(&self) -> DomainPackList {
        let registry = PackRegistry::default_path();
        let available: Vec<DomainPackInfo> = registry
            .list_builtin()
            .iter()
            .map(|e| DomainPackInfo {
                name: e.name.clone(),
                description: if e.description.is_empty() {
                    None
                } else {
                    Some(e.description.clone())
                },
            })
            .collect();
        DomainPackList {
            active: self.domain_pack_name.clone(),
            available,
        }
    }

    async fn skills(&self) -> Vec<SkillInfo> {
        let descs = self.app.skill_registry.list().await;
        let metas = match self.app.skill_metadata_store.as_ref() {
            Some(store) => store.list().await,
            None => std::collections::HashMap::new(),
        };
        descs
            .iter()
            .map(|d| skill_info(d, metas.get(&d.name)))
            .collect()
    }

    async fn skill_pin(&self, name: &str) -> SkillOpResult {
        self.skill_op(name, |c| async move { c.pin(name).await }, "pin")
            .await
    }
    async fn skill_unpin(&self, name: &str) -> SkillOpResult {
        self.skill_op(name, |c| async move { c.unpin(name).await }, "unpin")
            .await
    }
    async fn skill_archive(&self, name: &str) -> SkillOpResult {
        self.skill_op(name, |c| async move { c.archive(name).await }, "archive")
            .await
    }
    async fn skill_restore(&self, name: &str) -> SkillOpResult {
        self.skill_op(name, |c| async move { c.restore(name).await }, "restore")
            .await
    }

    async fn provider_add(&self, entry: ProviderEntryDto) -> ProviderOpResult {
        let name = entry.name.clone();
        // 1. Persist to config.toml ([[providers]]).
        let mut cfg = OneaiConfig::load_or_default();
        cfg.add_provider(dto_to_entry_config(&entry));
        if let Err(e) = cfg.save() {
            return ProviderOpResult {
                ok: false,
                providers: None,
                error: Some(format!("save config: {e}")),
            };
        }
        // 2. Add live to the pool (immediately switchable).
        if let Some(pool) = self.pool.as_ref() {
            let mc = entry_to_model_config_strict(&entry);
            let provider = ProviderFactory::create(mc);
            pool.add_entry(ProviderEntry::new(name.clone(), Arc::from(provider), 0));
        }
        ProviderOpResult {
            ok: true,
            providers: Some(self.provider_list()),
            error: None,
        }
    }

    async fn provider_delete(&self, name: &str) -> ProviderOpResult {
        let mut cfg = OneaiConfig::load_or_default();
        let removed = cfg.remove_provider(name);
        if !removed {
            return ProviderOpResult {
                ok: false,
                providers: Some(self.provider_list()),
                error: Some(format!("unknown provider: {name}")),
            };
        }
        if let Err(e) = cfg.save() {
            return ProviderOpResult {
                ok: false,
                providers: None,
                error: Some(format!("save config: {e}")),
            };
        }
        if let Some(pool) = self.pool.as_ref() {
            pool.remove_entry(name);
        }
        ProviderOpResult {
            ok: true,
            providers: Some(self.provider_list()),
            error: None,
        }
    }

    async fn provider_set_active(&self, name: &str) -> ProviderOpResult {
        // 1. Live pool switch (atomic active_index) — takes effect next turn.
        if let Some(pool) = self.pool.as_ref() {
            if let Err(e) = pool.set_active_by_name(name) {
                return ProviderOpResult {
                    ok: false,
                    providers: Some(self.provider_list()),
                    error: Some(e),
                };
            }
        }
        // 2. Persist `active_provider` to config.toml (launch default).
        let mut cfg = OneaiConfig::load_or_default();
        cfg.set_active_provider(name);
        if let Err(e) = cfg.save() {
            return ProviderOpResult {
                ok: false,
                providers: None,
                error: Some(format!("save config: {e}")),
            };
        }
        ProviderOpResult {
            ok: true,
            providers: Some(self.provider_list()),
            error: None,
        }
    }

    async fn config_read(&self) -> ConfigFileView {
        let path = OneaiConfig::default_path();
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        ConfigFileView {
            path: path.display().to_string(),
            content,
        }
    }
}

impl AppProbeImpl {
    /// Run a curator op by name, returning the post-op `SkillInfo` (or an
    /// error message when no curator / unknown skill).
    async fn skill_op<F, Fut>(&self, name: &str, op: F, label: &str) -> SkillOpResult
    where
        F: FnOnce(Arc<oneai_skill::SkillCurator>) -> Fut,
        Fut: std::future::Future<Output = oneai_skill::SkillMetadata>,
    {
        let curator = match self.app.skill_curator.as_ref() {
            Some(c) => c.clone(),
            None => {
                return SkillOpResult {
                    ok: false,
                    skill: None,
                    error: Some(format!("skill {label} unavailable: no curator")),
                }
            }
        };
        // The curator op errors (unknown skill) surface as a panic-free
        // empty/unchanged metadata; detect unknown skills by checking the
        // registry first.
        if curator.registry().find_by_name(name).await.is_none() {
            return SkillOpResult {
                ok: false,
                skill: None,
                error: Some(format!("unknown skill: {name}")),
            };
        }
        let meta = op(curator).await;
        let desc = self.app.skill_registry.find_by_name(name).await;
        let info = desc.as_ref().map(|d| skill_info(d, Some(&meta)));
        SkillOpResult {
            ok: true,
            skill: info,
            error: None,
        }
    }

    /// Sync helper: build the `ProviderInfo` list from the live pool (or the
    /// launch config fallback). Shared by `providers()` and the provider op
    /// results so both stay consistent.
    fn provider_list(&self) -> Vec<ProviderInfo> {
        let active_name = self.pool.as_ref().map(|p| p.active_provider_name());
        let pool_entries: Vec<(String, ModelConfig)> = self
            .pool
            .as_ref()
            .map(|p| p.provider_entries_view())
            .unwrap_or_default();
        if !pool_entries.is_empty() {
            return pool_entries
                .into_iter()
                .map(|(name, mc)| ProviderInfo {
                    kind: provider_kind_str(&mc).unwrap_or_else(|| name.clone()),
                    model: mc.model_name.clone().unwrap_or_default(),
                    base_url: mc.base_url.clone(),
                    active: active_name.as_deref() == Some(name.as_str()),
                })
                .collect();
        }
        self.provider_config
            .as_ref()
            .map(|mc| ProviderInfo {
                kind: provider_kind_str(mc).unwrap_or_default(),
                model: mc.model_name.clone().unwrap_or_default(),
                base_url: mc.base_url.clone(),
                active: true,
            })
            .into_iter()
            .collect()
    }
}

/// Map a probe DTO to a config `ProviderEntryConfig` (for `[[providers]]`).
fn dto_to_entry_config(dto: &ProviderEntryDto) -> ProviderEntryConfig {
    ProviderEntryConfig {
        name: dto.name.clone(),
        kind: dto.kind.clone(),
        api_key: dto.api_key.clone(),
        base_url: dto.base_url.clone(),
        model: dto.model.clone(),
    }
}

/// Map a probe DTO to a `ModelConfig` for live pool entry construction. Env
/// vars (`ONEAI_API_KEY`/`BASE_URL`/`MODEL`) only fill in unset fields, so a
/// fully-specified entry is self-contained.
fn entry_to_model_config_strict(dto: &ProviderEntryDto) -> ModelConfig {
    use oneai_core::{CloudProviderKind, ProviderType};
    let cloud_kind = dto
        .kind
        .as_deref()
        .and_then(|k| match k.to_lowercase().as_str() {
            "openai" => Some(CloudProviderKind::OpenAI),
            "anthropic" => Some(CloudProviderKind::Anthropic),
            "gemini" => Some(CloudProviderKind::Gemini),
            _ => None,
        });
    let api_key = dto
        .api_key
        .clone()
        .or_else(|| std::env::var("ONEAI_API_KEY").ok());
    let base_url = dto
        .base_url
        .clone()
        .or_else(|| std::env::var("ONEAI_BASE_URL").ok());
    let model = dto
        .model
        .clone()
        .or_else(|| std::env::var("ONEAI_MODEL").ok());
    ModelConfig {
        provider_type: ProviderType::Cloud,
        cloud_kind,
        api_key,
        base_url,
        port: None,
        model_name: model,
        model_path: None,
        extra: std::collections::HashMap::new(),
    }
}

// ─── EngineServer ────────────────────────────────────────────────────────────
//
// The shared engine build: identical whether the frontend wire is multi-
// transport JSON-RPC (`app-server`) or the single-port HTTP+ws `web` server.
// `build_engine_server` constructs the App (provider pool, SQLite
// persistence, working-state, domain pack, skills/tools), the AppSession, the
// in-process bus, the directive pump (detached — owned by the tokio runtime),
// and the three frontend-facing handles a transport needs (conversation /
// feedback / scenario stores + the config probe). Both `cmd_app_server` and
// `cmd_web` call this so there is one place that wires the engine.

/// The handles a transport binds against. The pump + `AppServerRuntime` +
/// `App` Arc stay alive inside the (detached) pump task — these four are the
/// transport-facing surface.
pub(crate) struct EngineServer {
    pub bus: Arc<InProcessBus>,
    pub scenario_store: SharedScenarioStore,
    pub conversation_store: oneai_app_server::SharedConversationStore,
    pub feedback_store: oneai_app_server::SharedFeedbackStore,
    pub probe: SharedAppProbe,
}

/// Build the engine + pump + stores + probe. `provider_config` is the launch
/// provider (env / `--model`) computed once by the caller (it also needs it
/// for the startup banner). The pump is spawned detached — the tokio runtime
/// owns it for the process lifetime (the returned `EngineServer` keeps the
/// stores/probe, which hold `Arc<App>` clones, alive).
pub(crate) async fn build_engine_server(
    config: &OneaiConfig,
    provider_config: Option<ModelConfig>,
    domain: Option<&str>,
    user: Option<&str>,
) -> std::result::Result<EngineServer, Box<dyn std::error::Error + Send + Sync>> {
    // Engine bus — wires the InProcessBus + BusInteractionGate (approvals
    // surface as EngineYield::ApprovalRequest ↔ Directive::Approve).
    let (builder, directive_rx) = AppBuilder::new()
        .default_parser()
        .default_rate_limiter()
        .engine_bus();
    let mut builder = builder.generation_config(config.generation.clone());
    // Clone before the move into ProviderFactory::create — start_group
    // injects these defaults into group-chat members that lack their own.
    let runtime_provider_config = provider_config.clone();
    // Build the provider POOL (the `App.provider`). Multi-provider
    // (`[[providers]]`) when configured, else a single-entry pool from the
    // legacy/env provider (model override applied via `provider_config`).
    let pool: Option<Arc<ProviderPool>> = if !config.providers.is_empty() {
        let entries: Vec<ProviderEntry> = config
            .to_pool_model_configs()
            .into_iter()
            .map(|(name, mc)| ProviderEntry::new(name, Arc::from(ProviderFactory::create(mc)), 0))
            .collect();
        let pool = ProviderPool::new(entries, ProviderPoolConfig::default());
        if let Some(active) = &config.active_provider {
            let _ = pool.set_active_by_name(active);
        }
        Some(Arc::new(pool))
    } else {
        provider_config.as_ref().map(|mc| {
            Arc::new(ProviderPool::single(
                Arc::from(ProviderFactory::create(mc.clone())),
                "default",
            ))
        })
    };
    let probe_pool = pool.clone();
    if let Some(p) = pool {
        builder = builder.provider(p);
    }
    if let Some(uid) = user {
        builder = builder.user_id(uid);
    }
    // SQLite persistence. Default path is ~/.oneai/oneai.db, but when a
    // host (the macOS app's sidecar spawn) sets ONEAI_DB_PATH, persist at
    // that path instead — so the sidecar shares the SAME DB the in-process
    // FFI engine writes. Switching transports then never loses history.
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
    let domain_pack =
        get_builtin_pack(domain_pack_name, ".").unwrap_or_else(|| oneai_domain::coding_pack("."));
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

    let conversation_store: oneai_app_server::SharedConversationStore =
        Arc::new(AppConversationStore { app: app.clone() });
    let feedback_store: oneai_app_server::SharedFeedbackStore =
        Arc::new(AppFeedbackStore { app: app.clone() });
    let probe: SharedAppProbe = Arc::new(AppProbeImpl {
        app: app.clone(),
        domain_pack_name: domain.map(|d| d.to_string()),
        provider_config: runtime_provider_config.clone(),
        pool: probe_pool,
    });
    let runtime = Arc::new(Mutex::new(AppServerRuntime {
        app: app.clone(),
        session,
        group: None,
        provider_config: runtime_provider_config,
    }));
    let interrupt_slot: Arc<Mutex<Option<AgentLoop>>> = Arc::new(Mutex::new(None));

    // Engine driver — the shared pump (detached; tokio owns it). Drains
    // Directive::UserMessage → run_turn_via_bus; Shutdown stops it.
    let _pump = oneai_app::spawn_directive_pump(directive_rx, runtime, interrupt_slot, bus.clone());

    // Shared scenario library — `~/.oneai/scenarios.json` (seeded with builtin
    // presets on first run). A bad file degrades to an in-memory store so the
    // transport still binds.
    let scenario_store: SharedScenarioStore = {
        let path = default_scenarios_path();
        match FileScenarioStore::new(path.clone()).await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                eprintln!("⚠️  scenario store at {} unavailable: {e}", path.display());
                eprintln!("   scenario/* methods will fail; other transports unaffected.");
                Arc::new(oneai_app_server::InMemoryScenarioStore::new())
            }
        }
    };

    Ok(EngineServer {
        bus,
        scenario_store,
        conversation_store,
        feedback_store,
        probe,
    })
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
        let es = build_engine_server(config, provider_config, domain, user).await?;

        // Multi-transport JSON-RPC server. Binds all `--listen` specs
        // concurrently against the one bus.
        let server = serve_all(
            specs,
            es.bus,
            es.scenario_store,
            es.conversation_store,
            es.feedback_store,
            es.probe,
        )
        .await?;

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
