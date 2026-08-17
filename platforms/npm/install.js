// install.js — postinstall: fetch the platform `oneai` binary from the GitHub
// Release matching this package's version (Codex/esbuild/rust-analyzer model).
//
// The npm package carries ZERO business logic — it only distributes the
// prebuilt engine binary + forwards argv/stdio (see bin/oneai.js). This keeps
// `npm install -g oneai-cli` a one-step install for non-Rust users (no cargo).
//
// Asset contract (produced by .github/workflows/release-binaries.yml on a
// `v*` tag):
//   oneai-aarch64-apple-darwin        (macOS arm64)
//   oneai-x86_64-unknown-linux-gnu   (Linux x86_64)
//   oneai-aarch64-unknown-linux-gnu  (Linux arm64)
//   oneai-x86_64-pc-windows-msvc.exe  (Windows x86_64)
//
// Proxy + resilience: the download honors HTTPS_PROXY/HTTP_PROXY (CONNECT
// tunnel, zero-dep — no https-proxy-agent dependency) and NO_PROXY, prints
// progress (so a slow CDN never looks like a silent hang), times out on
// 45s of socket inactivity, and retries once before soft-failing.
//
// Graceful fallback: if the asset is missing (release not published yet / no
// prebuilt for this platform / offline / proxy broken) the install still
// SUCCEEDS — bin/oneai.js falls back to a `oneai` on PATH (e.g. `cargo install
// oneai-cli`). We never hard-fail the install over a binary download; the user
// just gets a helpful message when they actually run it without any binary.

"use strict";

const fs = require("fs");
const http = require("http");
const https = require("https");
const tls = require("tls");
const path = require("path");
const { URL } = require("url");
const { createWriteStream } = require("fs");

const REPO = "Marssssss/OneAI";
// Version comes from package.json so progress/error messages track the
// installed npm version. NOTE: the npm package version is decoupled from
// the engine binary release tag (ENGINE_TAG below) — the npm package is a
// launcher wrapper, so a launcher-only fix (proxy/timeout/naming) bumps the
// npm version without rebuilding the (unchanged) engine binary; it just
// points at the existing binary release. Bump ENGINE_TAG only when the
// engine binary itself changes.
const VERSION = require("./package.json").version;
const ENGINE_TAG = "0.2.0"; // the binary release tag this launcher fetches
const IDLE_TIMEOUT_MS = 45_000; // socket inactivity → destroy + retry
const MAX_ATTEMPTS = 2; // one retry

/** Map the running platform/arch to the release asset name (== local file). */
function asset() {
  const p = process.platform;
  const a = process.arch;
  const isWin = p === "win32";
  const ext = isWin ? ".exe" : "";
  let name;
  if (p === "darwin" && a === "arm64") name = "oneai-aarch64-apple-darwin";
  // NB: macOS x86_64 has no prebuilt (ort-sys can't link ONNX for x64 mac) —
  // falls through to `null` → the PATH fallback below (cargo install).
  else if (p === "linux" && a === "x64") name = "oneai-x86_64-unknown-linux-gnu";
  else if (p === "linux" && a === "arm64") name = "oneai-aarch64-unknown-linux-gnu";
  else if (isWin && a === "x64") name = "oneai-x86_64-pc-windows-msvc";
  else return null; // unsupported platform — fall back to PATH
  // Local staged file matches the asset name verbatim (single "oneai-" prefix).
  return { name: name + ext, file: name + ext, isWin: isWin };
}

const outDir = path.join(__dirname, "bin");
const a = asset();

if (!a) {
  console.warn(
    `[oneai] no prebuilt binary for ${process.platform}/${process.arch}; ` +
      `will use \`oneai\` on PATH (install via \`cargo install oneai-cli\`).`,
  );
  return;
}

const url = `https://github.com/${REPO}/releases/download/v${ENGINE_TAG}/${a.name}`;
const dest = path.join(outDir, a.file);

function fail(msg) {
  // Soft-fail: leave the install in place; bin/oneai.js falls back to PATH.
  console.warn(`[oneai] ${msg}`);
  console.warn(
    `[oneai] binary not installed; will try \`oneai\` on PATH at run time.`,
  );
}

// Idempotent: skip if already present (e.g. reinstall over an existing copy).
if (fs.existsSync(dest)) {
  if (!a.isWin) fs.chmodSync(dest, 0o755);
  return;
}

fs.mkdirSync(outDir, { recursive: true });

// ─── proxy resolution (CONNECT tunnel, zero-dep) ──────────────────────
// Reads HTTPS_PROXY/https_proxy (then HTTP_PROXY/http_proxy); NO_PROXY
// (comma list, suffix/host match) disables the proxy for matching hosts.
function proxyFor(targetHost) {
  const noProxy = (process.env.NO_PROXY || process.env.no_proxy || "")
    .split(",")
    .map((s) => s.trim().toLowerCase())
    .filter(Boolean);
  if (noProxy.includes("*")) return null;
  const host = targetHost.toLowerCase();
  if (noProxy.some((p) => host === p || host.endsWith("." + p) || p.endsWith("." + host))) {
    return null;
  }
  const raw =
    process.env.HTTPS_PROXY || process.env.https_proxy ||
    process.env.HTTP_PROXY || process.env.http_proxy || "";
  return raw || null;
}

// A minimal https.Agent that opens a CONNECT tunnel through an HTTP proxy,
// then TLS-upgrades the tunneled socket. No external dependency. Handles
// proxy auth (user:pass in the proxy URL). Used for github.com AND the
// objects.githubusercontent.com redirect target (the agent re-tunnels per
// host).
class ProxyAgent extends https.Agent {
  constructor(proxyUrl) {
    super();
    this.proxy = new URL(proxyUrl);
  }
  createConnection(opts, cb) {
    const targetHost = opts.host || opts.servername || opts.hostname;
    const targetPort = opts.port || 443;
    const p = this.proxy;
    const proxyPort = Number(p.port) || (p.protocol === "https:" ? 443 : 80);
    const connectReq = http.request({
      host: p.hostname,
      port: proxyPort,
      method: "CONNECT",
      path: `${targetHost}:${targetPort}`,
      setHost: false,
      headers: p.username
        ? {
            "Proxy-Authorization":
              "Basic " +
              Buffer.from(
                `${decodeURIComponent(p.username)}:${decodeURIComponent(p.password || "")}`,
              ).toString("base64"),
          }
        : {},
    });
    const connectTimeout = setTimeout(
      () => connectReq.destroy(new Error("proxy CONNECT timeout")),
      IDLE_TIMEOUT_MS,
    );
    connectReq.once("connect", (res, socket) => {
      clearTimeout(connectTimeout);
      if (res.statusCode !== 200) {
        socket && socket.destroy();
        cb(new Error(`proxy CONNECT returned ${res.statusCode}`));
        return;
      }
      const tlsSock = tls.connect(
        { socket, servername: targetHost, ALPNProtocols: ["http/1.1"] },
        () => cb(null, tlsSock),
      );
      tlsSock.once("error", cb);
    });
    connectReq.once("error", (err) => {
      clearTimeout(connectTimeout);
      cb(err);
    });
    connectReq.end();
  }
}

function agentFor(host) {
  const proxy = proxyFor(host);
  if (!proxy) return undefined;
  try {
    return new ProxyAgent(proxy);
  } catch (err) {
    console.warn(`[oneai] invalid proxy URL "${proxy}": ${err.message}; trying direct.`);
    return undefined;
  }
}

// ─── download with progress + idle timeout + redirect-follow ───────────
function download(attempt) {
  const tmp = dest + ".part";
  return new Promise((resolve, reject) => {
    const target = new URL(url);
    const req = https.get(url, { agent: agentFor(target.host) }, (res) => {
      // Follow one redirect (GitHub releases 302 → CDN).
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        res.resume();
        const redir = new URL(res.headers.location, url);
        return https.get(
          res.headers.location,
          { agent: agentFor(redir.host) },
          (res2) => pipe(res2),
        ).on("error", reject);
      }
      if (res.statusCode !== 200) {
        res.resume();
        return reject(new Error(`HTTP ${res.statusCode} for ${url}`));
      }
      pipe(res);
    });
    req.on("error", reject);

    function pipe(res) {
      const total = res.headers["content-length"]
        ? parseInt(res.headers["content-length"], 10)
        : null;
      const mb = (b) => (b / (1024 * 1024)).toFixed(1) + "MB";
      process.stderr.write(
        `[oneai] downloading oneai engine v${ENGINE_TAG} (${a.name}, oneai-cli v${VERSION})` +
          (total ? ` ~${mb(total)}` : "") + ` ...${attempt > 1 ? ` (retry ${attempt - 1})` : ""}\n`,
      );
      let got = 0;
      let lastReport = 0;
      const stream = createWriteStream(tmp);
      const idle = setTimeout(() => {
        req.destroy(new Error("download idle timeout (45s no data)"));
      }, IDLE_TIMEOUT_MS);
      const bump = () => {
        idle.refresh();
        if (total) {
          const now = got;
          if (now - lastReport >= 5 * 1024 * 1024 || now === total) {
            lastReport = now;
            process.stderr.write(`\r[oneai] ${mb(now)}/${mb(total)} (${Math.round((now / total) * 100)}%)`);
          }
        }
      };
      res.on("data", (chunk) => { got += chunk.length; bump(); });
      res.on("end", () => process.stderr.write(total ? "\n" : `[oneai] downloaded ${mb(got)}\n`));
      res.pipe(stream);
      res.on("error", (err) => { clearTimeout(idle); reject(err); });
      stream.on("error", (err) => { clearTimeout(idle); reject(err); });
      stream.on("finish", () => {
        clearTimeout(idle);
        stream.close((err) => {
          if (err) return reject(err);
          fs.renameSync(tmp, dest);
          if (!a.isWin) fs.chmodSync(dest, 0o755);
          resolve();
        });
      });
    }
  });
}

(async () => {
  for (let attempt = 1; attempt <= MAX_ATTEMPTS; attempt++) {
    try {
      await download(attempt);
      console.log(`[oneai] installed oneai engine v${ENGINE_TAG} → ${path.relative(process.cwd(), dest)}`);
      return;
    } catch (err) {
      try { fs.unlinkSync(dest + ".part"); } catch {}
      if (attempt < MAX_ATTEMPTS) {
        console.warn(`[oneai] download attempt ${attempt} failed: ${err.message}; retrying...`);
        continue;
      }
      fail(`could not download binary (engine v${ENGINE_TAG}) after ${MAX_ATTEMPTS} attempts: ${err.message}`);
      // Hint the proxy lever for users behind a firewall (e.g. CN networks).
      const hasProxy = process.env.HTTPS_PROXY || process.env.http_proxy;
      if (!hasProxy) {
        console.warn(
          `[oneai] tip: if github.com is slow/blocked, set HTTPS_PROXY (e.g. ` +
            `export HTTPS_PROXY=http://127.0.0.1:7890) and reinstall, or use ` +
            `\`cargo install oneai-cli\`.`,
        );
      }
      return;
    }
  }
})();
