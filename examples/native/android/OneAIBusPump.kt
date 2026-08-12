// OneAIBusPump.kt
// Android in-process frontend for the engine bus — the Shape A counterpart to
// `windows/OneAIBusClient.cs` (Shape B, socket sidecar). Android links
// `liboneai.so` and drives the engine through the 3 `extern "C"` symbols P4
// collapsed the facade to:
//
//   int32_t  oneai_submit_directive(const char* json);
//   const char* oneai_poll_yield(void);   // null = none; valid until next call
//   int32_t oneai_shutdown(void);
//
// The pump owns a dedicated `HandlerThread` (the poll buffer is thread-local —
// it MUST be polled from one thread). A 20fps `Handler` post drains
// `oneai_poll_yield` and routes each yield (one JSON line) to the listener;
// the `approval_request` arm shows an `AlertDialog` and replies with a
// `Directive::Approve`.
//
// Wire framing + `kind` tags are identical to the sidecar's — see
// `crates/oneai-bus/src/protocol.rs` and `examples/native/macos/OneAIBusClient.swift`.
//
// SOURCE ONLY — built inside the Android app module on a machine with the
// Android NDK + `cargo-ndk` (see `scripts/build_android.sh`). The 3 symbols
// are `external`-declared here (the cdylib exports them `#[no_mangle]`).

package oneai.bus

import android.app.AlertDialog
import android.content.Context
import android.os.Handler
import android.os.HandlerThread
import org.json.JSONObject

/** One yield the engine produced. */
data class BusYield(val kind: String, val json: JSONObject)

interface OneAIBusPumpListener {
    /** Called for every yield off the pump. `approval_request` is handled
     * internally (the listener never sees it); route everything else by
     * `speaker` for group chat. */
    fun onYield(pump: OneAIBusPump, yield: BusYield)
    fun onShutdown(pump: OneAIBusPump)
}

class OneAIBusPump(private val context: Context) {
    var listener: OneAIBusPumpListener? = null

    private val pollThread = HandlerThread("oneai.bus.pump.poll").apply { start() }
    private val pollHandler = Handler(pollThread.looper)
    private val pollRunnable = object : Runnable {
        override fun run() { drain(); pollHandler.postDelayed(this, 50L /* 20fps */) }
    }

    companion object {
        init { System.loadLibrary("oneai") }
    }

    // ── 3 extern "C" symbols ──────────────────────────────────────────
    private external fun oneai_submit_directive(json: String): Int
    // The C symbol returns `const char*` (null = none, valid until next call).
    // JNI maps it to String? (a copy — the buffer is invalidated by the next
    // poll on this thread, but the JNI string is independent).
    private external fun oneai_poll_yield(): String?
    private external fun oneai_shutdown(): Int

    // ── Lifecycle ──────────────────────────────────────────────────────

    /** Submit a `Directive::Init { config }` to build the engine + bus + pump.
     * Call once at app launch. Returns the submit status (0 = ok). */
    fun initialize(config: JSONObject): Int =
        submitDirective(JSONObject().put("kind", "init").put("config", config))

    /** Start the 20fps poll loop. Idempotent. */
    fun start() {
        pollHandler.post(pollRunnable)
    }

    /** Shut the engine down — submits `Directive::Shutdown`, stops polling. */
    fun shutdown() {
        oneai_shutdown()
        pollHandler.removeCallbacks(pollRunnable)
        listener?.onShutdown(this)
    }

    // ── Sending directives ────────────────────────────────────────────

    /** Send a `Directive` (full JSON incl. `kind`). Returns 0 on success. */
    fun submitDirective(payload: JSONObject): Int =
        oneai_submit_directive(payload.toString())

    fun sendUserMessage(text: String) {
        submitDirective(JSONObject().apply {
            put("kind", "user_message")
            put("content", arrayOf(JSONObject().put("type", "text").put("text", text)))
        })
    }

    fun sendInterrupt(reason: String) {
        submitDirective(JSONObject().apply {
            put("kind", "interrupt")
            put("reason", JSONObject().put("Custom", JSONObject().put("reason", reason)))
        })
    }

    /** Reply to an `approval_request` yield. `proceed == true` sends
     * `InteractionResponse::Proceed` (a bare `"Proceed"` JSON string). */
    fun respondToApproval(requestId: String, proceed: Boolean) {
        val response: Any = if (proceed) "Proceed"
                            else JSONObject().put("Abort", JSONObject().put("reason", "user denied"))
        submitDirective(JSONObject().apply {
            put("kind", "approve")
            put("request_id", requestId)
            put("response", response)
        })
    }

    // ── Poll loop ─────────────────────────────────────────────────────

    /** Drain every available yield off the bus (non-blocking). Runs on the
     * dedicated poll thread so the thread-local buffer is consistent. */
    private fun drain() {
        while (true) {
            val line = oneai_poll_yield() ?: return // no yield pending
            val obj = JSONObject(line)
            val kind = obj.optString("kind")
            if (kind.isEmpty()) continue
            val yield = BusYield(kind, obj)
            // Route to the UI thread; the poll thread must NOT block on UI.
            pollHandler.post { handleYield(yield) }
        }
    }

    private fun handleYield(yield: BusYield) {
        if (yield.kind == "approval_request") {
            val requestId = yield.json.optString("request_id")
            val req = yield.json.optJSONObject("request")
            presentApproval(requestId, req)
            return
        }
        listener?.onYield(this, yield)
    }

    /** Present an `AlertDialog` for a tool-approval request and reply. Wire
     * into your Activity/Fragment in a real app — the skeleton uses `context`
     * directly (replace with a proper presenter). */
    private fun presentApproval(requestId: String, req: JSONObject?) {
        val tool = req?.optString("tool_name") ?: "tool"
        AlertDialog.Builder(context)
            .setTitle("Approve tool?")
            .setMessage("$tool\n${req?.opt("args") ?: ""}")
            .setPositiveButton("Approve") { _, _ -> respondToApproval(requestId, true) }
            .setNegativeButton("Deny") { _, _ -> respondToApproval(requestId, false) }
            .show()
    }
}
