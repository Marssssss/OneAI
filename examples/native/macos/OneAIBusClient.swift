// OneAIBusClient.swift
// macOS native frontend for `oneai serve` (the engine-bus sidecar).
//
// A Directive writer + Yield reader over a Unix domain socket. This replaces
// the extern-C FFI binding (crates/oneai-platform-desktop/macos.rs +
// bridge_common.rs) — see the P3 plan's "Deferred" section: build the Swift
// app against this client, verify a real turn + approval roundtrip, then
// delete the FFI glue.
//
// Wire framing: one newline-terminated JSON object per message, same as
// oneai-bus's `serialize_yield` / `serialize_directive`. See
// crates/oneai-bus/src/protocol.rs for the canonical `kind` tags.
//
// This is a skeleton — connect/send/receive/approval loop only. Wire it into
// the SwiftUI app's view model; the app renders off the yields it receives.

import Foundation
import Network

/// One yield the engine sent, decoded enough to dispatch on `kind`.
public struct EngineYield {
    public let kind: String
    public let json: [String: Any]
}

public protocol OneAIBusClientDelegate: AnyObject {
    /// Called for every yield. Special-case `approval_request` — the delegate
    /// should present UI and call `client.respondToApproval(...)`; everything
    /// else renders into the chat surface.
    func busClient(_ client: OneAIBusClient, didReceive yield: EngineYield)
    /// Called when the socket closed or failed.
    func busClient(_ client: OneAIBusClient, didCloseWithError error: Error?)
}

/// Network.framework connection to `oneai serve` over a Unix domain socket.
public final class OneAIBusClient {
    private let socketPath: String
    private var connection: NWConnection?
    public weak var delegate: OneAIBusClientDelegate?

    /// - Parameter socketPath: filesystem path to the UDS (default
    ///   `~/.oneai/serve.sock`). Pass the same value as `oneai serve --socket`.
    public init(socketPath: String = "~/.oneai/serve.sock") {
        let expanded = socketPath.withCString { strlen($0) > 0 && socketPath.hasPrefix("~")
            ? NSString(string: socketPath).expandingTildeInPath
            : socketPath }
        self.socketPath = expanded
    }

    public func connect() {
        let endpoint = NWEndpoint.unix(path: socketPath)
        let parameters = NWParameters.tcp
        // NWParameters default works for UDS; .unix path is set on the endpoint.
        let conn = NWConnection(to: endpoint, using: parameters)
        self.connection = conn
        conn.stateUpdateHandler = { [weak self] state in
            switch state {
            case .ready:
                self?.startReceiveLoop()
            case .failed(let err):
                self?.delegate?.busClient(self!, didCloseWithError: err)
            case .cancelled:
                self?.delegate?.busClient(self!, didCloseWithError: nil)
            default:
                break
            }
        }
        conn.start(queue: .global(qos: .userInitiated))
    }

    public func disconnect() {
        connection?.cancel()
    }

    // MARK: - Sending directives

    /// Send a `Directive` as one JSON line. `payload` is the full object
    /// including its `kind` field, e.g. `{"kind": "user_message", "content": [{"type":"text","text":"hi"}]}`.
    public func sendDirective(_ payload: [String: Any]) {
        guard let data = try? JSONSerialization.data(withJSONObject: payload),
              var line = String(data: data, encoding: .utf8) else { return }
        if !line.hasSuffix("\n") { line.append("\n") }
        guard let bytes = line.data(using: .utf8) else { return }
        connection?.send(content: bytes, completion: .contentProcessed { err in
            if let err = err { print("[OneAIBusClient] send failed: \(err)") }
        })
    }

    /// Convenience: start a turn.
    public func sendUserMessage(_ text: String) {
        sendDirective([
            "kind": "user_message",
            "content": [["type": "text", "text": text]],
        ])
    }

    /// Convenience: interrupt the in-flight turn.
    public func sendInterrupt(reason: String) {
        sendDirective([
            "kind": "interrupt",
            "reason": ["Custom": ["reason": reason]],
        ])
    }

    /// Reply to an `approval_request` yield. `proceed == true` sends
    /// `InteractionResponse::Proceed` (a bare `"Proceed"` JSON string — see
    /// oneai-core's externally-tagged enum).
    public func respondToApproval(requestId: String, proceed: Bool) {
        let response: Any
        if proceed {
            // Unit variant → bare string "Proceed" (externally-tagged serde).
            response = "Proceed"
        } else {
            response = ["Abort": ["reason": "user denied"]]
        }
        sendDirective([
            "kind": "approve",
            "request_id": requestId,
            "response": response,
        ])
    }

    // MARK: - Receiving yields

    private var lineBuffer = Data()

    private func startReceiveLoop() {
        connection?.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) {
            [weak self] data, _, _, error in
            guard let self = self else { return }
            if let data = data, !data.isEmpty {
                self.lineBuffer.append(data)
                self.drainCompleteLines()
            }
            if let error = error {
                self.delegate?.busClient(self, didCloseWithError: error)
                return
            }
            // Keep reading until the connection closes.
            self.startReceiveLoop()
        }
    }

    private func drainCompleteLines() {
        while let nl = lineBuffer.firstIndex(of: 0x0A) {
            var lineData = lineBuffer.prefix(upTo: nl)
            // Strip trailing \r if present.
            if lineData.last == 0x0D { lineData = lineData.dropLast() }
            lineBuffer.removeSubrange(lineBuffer.startIndex...nl)
            guard let obj = try? JSONSerialization.jsonObject(with: lineData) as? [String: Any],
                  let kind = obj["kind"] as? String else { continue }
            let yield = EngineYield(kind: kind, json: obj)
            DispatchQueue.main.async { [weak self] in
                guard let self = self else { return }
                self.delegate?.busClient(self, didReceive: yield)
            }
        }
    }
}
