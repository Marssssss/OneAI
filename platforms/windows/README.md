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
    OneAiNative.cs          # P/Invoke of oneai.dll (UTF-8 string marshalling)
    Models.cs              # ChatEvent / SessionInfo / ChatMessage / ProviderConfig DTOs (System.Text.Json)
  ViewModels/
    ChatModels.cs          # ObservableObject base + UserItem / AssistantItem / ToolStep
    ChatViewModel.cs       # App/session/run lifecycle; callback marshalled to UI via DispatcherQueue
  Services/
    ProviderStore.cs       # LocalSettings persistence + provider presets + db path
    MarkdownHelper.cs      # splitMarkdown / inline bold+code (no deps)
  Views/
    ChatView.xaml(.cs)     # message list (user/assistant templates) + input bar + first-run hint
    MainWindow.xaml(.cs)   # NavigationView sidebar (sessions) + ChatView
    SettingsDialog.xaml(.cs)
    MarkdownTextBlock.xaml(.cs)  # rebuilds Inlines from MarkdownHelper
    ChatTemplateSelector.cs
  OneAI.csproj / App.xaml(.cs) / app.manifest
native/oneai.dll           # staged by build_windows.ps1 (gitignored)
```

The `extern "C"` contract is documented in `bindings/c/oneai_c.h`. JSON event
shapes match `ChatEvent` in `Models.cs`.

## Feature parity (with Android S5 / macOS)

Sidebar session list (new/switch/delete-confirm), streaming chat (callback fires
on a tokio worker thread → `DispatcherQueue.TryEnqueue` to UI), thinking
Expander (collapsible, "思考中…" → "已深度思考"), tool steps (`✓/✗/⚙ name(args)`
+ truncated result), markdown (fenced code + inline `code`/`**bold**`/bullets),
blinking cursor, retry-on-error, copy/share context menu, dark theme (follows
system), first-run hint, stop button → `oneai_session_interrupt`. Provider
settings persisted in `ApplicationData.Current.LocalSettings`; SQLite db at
`ApplicationData.Current.LocalFolder/oneai.db`.

## Caveats

- `build_windows.ps1` must run on Windows (MSVC) — this Mac has no Windows
  toolchain. The Rust c_facade itself is unit-tested on macOS (`cargo test -p
  oneai-uniffi c_facade`) and its symbols are confirmed exported from the
  cdylib, so the interop surface is verified; only the C#/XAML build is not.
- `Microsoft.WindowsAppSDK` version pin (`1.5.*`) — adjust to whatever is
  installed in your workload.
- Share uses `DataTransferManager`; in unpackaged mode it works but the share
  UI requires a registered window — if it misbehaves, copy still works.
