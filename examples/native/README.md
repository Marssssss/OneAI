# Native bus clients — `oneai serve` frontends

These are the macOS Swift and Windows C# socket clients that consume the
engine bus over IPC, replacing the extern-C FFI bindings. **Source only** —
they are built inside the native app projects on a macOS / Windows machine,
not in this repo's CI.

See `docs/` and `crates/oneai-bus/src/protocol.rs` for the canonical
`Directive` / `EngineYield` `kind` tags and wire framing (one newline-terminated
JSON object per message).

## macOS — `macos/OneAIBusClient.swift`

- `Network.framework` `NWConnection` to the UDS at `~/.oneai/serve.sock`
  (default; override with `oneai serve --socket <path>`).
- `OneAIBusClientDelegate.didReceive` gets every `EngineYield`; the
  `approval_request` arm should present an `NSAlert` and call
  `respondToApproval(requestId:proceed:)`.
- Copy into the SwiftUI app target; the chat view model implements the
  delegate and renders off the yields.

## Windows — `windows/OneAIBusClient.cs`

- `NamedPipeClientStream` to `\\.\pipe\oneai-serve` (start the sidecar with
  `oneai serve --socket oneai-serve` so the path flattens to that pipe name —
  `oneai_supervisor::transport::to_pipe_name`).
- `OnYield` event for every `EngineYield`; the `approval_request` arm shows a
  `ContentDialog` and calls `RespondToApprovalAsync(requestId, proceed)`.
- Copy into the WinUI3 app project.

## FFI switchover (do on your machine — needs native builds)

1. Start `oneai serve` (with a provider env configured).
2. Build the native app against the client above; verify a real turn
   (`UserMessage` → `StreamChunk`… → `TurnComplete`) and an approval
   roundtrip (`ApprovalRequest` → `Approve` → turn resumes) over the socket.
3. Once the socket path works end-to-end:
   - **macOS**: delete `crates/oneai-platform-desktop/src/macos.rs` +
     `bridge_common.rs` approval glue; `NSAlert` now lives in Swift.
   - **Windows**: collapse `crates/oneai-uniffi/src/c_facade.rs` to the
     mobile-only 3-symbol form (`oneai_submit_directive` /
     `oneai_poll_yield` / `oneai_shutdown`) — see the bus plan's Phase 4.
4. `cargo build --workspace` + `cargo test --workspace` must stay green after
   the deletions (the desktop demo / platform crates lose their FFI surface).

The Rust side of P3 (the sidecar `oneai serve` + `oneai-bus` wire bridge) is
done in-repo and tested; only the native FFI deletion is deferred to a machine
that can build Swift / C#.
