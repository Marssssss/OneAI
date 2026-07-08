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

## S3 — Compose chat screen

`MainActivity` (`app/src/main/kotlin/ai/oneai/MainActivity.kt`) is a single
Compose screen wired end-to-end through the FFI:

- **Input bar** → `session.runTask(task, callback)` (suspend, driven from a
  `rememberCoroutineScope` on the main dispatcher).
- **`ChatEventCallback`** receives `ChatEventView` events *on the tokio worker
  thread*; every state mutation is marshalled back to main via
  `runOnUiThread` before touching Compose snapshot state:
  - `StreamChunk` → append to the live assistant bubble (typewriter).
  - `Thinking` / `ToolCall` / `ToolResult` → dimmed monospace trace lines.
  - `DirectAnswer` / `Complete` → finalize the bubble, stop the spinner.
  - `Error` → surface the message below the transcript.
- **Provider config** (⚙ icon): `kind` / `model` / `apiKey` / `baseUrl`
  fields feed `OneAiAppBuilder().providerConfig(ProviderConfigView(...))`.
  No secrets are baked into the APK; the session is built lazily on first
  send and reused. Defaults: `openai` / `gpt-4o-mini`.
  - Ollama on the host from the emulator: `kind=ollama`, `model=llama3`,
    `base url=http://10.0.2.2:11434`.

> Earlier milestones: S1/S2 proved `.so` load + FFI surface callability
> (`OneAiAppBuilder().build()` → `createSession()` → `sessionId()` /
> `platform()`). See the S2 smoke commit for the prior TextView activity.

> JNA (`net.java.dev:jna`) is resolved from Maven Central; ensure your
> network/proxy can reach `repo.maven.apache.org`. Compose deps (BOM
> 2024.02.02 + material3) resolve from `google()` / `mavenCentral()`.

