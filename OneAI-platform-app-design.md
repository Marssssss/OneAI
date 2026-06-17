# OneAI Cross-Platform App Design Document

## Context

OneAI is a Rust cross-platform Agent framework with 18 crates covering the full agent stack. Currently only the **TUI** (ratatui + crossterm) on Windows is a complete app. This document defines the full native app design for Android (Compose), iOS (SwiftUI), macOS (SwiftUI), and Linux (GTK4+libadwaita), each implementing the **complete feature set** of the Agent framework as a product-grade app.

**User decisions**: Linux=GTK4+libadwaita, Mobile=Compose+SwiftUI native, Priority=Android+iOS first, Scope=Full feature set per platform.

---

## Shared App Contract — Full Feature Surface

Every platform app implements **all** of these features:

| # | Feature | Source Crate | Description |
|---|---------|-------------|-------------|
| 1 | Chat Interface | oneai-app/agent | Multi-turn conversation with streaming typewriter |
| 2 | Agent Loop Visualization | oneai-agent | Iteration count, paradigm badges, sub-agent delegation |
| 3 | Tool Execution + Display | oneai-tool | Register/call tools, render results as expandable cards |
| 4 | Native Approval Gate | oneai-tool+platform | Platform-native dialog for Full-permission tool approval |
| 5 | Memory Browser | oneai-memory | View/search STM+LTM entries, hybrid scoring |
| 6 | RAG Search | oneai-rag | Document search, context assembly display |
| 7 | Session Management | oneai-app | Create/reset/switch sessions, view history |
| 8 | Provider Configuration | oneai-provider | API key/base URL/model selector (OpenAI, Anthropic, Ollama, DashScope, DeepSeek) |
| 9 | Permission Policy | oneai-core | Approval threshold config (Read/Standard/Full) |
| 10 | Workflow Editor | oneai-workflow | Visual DAG definition + execution + StateGraph |
| 11 | Checkpoint Recovery | oneai-persistence | Save/load/recover from checkpoints |
| 12 | Platform Capabilities | oneai-core/platform_capabilities | Screenshot, camera, filesystem sandbox, notifications, network |
| 13 | Trajectory Viewer | oneai-trace | Agent eval metrics (success rate, cost, latency, span tree) |
| 14 | Skill System | oneai-skill | Progressive disclosure, keyword/vector matching |
| 15 | Domain Pack Selector | (future) | Switch between coding/research/data-analysis domains |

### Shared Rust→Platform Bridge Architecture

Two communication channels between Rust core and platform UI:

1. **UniFFI bindings** (sync) — create app, register tools, execute tools, configure provider, save checkpoint
2. **AgentLoopObserver** (async) — event stream: iteration_start, tool_calls, stream_chunk, paradigm_switch, checkpoint, complete, direct_answer, tool_result, delegate

Every platform app implements:
- **NativeAppShell** — UI that renders ObserverEvent stream
- **NativeApprovalGate** — native dialog: ApprovalRequest → ApprovalResponse
- **NativePlatformCapabilities** — platform-specific screenshot/camera/notification/sandbox

---

## Platform 1: Android — Jetpack Compose

### Technology Stack
| Component | Technology |
|-----------|-----------|
| Rust core | `cargo-ndk --target aarch64-linux-android` (Android NDK r25+) |
| FFI bridge | UniFFI → Kotlin bindings (oneai-uniffi crate) |
| Approval bridge | JNI (oneai-platform-android JniApprovalBridge) |
| Native UI | Kotlin + Jetpack Compose + Material3 |
| Build | Gradle + cargo-ndk, minSdk 26 (Android 8+) |
| Persistence | Room (SQLite) → SqliteCheckpointBackend |
| Background | ForegroundService + WorkManager |
| Camera | CameraX → Image ContentBlock |

### Architecture

```
AndroidApp/
├── app/
│   ├── src/main/
│   │   ├── java/com/oneai/app/
│   │   │   ├── MainActivity.kt                  # Compose entry
│   │   │   ├── OneAIApplication.kt               # App singleton, init Rust core
│   │   │   ├── ui/
│   │   │   │   ├── chat/
│   │   │   │   │   ├── ChatScreen.kt             # Main chat Compose UI
│   │   │   │   │   ├── ChatViewModel.kt          # Holds session, dispatches observer
│   │   │   │   │   ├── MessageBubble.kt          # Role-based styling (emoji prefix+color)
│   │   │   │   │   ├── StreamingText.kt          # Typewriter animation (AnimatedContent)
│   │   │   │   │   ├── ToolResultCard.kt         # Expandable tool result card
│   │   │   │   │   ├── IterationTimeline.kt      # Horizontal iteration+paradigm badges
│   │   │   │   │   ├── SubAgentBadge.kt          # Delegation indicator
│   │   │   │   ├── sidebar/
│   │   │   │   │   ├── DrawerContent.kt          # NavigationDrawer: tools, memory, sessions
│   │   │   │   │   ├── ToolsPanel.kt             # Registered tools list
│   │   │   │   │   ├── MemoryBrowser.kt          # STM+LTM search + entry cards
│   │   │   │   │   ├── SessionsPanel.kt          # Session list + switcher
│   │   │   │   ├── settings/
│   │   │   │   │   ├── SettingsScreen.kt         # Settings navigation host
│   │   │   │   │   ├── ProviderConfigScreen.kt   # API key/base URL/model dropdown
│   │   │   │   │   ├── PermissionPolicyScreen.kt # Approval threshold selector
│   │   │   │   │   ├── DomainPackScreen.kt       # Domain pack selection
│   │   │   │   ├── approval/
│   │   │   │   │   ├── ApprovalDialog.kt         # Material3 AlertDialog for tool approval
│   │   │   │   │   ├── ApprovalActivity.kt       # Full-screen approval for complex ops
│   │   │   │   ├── workflow/
│   │   │   │   │   ├── WorkflowEditorScreen.kt   # Visual DAG editor (Compose Canvas)
│   │   │   │   │   ├── WorkflowResultScreen.kt   # Workflow execution results
│   │   │   │   ├── rag/
│   │   │   │   │   ├── RAGSearchScreen.kt        # Document search UI
│   │   │   │   ├── trace/
│   │   │   │   │   ├── TraceScreen.kt            # Span tree + metrics charts
│   │   │   │   ├── camera/
│   │   │   │   │   ├── CameraCapture.kt          # CameraX → Image ContentBlock
│   │   │   │   ├── checkpoint/
│   │   │   │   │   ├── CheckpointListScreen.kt   # Save/load/recover checkpoints
│   │   │   │   ├── navigation/
│   │   │   │   │   ├── NavGraph.kt               # Compose Navigation graph
│   │   │   │   ├── theme/
│   │   │   │   │   ├── OneAITheme.kt             # Material3 dynamic color theme
│   │   │   │   │   ├── Type.kt                   # Typography
│   │   │   │   ├── components/
│   │   │   │   │   ├── MarkdownRenderer.kt       # Markdown→Compose (commonmark+compose)
│   │   │   │   │   ├── CodeBlockRenderer.kt      # Syntax-highlighted code blocks
│   │   │   │   │   ├── ParadigmBadge.kt          # ReAct/Plan/Reflect/Explore icon badges
│   │   │   │   │   ├── LoadingIndicator.kt       # Thinking spinner animation
│   │   │   ├── bridge/
│   │   │   │   ├── OneAIBridge.kt                # UniFFI-generated Kotlin facade
│   │   │   │   ├── ObserverChannel.kt            # Rust observer → Kotlin Flow<ObserverEvent>
│   │   │   │   ├── ApprovalBridge.kt             # Polls JNI, shows AlertDialog
│   │   │   │   ├── PlatformCapsImpl.kt           # Android PlatformCapabilities
│   │   │   ├── service/
│   │   │   │   ├── AgentForegroundService.kt     # ForegroundService for long-running agents
│   │   │   │   ├── NotificationHelper.kt         # NotificationCompat for completion alerts
│   │   │   ├── worker/
│   │   │   │   ├── ScheduledAgentWorker.kt       # WorkManager for periodic tasks
│   │   │   ├── camera/
│   │   │   │   ├── CameraProvider.kt             # CameraX lifecycle-aware provider
│   │   │   ├── data/
│   │   │   │   ├── AppDatabase.kt                # Room DB
│   │   │   │   ├── SessionDao.kt                 # Session + checkpoint DAO
│   │   │   │   ├── MemoryDao.kt                  # Memory entry DAO
│   │   │   │   ├── SessionRepository.kt          # Repository pattern
│   │   │   ├── util/
│   │   │   │   ├── NetworkMonitor.kt             # ConnectivityManager
│   │   │   │   ├── MarkdownParser.kt             # Commonmark-JVM for rendering
│   │   │   │   ├── FileSandboxHelper.kt          # App private storage paths
│   │   ├── jniLibs/
│   │   │   ├── arm64-v8a/liboneai.so             # Rust cross-compiled
│   │   │   ├── armeabi-v7a/liboneai.so           # (optional, for older devices)
│   │   ├── res/
│   │   │   ├── values/strings.xml
│   │   │   ├── drawable/app_icon.xml
│   │   │   ├── xml/file_paths.xml                # FileProvider for sharing
│   │   ├── AndroidManifest.xml                   # Permissions + service declarations
│   ├── build.gradle.kts                          # cargo-ndk + Compose deps
├── build.gradle.kts (root)                       # Kotlin + Android SDK config
├── local.properties                              # ndk.dir path
├── gradle.properties
```

### UX Design

**Chat Screen** (primary screen, always visible):
- Bottom: input bar with "oneai> " prefix. Send button (▶). "📷" camera button adjacent.
- Above input: scrollable message list. Each message = role-colored bubble with emoji prefix (🤔/🤖/⚡/🔧/✓/──/✗).
- Streaming: text appears character-by-character (AnimatedContent with 30ms delay per chunk).
- Tool calls: render as Material3 Card with 🔧 header, collapsible result content.
- Iteration: thin horizontal timeline at top showing "iter 3 · ReAct" badge. Tap = iteration detail sheet.
- Thinking: top bar shows "⏳ thinking..." with animated spinner when agent is active.

**NavigationDrawer** (pull from left):
- Tools panel: list of registered tools with icons
- Memory panel: search bar + scrollable entry cards (hybrid scoring: relevance + recency)
- Sessions panel: session list with timestamps, tap to switch, long-press to delete
- Settings shortcut

**Approval Flow**:
- Tool requires Full permission → Dialog slides up from bottom (Material3 BottomSheet)
- Shows: tool name, args preview (JSON formatted), justification text
- Three buttons: ✅ Approve, ✗ Deny, ✎ Modify (opens args editor)
- Background execution pauses until response via JNI bridge

**Camera Integration**:
- "📷" button → CameraX launches → user captures frame → PNG bytes → ContentBlock.Image → agent receives visual context
- Or: continuous stream mode (CameraX video → periodic frame extraction → agent gets live visual updates)

**Foreground Service**:
- When agent starts execution → ForegroundService starts with notification "OneAI agent running..."
- Notification bar shows persistent icon. When complete → notification updates to "Tap to view result"
- Service keeps wakelock during execution so agent continues even if app minimized

### AndroidManifest Permissions

```xml
<uses-permission android:name="android.permission.INTERNET" />
<uses-permission android:name="android.permission.FOREGROUND_SERVICE" />
<uses-permission android:name="android.permission.POST_NOTIFICATIONS" />
<uses-permission android:name="android.permission.CAMERA" />
<uses-permission android:name="android.permission.READ_EXTERNAL_STORAGE" />
<uses-permission android:name="android.permission.WRITE_EXTERNAL_STORAGE" />
<uses-permission android:name="android.permission.ACCESS_NETWORK_STATE" />
```

### Rust→Kotlin Bridge Flow

```
ChatViewModel.runAgent(task)
  → session.run_agent(task, observer)    [UniFFI sync call, returns async]
  → observer.on_stream_chunk(text)       [Kotlin Channel → Flow]
  → ChatViewModel receives Flow event
  → updates _messages StateFlow
  → Compose recomposes → streaming text appears

  → observer.on_tool_calls(calls)        [Flow event]
  → adds ToolCallMessage to StateFlow
  → Compose renders tool card

  → Full-permission tool → approval gate
  → JNI bridge polls → ApprovalPendingItem
  → ApprovalBridge shows AlertDialog
  → user taps "Approve" → ApprovalDecision
  → JNI bridge sends response back
  → Rust core continues execution
```

---

## Platform 2: iOS — SwiftUI

### Technology Stack
| Component | Technology |
|-----------|-----------|
| Rust core | `cargo-lipo --targets aarch64-apple-ios,x86_64-apple-ios` (universal) |
| FFI bridge | UniFFI → Swift bindings + C callback bridge (oneai-platform-ios) |
| Native UI | SwiftUI + UIKit (for camera/notifications) |
| Build | Xcode project, Swift Package Manager for UniFFI module |
| Persistence | CoreData (SQLite) → SqliteCheckpointBackend |
| Background | BGTaskScheduler (limited, 30s) + foreground keep-alive |
| Camera | AVCaptureSession → UIImage → PNG → ContentBlock.Image |
| Haptics | UIFeedbackGenerator (tool execution + approval alerts) |

### Architecture

```
iOSApp/
├── OneAI/
│   ├── App/
│   │   ├── OneAIApp.swift                    # SwiftUI App entry
│   │   ├── ContentView.swift                 # Main TabView
│   │   ├── OneAIViewModel.swift              # @Observable global state
│   ├── Views/
│   │   ├── Chat/
│   │   │   ├── ChatView.swift                # ScrollView + TextField
│   │   │   ├── MessageRow.swift              # Per-role styling (emoji+color)
│   │   │   ├── StreamingTextView.swift       # Typewriter (withAnimation)
│   │   │   ├── ToolResultCard.swift          # Expandable card
│   │   │   ├── IterationTimeline.swift       # Iteration + paradigm badges
│   │   │   ├── SubAgentIndicator.swift       # Delegation indicator
│   │   │   ├── ThinkingIndicator.swift       # ProgressView while thinking
│   │   ├── Sidebar/
│   │   │   ├── ToolsList.swift               # Registered tools list
│   │   │   ├── MemoryBrowser.swift           # Search + entry cards
│   │   │   ├── SessionsList.swift            # Session switcher
│   │   ├── Settings/
│   │   │   ├── SettingsView.swift            # Tab-based settings
│   │   │   ├── ProviderConfig.swift          # API key/base URL/model
│   │   │   ├── PermissionPolicy.swift        # Approval threshold
│   │   │   ├── DomainPackSelect.swift        # Domain selection
│   │   ├── Approval/
│   │   │   ├── ApprovalSheet.swift           # Half-sheet with Approve/Deny
│   │   │   ├── ApprovalAlert.swift           # UIAlertController fallback
│   │   ├── Workflow/
│   │   │   ├── WorkflowEditor.swift          # Visual DAG (SwiftUI Canvas)
│   │   │   ├── WorkflowExecution.swift       # Run + results
│   │   ├── RAG/
│   │   │   ├── RAGSearchView.swift           # Document search UI
│   │   ├── Trace/
│   │   │   ├── TraceTreeView.swift           # Span tree + metrics
│   │   ├── Checkpoint/
│   │   │   ├── CheckpointList.swift          # Save/load/recover
│   │   ├── Camera/
│   │   │   ├── CameraCaptureView.swift       # UIImagePickerController
│   │   │   ├── CameraStreamView.swift        # AVCaptureSession live stream
│   │   ├── Components/
│   │   │   ├── MarkdownView.swift            # Markdown→SwiftUI rendering
│   │   │   ├── CodeBlockView.swift           # Syntax highlighting
│   │   │   ├── ParadigmBadge.swift           # ReAct/Plan/Reflect/Explore
│   │   │   ├── NetworkStatusBanner.swift     # Offline/metered warning
│   ├── Bridge/
│   │   ├── OneAIBridge.swift                 # UniFFI-generated Swift module
│   │   ├── ObserverDelegate.swift            # C callbacks → Swift AsyncStream
│   │   ├── ApprovalDelegate.swift            # Callback bridge → SwiftUI sheet
│   │   ├── PlatformCapabilities.swift        # iOS-specific capabilities impl
│   │   ├── ObserverEvent.swift               # Swift enum mirroring Rust events
│   ├── Models/
│   │   ├── ChatMessage.swift                 # Swift model (role, content, timestamp)
│   │   ├── SessionState.swift                # Session wrapper
│   │   ├── ProviderConfig.swift              # Provider config model
│   │   ├── MemoryEntry.swift                 # Memory entry model
│   │   ├── CheckpointInfo.swift              # Checkpoint metadata model
│   ├── Services/
│   │   ├── AgentService.swift                # Background agent execution manager
│   │   ├── NotificationService.swift         # UNUserNotificationCenter wrapper
│   │   ├── NetworkMonitor.swift              # NWPathMonitor connectivity
│   │   ├── HapticService.swift               # UIFeedbackGenerator wrapper
│   │   ├── BackgroundTaskService.swift       # BGTaskScheduler registration
│   ├── Persistence/
│   │   ├── CoreDataStore.swift               # CoreData stack + session storage
│   │   ├── CheckpointStore.swift             # CoreData checkpoint persistence
│   ├── Resources/
│   │   ├── Assets.xcassets                   # App icons + colors
│   │   ├── oneai_bindings/                   # UniFFI-generated Swift modulemap
│   │   ├── Preview Content/
│   ├── Info.plist                            # Permissions + background modes
├── OneAI.xcodeproj
├── OneAI.xcscheme
```

### UX Design

**TabView** (bottom tabs):
- 💬 Chat (primary) — always first tab
- 🧠 Memory — STM+LTM browser
- ⚙️ Settings — provider, permissions, domains
- 📊 Trace — trajectory tree + metrics

**Chat Tab**:
- Bottom: TextField with "oneai>" placeholder + Send button. Camera (📷) button adjacent.
- Above: ScrollView of messages. Each = role-colored row with emoji prefix.
- Streaming: `withAnimation(.easeInOut(duration: 0.03))` per StreamChunk → typewriter effect.
- Tool calls: expandable GroupBox with 🔧 header, disclosure arrow for result content.
- Thinking: ProgressView() spinner at top bar. Paradigm badge alongside.
- Iteration: horizontal HStack of small circles (filled = completed iteration) + paradigm label.

**Approval Sheet**:
- Tool needs Full permission → `.sheet()` presents half-modal
- Content: tool name (bold), args (formatted JSON), justification
- Buttons: "Approve" (green), "Deny" (red), "Modify" (blue, opens args editor TextField)
- Execution pauses via `await approvalGate.requestApproval(request)` (Swift async)

**Camera**:
- "📷" → UIImagePickerController (photo mode) → UIImage → PNG compression → ContentBlock.Image
- Or: CameraStreamView (AVCaptureSession) → periodic frame extraction → live visual context

**Haptics**:
- Tool execution starts → UIImpactFeedbackGenerator (.medium)
- Approval required → UINotificationFeedbackGenerator (.warning)
- Agent completes → UINotificationFeedbackGenerator (.success)

**Dynamic Island** (iPhone 14 Pro+):
- ActivityKit Live Activity showing "Agent · iter 3 · ReAct" during execution
- Updates per iteration → user sees progress even with app minimized
- Completion → "Done" state → tap to return to app

**Background Constraints**:
- iOS kills background tasks after ~30s → use BGTaskScheduler for brief processing
- Primary strategy: keep app foreground during agent execution
- Notifications: UNUserNotificationCenter alerts user to return if they leave

### Info.plist Permissions

```xml
<key>NSCameraUsageDescription</key>
<string>OneAI uses camera to provide visual context to the AI agent</string>
<key>UIBackgroundModes</key>
<array>
  <string>processing</string>
  <string>remote-notification</string>
</array>
```

### Rust→Swift Bridge Flow

```
OneAIViewModel.runAgent(task)
  → session.runAgent(task, observer)       [UniFFI call]
  → C callback: on_stream_chunk(text)      [CallbackApprovalBridge]
  → ObserverDelegate converts to Swift AsyncStream
  → ViewModel receives AsyncStream event
  → updates @Observable messages array
  → SwiftUI recomposes → streaming text

  → C callback: on_tool_calls(calls)
  → adds ToolCallMessage
  → SwiftUI renders expandable GroupBox

  → Full-permission tool → approval gate
  → C callback bridge receives request
  → ApprovalDelegate shows .sheet()
  → user taps "Approve"
  → sends ApprovalResponse via C callback
  → Rust core continues
```

---

## Platform 3: macOS — SwiftUI

### Technology Stack
| Component | Technology |
|-----------|-----------|
| Rust core | `cargo build --target aarch64-apple-darwin` (native) |
| FFI bridge | UniFFI → Swift bindings + NSAlert (oneai-platform-desktop/macos) |
| Native UI | SwiftUI (macOS 13+ Ventura, NavigationSplitView) |
| Build | Xcode project |
| Persistence | SQLite (rusqlite) → SqliteCheckpointBackend |
| Shell | Full ShellTool access (no restriction) |
| Screenshot | CGWindowListCreateImage → PNG → ContentBlock.Image |
| Clipboard | NSPasteboard → ClipboardTool |
| AppleScript | NSAppleScript → AppleScriptTool PlatformTool |
| Daemon | LaunchAgent plist for scheduled tasks |

### Architecture

```
macOSApp/
├── OneAI/
│   ├── App/
│   │   ├── OneAIApp.swift                    # SwiftUI macOS App
│   │   ├── AppDelegate.swift                 # NSApplication delegate
│   ├── Views/
│   │   ├── Main/
│   │   │   ├── MainWindow.swift              # NavigationSplitView (3-column)
│   │   │   ├── SidebarColumn.swift           # Sessions, tools, settings sidebar
│   │   │   ├── ContentColumn.swift           # Chat + agent visualization
│   │   │   ├── DetailColumn.swift            # Tool details, memory, trace, workflow
│   │   ├── Chat/
│   │   │   ├── ChatView.swift                # ScrollView + TextField
│   │   │   ├── MessageRow.swift              # Role-colored message rows
│   │   │   ├── StreamingTextView.swift       # Typewriter animation
│   │   │   ├── ToolResultCard.swift          # Expandable disclosure group
│   │   │   ├── IterationTimeline.swift       # Iteration count + paradigm badges
│   │   ├── Workflow/
│   │   │   ├── WorkflowDAGView.swift         # Visual DAG editor (Canvas)
│   │   │   ├── WorkflowExecutionView.swift   # Run + results display
│   │   ├── Memory/
│   │   │   ├── MemoryBrowserView.swift       # Search + entry list
│   │   ├── Trace/
│   │   │   ├── TraceTreeView.swift           # Tree + metrics charts (Charts framework)
│   │   ├── RAG/
│   │   │   ├── RAGSearchView.swift           # Document search + context display
│   │   ├── Settings/
│   │   │   ├── SettingsWindow.swift          # Preferences window (NSWindow)
│   │   │   ├── ProviderSettings.swift        # Provider config tab
│   │   │   ├── PermissionSettings.swift      # Approval threshold tab
│   │   │   ├── DomainSettings.swift          # Domain pack tab
│   │   ├── Approval/
│   │   │   ├── ApprovalAlert.swift           # NSAlert modal
│   │   ├── Screenshot/
│   │   │   ├── ScreenshotCapture.swift       # CGWindowListCreateImage
│   │   ├── Components/
│   │   │   ├── MarkdownView.swift            # AttributedString rendering
│   │   │   ├── CodeBlockView.swift           # Syntax highlighting
│   │   │   ├── ParadigmBadge.swift
│   ├── Bridge/
│   │   ├── OneAIBridge.swift                 # UniFFI Swift bindings
│   │   ├── MacOSPlatformCaps.swift           # Screenshot, clipboard, sandbox, notifications
│   │   ├── ObserverAdapter.swift             # Channel → Swift AsyncStream
│   │   ├── ApprovalAdapter.swift             # NSAlert integration
│   ├── Services/
│   │   ├── DaemonManager.swift               # LaunchAgent plist creation
│   │   ├── AppleScriptService.swift          # NSAppleScript execution
│   │   ├── ClipboardService.swift            # NSPasteboard read/write
│   │   ├── NotificationService.swift         # NSUserNotificationCenter
│   │   ├── NetworkMonitor.swift              # NWPathMonitor
│   ├── PlatformTools/
│   │   ├── AppleScriptTool.swift             # PlatformTool for macOS automation
│   │   ├── ClipboardTool.swift               # PlatformTool for clipboard
│   │   ├── ScreenshotTool.swift              # PlatformTool for screenshots
│   │   ├── WindowManagerTool.swift           # PlatformTool for window management
│   ├── Persistence/
│   │   ├── SqliteStore.swift                 # SQLite via rusqlite
│   ├── Resources/
│   │   ├── Assets.xcassets
│   │   ├── oneai_bindings/
│   ├── Info.plist
├── OneAI.xcodeproj
```

### UX Design

**3-column NavigationSplitView** (macOS standard):
- Sidebar (left): session list, tools panel, settings shortcuts. Collapsible via Cmd+S.
- Content (center): chat view. Main interaction area.
- Detail (right, optional): trace tree, memory browser, workflow DAG, RAG results.

**Menu Bar**:
- OneAI: About, Preferences (Cmd+,), Quit
- File: New Session (Cmd+N), Save Checkpoint (Cmd+S), Load Checkpoint (Cmd+O)
- Edit: Clear Conversation (Cmd+K), Copy Response
- View: Toggle Sidebar (Cmd+T), Toggle Detail Panel, Show Memory (Cmd+M), Show Trace (Cmd+R)
- Agent: Switch Paradigm → Plan/ReAct/Reflect/Explore, Add Visual Context (Screenshot)
- Domain: Switch Domain → Coding/Research/Data Analysis

**Keyboard Shortcuts**:
- Cmd+Enter: send message
- Cmd+Shift+Enter: newline
- Cmd+K: clear conversation
- Cmd+S: save checkpoint
- Cmd+N: new session
- Cmd+T: toggle sidebar

**Approval Alert**: NSAlert modal (modal panel, not sheet — macOS convention for dangerous operations). Shows tool name, args, justification. Approve/Deny/Modify buttons.

**ShellTool**: Full shell access on macOS. ShellTool executes without restriction. Optional sandbox via macOS Seatbelt profile for coding domain.

**Screenshot**: CGWindowListCreateImage captures entire desktop or specific window → PNG → ContentBlock.Image → agent sees screen context.

**AppleScript**: Custom PlatformTool → agent can control macOS apps (open Safari, run Terminal command, manipulate Finder). Registered as `apple_script` tool.

**Daemon**: LaunchAgent plist in ~/Library/LaunchAgents/ enables scheduled periodic tasks (monitoring, reporting) that survive app closure.

### macOS-specific Capabilities

| Capability | Implementation | Registered as PlatformTool |
|------------|---------------|---------------------------|
| Shell | ShellTool (full access) | `shell` |
| Screenshot | CGWindowListCreateImage | `screenshot` |
| Clipboard | NSPasteboard read/write | `clipboard` |
| AppleScript | NSAppleScript | `apple_script` |
| Filesystem | Project directory sandbox | (FileReadTool, FileEditTool, etc.) |
| Notifications | NSUserNotificationCenter | (built-in) |
| Window management | NSWindow manipulation | `window_manager` |
| Network | NWPathMonitor | (built-in PlatformCapabilities) |

---

## Platform 4: Linux — GTK4 + libadwaita

### Technology Stack
| Component | Technology |
|-----------|-----------|
| Rust core | `cargo build --target x86_64-unknown-linux-gnu` (native) |
| FFI bridge | **None** — pure Rust imports (oneai-* crates directly) |
| Native UI | gtk4-rs + libadwaita-rs (AdwApplication) |
| Build | Meson + cargo (or pure Cargo with build.rs) |
| Persistence | SQLite (rusqlite) → SqliteCheckpointBackend |
| Shell | Full ShellTool access |
| Notifications | libnotify (dbus) |
| Screenshot | scrot / GDK Pixbuf / xdg-screenshot |
| Clipboard | xclip / xsel |
| Background | systemd user service |
| XDG paths | ~/.config/oneai/, ~/.local/share/oneai/, ~/.cache/oneai/ |

### Architecture

```
LinuxApp/
├── src/
│   ├── main.rs                              # AdwApplication entry
│   ├── app.rs                               # AdwApplication + window management
│   ├── config.rs                            # XDG path resolution + config loading
│   ├── ui/
│   │   ├── window.rs                        # AdwApplicationWindow (main window)
│   │   ├── header_bar.rs                    # AdwHeaderBar with buttons
│   │   ├── chat_view.rs                     # ScrolledWindow + input bar
│   │   ├── message_row.rs                   # Per-message ListBoxRow (role+color+emoji)
│   │   ├── streaming_label.rs               # gtk::Label + glib::timeout_add typewriter
│   │   ├── tool_card.rs                     # ExpanderRow for tool results
│   │   ├── iteration_bar.rs                 # Horizontal iteration + paradigm badges
│   │   ├── thinking_spinner.rs              # gtk::Spinner for agent thinking
│   │   ├── sidebar.rs                       # AdwFlap (sidebar panel)
│   │   ├── tools_panel.rs                   # Tools ListBox
│   │   ├── memory_browser.rs                # Search + entry list
│   │   ├── sessions_panel.rs                # Session ListBox + switcher
│   │   ├── settings_view.rs                 # AdwPreferencesDialog
│   │   ├── provider_settings.rs             # Provider config page
│   │   ├── permission_settings.rs           # Approval threshold page
│   │   ├── domain_settings.rs               # Domain pack page
│   │   ├── approval_dialog.rs               # gtk::MessageDialog for approval
│   │   ├── workflow_editor.rs               # gtk::Canvas DAG editor
│   │   ├── trace_viewer.rs                  # gtk::TreeView + metrics
│   │   ├── rag_search.rs                    # Search + results display
│   │   ├── checkpoint_view.rs               # Checkpoint list + recovery
│   │   ├── camera_capture.rs                # Pipewire/v4l2 (optional, feature-gated)
│   ├── bridge/
│   │   ├── observer_adapter.rs              # AgentLoopObserver → glib::idle_add
│   │   ├── linux_platform_caps.rs           # Screenshot, filesystem, notifications
│   │   ├── native_approval_gate.rs          # GTK4 dialog → ApprovalGate impl
│   ├── services/
│   │   ├── notification_service.rs          # libnotify wrapper
│   │   ├── network_monitor.rs               # NetworkManager dbus
│   │   ├── daemon_service.rs                # systemd user service generation
│   │   ├── clipboard_service.rs             # xclip/xsel wrapper
│   ├── platform_tools/
│   │   ├── clipboard_tool.rs                # PlatformTool for clipboard
│   │   ├── screenshot_tool.rs               # PlatformTool for screenshots
│   │   ├── xdg_tool.rs                      # PlatformTool for XDG desktop operations
│   ├── persistence/
│   │   ├── sqlite_store.rs                  # rusqlite implementation
│   ├── theme/
│   │   ├── style.css                        # GTK CSS for custom styling
│   ├── resources/
│   │   ├── icons/                           # SVG app icons
│   │   ├── gresource.xml                    # GTK resource bundle
│   ├── build.rs                             # GTK4 resource compilation
├── Cargo.toml                               # gtk4 + libadwaita + oneai-* deps
├── meson.build                              # (optional) Meson build integration
├── data/
│   ├── oneai.metainfo.xml                   # AppStream metadata
│   ├── oneai.desktop                        # Desktop entry file
```

### UX Design

**AdwApplicationWindow** (GNOME-style):
- AdwHeaderBar: "OneAI" title, thinking indicator, menu button (☰), sidebar toggle
- AdwFlap: foldable sidebar (sessions, tools, memory) on left
- Content area: chat view (vertical layout: messages scroll + input bar at bottom)
- Optional right panel: trace, workflow, RAG

**Chat View**:
- gtk::ScrolledWindow with gtk::ListBox of MessageRow widgets
- Each row: role-colored label with emoji prefix (same visual language as TUI)
- Streaming: gtk::Label updated via `glib::timeout_add_local(30ms, || { label.set_text(...) })`
- Input: gtk::Entry at bottom with "oneai>" placeholder + Send button

**Approval Dialog**: gtk::MessageDialog (modal)
- Shows tool name, args, justification
- "Approve" / "Deny" / "Modify" buttons
- Maps to ApprovalResponse via native_approval_gate impl

**Observer → GTK Bridge** (the key pattern):
```rust
// No FFI! Direct Rust→GTK integration
let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ObserverEvent>();
let observer = GtkObserver::new(tx);

// Spawn agent in background tokio task
tokio::spawn(async move {
    session.run_agent(&task, &observer).await
});

// Feed events into GTK main loop
glib::idle_add_local(move || {
    while let Ok(event) = rx.try_recv() {
        match event {
            ObserverEvent::StreamChunk(text) => {
                streaming_label.append_text(&text);
            }
            ObserverEvent::ToolCalls(calls) => {
                for call in calls {
                    chat_view.add_tool_card(&call.name, &call.args);
                }
            }
            // ... other events
        }
    }
    glib::ControlFlow::Continue
});
```

**XDG Compliance**:
- Config: `~/.config/oneai/config.toml`
- Data: `~/.local/share/oneai/sessions/`, `~/.local/share/oneai/checkpoints/`
- Cache: `~/.cache/oneai/trace/`
- Runtime: `~/.local/share/oneai/daemon/`

**systemd Daemon**: `oneai-agent.service` user unit for periodic scheduling:
```ini
[Unit]
Description=OneAI Agent Scheduled Task Runner

[Service]
Type=simple
ExecStart=/usr/bin/oneai-agent-daemon --config ~/.config/oneai/config.toml

[Install]
WantedBy=default.target
```

---

## ObserverEvent → Native UI Bridge (All Platforms)

The TUI already defines `ObserverEvent` enum (in `examples/cli/src/tui.rs`). Every platform app uses the same event types, adapted to native UI:

| ObserverEvent | Android Compose | iOS SwiftUI | macOS SwiftUI | Linux GTK4 |
|---------------|----------------|-------------|---------------|------------|
| IterationStart | ParadigmBadge Composable | ParadigmBadge View | ParadigmBadge View | iteration_bar update |
| DirectAnswer | MessageBubble append | MessageRow append | MessageRow append | message_row append |
| StreamChunk | AnimatedContent typewriter | withAnimation typewriter | withAnimation typewriter | glib::timeout label |
| ToolCalls | ToolResultCard Composable | ToolResultCard GroupBox | ToolResultCard Disclosure | tool_card ExpanderRow |
| ToolResult | Update tool card | Update GroupBox | Update Disclosure | Update ExpanderRow |
| Delegate | SubAgentBadge | SubAgentIndicator | SubAgentIndicator | SubAgentLabel |
| ParadigmSwitch | Update timeline | Update timeline | Update timeline | Update iteration bar |
| Checkpoint | Snackbar "saved" | Toast notification | Notification | libnotify toast |
| Complete | Stop spinner | Stop ProgressView | Stop spinner | Stop gtk::Spinner |
| Error | Error card (red) | Error row (red) | Error row (red) | Error row (red) |

---

## Platform Capabilities Mapping

| Capability | Android | iOS | macOS | Linux |
|------------|---------|-----|-------|-------|
| **Screenshot** | ✅ MediaProjection API | ⚠️ foreground only | ✅ CGWindowListCreate | ✅ scrot/GDK |
| **Camera** | ✅ CameraX | ✅ AVCaptureSession | ❌ (no built-in camera) | ⚠️ Pipewire/v4l2 |
| **Filesystem sandbox** | ✅ AppSandbox (OS-enforced) | ✅ AppSandbox (OS-enforced) | ✅ ProjectDir sandbox | ✅ ProjectDir sandbox |
| **Notifications** | ✅ NotificationCompat | ✅ UNUserNotification | ✅ NSUserNotification | ✅ libnotify |
| **Network status** | ✅ ConnectivityManager | ✅ NWPathMonitor | ✅ NWPathMonitor | ✅ NetworkManager dbus |
| **Shell execution** | ❌ (restricted) | ❌ (no access) | ✅ (full) | ✅ (full) |
| **Clipboard** | ❌ (limited) | ⚠️ (foreground) | ✅ NSPasteboard | ✅ xclip/xsel |
| **Background execution** | ✅ ForegroundService | ⚠️ BGTaskScheduler (30s) | ✅ LaunchAgent daemon | ✅ systemd user service |
| **Scheduled tasks** | ✅ WorkManager | ⚠️ BGTaskScheduler | ✅ LaunchAgent | ✅ systemd timer |
| **App automation** | ❌ | ❌ | ✅ AppleScript | ✅ DBus/XDG |

**Platform-specific ShellTool replacement**:
- Android: No ShellTool → AndroidTool (PackageManager, Intent launcher, ContentProvider queries)
- iOS: No ShellTool → IOSTool (FileManager operations within sandbox, Process limited to app)
- macOS/Linux: Full ShellTool as-is

---

## Data Persistence Strategy

| Platform | Primary Store | Rust Backend | Config Location |
|----------|--------------|-------------|-----------------|
| Android | Room (Kotlin side) | SqliteCheckpointBackend (Rust) | App private storage |
| iOS | CoreData (Swift side) | SqliteCheckpointBackend (Rust) | App sandbox Library/ |
| macOS | SQLite (Rust rusqlite) | SqliteCheckpointBackend | ~/.config/oneai/ |
| Linux | SQLite (Rust rusqlite) | SqliteCheckpointBackend | ~/.config/oneai/ |

Mobile platforms use their native ORM (Room/CoreData) for UI-facing data (sessions, messages, settings) while the Rust core uses SqliteCheckpointBackend for agent state. Desktop platforms use pure Rust (rusqlite) for everything.

---

## Domain Pack Integration

Each platform app exposes a Domain Pack selector that reconfigures the entire agent:

```
DomainPack.coding("/project/dir")
  → Tools: ShellTool, FileReadTool, FileEditTool, GrepTool, GlobTool, CalculatorTool
  → Permission: Read=auto, Standard=confirm, Full=require_approval
  → System prompt: "You are a coding assistant..."
  → Skills: rust_debugging, code_refactoring, git_operations
  → Context sources: git_status, file_tree, project_config
  → Compression: preserve "critical files + progress + key decisions"

DomainPack.research()
  → Tools: WebSearchTool (MCP), WebFetchTool, PDFTool, CalculatorTool
  → Permission: Read=auto, Standard=auto, Full=require_approval
  → System prompt: "You are a research assistant..."
  → Skills: academic_search, data_extraction, citation_management
  → Context sources: search_results, document_library
  → Compression: preserve "source URLs + key findings + citations"
```
---

## Implementation Priority & Roadmap

### Phase A: Android Compose App (4-6 weeks)

**Week 1-2**: Project setup + core chat
1. Create Gradle project with cargo-ndk integration
2. Generate Kotlin bindings via UniFFI
3. Implement ChatScreen + ChatViewModel
4. Bridge observer events (Kotlin Channel → Flow)
5. Implement streaming typewriter (AnimatedContent)

**Week 3-4**: Tools + approval + platform features
6. Implement ToolResultCard, IterationTimeline
7. Implement ApprovalDialog (JNI bridge + AlertDialog)
8. Implement DrawerContent (tools, memory, sessions sidebar)
9. Implement ProviderConfigScreen, PermissionPolicyScreen
10. Implement CameraCapture (CameraX → Image ContentBlock)
11. Implement ForegroundService for background execution

**Week 5-6**: Advanced features
12. Memory browser with search
13. Trace viewer with metrics
14. Workflow editor (Compose Canvas)
15. RAG search UI
16. Domain pack selector
17. Checkpoint recovery UI
18. Notification integration
19. WorkManager scheduled tasks
20. End-to-end testing on emulator + physical device

### Phase B: iOS SwiftUI App (4-6 weeks)

**Week 1-2**: Project setup + core chat
1. Create Xcode project with cargo-lipo
2. Generate Swift bindings via UniFFI
3. Implement ChatView + OneAIViewModel
4. Bridge observer events (C callback → AsyncStream)
5. Implement streaming typewriter (withAnimation)

**Week 3-4**: Tools + approval + platform features
6. Implement ToolResultCard, IterationTimeline
7. Implement ApprovalSheet (UIAlertController → SwiftUI)
8. Implement Sidebar (tools, memory, sessions)
9. Implement Settings tabs (provider, permission, domain)
10. Implement CameraCaptureView (AVCaptureSession)
11. Implement Haptic feedback
12. Implement Dynamic Island (ActivityKit)

**Week 5-6**: Advanced features
13. Memory browser with search
14. Trace tree viewer
15. Workflow editor (SwiftUI Canvas)
16. RAG search UI
17. Domain pack selector
18. Checkpoint recovery UI
19. Notification handling
20. Background execution constraints
21. End-to-end testing on simulator + physical device

### Phase C: macOS SwiftUI App (3-4 weeks)

1. Create Xcode project (macOS target)
2. Reuse iOS Swift bindings (same UniFFI)
3. Implement 3-column NavigationSplitView
4. Reuse observer bridge (same AsyncStream pattern)
5. Implement NSAlert approval gate
6. Implement MacOSPlatformCapabilities (screenshot, clipboard)
7. Add PlatformTools (AppleScriptTool, ClipboardTool, ScreenshotTool)
8. Add ShellTool (full access, no restriction)
9. Implement DaemonManager (LaunchAgent)
10. Add keyboard shortcuts (Cmd+Enter, Cmd+K, Cmd+S, Cmd+N, Cmd+T)
11. Wire up all advanced features (memory, trace, workflow, RAG, checkpoint, domain)
12. End-to-end testing

### Phase D: Linux GTK4 App (3-4 weeks)

1. Create Cargo project with gtk4-rs + libadwaita deps
2. Implement AdwApplication + AdwApplicationWindow
3. Direct Rust import of oneai-* crates (no FFI!)
4. Implement observer → glib::idle_add bridge
5. Implement chat view (gtk::ScrolledWindow + ListBox)
6. Implement streaming label (glib::timeout_add)
7. Implement gtk::MessageDialog approval gate
8. Implement LinuxPlatformCapabilities (screenshot, notifications, filesystem)
9. Add PlatformTools (ClipboardTool, ScreenshotTool, XDGTool)
10. Add ShellTool (full access)
11. Implement AdwFlap sidebar (tools, memory, sessions)
12. Implement AdwPreferencesDialog for settings
13. Wire up all advanced features (memory, trace, workflow, RAG, checkpoint, domain)
14. Add systemd user service for daemon mode
15. Add XDG desktop file + AppStream metadata
16. End-to-end testing on GNOME + other desktops

---

## Key Files to Create/Modify

### New files (examples/)

**Android** (entire new directory):
- `examples/android-app/` — Gradle project with Compose UI, JNI bridge, cargo-ndk

**iOS** (entire new directory):
- `examples/ios-app/OneAI/` — Xcode SwiftUI project with UniFFI bindings

**macOS** (entire new directory):
- `examples/macos-app/OneAI/` — Xcode SwiftUI project (macOS target)

**Linux** (new Cargo project):
- `examples/linux-gtk4-app/Cargo.toml` — gtk4-rs + libadwaita + oneai-* deps
- `examples/linux-gtk4-app/src/main.rs` — AdwApplication entry
- `examples/linux-gtk4-app/src/ui/*.rs` — All GTK4 UI components
- `examples/linux-gtk4-app/src/bridge/*.rs` — Observer adapter, platform caps

### Existing files to modify

- `Cargo.toml` (workspace root) — Add new example workspace members
- `crates/oneai-platform-android/src/gate.rs` — Enhance with Compose Dialog support
- `crates/oneai-platform-android/src/jni_bridge.rs` — Add observer event JNI bridge
- `crates/oneai-platform-ios/src/gate.rs` — Enhance with SwiftUI sheet support
- `crates/oneai-platform-ios/src/callback_bridge.rs` — Add observer event C callbacks
- `crates/oneai-platform-desktop/src/linux.rs` — Add LinuxPlatformCapabilities + GTK4 dialog
- `crates/oneai-platform-desktop/src/macos.rs` — Add MacOSPlatformCapabilities
- `crates/oneai-platform-desktop/src/windows.rs` — Add WindowsPlatformCapabilities
- `crates/oneai-core/src/platform_capabilities.rs` — Add concrete FilesystemSandbox impls per platform
- `crates/oneai-uniffi/src/types.rs` — Add ObserverEvent UniFFI enum for Kotlin/Swift

### Crate dependencies to add

**Linux GTK4 app (Cargo.toml)**:
```toml
gtk4 = "0.9"
libadwaita = "0.7"
rusqlite = { version = "0.31", features = ["bundled"] }
```

**Android/iOS/macOS**: UniFFI bindings are already in workspace. No new Rust deps needed for the framework side — platform-native deps (Compose, SwiftUI, etc.) are managed by Gradle/Xcode.

---

## Verification Plan

### Android
1. `cargo ndk -t arm64-v8a build --release` — Rust cross-compilation succeeds
2. Gradle build → APK installs on emulator
3. Send chat message → verify streaming typewriter appears
4. Trigger CalculatorTool → verify tool card renders with result
5. Trigger ShellTool → verify ApprovalDialog appears → approve → verify execution continues
6. Camera capture → verify image sent to agent as ContentBlock.Image
7. Background: minimize app → verify ForegroundService continues → verify notification on completion
8. Memory search → verify hybrid scoring returns relevant entries
9. Workflow: define DAG → verify execution → verify result display
10. Trace: complete agent run → verify span tree + metrics (success rate, token usage)
11. Domain pack switch → verify tool set, permission profile, system prompt changes

### iOS
1. `cargo lipo` — Universal binary builds for arm64 + x86_64
2. Xcode build → app installs on simulator
3. Same chat/tool/approval/camera/memory/workflow/trace verification as Android
4. Background: leave app → verify BGTaskScheduler handles → verify notification
5. Haptic: verify feedback on tool execution and approval
6. Dynamic Island: verify Live Activity shows during agent execution (physical device only)
7. No ShellTool: verify alternative tools (FileManager) work correctly

### macOS
1. `cargo build` — native binary builds
2. Xcode build → app launches
3. Same chat/tool/approval verification, plus:
4. ShellTool: verify full shell execution works
5. Screenshot: verify CGWindowListCreateImage captures → agent receives
6. AppleScriptTool: verify "open Safari" command works
7. ClipboardTool: verify read/write clipboard
8. Keyboard shortcuts: verify Cmd+Enter, Cmd+K, Cmd+S, Cmd+N, Cmd+T
9. Daemon: verify LaunchAgent plist created → periodic task runs

### Linux
1. `cargo build` — native binary builds with GTK4 deps
2. App launches → GNOME-style window appears
3. Same chat/tool/approval verification, plus:
4. ShellTool: verify full shell execution works
5. Screenshot: verify scrot/GDK capture → agent receives
6. ClipboardTool: verify xclip read/write
7. Notifications: verify libnotify toast on agent completion
8. XDG paths: verify config/data/cache in correct locations
9. systemd: verify daemon service runs periodic tasks

### Cross-Platform Integration Tests
- Same Rust core behavior on all platforms (agent loop, tool execution, memory, checkpoint)
- Approval gate works on each platform with native UI
- UniFFI bindings compile for Kotlin, Swift, C++
- Platform capabilities correctly detect availability (supports_camera, supports_screenshot, etc.)
- All 212 existing tests continue passing
