# OneAI Windows app (WinUI 3 / C#)

Native C# chat app — the Windows port of `platforms/android` / `platforms/macos`.
Consumes the Rust `oneai-uniffi` core through a **hand-rolled `extern "C"` JSON
facade** (`crates/oneai-uniffi/src/c_facade.rs`) because `uniffi-bindgen` 0.32
has no C# generator. P/Invokes `oneai.dll`; all strings cross as UTF-8 so CJK
(thinking text, answers) round-trips correctly.

## Build (on a Windows machine with Visual Studio + WindowsAppSDK workload)

```powershell
# 1. Build the native oneai.dll (cdylib — exports both the uniffi symbols AND
#    the c_facade extern "C" symbols). Stages it at platforms/windows/native/.
pwsh ./scripts/build_windows.ps1
# (requires: rustup target add x86_64-pc-windows-msvc)

# 2. Build the app in Visual Studio (open OneAI.sln) or via dotnet:
dotnet build platforms\windows\OneAI.sln -c Debug
# oneai.dll is copied to the output dir by the csproj (../native/oneai.dll).

# 3. Run (unpackaged) — Visual Studio F5, or:
dotnet run --project platforms\windows\OneAI\OneAI.csproj -c Debug
```

## Architecture

```
OneAI/                      # the WinUI 3 app (net8.0-windows10.0.19041, unpackaged)
  Native/
    OneAiNative.cs          # P/Invoke of oneai.dll (UTF-8 string marshalling) — single +
                            # group-chat sessions
    Models.cs              # ChatEvent(+speaker) / SessionInfo / ChatMessage(+speaker) /
                            # ProviderConfig / AgentSpecDto / ScenarioSpecDto / ReviewLoopSpecDto DTOs
  ViewModels/
    ChatModels.cs          # ObservableObject base + UserItem / AssistantItem(+speaker meta,
                            # streaming cap) / ToolStep + ColorUtil
    ChatViewModel.cs       # App/session/group lifecycle; StreamCoalescer (20fps hot-event
                            # batching to avoid UI-thread flooding); speaker-routed Handle
    ScenarioModels.cs      # TurnPolicy / Agent / TopicField / DebriefConfig /
                            # ReviewLoopConfig / Scenario.SpecDto (per-member visibility →
                            # background fold) — port of macOS Models.swift
  Services/
    ProviderStore.cs       # JSON-file persistence (%LOCALAPPDATA%\OneAI) + provider presets + db path
    AppPaths.cs             # %LOCALAPPDATA%\OneAI dir + provider/db/scenarios paths (unpackaged-safe)
    MarkdownHelper.cs      # splitMarkdown (code/heading/blockquote/ordered+unordered list/
                            # table) + inline bold/code — port of macOS Markdown.swift
    ScenarioStore.cs       # JSON persistence (schema-versioned) + 5 built-in presets
                            # (面试演练/语言伙伴/辩论/写作工坊/头脑风暴) + SpeakerMeta
    ArtifactStore.cs       # shared canvas state (long code → docked tab)
  Views/
    ChatView.xaml(.cs)     # top bar (scenario/debrief/tokens) + turn-status bar + first-run
                            # hint + message list (user/assistant with speaker header + accent
                            # bar) + inline topic-intake page + artifact canvas split +
                            # streaming plain-text(cap)+caret vs markdown-on-done + input bar
                            # (mic placeholder / send / stop)
    MainWindow.xaml(.cs)   # custom sidebar (scenarios section + recent sessions + new menu +
                            # settings) + Ctrl+K command palette
    ScenarioEditor.xaml(.cs)  # ContentDialog: cast + topic fields (per-member visibility) +
                            # debrief + turn policy + opener
    CommandPalette.xaml(.cs)  # Ctrl+K fuzzy switch (scenarios/sessions/actions)
    ArtifactCanvas.xaml(.cs)  # tab bar + copy/export + monospace content
    MarkdownTextBlock.xaml(.cs)  # rebuilds blocks from MarkdownHelper.Split
    SettingsDialog.xaml(.cs)
    ChatTemplateSelector.cs
  OneAI.csproj / App.xaml(.cs) / app.manifest
native/oneai.dll           # staged by build_windows.ps1 (gitignored)
```

The `extern "C"` contract is documented in `bindings/c/oneai_c.h` (now incl. group-chat
entry points + scenario JSON shape). JSON event shapes match `ChatEvent` in `Models.cs`.

## Feature parity (with macOS — full design port)

- **Sidebar**: scenarios section (5 built-in presets + user-edited, tap→start, edit/delete)
  + recent sessions (new/switch/delete-confirm), new-conversation menu (single Agent /
  from scenario).
- **Scenario system** (multi-agent group chat via the C facade group entry points):
  - 5 presets: 面试演练 / 语言伙伴 / 辩论赛 / 写作工坊 / 头脑风暴 — system prompts,
    topic fields, debrief, review loop ported verbatim from macOS AgentStore.swift.
  - Turn policies: scripted / round-robin / moderator.
  - Inline **topic-intake page**: collected values baked into each member's system prompt
    as background (per-member `visibleTo` — e.g. interviewee projects → coach only) and
    into the session title.
  - **Debrief phase** ("结束面试"): switches turn policy to the debrief member only,
    sends the summary prompt.
  - **Review-revise loop** (writing workshop): writer→editor→… until "定稿" or max rounds.
  - Speaker-routed bubbles: avatar + name + role pill + left accent bar in speaker color;
    turn-status bar shows who's speaking / "轮到你".
  - **ScenarioEditor** ContentDialog: full cast/topic/debrief/policy/opener editing.
- **Command palette** (Ctrl+K): fuzzy-filtered scenario/session/action switch.
- **Artifact canvas**: long code (>600 chars) or "在画布打开" promotes to a docked tab
  (copy/export); short code stays inline.
- **Streaming hardening** (the macOS beachball root-cause, ported): `StreamCoalescer`
  batches hot events (StreamChunk/Thinking) to ~20fps `DispatcherQueue.TryEnqueue` so
  per-token dispatch doesn't flood the UI thread; non-hot events flush immediately;
  during streaming the partial text renders as plain Text capped to 1800 chars + steady
  caret (NOT re-parsed markdown per token); full markdown renders once on `.done`.
- Markdown (code fence + copy + open-in-canvas, heading, blockquote, ordered/unordered
  list, table, inline `code`/`**bold**`), thinking Expander (collapsible, "思考中…" →
  "已深度思考"), tool steps (`✓/✗/⚙ name(args)` + truncated result), retry-on-error,
  copy/share context menu, dark theme (follows system), first-run hint, stop button →
  `oneai_group_interrupt` / `oneai_session_interrupt`.
- Provider settings (openai/anthropic/ollama presets) persisted as a JSON file;
  SQLite db and the scenarios file all live under `%LOCALAPPDATA%\OneAI\`
  (`provider.json`, `oneai.db`, `oneai_scenarios.json`). The app is unpackaged, so
  `Windows.Storage.ApplicationData` (needs package identity) is NOT used.
- **Voice dictation deferred** — the input bar has a disabled mic placeholder button
  (WinRT speech recognition needs package identity in unpackaged WinUI 3; tracked as a
  follow-up).

## Caveats

- `build_windows.ps1` must run on Windows (MSVC) — this Mac has no Windows
  toolchain. The Rust c_facade itself is unit-tested on macOS (`cargo test -p
  oneai-uniffi c_facade`) and its symbols are confirmed exported from the
  cdylib, so the interop surface is verified; only the C#/XAML build is not.
- `Microsoft.WindowsAppSDK` version pin (`1.5.*`) — adjust to whatever is
  installed in your workload.
- Share uses `DataTransferManager`; in unpackaged mode it works but the share
  UI requires a registered window — if it misbehaves, copy still works.
