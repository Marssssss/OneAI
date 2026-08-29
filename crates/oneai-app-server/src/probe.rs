//! App probe — the read-only + skill-lifecycle handle the app-server threads
//! to the `config/get` · `provider/list` · `domainpack/list` · `skill/*`
//! JSON-RPC methods.
//!
//! Mirrors [`crate::scenario::ScenarioStore`] / [`crate::conversation::ConversationStore`]
//! in shape: an object-safe `#[async_trait]` the app-server holds as
//! `Arc<dyn AppProbe + Send + Sync>`, so the crate stays decoupled from
//! `oneai-app` (it never touches an `App` directly — the CLI passes a concrete
//! impl wrapping `Arc<App>`). The DTOs are plain serde structs of primitives so
//! this crate depends on neither `oneai-skill` nor `oneai-domain`; the CLI impl
//! converts from the real types.
//!
//! ## Architectural honesty
//!
//! `App.domain_pack` is `Option<Arc<MergedDomainPack>>` (immutable, held by the
//! live session/AgentLoop) and `App.provider` is `Option<Arc<dyn LlmProvider>>`
//! (immutable). There is **no** runtime hot-swap path for either. So this trait
//! exposes only:
//!  - **read-only** snapshots of the active pack/provider and the available
//!    builtin packs (`config`/`providers`/`domainpacks`); and
//!  - the genuinely hot-switchable **skill** lifecycle (`pin`/`unpin`/`archive`/
//!    `restore`) — those mutate `SkillMetadataStore`, which is a separate
//!    `Arc<RwLock<…>>`-backed store not bound to the live session's paradigm.
//!
//! `domainpack/switch` and `provider/add` are deliberately NOT here — a switch
//! requires restarting the app-server with `--domain` / `--model`. Surfacing
//! them as fake hot-swap RPCs would be a lie; the settings UI shows them as
//! restart-required instead.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Read-only snapshot of the app's running configuration.
#[derive(Debug, Clone, Default, Serialize)]
pub struct AppConfigSnapshot {
    /// Active DomainPack name (e.g. "coding"), or null if none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain_pack: Option<String>,
    /// Configured provider kind (e.g. "openai"/"anthropic"/"ollama"), or null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_kind: Option<String>,
    /// Configured model name, or null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_model: Option<String>,
    /// Provider base URL override, or null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Whether plan mode is the default for new sessions.
    pub plan_mode: bool,
    /// A short label for the active permission profile (e.g. "standard"), or null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_profile: Option<String>,
    /// The persisted thinking-effort tier label (e.g. "off"/"low"/"medium"/
    /// "high"/"max"), or null if no store is wired (legacy path). Set by
    /// `AppProbe::config()` from the `ThinkingEffortStore`; the web settings
    /// panel shows + edits it (writes go via `thinking/set`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_effort: Option<String>,
}

/// One configured provider entry.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ProviderInfo {
    /// Unique entry name — the key `provider/set_active` / `provider/delete`
    /// operate on (issue #37: two entries may share a `kind`, and the name is
    /// whatever the user picked at add time, so the UI must address entries by
    /// name, never by kind).
    pub name: String,
    /// Provider kind (openai/anthropic/gemini/ollama/…).
    pub kind: String,
    /// Model name (may be empty when inherited).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// Base URL override, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Whether this is the entry the pool currently routes to (the active one).
    pub active: bool,
}

/// The available + active DomainPacks.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DomainPackList {
    /// Name of the active pack, or null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    /// Builtin + discovered packs the app could launch with.
    pub available: Vec<DomainPackInfo>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DomainPackInfo {
    /// Pack id/name.
    pub name: String,
    /// Short human description, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One skill's descriptor + lifecycle metadata, merged.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SkillInfo {
    /// Skill name (id).
    pub name: String,
    /// One-line description, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Number of times the skill has been surfaced/used.
    #[serde(default)]
    pub use_count: u64,
    /// Whether the skill is pinned (exempt from auto-retirement).
    #[serde(default)]
    pub pinned: bool,
    /// Lifecycle state: "active" / "stale" / "archived".
    #[serde(default = "default_skill_state")]
    pub state: String,
    /// Origin: "bundled" / "project" / "trusted".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

#[allow(dead_code)] // referenced by the serde `default = "default_skill_state"` attribute
fn default_skill_state() -> String {
    "active".to_string()
}

/// Result of a skill lifecycle op (the post-op skill state).
#[derive(Debug, Clone, Serialize)]
pub struct SkillOpResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill: Option<SkillInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A provider entry the user adds via the settings UI — written to
/// `~/.oneai/config.toml` `[[providers]]` and added live to the running
/// `ProviderPool` (so it's immediately switchable from the composer).
/// Decoupled from `oneai_core::ModelConfig` so this crate depends on neither
/// `oneai-provider` nor `oneai-core`'s provider enums.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderEntryDto {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Result of a provider lifecycle op (add/delete/set_active). `ok:false` is a
/// normal result (the UI renders the error), NOT a JSON-RPC error.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderOpResult {
    pub ok: bool,
    /// The post-op provider list (so the UI updates without a re-list).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub providers: Option<Vec<ProviderInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Query for `provider/models` — describes the endpoint whose model list the
/// UI wants (typically the NOT-yet-submitted add-provider form's kind /
/// api_key / base_url fields). Unset fields inherit the engine's env
/// (`ONEAI_API_KEY` / `ONEAI_BASE_URL`) in the CLI impl, matching what
/// `provider/add` would do.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderModelsQuery {
    /// Configured entry name — when present, resolve the entry's full stored
    /// config (incl. its config.toml api_key) and list THAT endpoint's models
    /// (the composer's per-provider model switcher, issue #41). When absent,
    /// use the kind/api_key/base_url fields (the add-provider form).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Protocol kind ("openai"/"anthropic"/"gemini"/"ollama"); absent ⇒
    /// auto-detect from the base URL host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

/// Query for `provider/detect` (issue #41) — auto-detect the protocol family +
/// normalized base URL from a bare `base_url`, no API key required.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderDetectQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

/// Result of `provider/detect` — the wire `kind`, its display label, and the
/// normalized base URL the engine will actually request against.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ProviderDetectResult {
    /// Detected protocol kind ("openai"/"anthropic"/"gemini"/"ollama").
    pub kind: String,
    /// Human protocol label (e.g. "OpenAI Completions").
    pub label: String,
    /// The normalized base URL (version segment ensured / endpoint stripped).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub base_url: String,
}

/// Result of `provider/models` (issue #37 — the model dropdown's data
/// source). `ok:false` + `error` is a normal result (the UI shows a hint and
/// leaves manual model-name entry available), NOT a JSON-RPC error.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderModelsResult {
    pub ok: bool,
    /// Model ids the endpoint serves (sorted). Empty on failure.
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Raw config-file view (path + contents) for the settings "open config file"
/// affordance — web can't reveal Finder, so it shows the path + a read-only
/// preview.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigFileView {
    pub path: String,
    /// Empty when the file doesn't exist yet.
    #[serde(default)]
    pub content: String,
}

/// One background sub-agent task (for the `background/list` RPC + web
/// `BackgroundTasksBar`). Decoupled from `oneai_agent::TaskInfo` /
/// `SubAgentKind` so this crate depends on neither — the CLI impl converts.
#[derive(Debug, Clone, Default, Serialize)]
pub struct BackgroundTaskInfoDto {
    /// The task id (the model's, or an assigned `bg_task_N`).
    pub id: String,
    /// The sub-agent kind serialized as a label ("code" / "explore" / …, or a
    /// `Custom:<role>` string for custom kinds).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub kind: String,
    /// The task spec text the parent delegated.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// "running" / "completed" / "failed" / "cancelled".
    pub status: String,
    /// On a failed task, the failure detail (empty otherwise).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
}

/// Result of a background-task cancel op. `ok:false` is a normal result (the
/// UI surfaces the error), NOT a JSON-RPC error — e.g. an unknown task id.
#[derive(Debug, Clone, Serialize)]
pub struct BackgroundTaskOpResult {
    pub ok: bool,
    /// For `cancel_all`, how many running tasks were cancelled; `None` for a
    /// single `cancel` (which targets one id) or when nothing ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancelled_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Result of `session/trajectory` (issue #40) — the persisted bus-event log
/// of one session, replayed by the frontend to rebuild a historical
/// trajectory. `events` are the raw serialized `EngineYield` JSON lines in
/// append order; empty when the session has no log (or no store is wired).
#[derive(Debug, Clone, Default, Serialize)]
pub struct SessionTrajectoryResult {
    pub ok: bool,
    pub events: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The app-probe trait. Object-safe via `#[async_trait]`; the production impl
/// (in `cmd_app_server`) wraps `Arc<oneai_app::App>`.
#[async_trait]
pub trait AppProbe: Send + Sync {
    /// Read-only running-config snapshot.
    async fn config(&self) -> AppConfigSnapshot;
    /// Configured provider(s) + pool members (read-only).
    async fn providers(&self) -> Vec<ProviderInfo>;
    /// Available + active DomainPacks (read-only).
    async fn domainpacks(&self) -> DomainPackList;
    /// All skills with lifecycle metadata (read-only).
    async fn skills(&self) -> Vec<SkillInfo>;
    /// Pin a skill (exempt from auto-retirement). Returns the post-op state.
    async fn skill_pin(&self, name: &str) -> SkillOpResult;
    /// Unpin a skill.
    async fn skill_unpin(&self, name: &str) -> SkillOpResult;
    /// Archive a skill (retire it from the surfaced set).
    async fn skill_archive(&self, name: &str) -> SkillOpResult;
    /// Restore an archived skill.
    async fn skill_restore(&self, name: &str) -> SkillOpResult;
    /// Add a provider — writes to config.toml AND adds it live to the running
    /// pool (immediately switchable). Returns the post-op provider list.
    async fn provider_add(&self, entry: ProviderEntryDto) -> ProviderOpResult;
    /// Delete a provider — writes to config.toml + removes it live. Returns the
    /// post-op provider list.
    async fn provider_delete(&self, name: &str) -> ProviderOpResult;
    /// Live-switch the active provider (atomic pool active_index) + write
    /// `active_provider` to config. Returns the post-op provider list.
    async fn provider_set_active(&self, name: &str) -> ProviderOpResult;
    /// List the models served by the endpoint described by `query` (the
    /// add-provider form fields, or a configured entry by `name`) — the
    /// settings UI's model dropdown data source (issue #37/#41). Backed by
    /// `LlmProvider::list_models`.
    async fn provider_models(&self, query: ProviderModelsQuery) -> ProviderModelsResult;
    /// Auto-detect the protocol family + normalized base URL from a bare
    /// `base_url` (issue #41). No API key required.
    async fn provider_detect(&self, query: ProviderDetectQuery) -> ProviderDetectResult;
    /// Update a provider entry by name — writes to config.toml (replacing the
    /// entry) and rebuilds the live pool entry (preserving priority/active).
    /// `api_key: None` retains the stored key.
    async fn provider_update(&self, entry: ProviderEntryDto) -> ProviderOpResult;
    /// Set the model of a provider entry by name — writes to config.toml +
    /// rebuilds the live pool entry. Preserves priority + active status.
    async fn provider_set_model(&self, name: &str, model: &str) -> ProviderOpResult;
    /// Read the raw config file (path + contents) for the "open config file"
    /// affordance.
    async fn config_read(&self) -> ConfigFileView;
    /// The persisted thinking-effort tier (web UI "思考程度" toggle). The
    /// production impl reads the shared `ThinkingEffortStore`; the returned
    /// tier is what the main agent + sub-agents (capped per-kind) use on the
    /// next turn. Defaults to `Medium` when no store is wired.
    async fn thinking_effort(&self) -> oneai_core::ThinkingEffort;
    /// Persist a new thinking-effort tier. Hot-swaps immediately — the next
    /// turn's main agent + new sub-agents read the new value. Idempotent.
    async fn set_thinking_effort(&self, effort: oneai_core::ThinkingEffort);
    /// List in-flight + settled background sub-agent tasks (for the web
    /// `BackgroundTasksBar`). Reaches the app-level `BackgroundTaskRegistry`
    /// — a task launched by an earlier turn (whose per-turn runner is gone)
    /// still appears here. Empty when no engine bus / registry is wired.
    async fn list_background_tasks(&self) -> Vec<BackgroundTaskInfoDto>;
    /// Cancel one background sub-agent by `task_id` (graceful child-token
    /// cancel + hard-abort backstop). Emits a `DelegateProgress { Cancelled }`
    /// so the frontend flips the card immediately; does NOT inject a result
    /// into the parent (the user asked to stop it). `ok:false` (with `error`)
    /// when the task id isn't found or no registry is wired.
    async fn cancel_background_task(&self, task_id: &str) -> BackgroundTaskOpResult;
    /// Cancel all in-flight background sub-agents (e.g. a "stop all" button).
    async fn cancel_all_background(&self) -> BackgroundTaskOpResult;
    /// Load the persisted bus-event log of one session (issue #40 trajectory
    /// replay). `ok:false` when no `SessionEventStore` is wired; empty
    /// `events` with `ok:true` when the session simply has no log yet.
    async fn session_trajectory(&self, session_id: &str) -> SessionTrajectoryResult;
}

/// Shared, thread-safe handle threaded through `serve_all` → transports →
/// `serve_connection` → `handle_request` for the `config/*` / `provider/*` /
/// `domainpack/*` / `skill/*` methods. `Arc<dyn AppProbe + Send + Sync>`.
pub type SharedAppProbe = Arc<dyn AppProbe + Send + Sync>;

/// A no-op [`AppProbe`] for tests / a launch without a backing `App` — every
/// method returns empty/default. Lets the adapter's existing tests keep working
/// by passing this in where the production path passes a real probe.
#[derive(Default)]
pub struct NullAppProbe;

#[async_trait]
impl AppProbe for NullAppProbe {
    async fn config(&self) -> AppConfigSnapshot {
        AppConfigSnapshot::default()
    }
    async fn providers(&self) -> Vec<ProviderInfo> {
        Vec::new()
    }
    async fn domainpacks(&self) -> DomainPackList {
        DomainPackList::default()
    }
    async fn skills(&self) -> Vec<SkillInfo> {
        Vec::new()
    }
    async fn skill_pin(&self, _name: &str) -> SkillOpResult {
        not_supported("skill pin")
    }
    async fn skill_unpin(&self, _name: &str) -> SkillOpResult {
        not_supported("skill unpin")
    }
    async fn skill_archive(&self, _name: &str) -> SkillOpResult {
        not_supported("skill archive")
    }
    async fn skill_restore(&self, _name: &str) -> SkillOpResult {
        not_supported("skill restore")
    }
    async fn provider_add(&self, _entry: ProviderEntryDto) -> ProviderOpResult {
        ProviderOpResult {
            ok: false,
            providers: None,
            error: Some("provider add not supported by this probe".to_string()),
        }
    }
    async fn provider_delete(&self, _name: &str) -> ProviderOpResult {
        ProviderOpResult {
            ok: false,
            providers: None,
            error: Some("provider delete not supported by this probe".to_string()),
        }
    }
    async fn provider_set_active(&self, _name: &str) -> ProviderOpResult {
        ProviderOpResult {
            ok: false,
            providers: None,
            error: Some("provider set-active not supported by this probe".to_string()),
        }
    }
    async fn provider_models(&self, _query: ProviderModelsQuery) -> ProviderModelsResult {
        ProviderModelsResult {
            ok: false,
            models: Vec::new(),
            error: Some("provider models not supported by this probe".to_string()),
        }
    }
    async fn provider_detect(&self, _query: ProviderDetectQuery) -> ProviderDetectResult {
        ProviderDetectResult::default()
    }
    async fn provider_update(&self, _entry: ProviderEntryDto) -> ProviderOpResult {
        ProviderOpResult {
            ok: false,
            providers: None,
            error: Some("provider update not supported by this probe".to_string()),
        }
    }
    async fn provider_set_model(&self, _name: &str, _model: &str) -> ProviderOpResult {
        ProviderOpResult {
            ok: false,
            providers: None,
            error: Some("provider set-model not supported by this probe".to_string()),
        }
    }
    async fn config_read(&self) -> ConfigFileView {
        ConfigFileView {
            path: String::new(),
            content: String::new(),
        }
    }
    async fn thinking_effort(&self) -> oneai_core::ThinkingEffort {
        oneai_core::ThinkingEffort::default()
    }
    async fn set_thinking_effort(&self, _effort: oneai_core::ThinkingEffort) {}
    async fn list_background_tasks(&self) -> Vec<BackgroundTaskInfoDto> {
        Vec::new()
    }
    async fn cancel_background_task(&self, _task_id: &str) -> BackgroundTaskOpResult {
        BackgroundTaskOpResult {
            ok: false,
            cancelled_count: None,
            error: Some("background cancel not supported by this probe".to_string()),
        }
    }
    async fn cancel_all_background(&self) -> BackgroundTaskOpResult {
        BackgroundTaskOpResult {
            ok: false,
            cancelled_count: Some(0),
            error: Some("background cancel not supported by this probe".to_string()),
        }
    }
    async fn session_trajectory(&self, _session_id: &str) -> SessionTrajectoryResult {
        SessionTrajectoryResult {
            ok: false,
            events: Vec::new(),
            error: Some("session trajectory not supported by this probe".to_string()),
        }
    }
}

fn not_supported(what: &str) -> SkillOpResult {
    SkillOpResult {
        ok: false,
        skill: None,
        error: Some(format!("{what} not supported by this probe")),
    }
}
