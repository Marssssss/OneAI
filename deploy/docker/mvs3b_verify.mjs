#!/usr/bin/env node
// MVS3-B 存储外部化真机验收 —— PgMemoryStore(pgvector)/PgUsageTracker/PgHostAllowlist
// （docs/cloud-orchestrator-design.md §6 MVS3 余项）。
// 自包含驱动：自己拉起/关停编排器进程，经控制面 HTTP + WS 反代跑验收矩阵。
//
// 验收项：
//   A. POST /v1/sessions（body env 注入 ONEAI_PG_DSN）×2 → 全部 Running
//   B. 引擎容器日志出现 "memory: Postgres (shared)" + "usage: Postgres (shared)"
//      + "host-allowlist: Postgres (shared)"（三后端选择地面真值），且无回退告警
//   C. WS 真实 turn（固定会话 id）：模型复述暗号 → psql 地面真值：
//      conversations_pg 有该会话行、usage_records_pg 有该会话用量行；
//      session/list 列出它；session/rename 后 conversations_pg.title 同步
//      （trait 化 rename 走 Pg 端到端）
//   D. 跨容器白名单共享：容器1 host/allow → psql 见 host_allowlist_pg 行 →
//      容器2 host/list 直接看到（零卷共享，Pg 是唯一真相源）
//   E. docker kill + rm 受害容器 + **删光两个卷** → WS 重连自动 Resuming →
//      空卷新容器 session/list 仍见会话、session/load 回放历史、真实 turn
//      答出暗号（记忆跨容器死亡存活——只可能来自 Pg）；usage 行数继续累计
//   F. 无 pgvector 优雅降级（宿主侧）：临时起 postgres:16（无 pgvector，
//      端口 5433），带 postgres feature 的宿主二进制 `oneai app-server` 指过去
//      → stderr 出现 PgMemoryStore 连接失败告警 + 回退 SQLite memory，而
//      working-state/usage/host-allowlist 仍正常选 Pg（各 store 独立降级）
//   G. DELETE ×2 → 容器与卷零残留；psql 清验收行
//
// 前置：镜像 oneai-engine:mvs1（**用含 MVS3-B 代码的源码重建**：docker build
//      -f deploy/docker/Dockerfile -t oneai-engine:mvs1 .）、Postgres 容器
//      oneai-pg-test（**必须是 pgvector 镜像** pgvector/pgvector:pg16，
//      -p 5432:5432，库 oneai_mvs3）、宿主二进制带 postgres feature
//      （cargo build -p oneai-cli --features postgres，F 阶段用）、
//      ~/.oneai/config.toml、node ≥18、platforms/web/node_modules/ws。
//      colima 注意：容器内到宿主 Pg 用 bridge 网关 172.17.0.1。
//
// 用法（仓库根目录）：
//   node deploy/docker/mvs3b_verify.mjs --bin target/debug/oneai \
//       [--url 127.0.0.1:9193] [--dsn postgres://postgres:oneai@172.17.0.1:5432/oneai_mvs3] \
//       [--pg-container oneai-pg-test] [--image oneai-engine:mvs1] \
//       [--novec-port 5433] [--skip-novec] [--keep]
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
const LISTEN = arg("url", "127.0.0.1:9193");
const IMAGE = arg("image", "oneai-engine:mvs1");
const DSN = arg("dsn", "postgres://postgres:oneai@172.17.0.1:5432/oneai_mvs3");
const PG_CONTAINER = arg("pg-container", "oneai-pg-test");
const NOVEC_CONTAINER = "oneai-pg-novec";
const NOVEC_PORT = Number(arg("novec-port", "5433"));
const SKIP_NOVEC = argv.includes("--skip-novec");
const KEEP = argv.includes("--keep");
const BASE = `http://${LISTEN}`;
const SECRET = `mvs3b-verify-${process.pid}-${Date.now().toString(36)}`;
const REGISTRY = mkdtempSync(join(tmpdir(), "mvs3b-registry-"));

const S1 = "mvs3b-s1"; // victim：C 记忆写入 + E 杀容器删卷
const S2 = "mvs3b-s2"; // D 跨容器白名单可见性
const CONV = `mvs3b-conv-${process.pid}`; // 固定会话 id（session/create {id}）
const PROOF = `MVS3B-PROOF-${Date.now().toString(36).toUpperCase()}`;
const RENAMED = `mvs3b-renamed-${process.pid}`;
const HOST = `mvs3b-${process.pid}.example`;

const results = [];
let orch = null;
const orchLog = `/tmp/mvs3b-orchestrator-${process.pid}.log`;

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
const sh = (cmd, opts = {}) => (execSync(cmd, { encoding: "utf8", ...opts }) ?? "").trim();

// ─── Pg 地面真值/清理（宿主经 docker exec psql，无需本机 psql）──────────────
function psql(sql) {
  return sh(`docker exec ${PG_CONTAINER} psql -U postgres -d oneai_mvs3 -tA -c ${JSON.stringify(sql)}`);
}

function cleanRows() {
  try {
    psql(
      `DELETE FROM conversations_pg WHERE id LIKE '${CONV}%';` +
      `DELETE FROM stm_entries_pg WHERE session_id LIKE '${CONV}%';` +
      `DELETE FROM ltm_entries_pg WHERE id LIKE '${CONV}%';` +
      `DELETE FROM usage_records_pg WHERE session_id LIKE '${CONV}%';` +
      `DELETE FROM host_allowlist_pg WHERE host = '${HOST}';` +
      `DELETE FROM host_denylist_pg WHERE host = '${HOST}';`
    );
  } catch {}
}

// ─── 编排器进程管理（同 mvs3）───────────────────────────────────────────────
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

// ─── WS JSON-RPC 客户端（同 mvs3 协议）──────────────────────────────────────
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
  catch { console.error(`前置失败：镜像 ${IMAGE} 不存在（docker build -f deploy/docker/Dockerfile -t ${IMAGE} .）`); process.exit(2); }
  try { execSync(`test -f ${join(process.env.HOME, ".oneai", "config.toml")}`, { stdio: "ignore" }); }
  catch { console.error("前置失败：~/.oneai/config.toml 不存在"); process.exit(2); }
  try { execSync(`test -x ${BIN}`, { stdio: "ignore" }); }
  catch { console.error(`前置失败：${BIN} 不存在/不可执行（cargo build -p oneai-cli --features postgres）`); process.exit(2); }
  try { execSync(`docker inspect ${PG_CONTAINER} >/dev/null`, { stdio: "ignore" }); }
  catch { console.error(`前置失败：Pg 容器 ${PG_CONTAINER} 不存在（docker run -d --name ${PG_CONTAINER} -p 5432:5432 -e POSTGRES_PASSWORD=oneai pgvector/pgvector:pg16 + CREATE DATABASE oneai_mvs3）`); process.exit(2); }
  // pgvector 硬依赖地面真值：测试库必须能 CREATE EXTENSION vector。
  try { psql("CREATE EXTENSION IF NOT EXISTS vector; SELECT 1;"); }
  catch (e) { console.error(`前置失败：${PG_CONTAINER} 不是 pgvector 镜像（CREATE EXTENSION vector 失败）：${e.message}`); process.exit(2); }
  try { psql("SELECT 1;"); }
  catch (e) { console.error(`前置失败：oneai_mvs3 库不可达：${e.message}`); process.exit(2); }
  // B 轮表可能尚未建（引擎首连时建）；F 阶段宿主二进制要带 postgres feature。
  if (!SKIP_NOVEC) {
    try {
      const out = execSync(`"${BIN}" --help`, { encoding: "utf8" });
      if (!out.includes("app-server")) throw new Error("no app-server subcommand");
    } catch (e) { console.error(`前置失败：F 阶段需要 \`${BIN}\` 带 app-server 子命令（postgres feature 构建）：${e.message}`); process.exit(2); }
  }
}

async function createSession(id) {
  const r = await http("/v1/sessions", {
    method: "POST",
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
  // 引擎日志等三行后端选择（build_engine_server 启动行，pg_backends.rs）。
  const markers = [
    "working-state: Postgres (shared)",
    "memory: Postgres (shared)",
    "usage: Postgres (shared)",
    "host-allowlist: Postgres (shared)",
  ];
  let logs = "";
  const deadline = Date.now() + 90_000;
  while (Date.now() < deadline) {
    logs = sh(`docker logs oneai-orch-${S1} 2>&1 || true`);
    if (markers.every((m) => logs.includes(m))) break;
    await sleep(1_000);
  }
  const missing = markers.filter((m) => !logs.includes(m));
  record("B", "engine picked ALL Pg backends (memory/usage/host-allowlist/working-state)",
    missing.length === 0, missing.length ? `missing: ${missing.join(" | ")}; log tail: ${logs.slice(-400)}` : "", t0);
  const warn = /falling back to (SQLite|the file)/.test(logs) || logs.includes("without the `postgres` feature");
  record("B", "no feature-missing / fallback warning in engine log", !warn,
    warn ? `fallback warning present — image stale (rebuild with MVS3-B code)? tail: ${logs.slice(-400)}` : "", t0);
  if (missing.length || warn) throw new Error("Phase B failed — aborting");
}

async function phaseC_memoryTurnAndPgTruth() {
  const t0 = Date.now();
  const client = new EngineClient(S1);
  try {
    await client.connectRetry(120_000, S1);
    // 固定会话 id：psql 地面真值与 E 阶段 session/load 都按 CONV 定位。
    await client.send("session/create", { id: CONV, workspace: "/workspace" });
    const done = await client.runTurn(
      `不要使用任何工具，直接回答：请记住暗号 ${PROOF} ，并逐字复述它。`,
    );
    record("C", "real LLM turn via WS proxy recalls the proof string",
      done.final_answer?.includes(PROOF) === true,
      `answer=${JSON.stringify((done.final_answer ?? "").slice(0, 160))}`, t0);

    // 地面真值 #1：会话行进了共享 Pg（turn 尾自动 save_conversation）。
    await sleep(1_000); // turn_complete 后的落盘稍候
    const convRows = psql(`SELECT count(*) FROM conversations_pg WHERE id = '${CONV}';`);
    record("C", "conversation row landed in conversations_pg (shared Pg)", convRows === "1", `rows=${convRows}`, t0);

    // 地面真值 #2：ProviderPool 的用量记录进了 Pg（含 token 数 > 0）。
    const usage = psql(`SELECT count(*), COALESCE(sum(prompt_tokens + completion_tokens), 0) FROM usage_records_pg WHERE session_id = '${CONV}';`);
    const [usageRows, usageTokens] = usage.split("|").map((s) => Number(s));
    record("C", "usage records landed in usage_records_pg with real tokens",
      usageRows >= 1 && usageTokens > 0, `rows=${usageRows} tokens=${usageTokens}`, t0);

    // session/list 走 App→memory_persistence 覆写路径（Pg），能看到 CONV。
    const listed = await client.send("session/list", {});
    const found = (listed?.sessions ?? []).find((s) => s.id === CONV);
    record("C", "session/list (Pg path) includes the conversation", !!found,
      found ? `message_count=${found.message_count}` : JSON.stringify(listed?.sessions?.map((s) => s.id) ?? listed), t0);

    // session/rename → Pg 定向 UPDATE（title 列 + metadata.title）。
    await client.send("session/rename", { id: CONV, title: RENAMED });
    const title = psql(`SELECT title FROM conversations_pg WHERE id = '${CONV}';`);
    const metaTitle = psql(`SELECT metadata_json->>'title' FROM conversations_pg WHERE id = '${CONV}';`);
    record("C", "session/rename hit conversations_pg (targeted metadata UPDATE)",
      title === RENAMED && metaTitle === RENAMED, `title=${title} metadata.title=${metaTitle}`, t0);

    if (convRows !== "1") throw new Error("Phase C failed — aborting");
  } finally {
    client.close();
  }
}

async function phaseD_hostAllowlistCrossContainer() {
  const t0 = Date.now();
  const c1 = new EngineClient(S1);
  const c2 = new EngineClient(S2);
  try {
    await c1.connectRetry(60_000, S1);
    await c2.connectRetry(60_000, S2);
    // 容器1 admit（host/allow → PgHostAllowlistRpc → PgHostAllowlist）。
    await c1.send("host/allow", { host: HOST });
    const rows = psql(`SELECT count(*) FROM host_allowlist_pg WHERE host = '${HOST}';`);
    record("D", "host/allow in container 1 wrote host_allowlist_pg", rows === "1", `rows=${rows}`, t0);

    // 容器2（独立进程/独立卷）host/list 直接看到——Pg 是唯一真相源。
    const list2 = await c2.send("host/list", {});
    const hosts2 = JSON.stringify(list2 ?? {});
    record("D", "container 2 host/list sees the host (cross-container sharing via Pg)",
      hosts2.includes(HOST), hosts2.includes(HOST) ? "" : `list2=${hosts2.slice(0, 300)}`, t0);

    // 引擎侧代理读路径：容器2 的 NetworkProxy 查同一 store（is_allowed）。
    // 地面真值已由上一项覆盖（RPC 与 proxy 同表）；此处校验 deny 互斥语义。
    await c2.send("host/deny", { host: HOST });
    const allowLeft = psql(`SELECT count(*) FROM host_allowlist_pg WHERE host = '${HOST}';`);
    const denyRows = psql(`SELECT count(*) FROM host_denylist_pg WHERE host = '${HOST}';`);
    record("D", "host/deny clears the admission (mutual exclusion in one tx)",
      allowLeft === "0" && denyRows === "1", `allow=${allowLeft} deny=${denyRows}`, t0);
    // 恢复为 admit 状态（E/G 阶段清理按 allowlist 行删）。
    await c2.send("host/allow", { host: HOST });
    if (rows !== "1") throw new Error("Phase D failed — aborting");
  } finally {
    c1.close();
    c2.close();
  }
}

async function phaseE_killWipeAndRecoverMemory() {
  const t0 = Date.now();
  // 杀 + 删容器 + 删光两个卷：SQLite/文件全丢，Pg 是唯一幸存存储。
  sh(`docker rm -f oneai-orch-${S1}`, { stdio: "ignore" });
  sh(`docker volume rm -f oneai-orch-${S1}-state oneai-orch-${S1}-ws`, { stdio: "ignore" });
  const volsLeft = sh(`docker volume ls -q --filter name=oneai-orch-${S1} | wc -l`);
  record("E", "victim container + BOTH volumes wiped", volsLeft === "0", `volumes left=${volsLeft}`, t0);

  const client = new EngineClient(S1);
  try {
    await client.connectRetry(180_000, `${S1} post-wipe`);
    const st = (await (await http(`/v1/sessions/${S1}`)).json()).state;
    record("E", "reconnect after wipe → auto-Resuming (fresh empty volumes)", st === "Running", `state=${st}`, t0);

    // 地面真值：新容器卷内 SQLite 是全新的（无会话数据）。
    const dbSize = sh(`docker exec oneai-orch-${S1} sh -c 'ls -la /home/oneai/.oneai/oneai.db 2>/dev/null | wc -l'`);
    const convInSqlite = sh(
      `docker exec oneai-orch-${S1} sh -c 'command -v sqlite3 >/dev/null && sqlite3 /home/oneai/.oneai/oneai.db "SELECT count(*) FROM conversations WHERE id = \\\"${CONV}\\\";" 2>/dev/null || echo n/a'`
    );
    record("E", "new container's local SQLite has NO trace of the conversation",
      convInSqlite === "0" || convInSqlite === "n/a", `sqlite-probe=${convInSqlite} (db present=${dbSize})`, t0);

    // session/list 仍见会话——只可能来自 Pg。
    const listed = await client.send("session/list", {});
    const found = (listed?.sessions ?? []).find((s) => s.id === CONV);
    record("E", "survivor session/list recovers the conversation from Pg ALONE", !!found,
      found ? `message_count=${found.message_count} title=${found.title}` : JSON.stringify(listed?.sessions?.map((s) => s.id) ?? listed), t0);

    // session/load 回放 + 真实 turn：模型看到跨容器死亡存活的历史，答出暗号。
    await client.send("session/load", { id: CONV });
    const done = await client.runTurn("不要使用任何工具，直接回答：本会话早先让你记住的暗号是什么？逐字给出。");
    record("E", "resumed engine recalls the PROOF in a real turn (memory survived container death)",
      done.final_answer?.includes(PROOF) === true,
      `answer=${JSON.stringify((done.final_answer ?? "").slice(0, 160))}`, t0);

    // usage 连续性：新容器的 turn 用量继续记进同一张 Pg 表（累计 ≥ C 阶段）。
    await sleep(1_000);
    const usage = psql(`SELECT count(*) FROM usage_records_pg WHERE session_id = '${CONV}';`);
    record("E", "usage ledger kept accumulating across the container death",
      Number(usage) >= 2, `rows=${usage}`, t0);

    // rename 在恢复后仍存活（title 列来自 Pg）。
    const title = psql(`SELECT title FROM conversations_pg WHERE id = '${CONV}';`);
    record("E", "rename from phase C survived the wipe (Pg is the source of truth)",
      title === RENAMED, `title=${title}`, t0);
  } finally {
    client.close();
  }
}

async function phaseF_noPgvectorDegradation() {
  const t0 = Date.now();
  if (SKIP_NOVEC) {
    record("F", "no-pgvector degradation (--skip-novec)", true, "skipped", t0);
    return;
  }
  // 临时起一个**无 pgvector** 的 postgres:16（独立端口/容器），宿主侧带
  // postgres feature 的二进制指过去：PgMemoryStore 的 CREATE EXTENSION vector
  // 必然失败 → connect 报错 → 选择层告警并回退 SQLite；其余三 store 不依赖
  // pgvector，应照常选 Pg（各 store 独立降级，互不拖累）。
  try { sh(`docker rm -f ${NOVEC_CONTAINER}`, { stdio: "ignore" }); } catch {}
  sh(`docker run -d --name ${NOVEC_CONTAINER} -p ${NOVEC_PORT}:5432 -e POSTGRES_PASSWORD=oneai -e POSTGRES_DB=oneai_test postgres:16 >/dev/null`);
  try {
    for (let i = 0; i < 60; i++) {
      try { sh(`docker exec ${NOVEC_CONTAINER} pg_isready -U postgres`, { stdio: "ignore" }); break; }
      catch { await sleep(1_000); }
    }
    const novecDsn = `postgres://postgres:oneai@127.0.0.1:${NOVEC_PORT}/oneai_test`;
    const proc = spawn(BIN, ["app-server"], {
      env: { ...process.env, ONEAI_PG_DSN: novecDsn },
      stdio: ["ignore", "ignore", "pipe"],
    });
    let stderr = "";
    proc.stderr.on("data", (d) => { stderr += d.toString(); });
    // 等到启动选择序列**走完**再杀进程：memory 告警最先出现，随后
    // usage/host-allowlist/working-state 三行依次打印（首轮验收抓到的
    // 失败就是在告警出现瞬间杀进程，后三行还没打出来——脚本竞态，非产品缺陷）。
    const bootDone = () =>
      stderr.includes("PgMemoryStore connect failed")
      && stderr.includes("working-state: Postgres (shared)")
      && stderr.includes("usage: Postgres (shared)")
      && stderr.includes("host-allowlist: Postgres (shared)");
    const deadline = Date.now() + 120_000;
    while (Date.now() < deadline && !bootDone()) {
      await sleep(500);
      if (proc.exitCode !== null) break;
    }
    proc.kill("SIGTERM");
    setTimeout(() => { try { proc.kill("SIGKILL"); } catch {} }, 3000).unref();

    const memWarn = stderr.includes("PgMemoryStore connect failed")
      && stderr.includes("falling back to SQLite memory persistence");
    const memNotPg = !stderr.includes("memory: Postgres (shared)");
    const othersPg = stderr.includes("working-state: Postgres (shared)")
      && stderr.includes("usage: Postgres (shared)")
      && stderr.includes("host-allowlist: Postgres (shared)");
    record("F", "no-pgvector Pg → memory degrades to SQLite with a loud warning",
      memWarn && memNotPg, memWarn ? "" : `stderr tail: ${stderr.slice(-500)}`, t0);
    record("F", "the other three stores still select Pg (independent degradation)",
      othersPg, othersPg ? "" : `stderr tail: ${stderr.slice(-500)}`, t0);
  } finally {
    try { sh(`docker rm -f ${NOVEC_CONTAINER}`, { stdio: "ignore" }); } catch {}
  }
}

async function phaseG_teardown() {
  const t0 = Date.now();
  const dels = await Promise.all([S1, S2].map((id) =>
    http(`/v1/sessions/${id}`, { method: "DELETE" }).then((r) => r.status).catch(() => 0)));
  record("G", "DELETE both sessions → 200", dels.every((s) => s === 200), `statuses=${dels.join(",")}`, t0);
  await sleep(2_000);
  const containers = sh(`docker ps -aq --filter name=oneai-orch- | wc -l`);
  const volumes = sh(`docker volume ls -q --filter name=oneai-orch- | wc -l`);
  record("G", "no oneai-orch-* containers/volumes left", containers === "0" && volumes === "0",
    `containers=${containers} volumes=${volumes}`, t0);
  cleanRows();
  const left = psql(`SELECT (SELECT count(*) FROM conversations_pg WHERE id LIKE '${CONV}%') + (SELECT count(*) FROM usage_records_pg WHERE session_id LIKE '${CONV}%') + (SELECT count(*) FROM host_allowlist_pg WHERE host = '${HOST}');`);
  record("G", "acceptance rows cleaned from Pg", left === "0", `rows left=${left}`, t0);
}

// ─── main ────────────────────────────────────────────────────────────────────
async function main() {
  console.log(`MVS3-B 验收（PgMemoryStore/PgUsageTracker/PgHostAllowlist）：image=${IMAGE} · listen=${LISTEN}`);
  console.log(`dsn(host view)=${DSN.replace(/:[^:@/]*@/, ":***@")} · pg=${PG_CONTAINER} · conv=${CONV}`);
  console.log(`proof=${PROOF} · host=${HOST} · orchestrator log=${orchLog}\n`);
  preflight();

  try {
    startOrchestrator();
    if (!await waitHealthz()) throw new Error(`orchestrator did not come up — see ${orchLog}`);

    await phaseA_create();
    await phaseB_backendSelection();
    await phaseC_memoryTurnAndPgTruth();
    await phaseD_hostAllowlistCrossContainer();
    await phaseE_killWipeAndRecoverMemory();
    await phaseF_noPgvectorDegradation();
    await phaseG_teardown();
  } catch (e) {
    console.error(`\nABORT: ${e.message}`);
  } finally {
    await stopOrchestrator();
    if (!KEEP) {
      try { execSync(`docker rm -f $(docker ps -aq --filter name=oneai-orch-) 2>/dev/null || true`, { stdio: "ignore", shell: "/bin/bash" }); } catch {}
      try { execSync(`docker volume rm -f $(docker volume ls -q --filter name=oneai-orch-) 2>/dev/null || true`, { stdio: "ignore", shell: "/bin/bash" }); } catch {}
      try { execSync(`docker rm -f ${NOVEC_CONTAINER} 2>/dev/null || true`, { stdio: "ignore" }); } catch {}
      try { rmSync(REGISTRY, { recursive: true, force: true }); } catch {}
      cleanRows();
    }
  }

  const failed = results.filter((r) => !r.ok);
  console.log(`\n=== MVS3-B 验收汇总：${results.length - failed.length}/${results.length} 项通过 ===`);
  for (const r of results) console.log(`${r.ok ? "✅" : "❌"} [${r.phase}] ${r.name} (${r.ms}ms)${!r.ok && r.detail ? ` — ${r.detail}` : ""}`);
  process.exit(failed.length === 0 ? 0 : 1);
}

main();
