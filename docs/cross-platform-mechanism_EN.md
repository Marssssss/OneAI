# OneAI Cross-Platform Mechanism

> One Rust core (`oneai-core` + `oneai-app`), two FFI paths, six native apps.

## Responsibility

Run the same engine logic as **native apps** on macOS / Windows / Linux / Android / iOS / HarmonyOS — not WebView shells. Two FFI paths:

- **UniFFI bindings** (Kotlin / Swift / Python) — Android, Apple platforms.
- **Hand-written `extern "C"` JSON facade** (`crates/oneai-uniffi/src/c_facade.rs`, header `bindings/c/oneai_c.h`) — because `uniffi-bindgen` 0.32 has no C#/ArkTS generator, Windows (C# P/Invoke `oneai.dll`) and HarmonyOS (NAPI-wrapped) reuse this facade. All strings cross the boundary as UTF-8; CJK round-trips correctly.

> Passing a String over extern C requires `CString::new().as_ptr()` (NUL-terminated); `String::as_ptr` is not NUL-terminated and causes CStr out-of-bounds UB.

## Six targets at a glance

| Platform | Stack | Binding language | Native InteractionGate |
|---|---|---|---|
| macOS | SwiftUI (`swiftc`, no Xcode needed) | Swift (UniFFI) | NSAlert |
| Windows | WinUI 3 / C# | C# (P/Invoke facade) | MessageBox |
| Linux | desktop platform crate | C++ (facade) | MessageBox |
| Android | Jetpack Compose / Kotlin | Kotlin (UniFFI) | AlertDialog |
| iOS | SwiftUI / Swift | Swift (UniFFI xcframework) | UIAlertController |
| HarmonyOS | ArkTS / ArkUI + NAPI | C++ (NAPI-wrapped facade) | CommonDialog |

Every target shares the same design: scenario-based multi-agent group chat (5 built-in presets), 20fps streaming coalesce-render, markdown, system-following dark mode, command palette, artifact canvas. **The macOS app is the reference implementation; other targets mirror it.**

## Build scripts

| Platform | Script |
|---|---|
| Apple (macOS + iOS xcframework) | `./scripts/build_apple.sh`, `./platforms/macos/build_macos.sh` |
| Windows | `./scripts/build_windows.ps1` |
| Android (4 ABI) | `./scripts/build_android.sh` |
| HarmonyOS | `./scripts/build_harmony.sh` |
| Binding generation | `./scripts/generate_bindings.sh {swift|...}` |

Per-platform build steps in `platforms/{macos,windows,android,harmony}/README.md`.

## Key types & files

| Item | Location |
|---|---|
| UniFFI binding defs + extern C facade | `crates/oneai-uniffi/src/{c_facade,app_builder,app,group_chat,callback,types}.rs` |
| C header | `bindings/c/oneai_c.h` |
| Platform gate adapters | `crates/oneai-platform-{desktop,android,ios,harmony}/src/` |
| Desktop platform bridge (macOS/Win/Linux) | `crates/oneai-platform-desktop/src/{macos,windows,linux,bridge_common}.rs` |
| Android JNI bridge | `crates/oneai-platform-android/src/{jni_bridge,gate}.rs` |

## Further reading

- [CLAUDE.md — Cross-platform / Network proxy](../CLAUDE.md)
- Native UI per platform in each `platforms/*/` project
- Permission gate — see [Permission mechanism](permission-mechanism_EN.md)
