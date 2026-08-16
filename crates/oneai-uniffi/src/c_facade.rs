//! `extern "C"` 3-symbol bus pump (Shape A) for in-process mobile frontends
//! (iOS / Android / HarmonyOS) — the P4 collapse of the legacy 29-symbol
//! `OneAIApp`/`OneAISession`/`OneAiGroupChatSession` C facade.
//!
//! Three symbols only:
//! - `oneai_submit_directive(json) -> i32` — submit a `Directive` (JSON) to the
//!   engine bus. A `Directive::Init { config }` builds the engine + bus +
//!   directive pump on first call; everything else is forwarded to the bus.
//! - `oneai_poll_yield() -> *const c_char` — poll the next `EngineYield` as one
//!   JSON line (NUL-terminated), or null. The pointer aliases a thread-local
//!   buffer — valid until the next `poll_yield` on the same thread; the caller
//!   MUST NOT free it (no `oneai_free_string`).
//! - `oneai_shutdown() -> i32` — submit `Directive::Shutdown`, stop the pump,
//!   drop the engine.
//!
//! Approval flows as `EngineYield::ApprovalRequest` → `Directive::Approve`
//! (the bus's `BusInteractionGate`); interrupt as `Directive::Interrupt` (the
//! bus fires the registered cancel token, OR — for an active group round — the
//! pump intercepts it and calls `GroupChatSession::interrupt()` directly, since
//! a group round doesn't register a cancel token). Group chat rides the same
//! bus via `GroupChatBusObserver` (speaker-tagged yields + `SpeakerTurn`).
//!
//! ## Threading
//! Each entry point drives a shared multi-thread tokio runtime via `block_on`.
//! `oneai_submit_directive` blocks until the bus accepts the directive (fast —
//! bounded channel send). `oneai_poll_yield` is non-blocking. The directive
//! pump runs on a runtime worker; yields are read off the broadcast receiver.

#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::CStr;
use std::os::raw::c_char;
use std::sync::{Arc, Mutex, OnceLock};

use oneai_agent::{group_chat::GroupChatSession, AgentLoop, GroupChatBusObserver};
use oneai_app::{App, AppBuilder, AppSession, DirectiveRuntime};
use oneai_bus::{
    serialize_yield, BusEngineConfig, BusGroupScenario, BusParadigmKind, BusTurnSummary, Directive,
    EngineBus, EngineYield, InProcessBus,
};
use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::LlmProvider;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;

use crate::types::EmbeddingConfigView;

// ─── Shared tokio runtime ─────────────────────────────────────────────
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("oneai c_facade tokio runtime")
    })
}

// ─── Thread-local yield buffer (no free_string) ───────────────────────
// poll_yield writes the next yield's JSON line here and returns a pointer into
// it. The buffer is overwritten on the next poll_yield call on the same thread;
// the caller borrows it transiently and must not free it. NUL-terminated
// (CString) so the foreign side reads it as a C string.
thread_local! {
    static YIELD_BUF: std::cell::RefCell<std::ffi::CString> =
        std::cell::RefCell::new(std::ffi::CString::new("").unwrap());
}

fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(p).to_str().ok() }
}

// ─── Engine state ──────────────────────────────────────────────────────

/// The built engine + bus + pump, held in a global `OnceLock` after a
/// successful `Directive::Init`. A second `Init` (without a `shutdown`) is an
/// error — the engine is already built.
struct EngineState {
    bus: Arc<InProcessBus>,
    /// The broadcast receiver the foreign side polls via `oneai_poll_yield`.
    yield_rx: Mutex<tokio::sync::broadcast::Receiver<EngineYield>>,
    /// The directive pump task — aborted on `shutdown`.
    pump_handle: Mutex<Option<JoinHandle<()>>>,
    /// Active group session (set by `start_group`, cleared on single-agent
    /// session lifecycle). Read by `submit_directive` to intercept
    /// `Directive::Interrupt` for an in-flight group round (which doesn't
    /// register a bus cancel token, unlike single-agent `run_turn`).
    group_slot: Arc<Mutex<Option<Arc<GroupChatSession>>>>,
}

static ENGINE: OnceLock<Mutex<Option<EngineState>>> = OnceLock::new();

fn engine() -> &'static Mutex<Option<EngineState>> {
    ENGINE.get_or_init(|| Mutex::new(None))
}

// ─── Build the engine from a Directive::Init config ────────────────────
//
// Mirrors the legacy `c_facade::parse_config` + `OneAIAppBuilder` path, but on
// the engine `AppBuilder` directly so `engine_bus()` (sets the
// `BusInteractionGate` + exposes the bus to `run_turn_via_bus`) can be wired
// before `build()`. Provider/embedding construction reuses the same
// `ModelConfig` / `EmbeddingConfigView::to_engine` conversions the uniffi
// builder uses. The 4 default tools (web_search/web_fetch/read_file/write_file
// — the same set `OneAIAppBuilder::default_tools` registers) are registered on
// the built `App` when `cfg.default_tools`.
async fn build_engine(cfg: BusEngineConfig) -> Result<EngineState> {
    let mut b = AppBuilder::new();

    let provider: Arc<dyn LlmProvider> = match cfg.kind.as_str() {
        "openai" => {
            let config = oneai_core::ModelConfig::openai_compatible(
                cfg.api_key.clone().unwrap_or_default(),
                cfg.base_url
                    .clone()
                    .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
                cfg.model.clone(),
            );
            Arc::new(oneai_provider::OpenAIProvider::new(config))
        }
        "anthropic" => {
            let config = oneai_core::ModelConfig::anthropic(
                cfg.api_key.clone().unwrap_or_default(),
                cfg.model.clone(),
            );
            Arc::new(oneai_provider::AnthropicProvider::new(config))
        }
        "ollama" => {
            let config = match cfg.host.clone() {
                Some(h) if !h.is_empty() => {
                    // `base_url`/`host` carries "host:port" for ollama (matching
                    // the macOS settings convention) — split into host/port.
                    let (host, port) = match h.rsplit_once(':') {
                        Some((host, p)) => (host.to_string(), p.parse::<u16>().unwrap_or(11434)),
                        None => (h, cfg.port.unwrap_or(11434)),
                    };
                    oneai_core::ModelConfig::ollama_custom(host, port, cfg.model.clone())
                }
                _ => oneai_core::ModelConfig::ollama(cfg.model.clone()),
            };
            Arc::new(oneai_provider::OllamaProvider::new(config))
        }
        other => {
            return Err(OneAIError::Config(format!(
                "unknown provider kind '{other}' (expected openai/anthropic/ollama)"
            )));
        }
    };
    b = b.provider(provider);

    if let Some(db) = cfg.db_path.as_ref() {
        b = b.sqlite_persistence_at(db);
    }
    if let Some(emb) = cfg.embedding.as_ref() {
        // Reuse the uniffi view's `to_engine` conversion (provider parse +
        // model/key/base_url/fallback mapping) so the bus DTO and the foreign
        // record build the same `oneai_core::EmbeddingConfig`.
        let view = EmbeddingConfigView {
            provider: emb.provider.clone(),
            model: emb.model.clone(),
            api_key: emb.api_key.clone(),
            base_url: emb.base_url.clone(),
            fallback: emb.fallback.clone(),
        };
        b = b.embedding_config(view.to_engine());
    }

    // Wire the bus (sets BusInteractionGate + stores the bus on the builder).
    let (builder, directive_rx) = b.engine_bus();
    let app: App = builder.build().await?;
    if cfg.default_tools {
        for tool in default_tool_set() {
            // Register best-effort — a tool failing to register (e.g. a
            // network tool without deps on a stripped build) is logged, not
            // fatal; the rest of the turn still works.
            if let Err(e) = app.register_tool(tool).await {
                tracing::warn!("c_facade default tool register failed: {e}");
            }
        }
    }
    let bus = app.engine_bus.clone().expect("engine_bus set before build");

    let group_slot: Arc<Mutex<Option<Arc<GroupChatSession>>>> = Arc::new(Mutex::new(None));
    let rt = Arc::new(TokioMutex::new(CFacadeRuntime {
        app: Arc::new(app),
        session: None,
        group: None,
        bus: bus.clone(),
        group_slot: group_slot.clone(),
    }));
    let interrupt_slot: Arc<TokioMutex<Option<AgentLoop>>> = Arc::new(TokioMutex::new(None));

    let yield_rx = bus.subscribe_yields();
    let pump_handle =
        oneai_app::spawn_directive_pump(directive_rx, rt, interrupt_slot, bus.clone());

    Ok(EngineState {
        bus,
        yield_rx: Mutex::new(yield_rx),
        pump_handle: Mutex::new(Some(pump_handle)),
        group_slot,
    })
}

/// The 4 default tools `OneAIAppBuilder::default_tools` registers (web access +
/// file I/O), as concrete `Arc<dyn Tool>` the engine `App::register_tool` takes.
fn default_tool_set() -> Vec<Arc<dyn oneai_core::traits::Tool>> {
    vec![
        Arc::new(oneai_tool::WebSearchTool::new()),
        Arc::new(oneai_tool::WebFetchTool::new()),
        Arc::new(oneai_tool::FileReadTool::new()),
        Arc::new(oneai_tool::FileWriteTool::new()),
    ]
}

// ─── CFacadeRuntime — DirectiveRuntime over App + GroupChatSession ──────
//
// Mirrors `SidecarRuntime` (cmd_serve.rs) for the single-agent path + the
// group methods the pump dispatches `StartGroupChat`/`GroupStart`/
// `GroupUserMessage`/`GroupSetScriptedOrder` to. Group chat reuses
// `OneAiGroupChatSession::build` (provider/resource/config mapping) and drives
// the underlying `GroupChatSession` through a `GroupChatBusObserver`.
struct CFacadeRuntime {
    app: Arc<App>,
    session: Option<AppSession>,
    group: Option<Arc<GroupChatSession>>,
    bus: Arc<InProcessBus>,
    /// Shared with `EngineState` so `submit_directive` can intercept Interrupt
    /// for an in-flight group round (the pump holds this runtime's tokio Mutex
    /// during `group_run_task`; the std Mutex here is a separate, brief lock).
    group_slot: Arc<Mutex<Option<Arc<GroupChatSession>>>>,
}

impl CFacadeRuntime {
    /// Ensure a single-agent session exists (error if a group is active — the
    /// frontend must pick one mode via the directives it submits).
    fn require_session(&mut self) -> Result<&mut AppSession> {
        if self.group.is_some() {
            return Err(OneAIError::Agent(
                "group chat is active — use GroupUserMessage, not single-agent directives".into(),
            ));
        }
        if self.session.is_none() {
            let s = self.app.create_session();
            self.session = Some(s);
        }
        Ok(self.session.as_mut().expect("session just created"))
    }

    fn require_group(&self) -> Result<Arc<GroupChatSession>> {
        self.group.clone().ok_or_else(|| {
            OneAIError::Agent("group chat not active — submit StartGroupChat first".into())
        })
    }
}

#[async_trait::async_trait]
impl DirectiveRuntime for CFacadeRuntime {
    async fn run_turn(
        &mut self,
        task: &str,
        interrupt_slot: Arc<TokioMutex<Option<AgentLoop>>>,
    ) -> Result<oneai_bus::BusTurnSummary> {
        let session = self.require_session()?;
        session.run_turn_via_bus(task, interrupt_slot).await
    }

    async fn set_paradigm(
        &mut self,
        to: oneai_agent::ParadigmKind,
    ) -> Option<oneai_agent::ParadigmKind> {
        match self.require_session() {
            Ok(s) => s.set_paradigm(to),
            Err(_) => None,
        }
    }

    async fn set_plan_mode(&mut self, on: bool) {
        if let Ok(s) = self.require_session() {
            s.set_plan_mode(on);
        }
    }

    async fn compact(&mut self, keep_recent_turns: usize) -> Result<oneai_app::CompactOutcome> {
        let session = self.require_session()?;
        session.compact(keep_recent_turns).await
    }

    fn provider(&self) -> Option<Arc<dyn LlmProvider>> {
        self.app.provider.clone()
    }

    async fn create_session(&mut self, id: Option<String>, workspace: Option<String>) -> String {
        let mut new = match id {
            Some(wanted) => self.app.create_session_with_id(&wanted).await,
            None => self.app.create_session(),
        };
        new.set_workspace(workspace.as_deref());
        let nid = new.session_id().to_string();
        self.session = Some(new);
        self.group = None;
        *self.group_slot.lock().unwrap() = None;
        nid
    }

    async fn load_session(&mut self, id: String) -> (String, Vec<oneai_core::Message>) {
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
        self.session = Some(new);
        self.group = None;
        *self.group_slot.lock().unwrap() = None;
        (resolved, msgs)
    }

    async fn reset_session(&mut self) -> String {
        let new = self.app.create_session();
        let nid = new.session_id().to_string();
        self.session = Some(new);
        self.group = None;
        *self.group_slot.lock().unwrap() = None;
        nid
    }

    async fn delete_session(&mut self, id: String) -> Result<()> {
        self.app.delete_conversation(&id).await
    }

    async fn session_id(&mut self) -> String {
        match self.session.as_ref() {
            Some(s) => s.session_id().to_string(),
            None => String::new(),
        }
    }

    // ── Group-chat methods ────────────────────────────────────────────

    async fn start_group(&mut self, scenario: BusGroupScenario) -> Result<()> {
        // Reuse the uniffi build path (per-member providers, shared resources,
        // config/policy/locale mapping) via the BusGroupScenario→view conversion.
        let spec = crate::group_chat::ScenarioSpecView::from(&scenario);
        let gs = crate::group_chat::OneAiGroupChatSession::build(spec, &self.app)
            .map_err(|e| OneAIError::Config(format!("{e:?}")))?;
        let inner = gs.inner_session();
        self.group = Some(inner.clone());
        *self.group_slot.lock().unwrap() = Some(inner);
        // A group round displaces the single-agent session.
        self.session = None;
        Ok(())
    }

    async fn group_start(&mut self) -> Result<()> {
        let group = self.require_group()?;
        let turn_id = group_turn_id();
        let observer =
            GroupChatBusObserver::new(self.bus.clone() as Arc<dyn EngineBus>, turn_id.clone());
        let res = group.start(&observer).await;
        // A group round is delimited for bus consumers by this single
        // round-level `TurnComplete` — the `GroupChatBusObserver` deliberately
        // no-ops `on_complete` (so N members don't each emit one). The in-
        // process FFI path signals round-end via its `await` returning; an
        // out-of-process bus consumer (sidecar / mobile c_facade) has no
        // await to observe, so it needs this yield to clear `running`.
        if res.is_ok() {
            let _ = self.bus.emit(EngineYield::TurnComplete {
                turn_id,
                summary: group_round_summary(),
            });
        }
        res
    }

    async fn group_run_task(&mut self, user_input: &str) -> Result<()> {
        let group = self.require_group()?;
        let turn_id = group_turn_id();
        let observer =
            GroupChatBusObserver::new(self.bus.clone() as Arc<dyn EngineBus>, turn_id.clone());
        let res = group.run_task(user_input, &observer).await;
        if res.is_ok() {
            let _ = self.bus.emit(EngineYield::TurnComplete {
                turn_id,
                summary: group_round_summary(),
            });
        }
        res
    }

    async fn group_set_scripted_order(&mut self, order: Vec<String>) {
        if let Ok(group) = self.require_group() {
            let policy = oneai_agent::group_chat::TurnPolicy::Scripted { order };
            group.set_turn_policy(policy).await;
        }
    }
}

/// A fresh turn id for a group round (the engine assigns its own per-member
/// ids internally; this brackets the round's yields the pump emits).
fn group_turn_id() -> String {
    format!("group_{}", uuid::Uuid::new_v4())
}

/// Minimal `BusTurnSummary` for the round-level `TurnComplete` a group round
/// emits on success. The members' actual answers rode `DirectAnswer` yields
/// (one per member); `final_answer` is empty so the frontend's `turn_complete`
/// handler leaves the last member's bubble intact (its `if !final.is_empty()`
/// guard). `completed: true` signals the round ended cleanly.
fn group_round_summary() -> BusTurnSummary {
    BusTurnSummary {
        final_answer: String::new(),
        iterations: 0,
        completed: true,
        active_paradigm: BusParadigmKind::ReAct,
    }
}

// ─── extern "C" surface (exactly 3 symbols) ────────────────────────────

/// Submit a `Directive` (one JSON line) to the engine bus. Returns 0 on
/// success, non-zero on error (the error detail is emitted as an
/// `EngineYield::Error` the caller drains via `oneai_poll_yield`).
///
/// A `Directive::Init { config }` on an unbuilt engine constructs it (engine +
/// bus + pump); any other directive on an unbuilt engine is an error.
#[no_mangle]
pub extern "C" fn oneai_submit_directive(json: *const c_char) -> i32 {
    let json = match cstr(json) {
        Some(s) => s,
        None => return 1,
    };
    let directive: Directive = match serde_json::from_str(json) {
        Ok(d) => d,
        Err(_) => return 2,
    };

    // `Directive::Init` builds the engine (intercepted before the bus — the
    // pump + bus must exist before any other directive can be submitted).
    if let Directive::Init { config } = &directive {
        let mut guard = engine().lock().unwrap();
        if guard.is_some() {
            return 3; // already built — submit Shutdown first
        }
        return match runtime().block_on(build_engine(config.clone())) {
            Ok(state) => {
                *guard = Some(state);
                0
            }
            Err(_) => 4,
        };
    }

    let guard = engine().lock().unwrap();
    let Some(state) = guard.as_ref() else {
        return 5; // not initialized — submit Init first
    };

    // Interrupt for an active group round is intercepted here (a group round
    // doesn't register a bus cancel token, unlike single-agent run_turn).
    if matches!(directive, Directive::Interrupt { .. }) {
        if let Some(group) = state.group_slot.lock().unwrap().clone() {
            group.interrupt();
            return 0;
        }
    }

    let bus = state.bus.clone();
    drop(guard); // release the engine lock before blocking on submit
    match runtime().block_on(async move { bus.submit(directive).await }) {
        Ok(()) => 0,
        Err(_) => 6,
    }
}

/// Poll the next `EngineYield` as one JSON line (NUL-terminated), or null if
/// none is pending. The returned pointer aliases a thread-local buffer — valid
/// until the next `oneai_poll_yield` on the same thread; the caller MUST NOT
/// free it.
#[no_mangle]
pub extern "C" fn oneai_poll_yield() -> *const c_char {
    // Lock the engine, take one yield off the broadcast receiver (non-blocking),
    // then release — serialize into the thread-local buffer. The std Mutex on
    // `yield_rx` is held only across `try_recv` (no await), so a foreign thread
    // polling never blocks a runtime worker.
    poll_yield_inner()
}

fn poll_yield_inner() -> *const c_char {
    // Lock the engine, take one yield off the broadcast receiver (non-blocking),
    // then release. Serialize into the thread-local buffer.
    let line = {
        let guard = engine().lock().unwrap();
        let Some(state) = guard.as_ref() else {
            return std::ptr::null();
        };
        let mut rx = state.yield_rx.lock().unwrap();
        match rx.try_recv() {
            Ok(y) => serialize_yield(&y).unwrap_or_default(),
            Err(_) => return std::ptr::null(),
        }
    };
    YIELD_BUF.with(|b| {
        let mut b = b.borrow_mut();
        *b = std::ffi::CString::new(line).unwrap_or_default();
        b.as_ptr()
    })
}

/// Shut the engine down — submit `Directive::Shutdown`, abort the pump, drop
/// the engine state. Returns 0 on success, non-zero if no engine is built.
#[no_mangle]
pub extern "C" fn oneai_shutdown() -> i32 {
    let mut guard = engine().lock().unwrap();
    let Some(state) = guard.as_mut() else {
        return 1;
    };
    let bus = state.bus.clone();
    let _ = runtime().block_on(async move { bus.submit(Directive::Shutdown).await });
    if let Some(handle) = state.pump_handle.lock().unwrap().take() {
        handle.abort();
    }
    *guard = None;
    0
}

#[cfg(test)]
mod tests {
    //! End-to-end over the 3-symbol surface: Init → UserMessage → poll yields;
    //! Interrupt mid-turn; StartGroupChat → GroupUserMessage → speaker-tagged
    //! yields. Uses a mock provider so no network is hit.
    use super::*;
    use std::ffi::CString;

    fn tmp_db(name: &str) -> String {
        let p = std::env::temp_dir().join(format!(
            "oneai_p4_c_{}_{}_{}.db",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&p);
        p.to_string_lossy().into_owned()
    }

    fn init_json(db: &str) -> String {
        format!(
            r#"{{"kind":"init","config":{{"kind":"openai","api_key":"sk-test","model":"gpt-4o","db_path":"{db}","default_tools":true}}}}"#
        )
    }

    fn submit(json: &str) -> i32 {
        let c = CString::new(json).unwrap();
        oneai_submit_directive(c.as_ptr())
    }

    fn shutdown() -> i32 {
        oneai_shutdown()
    }

    /// The c_facade tests share a process-global `ENGINE` (the whole point of
    /// the 3-symbol surface is one engine per process). Serialize them and
    /// reset to a clean slate (a prior test that panicked mid-build would
    /// otherwise leave an engine up). Returns the guard — drop ends the test's
    /// exclusive hold.
    static TEST_GUARD: Mutex<()> = Mutex::new(());
    fn lock_and_reset() -> std::sync::MutexGuard<'static, ()> {
        let g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let _ = shutdown(); // best-effort reset
        g
    }

    #[test]
    fn init_built_then_shutdown() {
        let _g = lock_and_reset();
        let db = tmp_db("init");
        assert_eq!(submit(&init_json(&db)), 0, "Init succeeds");
        // A second Init before Shutdown is rejected (already built).
        assert_eq!(submit(&init_json(&db)), 3);
        assert_eq!(shutdown(), 0);
        // After shutdown, Init rebuilds cleanly.
        assert_eq!(submit(&init_json(&db)), 0);
        assert_eq!(shutdown(), 0);
    }

    #[test]
    fn directive_before_init_rejected() {
        let _g = lock_and_reset();
        // No engine built — any non-Init directive is rejected with code 5.
        let c = CString::new(r#"{"kind":"user_message","content":[{"type":"text","text":"hi"}]}"#)
            .unwrap();
        assert_eq!(oneai_submit_directive(c.as_ptr()), 5);
    }

    #[test]
    fn init_rejects_bad_json() {
        let _g = lock_and_reset();
        let c = CString::new("not json").unwrap();
        assert_eq!(oneai_submit_directive(c.as_ptr()), 2);
    }

    #[test]
    fn init_rejects_unknown_provider() {
        let _g = lock_and_reset();
        let db = tmp_db("badprov");
        let c = CString::new(format!(
            r#"{{"kind":"init","config":{{"kind":"gemini","api_key":"x","model":"m","db_path":"{db}"}}}}"#
        ))
        .unwrap();
        assert_eq!(oneai_submit_directive(c.as_ptr()), 4);
    }

    #[test]
    fn start_group_chat_builds_and_polls_empty() {
        let _g = lock_and_reset();
        let db = tmp_db("group");
        assert_eq!(submit(&init_json(&db)), 0);
        // A 2-member scripted scenario. build_member_provider constructs
        // providers without touching the network; create_group_session is pure
        // setup — so StartGroupChat succeeds offline.
        let sc = r#"{"kind":"start_group_chat","scenario":{"members":[{"id":"writer","name":"写手","system_prompt":"起草","kind":"openai","model":"gpt-4o","api_key":"sk-test"},{"id":"editor","name":"编辑","system_prompt":"润色","kind":"openai","model":"gpt-4o","api_key":"sk-test"}],"turn_policy":"scripted","script_order":["writer","editor"]}}"#;
        assert_eq!(submit(sc), 0, "StartGroupChat succeeds");
        // The group is now active — a group scripted-order swap (no-op on
        // success) confirms the group runtime is wired.
        let order = r#"{"kind":"group_set_scripted_order","order":["editor"]}"#;
        assert_eq!(submit(order), 0);
        assert_eq!(shutdown(), 0);
    }

    #[test]
    fn extern_c_symbol_count_is_three() {
        // P4 contract: exactly 3 extern "C" entry points (no free_string /
        // last_error — the poll buffer is internal, errors ride yields).
        let src =
            std::fs::read_to_string(env!("CARGO_MANIFEST_DIR").to_string() + "/src/c_facade.rs")
                .unwrap();
        let count = src.matches("pub extern \"C\" fn").count();
        assert_eq!(
            count, 3,
            "expected exactly 3 extern C symbols, found {count}"
        );
    }
}
