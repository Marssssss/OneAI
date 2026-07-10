# OneAI macOS app

A native SwiftUI chat app — the macOS port of `platforms/android` (S5).
Consumes the Rust `oneai-uniffi` core via the raw UniFFI Swift binding
(`bindings/swift/OneAI.swift`) + the universal macOS staticlib
(`platforms/apple/lib/liboneai.a`).

**Builds without Xcode** — only the Command Line Tools + the rust
`aarch64-apple-darwin`/`x86_64-apple-darwin` targets. The app is compiled
directly with `swiftc` (no `.xcodeproj`) by `build_macos.sh`, mirroring how
`build_android.sh` drives the Android build.

## Build & run

```bash
# 1. Stage the universal macOS liboneai.a + (for Xcode) the xcframework
./scripts/build_apple.sh

# 2. Build the app
./platforms/macos/build_macos.sh            # release → build/OneAI.app
./platforms/macos/build_macos.sh --debug

# 3. Run
open platforms/macos/build/OneAI.app
```

## What it does (feature parity with Android S5)

- Provider settings (openai / anthropic / ollama presets) persisted in
  `UserDefaults` (suite `oneai_provider`); save → rebuild app, history kept.
- Multi-session via SQLite (`~/Library/Application Support/oneai.db`): sidebar
  lists conversations (newest-first), new / switch / delete (with confirm).
- Streaming chat: `session.runTask(task, callback)` — a Swift
  `ChatEventCallback` whose `onEvent` fires on the tokio worker thread and
  marshals to the main thread via `DispatchQueue.main.async`. Renders thinking
  card (collapsible, "思考中…" + dots → "已深度思考"), tool-call steps
  (`✓/✗/⚙ name(args)` + truncated result), lightweight markdown (fenced code +
  inline `` `code` `` / `**bold**` + bullets), blinking cursor while streaming,
  retry-on-error, copy (NSPasteboard) / share (NSSharingServicePicker).
- Dark theme follows the system (adaptive `Theme` palette).
- First-run hint when an API key is missing; stop button → `session.interrupt()`.

## Source map

```
Sources/
  OneAIApp.swift        @main App + adaptive Theme palette
  ChatViewModel.swift   VM + models (UserItem/AssistantItem/ToolStep) + StreamCallback
  Markdown.swift        splitMarkdown / buildInline (no deps)
  Errors.swift          OneAiErrorView → Chinese hint (friendlyError)
  Views.swift           ChatScreen, Sidebar, ChatDetail, bubbles, settings, input, cursors
Info.plist
build_macos.sh         swiftc driver → OneAI.app
```

## Caveats

- iOS (`platforms/ios`) needs Xcode (iphoneos SDK + simulator) — install Xcode
  and re-run `./scripts/build_apple.sh` to also produce `OneAI.xcframework`.
- The `ld: ... built for newer 'macOS' version (26.2)` warnings are benign
  (ring/zstd asm from a newer SDK; links and runs fine on macOS 13+).
