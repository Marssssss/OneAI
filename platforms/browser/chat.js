// chat.js — the browser popup UI. Same JSON-RPC surface as the VS Code
// webview's chat.js (rpc.call / rpc.onEvent), but over native messaging
// (rpcNative). Renders events into #messages, sends turn/run, lists scenarios,
// and hosts the shared scenario editor (scenario-editor.js) — cast + turn
// policy, live-validated via scenario/validate (the single authoritative
// validator shipped in Phase G, no client-side mirror).
(function () {
  "use strict";

  const rpc = rpcNative; // same call/onEvent surface
  const messages = document.getElementById("messages");
  const input = document.getElementById("input");
  const send = document.getElementById("send");
  const scenarios = document.getElementById("scenarios");
  const editorRoot = document.getElementById("editor");
  const tabChat = document.getElementById("tab-chat");
  const tabEditor = document.getElementById("tab-editor");

  let currentTurn = null;
  let liveScenario = null;
  let editor = null; // OneAiScenarioEditor instance (lazy)

  // ── Tab switch ──────────────────────────────────────────────────────
  function show(which) {
    document.getElementById("chat").classList.toggle("show", which === "chat");
    document.getElementById("editor").classList.toggle("show", which === "editor");
    tabChat.classList.toggle("active", which === "chat");
    tabEditor.classList.toggle("active", which === "editor");
    if (which === "editor") {
      ensureEditor();
      editor.render();
    }
  }
  tabChat.onclick = () => show("chat");
  tabEditor.onclick = () => show("editor");

  // ── Rendering (mirrors the VS Code webview chat.js) ─────────────────
  function bubble(role, speaker) {
    const div = document.createElement("div");
    div.className = "msg " + role;
    if (speaker) {
      const sp = document.createElement("div");
      sp.className = "speaker";
      sp.textContent = speaker;
      div.appendChild(sp);
    }
    const body = document.createElement("div");
    div.appendChild(body);
    messages.appendChild(div);
    messages.scrollTop = messages.scrollHeight;
    return body;
  }
  function text(el, t) {
    el.appendChild(document.createTextNode(t));
    messages.scrollTop = messages.scrollHeight;
  }

  rpc.onEvent("turn_start", () => (currentTurn = bubble("assistant")));
  rpc.onEvent("stream_chunk", (p) => {
    if (!currentTurn) currentTurn = bubble("assistant");
    text(currentTurn, p.text || p.content || "");
  });
  rpc.onEvent("thinking", (p) => {
    const el = bubble("assistant");
    el.parentElement.classList.add("thinking");
    text(el, p.text || "");
  });
  rpc.onEvent("tool_calls", (p) =>
    text(bubble("assistant", "tools"), "🛠️ " + (p.calls || []).map((c) => c.name).join(", ")),
  );
  rpc.onEvent("speaker_turn", (p) => (currentTurn = bubble("assistant", p.speaker || p.member_id)));
  rpc.onEvent("approval_request", (p) => {
    const el = bubble("assistant");
    el.parentElement.className = "msg assistant approval";
    text(el, "Approval: " + (p.prompt || p.tool || "proceed?"));
    const y = document.createElement("button");
    y.textContent = "Approve";
    y.onclick = () => rpc.call("approval/respond", { request_id: p.request_id, response: "Proceed" });
    const n = document.createElement("button");
    n.textContent = "Deny";
    n.onclick = () =>
      rpc.call("approval/respond", { request_id: p.request_id, response: { Abort: { reason: "denied" } } });
    el.appendChild(y);
    el.appendChild(n);
  });
  rpc.onEvent("error", (p) => text(bubble("assistant"), "⚠️ " + (p.message || JSON.stringify(p))));
  rpc.onEvent("turn_complete", () => (currentTurn = null));

  // ── Send ─────────────────────────────────────────────────────────────
  async function sendTurn(t) {
    if (!t.trim()) return;
    bubble("user").textContent = t;
    input.value = "";
    if (liveScenario) await rpc.call("group/run", { user_input: t });
    else await rpc.call("turn/run", { content: [{ type: "text", text: t }] });
  }
  send.onclick = () => sendTurn(input.value);
  input.onkeydown = (e) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      sendTurn(input.value);
    }
  };

  // ── Scenario sidebar (scenario/list → start group chat) ─────────────
  async function loadScenarios() {
    try {
      const res = await rpc.call("scenario/list", {});
      const list = (res && res.scenarios) || [];
      scenarios.innerHTML = "";
      for (const sc of list) {
        const b = document.createElement("button");
        b.textContent = "🎬 " + (sc.name || sc.id);
        b.style.cssText = "display:block;margin:2px 0;width:100%";
        b.onclick = () => startScenario(sc);
        scenarios.appendChild(b);
      }
    } catch (e) {
      scenarios.textContent = "scenarios unavailable: " + e.message;
    }
  }

  async function startScenario(sc) {
    liveScenario = sc;
    messages.innerHTML = "";
    bubble("user").textContent = "🎬 " + sc.name;
    await rpc.call("group/start", { scenario: sc });
    if (sc.opener_agent_id) await rpc.call("group/open", {});
    else await rpc.call("group/run", { user_input: " " });
  }

  // ── Scenario editor (shared scenario-editor.js) ────────────────────
  // The same form runs in the VS Code webview and here; only the rpc
  // transport differs. Live-validates via scenario/validate, saves via
  // scenario/upsert, deletes via scenario/delete.
  function ensureEditor() {
    if (editor) return;
    editor = OneAiScenarioEditor.create({
      rpc,
      root: editorRoot,
      onSaved: () => loadScenarios(),
      onDeleted: () => loadScenarios(),
      onError: (m) => text(bubble("assistant"), "⚠️ " + m),
    });
  }

  // ── Boot: connect native messaging, then load scenarios ─────────────
  (async function boot() {
    try {
      await rpc.connect();
    } catch (e) {
      messages.innerHTML = "";
      const d = bubble("assistant");
      d.textContent = "⚠️ " + e.message;
    }
    loadScenarios();
  })();
})();
