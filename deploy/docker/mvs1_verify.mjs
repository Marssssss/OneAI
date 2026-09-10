#!/usr/bin/env node
// MVS1 容器化验证客户端 —— 从宿主机经 ws 驱动容器内引擎（JSON-RPC 2.0，
// 与 oneai-app-server adapter 协议一致：method 常量见 protocol.rs）。
//
// 用法（在仓库根目录跑；依赖 platforms/web/node_modules/ws）：
//   node deploy/docker/mvs1_verify.mjs ws://127.0.0.1:18787/ws list
//   node deploy/docker/mvs1_verify.mjs <url> create [--id sid] [--workspace /workspace]
//   node deploy/docker/mvs1_verify.mjs <url> load <sid>
//   node deploy/docker/mvs1_verify.mjs <url> turn "提示词" [--timeout 180] [--auto-approve] [--workspace /workspace]
//
// 退出码：0 成功；1 协议/超时错误；2 用法错误。

import { createRequire } from "node:module";
const require = createRequire(new URL("../../platforms/web/package.json", import.meta.url));
const WebSocket = require("ws");

const [url, cmd, ...rest] = process.argv.slice(2);
if (!url || !cmd) {
  console.error("usage: mvs1_verify.mjs <ws-url> <list|create|load|turn> ...");
  process.exit(2);
}

function flag(name, dflt = undefined) {
  const i = rest.indexOf(`--${name}`);
  if (i === -1) return dflt;
  return rest[i + 1];
}
function positional() {
  return rest.filter((a, i) => !a.startsWith("--") && !rest[i - 1]?.startsWith("--"));
}

const ws = new WebSocket(url);
let nextId = 1;
const pending = new Map(); // id -> {resolve, reject}
const eventLog = [];

function send(method, params) {
  const id = nextId++;
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    ws.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
  });
}

ws.on("message", (data) => {
  let msg;
  try {
    msg = JSON.parse(data.toString());
  } catch {
    return;
  }
  if (msg.method === "event") {
    eventLog.push(msg.params);
    const p = msg.params;
    // 流式进度可见化（一行一事件，chunk 只打长度防刷屏）。
    if (p.kind === "stream_chunk") {
      process.stdout.write(`.`, );
    } else {
      console.log(`\n[event] ${p.kind}${p.turn_id ? ` turn=${p.turn_id}` : ""}`);
    }
    // 审批自动应答（--auto-approve）。
    if (p.kind === "approval_request" && flag("auto-approve") !== undefined) {
      console.log(`[approve] request_id=${p.request_id} → Proceed`);
      send("approval/respond", { request_id: p.request_id, response: "Proceed" }).catch(() => {});
    }
    return;
  }
  if (msg.id !== undefined && pending.has(msg.id)) {
    const { resolve, reject } = pending.get(msg.id);
    pending.delete(msg.id);
    if (msg.error) reject(new Error(`RPC ${msg.id}: ${JSON.stringify(msg.error)}`));
    else resolve(msg.result);
  }
});

ws.on("error", (e) => {
  console.error(`\nws error: ${e.message}`);
  process.exit(1);
});

ws.on("open", async () => {
  try {
    switch (cmd) {
      case "list": {
        const r = await send("session/list", {});
        console.log(JSON.stringify(r.sessions?.map((s) => ({
          id: s.id, title: s.title, msgs: s.message_count,
        })) ?? r, null, 2));
        break;
      }
      case "create": {
        const params = {};
        if (flag("id")) params.id = flag("id");
        if (flag("workspace")) params.workspace = flag("workspace");
        const r = await send("session/create", params);
        console.log(`created: ${JSON.stringify(r)}`);
        break;
      }
      case "load": {
        const sid = positional()[0];
        if (!sid) throw new Error("load requires <session_id>");
        const r = await send("session/load", { id: sid });
        console.log(`loaded: ${JSON.stringify(r).slice(0, 400)}`);
        break;
      }
      case "turn": {
        const prompt = positional().join(" ");
        if (!prompt) throw new Error("turn requires a prompt");
        // turn/run 前可选绑定 workspace（新会话场景）。
        const r = await send("turn/run", {
          content: [{ type: "text", text: prompt }],
        });
        const turnId = r.turn_id;
        console.log(`turn_id=${turnId}`);
        const timeoutS = Number(flag("timeout", "180"));
        const done = await waitTurnComplete(turnId, timeoutS * 1000);
        const kinds = {};
        for (const e of eventLog) kinds[e.kind] = (kinds[e.kind] ?? 0) + 1;
        console.log("\n=== turn summary ===");
        console.log(JSON.stringify({ ok: true, turn_id: turnId, event_kinds: kinds, ...done }, null, 2));
        break;
      }
      default:
        throw new Error(`unknown cmd: ${cmd}`);
    }
    ws.close();
    process.exit(0);
  } catch (e) {
    console.error(`\nFAIL: ${e.message}`);
    ws.close();
    process.exit(1);
  }
});

function waitTurnComplete(turnId, timeoutMs) {
  return new Promise((resolve, reject) => {
    const t0 = Date.now();
    const timer = setInterval(() => {
      const tc = eventLog.find(
        (e) => e.kind === "turn_complete" && (!turnId || e.turn_id === turnId),
      );
      if (tc) {
        clearInterval(timer);
        const da = [...eventLog].reverse().find((e) => e.kind === "direct_answer");
        resolve({
          completed: tc.summary?.completed,
          iterations: tc.summary?.iterations,
          paradigm: tc.summary?.active_paradigm,
          final_answer: (tc.summary?.final_answer ?? da?.text ?? "").slice(0, 500),
          approvals: eventLog.filter((e) => e.kind === "approval_request").length,
          tool_calls: eventLog.filter((e) => e.kind === "tool_call").length,
          elapsed_ms: Date.now() - t0,
        });
      } else if (Date.now() - t0 > timeoutMs) {
        clearInterval(timer);
        reject(new Error(`turn ${turnId} did not complete within ${timeoutMs}ms (events so far: ${eventLog.map((e) => e.kind).join(",")})`));
      }
    }, 250);
  });
}
