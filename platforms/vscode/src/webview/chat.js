// chat.js — the webview UI: renders `event` notifications (turn_start /
// stream_chunk / thinking / tool_calls / approval_request / speaker_turn /
// turn_complete …) into the messages surface, sends `turn/run`, and shows the
// scenario sidebar (scenario/list → open a group chat via group/start +
// group/open + group/run). Multi-agent scenarios render speaker-tagged turns.
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

  let currentTurn = null; // live assistant bubble being streamed into
  let liveScenario = null; // the active BusScenario, if in group mode

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
  rpc.onEvent("turn_start", (p) => {
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

  // ── Scenario sidebar: scenario/list → click to start a group chat ───
  async function loadScenarios() {
    const res = await rpc.call("scenario/list", {});
    const scenarios = (res && res.scenarios) || [];
    scenarioList.innerHTML = "";
    for (const sc of scenarios) {
      const item = document.createElement("div");
      item.className = "item";
      item.textContent = sc.name || sc.id;
      item.title = (sc.members || []).map((m) => m.name).join(", ");
      item.addEventListener("click", () => startScenario(sc));
      scenarioList.appendChild(item);
    }
  }

  async function startScenario(sc) {
    liveScenario = sc;
    messages.innerHTML = "";
    bubble("user", null).textContent = `🎬 ${sc.name}`;
    // Compile the rich BusScenario → engine BusGroupScenario payload. The
    // shared `toGroupScenario` lives in scenario-editor.js; for the launch
    // flow we just submit the rich object as `scenario` — the app-server
    // adapter's StartGroupChat deserialize ignores the editor-only fields
    // (topic_fields/debrief/icon/name) via serde defaults, and the engine
    // consumes members/turn_policy/etc. (Topic-value baking into member
    // prompts is a future shared step; for now members ship their stored
    // system_prompt.)
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
