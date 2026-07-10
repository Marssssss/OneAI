/* ──────────────────────────────────────────────────────────────────────
 * OneAI C facade — extern "C" JSON ABI over the uniffi wrappers.
 *
 * Exported from liboneai.dylib / oneai.dll / liboneai.so alongside the
 * uniffi symbols. For foreign runtimes UniFFI 0.32 can't generate bindings
 * for (C# / NAPI-C++): every call is JSON-in / JSON-out + one C callback,
 * so neither side knows the uniffi wire protocol.
 *
 * Threading: each entry drives a shared multi-thread tokio runtime.
 * oneai_session_run_task BLOCKS the caller for the whole agent loop and
 * fires the callback on a worker thread — marshal to your UI thread.
 * oneai_session_interrupt is safe to call from a different thread.
 *
 * Ownership: strings returned by oneai_* (session_id, list_conversations,
 * session_messages, run_task error) are heap CString; free with
 * oneai_free_string. oneai_last_error returns a borrowed pointer (do NOT free).
 * ────────────────────────────────────────────────────────────────────── */
#ifndef ONEAI_C_FACADE_H
#define ONEAI_C_FACADE_H

#include <stddef.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque handles (heap-allocated Arc). */
typedef struct OneAiApp OneAiApp;
typedef struct OneAiSession OneAiSession;

/* Streaming event callback. Fires on a tokio worker thread.
 * event_json is a NUL-terminated UTF-8 string, borrowed for the duration
 * of the call only — copy it before returning. ctx is passed through. */
typedef void (*oneai_event_cb)(void* ctx, const char* event_json);

/* ── App lifecycle ─────────────────────────────────────────────────────
 * config_json:
 *   {"kind":"openai"|"anthropic"|"ollama",
 *    "api_key":"..","base_url":"..","model":"..",
 *    "host":"..","port":11434,
 *    "db_path":"/path/oneai.db","default_tools":true}
 * Returns null on error → call oneai_last_error. */
OneAiApp* oneai_create_app(const char* config_json);
void      oneai_free_app(OneAiApp* app);
bool      oneai_has_provider(OneAiApp* app);

/* ── Sessions ──────────────────────────────────────────────────────────
 * id null/empty → new conversation; else resume by id (history loaded). */
OneAiSession* oneai_create_session(OneAiApp* app, const char* id);
void          oneai_free_session(OneAiSession* s);
char*         oneai_session_id(OneAiSession* s);            /* free w/ oneai_free_string */

/* ── Conversations ───────────────────────────────────────────────────── */
/* JSON array of {"id","title"(null),"message_count","updated_at_ms"} */
char* oneai_list_conversations(OneAiApp* app);              /* free w/ oneai_free_string */
void  oneai_delete_conversation(OneAiApp* app, const char* id);

/* ── Messages (replay) ───────────────────────────────────────────────── */
/* JSON array of {"role":"user"|"assistant"|"system"|"tool","text":".."} */
char* oneai_session_messages(OneAiSession* s);              /* free w/ oneai_free_string */
bool  oneai_session_save(OneAiSession* s);

/* ── Run the agent loop (BLOCKS until complete) ───────────────────────
 * Returns null on success; else an error message (free w/ oneai_free_string). */
char* oneai_session_run_task(OneAiSession* s, const char* task,
                             oneai_event_cb cb, void* ctx);
void  oneai_session_interrupt(OneAiSession* s);

/* ── String + error ──────────────────────────────────────────────────── */
void  oneai_free_string(char* s);
const char* oneai_last_error(void);   /* borrowed, do NOT free */

#ifdef __cplusplus
}
#endif

#endif /* ONEAI_C_FACADE_H */
