# Native bus clients + pumps — engine-bus frontends

Two shapes consume the unified engine bus (`Directive` in / `EngineYield` out):

- **Shape B — socket sidecar** (macOS, Windows): the app is a Directive
  writer + Yield reader over IPC (`oneai serve` UDS / named-pipe). See
  `macos/OneAIBusClient.swift` and `windows/OneAIBusClient.cs`.
- **Shape A — in-process pump** (iOS, Android, HarmonyOS): the app can't run
  a sidecar (sandbox / App Store / NAPI rules), so it links `liboneai` and
  drives the engine through the **3 `extern "C"` symbols** P4 collapsed the
  facade to:
  - `oneai_submit_directive(json) -> i32` — submit a `Directive` (JSON). A
    `Directive::Init { config }` builds the engine + bus + pump on first call.
  - `oneai_poll_yield() -> *const c_char` — the next `EngineYield` as one
    JSON line, or null. The pointer aliases a thread-local buffer — valid
    until the next `poll_yield` on the same thread; the caller must NOT free it.
  - `oneai_shutdown() -> i32` — submit `Directive::Shutdown`, stop the pump.

See `crates/oneai-bus/src/protocol.rs` for the canonical `Directive` /
`EngineYield` `kind` tags + wire framing (one newline-terminated JSON object per
message). `crates/oneai-uniffi/src/c_facade.rs` is the 3-symbol implementation.

## Shape B — socket sidecar (macOS / Windows)

### `macos/OneAIBusClient.swift`
- `Network.framework` `NWConnection` to the UDS at `~/.oneai/serve.sock`
  (default; override with `oneai serve --socket <path>`).
- `OneAIBusClientDelegate.didReceive` gets every `EngineYield`; the
  `approval_request` arm should present an `NSAlert` and call
  `respondToApproval(requestId:proceed:)`.
- Copy into the SwiftUI app target; the chat view model implements the
  delegate and renders off the yields.

### `windows/OneAIBusClient.cs`
- `NamedPipeClientStream` to `\\.\pipe\oneai-serve` (start the sidecar with
  `oneai serve --socket oneai-serve` so the path flattens to that pipe name —
  `oneai_supervisor::transport::to_pipe_name`).
- `OnYield` event for every `EngineYield`; the `approval_request` arm shows a
  `ContentDialog` and calls `RespondToApprovalAsync(requestId, proceed)`.
- Copy into the WinUI3 app project.

## Shape A — in-process pump (iOS / Android / HarmonyOS)

Each pump runs a 20fps poll loop on a **dedicated single thread** (the poll
buffer is thread-local — all `oneai_poll_yield` calls must come from one
thread), routes yields to the UI thread, and handles `approval_request`
natively (`UIAlertController` / `AlertDialog` / ArkTS dialog →
`Directive::Approve`).

### `ios/OneAIBusPump.swift`
- `DispatchSource` timer at 20fps on a serial `DispatchQueue`; `String(cString:)`
  copies each yield before the next poll invalidates the pointer.
- Declare the 3 symbols in the bridging header `platforms/apple/headers/oneaiFFI.h`
  (P4 added them to the cdylib; add the declarations when wiring this in):
  ```c
  int32_t      oneai_submit_directive(const char* json);
  const char*  oneai_poll_yield(void);
  int32_t      oneai_shutdown(void);
  ```
- Copy into the iOS app target; the chat view controller implements
  `OneAIBusPumpDelegate` and renders off the yields (route by `speaker` for
  group-chat turns).

### `android/OneAIBusPump.kt`
- `HandlerThread` + 20fps `Handler` post; the JNI string copy is independent of
  the native buffer, so the next poll is safe.
- `external fun oneai_submit_directive/oneai_poll_yield/oneai_shutdown` — the
  cdylib exports them `#[no_mangle]`; `System.loadLibrary("oneai")` loads them.
- Copy into the Android app module (`platforms/android`); the chat
  Activity/Fragment implements `OneAIBusPumpListener`.

### `harmony/OneAIBusPump.cpp`
- NAPI module exporting `submitDirective`/`pollYield`/`shutdown` to ArkTS, each
  calling the 3 C symbols. ArkTS runs the poll loop on a worker thread.
- Build inside a HarmonyOS native module on a machine with DevEco Studio + the
  rust cross target.

## Build (on your machine — needs native toolchains)

The Rust side (the sidecar `oneai serve` + the 3-symbol `c_facade`) is done
in-repo and tested; only the native builds are deferred to a machine with the
toolchains.

- **macOS / iOS**: `./scripts/build_apple.sh` (rust apple targets; iOS needs Xcode).
- **Android**: `./scripts/build_android.sh` (Android NDK + `cargo-ndk`).
- **Windows**: build the WinUI3 project against `OneAIBusClient.cs` (needs
  Windows + the named-pipe sidecar `oneai serve` running).
- **HarmonyOS**: build the NAPI module against `OneAIBusPump.cpp` (DevEco Studio).

## Verification (on the native machine)

1. Submit `Directive::Init { config }` → engine built. Submit a `UserMessage`
   → poll yields `StreamChunk`…`TurnComplete` on the UI thread.
2. Trigger a tool approval → poll yields `approval_request` → present dialog →
   submit `Directive::Approve` → turn resumes (single-agent OR a group round).
3. Submit `StartGroupChat { scenario }` + `GroupUserMessage` → poll yields
   `SpeakerTurn` + `speaker`-tagged fragment yields per member.
4. `Directive::Interrupt` mid-turn → the round stops (single-agent: bus cancel
   token; group: the pump's `group.interrupt()` path).

The P4 3-symbol collapse + group-chat protocol extension are verified in-repo
(`cargo test -p oneai-uniffi` — incl. the `extern "C" == 3` symbol-count
assertion); native FFI deletion / binding migration is deferred to native builds.
