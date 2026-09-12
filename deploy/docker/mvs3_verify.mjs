#!/usr/bin/env node
// MVS3 存储外部化真机验收 —— PgWorkingStateStore（docs/cloud-orchestrator-design.md §6 MVS3）。
// 自包含驱动：自己拉起/关停编排器进程，经控制面 HTTP + WS 反代跑验收矩阵。
//
// 验收项：
//   A. POST /v1/sessions（body env 注入 ONEAI_PG_DSN）×2 → 全部 Running
//      —— 编排器零代码改动：per-session env 走既有 CreateSessionRequest.env
//   B. 引擎容器启动日志出现 "working-state: Postgres (shared)"（后端选择）
//   C. 宿主 psql 种子一个未完成任务（TaskCreated+StepAdded JSONB + brief 行）
//      → 容器内 `oneai tasks list` 列出它（容器→宿主 Pg 真实读路径地面真值）
//   D. WS 反代真实 turn：新会话首轮 surface [Unfinished Work From Previous
//      Sessions]（引擎 list_open_tasks 走 Pg）→ 模型答出种子任务 goal
//   E. docker kill + rm 受害容器 + **删光两个卷** → WS 重连自动 Resuming →
//      全新空卷容器里 tasks list 仍见种子任务（卷全丢，任务零丢失——MVS3
//      核心卖点：事件日志外部化后，卷不再是恢复的必需品）
//   F. DELETE ×2 → 容器与卷零残留；psql 清种子行
//
// 前置：镜像 oneai-engine:mvs1（带 postgres feature 重建，见 Dockerfile）、
//      Postgres 容器 oneai-pg-test（-p 5432:5432，库 oneai_mvs3）、
//      ~/.oneai/config.toml、node ≥18、platforms/web/node_modules/ws。
//      colima 注意：容器内到宿主 Pg 用 bridge 网关 172.17.0.1（无
//      host.docker.internal 自动注入；DockerRunner argv 不带 --add-host）。
//
// 用法（仓库根目录）：
//   node deploy/docker/mvs3_verify.mjs --bin target/debug/oneai \
//       [--url 127.0.0.1:9192] [--dsn postgres://postgres:oneai@172.17.0.1:5432/oneai_mvs3] \
//       [--pg-container oneai-pg-test] [--image oneai-engine:mvs1] [--keep]
//
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
const LISTEN = arg("url", "127.0.0.1:9192");
const IMAGE = arg("image", "oneai-engine:mvs1");
const DSN = arg("dsn", "postgres://postgres:oneai@172.17.0.1:5432/oneai_mvs3");
const PG_CONTAINER = arg("pg-container", "oneai-pg-test");
const KEEP = argv.includes("--keep");
const BASE = `http://${LISTEN}`;
const SECRET = `mvs3-verify-${process.pid}-${Date.now().toString(36)}`;
const REGISTRY = mkdtempSync(join(tmpdir(), "mvs3-registry-"));

const S1 = "mvs3-s1"; // victim：C 读路径 + E 杀容器删卷
const S2 = "mvs3-s2"; // D 首轮 surface turn
const TASK_ID = `task_mvs3accept${process.pid.toString(36)}`;
const GOAL = `MVS3-PROOF-${Date.now().toString(36).toUpperCase()}-recover-drill`;

const results = [];
let orch = null;
const orchLog = `/tmp/mvs3-orchestrator-${process.pid}.log`;

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
// stdio:"ignore" 时 execSync 返回 null —— 归一成 ""。
const sh = (cmd, opts = {}) => (execSync(cmd, { encoding: "utf8", ...opts }) ?? "").trim();

// ─── Pg 种子/清理（宿主经 docker exec psql，无需本机 psql）─────────────────
function psql(sql) {
  return sh(`docker exec ${PG_CONTAINER} psql -U postgres -d oneai_mvs3 -tA -c ${JSON.stringify(sql)}`);
}

function seedTask() {
  // serde 线上格式 = TaskEvent 的 JSON 表示（types.rs：event_type snake_case，
  // payload tag="kind" snake_case）。种子写两事件 + brief 行；随后 C 阶段的
  // 容器内 tasks list 会真反序列化它们——格式错了会当场暴露。
  const ts1 = "2026-09-12T00:00:01+00:00";
  const ts2 = "2026-09-12T00:00:02+00:00";
  const ev1 = {
    id: `${TASK_ID}-ev1`, task_id: TASK_ID, session_id: "mvs3-seed",
    event_type: "task_created",
    payload: { kind: "task", goal: GOAL, intent: "verify pg-externalized recovery" },
    schema_version: 1, ts: ts1,
  };
  const ev2 = {
    id: `${TASK_ID}-ev2`, task_id: TASK_ID, session_id: "mvs3-seed",
    event_type: "step_added",
    payload: {
      kind: "step_added",
      step: { id: "step_1", description: "write the recovery report", status: "pending", depends_on: [], order: 1, updated_at: "" },
    },
    schema_version: 1, ts: ts2,
  };
  const q = (s) => `'${s.replace(/'/g, "''")}'`;
  psql(
    `INSERT INTO working_state_events (id, task_id, event) VALUES ` +
    `(${q(ev1.id)}, ${q(TASK_ID)}, ${q(JSON.stringify(ev1))}::text::jsonb), ` +
    `(${q(ev2.id)}, ${q(TASK_ID)}, ${q(JSON.stringify(ev2))}::text::jsonb);`
  );
  psql(
    `INSERT INTO working_state_briefs (task_id, goal, status, open_step_count, open_blocker_count, user_id, project, last_event_ts) ` +
    `VALUES (${q(TASK_ID)}, ${q(GOAL)}, 'active', 1, 0, '', '/workspace', ${q(ts2)});`
  );
}

function cleanSeed() {
  try { psql(`DELETE FROM working_state_events WHERE task_id = '${TASK_ID}'; DELETE FROM working_state_briefs WHERE task_id = '${TASK_ID}';`); } catch {}
}

// ─── 编排器进程管理（同 mvs2）───────────────────────────────────────────────
function startOrchestrator() {
  const { openSync } = require("node:fs");
  const logFd = openSync(orchLog, "a");
  orch = spawn(BIN, [
    "orchestrator", "serve",
    "--listen", LISTEN,
    "--registry", REGISTRY,
    "--image", IMAGE,
    "--idle-timeout", "3600",
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

// ─── WS JSON-RPC 客户端（同 mvs2 协议）──────────────────────────────────────
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
            final_answer: (tc.summary?.final_answer ?? "").slice(0, 800),
            elapsed_ms: Date.now() - t0,
          });
        } else if (Date.now() - t0 > timeoutMs) {
          clearInterval(timer);
          reject(new Error(`turn ${turnId} timeout ${timeoutMs}ms (events: ${this.events.map((e) => e.kind).join(",")})`));
        }
      }, 250);
    });
  }

  async runTurn(prompt, timeoutMs = 300_000) {
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
  catch { console.error("前置失败：~/.oneai/config.toml 不存在"); process.exit(2); }
  try { execSync(`test -x ${BIN}`, { stdio: "ignore" }); }
  catch { console.error(`前置失败：${BIN} 不存在/不可执行（先 cargo build -p oneai-cli）`); process.exit(2); }
  try { execSync(`docker inspect ${PG_CONTAINER} >/dev/null`, { stdio: "ignore" }); }
  catch { console.error(`前置失败：Pg 容器 ${PG_CONTAINER} 不存在（docker run -d --name ${PG_CONTAINER} -p 5432:5432 -e POSTGRES_PASSWORD=oneai postgres:16 + CREATE DATABASE oneai_mvs3）`); process.exit(2); }
  try { psql("SELECT 1;"); }
  catch (e) { console.error(`前置失败：oneai_mvs3 库不可达：${e.message}`); process.exit(2); }
}

async function createSession(id) {
  const r = await http("/v1/sessions", {
    method: "POST",
    // per-session env 注入 = 既有 CreateSessionRequest.env 字段（编排器零改动）。
    body: JSON.stringify({ session_id: id, env: { ONEAI_PG_DSN: DSN } }),
  });
  const body = await r.json().catch(() => ({}));
  return { status: r.status, body };
}

// ─── 验收阶段 ────────────────────────────────────────────────────────────────
async function phaseA_create() {
  const t0 = Date.now();
  const rs = await Promise.all([createSession(S1), createSession(S2)]);
  const ok = rs.every((r) => r.status === 201 && r.body?.session?.state === "Running");
  record("A", "2× POST /v1/sessions (env: ONEAI_PG_DSN) → 201 Running", ok,
    ok ? "" : JSON.stringify(rs.map((r) => ({ status: r.status, body: r.body }))), t0);
  if (!ok) throw new Error("Phase A failed — aborting");
}

async function phaseB_backendSelection() {
  const t0 = Date.now();
  // 引擎日志等 "working-state: Postgres (shared)"（build_engine_server 启动行）。
  let logs = "";
  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    logs = sh(`docker logs oneai-orch-${S1} 2>&1 || true`);
    if (logs.includes("working-state: Postgres (shared)")) break;
    await sleep(1_000);
  }
  record("B", "engine picked Pg backend (container log ground truth)",
    logs.includes("working-state: Postgres (shared)"),
    logs.includes("working-state: Postgres (shared)") ? "" : `log tail: ${logs.slice(-400)}`, t0);
  const warn = logs.includes("falling back to the file working-state") || logs.includes("without the `postgres` feature");
  record("B", "no feature-missing / fallback warning in engine log", !warn, warn ? "fallback warning present — image built without postgres feature?" : "", t0);
  if (!logs.includes("working-state: Postgres (shared)") || warn) throw new Error("Phase B failed — aborting");
}

async function phaseC_seedAndContainerRead() {
  const t0 = Date.now();
  seedTask();
  const seeded = psql(`SELECT count(*) FROM working_state_events WHERE task_id = '${TASK_ID}';`);
  record("C", "seed task inserted into shared Pg (2 events + brief)", seeded === "2", `events=${seeded}`, t0);

  // 容器内 CLI → 宿主 Pg：真实读路径（含 JSONB→TaskEvent serde 反序列化）。
  // docker exec 继承容器 env（ONEAI_PG_DSN 经 docker run -e 注入）；cwd=/workspace
  // = tasks list 的 project scope，与种子 brief.project 一致。
  let out = "";
  try {
    out = sh(`docker exec oneai-orch-${S1} oneai tasks list 2>&1`);
  } catch (e) {
    out = `exec failed: ${e.message}`;
  }
  record("C", "container-side `oneai tasks list` sees the seeded task (Pg read path)",
    out.includes(GOAL), out.includes(GOAL) ? "" : `output: ${out.slice(-500)}`, t0);
  if (!out.includes(GOAL)) throw new Error("Phase C failed — aborting");
}

async function phaseD_engineSurfaceTurn() {
  const t0 = Date.now();
  const client = new EngineClient(S2);
  try {
    await client.connectRetry(120_000, S2);
    await client.send("session/create", { workspace: "/workspace" });
    // 首轮 [Unfinished Work From Previous Sessions] 注入（AppSession 读
    // list_open_tasks → Pg briefs 表），模型无需工具即可答出种子 goal。
    const done = await client.runTurn(
      "不要使用任何工具，直接回答：当前有没有来自以前会话的未完成任务？如有，逐字给出它的目标（goal）字符串。",
    );
    record("D", "fresh engine surfaces unfinished task from Pg (real LLM turn via WS proxy)",
      done.final_answer?.includes(GOAL) === true,
      `answer=${JSON.stringify((done.final_answer ?? "").slice(0, 160))}`, t0);
    if (!done.final_answer?.includes(GOAL)) throw new Error("Phase D failed — aborting");
  } finally {
    client.close();
  }
}

async function phaseE_killAndWipeVolumes() {
  const t0 = Date.now();
  // 杀 + 删容器 + 删光两个卷：模拟「卷全丢」的最坏崩溃（比 MVS2 验收 C 更狠：
  // MVS2 靠同卷恢复，MVS3 要求无卷也能从 Pg 恢复未完成任务）。
  sh(`docker rm -f oneai-orch-${S1}`, { stdio: "ignore" });
  sh(`docker volume rm -f oneai-orch-${S1}-state oneai-orch-${S1}-ws`, { stdio: "ignore" });
  const volsLeft = sh(`docker volume ls -q --filter name=oneai-orch-${S1} | wc -l`);
  record("E", "victim container + BOTH volumes wiped", volsLeft === "0", `volumes left=${volsLeft}`, t0);

  // WS 重连：检死→Resuming→新容器（DockerRunner 自动 volume create，空卷）。
  const client = new EngineClient(S1);
  try {
    await client.connectRetry(180_000, `${S1} post-wipe`);
    const st = (await (await http(`/v1/sessions/${S1}`)).json()).state;
    record("E", "reconnect after wipe → auto-Resuming (fresh empty volumes)", st === "Running", `state=${st}`, t0);

    // 地面真值 #1：新容器确实是空卷（SQLite/文件 working-state 都没了）。
    const lsState = sh(`docker exec oneai-orch-${S1} sh -c 'ls -A /home/oneai/.oneai 2>/dev/null | wc -l; ls -A /workspace/.oneai/tasks 2>/dev/null | wc -l'`);
    const [stateFiles, taskFiles] = lsState.split("\n").map((s) => Number(s.trim()));
    record("E", "new container really has no file working-state (empty-volume ground truth)",
      taskFiles === 0, `state-dir entries=${stateFiles}, file tasks=${taskFiles}`, t0);

    // 地面真值 #2：容器内 tasks list 仍见种子任务——只可能来自 Pg。
    const out = sh(`docker exec oneai-orch-${S1} oneai tasks list 2>&1`);
    record("E", "survivor recovers unfinished task from Pg ALONE (no volume)",
      out.includes(GOAL), out.includes(GOAL) ? "" : `output: ${out.slice(-400)}`, t0);

    // 地面真值 #3：恢复的引擎首轮 surface 也走 Pg（新 SQLite，无历史会话）。
    await client.send("session/create", { workspace: "/workspace" });
    const done = await client.runTurn(
      "不要使用任何工具，直接回答：当前有没有来自以前会话的未完成任务？如有，逐字给出它的目标（goal）字符串。",
    );
    record("E", "resumed engine surfaces the task in a real turn (Pg list_open_tasks)",
      done.final_answer?.includes(GOAL) === true,
      `answer=${JSON.stringify((done.final_answer ?? "").slice(0, 160))}`, t0);
  } finally {
    client.close();
  }
}

async function phaseF_teardown() {
  const t0 = Date.now();
  const dels = await Promise.all([S1, S2].map((id) =>
    http(`/v1/sessions/${id}`, { method: "DELETE" }).then((r) => r.status).catch(() => 0)));
  record("F", "DELETE both sessions → 200", dels.every((s) => s === 200), `statuses=${dels.join(",")}`, t0);
  await sleep(2_000);
  const containers = sh(`docker ps -aq --filter name=oneai-orch- | wc -l`);
  const volumes = sh(`docker volume ls -q --filter name=oneai-orch- | wc -l`);
  record("F", "no oneai-orch-* containers/volumes left", containers === "0" && volumes === "0",
    `containers=${containers} volumes=${volumes}`, t0);
  cleanSeed();
  const left = psql(`SELECT count(*) FROM working_state_events WHERE task_id = '${TASK_ID}';`);
  record("F", "seed rows cleaned from Pg", left === "0", `rows left=${left}`, t0);
}

// ─── main ────────────────────────────────────────────────────────────────────
async function main() {
  console.log(`MVS3 验收（PgWorkingStateStore 存储外部化）：image=${IMAGE} · listen=${LISTEN}`);
  console.log(`dsn(host view)=${DSN.replace(/:[^:@/]*@/, ":***@")} · pg=${PG_CONTAINER} · task=${TASK_ID}`);
  console.log(`goal=${GOAL} · orchestrator log=${orchLog}\n`);
  preflight();

  try {
    startOrchestrator();
    if (!await waitHealthz()) throw new Error(`orchestrator did not come up — see ${orchLog}`);

    await phaseA_create();
    await phaseB_backendSelection();
    await phaseC_seedAndContainerRead();
    await phaseD_engineSurfaceTurn();
    await phaseE_killAndWipeVolumes();
    await phaseF_teardown();
  } catch (e) {
    console.error(`\nABORT: ${e.message}`);
  } finally {
    await stopOrchestrator();
    if (!KEEP) {
      try { execSync(`docker rm -f $(docker ps -aq --filter name=oneai-orch-) 2>/dev/null || true`, { stdio: "ignore", shell: "/bin/bash" }); } catch {}
      try { execSync(`docker volume rm -f $(docker volume ls -q --filter name=oneai-orch-) 2>/dev/null || true`, { stdio: "ignore", shell: "/bin/bash" }); } catch {}
      try { rmSync(REGISTRY, { recursive: true, force: true }); } catch {}
      cleanSeed();
    }
  }

  const failed = results.filter((r) => !r.ok);
  console.log(`\n=== MVS3 验收汇总：${results.length - failed.length}/${results.length} 项通过 ===`);
  for (const r of results) console.log(`${r.ok ? "✅" : "❌"} [${r.phase}] ${r.name} (${r.ms}ms)${!r.ok && r.detail ? ` — ${r.detail}` : ""}`);
  process.exit(failed.length === 0 ? 0 : 1);
}

main();
