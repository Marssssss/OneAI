#!/usr/bin/env node
// MVS4-B 真机验收 —— 租户配额限流 + OTEL tenant/session 贯穿。
// 自包含驱动：拉起五个编排器进程（主实例 rep-m 无配额带 OTEL + 三个配额
// 实例 rep-bq/rep-cq/rep-dq + 文件模式回归 rep-f）+ 一个 OTLP 捕获桩
// (node http :4318)，经控制面 HTTP + WS 反代跑验收矩阵。
//
// 验收项（16）：
//   A. 租户基线：banner（Backend Postgres + Quotas disabled + OTEL endpoint）/
//      DDL tenant_id 列+索引 / body tenant 建会话 → snapshot+Pg 列一致 /
//      X-Oneai-Tenant 头兜底 / 非法 tenant 400 / 容器 Env 注入 4 契约变量
//   G. OTEL 贯穿（A 段会话的真实 turn 驱动）：OTLP 桩收到 span，traceId ==
//      注入 TRACEPARENT 的 trace id，resource 带 tenant.id +
//      orchestrator.session.id；用量行按租户打标（SUM>0，C 段前置）
//   B. 并发配额：max=2 → 第 3 个 429 reason=concurrent_sessions（无
//      Retry-After）；destroy 释放槽位；异租户桶独立
//   C. token 预算：--quota-max-tokens 1 + 已打标用量 → 429 reason=
//      token_budget；零用量新租户放行；banner 自证 usage 源接线
//   D. 创建限流：rate=3/min burst 6 → 恰 3 成 3 拒 + Retry-After 头
//   E. ?tenant= 列表过滤（含 default 桶匹配未打标）
//   F. 文件模式：banner file + default 桶配额生效 + 桶间独立
//   J. 文件模式持久化：sessions.json 落 tenant_id、删除零残留
//
// 前置：镜像 oneai-engine:mvs4b（引擎侧有用量打标+OTEL 改动，必须重建：
//      docker build -f deploy/docker/Dockerfile -t oneai-engine:mvs4b .）、
//      Pg 容器 oneai-pg-test（pgvector/pgvector:pg16，-p 5432:5432，库
//      oneai_mvs4b——run 脚本自动建）、宿主二进制带 postgres feature、
//      ~/.oneai/config.toml（真实 provider，暗号 turn 用）、node ≥18、
//      platforms/web/node_modules/ws。
//      colima 注意：容器内到宿主 Pg/OTLP 桩用 bridge 网关 172.17.0.1。
//
// 用法（仓库根目录）：
//   node deploy/docker/mvs4b_verify.mjs --bin target/debug/oneai \
//       [--image oneai-engine:mvs4b] [--lease-ttl 10] [--keep]
//
// 退出码：0 全过；1 有失败项；2 用法/前置错误。

import { createRequire } from "node:module";
import { spawn, execSync } from "node:child_process";
import { mkdtempSync, rmSync, existsSync, readFileSync } from "node:fs";
import { tmpdir, homedir } from "node:os";
import { join } from "node:path";
import { createServer } from "node:http";

const require = createRequire(new URL("../../platforms/web/package.json", import.meta.url));
const WebSocket = require("ws");

// ─── CLI 参数 ────────────────────────────────────────────────────────────────
const argv = process.argv.slice(2);
function arg(name, dflt) {
  const i = argv.indexOf(`--${name}`);
  return i === -1 ? dflt : argv[i + 1];
}
const BIN = arg("bin", "target/debug/oneai");
const IMAGE = arg("image", "oneai-engine:mvs4b");
const LEASE_TTL = Number(arg("lease-ttl", "10"));
const KEEP = argv.includes("--keep");

const LISTEN_M = arg("url-m", "127.0.0.1:9201"); // 主实例（无配额+OTEL）
const LISTEN_B = arg("url-b", "127.0.0.1:9202"); // 并发配额
const LISTEN_C = arg("url-c", "127.0.0.1:9203"); // token 预算
const LISTEN_D = arg("url-d", "127.0.0.1:9204"); // 创建限流
const LISTEN_F = arg("url-f", "127.0.0.1:9205"); // 文件模式回归
const OTLP_PORT = Number(arg("otlp-port", "4318"));

const PG_CONTAINER = arg("pg-container", "oneai-pg-test");
const DB = arg("db", "oneai_mvs4b");
const DSN_ORCH = arg("dsn", `postgres://postgres:oneai@127.0.0.1:5432/${DB}`);
const DSN_CTR = arg("dsn-ctr", `postgres://postgres:oneai@172.17.0.1:5432/${DB}`);
// 容器 → macOS 宿主进程的地址与容器 → published 容器端口不同：
// 172.17.0.1（bridge 网关）只达 VM 内 published 的端口（Pg 容器可用），
// macOS 宿主上的 OTLP 桩要走 lima/colima 的 VM→host 地址 192.168.5.2
// （host.lima.internal 的 IP；容器内无该 DNS，实测直连 IP 通）。
// Docker Desktop 环境改 --otel-host host.docker.internal。
const OTEL_HOST = arg("otel-host", "192.168.5.2");
const OTEL_ENDPOINT_CTR = `http://${OTEL_HOST}:${OTLP_PORT}`;

const SECRET = `mvs4b-verify-${process.pid}-${Date.now().toString(36)}`;
const REG_M = mkdtempSync(join(tmpdir(), "mvs4b-reg-m-"));
const REG_B = mkdtempSync(join(tmpdir(), "mvs4b-reg-b-"));
const REG_C = mkdtempSync(join(tmpdir(), "mvs4b-reg-c-"));
const REG_D = mkdtempSync(join(tmpdir(), "mvs4b-reg-d-"));
const REG_F = mkdtempSync(join(tmpdir(), "mvs4b-reg-f-"));

const S1 = "mvs4b-s1"; // A/G：租户基线 + 真实 turn + OTEL
const S_HDR = "mvs4b-shdr"; // A：header 兜底
const CONV1 = `mvs4b-conv1-${process.pid}`;
const PROOF1 = `MVS4B-ONE-${Date.now().toString(36).toUpperCase()}`;

const results = [];
const procs = {};
const logDir = `/tmp/mvs4b-logs-${process.pid}`;
execSync(`mkdir -p ${logDir}`);

// OTLP 捕获桩：POST /v1/traces 的 body 全量留存。
const otlpBatches = [];
let otlpServer = null;

function record(phase, name, ok, detail = "", t0 = Date.now()) {
  results.push({ phase, name, ok, detail, ms: Date.now() - t0 });
  console.log(`${ok ? "✅" : "❌"} [${phase}] ${name}${detail ? ` — ${detail}` : ""} (${Date.now() - t0}ms)`);
  return ok;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const sh = (cmd, opts = {}) => (execSync(cmd, { encoding: "utf8", ...opts }) ?? "").trim();

function http(base, path, opts = {}) {
  return fetch(`http://${base}${path}`, {
    ...opts,
    headers: { Authorization: `Bearer ${SECRET}`, "Content-Type": "application/json", ...(opts.headers ?? {}) },
    signal: AbortSignal.timeout(opts.timeout ?? 240_000),
  });
}

// ─── Pg 地面真值/清理 ────────────────────────────────────────────────────────
function psql(sql) {
  return sh(`docker exec ${PG_CONTAINER} psql -U postgres -d ${DB} -tA -c ${JSON.stringify(sql)}`);
}
const tenantOf = (id) => psql(`SELECT COALESCE(tenant_id,'') FROM orchestrator_sessions WHERE session_id='${id}'`);
const stateOf = (id) => psql(`SELECT COALESCE(state,'') FROM orchestrator_sessions WHERE session_id='${id}'`);
const usageSum = (tenant) => Number(psql(
  `SELECT COALESCE(SUM(prompt_tokens + completion_tokens),0) FROM usage_records_pg WHERE metadata_json->>'tenant_id'='${tenant}'`
) || "0");

function cleanRows() {
  // 逐条执行：冷启库上引擎侧表可能还不存在，单条失败不得连坐其他表
  // （psql -c 的多语句是单事务，一表缺失会回滚整批）。
  for (const sql of [
    `DELETE FROM orchestrator_sessions WHERE session_id LIKE 'mvs4b-%'`,
    `DELETE FROM conversations_pg WHERE id LIKE 'mvs4b-%'`,
    `DELETE FROM stm_entries_pg WHERE session_id LIKE 'mvs4b-%'`,
    `DELETE FROM ltm_entries_pg WHERE id LIKE 'mvs4b-%'`,
    `DELETE FROM usage_records_pg WHERE session_id LIKE 'mvs4b-%'`,
    `DELETE FROM session_events_pg WHERE session_id LIKE 'mvs4b-%'`,
    `DELETE FROM message_feedback_pg WHERE session_id LIKE 'mvs4b-%'`,
  ]) {
    try { psql(sql); } catch {}
  }
}

// ─── 编排器进程管理 ──────────────────────────────────────────────────────────
function startOrch(name, listen, registry, extraArgs, { env = {}, pgDsn = DSN_ORCH } = {}) {
  const logPath = join(logDir, `${name}.log`);
  const logFd = require("node:fs").openSync(logPath, "a");
  const procEnv = { ...process.env, ONEAI_ORCHESTRATOR_SECRET: SECRET, ...env };
  if (pgDsn) procEnv.ONEAI_PG_DSN = pgDsn;
  else delete procEnv.ONEAI_PG_DSN;
  const proc = spawn(BIN, [
    "orchestrator", "serve",
    "--listen", listen,
    "--registry", registry,
    "--image", IMAGE,
    "--lease-ttl", String(LEASE_TTL),
    "--provider-config", join(homedir(), ".oneai", "config.toml"),
    ...extraArgs,
  ], { env: procEnv, stdio: ["ignore", logFd, logFd] });
  proc.on("exit", (code, sig) => console.log(`[orch ${name} ${listen}] exited code=${code} sig=${sig} (log: ${logPath})`));
  procs[name] = proc;
  return { proc, logPath };
}

const orchLog = (name) => sh(`cat ${join(logDir, `${name}.log`)} 2>/dev/null || true`);

async function waitHealthz(listen, timeoutMs = 30_000) {
  const t0 = Date.now();
  while (Date.now() - t0 < timeoutMs) {
    try {
      const r = await fetch(`http://${listen}/healthz`, { signal: AbortSignal.timeout(2000) });
      if (r.ok) return true;
    } catch { /* not up yet */ }
    await sleep(300);
  }
  return false;
}

function stopOrch(name, sig = "SIGTERM") {
  return new Promise((resolve) => {
    const proc = procs[name];
    if (!proc || proc.exitCode !== null) return resolve();
    proc.once("exit", () => resolve());
    proc.kill(sig);
    setTimeout(() => { try { proc.kill("SIGKILL"); } catch {} resolve(); }, 5000).unref();
  });
}

// ─── WS 客户端（同 mvs4a 协议）──────────────────────────────────────────────
class EngineClient {
  constructor(base, sessionId, { autoApprove = true } = {}) {
    this.url = `ws://${base}/v1/sessions/${sessionId}/ws?token=${SECRET}`;
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
          resolve({ turn_id: tc.turn_id, final_answer: (tc.summary?.final_answer ?? "").slice(0, 800) });
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

// ─── 控制面 helpers ──────────────────────────────────────────────────────────
// colima/daemon 级代理注入坑：dockerd 给所有容器注入 HTTP(S)_PROXY（宿主
// 代理），NO_PROXY 默认只有 localhost/*.local —— 引擎的 OTLP POST 会被代理
// 劫持（实测 502，Pg 不受影响因为 tokio-postgres 不走 reqwest 代理）。显式
// -e（即 create 请求 env）可覆盖 daemon 注入，故这里把 OTLP 桩地址加进
// NO_PROXY。生产部署同理：collector 地址必须在容器 NO_PROXY 内（或代理可
// 达 collector）——已记入设计文档 B 轮边界 + deploy README §23。
const NO_PROXY_CTR = `${OTEL_HOST},localhost,127.0.0.1,*.local,172.17.0.1`;

async function createSession(base, id, { tenant, tenantHeader, env = {} } = {}) {
  const body = {
    session_id: id,
    env: { ONEAI_PG_DSN: DSN_CTR, NO_PROXY: NO_PROXY_CTR, no_proxy: NO_PROXY_CTR, ...env },
  };
  if (tenant !== undefined) body.tenant_id = tenant;
  const headers = tenantHeader ? { "X-Oneai-Tenant": tenantHeader } : {};
  const r = await http(base, "/v1/sessions", { method: "POST", body: JSON.stringify(body), headers });
  return { status: r.status, headers: Object.fromEntries(r.headers.entries()), body: await r.json().catch(() => ({})) };
}

async function listSessions(base, tenant) {
  const q = tenant === undefined ? "" : `?tenant=${encodeURIComponent(tenant)}`;
  const r = await http(base, `/v1/sessions${q}`);
  const j = await r.json().catch(() => ({}));
  return (j.sessions ?? []).map((s) => s.session_id).sort();
}

async function destroySession(base, id) {
  const r = await http(base, `/v1/sessions/${id}`, { method: "DELETE" });
  return r.status;
}

const containerGone = (id) => {
  try { sh(`docker inspect oneai-orch-${id} >/dev/null 2>&1`); return false; } catch { return true; }
};
const volumesOf = (id) => sh(`docker volume ls -q --filter name=oneai-orch-${id} | wc -l | tr -d ' '`);
const containerEnv = (id) => {
  try {
    const raw = sh(`docker inspect oneai-orch-${id} --format '{{json .Config.Env}}'`);
    return JSON.parse(raw); // ["K=V", ...]
  } catch { return []; }
};
const envValue = (id, key) => {
  const hit = containerEnv(id).find((kv) => kv.startsWith(`${key}=`));
  return hit === undefined ? null : hit.slice(key.length + 1);
};

// ─── OTLP 捕获桩 ─────────────────────────────────────────────────────────────
function startOtlpStub() {
  return new Promise((resolve, reject) => {
    otlpServer = createServer((req, res) => {
      if (req.method === "POST" && req.url === "/v1/traces") {
        let body = "";
        req.on("data", (c) => (body += c));
        req.on("end", () => {
          try { otlpBatches.push(JSON.parse(body)); } catch {}
          res.writeHead(200, { "Content-Type": "application/json" });
          res.end("{}");
        });
        return;
      }
      res.writeHead(404); res.end();
    });
    otlpServer.on("error", reject);
    otlpServer.listen(OTLP_PORT, "0.0.0.0", () => resolve());
  });
}

function allCapturedSpans() {
  const out = [];
  for (const b of otlpBatches) {
    for (const rs of b.resourceSpans ?? []) {
      const resource = Object.fromEntries(
        (rs.resource?.attributes ?? []).map((a) => [a.key, a.value?.stringValue ?? a.value])
      );
      for (const ss of rs.scopeSpans ?? []) {
        for (const s of ss.spans ?? []) out.push({ ...s, resource });
      }
    }
  }
  return out;
}

// ─── 前置检查 ────────────────────────────────────────────────────────────────
function preflight() {
  try { execSync(`docker image inspect ${IMAGE}`, { stdio: "ignore" }); }
  catch { console.error(`前置失败：镜像 ${IMAGE} 不存在（MVS4-B 引擎侧有改动，须重建：docker build -f deploy/docker/Dockerfile -t ${IMAGE} .）`); process.exit(2); }
  try { execSync(`test -f ${join(homedir(), ".oneai", "config.toml")}`, { stdio: "ignore" }); }
  catch { console.error("前置失败：~/.oneai/config.toml 不存在"); process.exit(2); }
  try { execSync(`test -x ${BIN}`, { stdio: "ignore" }); }
  catch { console.error(`前置失败：${BIN} 不存在/不可执行（cargo build -p oneai-cli --features postgres）`); process.exit(2); }
  try { execSync(`docker inspect ${PG_CONTAINER} >/dev/null`, { stdio: "ignore" }); }
  catch { console.error(`前置失败：Pg 容器 ${PG_CONTAINER} 不存在`); process.exit(2); }
  try { psql("CREATE EXTENSION IF NOT EXISTS vector; SELECT 1;"); }
  catch (e) { console.error(`前置失败：库 ${DB} 不可用或无 pgvector：${e.message}`); process.exit(2); }
}

// ─── 验收阶段 ────────────────────────────────────────────────────────────────
async function phaseA() {
  const t0 = Date.now();
  const m = startOrch("m", LISTEN_M, REG_M, ["--replica-id", "rep-m", "--otel-endpoint", OTEL_ENDPOINT_CTR]);
  if (!await waitHealthz(LISTEN_M)) throw new Error(`orchestrator M 未就绪（log: ${m.logPath}）`);
  const log = orchLog("m");
  record("A", "A1 banner: Postgres + rep-m + Quotas disabled + OTEL endpoint",
    log.includes("Backend:  Postgres") && log.includes("Replica:  rep-m") &&
    log.includes("Quotas: disabled") && log.includes(OTEL_ENDPOINT_CTR),
    "", t0);
  record("A", "A2 DDL: tenant_id 列 + idx_orch_sess_tenant 索引存在",
    psql(`SELECT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_name='orchestrator_sessions' AND column_name='tenant_id') AND to_regclass('idx_orch_sess_tenant') IS NOT NULL`) === "t",
    "", t0);

  const t3 = Date.now();
  const r = await createSession(LISTEN_M, S1, { tenant: "acme" });
  record("A", "A3 body tenant 建会话 → 201 + snapshot.tenant_id + Pg 列一致",
    r.status === 201 && r.body?.session?.tenant_id === "acme" && tenantOf(S1) === "acme",
    r.status === 201 ? `snap=${r.body?.session?.tenant_id} pg=${tenantOf(S1)}` : JSON.stringify(r).slice(0, 200), t3);

  const t4 = Date.now();
  const rh = await createSession(LISTEN_M, S_HDR, { tenantHeader: "hdr-tenant" });
  record("A", "A4 X-Oneai-Tenant 头兜底（body 缺省时生效）",
    rh.status === 201 && rh.body?.session?.tenant_id === "hdr-tenant" && tenantOf(S_HDR) === "hdr-tenant",
    `snap=${rh.body?.session?.tenant_id}`, t4);
  await destroySession(LISTEN_M, S_HDR);

  const t5 = Date.now();
  const rBad = await http(LISTEN_M, "/v1/sessions", {
    method: "POST",
    body: JSON.stringify({ session_id: "mvs4b-bad", tenant_id: "bad tenant!" }),
  });
  const badBody = await rBad.json().catch(() => ({}));
  record("A", "A5 非法 tenant_id → 400 invalid tenant id",
    rBad.status === 400 && String(badBody.error ?? "").includes("invalid tenant id"),
    `status=${rBad.status}`, t5);

  const t6 = Date.now();
  const envTenant = envValue(S1, "ONEAI_TENANT_ID");
  const envSid = envValue(S1, "ONEAI_ORCH_SESSION_ID");
  const envOtel = envValue(S1, "OTEL_EXPORTER_OTLP_ENDPOINT");
  const envTp = envValue(S1, "TRACEPARENT");
  record("A", "A6 容器 Env 注入 4 契约变量（tenant/orch-sid/OTLP/TRACEPARENT）",
    envTenant === "acme" && envSid === S1 && envOtel === OTEL_ENDPOINT_CTR &&
    /^00-[0-9a-f]{32}-[0-9a-f]{16}-01$/.test(envTp ?? ""),
    `tenant=${envTenant} sid=${envSid} otel=${envOtel} tp=${(envTp ?? "").slice(0, 20)}…`, t6);
  return { traceparent: envTp };
}

async function phaseG(traceparent) {
  const t0 = Date.now();
  const client = new EngineClient(LISTEN_M, S1);
  await client.connect();
  await client.send("session/create", { id: CONV1, workspace: "/workspace" });
  const done = await client.runTurn(`不要使用任何工具，直接回答：请记住暗号 ${PROOF1} ，并逐字复述它。`);
  client.close();
  record("G", "G1 前置：真实 turn 答出暗号（引擎+provider 正常）",
    done.final_answer?.includes(PROOF1) === true, "", t0);

  // 引擎 5s 周期 flush + turn 内 span end → 轮询捕获桩。
  const wantTraceId = (traceparent ?? "").split("-")[1];
  let spans = [];
  const deadline = Date.now() + 90_000;
  while (Date.now() < deadline) {
    spans = allCapturedSpans().filter((s) => s.traceId === wantTraceId);
    if (spans.length > 0) break;
    await sleep(2_000);
  }
  const resource = spans[0]?.resource ?? {};
  record("G", "G2 OTLP 桩收到引擎 span：traceId == 注入 TRACEPARENT 的 trace id",
    spans.length > 0, `spans=${spans.length} want=${wantTraceId?.slice(0, 12)}… got=${spans[0]?.traceId?.slice(0, 12) ?? "-"}`, t0);
  record("G", "G3 resource 属性带 tenant.id=acme + orchestrator.session.id",
    resource["tenant.id"] === "acme" && resource["orchestrator.session.id"] === S1,
    `tenant.id=${resource["tenant.id"] ?? "-"} sid=${resource["orchestrator.session.id"] ?? "-"}`, t0);
  record("G", "G4 导出 span 含 agent_loop（引擎主循环真贯穿，非仅资源声明）",
    spans.some((s) => s.name === "agent_loop"), `names=${[...new Set(spans.map((s) => s.name))].join(",")}`, t0);

  const t5 = Date.now();
  // 引擎侧用量打标：本 turn 的 usage 行必须带 metadata.tenant_id=acme
  // （C 段预算的地面真值；SUM>0 即打标成功——行本身由引擎写入）。
  let sum = 0;
  const deadline2 = Date.now() + 30_000;
  while (Date.now() < deadline2) {
    sum = usageSum("acme");
    if (sum > 0) break;
    await sleep(2_000);
  }
  const orchTagged = psql(`SELECT COUNT(*) FROM usage_records_pg WHERE metadata_json->>'orch_session_id'='${S1}'`);
  record("G", "G5 用量行按租户打标：SUM(acme)>0 且 orch_session_id 可回联路由表",
    sum > 0 && Number(orchTagged) > 0, `sum=${sum} rows=${orchTagged}`, t5);
}

async function phaseB() {
  const t0 = Date.now();
  const b = startOrch("b", LISTEN_B, REG_B, ["--replica-id", "rep-bq", "--quota-max-sessions", "2"]);
  if (!await waitHealthz(LISTEN_B)) throw new Error(`orchestrator B 未就绪（log: ${b.logPath}）`);

  const r1 = await createSession(LISTEN_B, "mvs4b-q1", { tenant: "qt" });
  const r2 = await createSession(LISTEN_B, "mvs4b-q2", { tenant: "qt" });
  record("B", "B1 max=2：前两个 201 Running",
    r1.status === 201 && r2.status === 201, `${r1.status}/${r2.status}`, t0);

  const t3 = Date.now();
  const r3 = await createSession(LISTEN_B, "mvs4b-q3", { tenant: "qt" });
  record("B", "B2 第 3 个 → 429 reason=concurrent_sessions（无 Retry-After）",
    r3.status === 429 && r3.body?.reason === "concurrent_sessions" &&
    r3.body?.tenant_id === "qt" && r3.body?.limit === 2 && r3.body?.current >= 2 &&
    r3.headers?.["retry-after"] === undefined,
    `status=${r3.status} body=${JSON.stringify(r3.body).slice(0, 160)}`, t3);

  const t4 = Date.now();
  await destroySession(LISTEN_B, "mvs4b-q2");
  await sleep(1500);
  const r4 = await createSession(LISTEN_B, "mvs4b-q4", { tenant: "qt" });
  record("B", "B3 destroy 释放槽位 → 再建 201",
    r4.status === 201, `status=${r4.status}`, t4);

  const t5 = Date.now();
  const r5 = await createSession(LISTEN_B, "mvs4b-q5", { tenant: "qt2" });
  record("B", "B4 异租户桶独立（qt 满员不影响 qt2）",
    r5.status === 201, `status=${r5.status}`, t5);

  for (const id of ["mvs4b-q1", "mvs4b-q4", "mvs4b-q5"]) await destroySession(LISTEN_B, id);
}

async function phaseC() {
  const t0 = Date.now();
  const c = startOrch("c", LISTEN_C, REG_C, ["--replica-id", "rep-cq", "--quota-max-tokens", "1"]);
  if (!await waitHealthz(LISTEN_C)) throw new Error(`orchestrator C 未就绪（log: ${c.logPath}）`);
  record("C", "C1 banner 自证 usage 源接线（token budget reads usage_records_pg）",
    orchLog("c").includes("token budget reads usage_records_pg"), "", t0);

  const t2 = Date.now();
  // acme 已有 G5 打标的真实用量（SUM ≥ 1）→ 预算拒绝。
  const r = await createSession(LISTEN_C, "mvs4b-c1", { tenant: "acme" });
  record("C", "C2 已耗预算租户建会话 → 429 reason=token_budget",
    r.status === 429 && r.body?.reason === "token_budget" && r.body?.tenant_id === "acme" &&
    r.body?.limit === 1 && r.body?.current >= 1 && r.headers?.["retry-after"] === undefined,
    `status=${r.status} body=${JSON.stringify(r.body).slice(0, 160)}`, t2);

  const t3 = Date.now();
  const fresh = `fresh-${Date.now().toString(36)}`;
  const r2 = await createSession(LISTEN_C, "mvs4b-c2", { tenant: fresh });
  record("C", "C3 零用量新租户放行（201）",
    r2.status === 201, `status=${r2.status}`, t3);
  await destroySession(LISTEN_C, "mvs4b-c2");
}

async function phaseD() {
  const t0 = Date.now();
  const d = startOrch("d", LISTEN_D, REG_D, ["--replica-id", "rep-dq", "--quota-rate-per-min", "3"]);
  if (!await waitHealthz(LISTEN_D)) throw new Error(`orchestrator D 未就绪（log: ${d.logPath}）`);

  // burst 6 并发（rate check 在 insert/spawn 之前——被拒的 create 零容器成本）。
  const rs = await Promise.all(
    Array.from({ length: 6 }, (_, i) => createSession(LISTEN_D, `mvs4b-r${i}`, { tenant: "rt" }))
  );
  const ok = rs.filter((r) => r.status === 201);
  const rejected = rs.filter((r) => r.status === 429);
  const allRate = rejected.every((r) => r.body?.reason === "create_rate");
  const allRetry = rejected.every((r) => Number(r.headers?.["retry-after"] ?? 0) >= 1);
  record("D", "D1 rate=3/min burst 6 → 恰 3 成 3 拒 reason=create_rate + Retry-After",
    ok.length === 3 && rejected.length === 3 && allRate && allRetry,
    `ok=${ok.length} rej=${rejected.length} rate=${allRate} retry=${allRetry}`, t0);
  for (const r of ok) await destroySession(LISTEN_D, r.body?.session?.session_id);
}

async function phaseE() {
  const t0 = Date.now();
  const r1 = await createSession(LISTEN_M, "mvs4b-e1", { tenant: "xx" });
  const r2 = await createSession(LISTEN_M, "mvs4b-e2", { tenant: "yy" });
  const r3 = await createSession(LISTEN_M, "mvs4b-e3"); // untagged → default 桶
  if (r1.status !== 201 || r2.status !== 201 || r3.status !== 201) {
    throw new Error(`E: create failed ${r1.status}/${r2.status}/${r3.status}`);
  }
  const lx = await listSessions(LISTEN_M, "xx");
  const ly = await listSessions(LISTEN_M, "yy");
  const ld = await listSessions(LISTEN_M, "default");
  const lAll = await listSessions(LISTEN_M);
  record("E", "E1 ?tenant= 过滤：xx/yy 各归各，default 匹配未打标，全量含所有",
    JSON.stringify(lx) === JSON.stringify(["mvs4b-e1"]) &&
    JSON.stringify(ly) === JSON.stringify(["mvs4b-e2"]) &&
    ld.includes("mvs4b-e3") && !ld.includes("mvs4b-e1") &&
    lAll.includes("mvs4b-e1") && lAll.includes("mvs4b-e2") && lAll.includes("mvs4b-e3") && lAll.includes(S1),
    `xx=${lx} yy=${ly} default=${ld}`, t0);
  for (const id of ["mvs4b-e1", "mvs4b-e2", "mvs4b-e3"]) await destroySession(LISTEN_M, id);
  await destroySession(LISTEN_M, S1); // A/G 用完，清场
}

async function phaseFJ() {
  const t0 = Date.now();
  const f = startOrch("f", LISTEN_F, REG_F, ["--replica-id", "rep-f", "--quota-max-sessions", "1"], { pgDsn: "" });
  if (!await waitHealthz(LISTEN_F)) throw new Error(`orchestrator F 未就绪（log: ${f.logPath}）`);
  const log = orchLog("f");
  record("F", "F1 文件模式 banner：Backend file + default 桶 max_sessions=1",
    log.includes("Backend:  file") && log.includes("max_sessions=1"), "", t0);

  const t2 = Date.now();
  const r1 = await createSession(LISTEN_F, "mvs4b-f1");
  const r2 = await createSession(LISTEN_F, "mvs4b-f2");
  record("F", "F2 未打标会话归 default 桶：第 1 个 201、第 2 个 429（file 模式配额生效）",
    r1.status === 201 && r2.status === 429 && r2.body?.reason === "concurrent_sessions" &&
    r2.body?.tenant_id === "default",
    `${r1.status}/${r2.status} tenant=${r2.body?.tenant_id}`, t2);

  const t3 = Date.now();
  const r3 = await createSession(LISTEN_F, "mvs4b-f3", { tenant: "ff" });
  record("F", "F3 命名租户桶独立于 default（ff 首个 → 201）",
    r3.status === 201, `status=${r3.status}`, t3);

  const t4 = Date.now();
  const sessionsJson = existsSync(join(REG_F, "sessions.json")) ? readFileSync(join(REG_F, "sessions.json"), "utf8") : "";
  record("J", "J1 sessions.json 落 tenant_id（ff 持久化；file 格式 serde 兼容）",
    sessionsJson.includes('"tenant_id": "ff"') && sessionsJson.includes('"tenant_id": ""'),
    "", t4);

  const t5 = Date.now();
  await destroySession(LISTEN_F, "mvs4b-f1");
  await destroySession(LISTEN_F, "mvs4b-f3");
  await sleep(1000);
  record("J", "J2 文件模式删除零残留（容器+卷）",
    containerGone("mvs4b-f1") && volumesOf("mvs4b-f1") === "0" &&
    containerGone("mvs4b-f3") && volumesOf("mvs4b-f3") === "0",
    "", t5);
}

// ─── 主流程 ──────────────────────────────────────────────────────────────────
async function main() {
  console.log(`MVS4-B 验收开始：bin=${BIN} image=${IMAGE} lease_ttl=${LEASE_TTL}s db=${DB}`);
  console.log(`  M/B/C/D/F=${LISTEN_M}/${LISTEN_B}/${LISTEN_C}/${LISTEN_D}/${LISTEN_F} · OTLP=:${OTLP_PORT}`);
  console.log(`  proof=${PROOF1} · logs=${logDir}\n`);
  preflight();
  cleanRows();
  await startOtlpStub();

  try {
    const { traceparent } = await phaseA();
    await phaseG(traceparent);
    await phaseB();
    await phaseC();
    await phaseD();
    await phaseE();
    await phaseFJ();
  } catch (e) {
    console.error(`\n验收中断：${e.stack ?? e}`);
    record("FATAL", String(e.message ?? e).slice(0, 120), false);
  } finally {
    if (!KEEP) {
      console.log("\n── 清理 ──");
      for (const name of Object.keys(procs)) await stopOrch(name);
      try { sh(`${BIN} orchestrator cleanup`, { stdio: "ignore" }); } catch {}
      cleanRows();
      for (const d of [REG_M, REG_B, REG_C, REG_D, REG_F]) {
        try { rmSync(d, { recursive: true, force: true }); } catch {}
      }
    } else {
      console.log(`\n--keep：编排器进程与目录保留（logs: ${logDir}）`);
    }
    try { otlpServer?.close(); } catch {}
  }

  const failed = results.filter((r) => !r.ok);
  console.log(`\n=== MVS4-B 验收汇总：${results.length - failed.length}/${results.length} 项通过 ===`);
  if (failed.length) {
    console.log("失败项：");
    for (const f of failed) console.log(`  ❌ [${f.phase}] ${f.name}${f.detail ? ` — ${f.detail}` : ""}`);
  }
  process.exit(failed.length === 0 ? 0 : 1);
}

main();
