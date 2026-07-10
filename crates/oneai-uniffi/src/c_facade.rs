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
        ChatEventView::StreamChunk { text } => {
            o.push_str("{\"type\":\"StreamChunk\",\"text\":"); j_escape(text, &mut o); o.push('}');
        }
        ChatEventView::Thinking { text } => {
            o.push_str("{\"type\":\"Thinking\",\"text\":"); j_escape(text, &mut o); o.push('}');
        }
        ChatEventView::ToolCall { id, name, args_json } => {
            o.push_str("{\"type\":\"ToolCall\",\"id\":"); j_escape(id, &mut o);
            o.push_str(",\"name\":"); j_escape(name, &mut o);
            o.push_str(",\"args_json\":"); j_escape(args_json, &mut o); o.push('}');
        }
        ChatEventView::ToolResult { call_id, tool_name, content, success } => {
            o.push_str("{\"type\":\"ToolResult\",\"call_id\":"); j_escape(call_id, &mut o);
            o.push_str(",\"tool_name\":"); j_escape(tool_name, &mut o);
            o.push_str(",\"content\":"); j_escape(content, &mut o);
            o.push_str(",\"success\":"); o.push_str(if *success { "true" } else { "false" });
            o.push('}');
        }
        ChatEventView::DirectAnswer { text } => {
            o.push_str("{\"type\":\"DirectAnswer\",\"text\":"); j_escape(text, &mut o); o.push('}');
        }
        ChatEventView::Complete { final_text } => {
            o.push_str("{\"type\":\"Complete\",\"final_text\":"); j_escape(final_text, &mut o); o.push('}');
        }
        ChatEventView::Error { message } => {
            o.push_str("{\"type\":\"Error\",\"message\":"); j_escape(message, &mut o); o.push('}');
        }
    }
    o
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

// InterruptReason import kept honest — interrupt() is delegated to the
// uniffi OneAISession, which builds the reason internally.

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
        let e = ChatEventView::StreamChunk { text: "hi".into() };
        assert_eq!(event_to_json(&e), "{\"type\":\"StreamChunk\",\"text\":\"hi\"}");
        let e = ChatEventView::ToolResult { call_id: "1".into(), tool_name: "calc".into(), content: "5".into(), success: true };
        assert!(event_to_json(&e).contains("\"success\":true"));
    }

    #[test]
    fn callback_adapter_invokes_c_fn() {
        let c = Box::new(Collecting { events: Mutex::new(vec![]) });
        let ctx = &*c as *const Collecting as *mut c_void;
        let adapter = CCallback { cb: collect, ctx };
        ChatEventCallback::on_event(&adapter, ChatEventView::Thinking { text: "x".into() });
        assert_eq!(c.events.lock().unwrap().len(), 1);
        assert!(c.events.lock().unwrap()[0].contains("Thinking"));
    }
}
