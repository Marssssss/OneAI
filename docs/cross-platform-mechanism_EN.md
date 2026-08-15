# OneAI Cross-Platform Mechanism

> One Rust core (`oneai-core`+`oneai-app`) reaches six native-app targets via **two frontend-connection models**: ① **in-process FFI** (UniFFI bindings for Kotlin/Swift on Android·Apple, a hand-written `extern "C"` JSON facade for Windows C#·HarmonyOS ArkTS — because `uniffi-bindgen` 0.32 has no C#/ArkTS generator); ② **out-of-process app-server sidecar** (desktop/IDE/web frontends reach the engine via [`oneai-app-server`](app-server-mechanism_EN.md)'s JSON-RPC 2.0 protocol + auto-spawn, without embedding the Rust lib). All strings cross the boundary as UTF-8, CJK round-trips correctly.

## 1. Overview (what it is)

OneAI's cross-platform story is not a WebView shell — it runs the same Rust engine logic as **native apps** on macOS/Windows/Linux/Android/iOS/HarmonyOS. There are **two connection models** for a frontend to reach the engine, dispatched by "can/should the frontend spawn the engine process":

- **in-process FFI** (default; the only option on mobile): the foreign-language UI process directly embeds the Rust static/dynamic lib and calls `OneAIApp` via UniFFI or the extern C facade. Mobile has no on-device spawn and no cloud-engine fallback, so it **must** use this path. On desktop, FFI is the verified default transport.
- **out-of-process app-server sidecar** (desktop/IDE/web): the frontend does not embed the Rust lib; instead it (or its host) auto-spawns a `oneai app-server` subprocess and talks JSON-RPC 2.0 (stdio/ipc/ws/native-messaging). The VS Code extension, browser extension, and macOS/Windows desktop sidecar use this — the Codex-style "a frontend that can spawn owns the spawn", so the user never starts a server manually. See [App-Server mechanism](app-server-mechanism_EN.md).

Behind both models lies the **same `oneai-bus` protocol** (`Directive`/`EngineYield`): in-process holds `Arc<InProcessBus>` directly (zero serialization); the app-server adapts JSON-RPC method/event to the bus's Directive/EngineYield (L2 adapter). So approval correlation (`request_id`), interrupt (`CancellationToken`), and the group-chat `speaker` tag behave identically across both models.

`oneai-uniffi` is the FFI binding layer exposing `oneai-app`'s `AppBuilder`/`App`/`AppSession` to foreign languages; `oneai-platform-{desktop,android,ios,harmony}` each provide a native `InteractionGate` (NSAlert/MessageBox/AlertDialog/UIController/CommonDialog); `oneai-staticlib` is a thin staticlib packaging crate producing `liboneai.a` for the Apple xcframework and HarmonyOS NAPI to link.

The binding strategy is two-layer: the UniFFI generator covers Kotlin/Swift (Android, Apple), a hand-written `extern "C"` JSON facade covers C# (Windows P/Invoke) and ArkTS (HarmonyOS NAPI). Both FFI paths converge on the same `OneAIApp` Rust entry. One key FFI discipline: passing a String over extern C must use `CString::new().as_ptr()` (NUL-terminated); `String::as_ptr` is not NUL-terminated and causes `CStr::from_ptr` out-of-bounds UB — macOS happens to pass, Linux CI crashes.

> This page focuses on the **in-process FFI** connection model (§2-§9). The out-of-process app-server sidecar model is in [App-Server mechanism](app-server-mechanism_EN.md) (four-frontend access status, Codex-style auto-spawn, JSON-RPC schema tables, per-frontend real-machine test status).

## 2. Responsibilities & capabilities (what it does)

**UniFFI bindings (Kotlin/Swift).** View types (`RiskLevelView`/`ApprovalRequestView`/`ChatEventView`, etc.) use UniFFI derive macros; traits are Rust-only (foreign code uses concrete impls); factory methods build pre-configured instances; `AppBuilderWrapper`/`OneAIApp` provide idiomatic foreign-language APIs.

**Hand-written extern C JSON facade.** `#[no_mangle] extern "C"` symbols (`oneai_create_app`/`oneai_free_app`/`oneai_create_session`/`oneai_list_conversations`/…) + `AppHandle`/`SessionHandle` opaque handles + `EventCb` callback. Header `bindings/c/oneai_c.h`. All data crosses as UTF-8 JSON, CJK round-trips correctly.

**Native InteractionGate.** `PlatformInteractionGate` per-target impl: macOS `MacOSInteractionGate` (NSAlert), Windows `WindowsInteractionGate` (MessageBox/AlertDialog), Linux `LinuxCliInteractionGate` (stdin/stdout), Android `AndroidInteractionGate` (AlertDialog, JNI bridge), iOS `IOSInteractionGate` (UIController, callback bridge), HarmonyOS `HarmonyInteractionGate` (CommonDialog, callback bridge).

**Staticlib packaging.** `oneai-staticlib` produces `liboneai.a` (~900MB archive), crate-type=staticlib, excluded from `default-members`, built explicitly only when packaging a native lib (`scripts/build_apple.sh`, `build_harmony.sh`).

**Six-target shared design.** scenario-based multi-agent group chat (5 built-in presets), streaming 20fps coalesced rendering (`StreamCoalescer`), Markdown rendering, dark-mode following system, command palette, artifact canvas. The macOS app is the reference impl; other targets mirror it.

**Explicitly does not**: no per-target UI-framework implementation (that's each `platforms/*/` project); no WebView shell; no single all-platform binary (each target builds independently); staticlib not in default-members (doesn't pollute daily `cargo build`).

## 3. Design motivation (why this way)

| Decision | Rationale | Rejected alternative |
|---|---|---|
| Two FFI paths, not UniFFI only | `uniffi-bindgen` 0.32 has no C#/ArkTS generator; Windows (C#) and HarmonyOS (ArkTS) must go through a hand-written extern C facade; UniFFI covers what it can (Kotlin/Swift) | All extern C → Kotlin/Swift lose idiomatic APIs, more hand-writing |
| View types + derive macros, not exposing internal types directly | UniFFI doesn't support exposing trait objects (`dyn LlmProvider`) directly; View types are flat DTOs, derive macros generate bindings, traits stay Rust-only | Expose trait objects directly → UniFFI fails to compile |
| Factory methods build pre-configured instances | Foreign code can't impl Rust traits but can call factory methods to get pre-configured concrete instances (e.g. default_tools, provider_config) | Require foreign code to impl traits → impossible |
| extern C uses JSON across the boundary, not raw structs | C-ABI struct layout is error-prone cross-platform/cross-compiler (padding/alignment/ABI); a JSON string is simple, reliable, version-tolerant, debuggable | Raw structs → ABI fragility, cross-compiler drift |
| UTF-8 + CJK correct round-trip | Chinese users are the default; JSON UTF-8 is the safe boundary; must ensure `CString` NUL-termination and correct decoding on the foreign side | System-locale encoding → CJK garbled |
| `CString::new().as_ptr()`, not `String::as_ptr` | Passing a String over extern C must be NUL-terminated, or `CStr::from_ptr` goes out of bounds; macOS happens to pass, Linux CI crashes (verified `create_app_with_mock_provider_in_env`) | `String::as_ptr` → out-of-bounds UB, CI crashes |
| `oneai-staticlib` a separate crate + excluded from default-members | The staticlib produces a ~900MB archive that should not pollute daily `cargo build`/`cargo test`; a separate crate means it's built explicitly only when packaging a native lib | Put the staticlib crate-type in uniffi → every build emits 900MB |
| Per-target native Gate, not a unified callback bridge | Each target's UI framework differs (NSAlert vs AlertDialog vs CommonDialog), a native Gate per target is most idiomatic; a unified bridge would lose native experience | Unified callback bridge → non-idiomatic UI, downgraded experience |
| `self: Arc<Self>` methods consuming the handle must接住 the return value | UniFFI 0.32 builder methods consume self and return Arc<Self>; if the foreign side doesn't接住 the return value, build has no provider → runTask reports "No LLM provider configured" (a verified pitfall) | `&self` immutable builder → UniFFI 0.32 doesn't support |

## 4. Architecture & core abstractions

```mermaid
flowchart TB
    subgraph FE2 ["🖥️ out-of-process frontends (desktop/IDE/web)"]
        direction LR
        Sidecar["VS Code ext / browser ext / macOS·Windows desktop sidecar<br/>auto-spawn `oneai app-server` (Codex-style)"]
    end
    AppServer["🧾 oneai-app-server · JSON-RPC 2.0 (L2)<br/>method/event ↔ Directive/EngineYield<br/>stdio / ipc / ws / native-messaging"]

    Native["macOS SwiftUI / Windows WinUI3 C# / Linux / Android Compose / iOS SwiftUI / HarmonyOS ArkUI"]
    Uni["UniFFI bindings (Kotlin/Swift)<br/>View types + AppBuilderWrapper + OneAIApp"]
    CFacade["extern C JSON facade<br/>#[no_mangle] oneai_* + AppHandle/SessionHandle + EventCb"]
    Bus["🚌 oneai-bus · Directive/EngineYield (in-process or oneai serve sidecar)"]
    App["oneai-app OneAIApp (unified Rust entry)"]
    Engine["oneai-agent AgentLoop + GroupChat + feature crates"]
    Gates["native PlatformInteractionGate<br/>NSAlert/MessageBox/AlertDialog/UIController/CommonDialog"]

    Native --> Uni
    Native --> CFacade
    Uni --> App
    CFacade --> App
    App --> Bus
    Sidecar -->|JSON-RPC| AppServer
    AppServer -->|Directive/EngineYield| Bus
    Bus --> Engine
    App --> Engine
    Native --> Gates
    Gates -. ToolApproval decision point .-> App
```

**Core abstractions (c_facade):**

```rust
pub type AppHandle = *mut c_void;
pub type SessionHandle = *mut c_void;
pub type EventCb = extern "C" fn(ctx: *mut c_void, event_json: *const c_char);

#[no_mangle]
pub extern "C" fn oneai_create_app(config_json: *const c_char) -> AppHandle;
#[no_mangle]
pub extern "C" fn oneai_create_session(h: AppHandle, id: *const c_char) -> SessionHandle;
#[no_mangle]
pub extern "C" fn oneai_list_conversations(h: AppHandle) -> *mut c_char;   // UTF-8 JSON
// header bindings/c/oneai_c.h
```

**Platform Gate (trait in core):**

```rust
pub trait PlatformInteractionGate: InteractionGate { /* native UI dialogs */ }
// per-target: MacOSInteractionGate / WindowsInteractionGate / LinuxCliInteractionGate
//             AndroidInteractionGate / IOSInteractionGate / HarmonyInteractionGate
```

## 5. Flows it participates in

**Native app launch:**

1. The foreign side (SwiftUI/Compose/C#/ArkUI) starts, calls `oneai_create_app(config_json)` (or UniFFI `AppBuilderWrapper`) to build the Rust `OneAIApp`, gets `AppHandle`.
2. `oneai_create_session(handle, id)` builds an `AppSession`, gets `SessionHandle`.
3. Registers an `EventCb` callback; the Rust side streams tokens as UTF-8 JSON to the foreign side for rendering (`StreamCoalescer` 20fps coalesced, anti-flooding the main queue).
4. On high-risk tool execution the Rust side calls each target's native `PlatformInteractionGate` (NSAlert/AlertDialog…), awaiting Proceed/Abort.

**Build packaging:**

1. `./scripts/build_apple.sh` produces macOS `.dylib` + iOS xcframework (links `liboneai.a` staticlib).
2. `./scripts/build_windows.ps1` produces `oneai.dll` (C# P/Invoke facade).
3. `./scripts/build_android.sh` cross-compiles 4 ABIs + `generate_bindings.sh` emits Kotlin bindings.
4. `./scripts/build_harmony.sh` produces a NAPI module (C++ wrapping the facade).
5. The staticlib is built explicitly only when packaging (`cargo build -p oneai-staticlib`), not in daily builds.

**macOS streaming 20fps coalescing**: per-token DispatchQueue.main.async floods the main queue; `StreamCallback` coalesces hot fragments 20fps flush, non-hot immediate in-order (see the stream mechanism).

### App-Server sidecar turn (desktop/IDE/web frontends)

Frontends that don't embed the Rust lib use this path — the frontend (or its host) auto-spawns a `oneai app-server --listen <transport>` subprocess and talks JSON-RPC 2.0 to the engine:

1. **spawn**: the VS Code extension spawns `oneai app-server --listen stdio` on activation; the browser spawns it via the registered native-messaging host on demand; the macOS/Windows desktop `EngineProcessManager` spawns `--listen ipc://<ephemeral>`. The user **never starts a server manually** (Codex-style).
2. **turn/run**: the frontend sends `turn/run {content}`; the adapter submits `Directive::UserMessage` and the engine starts the turn — `turn/run` returns `turn_id` at TurnStart (non-blocking until turn end).
3. **event stream**: each engine observer callback is translated by `BusObserver` into an `EngineYield`; the adapter broadcasts a single `event` notification (`params` = the full yield, with a `kind` tag: `stream_chunk`/`thinking`/`tool_calls`/`tool_result`/`speaker_turn`/…). The frontend renders by `params.kind`.
4. **approval loop**: on an `event` with `approval_request` (carrying `request_id`), the frontend shows a native dialog and replies `approval/respond {request_id, response}` — the same pair of bus channels as the in-process `BusInteractionGate`, identical behavior.
5. **turn end**: the frontend finishes on the `turn_complete` `event`.

The full JSON-RPC method table, `event` yield variants, four-frontend access status, and honest "awaiting real-machine test" markers are in [App-Server mechanism](app-server-mechanism_EN.md) (§4 schema, §7 frontend-access table, §11 auto-spawn).

## 6. Dependencies

| Direction | Who | What |
|---|---|---|
| Upstream | `oneai-app` | `AppBuilder`/`App`/`AppSession` (the bound core entry) |
| Upstream | `oneai-agent`/`oneai-memory`/`oneai-persistence`/`oneai-core`/`oneai-parser` | re-exports the whole engine |
| Upstream | `uniffi`/`tokio`/`serde_json` | binding generation, async runtime, JSON across the boundary |
| Downstream | each `platforms/*/` project | native UI (SwiftUI/WinUI3/Compose/SwiftUI/ArkUI) |
| Cross-cutting | `oneai-platform-*` | native `PlatformInteractionGate` impls per target |
| Cross-cutting | scripts | `scripts/build_{apple,windows.ps1,android,harmony}.sh` + `generate_bindings.sh` |

## 7. Key types & files

| Item | Location |
|---|---|
| `AppBuilderWrapper` (self:Arc<Self> chain) | `crates/oneai-uniffi/src/app_builder.rs:43` (`provider_config:115`/`default_tools:82`/`sqlite_persistence_at:226`) |
| `OneAIApp` + `AppSession` wrapper | `crates/oneai-uniffi/src/app.rs` |
| GroupChat FFI | `crates/oneai-uniffi/src/group_chat.rs` |
| `ChatEventCallback` + `ChatEventView` | `crates/oneai-uniffi/src/callback.rs:46` |
| extern C facade (`oneai_*` symbols + `AppHandle`/`SessionHandle`/`EventCb`) | `crates/oneai-uniffi/src/c_facade.rs:364,433,449,469` (`EventCb:332`) |
| C header | `bindings/c/oneai_c.h` |
| View types | `crates/oneai-uniffi/src/types.rs` |
| Desktop gates (macOS NSAlert / Windows MessageBox / Linux CLI) | `crates/oneai-platform-desktop/src/{macos,windows,linux,bridge_common}.rs` |
| Android JNI bridge + gate | `crates/oneai-platform-android/src/{jni_bridge,gate}.rs` |
| iOS callback bridge + gate | `crates/oneai-platform-ios/src/{callback_bridge,gate}.rs` |
| HarmonyOS callback bridge + gate | `crates/oneai-platform-harmony/src/{callback_bridge,gate}.rs` |
| `oneai-staticlib` (produces `liboneai.a`) | `crates/oneai-staticlib/src/lib.rs` (crate-type=staticlib, excluded from default-members) |

## 8. Industry comparison

| System | Model | OneAI's trade-off |
|---|---|---|
| **React Native / Flutter** | JS/Dart cross-platform UI, one binary | OneAI is a Rust core + per-target native UI (SwiftUI/WinUI3/Compose…), not a unified cross-platform UI — preserves native experience, shares the engine |
| **UniFFI standard usage** | one generator covers all target languages | OneAI's two paths: UniFFI covers Kotlin/Swift, a hand-written extern C facade covers C#/ArkTS (0.32 has no generator), pragmatic |
| **Rust + JNI (Android)** | hand-written JNI bindings | OneAI's Android uses UniFFI Kotlin bindings + a JNI bridge for the Gate, less effort than pure hand-written JNI |
| **Tauri / Electron** | WebView shell + JS | OneAI explicitly avoids shells; native UI + native Gate dialogs, more native performance and experience |
| **Mozilla application-services** | Rust + UniFFI multi-target SDK | OneAI is the same idea (UniFFI exposes Rust to multiple targets), plus a hand-written extern C facade for languages UniFFI doesn't support |

OneAI's distinct points: **Rust core + per-target native UI (no shell)** + **two FFI paths pragmatic coverage** (UniFFI where it works, hand-written facade where it doesn't) + **staticlib isolates the 900MB archive from daily builds**.

## 9. Extension points & config

- **Add a target**: impl `PlatformInteractionGate` (native dialogs) + expose `OneAIApp` via UniFFI or the extern C facade.
- **Build an App (foreign)**: UniFFI `AppBuilderWrapper` chain (note `self:Arc<Self>` methods must接住 the return value); or extern C `oneai_create_app(config_json)`.
- **Streaming callback**: register an `EventCb`; Rust pushes UTF-8 JSON; `StreamCoalescer` 20fps coalesces anti-flooding.
- **Packaging**: `scripts/build_{apple,windows.ps1,android,harmony}.sh` + `generate_bindings.sh {swift\|kotlin\|...}`.
- **Staticlib**: `cargo build -p oneai-staticlib` (packaging only).
- **Per-target README**: `platforms/{macos,windows,android,harmony}/README.md`.
- **macOS needs a release build**: debug is 5-10× slower; streaming and scrolling need release (see Issue #11).

## 10. Further reading

- [permission-mechanism](permission-mechanism_EN.md) — `PlatformInteractionGate` and the 7 decision points
- [multi-agent-mechanism](multi-agent-mechanism_EN.md) — GroupChat FFI + scenario multi-role chat
- [bus-mechanism](bus-mechanism_EN.md) — unified engine↔frontend protocol (the shared foundation of both connection models)
- [app-server-mechanism](app-server-mechanism_EN.md) — the out-of-process app-server sidecar connection model: JSON-RPC schema, four-frontend access status, auto-spawn
- [memory-mechanism](memory-mechanism_EN.md) — `sqlite_persistence_at` cross-restart memory
- [CLAUDE.md — Cross-platform / Network proxy](../CLAUDE.md)
- Source: `crates/oneai-uniffi/src/` (8 files / ~3.9K LOC) + `crates/oneai-platform-{desktop,android,ios,harmony}/src/` + `crates/oneai-staticlib/` + `crates/oneai-app-server/src/`
- Per-target projects: `platforms/{macos,windows,android,harmony,vscode,browser,npm}/`
