# OneAI Android

Android app shell that loads `liboneai.so` and drives the full UniFFI
inference loop — a Jetpack Compose chat screen (S3) built on top of the
FFI surface staged in S2.

## Build

The native `.so` and the UniFFI-generated Kotlin bindings are **not**
committed — `scripts/build_android.sh` cross-compiles and stages them:

```bash
# from repo root
./scripts/build_android.sh            # 4 ABIs, release
cd platforms/android
./gradlew assembleDebug               # produces app/build/outputs/apk/debug/app-debug.apk
./gradlew installDebug                # install on a connected device/emulator
```

Prerequisites (one-time):
- Android NDK — `brew install --cask android-ndk` (or Android Studio SDK Manager)
- `cargo install cargo-ndk`
- `rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android i686-linux-android`

## S3 — Compose chat screen (豆包-style)

`MainActivity` (`app/src/main/kotlin/ai/oneai/MainActivity.kt`) is a single
Compose screen wired end-to-end through the FFI:

- **Input bar** → `session.runTask(task, callback)` (suspend, main-dispatcher
  `rememberCoroutineScope`). While running, the send button becomes a **停止**
  button → `session.interrupt()`.
- **`ChatEventCallback`** receives `ChatEventView` on the tokio worker thread;
  every state mutation marshalled to main via `runOnUiThread`:
  - `StreamChunk` → append to the live answer (typewriter + blinking cursor).
  - `Thinking` → **accumulated into one collapsible "思考过程" card** (not one
    bubble per chunk — `on_thinking` fires per token like `on_stream_chunk`).
    Live "思考中…" while streaming, auto-collapses to "已深度思考 ▾" once the
    answer begins; tap to expand.
  - `ToolCall` / `ToolResult` → compact dim step lines (matched by call id).
  - `DirectAnswer` / `Complete` → finalize, stop cursor.
  - `Error` → inline error under the turn.
- **Scrolling**: a sentinel item + `derivedStateOf(atBottom)` (from
  `canScrollForward`). While at bottom, content chunks auto-stick to the
  bottom; once the user scrolls up, auto-scroll stops and a "回到底部" FAB
  appears. Scrolling back to the bottom resumes follow.
- **Markdown** (lightweight, no deps): fenced ```code blocks``` (monospace +
  card bg, horizontal-scroll), inline `` `code` ``, `**bold**`, bullet list
  prefix. No full markdown renderer by choice (zero dep / zero version risk).
- **Provider config** (⚙ icon): `kind` / `model` / `apiKey` / `baseUrl`
  → `OneAiAppBuilder().providerConfig(ProviderConfigView(...))` (the returned
  builder is reused — `provider_config` consumes the Arc). No secrets baked
  in; session built lazily on first send and reused. Defaults
  `openai` / `gpt-4o-mini`.
  - Ollama on the host from the emulator: `kind=ollama`, `model=llama3`,
    `base url=http://10.0.2.2:11434`.

> `AndroidManifest` sets `windowSoftInputMode="adjustResize"`; the activity
> calls `enableEdgeToEdge()` so Compose insets drive the layout.

> Earlier milestones: S1/S2 proved `.so` load + FFI surface callability
> (`OneAiAppBuilder().build()` → `createSession()` → `sessionId()` /
> `platform()`). See the S2 smoke commit for the prior TextView activity.

> JNA (`net.java.dev:jna`) is resolved from Maven Central; ensure your
> network/proxy can reach `repo.maven.apache.org`. Compose deps (BOM
> 2024.02.02 + material3) resolve from `google()` / `mavenCentral()`.

