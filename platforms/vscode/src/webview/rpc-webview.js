// rpc-webview.js — the webview's JSON-RPC client. The webview cannot spawn the
// engine; it posts `{kind:"rpc", id, method, params}` to the extension host
// (extension.ts forwards to the spawned `oneai app-server --listen stdio`),
// and receives `{kind:"rpc-result", id, result|error}` back. This module
// correlates requests by id and exposes a Promise-based `rpc.call()` plus an
// `rpc.onEvent(kind, cb)` for the `event` notifications the extension relays.
//
// Mirrors crates/oneai-app-server/src/protocol.rs (id: number|string, no id
// on notifications). Shared in spirit with the browser popup's native-
// messaging client — the JSON-RPC envelope is identical; only the transport
// differs (postMessage here, native messaging port there).
(function (global) {
  "use strict";

  const pending = new Map();
  let nextId = 1;
  const eventHandlers = new Map(); // kind -> Set<cb>

  function post(msg) {
    // acquireVsCodeApi is injected by the webview host; falls back to a mock
    // when loaded outside VS Code (e.g. a browser popup, for dev).
    const vs = global.acquireVsCodeApi;
    if (vs) vs().postMessage(msg);
    else global.postMessage && global.postMessage(msg, "*");
  }

  global.rpc = {
    /** Send a JSON-RPC request, resolve with `result`. */
    call(method, params) {
      const id = nextId++;
      return new Promise((resolve, reject) => {
        pending.set(id, { resolve, reject });
        post({ kind: "rpc", id, method, params });
      });
    },
    /** Subscribe to `event` notifications by params.kind. Returns unsubscriber. */
    onEvent(kind, cb) {
      if (!eventHandlers.has(kind)) eventHandlers.set(kind, new Set());
      eventHandlers.get(kind).add(cb);
      return () => eventHandlers.get(kind).delete(cb);
    },
  };

  // The host relays: {kind:"rpc-result", id, result|error} and
  // {kind:"event", eventKind, params}.
  global.addEventListener("message", (e) => {
    const msg = e.data;
    if (!msg) return;
    if (msg.kind === "rpc-result" && pending.has(msg.id)) {
      const { resolve, reject } = pending.get(msg.id);
      pending.delete(msg.id);
      if (msg.error) reject(new Error(msg.error));
      else resolve(msg.result);
    } else if (msg.kind === "event") {
      const set = eventHandlers.get(msg.eventKind);
      if (set) set.forEach((cb) => cb(msg.params));
      // Also fire a catch-all for views that want every event.
      const all = eventHandlers.get("*");
      if (all) all.forEach((cb) => cb(msg.params));
    } else if (msg.kind === "navigate") {
      // extension.ts reveals a panel with a requested initial view.
      global.dispatchEvent(new CustomEvent("navigate", { detail: msg.view }));
    }
  });
})(typeof window !== "undefined" ? window : this);
