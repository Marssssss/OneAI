// OneAIBusPump.cpp
// HarmonyOS (ArkTS/NAPI) in-process frontend for the engine bus — the Shape A
// counterpart to the macOS/Windows socket sidecar (Shape B). HarmonyOS can't
// run a sidecar, so the app links `liboneai.so` and drives the engine through
// the 3 `extern "C"` symbols P4 collapsed the facade to:
//
//   int32_t      oneai_submit_directive(const char* json);
//   const char*  oneai_poll_yield(void);   // null = none; valid until next call
//   int32_t      oneai_shutdown(void);
//
// This is a NAPI skeleton: the native module exports `submitDirective` /
// `pollYield` / `shutdown` to ArkTS (which runs the poll loop + UI), calling
// the 3 C symbols. Wire framing + `kind` tags are identical to the sidecar's —
// see `crates/oneai-bus/src/protocol.rs`.
//
// SOURCE ONLY — built inside a HarmonyOS native module on a machine with
// DevEco Studio + the rust cross target (see the bus plan's Phase 4). The
// `napi_*` calls below are the canonical HarmonyOS N-API (Node-API) surface.

#include <cstdint>
#include <cstring>
#include <string>
#include "napi/native_api.h"

// ── the 3 extern "C" symbols (defined in liboneai.so) ─────────────────────
extern "C" {
int32_t oneai_submit_directive(const char* json);
const char* oneai_poll_yield(void);
int32_t oneai_shutdown(void);
}

// ── ArkTS-facing wrappers ───────────────────────────────────────────────────

// submitDirective(json: string): number  — submit a Directive (JSON). 0 = ok.
static napi_value SubmitDirective(napi_env env, napi_callback_info info) {
    size_t argc = 1;
    napi_value args[1];
    napi_get_cb_info(env, info, &argc, args, nullptr, nullptr);
    if (argc < 1) {
        napi_value zero;
        napi_create_int32(env, 2, &zero); // 2 = invalid arg (mirrors c_facade)
        return zero;
    }
    size_t len = 0;
    napi_get_value_string_utf8(env, args[0], nullptr, 0, &len);
    std::string json(len, '\0');
    napi_get_value_string_utf8(env, args[0], json.data(), len + 1, &len);
    int32_t rc = oneai_submit_directive(json.c_str());
    napi_value result;
    napi_create_int32(env, rc, &result);
    return result;
}

// pollYield(): string | null  — the next EngineYield as one JSON line, or null.
// The pointer from `oneai_poll_yield` is valid only until the next call on the
// same thread — we copy it into an napi string immediately.
static napi_value PollYield(napi_env env, napi_callback_info /*info*/) {
    const char* ptr = oneai_poll_yield();
    if (ptr == nullptr) {
        napi_value null;
        napi_get_null(env, &null);
        return null;
    }
    napi_value str;
    napi_create_string_utf8(env, ptr, NAPI_AUTO_LENGTH, &str);
    return str;
}

// shutdown(): number  — submit Directive::Shutdown, stop the pump.
static napi_value Shutdown(napi_env env, napi_callback_info /*info*/) {
    int32_t rc = oneai_shutdown();
    napi_value result;
    napi_create_int32(env, rc, &result);
    return result;
}

// ── module registration ─────────────────────────────────────────────────────
EXTERN_C_START
static napi_value Init(napi_env env, napi_value exports) {
    napi_property_descriptor desc[] = {
        {"submitDirective", nullptr, SubmitDirective, nullptr, nullptr, nullptr, napi_default, nullptr},
        {"pollYield",       nullptr, PollYield,       nullptr, nullptr, nullptr, napi_default, nullptr},
        {"shutdown",        nullptr, Shutdown,        nullptr, nullptr, nullptr, napi_default, nullptr},
    };
    napi_define_properties(env, exports, sizeof(desc) / sizeof(desc[0]), desc);
    return exports;
}
EXTERN_C_END

static napi_module oneaiBusModule = {
    .nm_version = 1,
    .nm_flags = 0,
    .nm_filename = nullptr,
    .nm_register_func = Init,
    .nm_modname = "oneai.bus",
    .nm_priv = nullptr,
    .reserved = {0},
};

extern "C" __attribute__((constructor)) void RegisterOneAIBusModule(void) {
    napi_module_register(&oneaiBusModule);
}

// ArkTS usage (in the entry ability / a worker thread):
//
//   import bus from 'liboneai.bus.so';
//   bus.submitDirective(JSON.stringify({kind: 'init', config: {...}}));
//   setInterval(() => {
//     const line = bus.pollYield();
//     if (line) { /* parse, route by kind; approval_request → dialog → Approve */ }
//   }, 50);  // 20fps — poll on a dedicated worker so the buffer stays consistent
