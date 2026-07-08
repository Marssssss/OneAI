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
  → `OneAiAppBuilder().providerConfig(ProviderConfigView(...)).defaultTools()`
  (the returned builder is reused — `provider_config`/`default_tools` consume
  the Arc; `extra_tools` survives the chain via `from_builder`). **Persisted**
  in `SharedPreferences("oneai_provider")` — survives app restarts. No secrets
  baked in; session built lazily on first send and reused. Defaults
  `openai` / `gpt-4o-mini`.
  - Ollama on the host from the emulator: `kind=ollama`, `model=llama3`,
    `base url=http://10.0.2.2:11434`.
- **Web search**: `defaultTools()` registers the built-in `web_search`
  (DuckDuckGo backend, no API key) + `web_fetch` read-only research tools on
  the Rust side, so the model can search without wiring a DomainPack.

> `AndroidManifest` sets `windowSoftInputMode="adjustResize"`; the activity
> calls `enableEdgeToEdge()` so Compose insets drive the layout.

## S4 — Multi-session + persistence + settings

S3's single-session screen becomes a real app:

- **Persistence**: the App is built with `sqlite_persistence_at(<filesDir>/oneai.db)`.
  `run_task` auto-saves the conversation after every turn (`AppSession::run_agent`
  → `save_session`), so chats survive process restart. No explicit save call
  from Kotlin.
- **Multi-session drawer** (`ModalNavigationDrawer`, ☰ in the top bar):
  `OneAiApp.listConversations()` → `SessionInfoView` list (newest-first),
  each row showing message count + relative time. Tap → `createSessionWithId(id)`
  + `session.messages()` replays user/assistant turns as finalized bubbles.
  🗑 → `deleteConversation(id)`. 「新对话」 → `createSession()` (fresh uuid).
- **Settings sheet** (`ModalBottomSheet`, ⚙ or drawer 设置 row): kind/model/
  apiKey/baseUrl. 保存 → `rebuildApp()` drops the App and rebuilds it with the
  new config (same db path → history preserved), then re-resumes the most
  recent session. Provider config still persisted in `SharedPreferences`.
- **Startup**: `ChatViewModel.ensureApp()` builds the App eagerly on first
  frame, loads the session list, and resumes the most recent conversation (or
  starts a fresh one).

The FFI surface added for S4 (`crates/oneai-uniffi`): `OneAIAppBuilder::
sqlite_persistence_at`, `OneAIApp::{create_session_with_id, list_conversations,
delete_conversation}`, `OneAISession::messages`, plus `SessionInfoView` /
`MessageView` records. Backed by `oneai-app`'s `App::create_session_with_id` /
`list_conversations` / `delete_conversation` (which thin-wrap `SqliteSessionStore`)
and `AppSession::new_with_conversation` (resume constructor).

> Earlier milestones: S1/S2 proved `.so` load + FFI surface callability
> (`OneAiAppBuilder().build()` → `createSession()` → `sessionId()` /
> `platform()`). See the S2 smoke commit for the prior TextView activity.

> JNA (`net.java.dev:jna`) is resolved from Maven Central; ensure your
> network/proxy can reach `repo.maven.apache.org`. Compose deps (BOM
> 2024.02.02 + material3) resolve from `google()` / `mavenCentral()`.

