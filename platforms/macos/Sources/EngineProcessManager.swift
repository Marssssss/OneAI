// EngineProcessManager.swift — owns the spawned `oneai app-server` sidecar
// process for the macOS app's sidecar transport. The Codex model applied to
// the desktop: the frontend that CAN spawn a process OWNS the spawn, so the
// user never starts a server manually.
//
// On `start()`: locate the `oneai` binary (bundled in the .app's
// Contents/Resources/bin first, then PATH), spawn
// `oneai app-server --listen ipc://<ephemeral socket>`, wait for the socket
// to appear, then hand the socket path to a `OneAiRpcClient`. On unexpected
// exit, restart with exponential backoff (mirrors the VS Code extension's
// server.ts). On `stop()`, tear down the child + the client.
//
// Selected by UserDefaults `oneai_engine_transport = sidecar`. When the flag
// is `ffi` (default), the app uses the in-process c_facade instead and this
// manager isn't constructed. See Phase D status in the plan — the live
// ChatViewModel rewiring onto this client is the remaining focused step.

import Foundation

public protocol EngineProcessManagerDelegate: AnyObject {
    /// Called once the sidecar is up + the RPC client is connected. The VM
    /// then routes turns/approvals/group/scenario through `client`.
    func engineProcessManager(_ mgr: EngineProcessManager, didStart client: OneAiRpcClient)
    /// Called when the sidecar exited and is being restarted (backoff), or
    /// failed to start (binary missing). The VM surfaces a transport error.
    func engineProcessManager(_ mgr: EngineProcessManager, didFailWith error: Error)
}

/// Manages the `oneai app-server` child process lifecycle + restart.
public final class EngineProcessManager {
    public weak var delegate: EngineProcessManagerDelegate?

    private var process: Process?
    private var client: OneAiRpcClient?
    private var socketPath: String = ""
    private var backoffMs: Double = 500
    private var restartTimer: DispatchSourceTimer?
    private var started = false
    private var stopped = false

    /// The app's bundle-relative `oneai` location, checked before PATH so a
    /// signed .app carries its engine (independent of the user's PATH).
    static func resolveOneaiBin() -> String? {
        // 1. Bundled next to the app executable.
        if let exe = Bundle.main.executableURL?.deletingLastPathComponent() {
            let bundled = exe
                .appendingPathComponent("Resources")
                .appendingPathComponent("bin")
                .appendingPathComponent("oneai")
            if FileManager.default.isExecutableFile(atPath: bundled.path) {
                return bundled.path
            }
        }
        // 2. PATH (so a `cargo install` oneai works without bundling).
        if let found = findOnPath("oneai"), FileManager.default.isExecutableFile(atPath: found) {
            return found
        }
        return nil
    }

    /// Spawn the sidecar + connect the RPC client. Idempotent if already
    /// started.
    public func start() {
        guard !started else { return }
        started = true
        spawn()
    }

    /// Tear down: stop the child, disconnect the client, cancel any restart
    /// timer. Safe to call from app-exit teardown.
    public func stop() {
        stopped = true
        restartTimer?.cancel()
        restartTimer = nil
        client?.disconnect()
        client = nil
        process?.terminate()
        process = nil
        // Remove the ephemeral socket file if present.
        if !socketPath.isEmpty {
            try? FileManager.default.removeItem(atPath: socketPath)
        }
    }

    private func spawn() {
        guard !stopped else { return }
        guard let bin = Self.resolveOneaiBin() else {
            delegate?.engineProcessManager(self, didFailWith: EngineProcessError.binaryMissing)
            return
        }
        // Ephemeral socket: app-server-<pid>.sock so it never clashes with a
        // user's manually-started app-server.sock. Sits in ~/.oneai.
        let oneaiDir = (FileManager.default.homeDirectoryForCurrentUser.path as NSString)
            .appendingPathComponent(".oneai")
        try? FileManager.default.createDirectory(atPath: oneaiDir, withIntermediateDirectories: true)
        let pid = ProcessInfo.processInfo.processIdentifier
        socketPath = (oneaiDir as NSString).appendingPathComponent("app-server-\(pid).sock")
        // A stale socket from a prior crash would block bind — remove first.
        try? FileManager.default.removeItem(atPath: socketPath)

        let p = Process()
        p.executableURL = URL(fileURLWithPath: bin)
        // The child inherits env (provider keys etc.); the app-server reads
        // them via ONEAI_API_KEY / ONEAI_BASE_URL / ONEAI_MODEL just like the
        // CLI.
        p.environment = ProcessInfo.processInfo.environment
        p.arguments = ["app-server", "--listen", "ipc://\(socketPath)"]
        p.standardOutput = FileHandle(forWritingAtPath: "/dev/null")
        p.standardError = FileHandle(forUpdatingAtPath: (oneaiDir as NSString).appendingPathComponent("app-server-sidecar.log"))
        p.terminationHandler = { [weak self] proc in
            self?.handleExit(code: proc.terminationStatus)
        }
        do {
            try p.run()
            process = p
            waitForSocketThenConnect()
        } catch {
            delegate?.engineProcessManager(self, didFailWith: error)
            scheduleRestart()
        }
    }

    /// Poll for the socket file to appear (the app-server binds async), then
    /// connect the RPC client. Bounded retries so a bind failure doesn't hang.
    private func waitForSocketThenConnect(attempt: Int = 0) {
        guard !stopped else { return }
        if FileManager.default.fileExists(atPath: socketPath) {
            connectClient()
            return
        }
        if attempt > 100 { // ~10s @100ms
            delegate?.engineProcessManager(self, didFailWith: EngineProcessError.socketTimeout)
            scheduleRestart()
            return
        }
        DispatchQueue.global().asyncAfter(deadline: .now() + 0.1) { [weak self] in
            self?.waitForSocketThenConnect(attempt: attempt + 1)
        }
    }

    private func connectClient() {
        let c = OneAiRpcClient(socketPath: socketPath)
        // callbackQueue defaults to a background queue; the VM's delegate
        // marshals to main (mirrors the FFI callback path).
        Task { [weak self] in
            guard let self = self else { return }
            do {
                try await c.connect()
                self.client = c
                self.backoffMs = 500 // reset backoff after a healthy start
                self.delegate?.engineProcessManager(self, didStart: c)
            } catch {
                self.delegate?.engineProcessManager(self, didFailWith: error)
                self.scheduleRestart()
            }
        }
    }

    private func handleExit(code: Int32) {
        client?.disconnect()
        client = nil
        if stopped { return }
        // Log + restart with backoff (mirrors server.ts).
        scheduleRestart()
    }

    private func scheduleRestart() {
        guard !stopped else { return }
        let delay = backoffMs
        backoffMs = min(backoffMs * 2, 30_000) // cap at 30s
        let t = DispatchSource.makeTimerSource(queue: .global())
        t.schedule(deadline: .now() + delay / 1000.0)
        t.setEventHandler { [weak self] in
            self?.spawn()
        }
        t.resume()
        restartTimer = t
    }

    // MARK: - PATH lookup

    private static func findOnPath(_ name: String) -> String? {
        guard let path = ProcessInfo.processInfo.environment["PATH"] else { return nil }
        for dir in path.split(separator: ":") {
            let candidate = (String(dir) as NSString).appendingPathComponent(name)
            if FileManager.default.isExecutableFile(atPath: candidate) {
                return candidate
            }
        }
        return nil
    }
}

public enum EngineProcessError: Error {
    case binaryMissing
    case socketTimeout
}
