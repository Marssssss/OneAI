#!/usr/bin/env node
// MVS3-C 真机验收 —— PgSessionEventStore/PgFeedbackStore + 深度休眠卷归档 +
// 镜像瘦身/预热（docs/cloud-orchestrator-design.md §6 MVS3 余项收尾）。
// 自包含驱动：自己拉起/关停两个编排器进程（主实例 + 短超时深度归档实例），
// 经控制面 HTTP + WS 反代跑验收矩阵。
//
// 验收项：
//   A. POST /v1/sessions（env 注入 ONEAI_PG_DSN）→ Running
//   B. 引擎容器日志六行后端选择齐全（working-state/memory/usage/
//      host-allowlist/session-events/feedback: Postgres (shared)），无回退
//      告警，且出现 prewarm 行（MVS3-C 启动预热）
//   C. 真实 turn（固定会话 id）答出暗号 + feedback/submit×2 → psql 地面
//      真值 message_feedback_pg 落行、feedback/list 回读一致 +
//      session/trajectory 返回事件、psql session_events_pg 落行
//   D. 镜像瘦身地面真值：镜像尺寸 < 阈值（MVS3-B 时 497MB，去 ONNX 后应
//      显著缩小）
//   E. 深度归档全生命周期（第二编排器：idle=5s/deep=5s）：写 proof 文件 →
//      断连 → 自动 Hibernating → 自动 deep-archive（容器+两卷从 docker 消失、
//      归档目录出现 2×tar.gz+manifest.json、status.archived 带 manifest）→
//      WS 重连自动 Resuming（restore+spawn）→ 卷回来了、`docker exec cat`
//      proof 文件逐字存活、archived 标记清除、归档文件删除 → session/load +
//      真实 turn 答出归档前暗号（Pg 记忆 + 卷恢复双通道）
//   F. 归档失败红线（E 的第二个循环）：archive_dir 置只读 → 会话保持
//      Hibernating、卷一个不少、status.last_error 带 deep-archive 诊断 →
//      恢复可写 → 下一轮 sweep 归档成功 → 重连恢复 Running
//   G. S1 kill + 删光两卷 → 重连自动 Resuming → session/list 仅凭 Pg 恢复、
//      feedback/list 反馈跨卷删除存活（Pg）、session/trajectory 跨卷删除
//      存活（Pg）、真实 turn 答出暗号
//   H. DELETE 全部 → 容器/卷零残留；psql 清验收行
//
// 前置：镜像 oneai-engine:mvs3c（**用含 MVS3-C 代码的源码重建**：docker
//      build -f deploy/docker/Dockerfile -t oneai-engine:mvs3c .）、Pg 容器
//      oneai-pg-test（pgvector/pgvector:pg16，-p 5432:5432，库 oneai_mvs3）、
//      宿主二进制带 postgres feature、~/.oneai/config.toml、node ≥18、
//      platforms/web/node_modules/ws、alpine:3.20 可拉取（归档 helper）。
//      colima 注意：容器内到宿主 Pg 用 bridge 网关 172.17.0.1；归档目录须
//      在 colima 可挂载的宿主路径下（默认 /tmp 可以）。
//
// 用法（仓库根目录）：
//   node deploy/docker/mvs3c_verify.mjs --bin target/debug/oneai \
//       [--url 127.0.0.1:9194] [--url2 127.0.0.1:9195] \
//       [--dsn postgres://postgres:oneai@172.17.0.1:5432/oneai_mvs3] \
//       [--image oneai-engine:mvs3c] [--max-image-mb 400] [--keep]
//
// 退出码：0 全过；1 有失败项；2 用法/前置错误。

import { createRequire } from "node:module";
import { spawn, execSync } from "node:child_process";
import { mkdtempSync, rmSync, existsSync, readdirSync, chmodSync, writeFileSync } from "node:fs";
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
const LISTEN = arg("url", "127.0.0.1:9194");
const LISTEN2 = arg("url2", "127.0.0.1:9195");
const IMAGE = arg("image", "oneai-engine:mvs3c");
const MAX_IMAGE_MB = Number(arg("max-image-mb", "400"));
const DSN = arg("dsn", "postgres://postgres:oneai@172.17.0.1:5432/oneai_mvs3");
const PG_CONTAINER = arg("pg-container", "oneai-pg-test");
const KEEP = argv.includes("--keep");
const BASE = `http://${LISTEN}`;
const BASE2 = `http://${LISTEN2}`;
const SECRET = `mvs3c-verify-${process.pid}-${Date.now().toString(36)}`;
const REGISTRY = mkdtempSync(join(tmpdir(), "mvs3c-registry-"));
const REGISTRY2 = mkdtempSync(join(tmpdir(), "mvs3c-registry2-"));
// 归档目录必须在 docker VM 可见的宿主路径下：colima 默认只挂 $HOME 与 /tmp，
// macOS 的 $TMPDIR（/var/folders/…）在 VM 侧是自动创建的空目录——bind-mount
// 内容互不可见，tar 会在容器内失败（首轮验收 E/F 项实测踩中）。
const ARCHIVE_DIR = mkdtempSync(join(homedir(), ".mvs3c-archive-"));

const S1 = "mvs3c-s1"; // 主实例：Pg truth + G 杀容器删卷
const ARCH = "mvs3c-arch"; // 归档实例：E/F 深度归档循环
const CONV = `mvs3c-conv-${process.pid}`; // S1 引擎会话 id
const CONV2 = `mvs3c-conv2-${process.pid}`; // ARCH 引擎会话 id
const PROOF = `MVS3C-PROOF-${Date.now().toString(36).toUpperCase()}`;
const PROOF2 = `MVS3C-ARCH-${Date.now().toString(36).toUpperCase()}`;

const results = [];
let orch = null;
let orch2 = null;
const orchLog = `/tmp/mvs3c-orchestrator-${process.pid}.log`;
const orchLog2 = `/tmp/mvs3c-orchestrator2-${process.pid}.log`;

function record(phase, name, ok, detail = "", t0 = Date.now()) {
  results.push({ phase, name, ok, detail, ms: Date.now() - t0 });
  console.log(`${ok ? "✅" : "❌"} [${phase}] ${name}${detail ? ` — ${detail}` : ""} (${Date.now() - t0}ms)`);
  return ok;
}

function http(path, opts = {}, base = BASE) {
  return fetch(`${base}${path}`, {
    ...opts,
    headers: { Authorization: `Bearer ${SECRET}`, "Content-Type": "application/json", ...(opts.headers ?? {}) },
    signal: AbortSignal.timeout(opts.timeout ?? 240_000),
  });
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const sh = (cmd, opts = {}) => (execSync(cmd, { encoding: "utf8", ...opts }) ?? "").trim();

// ─── Pg 地面真值/清理 ────────────────────────────────────────────────────────
function psql(sql) {
  return sh(`docker exec ${PG_CONTAINER} psql -U postgres -d oneai_mvs3 -tA -c ${JSON.stringify(sql)}`);
}

function cleanRows() {
  try {
    psql(
      `DELETE FROM conversations_pg WHERE id LIKE 'mvs3c-%';` +
      `DELETE FROM stm_entries_pg WHERE session_id LIKE 'mvs3c-%';` +
      `DELETE FROM ltm_entries_pg WHERE id LIKE 'mvs3c-%';` +
      `DELETE FROM usage_records_pg WHERE session_id LIKE 'mvs3c-%';` +
      `DELETE FROM session_events_pg WHERE session_id LIKE 'mvs3c-%';` +
      `DELETE FROM message_feedback_pg WHERE session_id LIKE 'mvs3c-%';`
    );
  } catch {}
}

// ─── 编排器进程管理 ──────────────────────────────────────────────────────────
function startOrch(listen, registry, logPath, extraArgs, assign) {
  const { openSync } = require("node:fs");
  const logFd = openSync(logPath, "a");
  const proc = spawn(BIN, [
    "orchestrator", "serve",
    "--listen", listen,
    "--registry", registry,
    "--image", IMAGE,
    "--provider-config", join(process.env.HOME, ".oneai", "config.toml"),
    ...extraArgs,
  ], { env: { ...process.env, ONEAI_ORCHESTRATOR_SECRET: SECRET }, stdio: ["ignore", logFd, logFd] });
  proc.on("exit", (code, sig) => console.log(`[orchestrator ${listen}] exited code=${code} sig=${sig} (log: ${logPath})`));
  assign(proc);
  return proc;
}

async function waitHealthz(base, timeoutMs = 20_000) {
  const t0 = Date.now();
  while (Date.now() - t0 < timeoutMs) {
    try {
      const r = await fetch(`${base}/healthz`, { signal: AbortSignal.timeout(2000) });
      if (r.ok) return true;
    } catch { /* not up yet */ }
    await sleep(300);
  }
  return false;
}

function stopOrch(proc) {
  return new Promise((resolve) => {
    if (!proc || proc.exitCode !== null) return resolve();
    proc.once("exit", () => resolve());
    proc.kill("SIGTERM");
    setTimeout(() => { try { proc.kill("SIGKILL"); } catch {} resolve(); }, 5000).unref();
  });
}

// ─── WS JSON-RPC 客户端（同 mvs3b 协议）─────────────────────────────────────
class EngineClient {
  constructor(sessionId, { base = BASE, autoApprove = true } = {}) {
    this.url = `ws://${base.replace("http://", "")}/v1/sessions/${sessionId}/ws?token=${SECRET}`;
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
            turn_id: tc.turn_id,
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
  catch { console.error(`前置失败：Pg 容器 ${PG_CONTAINER} 不存在`); process.exit(2); }
  try { psql("CREATE EXTENSION IF NOT EXISTS vector; SELECT 1;"); }
  catch (e) { console.error(`前置失败：${PG_CONTAINER} 不是 pgvector 镜像：${e.message}`); process.exit(2); }
  // 归档 helper 镜像（离线环境需预拉取）。
  try { sh(`docker image inspect alpine:3.20 >/dev/null 2>&1 || docker pull alpine:3.20 >/dev/null`); }
  catch (e) { console.error(`前置失败：alpine:3.20 不可得（归档 helper）：${e.message}`); process.exit(2); }
  // 归档目录对 docker VM 的可见性 canary：宿主写文件 → 容器内必须读到。
  // colima 只挂 $HOME 与 /tmp；不可见时 daemon 会自动建空目录顶替 bind 源，
  // tar 产物落进 VM 侧而宿主永远看不到（首轮验收 E 项失败根因）。
  try {
    writeFileSync(join(ARCHIVE_DIR, ".canary"), "mvs3c");
    const vis = sh(`docker run --rm -v "${ARCHIVE_DIR}:/archive" alpine:3.20 cat /archive/.canary 2>/dev/null || true`);
    if (vis !== "mvs3c") {
      console.error(`前置失败：archive_dir 对 docker VM 不可见（${ARCHIVE_DIR}）——colima 仅挂载 $HOME 与 /tmp，请把归档目录放到挂载路径下`);
      process.exit(2);
    }
  } catch (e) {
    console.error(`前置失败：archive_dir canary 检查异常：${e.message}`);
    process.exit(2);
  }
}

async function createSession(base, id) {
  const r = await http("/v1/sessions", {
    method: "POST",
    body: JSON.stringify({ session_id: id, env: { ONEAI_PG_DSN: DSN } }),
  }, base);
  const body = await r.json().catch(() => ({}));
  return { status: r.status, body };
}

async function getStatus(base, id) {
  const r = await http(`/v1/sessions/${id}`, {}, base);
  return r.json().catch(() => ({}));
}

// ─── 验收阶段 ────────────────────────────────────────────────────────────────
async function phaseA_create() {
  const t0 = Date.now();
  const r = await createSession(BASE, S1);
  const ok = r.status === 201 && r.body?.session?.state === "Running";
  record("A", "POST /v1/sessions (env: ONEAI_PG_DSN) → 201 Running", ok,
    ok ? "" : JSON.stringify(r), t0);
  if (!ok) throw new Error("Phase A failed — aborting");
}

async function phaseB_backendSelection() {
  const t0 = Date.now();
  // 引擎日志等六行后端选择 + 预热行（build_engine_server 启动序列）。
  const markers = [
    "working-state: Postgres (shared)",
    "memory: Postgres (shared)",
    "usage: Postgres (shared)",
    "host-allowlist: Postgres (shared)",
    "session-events: Postgres (shared)",
    "feedback: Postgres (shared)",
  ];
  let logs = "";
  const deadline = Date.now() + 90_000;
  while (Date.now() < deadline) {
    logs = sh(`docker logs oneai-orch-${S1} 2>&1 || true`);
    if (markers.every((m) => logs.includes(m))) break;
    await sleep(1_000);
  }
  const missing = markers.filter((m) => !logs.includes(m));
  record("B", "engine picked ALL SIX Pg backends (+session-events/+feedback)",
    missing.length === 0, missing.length ? `missing: ${missing.join(" | ")}; log tail: ${logs.slice(-400)}` : "", t0);
  const warn = /falling back to (SQLite|the file)/.test(logs) || logs.includes("without the `postgres` feature");
  record("B", "no feature-missing / fallback warning in engine log", !warn,
    warn ? `fallback warning present — image stale? tail: ${logs.slice(-400)}` : "", t0);
  const prewarm = logs.includes("prewarm: model context ready");
  record("B", "startup prewarm ran before the port bound (MVS3-C)", prewarm,
    prewarm ? "" : `no prewarm line; tail: ${logs.slice(-300)}`, t0);
  if (missing.length || warn) throw new Error("Phase B failed — aborting");
}

async function phaseC_feedbackTrajectoryPgTruth() {
  const t0 = Date.now();
  const client = new EngineClient(S1);
  try {
    await client.connectRetry(120_000, S1);
    await client.send("session/create", { id: CONV, workspace: "/workspace" });
    const done = await client.runTurn(
      `不要使用任何工具，直接回答：请记住暗号 ${PROOF} ，并逐字复述它。`,
    );
    record("C", "real LLM turn via WS proxy recalls the proof string",
      done.final_answer?.includes(PROOF) === true,
      `answer=${JSON.stringify((done.final_answer ?? "").slice(0, 160))}`, t0);

    // feedback/submit ×2（up + note）→ Pg 落行 → feedback/list 回读。
    const turnId = done.turn_id ?? "t-mvs3c";
    await client.send("feedback/submit", { session_id: CONV, turn_id: turnId, kind: "up" });
    await client.send("feedback/submit", {
      session_id: CONV, turn_id: turnId, message_role: "assistant", kind: "note", text: "mvs3c 验收备注",
    });
    await sleep(500);
    const fbRows = psql(`SELECT count(*) FROM message_feedback_pg WHERE session_id = '${CONV}';`);
    record("C", "feedback/submit landed rows in message_feedback_pg (shared Pg)", fbRows === "2", `rows=${fbRows}`, t0);
    const fbList = await client.send("feedback/list", { session_id: CONV });
    const entries = fbList?.entries ?? fbList ?? [];
    const kinds = JSON.stringify(entries);
    record("C", "feedback/list reads both entries back (up + note with text)",
      kinds.includes('"up"') && kinds.includes("mvs3c 验收备注"), kinds.slice(0, 300), t0);

    // session/trajectory → 事件走 PgSessionEventStore；psql 地面真值。
    await sleep(1_000); // 事件 tap 是异步落盘
    const traj = await client.send("session/trajectory", { id: CONV });
    const evCount = (traj?.events ?? []).length;
    record("C", "session/trajectory replays events from Pg", traj?.ok === true && evCount > 0,
      `events=${evCount}`, t0);
    const evRows = psql(`SELECT count(*) FROM session_events_pg WHERE session_id = '${CONV}';`);
    record("C", "trajectory events landed in session_events_pg (shared Pg)", Number(evRows) > 0,
      `rows=${evRows}`, t0);

    if (fbRows !== "2" || Number(evRows) === 0) throw new Error("Phase C failed — aborting");
  } finally {
    client.close();
  }
}

async function phaseD_imageSize() {
  const t0 = Date.now();
  const bytes = Number(sh(`docker image inspect ${IMAGE} --format '{{.Size}}'`));
  const mb = Math.round(bytes / 1e6);
  record("D", `slimmed image < ${MAX_IMAGE_MB}MB (was 497MB with ONNX)`, mb < MAX_IMAGE_MB, `size=${mb}MB`, t0);
}

// 等编排器把会话推进到期望状态/标记（sweep tick 30s，留足裕量）。
async function waitFor(condFn, timeoutMs, label) {
  const t0 = Date.now();
  while (Date.now() - t0 < timeoutMs) {
    const v = await condFn();
    if (v) return { ok: true, ms: Date.now() - t0 };
    await sleep(2_000);
  }
  return { ok: false, ms: Date.now() - t0, label };
}

async function phaseE_deepArchiveLifecycle() {
  const t0 = Date.now();
  // 第二编排器：idle=5s + deep=5s（sweep tick 30s → 全程约 1-2 分钟）。
  startOrch(LISTEN2, REGISTRY2, orchLog2, [
    "--idle-timeout", "5",
    "--deep-archive-timeout", "5",
    "--archive-dir", ARCHIVE_DIR,
  ], (p) => { orch2 = p; });
  if (!await waitHealthz(BASE2)) throw new Error(`orchestrator#2 did not come up — see ${orchLog2}`);

  const r = await createSession(BASE2, ARCH);
  record("E", "archive-instance session → 201 Running", r.status === 201 && r.body?.session?.state === "Running",
    r.status === 201 ? "" : JSON.stringify(r), t0);
  if (r.status !== 201) throw new Error("Phase E create failed — aborting");

  // 真实 turn：写 proof 文件进 /workspace（卷内容地面真值）+ 记住暗号（Pg 记忆）。
  const client = new EngineClient(ARCH, { base: BASE2 });
  try {
    await client.connectRetry(120_000, ARCH);
    await client.send("session/create", { id: CONV2, workspace: "/workspace" });
    await client.runTurn(
      `用 write_file 工具把字符串 ${PROOF2} 写入 /workspace/proof.txt（覆盖写）。写完回复 done。`,
      300_000,
    );
    const proofInVol = sh(`docker exec oneai-orch-${ARCH} cat /workspace/proof.txt 2>/dev/null || true`);
    record("E", "proof file written into the workspace volume", proofInVol.includes(PROOF2),
      `file=${JSON.stringify(proofInVol.slice(0, 80))}`, t0);
  } finally {
    client.close(); // 断开 WS——idle 判定前提
  }

  // 等自动 Hibernating（sweep tick ≤30s + idle 5s）。
  const hib = await waitFor(async () => (await getStatus(BASE2, ARCH))?.state === "Hibernating", 90_000);
  record("E", "auto-hibernated after disconnect (idle tier)", hib.ok, "", t0);

  // 等深度归档：status.archived 出现 + 容器/两卷从 docker 消失 + 归档文件落盘。
  const archived = await waitFor(async () => {
    const st = await getStatus(BASE2, ARCH);
    if (!st?.archived) return false;
    const containers = sh(`docker ps -aq --filter name=oneai-orch-${ARCH} | wc -l`);
    const volumes = sh(`docker volume ls -q --filter name=oneai-orch-${ARCH} | wc -l`);
    return containers === "0" && volumes === "0";
  }, 120_000);
  const st = await getStatus(BASE2, ARCH);
  const vols = st?.archived?.volumes ?? [];
  record("E", "deep-archived: container + BOTH volumes gone, archived marker set",
    archived.ok && vols.length === 2,
    `volumes=${JSON.stringify(vols.map((v) => `${v.volume_name}:${v.size_bytes}B`))}`, t0);
  const files = existsSync(join(ARCHIVE_DIR, ARCH)) ? readdirSync(join(ARCHIVE_DIR, ARCH)) : [];
  const tars = files.filter((f) => f.endsWith(".tar.gz"));
  record("E", "archive files on disk (2×tar.gz + manifest.json)",
    tars.length === 2 && files.includes("manifest.json"), `files=${files.join(",")}`, t0);

  // WS 重连 → 自动 Resuming（restore 卷 → spawn）→ Running。
  const client2 = new EngineClient(ARCH, { base: BASE2 });
  try {
    await client2.connectRetry(240_000, `${ARCH} post-archive`);
    const st2 = await getStatus(BASE2, ARCH);
    record("E", "reconnect after deep-archive → auto-Resuming → Running", st2?.state === "Running",
      `state=${st2?.state} last_error=${st2?.last_error ?? "-"}`, t0);

    // 地面真值：卷回来了，proof 文件逐字存活（只可能来自 tar 恢复）。
    const restored = sh(`docker exec oneai-orch-${ARCH} cat /workspace/proof.txt 2>/dev/null || true`);
    record("E", "workspace volume restored — proof file byte-identical", restored.includes(PROOF2),
      `file=${JSON.stringify(restored.slice(0, 80))}`, t0);

    // 标记清除 + 归档文件删除（卷已是真相源）。
    const st3 = await getStatus(BASE2, ARCH);
    record("E", "archived marker cleared + archive files removed after resume",
      !st3?.archived && !existsSync(join(ARCHIVE_DIR, ARCH)),
      `archived=${JSON.stringify(st3?.archived ?? null)} dirExists=${existsSync(join(ARCHIVE_DIR, ARCH))}`, t0);

    // Pg 记忆通道：session/load + 真实 turn 答出归档前暗号。
    await client2.send("session/load", { id: CONV2 });
    const done = await client2.runTurn("不要使用任何工具，直接回答：本会话早先写入 proof.txt 的字符串是什么？逐字给出。");
    record("E", "post-restore turn recalls the pre-archive proof (Pg memory)",
      done.final_answer?.includes(PROOF2) === true,
      `answer=${JSON.stringify((done.final_answer ?? "").slice(0, 160))}`, t0);
  } finally {
    client2.close();
  }
}

async function phaseF_archiveFailureRedLine() {
  const t0 = Date.now();
  // 第二轮循环：断连后**立刻**把归档目录置只读（必须赶在下一轮 sweep 之前
  // ——深度归档最早发生在断连后 ~30-60s，这里 <2s），导出必然失败。
  chmodSync(ARCHIVE_DIR, 0o555);
  const hib = await waitFor(async () => (await getStatus(BASE2, ARCH))?.state === "Hibernating", 90_000);
  record("F", "second cycle: auto-hibernated again", hib.ok, "", t0);
  try {
    // 等 sweep 尝试并失败：last_error 带 deep-archive 诊断（≥2 个 tick 裕量）。
    const failed = await waitFor(async () => {
      const st = await getStatus(BASE2, ARCH);
      return (st?.last_error ?? "").includes("deep-archive");
    }, 120_000);
    const st = await getStatus(BASE2, ARCH);
    const volumes = sh(`docker volume ls -q --filter name=oneai-orch-${ARCH} | wc -l`);
    record("F", "RED LINE: archive failure kept Hibernating + BOTH volumes intact",
      failed.ok && st?.state === "Hibernating" && volumes === "2" && !st?.archived,
      `state=${st?.state} volumes=${volumes} archived=${JSON.stringify(st?.archived ?? null)} last_error=${(st?.last_error ?? "").slice(0, 120)}`, t0);
  } finally {
    chmodSync(ARCHIVE_DIR, 0o755);
  }
  // 恢复可写 → 下一轮 sweep 归档成功 → 重连恢复。
  const archived = await waitFor(async () => {
    const st = await getStatus(BASE2, ARCH);
    if (!st?.archived) return false;
    return sh(`docker volume ls -q --filter name=oneai-orch-${ARCH} | wc -l`) === "0";
  }, 150_000);
  record("F", "recovered dir → next sweep archived successfully (retry path)", archived.ok, "", t0);
  const client = new EngineClient(ARCH, { base: BASE2 });
  try {
    await client.connectRetry(240_000, `${ARCH} post-failure-cycle`);
    const st = await getStatus(BASE2, ARCH);
    const restored = sh(`docker exec oneai-orch-${ARCH} cat /workspace/proof.txt 2>/dev/null || true`);
    record("F", "resume after the failed-then-succeeded cycle restores the volume again",
      st?.state === "Running" && restored.includes(PROOF2), `state=${st?.state}`, t0);
  } finally {
    client.close();
  }
}

async function phaseG_killWipePgOnlyRecovery() {
  const t0 = Date.now();
  // 杀 + 删容器 + 删光两卷：SQLite/文件/本地 trajectory 全丢，Pg 唯一幸存。
  sh(`docker rm -f oneai-orch-${S1}`, { stdio: "ignore" });
  sh(`docker volume rm -f oneai-orch-${S1}-state oneai-orch-${S1}-ws`, { stdio: "ignore" });
  const volsLeft = sh(`docker volume ls -q --filter name=oneai-orch-${S1} | wc -l`);
  record("G", "victim container + BOTH volumes wiped", volsLeft === "0", `volumes left=${volsLeft}`, t0);

  const client = new EngineClient(S1);
  try {
    await client.connectRetry(180_000, `${S1} post-wipe`);
    const st = (await getStatus(BASE, S1)).state;
    record("G", "reconnect after wipe → auto-Resuming (fresh empty volumes)", st === "Running", `state=${st}`, t0);

    const listed = await client.send("session/list", {});
    const found = (listed?.sessions ?? []).find((s) => s.id === CONV);
    record("G", "session/list recovers the conversation from Pg ALONE", !!found,
      found ? `message_count=${found.message_count}` : JSON.stringify(listed).slice(0, 200), t0);

    // MVS3-C 增量：feedback + trajectory 也跨卷删除存活（只可能来自 Pg）。
    const fbList = await client.send("feedback/list", { session_id: CONV });
    const kinds = JSON.stringify(fbList?.entries ?? fbList ?? []);
    record("G", "feedback survived the volume wipe (Pg feedback store)",
      kinds.includes('"up"') && kinds.includes("mvs3c 验收备注"), kinds.slice(0, 200), t0);
    const traj = await client.send("session/trajectory", { id: CONV });
    record("G", "trajectory survived the volume wipe (Pg session-event store)",
      traj?.ok === true && (traj?.events ?? []).length > 0, `events=${(traj?.events ?? []).length}`, t0);

    await client.send("session/load", { id: CONV });
    const done = await client.runTurn("不要使用任何工具，直接回答：本会话早先让你记住的暗号是什么？逐字给出。");
    record("G", "resumed engine recalls the PROOF in a real turn", done.final_answer?.includes(PROOF) === true,
      `answer=${JSON.stringify((done.final_answer ?? "").slice(0, 160))}`, t0);
  } finally {
    client.close();
  }
}

async function phaseH_teardown() {
  const t0 = Date.now();
  const del1 = await http(`/v1/sessions/${S1}`, { method: "DELETE" }).then((r) => r.status).catch(() => 0);
  const del2 = await http(`/v1/sessions/${ARCH}`, { method: "DELETE" }, BASE2).then((r) => r.status).catch(() => 0);
  record("H", "DELETE both sessions → 200", del1 === 200 && del2 === 200, `statuses=${del1},${del2}`, t0);
  await sleep(2_000);
  const containers = sh(`docker ps -aq --filter name=oneai-orch- | wc -l`);
  const volumes = sh(`docker volume ls -q --filter name=oneai-orch- | wc -l`);
  record("H", "no oneai-orch-* containers/volumes left (incl. archived session's)",
    containers === "0" && volumes === "0", `containers=${containers} volumes=${volumes}`, t0);
  cleanRows();
  const left = psql(
    `SELECT (SELECT count(*) FROM conversations_pg WHERE id LIKE 'mvs3c-%')` +
    ` + (SELECT count(*) FROM session_events_pg WHERE session_id LIKE 'mvs3c-%')` +
    ` + (SELECT count(*) FROM message_feedback_pg WHERE session_id LIKE 'mvs3c-%');`);
  record("H", "acceptance rows cleaned from Pg", left === "0", `rows left=${left}`, t0);
}

// ─── main ────────────────────────────────────────────────────────────────────
async function main() {
  console.log(`MVS3-C 验收（session-events/feedback Pg + 深度归档 + 瘦身/预热）：image=${IMAGE} · listen=${LISTEN}/${LISTEN2}`);
  console.log(`dsn(host view)=${DSN.replace(/:[^:@/]*@/, ":***@")} · pg=${PG_CONTAINER} · archive=${ARCHIVE_DIR}`);
  console.log(`conv=${CONV}/${CONV2} · proof=${PROOF}/${PROOF2} · orch logs=${orchLog},${orchLog2}\n`);
  preflight();

  try {
    startOrch(LISTEN, REGISTRY, orchLog, ["--idle-timeout", "3600"], (p) => { orch = p; });
    if (!await waitHealthz(BASE)) throw new Error(`orchestrator did not come up — see ${orchLog}`);

    await phaseA_create();
    await phaseB_backendSelection();
    await phaseC_feedbackTrajectoryPgTruth();
    await phaseD_imageSize();
    await phaseE_deepArchiveLifecycle();
    await phaseF_archiveFailureRedLine();
    await phaseG_killWipePgOnlyRecovery();
    await phaseH_teardown();
  } catch (e) {
    console.error(`\nABORT: ${e.message}`);
  } finally {
    await stopOrch(orch2);
    await stopOrch(orch);
    if (!KEEP) {
      try { execSync(`docker rm -f $(docker ps -aq --filter name=oneai-orch-) 2>/dev/null || true`, { stdio: "ignore", shell: "/bin/bash" }); } catch {}
      try { execSync(`docker volume rm -f $(docker volume ls -q --filter name=oneai-orch-) 2>/dev/null || true`, { stdio: "ignore", shell: "/bin/bash" }); } catch {}
      for (const d of [REGISTRY, REGISTRY2, ARCHIVE_DIR]) {
        try { chmodSync(d, 0o755); } catch {}
        try { rmSync(d, { recursive: true, force: true }); } catch {}
      }
      cleanRows();
    }
  }

  const failed = results.filter((r) => !r.ok);
  console.log(`\n=== MVS3-C 验收汇总：${results.length - failed.length}/${results.length} 项通过 ===`);
  for (const r of results) console.log(`${r.ok ? "✅" : "❌"} [${r.phase}] ${r.name} (${r.ms}ms)${!r.ok && r.detail ? ` — ${r.detail}` : ""}`);
  process.exit(failed.length === 0 ? 0 : 1);
}

main();
