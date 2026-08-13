// OneAiRpcClient.swift — macOS JSON-RPC 2.0 client for `oneai app-server`
// (the sidecar transport). Replaces the old newline-JSON `OneAIBusClient`
// skeleton (which spoke the raw bus protocol to `oneai serve`) with the
// operation-oriented JSON-RPC frontend schema over a Unix domain socket.
//
// The macOS app's default transport stays in-process FFI (c_facade) — best
// UX (no process/socket). This client is the out-of-process option selected
// by the `oneai_engine_transport=sidecar` UserDefaults flag: the app spawns
// `oneai app-server --listen ipc://<ephemeral>` (see EngineProcessManager) on
// launch and routes turns/approvals/group/scenario through this client.
//
// Framing: newline-terminated JSON (matches `serve_ipc` / `serve_stdio`).
// The JSON-RPC envelope mirrors crates/oneai-app-server/src/protocol.rs
// (id: number|string on requests, no id on the `event` notification).
//
// NOT yet wired into ChatViewModel's group/scenario run path — see the
// Phase D status in the plan. The infra is complete + compilable; the live
// rewiring (and macOS runtime verification) is the remaining focused step.

import Foundation
import Network

/// One JSON-RPC `event` notification the engine emitted, decoded enough to
/// dispatch on `kind`. `params` is the full EngineYield (with its `kind` tag).
public struct OneAiEvent {
    public let kind: String
    public let params: [String: Any]
}

public protocol OneAiRpcClientDelegate: AnyObject {
    /// Called for every `event` notification. Switch on `event.kind`
    /// (turn_start / stream_chunk / thinking / tool_calls / tool_result /
    /// delegate / delegate_complete / speaker_turn / paradigm_switch /
    /// approval_request / working_state / context_accounting / plan_update /
    /// tools_added / init_result / compact_result / token_usage / error /
    /// turn_complete / iteration_start / session_* / …). Unknown kinds arrive
    /// as the bus adds yield variants (`#[non_exhaustive]`) — ignore them.
    func rpcClient(_ client: OneAiRpcClient, didReceive event: OneAiEvent)
    /// Called when the socket closed or failed. The EngineProcessManager
    /// restarts the child; the VM surfaces a transport error to the user.
    func rpcClient(_ client: OneAiRpcClient, didCloseWithError error: Error?)
}

/// A JSON-RPC 2.0 client over a Unix domain socket (the app-server ipc
/// transport). Promise-based `call(method, params)` (id correlation) + a
/// delegate for `event` notifications. Thread-safe: the pending-continuation
/// map + id counter are lock-protected; events are delivered on
/// `callbackQueue`. `@unchecked Sendable` — the lock guards all shared state
/// (NWConnection is itself thread-safe; `rxBuf` is only touched from the
/// serialized receive callback).
public final class OneAiRpcClient: @unchecked Sendable {
    private let socketPath: String
    private var connection: NWConnection?
    public weak var delegate: OneAiRpcClientDelegate?
    /// Queue the delegate callbacks fire on. Default a background queue; the
    /// VM supplies `.main` so `@Published` mutations are already on the main
    /// actor (mirrors the FFI callback path's main-thread marshalling).
    public var callbackQueue: DispatchQueue = .global(qos: .userInitiated)

    private var nextId: Int = 1
    private var pending: [Int: CheckedContinuation<[String: Any], Error>] = [:]
    private let pendingLock = NSLock()
    private var rxBuf = Data()
    private var connected = false

    public init(socketPath: String) {
        self.socketPath = socketPath
    }

    /// Connect to the app-server's UDS. Resolves once the socket is ready.
    public func connect() async throws {
        let endpoint = NWEndpoint.unix(path: socketPath)
        let conn = NWConnection(to: endpoint, using: .tcp)
        self.connection = conn
        try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Void, Error>) in
            conn.stateUpdateHandler = { [weak self] state in
                switch state {
                case .ready:
                    self?.connected = true
                    self?.startReceiveLoop()
                    cont.resume()
                case .failed(let err):
                    cont.resume(throwing: err)
                    self?.delegate?.rpcClient(self!, didCloseWithError: err)
                case .cancelled:
                    self?.delegate?.rpcClient(self!, didCloseWithError: nil)
                default:
                    break
                }
            }
            conn.start(queue: .global(qos: .userInitiated))
        }
    }

    public func disconnect() {
        connected = false
        connection?.cancel()
        connection = nil
        // Reject anything still pending — the responses will never arrive.
        pendingLock.lock()
        let doomed = pending
        pending.removeAll()
        pendingLock.unlock()
        for (_, c) in doomed {
            c.resume(throwing: OneAiRpcError.disconnected)
        }
    }

    /// Send a JSON-RPC request; resolves with the `result` object. The id is
    /// auto-assigned. For `turn/run` the result is `{turn_id}` (resolved at
    /// TurnStart); for ack methods `{ok:true}`; `scenario/validate` returns
    /// `{ok, errors}`. Errors throw `OneAiRpcError.server(code, message)`.
    public func call(_ method: String, params: Any?) async throws -> [String: Any] {
        guard connected, let conn = connection else { throw OneAiRpcError.disconnected }
        // Assign the id + register the continuation under one lock so two
        // concurrent calls can't collide on the id counter.
        let id: Int = {
            pendingLock.lock()
            defer { pendingLock.unlock() }
            let n = nextId
            nextId += 1
            return n
        }()
        var payload: [String: Any] = ["jsonrpc": "2.0", "id": id, "method": method]
        if let params = params { payload["params"] = params }
        let data = try JSONSerialization.data(withJSONObject: payload)
        var line = String(data: data, encoding: .utf8) ?? "{}"
        if !line.hasSuffix("\n") { line.append("\n") }

        return try await withCheckedThrowingContinuation { (cont: CheckedContinuation<[String: Any], Error>) in
            self.pendingLock.lock()
            self.pending[id] = cont
            self.pendingLock.unlock()
            guard let bytes = line.data(using: .utf8) else {
                self.resolve(id, throwing: OneAiRpcError.encode)
                return
            }
            conn.send(content: bytes, completion: .contentProcessed { [weak self] err in
                if let err = err {
                    self?.resolve(id, throwing: err)
                }
            })
        }
    }

    // MARK: - Receive loop

    private func startReceiveLoop() {
        receiveChunk()
    }

    private func receiveChunk() {
        guard let conn = connection else { return }
        // UDS stream — read up to 64KiB at a time, accumulate, split on `\n`.
        conn.receive(minimumIncompleteLength: 1, maximumLength: 65_536) { [weak self] data, _, isComplete, error in
            guard let self = self else { return }
            if let error = error {
                self.handleClose(error)
                return
            }
            if let data = data {
                self.rxBuf.append(data)
                self.drainLines()
            }
            if isComplete {
                self.handleClose(nil)
                return
            }
            self.receiveChunk()
        }
    }

    private func drainLines() {
        while let nl = rxBuf.firstIndex(of: 0x0A) {
            let lineData = rxBuf.subdata(in: 0..<nl)
            rxBuf.removeSubrange(0...nl)
            guard !lineData.isEmpty,
                  let obj = try? JSONSerialization.jsonObject(with: lineData) as? [String: Any]
            else { continue }
            handleMessage(obj)
        }
    }

    private func handleMessage(_ msg: [String: Any]) {
        // Response to a pending call (has id + result/error).
        if let id = msg["id"], msg["result"] != nil || msg["error"] != nil {
            // id may arrive as Int (JSON-RPC number) or NSNumber; normalize.
            let key = (id as? Int) ?? Int((id as? NSNumber)?.intValue ?? -1)
            if key >= 0 {
                if let err = msg["error"] as? [String: Any] {
                    let code = (err["code"] as? Int) ?? -1
                    let message = (err["message"] as? String) ?? "rpc error"
                    resolve(key, throwing: OneAiRpcError.server(code: code, message: message))
                } else if let result = msg["result"] as? [String: Any] {
                    resolve(key, with: result)
                } else {
                    // result is not an object (e.g. a bare string) — wrap it.
                    resolve(key, with: ["value": msg["result"] ?? NSNull()])
                }
            }
            return
        }
        // Notification (no id). The app-server's single outbound method is
        // `event`; params is the full EngineYield with its `kind` tag.
        if msg["method"] as? String == "event", let params = msg["params"] as? [String: Any] {
            let kind = (params["kind"] as? String) ?? ""
            let ev = OneAiEvent(kind: kind, params: params)
            callbackQueue.async { [weak self] in
                guard let self = self else { return }
                self.delegate?.rpcClient(self, didReceive: ev)
            }
        }
    }

    private func resolve(_ id: Int, with result: [String: Any]) {
        pendingLock.lock()
        let cont = pending.removeValue(forKey: id)
        pendingLock.unlock()
        cont?.resume(returning: result)
    }

    private func resolve(_ id: Int, throwing error: Error) {
        pendingLock.lock()
        let cont = pending.removeValue(forKey: id)
        pendingLock.unlock()
        cont?.resume(throwing: error)
    }

    private func handleClose(_ error: Error?) {
        connected = false
        pendingLock.lock()
        let doomed = pending
        pending.removeAll()
        pendingLock.unlock()
        for (_, c) in doomed { c.resume(throwing: OneAiRpcError.disconnected) }
        callbackQueue.async { [weak self] in
            guard let self = self else { return }
            self.delegate?.rpcClient(self, didCloseWithError: error)
        }
    }
}

public enum OneAiRpcError: Error {
    case disconnected
    case encode
    case server(code: Int, message: String)
}
