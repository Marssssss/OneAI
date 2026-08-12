// OneAIBusPump.swift
// iOS in-process frontend for the engine bus — the Shape A counterpart to
// `macos/OneAIBusClient.swift` (Shape B, socket sidecar). iOS can't run a
// sidecar (sandbox / App Store rules), so the app links `liboneai.a` and
// drives the engine through the 3 `extern "C"` symbols P4 collapsed the
// facade to:
//
//   int32_t  oneai_submit_directive(const char* json);
//   const char* oneai_poll_yield(void);   // null = none; valid until next call
//   int32_t  oneai_shutdown(void);
//
// The pump owns a dedicated serial queue (the poll buffer is thread-local —
// it MUST be polled from one thread). A 20fps `DispatchSource` timer drains
// `oneai_poll_yield` and routes each yield (one newline-terminated JSON line)
// to the delegate; the `approval_request` arm shows a `UIAlertController` and
// replies with a `Directive::Approve`.
//
// Wire framing + `kind` tags are identical to the sidecar's — see
// `crates/oneai-bus/src/protocol.rs` and `examples/native/macos/OneAIBusClient.swift`.
//
// SOURCE ONLY — built inside the iOS app target on a Mac with Xcode + the
// rust `aarch64-apple-ios` target (see `scripts/build_apple.sh`). The 3
// symbols are declared in the bridging header `platforms/apple/headers/oneaiFFI.h`
// (add them there when wiring this in).

import Foundation
import UIKit

/// One yield the engine produced, decoded enough to dispatch on `kind`.
public struct BusYield {
    public let kind: String
    public let json: [String: Any]
}

public protocol OneAIBusPumpDelegate: AnyObject {
    /// Called for every yield off the pump. Special-case `approval_request`:
    /// present UI and call `pump.respondToApproval(...)`; everything else
    /// renders into the chat surface (route by `speaker` for group chat).
    func busPump(_ pump: OneAIBusPump, didReceive yield: BusYield)
    /// The engine was shut down (no further yields will arrive).
    func busPumpDidShutdown(_ pump: OneAIBusPump)
}

/// In-process engine bus driver — 3 C symbols over the `liboneai` cdylib.
public final class OneAIBusPump {
    public weak var delegate: OneAIBusPumpDelegate?

    /// Dedicated serial queue — the poll buffer is thread-local, so all
    /// `oneai_poll_yield` calls must come from the same thread.
    private let pollQueue = DispatchQueue(label: "oneai.bus.pump.poll")
    private var timer: DispatchSourceTimer?
    private var started = false

    public init() {}

    // MARK: - Lifecycle

    /// Submit a `Directive::Init { config }` to build the engine + bus + pump.
    /// Call once at app launch (after the bridging header is imported). Returns
    /// the submit status (0 = ok; see c_facade.rs for the non-zero codes).
    @discardableResult
    public func initialize(config: [String: Any]) -> Int32 {
        let payload: [String: Any] = ["kind": "init", "config": config]
        return submitDirective(payload)
    }

    /// Start the 20fps poll loop. Idempotent.
    public func start() {
        guard !started else { return }
        started = true
        let timer = DispatchSource.makeTimerSource(queue: pollQueue)
        // 50ms ≈ 20fps — matches the macOS streaming coalescer cadence.
        timer.schedule(deadline: .now(), repeating: .milliseconds(50))
        timer.setEventHandler { [weak self] in self?.drain() }
        timer.resume()
        self.timer = timer
    }

    /// Shut the engine down — submits `Directive::Shutdown`, aborts the pump,
    /// stops polling. The delegate receives `busPumpDidShutdown`.
    public func shutdown() {
        let _ = oneai_shutdown()
        timer?.cancel()
        timer = nil
        started = false
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.delegate?.busPumpDidShutdown(self)
        }
    }

    // MARK: - Sending directives

    /// Send a `Directive` (full JSON object incl. `kind`). Returns the submit
    /// status (0 = ok).
    @discardableResult
    public func submitDirective(_ payload: [String: Any]) -> Int32 {
        guard let data = try? JSONSerialization.data(withJSONObject: payload),
              let line = String(data: data, encoding: .utf8)
        else { return 2 }
        // The C symbol takes a NUL-terminated C string; Swift bridges `String`
        // to `UnsafePointer<CChar>` via the bridging-header decl.
        return oneai_submit_directive(line)
    }

    /// Start (or continue) a single-agent turn.
    public func sendUserMessage(_ text: String) {
        submitDirective([
            "kind": "user_message",
            "content": [["type": "text", "text": text]],
        ])
    }

    /// Cooperatively interrupt the in-flight turn (single-agent or group round).
    public func sendInterrupt(reason: String) {
        submitDirective([
            "kind": "interrupt",
            "reason": ["Custom": ["reason": reason]],
        ])
    }

    /// Reply to an `approval_request` yield. `proceed == true` sends
    /// `InteractionResponse::Proceed` (a bare `"Proceed"` JSON string —
    /// oneai-core's externally-tagged serde enum).
    public func respondToApproval(requestId: String, proceed: Bool) {
        let response: Any = proceed ? "Proceed" : ["Abort": ["reason": "user denied"]]
        submitDirective([
            "kind": "approve",
            "request_id": requestId,
            "response": response,
        ])
    }

    // MARK: - Poll loop

    /// Drain every available yield off the bus (non-blocking). Called on the
    /// poll queue. The pointer from `oneai_poll_yield` is valid only until the
    /// next call on the same thread — `String(cString:)` copies it immediately.
    private func drain() {
        while true {
            guard let ptr = oneai_poll_yield() else { return } // no yield pending
            guard let line = String(cString: ptr, encoding: .utf8) else { continue }
            guard let data = line.data(using: .utf8),
                  let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                  let kind = obj["kind"] as? String
            else { continue }
            let yield = BusYield(kind: kind, json: obj)
            // Route to the UI thread; the poll queue must NOT block on UI work.
            DispatchQueue.main.async { [weak self] in
                guard let self else { return }
                self.handle(yield: yield)
            }
        }
    }

    /// UI-thread dispatch. The `approval_request` arm shows native UI and
    /// replies; everything else forwards to the delegate.
    private func handle(yield: BusYield) {
        if yield.kind == "approval_request",
           let req = yield.json["request"] as? [String: Any],
           let requestId = yield.json["request_id"] as? String
        {
            presentApproval(requestId: requestId, request: req)
            return
        }
        delegate?.busPump(self, didReceive: yield)
    }

    /// Present a `UIAlertController` for a tool-approval request and reply.
    /// Wire into the active `UIViewController` in a real app (the skeleton
    /// uses `UIApplication`'s key window — replace with your presenter).
    private func presentApproval(requestId: String, request: [String: Any]) {
        let tool = (request["tool_name"] as? String) ?? "tool"
        let alert = UIAlertController(
            title: "Approve tool?",
            message: "\(tool)\n\(request["args"] ?? "")",
            preferredStyle: .alert
        )
        alert.addAction(.init(title: "Approve", style: .default) { [weak self] _ in
            self?.respondToApproval(requestId: requestId, proceed: true)
        })
        alert.addAction(.init(title: "Deny", style: .cancel) { [weak self] _ in
            self?.respondToApproval(requestId: requestId, proceed: false)
        })
        UIApplication.shared.keyWindow?.rootViewController?.present(alert, animated: true)
    }
}
