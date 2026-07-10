// OneAI NAPI module for HarmonyOS — wraps the extern "C" JSON facade
// (crates/oneai-uniffi/src/c_facade.rs, header bindings/c/oneai_c.h) so ArkTS
// can drive the agent. liboneai.so (built by scripts/build_harmony.sh) is
// linked here alongside the NAPI glue.
//
// Why napi_async_work + threadsafe_function: oneai_session_run_task BLOCKS
// the caller for the whole agent loop and fires the C callback on a tokio
// worker thread. Calling it on the ArkTS thread would freeze the UI AND block
// the event loop so the streamed callbacks could never be dispatched. So
// run_task runs on a libuv worker thread (napi_async_work::execute); the
// tokio-thread C callback marshals each event back via
// napi_threadsafe_function, whose call_js_cb runs on the ArkTS thread.
//
// Handles (OneAiApp*/OneAiSession*) are passed as BigInt (u64) — ArkTS keeps
// them and calls oneai_free_app/session on rebuild/dispose.

#include <node_api.h>
#include "oneai_c.h"

#include <cstdlib>
#include <cstring>
#include <string>

#define NAPI_CALL(env, call)                                                    \
    do { if ((call) != napi_ok) { napi_throw_error((env), "EONEAI", #call); return nullptr; } } while (0)

// ── string / bigint helpers ────────────────────────────────────────────
static std::string GetString(napi_env env, napi_value v) {
    size_t len = 0;
    if (napi_get_value_string_utf8(env, v, nullptr, 0, &len) != napi_ok) return "";
    std::string s(len, '\0');
    size_t written = 0;
    napi_get_value_string_utf8(env, v, s.data(), len + 1, &written);
    return s;
}

static napi_value MakeString(napi_env env, const char* s) {
    napi_value v;
    napi_create_string_utf8(env, s ? s : "", NAPI_AUTO_LENGTH, &v);
    return v;
}

static napi_value MakeBigintU64(napi_env env, uint64_t v) {
    napi_value big;
    bool lossless = true;
    napi_create_bigint_uint64(env, v, &big, &lossless);
    return big;
}

static uint64_t GetBigintU64(napi_env env, napi_value v) {
    uint64_t val = 0;
    bool lossless = true;
    if (napi_get_value_bigint_uint64(env, v, &val, &lossless) != napi_ok) return 0;
    return val;
}

// Wrap a nullable returned CString (caller-frees via oneai_free_string) into
// a JS string, freeing the original. null ptr → null JS value (undefined).
static napi_value MakeOwnedString(napi_env env, char* p) {
    if (!p) { napi_value u; napi_get_null(env, &u); return u; }
    napi_value v = MakeString(env, p);
    oneai_free_string(p);
    return v;
}

// ── synchronous wrappers ──────────────────────────────────────────────
static napi_value JsCreateApp(napi_env env, napi_callback_info info) {
    size_t argc = 1; napi_value argv[1];
    napi_get_cb_info(env, info, &argc, argv, nullptr, nullptr);
    std::string cfg = GetString(env, argv[0]);
    OneAiApp* app = oneai_create_app(cfg.c_str());
    return MakeBigintU64(env, reinterpret_cast<uint64_t>(app));
}

static napi_value JsFreeApp(napi_env env, napi_callback_info info) {
    size_t argc = 1; napi_value argv[1];
    napi_get_cb_info(env, info, &argc, argv, nullptr, nullptr);
    oneai_free_app(reinterpret_cast<OneAiApp*>(GetBigintU64(env, argv[0])));
    return nullptr;
}

static napi_value JsHasProvider(napi_env env, napi_callback_info info) {
    size_t argc = 1; napi_value argv[1];
    napi_get_cb_info(env, info, &argc, argv, nullptr, nullptr);
    bool ok = oneai_has_provider(reinterpret_cast<OneAiApp*>(GetBigintU64(env, argv[0])));
    napi_value v; napi_get_boolean(env, ok, &v); return v;
}

static napi_value JsCreateSession(napi_env env, napi_callback_info info) {
    size_t argc = 2; napi_value argv[2];
    napi_get_cb_info(env, info, &argc, argv, nullptr, nullptr);
    OneAiApp* app = reinterpret_cast<OneAiApp*>(GetBigintU64(env, argv[0]));
    // second arg: id string or null → only pass a C string when it's a non-empty string
    napi_valuetype t;
    napi_typeof(env, argv[1], &t);
    std::string id;
    const char* idC = nullptr;
    if (t == napi_string) { id = GetString(env, argv[1]); if (!id.empty()) idC = id.c_str(); }
    OneAiSession* s = oneai_create_session(app, idC);
    return MakeBigintU64(env, reinterpret_cast<uint64_t>(s));
}

static napi_value JsFreeSession(napi_env env, napi_callback_info info) {
    size_t argc = 1; napi_value argv[1];
    napi_get_cb_info(env, info, &argc, argv, nullptr, nullptr);
    oneai_free_session(reinterpret_cast<OneAiSession*>(GetBigintU64(env, argv[0])));
    return nullptr;
}

static napi_value JsSessionId(napi_env env, napi_callback_info info) {
    size_t argc = 1; napi_value argv[1];
    napi_get_cb_info(env, info, &argc, argv, nullptr, nullptr);
    return MakeOwnedString(env, oneai_session_id(reinterpret_cast<OneAiSession*>(GetBigintU64(env, argv[0]))));
}

static napi_value JsListConversations(napi_env env, napi_callback_info info) {
    size_t argc = 1; napi_value argv[1];
    napi_get_cb_info(env, info, &argc, argv, nullptr, nullptr);
    return MakeOwnedString(env, oneai_list_conversations(reinterpret_cast<OneAiApp*>(GetBigintU64(env, argv[0]))));
}

static napi_value JsDeleteConversation(napi_env env, napi_callback_info info) {
    size_t argc = 2; napi_value argv[2];
    napi_get_cb_info(env, info, &argc, argv, nullptr, nullptr);
    OneAiApp* app = reinterpret_cast<OneAiApp*>(GetBigintU64(env, argv[0]));
    std::string id = GetString(env, argv[1]);
    oneai_delete_conversation(app, id.c_str());
    return nullptr;
}

static napi_value JsSessionMessages(napi_env env, napi_callback_info info) {
    size_t argc = 1; napi_value argv[1];
    napi_get_cb_info(env, info, &argc, argv, nullptr, nullptr);
    return MakeOwnedString(env, oneai_session_messages(reinterpret_cast<OneAiSession*>(GetBigintU64(env, argv[0]))));
}

static napi_value JsSessionSave(napi_env env, napi_callback_info info) {
    size_t argc = 1; napi_value argv[1];
    napi_get_cb_info(env, info, &argc, argv, nullptr, nullptr);
    bool ok = oneai_session_save(reinterpret_cast<OneAiSession*>(GetBigintU64(env, argv[0])));
    napi_value v; napi_get_boolean(env, ok, &v); return v;
}

static napi_value JsSessionInterrupt(napi_env env, napi_callback_info info) {
    size_t argc = 1; napi_value argv[1];
    napi_get_cb_info(env, info, &argc, argv, nullptr, nullptr);
    oneai_session_interrupt(reinterpret_cast<OneAiSession*>(GetBigintU64(env, argv[0])));
    return nullptr;
}

// ── async run_task (napi_async_work + threadsafe_function) ────────────
struct RunTaskData {
    OneAiSession* session;
    std::string task;
    std::string error;       // filled in execute; "" → resolve
    napi_threadsafe_function tsfn;
    napi_deferred deferred;
    napi_async_work work;
};

// call_js_cb — runs on the ArkTS thread; invokes the ArkTS onEvent(json).
static void CallJsCb(napi_env env, napi_value jsCb, void* /*ctx*/, void* data) {
    if (!env || !jsCb || !data) return;
    char* json = static_cast<char*>(data);
    napi_value str = MakeString(env, json);
    free(json);
    napi_value undefined;
    napi_get_undefined(env, &undefined);
    napi_call_function(env, undefined, jsCb, 1, &str, nullptr);
}

// The C callback the Rust tokio thread invokes during the run.
static void CEventCallback(void* ctx, const char* eventJson) {
    auto* d = static_cast<RunTaskData*>(ctx);
    if (!d || !d->tsfn) return;
    char* copy = strdup(eventJson ? eventJson : "");
    napi_call_threadsafe_function(d->tsfn, copy, napi_tsfn_nonblocking);
}

// execute — runs on a libuv worker thread; blocks for the whole agent loop.
static void ExecuteCb(napi_env /*env*/, void* data) {
    auto* d = static_cast<RunTaskData*>(data);
    char* err = oneai_session_run_task(d->session, d->task.c_str(), CEventCallback, d);
    if (err) { d->error = err; oneai_free_string(err); }
}

// complete — runs on the ArkTS thread; resolve/reject the promise + cleanup.
static void CompleteCb(napi_env env, napi_status status, void* data) {
    auto* d = static_cast<RunTaskData*>(data);
    napi_release_threadsafe_function(d->tsfn, napi_tsfn_release);
    if (d->error.empty()) {
        napi_value undefined;
        napi_get_undefined(env, &undefined);
        napi_resolve_deferred(env, d->deferred, undefined);
    } else {
        napi_reject_deferred(env, d->deferred, MakeString(env, d->error.c_str()));
    }
    napi_delete_async_work(env, d->work);
    delete d;
    (void)status;
}

static napi_value JsSessionRunTask(napi_env env, napi_callback_info info) {
    size_t argc = 3; napi_value argv[3];
    napi_get_cb_info(env, info, &argc, argv, nullptr, nullptr);
    OneAiSession* session = reinterpret_cast<OneAiSession*>(GetBigintU64(env, argv[0]));
    std::string task = GetString(env, argv[1]);
    napi_value jsCallback = argv[2];

    // 1. Promise for completion.
    napi_value promise;
    napi_deferred deferred;
    napi_create_promise(env, &deferred, &promise);

    // 2. threadsafe function from the ArkTS onEvent callback.
    auto* d = new RunTaskData{ session, task, "", nullptr, deferred, nullptr };
    napi_value name;
    napi_create_string_utf8(env, "oneai_event", NAPI_AUTO_LENGTH, &name);
    napi_status s = napi_create_threadsafe_function(
        env, jsCallback, nullptr, name,
        /*max_queue_size*/ 0, /*initial_thread_count*/ 1,
        /*finalize_data*/ nullptr, /*finalize_cb*/ nullptr,
        /*context*/ d,
        CallJsCb,
        &d->tsfn);
    if (s != napi_ok) {
        napi_value err;
        napi_create_string_utf8(env, "tsfn create failed", NAPI_AUTO_LENGTH, &err);
        napi_reject_deferred(env, deferred, err);
        delete d;
        return promise;
    }

    // 3. async work — execute on a worker thread (run_task blocks), complete on ArkTS thread.
    napi_value workName;
    napi_create_string_utf8(env, "oneai_run_task", NAPI_AUTO_LENGTH, &workName);
    napi_status ws = napi_create_async_work(env, nullptr, workName, ExecuteCb, CompleteCb, d, &d->work);
    if (ws != napi_ok) {
        napi_release_threadsafe_function(d->tsfn, napi_tsfn_release);
        napi_value err;
        napi_create_string_utf8(env, "async_work create failed", NAPI_AUTO_LENGTH, &err);
        napi_reject_deferred(env, deferred, err);
        delete d;
        return promise;
    }
    napi_queue_async_work(env, d->work);
    return promise;
}

// ── module init ───────────────────────────────────────────────────────
static napi_property_descriptor Desc[] = {
    { "createApp",         nullptr, JsCreateApp,         nullptr, nullptr, nullptr, napi_default, nullptr },
    { "freeApp",           nullptr, JsFreeApp,           nullptr, nullptr, nullptr, napi_default, nullptr },
    { "hasProvider",       nullptr, JsHasProvider,       nullptr, nullptr, nullptr, napi_default, nullptr },
    { "createSession",     nullptr, JsCreateSession,     nullptr, nullptr, nullptr, napi_default, nullptr },
    { "freeSession",       nullptr, JsFreeSession,       nullptr, nullptr, nullptr, napi_default, nullptr },
    { "sessionId",         nullptr, JsSessionId,         nullptr, nullptr, nullptr, napi_default, nullptr },
    { "listConversations", nullptr, JsListConversations, nullptr, nullptr, nullptr, napi_default, nullptr },
    { "deleteConversation",nullptr, JsDeleteConversation,nullptr, nullptr, nullptr, napi_default, nullptr },
    { "sessionMessages",   nullptr, JsSessionMessages,   nullptr, nullptr, nullptr, napi_default, nullptr },
    { "sessionSave",       nullptr, JsSessionSave,       nullptr, nullptr, nullptr, napi_default, nullptr },
    { "sessionInterrupt",  nullptr, JsSessionInterrupt,  nullptr, nullptr, nullptr, napi_default, nullptr },
    { "sessionRunTask",    nullptr, JsSessionRunTask,    nullptr, nullptr, nullptr, napi_default, nullptr },
};

static napi_value Init(napi_env env, napi_value exports) {
    NAPI_CALL(env, napi_define_properties(env, exports, sizeof(Desc) / sizeof(Desc[0]), Desc));
    return exports;
}

NAPI_MODULE(NODE_GYP_MODULE_NAME, Init)
