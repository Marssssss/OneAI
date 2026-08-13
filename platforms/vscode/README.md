# OneAI — VS Code extension

Spawn the OneAI engine as a child process and chat with it in a webview — no
manual server start (Codex/LSP model: the extension owns the spawn).

## What it does

On activation the extension runs `oneai app-server --listen stdio` and speaks
JSON-RPC 2.0 over the child's stdin/stdout (newline-delimited). `oneai.chat`
opens a webview panel that:

- sends `turn/run` / `approval/respond` from the composer,
- renders `event` notifications (`stream_chunk`, `thinking`, `tool_calls`,
  `approval_request`, `speaker_turn`, `turn_complete`, …),
- lists the shared scenario library (`scenario/list`) and starts a multi-agent
  group chat (`group/start` → `group/open` → `group/run`).

If the engine exits, the extension restarts it with exponential backoff
(`oneai.autoRestart`).

## Setup

```bash
cd platforms/vscode
npm install
npm run compile        # esbuild → dist/extension.js
```

## Run

Press <kbd>F5</kbd> (Launch Extension Development Host), or:

```bash
code --extensionDevelopmentPath="$PWD"
```

Set `oneai.oneaiPath` if `oneai` is not on PATH; set `oneai.apiKey` /
`oneai.baseUrl` / `oneai.model` for your provider (Ollama works with an empty
key). Then run **OneAI: Open Chat**.

## Layout

- `src/extension.ts` — activation, commands, webview panel, postMessage ↔ engine relay.
- `src/server.ts` — `OneAiServer`: spawn + newline-JSON-RPC + restart.
- `src/webview/rpc-webview.js` — the webview's postMessage JSON-RPC client.
- `src/webview/chat.js` — event rendering + scenario sidebar + send.

The webview cannot spawn the engine; it relays JSON-RPC through the extension
host. The browser extension (`platforms/browser`) speaks the same JSON-RPC
envelope over Chrome native messaging instead.
