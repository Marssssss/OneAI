// chat.js — the browser popup UI. Same JSON-RPC surface as the VS Code
// webview's chat.js (rpc.call / rpc.onEvent), but over native messaging
// (rpcNative). Renders events into #messages, sends turn/run, lists scenarios,
// and hosts a live scenario editor (cast + turn policy + topic fields) that
// calls scenario/validate on every edit — the single authoritative validator
// shipped in Phase G, no client-side mirror.
(function () {
  "use strict";

  const rpc = rpcNative; // same call/onEvent surface
  const messages = document.getElementById("messages");
  const input = document.getElementById("input");
  const send = document.getElementById("send");
  const scenarios = document.getElementById("scenarios");
  const editor = document.getElementById("editor");
  const tabChat = document.getElementById("tab-chat");
  const tabEditor = document.getElementById("tab-editor");

  let currentTurn = null;
  let liveScenario = null;

  // ── Tab switch ──────────────────────────────────────────────────────
  function show(which) {
    document.getElementById("chat").classList.toggle("show", which === "chat");
    document.getElementById("editor").classList.toggle("show", which === "editor");
    tabChat.classList.toggle("active", which === "chat");
    tabEditor.classList.toggle("active", which === "editor");
    if (which === "editor") renderEditor();
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

  // ── Scenario editor (live scenario/validate) ────────────────────────
  // The form edits a BusScenario's name + cast (name/system_prompt/kind) +
  // turn_policy + topic fields, validating on every edit via scenario/validate
  // — the shared authoritative validator (Phase G). This is the cross-frontend
  // scenario editor: the same form runs in the VS Code webview (future) and
  // here; only the rpc transport differs.
  let editing = null; // the BusScenario being edited

  async function renderEditor() {
    if (!editing) {
      // Start from a fresh blank scenario or the first preset.
      try {
        const res = await rpc.call("scenario/list", {});
        editing = ((res && res.scenarios) || [])[0] || blankScenario();
      } catch {
        editing = blankScenario();
      }
    }
    editor.innerHTML = "";
    const name = field("Name", "input", editing.name);
    name.input.oninput = () => { editing.name = name.input.value; validate(); };
    editor.appendChild(name.row);

    const policy = field("Turn policy", "select", editing.turn_policy, ["scripted", "moderator", "roundrobin"]);
    policy.input.onchange = () => { editing.turn_policy = policy.input.value; validate(); };
    editor.appendChild(policy.row);

    // Cast.
    const castHeader = document.createElement("div");
    castHeader.textContent = "Cast";
    castHeader.style.fontWeight = "bold";
    editor.appendChild(castHeader);
    editing.members.forEach((m, i) => editor.appendChild(memberRow(m, i, validate)));

    const addMember = document.createElement("button");
    addMember.textContent = "+ member";
    addMember.onclick = () => {
      editing.members.push({ id: "m" + Date.now(), name: "", system_prompt: "", kind: "openai", model: "" });
      renderEditor();
    };
    editor.appendChild(addMember);

    // Save (validates first; rejects if invalid).
    const save = document.createElement("button");
    save.textContent = "Save";
    save.onclick = async () => {
      const res = await rpc.call("scenario/upsert", { scenario: editing });
      if (res && res.ok === false) return; // errors already shown by validate()
      loadScenarios();
    };
    editor.appendChild(save);

    const errBox = document.createElement("div");
    errBox.className = "errors";
    errBox.id = "errors";
    editor.appendChild(errBox);
    validate();
  }

  function memberRow(m, i, validate) {
    const row = document.createElement("div");
    row.className = "field";
    row.style.border = "1px solid #eee";
    const nm = document.createElement("input");
    nm.value = m.name;
    nm.placeholder = "name";
    nm.oninput = () => { m.name = nm.value; validate(); };
    const prompt = document.createElement("input");
    prompt.value = m.system_prompt;
    prompt.placeholder = "system prompt";
    prompt.oninput = () => { m.system_prompt = prompt.value; validate(); };
    const wrap = document.createElement("div");
    wrap.className = "row";
    wrap.appendChild(nm);
    wrap.appendChild(prompt);
    row.appendChild(wrap);
    const del = document.createElement("button");
    del.textContent = "remove";
    del.onclick = () => { editing.members.splice(i, 1); renderEditor(); };
    row.appendChild(del);
    return row;
  }

  async function validate() {
    if (!editing) return;
    const res = await rpc.call("scenario/validate", { scenario: editing });
    const box = document.getElementById("errors");
    if (!box) return;
    const errs = (res && res.errors) || [];
    box.innerHTML = "";
    for (const e of errs) {
      const d = document.createElement("div");
      d.textContent = e.field + ": " + e.message;
      box.appendChild(d);
    }
  }

  function blankScenario() {
    return {
      id: "sc-" + Date.now(),
      name: "",
      members: [{ id: "a", name: "", system_prompt: "", kind: "openai", model: "" }],
      turn_policy: "roundrobin",
    };
  }

  function field(label, tag, value, options) {
    const row = document.createElement("div");
    row.className = "field";
    const lab = document.createElement("label");
    lab.textContent = label;
    const input =
      tag === "select"
        ? document.createElement("select")
        : document.createElement(tag === "textarea" ? "textarea" : "input");
    if (tag === "select") {
      for (const o of options) {
        const opt = document.createElement("option");
        opt.value = o;
        opt.textContent = o;
        if (o === value) opt.selected = true;
        input.appendChild(opt);
      }
    } else {
      input.value = value || "";
    }
    row.appendChild(lab);
    row.appendChild(input);
    return { row, input };
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
