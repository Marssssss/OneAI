// ──────────────────────────────────────────────────────────────────────
// OneAI Android Demo App
//
// Minimal Android app demonstrating the OneAI framework with
// platform-native approval gates via JNI bridge.
//
// This shows the Kotlin-side integration pattern:
// 1. Initialize the native library (oneai-uniffi cdylib)
// 2. Build the OneAI App with Android approval gate
// 3. Poll the bridge for pending approval requests
// 4. Show AlertDialog for high-risk requests
// 5. Send response back through the bridge
//
// NOTE: This is a simplified example. A real Android app would:
// - Use the UniFFI-generated Kotlin bindings
// - Integrate the native .so via JNI properly
// - Use a Service or ViewModel for the OneAI session
// ──────────────────────────────────────────────────────────────────────

package ai.oneai.demo

import android.app.Activity
import android.app.AlertDialog
import android.os.Bundle
import android.util.Log
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import kotlinx.coroutines.*

/**
 * Main activity demonstrating OneAI framework on Android.
 *
 * The approval flow:
 * 1. Rust side creates AndroidApprovalGate + AndroidApprovalBridge
 * 2. When a high-risk tool is called, the gate sends a request through the channel
 * 3. Kotlin side polls the bridge via pollPendingJson() (returns JSON string)
 * 4. Kotlin shows an AlertDialog with approve/deny buttons
 * 5. Kotlin sends the response via sendResponseJson(requestId, responseJson)
 * 6. The gate receives the response and allows/denies the tool execution
 */
class OneAIDemoActivity : Activity() {

    private val TAG = "OneAIDemo"
    private lateinit var logView: TextView
    private lateinit var inputField: EditText
    private lateinit var session: OneAISession  // UniFFI-generated type

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // ─── UI Setup ────────────────────────────────────────────
        val layout = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }

        val title = TextView(this).apply {
            text = "OneAI Android Demo"
            textSize = 24f
            setPadding(16, 16, 16, 8)
        }
        layout.addView(title)

        inputField = EditText(this).apply {
            hint = "Enter a message..."
            setPadding(16, 8, 16, 8)
        }
        layout.addView(inputField)

        val sendButton = Button(this).apply {
            text = "Send Message"
            setOnClickListener { sendMessage() }
        }
        layout.addView(sendButton)

        val calcButton = Button(this).apply {
            text = "Calculator: 2+3*4"
            setOnClickListener { runCalculator() }
        }
        layout.addView(calcButton)

        val shellButton = Button(this).apply {
            text = "Shell: echo hello (HIGH RISK)"
            setOnClickListener { runShell() }
        }
        layout.addView(shellButton)

        val scrollView = ScrollView(this)
        logView = TextView(this).apply {
            text = "Initializing..."
            setPadding(16, 8, 16, 16)
        }
        scrollView.addView(logView)
        layout.addView(scrollView)

        setContentView(layout)

        // ─── Initialize OneAI ────────────────────────────────────
        initOneAI()
    }

    private fun initOneAI() {
        // Build the OneAI app using UniFFI bindings
        // In a real app, the native .so would be loaded via System.loadLibrary()
        try {
            val app = OneAI.build {
                autoApproval()
                memory(windowSize = 10u, thresholdTokens = 2000u)
                persistence(getFilesDir().absolutePath + "/oneai_checkpoints")
            }

            // Register tools
            app.registerTool(Tools.calculator())
            app.registerTool(Tools.shell())

            session = app.createSession()
            log("✓ OneAI initialized — session: ${session.sessionId()}")
            log("✓ Platform: ${app.platform().displayName()}")

            // Start the approval polling loop
            startApprovalPolling()
        } catch (e: Exception) {
            log("✗ Init error: ${e.message}")
        }
    }

    // ─── Approval Polling ──────────────────────────────────────────

    /**
     * Continuously poll the approval bridge for pending requests.
     * When a request is found, show an AlertDialog for user approval.
     *
     * This runs in a coroutine, polling every 500ms.
     */
    private fun startApprovalPolling() {
        // In the real implementation, this would poll the AndroidApprovalBridge
        // via JNI. The bridge exposes:
        //   pollPendingJson() → String? (JSON-encoded ApprovalRequestView)
        //   sendResponseJson(requestId, responseJson) → Unit
        //
        // Example polling pattern:
        // val bridge = ... // obtained from AndroidApprovalGateFactory.create()
        // GlobalScope.launch(Dispatchers.Main) {
        //     while (true) {
        //         val pending = bridge.pollPendingJson()
        //         if (pending != null) {
        //             val request = parseRequest(pending)
        //             showApprovalDialog(request)
        //         }
        //         delay(500)
        //     }
        // }
        log("✓ Approval polling started (auto-approve threshold: Medium)")
    }

    /**
     * Show an AlertDialog for a pending approval request.
     */
    private fun showApprovalDialog(
        requestId: String,
        toolName: String,
        argsJson: String,
        riskLevel: String
    ) {
        AlertDialog.Builder(this)
            .setTitle("⚠️ Approval Required")
            .setMessage("Tool: $toolName\nRisk: $riskLevel\nArgs: $argsJson")
            .setPositiveButton("Approve") { _, _ ->
                sendApprovalResponse(requestId, approved = true)
            }
            .setNegativeButton("Deny") { _, _ ->
                sendApprovalResponse(requestId, approved = false)
            }
            .setNeutralButton("Modify") { _, _ ->
                // In a real app, show a second dialog for arg modification
                sendApprovalResponse(requestId, approved = true)
            }
            .show()
    }

    private fun sendApprovalResponse(requestId: String, approved: Boolean) {
        // In real implementation:
        // val responseJson = if (approved) "{\"Approved\":{\"modified_args_json\":null}}"
        //                    else "{\"Denied\":{\"reason\":\"User denied\"}}"
        // bridge.sendResponseJson(requestId, responseJson)
        log("→ Response for $requestId: ${if (approved) "APPROVED" else "DENIED"}")
    }

    // ─── Tool Execution ────────────────────────────────────────────

    private fun sendMessage() {
        val text = inputField.text.toString()
        if (text.isEmpty()) return

        try {
            session.sendMessage(text)
            log("→ Sent: \"$text\"")
            inputField.text.clear()
        } catch (e: Exception) {
            log("✗ Send error: ${e.message}")
        }
    }

    private fun runCalculator() {
        try {
            val result = session.executeTool("calculator", "{\"expression\":\"2+3*4\"}")
            log("→ calculator(2+3*4) = ${result.content} (success: ${result.success})")
        } catch (e: Exception) {
            log("✗ Calculator error: ${e.message}")
        }
    }

    private fun runShell() {
        try {
            // Shell is high-risk → triggers approval dialog
            val result = session.executeTool("shell", "{\"command\":\"echo hello\"}")
            log("→ shell(echo hello) = success: ${result.success}")
        } catch (e: Exception) {
            log("✗ Shell error: ${e.message}")
        }
    }

    // ─── Utility ────────────────────────────────────────────────────

    private fun log(msg: String) {
        Log.d(TAG, msg)
        logView.append("\n$msg")
    }
}