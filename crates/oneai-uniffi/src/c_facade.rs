//! `extern "C"` JSON facade — a C ABI over the uniffi `OneAIApp` /
//! `OneAISession` wrappers, for foreign runtimes UniFFI 0.32 can't generate
//! bindings for (C# / NAPI-C++). Every call is JSON-in / JSON-out + a single
//! C event callback, so neither side has to know the uniffi wire protocol.
//!
//! The same cdylib (`liboneai.*`) exports both the uniffi symbols (Swift/
//! Kotlin) and these `#[no_mangle] extern "C"` symbols. Windows P/Invokes
//! them from C#; HarmonyOS NAPI wraps them in C++.
//!
//! ## Threading model
//! Each entry point drives a shared multi-thread tokio runtime via
//! `block_on`. `oneai_session_run_task` blocks the caller for the whole agent
//! loop (like Android's `runTask`) and fires the C callback on a *worker*
//! thread — the foreign side must marshal to its UI thread. `interrupt()` is
//! safe to call from a different thread (the loop's interrupt slot is set
//! briefly at start / cleared at end, so a concurrent `block_on` lock wins).

// FFI entry points take raw pointer handles by design — the whole module is
// an unsafe-contract boundary (null/stale pointers are UB, same as any C API).
#![allow(clippy::not_unsafe_ptr_arg_deref)]


use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};
use std::sync::OnceLock;

use crate::app::{OneAIApp, OneAISession};
use crate::app_builder::OneAIAppBuilder;
use crate::callback::ChatEventCallback;
use crate::group_chat::{OneAiGroupChatSession, ScenarioSpecView};
use crate::types::{
    ChatEventView, MessageView, OneAIErrorView, ProviderConfigView, SessionInfoView,
};

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

// ─── Handle layout: heap-allocated Arc behind a raw pointer ───────────
// Handle = `*mut Arc<OneAIApp>` (Box<Arc>). Borrowing is a plain cast to a
// `&Arc`; freeing is `Box::from_raw`. No refcount juggling.
type AppHandle = *mut std::sync::Arc<OneAIApp>;
type SessionHandle = *mut std::sync::Arc<OneAISession>;

unsafe fn borrow_app(h: AppHandle) -> &'static std::sync::Arc<OneAIApp> {
    &*(h as *const std::sync::Arc<OneAIApp>)
}
unsafe fn borrow_session(h: SessionHandle) -> &'static std::sync::Arc<OneAISession> {
    &*(h as *const std::sync::Arc<OneAISession>)
}

// ─── Group-chat handle: heap-allocated Arc<OneAiGroupChatSession> ─────────
type GroupSessionHandle = *mut std::sync::Arc<OneAiGroupChatSession>;

unsafe fn borrow_group(h: GroupSessionHandle) -> &'static std::sync::Arc<OneAiGroupChatSession> {
    &*(h as *const std::sync::Arc<OneAiGroupChatSession>)
}

// ─── Scenario JSON parser (no serde dep; reads serde_json::Value) ────────
// Shape: {"members":[{"id","name","system_prompt","kind","model",
//   "api_key"?,"base_url"?,"color"?,"avatar"?}],
//   "turn_policy":"scripted"|"roundrobin"|"moderator",
//   "script_order"?:[..], "moderator_id"?, "opener_agent_id"?,
//   "opener_line"?, "title"?, "review_loop"?:{"reviewer_id","approve_marker","max_rounds"}}
// Mirrors the uniffi `ScenarioSpecView`/`AgentSpecView` records the Swift/macOS
// binding builds on the foreign side — so the C# Windows port constructs the
// same JSON the macOS app constructs as a typed Record.
fn parse_scenario(json: &str) -> Option<ScenarioSpecView> {
    use crate::group_chat::{AgentSpecView, ReviewLoopSpecView};
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let members = v.get("members")?.as_array()?;
    let mut agents = Vec::with_capacity(members.len());
    for m in members {
        let s = |k: &str| m.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let opt = |k: &str| m.get(k).and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(|s| s.to_string());
        agents.push(AgentSpecView {
            id: s("id"),
            name: s("name"),
            system_prompt: s("system_prompt"),
            kind: { let k = s("kind"); if k.is_empty() { "openai".to_string() } else { k } },
            model: s("model"),
            api_key: opt("api_key"),
            base_url: opt("base_url"),
            color: opt("color"),
            avatar: opt("avatar"),
        });
    }
    let opt_str = |k: &str| v.get(k).and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(|s| s.to_string());
    let opt_strs = |k: &str| {
        v.get(k).and_then(|x| x.as_array()).map(|a| {
            a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect::<Vec<_>>()
        })
    };
    let review_loop = v.get("review_loop").map(|r| {
        let rs = |k: &str| r.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let max = r.get("max_rounds").and_then(|x| x.as_u64()).unwrap_or(1);
        ReviewLoopSpecView { reviewer_id: rs("reviewer_id"), approve_marker: rs("approve_marker"), max_rounds: max }
    });
    Some(ScenarioSpecView {
        members: agents,
        turn_policy: opt_str("turn_policy").unwrap_or_else(|| "scripted".to_string()),
        script_order: opt_strs("script_order"),
        moderator_id: opt_str("moderator_id"),
        opener_agent_id: opt_str("opener_agent_id"),
        opener_line: opt_str("opener_line"),
        title: opt_str("title"),
        review_loop,
    })
}

// ─── JSON helpers (views don't derive serde) ───────────────────────────
fn j_escape(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn message_to_json(m: &MessageView) -> String {
    let mut s = String::from("{\"role\":");
    j_escape(&m.role, &mut s);
    s.push_str(",\"text\":");
    j_escape(&m.text, &mut s);
    s.push_str(",\"speaker\":");
    match &m.speaker {
        Some(sp) => j_escape(sp, &mut s),
        None => s.push_str("null"),
    }
    s.push('}');
    s
}

fn session_to_json(s: &SessionInfoView) -> String {
    let mut o = String::from("{\"id\":");
    j_escape(&s.id, &mut o);
    o.push_str(",\"title\":");
    match &s.title {
        Some(t) if !t.is_empty() => j_escape(t, &mut o),
        _ => o.push_str("null"),
    }
    o.push_str(",\"message_count\":");
    o.push_str(&s.message_count.to_string());
    o.push_str(",\"updated_at_ms\":");
    o.push_str(&s.updated_at_ms.to_string());
    o.push('}');
    o
}

fn event_to_json(e: &ChatEventView) -> String {
    let mut o = String::new();
    match e {
        ChatEventView::StreamChunk { text, speaker } => {
            o.push_str("{\"type\":\"StreamChunk\",\"text\":"); j_escape(text, &mut o);
            push_speaker(&mut o, speaker); o.push('}');
        }
        ChatEventView::Thinking { text, speaker } => {
            o.push_str("{\"type\":\"Thinking\",\"text\":"); j_escape(text, &mut o);
            push_speaker(&mut o, speaker); o.push('}');
        }
        ChatEventView::ToolCall { id, name, args_json, speaker } => {
            o.push_str("{\"type\":\"ToolCall\",\"id\":"); j_escape(id, &mut o);
            o.push_str(",\"name\":"); j_escape(name, &mut o);
            o.push_str(",\"args_json\":"); j_escape(args_json, &mut o);
            push_speaker(&mut o, speaker); o.push('}');
        }
        ChatEventView::ToolResult { call_id, tool_name, content, success, speaker } => {
            o.push_str("{\"type\":\"ToolResult\",\"call_id\":"); j_escape(call_id, &mut o);
            o.push_str(",\"tool_name\":"); j_escape(tool_name, &mut o);
            o.push_str(",\"content\":"); j_escape(content, &mut o);
            o.push_str(",\"success\":"); o.push_str(if *success { "true" } else { "false" });
            push_speaker(&mut o, speaker); o.push('}');
        }
        ChatEventView::DirectAnswer { text, speaker } => {
            o.push_str("{\"type\":\"DirectAnswer\",\"text\":"); j_escape(text, &mut o);
            push_speaker(&mut o, speaker); o.push('}');
        }
        ChatEventView::Complete { final_text, speaker } => {
            o.push_str("{\"type\":\"Complete\",\"final_text\":"); j_escape(final_text, &mut o);
            push_speaker(&mut o, speaker); o.push('}');
        }
        ChatEventView::Error { message, speaker } => {
            o.push_str("{\"type\":\"Error\",\"message\":"); j_escape(message, &mut o);
            push_speaker(&mut o, speaker); o.push('}');
        }
    }
    o
}

/// Append `,"speaker":<id|null>` for a group-chat event; single-agent
/// events pass `None` → emitted as JSON `null` (the foreign side treats
/// null as the single assistant).
fn push_speaker(o: &mut String, speaker: &Option<String>) {
    o.push_str(",\"speaker\":");
    match speaker {
        Some(s) => j_escape(s, o),
        None => o.push_str("null"),
    }
}

// Box::into_raw a CString the caller frees with oneai_free_string.
fn return_string(s: String) -> *mut c_char {
    if s.is_empty() {
        return std::ptr::null_mut();
    }
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

fn err_msg(e: OneAIErrorView) -> String {
    match e {
        OneAIErrorView::Provider { message: m } | OneAIErrorView::Parser { message: m }
        | OneAIErrorView::Tool { message: m } | OneAIErrorView::Memory { message: m }
        | OneAIErrorView::Workflow { message: m } | OneAIErrorView::Agent { message: m }
        | OneAIErrorView::Skill { message: m } | OneAIErrorView::Scheduler { message: m }
        | OneAIErrorView::Persistence { message: m } | OneAIErrorView::Rag { message: m }
        | OneAIErrorView::Config { message: m } | OneAIErrorView::Serialization { message: m }
        | OneAIErrorView::Network { message: m } | OneAIErrorView::Timeout { message: m }
        | OneAIErrorView::Platform { message: m } | OneAIErrorView::Wasm { message: m }
        | OneAIErrorView::Other { message: m } => m,
    }
}

// ─── C-callback adapter (Send+Sync: raw ctx is a foreign handle) ──────
type EventCb = extern "C" fn(ctx: *mut c_void, event_json: *const c_char);

struct CCallback {
    cb: EventCb,
    ctx: *mut c_void,
}
unsafe impl Send for CCallback {}
unsafe impl Sync for CCallback {}

impl ChatEventCallback for CCallback {
    fn on_event(&self, event: ChatEventView) {
        let json = event_to_json(&event);
        if let Ok(c) = CString::new(json) {
            (self.cb)(self.ctx, c.as_ptr());
        }
    }
}

fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() { return None; }
    unsafe { CStr::from_ptr(p).to_str().ok() }
}

// ─── extern "C" surface ───────────────────────────────────────────────

/// Create an app from a JSON config:
/// `{"kind":"openai","api_key":"..","base_url":"..","model":"..","host":"..","port":11434,"db_path":"/path/oneai.db","default_tools":true}`
/// Returns an opaque app handle, or null on error (call `oneai_last_error`).
#[no_mangle]
pub extern "C" fn oneai_create_app(config_json: *const c_char) -> AppHandle {
    let cfg = match cstr(config_json) {
        Some(s) => parse_config(s),
        None => None,
    };
    let cfg = match cfg {
        Some(c) => c,
        None => { set_last_error("invalid config_json".into()); return std::ptr::null_mut(); }
    };
    let app = runtime().block_on(async move {
        let mut b = std::sync::Arc::new(OneAIAppBuilder::new());
        if cfg.default_tools { b = b.default_tools(); }
        if let Some(db) = cfg.db_path.as_ref() { b = b.sqlite_persistence_at(db.clone()); }
        match b.provider_config(cfg.provider) {
            Ok(b2) => match b2.build().await {
                Ok(a) => Some(a),
                Err(e) => { set_last_error(err_msg(e)); None }
            },
            Err(e) => { set_last_error(err_msg(e)); None }
        }
    });
    match app {
        Some(a) => Box::into_raw(Box::new(a)),
        None => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn oneai_free_app(h: AppHandle) {
    if !h.is_null() { unsafe { drop(Box::from_raw(h)); } }
}

#[no_mangle]
pub extern "C" fn oneai_has_provider(h: AppHandle) -> bool {
    if h.is_null() { return false; }
    unsafe { borrow_app(h).has_provider() }
}

/// Create a session. If `id` is null/empty, a new conversation is created;
/// otherwise the conversation with that id is resumed (history loaded).
#[no_mangle]
pub extern "C" fn oneai_create_session(h: AppHandle, id: *const c_char) -> SessionHandle {
    if h.is_null() { return std::ptr::null_mut(); }
    let app = unsafe { borrow_app(h) };
    let id = cstr(id).map(|s| s.to_string()).unwrap_or_default();
    let sess = if id.is_empty() {
        app.create_session()
    } else {
        let id2 = id.clone();
        runtime().block_on(async move { app.create_session_with_id(id2).await })
    };
    Box::into_raw(Box::new(sess))
}

#[no_mangle]
pub extern "C" fn oneai_free_session(h: SessionHandle) {
    if !h.is_null() { unsafe { drop(Box::from_raw(h)); } }
}

/// Returns the session id (caller frees with `oneai_free_string`).
#[no_mangle]
pub extern "C" fn oneai_session_id(h: SessionHandle) -> *mut c_char {
    if h.is_null() { return std::ptr::null_mut(); }
    let s = unsafe { borrow_session(h) };
    return_string(s.session_id())
}

/// List conversations as a JSON array (caller frees).
#[no_mangle]
pub extern "C" fn oneai_list_conversations(h: AppHandle) -> *mut c_char {
    if h.is_null() { return std::ptr::null_mut(); }
    let app = unsafe { borrow_app(h) };
    let list = runtime().block_on(async move { app.list_conversations().await });
    let mut out = String::from("[");
    for (i, s) in list.iter().enumerate() {
        if i > 0 { out.push(','); }
        out.push_str(&session_to_json(s));
    }
    out.push(']');
    return_string(out)
}

/// Delete a conversation by id (best-effort).
#[no_mangle]
pub extern "C" fn oneai_delete_conversation(h: AppHandle, id: *const c_char) {
    if h.is_null() { return; }
    let app = unsafe { borrow_app(h) };
    if let Some(id) = cstr(id).map(|s| s.to_string()) {
        let _ = runtime().block_on(async move { app.delete_conversation(id).await });
    }
}

/// Snapshot the conversation messages as a JSON array (caller frees).
#[no_mangle]
pub extern "C" fn oneai_session_messages(h: SessionHandle) -> *mut c_char {
    if h.is_null() { return std::ptr::null_mut(); }
    let s = unsafe { borrow_session(h) };
    let msgs = runtime().block_on(async move { s.messages().await });
    let mut out = String::from("[");
    for (i, m) in msgs.iter().enumerate() {
        if i > 0 { out.push(','); }
        out.push_str(&message_to_json(m));
    }
    out.push(']');
    return_string(out)
}

/// Persist the current conversation to SQLite immediately.
#[no_mangle]
pub extern "C" fn oneai_session_save(h: SessionHandle) -> bool {
    if h.is_null() { return false; }
    let s = unsafe { borrow_session(h) };
    runtime().block_on(async move { s.save().await.is_ok() })
}

/// Run the agent loop. Blocks until complete; `cb` fires on a worker thread
/// with a JSON event + `ctx` (marshal to the UI thread on the foreign side).
/// Returns null on success, else an error message (caller frees).
#[no_mangle]
pub extern "C" fn oneai_session_run_task(
    h: SessionHandle,
    task: *const c_char,
    cb: Option<EventCb>,
    ctx: *mut c_void,
) -> *mut c_char {
    if h.is_null() { return return_string("null session".into()); }
    let cb = match cb { Some(f) => f, None => return return_string("no callback".into()) };
    let task = match cstr(task).map(|s| s.to_string()) {
        Some(t) => t,
        None => return return_string("invalid task".into()),
    };
    let s = unsafe { borrow_session(h) };
    let callback: std::sync::Arc<dyn ChatEventCallback> = std::sync::Arc::new(CCallback { cb, ctx });
    match runtime().block_on(async move { s.run_task(task, callback).await }) {
        Ok(()) => std::ptr::null_mut(),
        Err(e) => return_string(err_msg(e)),
    }
}

/// Request the running agent loop to interrupt at the next boundary.
#[no_mangle]
pub extern "C" fn oneai_session_interrupt(h: SessionHandle) {
    if h.is_null() { return; }
    let s = unsafe { borrow_session(h) };
    runtime().block_on(async move { s.interrupt().await });
}

// ─── Group-chat (multi-agent scenario) extern "C" surface ────────────────
//
// The macOS port consumes the high-level uniffi Swift binding for group chat
// (ScenarioSpecView Record + createGroupSession/start/runTask/setScriptedOrder).
// UniFFI 0.32 has no C# generator, so the Windows port P/Invokes these
// `extern "C"` entry points instead. Every call is JSON-in / JSON-out + the
// same `oneai_event_cb`; events carry a `speaker` id (see `push_speaker`) so
// the foreign UI can route fragments to the correct member's bubble.
// `oneai_group_run_task` / `oneai_group_start` BLOCK the caller for the whole
// round (like `oneai_session_run_task`) and fire the callback on a worker
// thread — marshal to the UI thread on the foreign side.

/// Build a multi-agent group-chat session from a scenario JSON (see
/// `parse_scenario` for the shape). Returns an opaque handle, or null on
/// error (call `oneai_last_error`).
#[no_mangle]
pub extern "C" fn oneai_create_group_session(app: AppHandle, scenario_json: *const c_char) -> GroupSessionHandle {
    if app.is_null() { set_last_error("null app".into()); return std::ptr::null_mut(); }
    let spec = match cstr(scenario_json).and_then(parse_scenario) {
        Some(s) => s,
        None => { set_last_error("invalid scenario_json".into()); return std::ptr::null_mut(); }
    };
    let app = unsafe { borrow_app(app) };
    match OneAiGroupChatSession::build(spec, &app.inner) {
        Ok(gs) => Box::into_raw(Box::new(gs)),
        Err(e) => { set_last_error(err_msg(e)); std::ptr::null_mut() }
    }
}

#[no_mangle]
pub extern "C" fn oneai_free_group_session(h: GroupSessionHandle) {
    if !h.is_null() { unsafe { drop(Box::from_raw(h)); } }
}

/// Run the scenario's opener turn (if configured). Call before the first
/// `oneai_group_run_task`. Blocks until complete; `cb` fires on a worker
/// thread. Returns null on success, else an error message (caller frees).
#[no_mangle]
pub extern "C" fn oneai_group_start(
    h: GroupSessionHandle,
    cb: Option<EventCb>,
    ctx: *mut c_void,
) -> *mut c_char {
    if h.is_null() { return return_string("null group session".into()); }
    let cb = match cb { Some(f) => f, None => return return_string("no callback".into()) };
    let gs = unsafe { borrow_group(h) };
    let callback: std::sync::Arc<dyn ChatEventCallback> = std::sync::Arc::new(CCallback { cb, ctx });
    match runtime().block_on(async move { gs.start(callback).await }) {
        Ok(()) => std::ptr::null_mut(),
        Err(e) => return_string(err_msg(e)),
    }
}

/// Append the user's message and run the round's speakers per the turn policy
/// until it's the user's turn again. Blocks; `cb` fires on a worker thread with
/// `speaker`-labeled events. Returns null on success, else an error (caller frees).
#[no_mangle]
pub extern "C" fn oneai_group_run_task(
    h: GroupSessionHandle,
    user_input: *const c_char,
    cb: Option<EventCb>,
    ctx: *mut c_void,
) -> *mut c_char {
    if h.is_null() { return return_string("null group session".into()); }
    let cb = match cb { Some(f) => f, None => return return_string("no callback".into()) };
    let input = match cstr(user_input).map(|s| s.to_string()) {
        Some(t) => t,
        None => return return_string("invalid user_input".into()),
    };
    let gs = unsafe { borrow_group(h) };
    let callback: std::sync::Arc<dyn ChatEventCallback> = std::sync::Arc::new(CCallback { cb, ctx });
    match runtime().block_on(async move { gs.run_task(input, callback).await }) {
        Ok(()) => std::ptr::null_mut(),
        Err(e) => return_string(err_msg(e)),
    }
}

/// Request the running member to interrupt at the next boundary.
#[no_mangle]
pub extern "C" fn oneai_group_interrupt(h: GroupSessionHandle) {
    if h.is_null() { return; }
    let gs = unsafe { borrow_group(h) };
    runtime().block_on(async move { gs.interrupt().await });
}

/// Switch the turn policy to a fixed scripted order at runtime. `order_json`
/// is a JSON string array `["id1","id2"]`. Used by scenarios that change
/// speakers mid-conversation (e.g. interview debrief → coach-only).
/// Returns null on success, else an error (caller frees).
#[no_mangle]
pub extern "C" fn oneai_group_set_scripted_order(h: GroupSessionHandle, order_json: *const c_char) -> *mut c_char {
    if h.is_null() { return return_string("null group session".into()); }
    let order: Vec<String> = match cstr(order_json).and_then(|s| serde_json::from_str::<Vec<String>>(s).ok()) {
        Some(o) => o,
        None => return return_string("invalid order_json (expected [\"id\",..])".into()),
    };
    let gs = unsafe { borrow_group(h) };
    runtime().block_on(async move { gs.set_scripted_order(order).await });
    std::ptr::null_mut()
}

/// Snapshot the shared conversation as speaker-labeled message views (JSON
/// array; caller frees). For replaying a resumed scenario session.
#[no_mangle]
pub extern "C" fn oneai_group_messages(h: GroupSessionHandle) -> *mut c_char {
    if h.is_null() { return std::ptr::null_mut(); }
    let gs = unsafe { borrow_group(h) };
    let msgs = runtime().block_on(async move { gs.messages().await });
    let mut out = String::from("[");
    for (i, m) in msgs.iter().enumerate() {
        if i > 0 { out.push(','); }
        out.push_str(&message_to_json(m));
    }
    out.push(']');
    return_string(out)
}

/// Persist the shared conversation immediately (no-op without SQLite
/// persistence). `run_task` already auto-saves after each round.
#[no_mangle]
pub extern "C" fn oneai_group_save(h: GroupSessionHandle) -> bool {
    if h.is_null() { return false; }
    let gs = unsafe { borrow_group(h) };
    runtime().block_on(async move { gs.save().await.is_ok() })
}

#[no_mangle]
pub extern "C" fn oneai_free_string(p: *mut c_char) {
    if !p.is_null() { unsafe { drop(CString::from_raw(p)); } }
}

// ─── last-error (thread-local; set on failure, read by the foreign side) ─
thread_local! {
    static LAST_ERROR: std::cell::RefCell<Option<CString>> = const { std::cell::RefCell::new(None) };
}
fn set_last_error(msg: String) {
    if let Ok(c) = CString::new(msg) { LAST_ERROR.with(|e| *e.borrow_mut() = Some(c)); }
}
#[no_mangle]
pub extern "C" fn oneai_last_error() -> *const c_char {
    LAST_ERROR.with(|e| {
        e.borrow().as_ref().map(|c| c.as_ptr()).unwrap_or(std::ptr::null())
    })
}

// ─── config parsing (tiny, no serde dep) ──────────────────────────────
struct Cfg { provider: ProviderConfigView, db_path: Option<String>, default_tools: bool }

fn parse_config(json: &str) -> Option<Cfg> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let kind = v.get("kind").and_then(|x| x.as_str())?.to_string();
    let model = v.get("model").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let api_key = v.get("api_key").and_then(|x| x.as_str()).map(|s| s.to_string());
    let base_url = v.get("base_url").and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(|s| s.to_string());
    let host = v.get("host").and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(|s| s.to_string());
    let port = v.get("port").and_then(|x| x.as_u64()).map(|p| p as u16);
    let db_path = v.get("db_path").and_then(|x| x.as_str()).map(|s| s.to_string());
    let default_tools = v.get("default_tools").and_then(|x| x.as_bool()).unwrap_or(true);
    Some(Cfg {
        provider: ProviderConfigView { kind, api_key, base_url, model, host, port },
        db_path, default_tools,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn tmp_db(name: &str) -> String {
        let p = std::env::temp_dir().join(format!(
            "oneai_cfacade_{}_{}_{}.db", name, std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let _ = std::fs::remove_file(&p);
        p.to_string_lossy().into_owned()
    }

    struct Collecting { events: Mutex<Vec<String>> }
    extern "C" fn collect(ctx: *mut c_void, json: *const c_char) {
        let c = unsafe { &*(ctx as *const Collecting) };
        if !json.is_null() {
            let s = unsafe { CStr::from_ptr(json) }.to_str().unwrap().to_string();
            c.events.lock().unwrap().push(s);
        }
    }

    #[test]
    fn create_app_with_mock_provider_in_env() {
        // Build a no-op-gate app with default tools + sqlite; provider is
        // optional — an app with no provider still builds (has_provider=false).
        let db = tmp_db("create");
        let cfg = format!(
            "{{\"kind\":\"openai\",\"api_key\":\"sk-test\",\"model\":\"gpt-4o\",\"db_path\":\"{}\",\"default_tools\":true}}",
            db
        );
        let h = oneai_create_app(unsafe { CStr::from_ptr(cfg.as_ptr() as *const c_char) }.as_ptr());
        assert!(!h.is_null(), "create_app should succeed; err={:?}",
            unsafe { CStr::from_ptr(oneai_last_error()) }.to_str().unwrap_or(""));
        assert!(unsafe { borrow_app(h) }.has_provider());
        oneai_free_app(h);
    }

    #[test]
    fn session_id_and_messages_roundtrip() {
        let db = tmp_db("msg");
        let cfg = format!("{{\"kind\":\"openai\",\"api_key\":\"sk\",\"model\":\"gpt-4o\",\"db_path\":\"{}\"}}", db);
        let c = CString::new(cfg).unwrap();
        let app = oneai_create_app(c.as_ptr());
        assert!(!app.is_null());
        let s = oneai_create_session(app, std::ptr::null());
        assert!(!s.is_null());
        let id_ptr = oneai_session_id(s);
        assert!(!id_ptr.is_null());
        oneai_free_string(id_ptr);
        // messages() on a fresh session is empty
        let m_ptr = oneai_session_messages(s);
        let m = unsafe { CStr::from_ptr(m_ptr) }.to_str().unwrap().to_string();
        oneai_free_string(m_ptr);
        assert_eq!(m, "[]");
        oneai_free_session(s);
        oneai_free_app(app);
    }

    #[test]
    fn event_to_json_shape() {
        let e = ChatEventView::StreamChunk { text: "hi".into(), speaker: None };
        assert_eq!(event_to_json(&e), "{\"type\":\"StreamChunk\",\"text\":\"hi\",\"speaker\":null}");
        let e = ChatEventView::ToolResult { call_id: "1".into(), tool_name: "calc".into(), content: "5".into(), success: true, speaker: Some("interviewer".into()) };
        assert!(event_to_json(&e).contains("\"success\":true"));
        assert!(event_to_json(&e).contains("\"speaker\":\"interviewer\""));
    }

    #[test]
    fn callback_adapter_invokes_c_fn() {
        let c = Box::new(Collecting { events: Mutex::new(vec![]) });
        let ctx = &*c as *const Collecting as *mut c_void;
        let adapter = CCallback { cb: collect, ctx };
        ChatEventCallback::on_event(&adapter, ChatEventView::Thinking { text: "x".into(), speaker: None });
        assert_eq!(c.events.lock().unwrap().len(), 1);
        assert!(c.events.lock().unwrap()[0].contains("Thinking"));
    }

    #[test]
    fn scenario_json_parses() {
        let json = r##"{"members":[
            {"id":"pro","name":"正方","system_prompt":"正方","kind":"openai","model":"gpt-4o","api_key":"sk-x","color":"#4D6BFE","avatar":"arrow.up"},
            {"id":"con","name":"反方","system_prompt":"反方","kind":"ollama","model":"llama3","base_url":"127.0.0.1:11434"}
        ],"turn_policy":"moderator","moderator_id":"pro","opener_agent_id":"pro","opener_line":"hi",
        "title":"辩论","review_loop":{"reviewer_id":"con","approve_marker":"定稿","max_rounds":3}}"##;
        let s = parse_scenario(json).expect("scenario parses");
        assert_eq!(s.members.len(), 2);
        assert_eq!(s.members[0].id, "pro");
        assert_eq!(s.members[0].color.as_deref(), Some("#4D6BFE"));
        assert_eq!(s.members[1].kind, "ollama");
        assert_eq!(s.members[1].base_url.as_deref(), Some("127.0.0.1:11434"));
        assert_eq!(s.turn_policy, "moderator");
        assert_eq!(s.moderator_id.as_deref(), Some("pro"));
        assert_eq!(s.opener_agent_id.as_deref(), Some("pro"));
        assert_eq!(s.title.as_deref(), Some("辩论"));
        let rl = s.review_loop.expect("review_loop");
        assert_eq!(rl.reviewer_id, "con");
        assert_eq!(rl.approve_marker, "定稿");
        assert_eq!(rl.max_rounds, 3);
    }

    #[test]
    fn create_group_session_builds_and_messages_empty() {
        let db = tmp_db("group");
        let cfg = format!(
            "{{\"kind\":\"openai\",\"api_key\":\"sk-test\",\"model\":\"gpt-4o\",\"db_path\":\"{}\"}}",
            db
        );
        let app = oneai_create_app(CString::new(cfg).unwrap().as_ptr());
        assert!(!app.is_null());
        // 2-member scripted scenario. build_member_provider constructs providers
        // without touching the network, and GroupChatSession::new is pure setup —
        // so create_group_session succeeds offline. (We do NOT call run_task.)
        let sc = r#"{"members":[
            {"id":"writer","name":"写手","system_prompt":"起草","kind":"openai","model":"gpt-4o","api_key":"sk-test"},
            {"id":"editor","name":"编辑","system_prompt":"润色","kind":"openai","model":"gpt-4o","api_key":"sk-test"}
        ],"turn_policy":"scripted","script_order":["writer","editor"]}"#;
        let gs = oneai_create_group_session(app, CString::new(sc).unwrap().as_ptr());
        assert!(!gs.is_null(), "create_group_session should succeed; err={:?}",
            unsafe { CStr::from_ptr(oneai_last_error()) }.to_str().unwrap_or(""));
        // Fresh group session has no messages yet.
        let m_ptr = oneai_group_messages(gs);
        let m = unsafe { CStr::from_ptr(m_ptr) }.to_str().unwrap().to_string();
        oneai_free_string(m_ptr);
        assert_eq!(m, "[]");
        oneai_free_group_session(gs);
        oneai_free_app(app);
    }

    #[test]
    fn group_set_scripted_order_rejects_bad_json() {
        let db = tmp_db("order");
        let cfg = format!("{{\"kind\":\"openai\",\"api_key\":\"sk-test\",\"model\":\"gpt-4o\",\"db_path\":\"{}\"}}", db);
        let app = oneai_create_app(CString::new(cfg).unwrap().as_ptr());
        let sc = r#"{"members":[{"id":"a","name":"A","system_prompt":"x","kind":"openai","model":"gpt-4o","api_key":"sk-test"}],"turn_policy":"roundrobin"}"#;
        let gs = oneai_create_group_session(app, CString::new(sc).unwrap().as_ptr());
        assert!(!gs.is_null());
        // Valid order array → null (success).
        let err = oneai_group_set_scripted_order(gs, CString::new(r#"["a"]"#).unwrap().as_ptr());
        assert!(err.is_null());
        // Garbage → error string (non-null).
        let err = oneai_group_set_scripted_order(gs, CString::new("not json").unwrap().as_ptr());
        assert!(!err.is_null());
        oneai_free_string(err);
        oneai_free_group_session(gs);
        oneai_free_app(app);
    }
}
