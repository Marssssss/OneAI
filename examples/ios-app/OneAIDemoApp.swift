// ──────────────────────────────────────────────────────────────────────
// OneAI iOS Demo App
//
// Minimal iOS app demonstrating the OneAI framework with
// platform-native approval gates via C callback bridge.
//
// The approval flow:
// 1. Rust side creates IOSApprovalGate + IOSApprovalBridge
// 2. Swift registers a C callback function: `extern "C" fn(*const c_char)`
// 3. When a high-risk tool is called, the bridge invokes the callback
//    with JSON-encoded approval request data
// 4. Swift parses the JSON, presents UIAlertController with Approve/Deny
// 5. Swift sends response back via the bridge's send_response method
// 6. The gate receives the response and allows/denies the tool execution
//
// NOTE: This is a simplified example. A real iOS app would:
// - Use the UniFFI-generated Swift bindings
// - Integrate via XCFramework
// - Use a ViewModel for the OneAI session
// ──────────────────────────────────────────────────────────────────────

import SwiftUI
import UIKit

// ─── OneAI iOS Demo View ──────────────────────────────────────────────

struct OneAIDemoView: View {
    @StateObject private var viewModel = OneAIDemoViewModel()

    var body: some View {
        NavigationView {
            VStack(spacing: 16) {
                // ─── Status Header ────────────────────────────────────
                HStack {
                    Text("OneAI iOS Demo")
                        .font(.title2)
                        .fontWeight(.bold)
                    Spacer()
                    Text(viewModel.platformName)
                        .font(.caption)
                        .foregroundColor(.secondary)
                }
                .padding(.horizontal)

                // ─── Session Info ──────────────────────────────────────
                Text("Session: \(viewModel.sessionId)")
                    .font(.caption)
                    .foregroundColor(.secondary)

                // ─── Message Input ──────────────────────────────────────
                HStack {
                    TextField("Enter a message...", text: $viewModel.inputText)
                        .textFieldStyle(RoundedBorderTextFieldStyle())
                    Button("Send") {
                        viewModel.sendMessage()
                    }
                    .buttonStyle(.borderedProminent)
                }
                .padding(.horizontal)

                // ─── Tool Buttons ──────────────────────────────────────
                HStack(spacing: 12) {
                    Button("Calculator") {
                        viewModel.runCalculator()
                    }
                    .buttonStyle(.bordered)

                    Button("Shell (⚠️ High Risk)") {
                        viewModel.runShell()
                    }
                    .buttonStyle(.bordered)
                    .tint(.orange)
                }
                .padding(.horizontal)

                // ─── Log Output ──────────────────────────────────────
                ScrollView {
                    LazyVStack(alignment: .leading) {
                        ForEach(viewModel.logLines, id: \.self) { line in
                            Text(line)
                                .font(.system(size: 12))
                                .foregroundColor(line.hasPrefix("✗") ? .red :
                                                  line.hasPrefix("⚠") ? .orange : .primary)
                        }
                    }
                    .padding(.horizontal)
                }

                Spacer()
            }
            .navigationBarHidden(true)
        }
    }
}

// ─── View Model ────────────────────────────────────────────────────────

class OneAIDemoViewModel: ObservableObject {
    @Published var sessionId: String = "Not initialized"
    @Published var platformName: String = "Unknown"
    @Published var inputText: String = ""
    @Published var logLines: [String] = ["Initializing..."]

    private var app: OneAIApp?      // UniFFI-generated type
    private var session: OneAISession?

    init() {
        initializeOneAI()
    }

    // ─── Initialization ────────────────────────────────────────────

    private func initializeOneAI() {
        // Build the OneAI app using UniFFI bindings
        // In a real app, the native XCFramework would be linked in Xcode
        do {
            let builtApp = try OneAI.build { config in
                _ = OneAI.autoApproval(&config)
                _ = OneAI.memory(windowSize: 10, thresholdTokens: 2000, &config)
                _ = OneAI.persistence("/tmp/oneai_ios_checkpoints", &config)
            }

            // Register tools
            builtApp.registerTool(OneAITools.calculator())
            builtApp.registerTool(OneAITools.shell())

            let newSession = builtApp.createSession()

            self.app = builtApp
            self.session = newSession
            self.sessionId = newSession.sessionId()
            self.platformName = builtApp.platform().displayName

            addLog("✓ OneAI initialized — session: \(sessionId)")
            addLog("✓ Platform: \(platformName)")

            // ─── Register Approval Callback ──────────────────────────
            // The iOS bridge uses C callbacks for approval flow:
            // Swift registers `extern "C" fn(*const c_char)` callback
            // When a high-risk request arrives, the callback is invoked
            // with JSON data, and Swift presents UIAlertController
            registerApprovalCallback()

        } catch {
            addLog("✗ Init error: \(error.localizedDescription)")
        }
    }

    // ─── Approval Callback Registration ──────────────────────────────

    /**
     * Register a C callback with the IOSApprovalBridge.
     *
     * The callback receives a C string (JSON-encoded ApprovalRequestView)
     * and must parse it, show a dialog, and send a response back.
     *
     * Pattern:
     *   bridge.register_callback(approvalCallback)
     *
     * The callback signature is:
     *   extern "C" fn(request_json: *const c_char) -> ()
     */
    private func registerApprovalCallback() {
        // In a real implementation:
        // let bridge = ... // obtained from IOSApprovalGateFactory.create()
        // bridge.register_callback { requestJson in
        //     self.showApprovalDialog(requestJson: requestJson)
        // }
        addLog("✓ Approval callback registered (threshold: Medium)")
    }

    /**
     * Show a UIAlertController for a pending approval request.
     * Called when the C callback fires with a JSON-encoded request.
     */
    private func showApprovalDialog(requestJson: String) {
        // Parse the request JSON to extract tool name, args, risk level
        // Then present a UIAlertController with Approve/Deny actions

        DispatchQueue.main.async {
            guard let windowScene = UIApplication.shared.connectedScenes.first as? UIWindowScene,
                  let rootVC = windowScene.windows.first?.rootViewController else { return }

            let alert = UIAlertController(
                title: "⚠️ Approval Required",
                message: "Tool approval request:\n\(requestJson)",
                preferredStyle: .alert
            )

            alert.addAction(UIAlertAction(title: "Approve", style: .default) { _ in
                self.sendApprovalResponse(approved: true)
            })

            alert.addAction(UIAlertAction(title: "Deny", style: .destructive) { _ in
                self.sendApprovalResponse(approved: false)
            })

            alert.addAction(UIAlertAction(title: "Modify Args", style: .default) { _ in
                self.sendApprovalResponse(approved: true) // simplified
            })

            rootVC.present(alert, animated: true)
        }
    }

    private func sendApprovalResponse(approved: Bool) {
        // In real implementation:
        // let responseJson = approved
        //     ? "{\"Approved\":{\"modified_args_json\":null}}"
        //     : "{\"Denied\":{\"reason\":\"User denied\"}}"
        // bridge.send_response(requestId, responseJson)
        addLog("→ Response: \(approved ? "APPROVED" : "DENIED")")
    }

    // ─── Tool Execution ────────────────────────────────────────────

    func sendMessage() {
        guard let session = session, !inputText.isEmpty else { return }

        do {
            try session.sendMessage(inputText)
            addLog("→ Sent: \"\(inputText)\"")
            inputText = ""
        } catch {
            addLog("✗ Send error: \(error.localizedDescription)")
        }
    }

    func runCalculator() {
        guard let session = session else { return }

        do {
            let result = try session.executeTool(name: "calculator", argsJson: "{\"expression\":\"2+3*4\"}")
            addLog("→ calculator(2+3*4) = \(result.content) (success: \(result.success))")
        } catch {
            addLog("✗ Calculator error: \(error.localizedDescription)")
        }
    }

    func runShell() {
        guard let session = session else { return }

        do {
            // Shell is high-risk → triggers approval dialog via callback
            let result = try session.executeTool(name: "shell", argsJson: "{\"command\":\"echo hello\"}")
            addLog("→ shell(echo hello) = success: \(result.success)")
        } catch {
            addLog("✗ Shell error: \(error.localizedDescription)")
        }
    }

    // ─── Utility ────────────────────────────────────────────────────

    private func addLog(_ line: String) {
        logLines.append(line)
    }
}

// ─── SwiftUI App Entry ──────────────────────────────────────────────

@main
struct OneAIDemoApp: App {
    var body: some Scene {
        WindowGroup {
            OneAIDemoView()
        }
    }
}