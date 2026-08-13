// rpc-native.js — the browser extension's JSON-RPC client over Chrome/Firefox
// native messaging. `chrome.runtime.connectNative('com.oneai.appserver')`
// opens a Port to the host process; the browser handles the 4-byte LE
// length-prefix framing (matching `oneai app-server --listen native-messaging`,
// Phase A's `serve_native_messaging`), and this module exposes the SAME
// Promise-based `rpc.call(method, params)` + `rpc.onEvent(kind, cb)` surface
// the VS Code webview's rpc-webview.js does — only the transport differs.
//
// The host is spawned by the browser on `connectNative` (after install-host.sh
// registered the host manifest once). No manual server start.
(function (global) {
  "use strict";

  const HOST_NAME = "com.oneai.appserver";
  const pending = new Map();
  let nextId = 1;
  let port = null;
  const eventHandlers = new Map(); // kind -> Set<cb>

  global.rpcNative = {
    /** Open the native-messaging port. Resolves on connect; rejects if the
     *  host isn't installed (user must run install-host.sh first). */
    connect() {
      return new Promise((resolve, reject) => {
        try {
          port = chrome.runtime.connectNative(HOST_NAME);
        } catch (e) {
          reject(new Error("connectNative threw: " + e.message));
          return;
        }
        if (!port) {
          const err = chrome.runtime.lastError;
          reject(
            new Error(
              "native messaging host unavailable — " +
                (err ? err.message : "run install-host.sh first"),
            ),
          );
          return;
        }
        port.onDisconnect.addListener(() => {
          const err = chrome.runtime.lastError;
          global.dispatchEvent(
            new CustomEvent("nm-disconnect", { detail: err ? err.message : "disconnected" }),
          );
          // Reject anything still pending — the responses will never arrive.
          for (const [, p] of pending) p.reject(new Error("host disconnected"));
          pending.clear();
          port = null;
        });
        port.onMessage.addListener((msg) => handleMessage(msg));
        resolve();
      });
    },
    /** Send a JSON-RPC request; resolve with result. */
    call(method, params) {
      if (!port) return Promise.reject(new Error("not connected"));
      const id = nextId++;
      return new Promise((resolve, reject) => {
        pending.set(id, { resolve, reject });
        port.postMessage({ jsonrpc: "2.0", id, method, params });
      });
    },
    onEvent(kind, cb) {
      if (!eventHandlers.has(kind)) eventHandlers.set(kind, new Set());
      eventHandlers.get(kind).add(cb);
      return () => eventHandlers.get(kind).delete(cb);
    },
    disconnect() {
      if (port) port.disconnect();
      port = null;
    },
  };

  function handleMessage(msg) {
    // Response to a pending call (has id + result/error).
    if (msg && ("id" in msg) && ("result" in msg || "error" in msg)) {
      const p = pending.get(msg.id);
      if (!p) return;
      pending.delete(msg.id);
      if (msg.error) p.reject(new Error(msg.error.message + " (code " + msg.error.code + ")"));
      else p.resolve(msg.result);
      return;
    }
    // Notification (no id) — the app-server's `event` method.
    if (msg && msg.method === "event") {
      const params = msg.params || {};
      const kind = params.kind || "";
      const set = eventHandlers.get(kind);
      if (set) set.forEach((cb) => cb(params));
      const all = eventHandlers.get("*");
      if (all) all.forEach((cb) => cb(params));
    }
  }
})(typeof window !== "undefined" ? window : this);
