#!/usr/bin/env node
// MVS2 薄编排层真机验收 —— 自包含驱动：自己拉起/重启/关停编排器进程，
// 经控制面 HTTP + WS 反代跑完整验收矩阵（docs/cloud-orchestrator-design.md §6 MVS2）。
//
// 验收项：
//   A. N 个并发会话容器稳定创建（POST /v1/sessions ×N → 全部 Running）
//   B. 每会话经编排器 WS 反代跑真实 turn（session/create + turn/run + turn_complete）
//   C. docker kill 受害会话 → 前端重连自动检死+Resuming → session/load 历史完整
//      → 恢复后的引擎答出杀容器前写入的文件内容（状态重建 G3）
//   D. 重启编排器进程 → 路由表对账 → 全部会话重挂 → WS 直连可用
//   E. DELETE ×N → 容器与卷全清
//
// 用法（仓库根目录）：
//   node deploy/docker/mvs2_verify.mjs --bin target/debug/oneai \
//       [--url 127.0.0.1:9191] [--sessions 10] [--image oneai-engine:mvs1] [--keep]
//
// 依赖：node ≥18（fetch）、platforms/web/node_modules/ws、本机 docker daemon。
// 退出码：0 全过；1 有失败项；2 用法/前置错误。

import { createRequire } from "node:module";
import { spawn, execSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const require = createRequire(new URL("../../platforms/web/package.json", import.meta.url));
const WebSocket = require("ws");

// ─── CLI 参数 ────────────────────────────────────────────────────────────────
const argv = process.argv.slice(2);
function arg(name, dflt) {
  const i = argv.indexOf(`--${name}`);
  return i === -1 ? dflt : argv[i + 1];
}
const BIN = arg("bin", "target/debug/oneai");
const LISTEN = arg("url", "127.0.0.1:9191");
const N = Number(arg("sessions", "10"));
const IMAGE = arg("image", "oneai-engine:mvs1");
const KEEP = argv.includes("--keep");
const BASE = `http://${LISTEN}`;
const SECRET = `mvs2-verify-${process.pid}-${Date.now().toString(36)}`;
const REGISTRY = mkdtempSync(join(tmpdir(), "mvs2-registry-"));
const VICTIM_IDX = Math.min(4, N - 1); // mvs2-s5（N<5 时取最后一个）
const PROOF = `MVS2-PROOF-${Date.now().toString(36).toUpperCase()}`;
const sid = (i) => `mvs2-s${i + 1}`;
const VICTIM = sid(VICTIM_IDX);

const results = []; // {phase, name, ok, detail, ms}
let orch = null;    // 编排器子进程
const orchLog = `/tmp/mvs2-orchestrator-${process.pid}.log`;

function record(phase, name, ok, detail = "", t0 = Date.now()) {
  results.push({ phase, name, ok, detail, ms: Date.now() - t0 });
  console.log(`${ok ? "✅" : "❌"} [${phase}] ${name}${detail ? ` — ${detail}` : ""} (${Date.now() - t0}ms)`);
  return ok;
}

function http(path, opts = {}) {
  return fetch(`${BASE}${path}`, {
    ...opts,
    headers: { Authorization: `Bearer ${SECRET}`, "Content-Type": "application/json", ...(opts.headers ?? {}) },
    signal: AbortSignal.timeout(opts.timeout ?? 240_000),
  });
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function mapPool(items, limit, fn) {
  const out = new Array(items.length);
  let next = 0;
  const workers = Array.from({ length: Math.min(limit, items.length) }, async () => {
    while (next < items.length) {
      const i = next++;
      out[i] = await fn(items[i], i);
    }
  });
  await Promise.all(workers);
  return out;
}

// ─── 编排器进程管理 ──────────────────────────────────────────────────────────
function startOrchestrator() {
  const { openSync } = require("node:fs");
  const logFd = openSync(orchLog, "a");
  orch = spawn(BIN, [
    "orchestrator", "serve",
    "--listen", LISTEN,
    "--registry", REGISTRY,
    "--image", IMAGE,
    "--idle-timeout", "3600", // 验收窗口内不触发 idle 休眠
    "--provider-config", join(process.env.HOME, ".oneai", "config.toml"),
  ], { env: { ...process.env, ONEAI_ORCHESTRATOR_SECRET: SECRET }, stdio: ["ignore", logFd, logFd] });
  orch.on("exit", (code, sig) => console.log(`[orchestrator] exited code=${code} sig=${sig} (log: ${orchLog})`));
}

async function waitHealthz(timeoutMs = 20_000) {
  const t0 = Date.now();
  while (Date.now() - t0 < timeoutMs) {
    try {
      const r = await fetch(`${BASE}/healthz`, { signal: AbortSignal.timeout(2000) });
      if (r.ok) return true;
    } catch { /* not up yet */ }
    await sleep(300);
  }
  return false;
}

function stopOrchestrator() {
  return new Promise((resolve) => {
    if (!orch || orch.exitCode !== null) return resolve();
    orch.once("exit", () => resolve());
    orch.kill("SIGTERM");
    setTimeout(() => { try { orch?.kill("SIGKILL"); } catch {} resolve(); }, 5000).unref();
  });
}

// ─── WS JSON-RPC 客户端（与 mvs1_verify.mjs 同协议）─────────────────────────
class EngineClient {
  constructor(sessionId, { autoApprove = true } = {}) {
    this.url = `ws://${LISTEN}/v1/sessions/${sessionId}/ws?token=${SECRET}`;
    this.autoApprove = autoApprove;
    this.events = [];
    this.pending = new Map();
    this.nextId = 1;
  }

  connect(timeoutMs = 150_000) {
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(this.url, { handshakeTimeout: timeoutMs });
      const timer = setTimeout(() => { ws.terminate(); reject(new Error(`connect timeout ${timeoutMs}ms`)); }, timeoutMs);
      ws.on("unexpected-response", (_req, res) => {
        clearTimeout(timer);
        reject(new Error(`handshake HTTP ${res.statusCode}`));
      });
      ws.on("error", (e) => { clearTimeout(timer); reject(e); });
      ws.on("open", () => {
        clearTimeout(timer);
        this.ws = ws;
        ws.on("message", (data) => this._onMessage(data));
        resolve(this);
      });
    });
  }

  async connectRetry(deadlineMs, label = "") {
    const t0 = Date.now();
    let lastErr;
    while (Date.now() - t0 < deadlineMs) {
      try { return await this.connect(Math.max(5_000, deadlineMs - (Date.now() - t0))); }
      catch (e) { lastErr = e; await sleep(3_000); }
    }
    throw new Error(`connect retry exhausted ${label}: ${lastErr?.message}`);
  }

  _onMessage(data) {
    let msg; try { msg = JSON.parse(data.toString()); } catch { return; }
    if (msg.method === "event") {
      const p = msg.params;
      this.events.push(p);
      if (p.kind === "approval_request" && this.autoApprove) {
        this.send("approval/respond", { request_id: p.request_id, response: "Proceed" }).catch(() => {});
      }
      return;
    }
    if (msg.id !== undefined && this.pending.has(msg.id)) {
      const { resolve, reject } = this.pending.get(msg.id);
      this.pending.delete(msg.id);
      if (msg.error) reject(new Error(`RPC ${msg.id}: ${JSON.stringify(msg.error)}`));
      else resolve(msg.result);
    }
  }

  send(method, params) {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.ws.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
    });
  }

  waitTurnComplete(turnId, timeoutMs) {
    return new Promise((resolve, reject) => {
      const t0 = Date.now();
      const timer = setInterval(() => {
        const tc = this.events.find((e) => e.kind === "turn_complete" && (!turnId || e.turn_id === turnId));
        if (tc) {
          clearInterval(timer);
          resolve({
            completed: tc.summary?.completed,
            iterations: tc.summary?.iterations,
            final_answer: (tc.summary?.final_answer ?? "").slice(0, 600),
            approvals: this.events.filter((e) => e.kind === "approval_request").length,
            // EngineYield serde tag="kind" snake_case: tool_calls/tool_result/tool_intent.
            tool_calls: this.events.filter((e) => e.kind === "tool_calls" || e.kind === "tool_intent").length,
            tool_results_ok: this.events.filter((e) => e.kind === "tool_result" && e.output?.success !== false).length,
            elapsed_ms: Date.now() - t0,
          });
        } else if (Date.now() - t0 > timeoutMs) {
          clearInterval(timer);
          reject(new Error(`turn ${turnId} timeout ${timeoutMs}ms (events: ${this.events.map((e) => e.kind).join(",")})`));
        }
      }, 250);
    });
  }

  async runTurn(prompt, timeoutMs = 240_000) {
    this.events = [];
    const r = await this.send("turn/run", { content: [{ type: "text", text: prompt }] });
    return this.waitTurnComplete(r.turn_id, timeoutMs);
  }

  close() { try { this.ws?.close(); } catch {} }
}

// ─── 前置检查 ────────────────────────────────────────────────────────────────
function preflight() {
  try { execSync(`docker image inspect ${IMAGE}`, { stdio: "ignore" }); }
  catch { console.error(`前置失败：镜像 ${IMAGE} 不存在（先跑 deploy/docker/README.md 的构建步骤）`); process.exit(2); }
  try { execSync(`test -f ${join(process.env.HOME, ".oneai", "config.toml")}`, { stdio: "ignore" }); }
  catch { console.error("前置失败：~/.oneai/config.toml 不存在（容器内引擎需要 provider 配置）"); process.exit(2); }
  try { execSync(`test -x ${BIN}`, { stdio: "ignore" }); }
  catch { console.error(`前置失败：${BIN} 不存在/不可执行（先 cargo build -p oneai-cli）`); process.exit(2); }
}

// ─── 验收阶段 ────────────────────────────────────────────────────────────────
async function phaseA_create() {
  // 未认证 → 401
  const t0 = Date.now();
  const unauth = await fetch(`${BASE}/v1/sessions`, { signal: AbortSignal.timeout(5000) }).catch((e) => e);
  record("A", "unauthenticated GET → 401", unauth?.status === 401, `status=${unauth?.status}`, t0);

  // N 个并发 POST
  const t1 = Date.now();
  const resps = await Promise.all(
    Array.from({ length: N }, (_, i) =>
      http("/v1/sessions", { method: "POST", body: JSON.stringify({ session_id: sid(i) }) })
        .then(async (r) => ({ i, status: r.status, body: await r.json().catch(() => ({})) }))
        .catch((e) => ({ i, status: 0, body: { error: e.message } })),
    ),
  );
  const created = resps.filter((r) => r.status === 201 && r.body?.session?.state === "Running");
  record("A", `${N} concurrent POST /v1/sessions → 201 Running`, created.length === N,
    `${created.length}/${N} created${created.length === N ? "" : `; failures: ${JSON.stringify(resps.filter((r) => r.status !== 201).map((r) => r.body))}`}`, t1);
  if (created.length !== N) throw new Error("Phase A failed — aborting");

  const list = await (await http("/v1/sessions")).json();
  record("A", "GET /v1/sessions lists all Running",
    list.sessions?.length === N && list.sessions.every((s) => s.state === "Running"),
    `${list.sessions?.length} sessions`, t1);
}

async function phaseB_turns() {
  const t0 = Date.now();
  const outcomes = await mapPool(Array.from({ length: N }, (_, i) => i), 3, async (i) => {
    const id = sid(i);
    const client = new EngineClient(id);
    try {
      await client.connectRetry(90_000, id);
      await client.send("session/create", { workspace: "/workspace" });
      const prompt = id === VICTIM
        ? `请用 write_file 把字符串 ${PROOF} 写入 /workspace/mvs2_proof.txt，然后确认。`
        : `不要使用任何工具，只回复一行：OK-${id}`;
      const done = await client.runTurn(prompt, 300_000);
      return { id, ok: true, done };
    } catch (e) {
      return { id, ok: false, err: e.message };
    } finally {
      client.close();
    }
  });

  const plain = outcomes.filter((o) => o.id !== VICTIM);
  const plainOk = plain.filter((o) => o.ok && o.done.completed !== false);
  record("B", `${plain.length} light turns through WS proxy`, plainOk.length === plain.length,
    `${plainOk.length}/${plain.length} completed${plainOk.length === plain.length ? "" : `; failures: ${JSON.stringify(plain.filter((o) => !o.ok || o.done?.completed === false).map((o) => ({ id: o.id, err: o.err ?? o.done })))}`}`, t0);

  const v = outcomes.find((o) => o.id === VICTIM);
  record("B", `victim turn wrote proof file (tool events observed)`,
    v?.ok && v.done.tool_calls >= 1 && v.done.tool_results_ok >= 1,
    v?.ok ? `tool_calls=${v.done.tool_calls} tool_results_ok=${v.done.tool_results_ok} approvals=${v.done.approvals} iterations=${v.done.iterations}` : v?.err, t0);
  // 引擎侧地面真值 #1：杀容器前，卷内文件必须真实存在且内容正确。
  const preKill = execSync(`docker exec oneai-orch-${VICTIM} cat /workspace/mvs2_proof.txt`, { encoding: "utf8" }).trim();
  record("B", `proof file exists in container workspace (docker exec ground truth)`,
    preKill === PROOF, `file=${JSON.stringify(preKill)} expected=${PROOF}`, t0);
  if (!v?.ok || preKill !== PROOF) throw new Error("Phase B victim failed — aborting");
}

async function phaseC_killResume() {
  const t0 = Date.now();
  execSync(`docker kill oneai-orch-${VICTIM}`, { stdio: "ignore" });
  await sleep(2_000);

  // 重连：首个连接触发检死→Resuming→（服务端等待就绪后）升级成功。
  const client = new EngineClient(VICTIM);
  let connected = false;
  try {
    await client.connectRetry(150_000, `${VICTIM} post-kill`);
    connected = true;
  } catch (e) {
    record("C", "reconnect after docker kill (auto-Resuming)", false, e.message, t0);
    throw new Error("Phase C failed — aborting");
  }
  record("C", "reconnect after docker kill (auto-Resuming)", connected,
    `state=${(await (await http(`/v1/sessions/${VICTIM}`)).json()).state}`, t0);

  // 引擎侧地面真值 #2：复活后的新容器挂同一卷，proof 文件仍在。
  const postResume = execSync(`docker exec oneai-orch-${VICTIM} cat /workspace/mvs2_proof.txt`, { encoding: "utf8" }).trim();
  record("C", "workspace volume survived kill+respawn (docker exec ground truth)",
    postResume === PROOF, `file=${JSON.stringify(postResume)}`, t0);

  // 历史完整：session/list 能看到杀容器前的会话。
  const list = await client.send("session/list", {});
  const convs = list.sessions ?? [];
  record("C", "session/list shows pre-kill conversation", convs.length >= 1,
    `${convs.length} conversation(s): ${convs.map((c) => c.id).join(",")}`, t0);
  if (convs.length >= 1) {
    await client.send("session/load", { id: convs[0].id });
  }

  // 状态重建：恢复后的引擎从历史答出 proof 内容（不靠工具）。
  const done = await client.runTurn(`不要使用任何工具，直接回答：我们之前写入 mvs2_proof.txt 的确切内容是什么？只输出该字符串。`, 240_000);
  const answer = done.final_answer ?? "";
  record("C", "resumed engine recalls proof from restored history", answer.includes(PROOF),
    `answer=${JSON.stringify(answer.slice(0, 120))} expected~=${PROOF}`, t0);
  client.close();
}

async function phaseD_orchestratorRestart() {
  const t0 = Date.now();
  await stopOrchestrator();
  await sleep(1_000);
  startOrchestrator();
  const up = await waitHealthz(30_000);
  record("D", "orchestrator restart → healthz", up, up ? "" : `see ${orchLog}`, t0);
  if (!up) throw new Error("Phase D failed — aborting");

  // 对账：N 个会话全部重挂为 Running（容器都活着，包括 C 阶段复活的 victim）。
  let list = { sessions: [] };
  const tPoll = Date.now();
  while (Date.now() - tPoll < 60_000) {
    list = await (await http("/v1/sessions")).json();
    if (list.sessions?.length === N && list.sessions.every((s) => s.state === "Running")) break;
    await sleep(2_000);
  }
  record("D", "routing table reconciled — all sessions re-mounted Running",
    list.sessions?.length === N && list.sessions.every((s) => s.state === "Running"),
    `${list.sessions?.filter((s) => s.state === "Running").length ?? 0}/${N} Running`, t0);

  // WS 直连立即可用（反代重挂）。
  const client = new EngineClient(sid(0));
  try {
    await client.connectRetry(60_000, "post-restart s1");
    const l = await client.send("session/list", {});
    record("D", "WS proxy works after restart (session/list)", Array.isArray(l.sessions ?? l), "", t0);
  } catch (e) {
    record("D", "WS proxy works after restart (session/list)", false, e.message, t0);
  } finally {
    client.close();
  }
}

async function phaseE_teardown() {
  const t0 = Date.now();
  const dels = await Promise.all(Array.from({ length: N }, (_, i) =>
    http(`/v1/sessions/${sid(i)}`, { method: "DELETE" })
      .then((r) => r.status).catch(() => 0)));
  record("E", "DELETE all sessions → 200", dels.every((s) => s === 200), `statuses=${dels.join(",")}`, t0);

  await sleep(2_000);
  const list = await (await http("/v1/sessions")).json();
  record("E", "routing table empty", (list.sessions?.length ?? -1) === 0, `${list.sessions?.length} left`, t0);

  const containers = execSync(`docker ps -aq --filter name=oneai-orch- | wc -l`, { encoding: "utf8" }).trim();
  const volumes = execSync(`docker volume ls -q --filter name=oneai-orch- | wc -l`, { encoding: "utf8" }).trim();
  record("E", "no oneai-orch-* containers/volumes left", containers === "0" && volumes === "0",
    `containers=${containers} volumes=${volumes}`, t0);
}

// ─── main ────────────────────────────────────────────────────────────────────
async function main() {
  console.log(`MVS2 验收：${N} 会话 · image=${IMAGE} · listen=${LISTEN} · registry=${REGISTRY}`);
  console.log(`victim=${VICTIM} · proof=${PROOF} · orchestrator log=${orchLog}\n`);
  preflight();

  try {
    startOrchestrator();
    if (!await waitHealthz()) throw new Error(`orchestrator did not come up — see ${orchLog}`);

    await phaseA_create();
    await phaseB_turns();
    await phaseC_killResume();
    await phaseD_orchestratorRestart();
    await phaseE_teardown();
  } catch (e) {
    console.error(`\nABORT: ${e.message}`);
  } finally {
    await stopOrchestrator();
    if (!KEEP) {
      // 兜底清理（阶段 E 失败/中断时）。
      try { execSync(`docker rm -f $(docker ps -aq --filter name=oneai-orch-) 2>/dev/null || true`, { stdio: "ignore", shell: "/bin/bash" }); } catch {}
      try { execSync(`docker volume rm -f $(docker volume ls -q --filter name=oneai-orch-) 2>/dev/null || true`, { stdio: "ignore", shell: "/bin/bash" }); } catch {}
      try { rmSync(REGISTRY, { recursive: true, force: true }); } catch {}
    }
  }

  const failed = results.filter((r) => !r.ok);
  console.log(`\n=== MVS2 验收汇总：${results.length - failed.length}/${results.length} 项通过 ===`);
  for (const r of results) console.log(`${r.ok ? "✅" : "❌"} [${r.phase}] ${r.name} (${r.ms}ms)${!r.ok && r.detail ? ` — ${r.detail}` : ""}`);
  process.exit(failed.length === 0 ? 0 : 1);
}

main();
