// chat.js — the webview UI: renders `event` notifications (turn_start /
// stream_chunk / thinking / tool_calls / approval_request / speaker_turn /
// turn_complete …) into the messages surface, sends `turn/run`, shows the
// scenario sidebar (scenario/list → start a group chat / edit / delete),
// and hosts the cross-frontend scenario editor (the shared
// scenario-editor.js, mounted in the #editor-view tab). Multi-agent
// scenarios render speaker-tagged turns.
//
// Event shapes (params.kind → fields) mirror crates/oneai-bus EngineYield —
// see docs/app-server-mechanism.md §4. The `event` notification's params is
// the full yield with its `kind` tag; we switch on it.

(function () {
  "use strict";

  const messages = document.getElementById("messages");
  const input = document.getElementById("input");
  const send = document.getElementById("send");
  const scenarioList = document.getElementById("scenario-list");
  const chatView = document.getElementById("chat-view");
  const editorView = document.getElementById("editor-view");
  const tabChat = document.getElementById("tab-chat");
  const tabScenarios = document.getElementById("tab-scenarios");

  let currentTurn = null; // live assistant bubble being streamed into
  let liveScenario = null; // the active BusScenario, if in group mode
  let editor = null; // OneAiScenarioEditor instance (lazy — built on first use)

  // ── Tabs ─────────────────────────────────────────────────────────────
  // The `oneai.scenarios` command (extension.ts) posts a `navigate` message
  // with view="scenarios"; rpc-webview.js re-dispatches it as a CustomEvent.
  function showView(v) {
    chatView.classList.toggle("show", v === "chat");
    editorView.classList.toggle("show", v === "scenarios");
    tabChat.classList.toggle("active", v === "chat");
    tabScenarios.classList.toggle("active", v === "scenarios");
  }
  tabChat.onclick = () => showView("chat");
  tabScenarios.onclick = () => {
    showView("scenarios");
    ensureEditor();
    editor.render();
  };
  window.addEventListener("navigate", (e) => {
    showView(e.detail || "chat");
    if ((e.detail || "") === "scenarios") {
      ensureEditor();
      editor.render();
    }
  });

  // ── Rendering ───────────────────────────────────────────────────────
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

  function appendText(el, text) {
    el.appendChild(document.createTextNode(text));
    messages.scrollTop = messages.scrollHeight;
  }

  // ── Event handlers (one per params.kind) ────────────────────────────
  rpc.onEvent("turn_start", () => {
    currentTurn = bubble("assistant", null);
  });
  rpc.onEvent("stream_chunk", (p) => {
    if (!currentTurn) currentTurn = bubble("assistant", null);
    const text = p.text || p.content || "";
    appendText(currentTurn, text);
  });
  rpc.onEvent("thinking", (p) => {
    const el = bubble("assistant", "thinking");
    el.className = "msg assistant thinking";
    appendText(el, p.text || "");
  });
  rpc.onEvent("tool_calls", (p) => {
    const el = bubble("assistant", "tools");
    const calls = (p.calls || []).map((c) => `${c.name}(${JSON.stringify(c.args || {}).slice(0, 80)})`);
    appendText(el, "🛠️ " + calls.join("; "));
  });
  rpc.onEvent("tool_result", (p) => {
    const el = bubble("assistant", "tool-result");
    appendText(el, "→ " + (p.summary || p.text || JSON.stringify(p.output || "").slice(0, 120)));
  });
  rpc.onEvent("speaker_turn", (p) => {
    // Group chat — each speaker gets its own bubble tagged with its name.
    currentTurn = bubble("assistant", p.speaker || p.member_id || "speaker");
  });
  rpc.onEvent("approval_request", (p) => {
    const el = bubble("assistant", "approval");
    el.className = "msg assistant approval";
    appendText(el, "Approval: " + (p.prompt || p.tool || "proceed?"));
    const yes = document.createElement("button");
    yes.textContent = "Approve";
    yes.onclick = () => rpc.call("approval/respond", { request_id: p.request_id, response: "Proceed" });
    const no = document.createElement("button");
    no.textContent = "Deny";
    no.onclick = () =>
      rpc.call("approval/respond", {
        request_id: p.request_id,
        response: { Abort: { reason: "denied" } },
      });
    const row = document.createElement("div");
    row.appendChild(yes);
    row.appendChild(no);
    el.appendChild(row);
  });
  rpc.onEvent("error", (p) => {
    const el = bubble("assistant", "error");
    appendText(el, "⚠️ " + (p.message || JSON.stringify(p)));
  });
  rpc.onEvent("turn_complete", () => {
    currentTurn = null;
  });

  // ── Send a turn ─────────────────────────────────────────────────────
  async function sendTurn(text) {
    if (!text.trim()) return;
    bubble("user", null).textContent = text;
    input.value = "";
    if (liveScenario) {
      // Group mode: scenario already started; run the round with this input.
      await rpc.call("group/run", { user_input: text });
    } else {
      await rpc.call("turn/run", { content: [{ type: "text", text }] });
    }
  }

  send.addEventListener("click", () => sendTurn(input.value));
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      sendTurn(input.value);
    }
  });

  // ── Scenario sidebar: scenario/list → start / edit / delete ────────
  async function loadScenarios() {
    const res = await rpc.call("scenario/list", {});
    const scenarios = (res && res.scenarios) || [];
    scenarioList.innerHTML = "";

    const newBtn = document.createElement("div");
    newBtn.className = "item new";
    newBtn.textContent = "+ New scenario";
    newBtn.addEventListener("click", () => openEditor(null));
    scenarioList.appendChild(newBtn);

    for (const sc of scenarios) {
      const item = document.createElement("div");
      item.className = "item";
      const label = document.createElement("span");
      label.textContent = sc.name || sc.id;
      label.title = (sc.members || []).map((m) => m.name).join(", ");
      label.addEventListener("click", () => startScenario(sc));

      const actions = document.createElement("span");
      actions.className = "row-actions";
      const edit = document.createElement("button");
      edit.textContent = "✎";
      edit.title = "Edit";
      edit.addEventListener("click", (e) => {
        e.stopPropagation();
        openEditor(sc);
      });
      const del = document.createElement("button");
      del.textContent = "🗑";
      del.title = "Delete";
      del.addEventListener("click", async (e) => {
        e.stopPropagation();
        if (!confirm(`Delete scenario "${sc.name || sc.id}"?`)) return;
        await rpc.call("scenario/delete", { id: sc.id });
        loadScenarios();
      });
      actions.appendChild(edit);
      actions.appendChild(del);
      item.appendChild(label);
      item.appendChild(actions);
      scenarioList.appendChild(item);
    }
  }

  // ── Scenario editor tab (shared scenario-editor.js) ───────────────
  // Lazy-create on first open so the scenario/list RPC for the preset picker
  // doesn't fire until the user actually opens the editor.
  function ensureEditor() {
    if (editor) return;
    editor = OneAiScenarioEditor.create({
      rpc,
      root: editorView,
      onSaved: () => loadScenarios(),
      onDeleted: () => loadScenarios(),
      onError: (m) => appendText(bubble("assistant", "error"), "⚠️ " + m),
    });
  }

  function openEditor(sc) {
    showView("scenarios");
    ensureEditor();
    if (sc) editor.edit(sc);
    else editor.newScenario();
  }

  async function startScenario(sc) {
    liveScenario = sc;
    messages.innerHTML = "";
    bubble("user", null).textContent = `🎬 ${sc.name}`;
    // Submit the rich BusScenario as `scenario` — the app-server adapter's
    // StartGroupChat deserialize ignores editor-only fields (topic_fields/
    // debrief/icon/name) via serde defaults; the engine consumes members/
    // turn_policy/etc. (Topic-value baking into member prompts is a future
    // shared step; for now members ship their stored system_prompt.)
    await rpc.call("group/start", { scenario: sc });
    if (sc.opener_agent_id) {
      await rpc.call("group/open", {}); // opener turn
    } else {
      // No opener — kick off the first round with a blank user turn so the
      // scripted/round-robin speakers begin.
      await rpc.call("group/run", { user_input: " " });
    }
  }

  loadScenarios();
})();
