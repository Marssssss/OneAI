// OneAI VS Code extension — activation spawns the engine (see server.ts) and
// registers a `oneai.chat` command that opens a webview panel. The webview
// renders chat + the scenario editor off `event` notifications; it relays
// JSON-RPC requests to the engine through this extension via postMessage
// (a webview cannot spawn the child itself).

import * as vscode from "vscode";
import { OneAiServer, type EngineConfig } from "./server";

let server: OneAiServer | undefined;

export function activate(context: vscode.ExtensionContext) {
  const log = vscode.window.createOutputChannel("OneAI");
  context.subscriptions.push(log);

  const cfg = readConfig();
  server = new OneAiServer(cfg, log);
  server.start(); // Codex/LSP: spawn on activation. If the binary is missing
  // the user sees a stderr line in the output channel + a chat-send error.

  // ── Commands ─────────────────────────────────────────────────────────
  context.subscriptions.push(
    vscode.commands.registerCommand("oneai.chat", () => openChatPanel(context)),
    vscode.commands.registerCommand("oneai.scenarios", () => openChatPanel(context, "scenarios")),
    vscode.commands.registerCommand("oneai.restart", () => {
      server?.dispose();
      server = new OneAiServer(readConfig(), log);
      server.start();
      log.appendLine("[oneai] engine restarted by user.");
    }),
  );
}

export function deactivate() {
  server?.dispose();
}

/** Read the engine config from the VS Code settings (package.json schema). */
function readConfig(): EngineConfig {
  const c = vscode.workspace.getConfiguration("oneai");
  return {
    oneaiPath: c.get("oneaiPath", "oneai"),
    providerKind: c.get("providerKind", "openai"),
    apiKey: c.get("apiKey", ""),
    baseUrl: c.get("baseUrl", ""),
    model: c.get("model", ""),
    autoRestart: c.get("autoRestart", true),
  };
}

/** The chat/scenario webview panel. One shared panel; reuses if open. */
let panel: vscode.WebviewPanel | undefined;

function openChatPanel(
  context: vscode.ExtensionContext,
  initialView: "chat" | "scenarios" = "chat",
) {
  if (panel) {
    panel.reveal();
    panel.webview.postMessage({ kind: "navigate", view: initialView });
    return;
  }
  panel = vscode.window.createWebviewPanel(
    "oneaiChat",
    "OneAI",
    vscode.ViewColumn.Two,
    {
      enableScripts: true,
      retainContextWhenHidden: true, // keep the chat state across tab switches
      localResourceRoots: [
        vscode.Uri.joinPath(context.extensionUri, "src", "webview"),
      ],
    },
  );
  panel.webview.html = chatHtml(panel.webview, context);

  // ── Wire webview ↔ engine via postMessage ─────────────────────────
  // The webview can't spawn the child; it posts JSON-RPC requests here and
  // this extension forwards them to the server, then relays the response +
  // every `event` notification back.
  const forwardEvents = (kind: string, params: unknown) => {
    panel?.webview.postMessage({ kind: "event", eventKind: kind, params });
  };
  // Subscribe to every event kind — the webview switches on eventKind.
  const eventKinds = [
    "turn_start", "stream_chunk", "thinking", "tool_calls", "tool_result",
    "delegate", "delegate_complete", "speaker_turn", "paradigm_switch",
    "approval_request", "working_state", "context_accounting", "plan_update",
    "tools_added", "init_result", "compact_result", "token_usage", "error",
    "turn_complete", "iteration_start", "session_created", "session_loaded",
    "session_cleared", "session_deleted", "session_ended",
  ];
  const unsubs = eventKinds.map((k) => server!.onEvent(k, (p) => forwardEvents(k, p)));

  panel.webview.onDidReceiveMessage(
    async (msg) => {
      if (msg?.kind === "rpc") {
        try {
          const result = await server!.call(msg.method, msg.params);
          panel?.webview.postMessage({ kind: "rpc-result", id: msg.id, result });
        } catch (e) {
          panel?.webview.postMessage({
            kind: "rpc-result",
            id: msg.id,
            error: e instanceof Error ? e.message : String(e),
          });
        }
      }
    },
    undefined,
    context.subscriptions,
  );

  panel.onDidDispose(() => {
    unsubs.forEach((u) => u());
    panel = undefined;
  });
}

/** Inline the webview HTML. The webview loads the static JS via webview URIs. */
function chatHtml(webview: vscode.Webview, context: vscode.ExtensionContext): string {
  const nonce = Math.random().toString(36).slice(2);
  const js = (name: string) =>
    webview.asWebviewUri(vscode.Uri.joinPath(context.extensionUri, "src", "webview", name)).toString();

  return /* html */ `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8" />
<meta http-equiv="Content-Security-Policy"
  content="default-src 'none'; script-src 'nonce-${nonce}'; style-src 'unsafe-inline';" />
<title>OneAI</title>
<style>
  body { font-family: var(--vscode-font-family); margin: 0; display: flex; height: 100vh; }
  #sidebar { width: 220px; border-right: 1px solid var(--vscode-panel-border); overflow-y: auto; }
  #sidebar h3 { padding: 8px 12px; margin: 0; font-size: 11px; text-transform: uppercase; opacity: .7; }
  #sidebar .item { padding: 6px 12px; cursor: pointer; display: flex; align-items: center; justify-content: space-between; }
  #sidebar .item:hover { background: var(--vscode-list-hover-background); }
  #sidebar .item .row-actions { display: none; gap: 2px; }
  #sidebar .item:hover .row-actions { display: inline-flex; }
  #sidebar .item .row-actions button { padding: 0 4px; background: none; border: none; cursor: pointer; color: inherit; }
  #sidebar .item.new { font-weight: 600; }
  #main { flex: 1; display: flex; flex-direction: column; }
  #tabs { display: flex; border-bottom: 1px solid var(--vscode-panel-border); }
  #tabs button { padding: 6px 14px; border: none; background: none; cursor: pointer; color: var(--vscode-foreground); opacity: .7; }
  #tabs button.active { opacity: 1; border-bottom: 2px solid var(--vscode-focusBorder); }
  .view { display: none; flex: 1; flex-direction: column; min-height: 0; }
  .view.show { display: flex; }
  #messages { flex: 1; overflow-y: auto; padding: 8px; }
  .msg { margin: 6px 0; padding: 8px; border-radius: 6px; }
  .msg.user { background: var(--vscode-input-background); }
  .msg.assistant { background: var(--vscode-editor-background); border: 1px solid var(--vscode-panel-border); }
  .msg .speaker { font-size: 11px; opacity: .7; margin-bottom: 4px; }
  .thinking { opacity: .6; font-style: italic; }
  .approval { background: var(--vscode-inputOption-activeBorder); padding: 8px; margin: 6px 0; border-radius: 6px; }
  #composer { display: flex; border-top: 1px solid var(--vscode-panel-border); }
  #composer textarea { flex: 1; resize: none; height: 4em; background: var(--vscode-input-background); color: var(--vscode-input-foreground); border: none; padding: 8px; font-family: inherit; }
  #composer button { padding: 8px 16px; }
  #editor-view { overflow-y: auto; padding: 12px; }
  #editor-view .field { margin: 6px 0; }
  #editor-view label { display: block; font-size: 11px; opacity: .7; }
  #editor-view input, #editor-view select, #editor-view textarea { width: 100%; box-sizing: border-box; background: var(--vscode-input-background); color: var(--vscode-input-foreground); border: 1px solid var(--vscode-input-border); padding: 4px; font-family: inherit; }
  #editor-view .errors { color: var(--vscode-errorForeground, #c00); font-size: 11px; margin-top: 6px; }
  #editor-view .row { display: flex; gap: 6px; }
  #editor-view .row > * { flex: 1; }
</style>
</head>
<body>
  <div id="sidebar">
    <h3>Scenarios</h3>
    <div id="scenario-list"></div>
  </div>
  <div id="main">
    <div id="tabs">
      <button id="tab-chat" class="active">Chat</button>
      <button id="tab-scenarios">Scenarios</button>
    </div>
    <div id="chat-view" class="view show">
      <div id="messages"></div>
      <div id="composer">
        <textarea id="input" placeholder="Message the agent… (Shift+Enter for newline)"></textarea>
        <button id="send">Send</button>
      </div>
    </div>
    <div id="editor-view" class="view"></div>
  </div>
  <script nonce="${nonce}" src="${js("rpc-webview.js")}"></script>
  <script nonce="${nonce}" src="${js("scenario-editor.js")}"></script>
  <script nonce="${nonce}" src="${js("chat.js")}"></script>
</body>
</html>`;
}
