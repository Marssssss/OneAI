package ai.oneai

import android.app.Activity
import android.os.Bundle
import android.util.Log
import android.widget.ScrollView
import android.widget.TextView
import kotlinx.coroutines.runBlocking
import uniffi.oneai.OneAiAppBuilder

// ──────────────────────────────────────────────────────────────────────
// OneAI Android smoke activity.
//
// Proves the full FFI chain works on-device:
//   1. System.loadLibrary("oneai") (via the generated UniffiLib)
//   2. OneAiAppBuilder().build()  (the async AppBuilder path over FFI)
//   3. app.createSession() / session.sessionId() / app.platform()
//
// No LLM provider is configured — the goal of S2 is just to prove the .so
// loads and the FFI surface is callable. Real chat + provider_config + a
// callback-driven UI land in S3.
// ──────────────────────────────────────────────────────────────────────
class MainActivity : Activity() {

    private val tag = "OneAI"
    private lateinit var logView: TextView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        logView = TextView(this).apply {
            textSize = 16f
            setPadding(32, 32, 32, 32)
        }
        setContentView(ScrollView(this).apply { addView(logView) })

        log("OneAI smoke start — platform.arch=${System.getProperty("os.arch")}")
        // Build off the main thread — build() is suspend and may do real work.
        Thread { initOneAi() }.start()
    }

    private fun initOneAi() {
        try {
            val app = runBlocking { OneAiAppBuilder().build() }
            val session = app.createSession()
            val sid = session.sessionId()
            val platform = app.platform()
            val msg = "OK: app built, sessionId=$sid, platform=$platform, hasProvider=${app.hasProvider()}"
            Log.i(tag, msg)
            runOnUiThread { log("✓ $msg") }
        } catch (e: Throwable) {
            Log.e(tag, "init failed", e)
            runOnUiThread { log("✗ init failed: ${e.message}") }
        }
    }

    private fun log(msg: String) {
        Log.d(tag, msg)
        runOnUiThread { logView.append(msg + "\n\n") }
    }
}
