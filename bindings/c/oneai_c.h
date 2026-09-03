/* ──────────────────────────────────────────────────────────────────────
 * OneAI C facade — the collapsed 3-symbol bus pump (Shape A).
 *
 * Exported from liboneai.dylib / oneai.dll / liboneai.so by
 * crates/oneai-uniffi/src/c_facade.rs. This is the ENTIRE C surface —
 * the legacy 29-symbol OneAiApp/OneAiSession/OneAiGroupSession facade is
 * gone. Everything else rides JSON `Directive` (inbound) / `EngineYield`
 * (outbound) — the oneai-bus protocol, serde-tagged `"kind"`,
 * snake_case: see crates/oneai-bus/src/protocol.rs.
 *
 * Lifecycle:
 *   1. oneai_submit_directive("{\"kind\":\"init\",\"config\":{...}}")
 *      builds the engine + bus + directive pump (once).
 *   2. Submit directives (user_message / approve / interrupt /
 *      create_session / load_session / delete_session / list_sessions /
 *      start_group_chat / group_start / group_user_message / ...) and
 *      drain yields (stream_chunk / thinking / tool_calls / tool_result /
 *      turn_complete / approval_request / session_* / error / ...) from
 *      your poll loop.
 *   3. oneai_shutdown() stops the pump and drops the engine; a later
 *      Init rebuilds it cleanly.
 *
 * Threading:
 *   - oneai_submit_directive blocks until the bus accepts the directive
 *     (a bounded-channel send — fast) and is safe from any thread.
 *   - oneai_poll_yield is non-blocking. Its return pointer aliases a
 *     THREAD-LOCAL buffer, valid only until the next oneai_poll_yield on
 *     the SAME thread: poll from exactly ONE thread, copy the string
 *     immediately, and NEVER free the pointer (there is no free_string).
 *   - Panics never unwind across the boundary (caught and surfaced as an
 *     `error` yield + non-zero return code).
 * ────────────────────────────────────────────────────────────────────── */
#ifndef ONEAI_C_FACADE_H
#define ONEAI_C_FACADE_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Submit one Directive as a NUL-terminated UTF-8 JSON line.
 *
 * {"kind":"init","config":{...}} builds the engine on first call:
 *   {"kind":"init","config":{
 *      "kind":"openai"|"anthropic"|"ollama",
 *      "api_key":"..","base_url":"..","model":"..",
 *      "host":"..","port":11434,
 *      "db_path":"/path/oneai.db","default_tools":true}}
 * Every other kind is forwarded to the engine bus.
 *
 * Returns: 0 ok · 1 null/invalid input · 2 bad JSON · 3 engine already
 * built (submit Shutdown first) · 4 engine build failed · 5 engine not
 * initialized (submit Init first) · 6 bus submit failed · 7 internal
 * panic caught at the FFI boundary (detail rides an `error` yield when an
 * engine exists). */
int32_t oneai_submit_directive(const char* json);

/* Poll the next EngineYield as one NUL-terminated UTF-8 JSON line, or
 * NULL when none is pending. The pointer aliases a thread-local buffer —
 * valid until the next call on the SAME thread; copy it immediately and
 * do NOT free it. Call from exactly one thread (dedicated poll thread). */
const char* oneai_poll_yield(void);

/* Shut the engine down: submits Directive::Shutdown, aborts the pump,
 * drops the engine state. Returns 0 ok · 1 no engine built · 2 internal
 * panic caught. A subsequent Init rebuilds cleanly. */
int32_t oneai_shutdown(void);

#ifdef __cplusplus
}
#endif

#endif /* ONEAI_C_FACADE_H */
