#!/usr/bin/env node
// MVS4-A 真机验收 —— 多副本编排器（Pg 共享路由表 + 每会话租约）。
// 自包含驱动：自己拉起/关停最多四个编排器进程（主对 rep-a/rep-b + 短超时
// 归档对 rep-c/rep-d + 文件模式回归实例），经控制面 HTTP + WS 反代跑验收矩阵。
//
// 验收项（20）：
//   A. 单副本 Pg 基线：banner(Backend Postgres+replica+lease) / DDL 冷启 /
//      建会话即落行且 owner=rep-a+lease 未过期 / WS 真实 turn 答暗号且
//      last_activity_ms 落库（≥1s 粒度） / DELETE 容器卷零残留+行删除
//   B. 双副本协作：B 启动异 replica id / A 持新租约时 WS 连 B → 409 +
//      x-oneai-owner-replica=rep-a / 租约过期后 B claim 成功并代理（owner
//      翻 rep-b）/ 双副本 list 收敛一致
//   C. 故障转移：kill -9 A → 租约过期 → WS 连 B 接管成功（owner=rep-b）/
//      容器 StartedAt 不变（零操作接管）/ 接管后真实 turn 答出 A 时代暗号
//   D. 并发对账：B 持活跃租约（WS 心跳中）时重启 A → 不夺租、不误判 /
//      预填过期租约后 A+B 同时重启 → 每条恰一 owner、全 Running、无 Crashed
//   E. 跨副本深度归档（rep-c/rep-d 短超时对）：C 建 s5+暗号 turn → kill C →
//      D 的 sweep 接管：自动 Hibernating → 自动 deep-archive（tar.gz×2+
//      manifest、容器+卷消失、psql archived 非空、owner=rep-d）→ WS 经 D
//      重连 Resuming → 卷恢复、真实 turn 答出归档前暗号、archived 清空
//   F. 红线与边界：archive_dir 只读 → 保持 Hibernating+卷 intact+last_error
//      诊断 → 恢复可写 → 下轮归档成功 / lease_ttl=0 + Pg DSN → 启动拒绝 /
//      文件模式回归（无 DSN）：banner file、建删会话、sessions.json 落盘
//
// 前置：镜像 oneai-engine:mvs3c（引擎本轮零改动，直接复用；或 --image 指定）、
//      Pg 容器 oneai-pg-test（pgvector/pgvector:pg16，-p 5432:5432，库
//      oneai_mvs4——run 脚本自动建）、宿主二进制带 postgres feature
//      （cargo build -p oneai-cli --features postgres）、~/.oneai/config.toml、
//      node ≥18、platforms/web/node_modules/ws、alpine:3.20（归档 helper）。
//      colima 注意：容器内到宿主 Pg 用 bridge 网关 172.17.0.1；归档目录须
//      在 colima 可挂载路径下（$HOME 或 /tmp）。
//
// 用法（仓库根目录）：
//   node deploy/docker/mvs4a_verify.mjs --bin target/debug/oneai \
//       [--lease-ttl 10] [--image oneai-engine:mvs3c] [--keep]
//
// 退出码：0 全过；1 有失败项；2 用法/前置错误。

import { createRequire } from "node:module";
import { spawn, execSync } from "node:child_process";
import { mkdtempSync, rmSync, existsSync, readdirSync, chmodSync, openSync } from "node:fs";
import { tmpdir, homedir } from "node:os";
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
const IMAGE = arg("image", "oneai-engine:mvs3c");
const LEASE_TTL = Number(arg("lease-ttl", "10"));
const KEEP = argv.includes("--keep");

const LISTEN_A = arg("url-a", "127.0.0.1:9196");
const LISTEN_B = arg("url-b", "127.0.0.1:9197");
const LISTEN_C = arg("url-c", "127.0.0.1:9198");
const LISTEN_D = arg("url-d", "127.0.0.1:9199");
const LISTEN_F = arg("url-f", "127.0.0.1:9200");

const PG_CONTAINER = arg("pg-container", "oneai-pg-test");
const DB = arg("db", "oneai_mvs4");
// 编排器进程（宿主侧）与引擎容器（bridge 网关侧）的 DSN 分开。
const DSN_ORCH = arg("dsn", `postgres://postgres:oneai@127.0.0.1:5432/${DB}`);
const DSN_CTR = arg("dsn-ctr", `postgres://postgres:oneai@172.17.0.1:5432/${DB}`);

const SECRET = `mvs4a-verify-${process.pid}-${Date.now().toString(36)}`;
const REG_A = mkdtempSync(join(tmpdir(), "mvs4a-reg-a-"));
const REG_B = mkdtempSync(join(tmpdir(), "mvs4a-reg-b-"));
const REG_F = mkdtempSync(join(tmpdir(), "mvs4a-reg-f-"));
// 归档目录必须在 docker VM 可见的宿主路径下（colima 只挂 $HOME 与 /tmp，
// macOS $TMPDIR 在 VM 侧是自动创建的空目录——见 mvs3c 首轮踩坑记录）。
const ARCHIVE_DIR = mkdtempSync(join(homedir(), ".mvs4a-archive-"));

const S1 = "mvs4a-s1"; // A 基线（用后即删）
const S2 = "mvs4a-s2"; // B：409 → 过期接管
const S3 = "mvs4a-s3"; // B 建（list 一致性）
const S4 = "mvs4a-s4"; // C：kill -9 故障转移
const S5 = "mvs4a-s5"; // E：跨副本深度归档
const S6 = "mvs4a-s6"; // F：归档失败红线
const SF = "mvs4a-f1"; // F：文件模式回归
const CONV1 = `mvs4a-conv1-${process.pid}`;
const CONV4 = `mvs4a-conv4-${process.pid}`;
const CONV5 = `mvs4a-conv5-${process.pid}`;
const PROOF1 = `MVS4A-ONE-${Date.now().toString(36).toUpperCase()}`;
const PROOF4 = `MVS4A-FOUR-${Date.now().toString(36).toUpperCase()}`;
const PROOF5 = `MVS4A-FIVE-${Date.now().toString(36).toUpperCase()}`;

const results = [];
const procs = {}; // name → child process
const logDir = `/tmp/mvs4a-logs-${process.pid}`;
execSync(`mkdir -p ${logDir}`);

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
const ownerOf = (id) => psql(`SELECT COALESCE(owner_replica,'') FROM orchestrator_sessions WHERE session_id='${id}'`);
const stateOf = (id) => psql(`SELECT COALESCE(state,'') FROM orchestrator_sessions WHERE session_id='${id}'`);
const activityOf = (id) => Number(psql(`SELECT COALESCE(last_activity_ms,0) FROM orchestrator_sessions WHERE session_id='${id}'`) || "0");
const archivedOf = (id) => psql(`SELECT CASE WHEN archived IS NULL THEN '' ELSE 'yes' END FROM orchestrator_sessions WHERE session_id='${id}'`);
const lastErrorOf = (id) => psql(`SELECT COALESCE(last_error,'') FROM orchestrator_sessions WHERE session_id='${id}'`);

function cleanRows() {
  try {
    psql(
      `DELETE FROM orchestrator_sessions WHERE session_id LIKE 'mvs4a-%';` +
      `DELETE FROM conversations_pg WHERE id LIKE 'mvs4a-%';` +
      `DELETE FROM stm_entries_pg WHERE session_id LIKE 'mvs4a-%';` +
      `DELETE FROM ltm_entries_pg WHERE id LIKE 'mvs4a-%';` +
      `DELETE FROM usage_records_pg WHERE session_id LIKE 'mvs4a-%';` +
      `DELETE FROM session_events_pg WHERE session_id LIKE 'mvs4a-%';` +
      `DELETE FROM message_feedback_pg WHERE session_id LIKE 'mvs4a-%';`
    );
  } catch {}
}

// ─── 编排器进程管理 ──────────────────────────────────────────────────────────
function startOrch(name, listen, registry, extraArgs, { env = {}, pgDsn = DSN_ORCH } = {}) {
  const logPath = join(logDir, `${name}.log`);
  const logFd = openSync(logPath, "a");
  const procEnv = {
    ...process.env,
    ONEAI_ORCHESTRATOR_SECRET: SECRET,
    ...env,
  };
  // pgDsn 为空串 = 显式清除（文件模式回归必须与外层 shell 的 DSN 隔离）。
  if (pgDsn) procEnv.ONEAI_PG_DSN = pgDsn;
  else delete procEnv.ONEAI_PG_DSN;
  const proc = spawn(BIN, [
    "orchestrator", "serve",
    "--listen", listen,
    "--registry", registry,
    "--image", IMAGE,
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

// ─── WS 客户端（同 mvs3c 协议）+ 裸握手探针（收 409 头）────────────────────
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
          resolve({
            turn_id: tc.turn_id,
            final_answer: (tc.summary?.final_answer ?? "").slice(0, 800),
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

/// 裸 WS 握手：解析 409/101，101 时返回打开的 ws（调用方负责关闭）。
function wsProbe(base, sessionId, timeoutMs = 20_000) {
  return new Promise((resolve) => {
    const ws = new WebSocket(`ws://${base}/v1/sessions/${sessionId}/ws?token=${SECRET}`, { handshakeTimeout: timeoutMs });
    const done = (v) => resolve(v);
    ws.on("unexpected-response", (_req, res) => {
      const headers = {};
      for (const [k, v] of Object.entries(res.headers ?? {})) headers[k] = v;
      try { ws.terminate(); } catch {}
      done({ status: res.statusCode, headers });
    });
    ws.on("open", () => done({ status: 101, ws }));
    ws.on("error", (e) => { try { ws.terminate(); } catch {} done({ status: 0, error: e.message }); });
  });
}

// ─── 控制面 helpers ──────────────────────────────────────────────────────────
async function createSession(base, id) {
  const r = await http(base, "/v1/sessions", {
    method: "POST",
    body: JSON.stringify({ session_id: id, env: { ONEAI_PG_DSN: DSN_CTR } }),
  });
  return { status: r.status, body: await r.json().catch(() => ({})) };
}

async function getStatus(base, id) {
  const r = await http(base, `/v1/sessions/${id}`);
  return r.json().catch(() => ({}));
}

async function listIds(base) {
  const r = await http(base, "/v1/sessions");
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
const volumesOf = (id) =>
  sh(`docker volume ls -q --filter name=oneai-orch-${id} | wc -l | tr -d ' '`);
const startedAt = (id) => sh(`docker inspect oneai-orch-${id} --format '{{.State.StartedAt}}' 2>/dev/null || true`);

// ─── 前置检查 ────────────────────────────────────────────────────────────────
function preflight() {
  try { execSync(`docker image inspect ${IMAGE}`, { stdio: "ignore" }); }
  catch { console.error(`前置失败：镜像 ${IMAGE} 不存在（MVS4-A 引擎零改动，可复用：docker build -f deploy/docker/Dockerfile -t ${IMAGE} .）`); process.exit(2); }
  try { execSync(`test -f ${join(homedir(), ".oneai", "config.toml")}`, { stdio: "ignore" }); }
  catch { console.error("前置失败：~/.oneai/config.toml 不存在"); process.exit(2); }
  try { execSync(`test -x ${BIN}`, { stdio: "ignore" }); }
  catch { console.error(`前置失败：${BIN} 不存在/不可执行（cargo build -p oneai-cli --features postgres）`); process.exit(2); }
  try { execSync(`docker inspect ${PG_CONTAINER} >/dev/null`, { stdio: "ignore" }); }
  catch { console.error(`前置失败：Pg 容器 ${PG_CONTAINER} 不存在`); process.exit(2); }
  try { psql("CREATE EXTENSION IF NOT EXISTS vector; SELECT 1;"); }
  catch (e) { console.error(`前置失败：库 ${DB} 不可用或无 pgvector：${e.message}`); process.exit(2); }
  try { sh(`docker image inspect alpine:3.20 >/dev/null 2>&1 || docker pull alpine:3.20 >/dev/null`); }
  catch (e) { console.error(`前置失败：alpine:3.20 不可得（归档 helper）：${e.message}`); process.exit(2); }
  // 归档目录对 docker VM 的可见性 canary（mvs3c 首轮踩坑）。
  try {
    sh(`echo mvs4a > "${join(ARCHIVE_DIR, ".canary")}"`);
    const vis = sh(`docker run --rm -v "${ARCHIVE_DIR}:/archive" alpine:3.20 cat /archive/.canary 2>/dev/null || true`);
    if (vis !== "mvs4a") {
      console.error(`前置失败：archive_dir 对 docker VM 不可见（${ARCHIVE_DIR}）——colima 仅挂载 $HOME 与 /tmp`);
      process.exit(2);
    }
  } catch (e) {
    console.error(`前置失败：archive_dir canary 检查异常：${e.message}`);
    process.exit(2);
  }
}

// ─── 验收阶段 ────────────────────────────────────────────────────────────────
async function phaseA() {
  const t0 = Date.now();
  const a = startOrch("a", LISTEN_A, REG_A, ["--replica-id", "rep-a", "--lease-ttl", String(LEASE_TTL)]);
  if (!await waitHealthz(LISTEN_A)) throw new Error(`orchestrator A 未就绪（log: ${a.logPath}）`);
  const log = orchLog("a");
  record("A", "A1 banner: Backend Postgres + replica rep-a + lease ttl",
    log.includes("Backend:  Postgres") && log.includes("Replica:  rep-a") && log.includes(`lease ttl ${LEASE_TTL}s`),
    "", t0);
  record("A", "A2 DDL 冷启：orchestrator_sessions + 双索引存在",
    psql(`SELECT to_regclass('orchestrator_sessions') IS NOT NULL AND to_regclass('idx_orch_sess_lease') IS NOT NULL AND to_regclass('idx_orch_sess_state_owner') IS NOT NULL`) === "t",
    "", t0);

  const t3 = Date.now();
  const r = await createSession(LISTEN_A, S1);
  const leaseFresh = psql(`SELECT owner_replica='rep-a' AND lease_expires_at > now() FROM orchestrator_sessions WHERE session_id='${S1}'`) === "t";
  record("A", "A3 POST /v1/sessions → 201 Running；落行 owner=rep-a 且租约未过期",
    r.status === 201 && r.body?.session?.state === "Running" && leaseFresh,
    r.status === 201 ? "" : JSON.stringify(r).slice(0, 300), t3);

  const t4 = Date.now();
  const client = new EngineClient(LISTEN_A, S1);
  await client.connect();
  await client.send("session/create", { id: CONV1, workspace: "/workspace" });
  const done = await client.runTurn(`不要使用任何工具，直接回答：请记住暗号 ${PROOF1} ，并逐字复述它。`);
  // 原断言假设「turn 远超一拍心跳（tick=ttl/3）→ 活动必已节流落库」——
  // provider 快时 turn 可短于一拍（MVS4-B 回归实测 2.7s < 3.3s 首拍，
  // 偶发 activity=0 假阴性）。改：先 close（LeaseGuard drop 强制 flush），
  // 再轮询等落库（异步 detached spawn，给 20s 上限）。语义不变：活动
  // 时间戳必须持久化到共享 Pg（异地副本 idle sweep 的输入）。
  client.close();
  let act = 0;
  const actDeadline = Date.now() + 20_000;
  while (Date.now() < actDeadline) {
    act = activityOf(S1);
    if (act > 0) break;
    await sleep(500);
  }
  const fresh = Math.abs(Date.now() - act) < 120_000;
  record("A", "A4 WS 经 A 真实 turn 答出暗号 + last_activity_ms 落库",
    done.final_answer?.includes(PROOF1) === true && act > 0 && fresh,
    `activity=${act} now=${Date.now()}`, t4);

  const t5 = Date.now();
  const del = await destroySession(LISTEN_A, S1);
  await sleep(1500);
  record("A", "A5 DELETE → 容器/卷零残留 + Pg 行删除",
    del === 200 && containerGone(S1) && volumesOf(S1) === "0" && stateOf(S1) === "",
    "", t5);
}

async function phaseB() {
  const t1 = Date.now();
  const b = startOrch("b", LISTEN_B, REG_B, ["--replica-id", "rep-b", "--lease-ttl", String(LEASE_TTL)]);
  if (!await waitHealthz(LISTEN_B)) throw new Error(`orchestrator B 未就绪（log: ${b.logPath}）`);
  const log = orchLog("b");
  record("B", "B1 第二副本启动：rep-b（≠rep-a）+ Backend Postgres",
    log.includes("Replica:  rep-b") && log.includes("Backend:  Postgres"), "", t1);

  const t2 = Date.now();
  const r = await createSession(LISTEN_A, S2);
  if (r.status !== 201) throw new Error(`B: create ${S2} failed ${JSON.stringify(r).slice(0, 200)}`);
  // 立即（租约 ttl 内）连 B → 409 + owner 头。
  const probe = await wsProbe(LISTEN_B, S2);
  record("B", "B2 A 持新租约时 WS 连 B → 409 + x-oneai-owner-replica=rep-a",
    probe.status === 409 && probe.headers?.["x-oneai-owner-replica"] === "rep-a",
    `status=${probe.status} hdr=${probe.headers?.["x-oneai-owner-replica"] ?? "-"}`, t2);

  const t3 = Date.now();
  // 等 A 的初始 claim 过期（A 不心跳——所有权按需 claim，不粘滞）。
  await sleep((LEASE_TTL + 3) * 1000);
  const probe2 = await wsProbe(LISTEN_B, S2);
  const okOpen = probe2.status === 101;
  // 先断言 owner 再断开：LeaseGuard drop 会立刻 release（异步）。
  const owner2 = ownerOf(S2);
  try { probe2.ws?.close(); } catch {}
  record("B", "B3 租约过期后 WS 连 B → claim 成功代理；owner 翻 rep-b",
    okOpen && owner2 === "rep-b", `status=${probe2.status} owner=${owner2}`, t3);

  const t4 = Date.now();
  const r3 = await createSession(LISTEN_B, S3);
  await sleep(500);
  const la = await listIds(LISTEN_A);
  const lb = await listIds(LISTEN_B);
  record("B", "B4 B 建会话 owner=rep-b；双副本 list 收敛一致",
    r3.status === 201 && ownerOf(S3) === "rep-b" && JSON.stringify(la) === JSON.stringify(lb) && la.includes(S2) && la.includes(S3),
    `A=${la.join(",")} B=${lb.join(",")}`, t4);
}

async function phaseC() {
  const t1 = Date.now();
  const r = await createSession(LISTEN_A, S4);
  if (r.status !== 201) throw new Error(`C: create ${S4} failed ${JSON.stringify(r).slice(0, 200)}`);
  const client = new EngineClient(LISTEN_A, S4);
  await client.connect();
  await client.send("session/create", { id: CONV4, workspace: "/workspace" });
  const done = await client.runTurn(`不要使用任何工具，直接回答：请记住暗号 ${PROOF4} ，并逐字复述它。`);
  client.close();
  const before = startedAt(S4);
  record("C", "C1 前置：经 A 建 s4 + 真实 turn 答出暗号", done.final_answer?.includes(PROOF4) === true, "", t1);

  const t2 = Date.now();
  procs.a.kill("SIGKILL"); // kill -9：无 release、无心跳——租约自然过期
  await sleep((LEASE_TTL + 3) * 1000);
  const client2 = new EngineClient(LISTEN_B, S4);
  await client2.connect(); // B claim 过期租约 → 正常代理
  record("C", "C2 kill -9 A → 租约过期 → WS 连 B 接管成功；owner=rep-b",
    ownerOf(S4) === "rep-b", `owner=${ownerOf(S4)}`, t2);

  record("C", "C3 接管零容器操作：StartedAt 不变",
    startedAt(S4) === before && before !== "", `${before} → ${startedAt(S4)}`, t2);

  const t4 = Date.now();
  const done2 = await client2.runTurn(`不要使用任何工具，直接回答：本会话早先让你记住的暗号是什么？逐字给出。`);
  record("C", "C4 接管后真实 turn 答出 A 时代暗号（引擎存活）",
    done2.final_answer?.includes(PROOF4) === true, "", t4);
  // client2 保持连接：D1 需要 B 持有活跃租约（心跳中）。
  return client2;
}

async function phaseD(client2) {
  const t1 = Date.now();
  // B 正持 s4 活跃租约（WS 连接 + ttl/3 心跳）。重启 A：
  const a2 = startOrch("a", LISTEN_A, REG_A, ["--replica-id", "rep-a", "--lease-ttl", String(LEASE_TTL)]);
  if (!await waitHealthz(LISTEN_A)) throw new Error(`orchestrator A(restart) 未就绪（log: ${a2.logPath}）`);
  await sleep(2000); // 对账完成窗口
  record("D", "D1 B 持活跃租约时重启 A：不夺租、s4 仍 Running",
    ownerOf(S4) === "rep-b" && stateOf(S4) === "Running",
    `owner=${ownerOf(S4)} state=${stateOf(S4)}`, t1);

  const t2 = Date.now();
  client2.close(); // 释放 s4 的 WS 连接（LeaseGuard drop → release）
  await sleep(1000);
  // 预填过期租约（模拟双副本都死过的孤儿），A+B 同时重启抢对账。
  psql(`UPDATE orchestrator_sessions SET owner_replica='ghost', lease_expires_at=now() - interval '1 minute' WHERE session_id IN ('${S2}','${S3}','${S4}')`);
  const before2 = startedAt(S2);
  await Promise.all([stopOrch("a", "SIGKILL"), stopOrch("b", "SIGKILL")]);
  const spawnArgs = ["--lease-ttl", String(LEASE_TTL)];
  const aStart = startOrch("a", LISTEN_A, REG_A, ["--replica-id", "rep-a", ...spawnArgs]);
  const bStart = startOrch("b", LISTEN_B, REG_B, ["--replica-id", "rep-b", ...spawnArgs]);
  const [okA, okB] = await Promise.all([waitHealthz(LISTEN_A), waitHealthz(LISTEN_B)]);
  if (!okA || !okB) throw new Error(`D: 双实例重启未就绪（${aStart.logPath} / ${bStart.logPath}）`);
  await sleep(2500); // 双对账竞态窗口
  const rows = psql(`SELECT session_id || '=' || COALESCE(owner_replica,'NULL') || '/' || state FROM orchestrator_sessions WHERE session_id IN ('${S2}','${S3}','${S4}') ORDER BY session_id`).split("\n");
  const parsed = rows.filter(Boolean).map((r) => {
    const [id, rest] = r.split("=");
    const [owner, state] = rest.split("/");
    return { id, owner, state };
  });
  const allOwnedOnce = parsed.length === 3 && parsed.every((p) => p.owner === "rep-a" || p.owner === "rep-b");
  const allRunning = parsed.every((p) => p.state === "Running");
  record("D", "D2 过期租约 + A/B 同时重启：每条恰一 owner、全 Running、无 Crashed、容器未动",
    allOwnedOnce && allRunning && startedAt(S2) === before2,
    `${parsed.map((p) => `${p.id}:${p.owner}/${p.state}`).join(" ")}`, t2);
}

async function phaseE() {
  const t0 = Date.now();
  // 短超时归档对：idle=20s deep=6s（sweep tick 30s）。idle 不能更短：
  // create→WS connect 之间会话活动时钟处于「首见宽限」，20s 宽限窗口
  // 足以避开对端副本 sweep 的误休眠竞态；连接建立后心跳+节流落盘接管。
  const archiveArgs = (rep) => ["--replica-id", rep, "--lease-ttl", String(LEASE_TTL),
    "--idle-timeout", "20", "--deep-archive-timeout", "6", "--archive-dir", ARCHIVE_DIR];
  const c = startOrch("c", LISTEN_C, mkdtempSync(join(tmpdir(), "mvs4a-reg-c-")), archiveArgs("rep-c"));
  const d = startOrch("d", LISTEN_D, mkdtempSync(join(tmpdir(), "mvs4a-reg-d-")), archiveArgs("rep-d"));
  if (!await waitHealthz(LISTEN_C) || !await waitHealthz(LISTEN_D)) throw new Error(`E: 归档对未就绪（${c.logPath}/${d.logPath}）`);

  const r = await createSession(LISTEN_C, S5);
  if (r.status !== 201) throw new Error(`E: create ${S5} failed ${JSON.stringify(r).slice(0, 200)}`);
  const client = new EngineClient(LISTEN_C, S5);
  await client.connect();
  await client.send("session/create", { id: CONV5, workspace: "/workspace" });
  const done = await client.runTurn(`不要使用任何工具，直接回答：请记住暗号 ${PROOF5} ，并逐字复述它。`);
  client.close();
  if (!record("E", "E1 前置：经 C 建 s5 + 真实 turn 答出暗号", done.final_answer?.includes(PROOF5) === true, "", t0)) {
    throw new Error("E1 failed — aborting phase E");
  }

  const t2 = Date.now();
  procs.c.kill("SIGKILL"); // C 死亡：D 的 sweep 必须接管整条休眠→归档链
  // 等 D：claim 过期租约 → idle 休眠 → 下轮 deep-archive（导出+claim+destroy）。
  let archived = false;
  const deadline = Date.now() + 240_000;
  while (Date.now() < deadline) {
    if (archivedOf(S5) === "yes" && containerGone(S5) && volumesOf(S5) === "0") { archived = true; break; }
    await sleep(3_000);
  }
  const tars = existsSync(join(ARCHIVE_DIR, S5)) ? readdirSync(join(ARCHIVE_DIR, S5)).filter((f) => f.endsWith(".tar.gz")) : [];
  const manifest = existsSync(join(ARCHIVE_DIR, S5, "manifest.json"));
  // 「是 D 干的」用 D 日志做地面真值：claim-on-act 语义下归档完成后
  // LeaseGuard drop 主动释放租约（所有权不粘滞），owner 为空是正确行为。
  const dDidIt = orchLog("d").includes("deep-archived") && orchLog("d").includes(S5);
  record("E", "E2 C 死亡 → D sweep 接管：自动休眠+归档（tar.gz×2+manifest、容器卷消失、archived 非空、D 日志自证）",
    archived && tars.length === 2 && manifest && dDidIt && stateOf(S5) === "Hibernating",
    `archived=${archivedOf(S5)} tars=${tars.length} dLog=${dDidIt} state=${stateOf(S5)}`, t2);

  const t3 = Date.now();
  const client2 = new EngineClient(LISTEN_D, S5);
  await client2.connect(); // 触发 Resuming：restore 卷 → spawn → Running
  const st = await getStatus(LISTEN_D, S5);
  // 容器是重建的全新引擎进程：先 session/load 从 Pg 还原会话，再问暗号
  //（与 mvs3c E 段同款双通道验证）。
  await client2.send("session/load", { id: CONV5 });
  const done2 = await client2.runTurn(`不要使用任何工具，直接回答：本会话早先让你记住的暗号是什么？逐字给出。`);
  client2.close();
  record("E", "E3 WS 经 D 重连 → Resuming：卷恢复、真实 turn 答出归档前暗号、archived 清空",
    st?.state === "Running" && done2.final_answer?.includes(PROOF5) === true && archivedOf(S5) === "" && volumesOf(S5) === "2",
    `state=${st?.state} archived='${archivedOf(S5)}' vols=${volumesOf(S5)}`, t3);
  // 清掉 s5，避免影响 F 的 sweep 节奏。
  await destroySession(LISTEN_D, S5);
}

async function phaseF() {
  // F1 红线：archive_dir 只读 → 归档失败但绝不 destroy；恢复可写 → 下轮成功。
  const t1 = Date.now();
  const r = await createSession(LISTEN_D, S6);
  if (r.status !== 201) throw new Error(`F: create ${S6} failed ${JSON.stringify(r).slice(0, 200)}`);
  chmodSync(ARCHIVE_DIR, 0o555);
  let redlined = false;
  const deadline = Date.now() + 180_000;
  while (Date.now() < deadline) {
    if (stateOf(S6) === "Hibernating" && lastErrorOf(S6).includes("deep-archive failed") && archivedOf(S6) === "" && volumesOf(S6) === "2") {
      redlined = true; break;
    }
    await sleep(3_000);
  }
  chmodSync(ARCHIVE_DIR, 0o755);
  let recovered = false;
  const deadline2 = Date.now() + 180_000;
  while (Date.now() < deadline2) {
    if (archivedOf(S6) === "yes" && volumesOf(S6) === "0" && containerGone(S6)) { recovered = true; break; }
    await sleep(3_000);
  }
  record("F", "F1 红线：只读归档目录 → Hibernating+卷 intact+last_error 诊断；恢复可写 → 下轮归档成功",
    redlined && recovered, `redlined=${redlined} recovered=${recovered} err='${lastErrorOf(S6).slice(0, 80)}'`, t1);

  // F2 lease_ttl=0 + Pg DSN → 启动拒绝。
  const t2 = Date.now();
  const rej = startOrch("reject", LISTEN_F, mkdtempSync(join(tmpdir(), "mvs4a-reg-r-")),
    ["--replica-id", "rep-r", "--lease-ttl", "0"]);
  const rejected = await new Promise((resolve) => {
    const timer = setTimeout(() => resolve(false), 15_000);
    rej.proc.on("exit", (code) => { clearTimeout(timer); resolve(code !== 0); });
  });
  record("F", "F2 lease_ttl=0 + Pg DSN → 启动拒绝（validate）",
    rejected && orchLog("reject").includes("lease_ttl_secs"),
    `exit-nonzero=${rejected}`, t2);

  // F3 文件模式回归：无 DSN → file backend，行为与 MVS2/3 一致。
  const t3 = Date.now();
  const f = startOrch("f", LISTEN_F, REG_F, ["--replica-id", "rep-f"], { pgDsn: "" });
  if (!await waitHealthz(LISTEN_F)) throw new Error(`F: 文件模式实例未就绪（${f.logPath}）`);
  const logF = orchLog("f");
  const rf = await createSession(LISTEN_F, SF);
  await sleep(500);
  const sessionsJson = existsSync(join(REG_F, "sessions.json")) ? sh(`cat ${join(REG_F, "sessions.json")}`) : "";
  const stF = await getStatus(LISTEN_F, SF);
  const delF = await destroySession(LISTEN_F, SF);
  await sleep(1000);
  record("F", "F3 文件模式回归：banner file、建会话 Running、sessions.json 落盘、删除零残留",
    logF.includes("Backend:  file") && rf.status === 201 && stF?.state === "Running" &&
    sessionsJson.includes(SF) && delF === 200 && containerGone(SF) && volumesOf(SF) === "0",
    `banner-file=${logF.includes("Backend:  file")} state=${stF?.state}`, t3);
}

// ─── 主流程 ──────────────────────────────────────────────────────────────────
async function main() {
  console.log(`MVS4-A 验收开始：bin=${BIN} image=${IMAGE} lease_ttl=${LEASE_TTL}s db=${DB}`);
  console.log(`  A/B=${LISTEN_A}/${LISTEN_B} · C/D=${LISTEN_C}/${LISTEN_D} · F=${LISTEN_F}`);
  console.log(`  archive=${ARCHIVE_DIR} · logs=${logDir}`);
  console.log(`  proof=${PROOF1}/${PROOF4}/${PROOF5}\n`);
  preflight();
  cleanRows();

  try {
    await phaseA();
    await phaseB();
    const client2 = await phaseC();
    await phaseD(client2);
    await phaseE();
    await phaseF();
  } catch (e) {
    console.error(`\n验收中断：${e.stack ?? e}`);
    record("FATAL", String(e.message ?? e).slice(0, 120), false);
  } finally {
    if (!KEEP) {
      console.log("\n── 清理 ──");
      for (const name of Object.keys(procs)) await stopOrch(name);
      try { sh(`${BIN} orchestrator cleanup`, { stdio: "ignore" }); } catch {}
      cleanRows();
      for (const d of [REG_A, REG_B, REG_F, ARCHIVE_DIR]) {
        try { chmodSync(d, 0o755); rmSync(d, { recursive: true, force: true }); } catch {}
      }
    } else {
      console.log(`\n--keep：编排器进程与目录保留（logs: ${logDir}）`);
    }
  }

  const failed = results.filter((r) => !r.ok);
  console.log(`\n=== MVS4-A 验收汇总：${results.length - failed.length}/${results.length} 项通过 ===`);
  if (failed.length) {
    console.log("失败项：");
    for (const f of failed) console.log(`  ❌ [${f.phase}] ${f.name}${f.detail ? ` — ${f.detail}` : ""}`);
  }
  process.exit(failed.length === 0 ? 0 : 1);
}

main();
