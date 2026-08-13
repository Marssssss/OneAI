# OneAI — browser extension

Chat with the OneAI engine from a browser popup, over Chrome/Firefox **native
messaging**. No manual server start: the browser spawns
`oneai app-server --listen native-messaging` on connect (the host manifest is
installed once by `install-host.sh`). This is the Codex model for the browser
case — a frontend that can't spawn a process uses native messaging so the
browser does the spawn.

## Wire format

Native messaging frames each message as a 4-byte little-endian length prefix
+ JSON. That matches `oneai app-server --listen native-messaging`
(`serve_native_messaging`, Phase A) — the browser handles the framing on its
side, the extension posts/receives plain JSON objects on the Port. The
JSON-RPC envelope is the same one the VS Code extension speaks over stdio
(`platforms/vscode/src/server.ts`); only the transport differs.

## Install

```bash
# 1. Build the engine and put `oneai` on PATH (or pass --bin=/path/to/oneai).
cargo install --path examples/cli   # or your local build

# 2. Register the native-messaging host (run once per browser/profile).
cd platforms/browser
./install-host.sh --browser=chrome --ext-id=<YOUR_EXTENSION_ID>
# Firefox:
./install-host.sh --browser=firefox --ext-id=oneai@oneai
```

Load the extension:

- **Chrome**: `chrome://extensions` → Developer mode → Load unpacked →
  select `platforms/browser`. Copy the extension id, pass it to
  `install-host.sh` (it writes the host manifest's `allowed_origins`).
- **Firefox**: `about:debugging` → This Firefox → Load Temporary Add-on →
  select `manifest.json`. The id is `oneai@oneai` (from
  `browser_specific_settings`).

Open the popup → it calls `chrome.runtime.connectNative('com.oneai.appserver')`
→ the browser spawns the host. Troubleshoot connect failures at
`chrome://native-internals`.

## What it does

- **Chat tab**: `turn/run`, render `stream_chunk` / `thinking` / `tool_calls` /
  `approval_request` / `speaker_turn` / `turn_complete`; scenario sidebar
  (`scenario/list`) → start a group chat (`group/start` → `group/open` →
  `group/run`).
- **Scenario Editor tab**: edits cast + turn policy + topic fields with **live
  `scenario/validate`** — the single authoritative validator (Phase G), no
  client-side mirror that drifts. Saves via `scenario/upsert`.

## Windows host

The manifest `path` must point at an `.exe` on Windows (a shell wrapper won't
do; Chrome uses `CreateProcess`). Windows native-messaging host packaging is
deferred — the engine binary is `oneai.exe` once cross-compiled; register it
directly with `--bin=...\oneai.exe` once that build path exists. macOS/Linux
hosts work today via the wrapper script.
