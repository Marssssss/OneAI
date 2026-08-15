// install.js — postinstall: fetch the platform `oneai` binary from the GitHub
// Release matching this package's version (Codex/esbuild/rust-analyzer model).
//
// The npm package carries ZERO business logic — it only distributes the
// prebuilt engine binary + forwards argv/stdio (see bin/oneai.js). This keeps
// `npm install -g oneai` a one-step install for non-Rust users (no cargo).
//
// Asset contract (produced by .github/workflows/release-binaries.yml on a
// `v*` tag):
//   oneai-aarch64-apple-darwin        (macOS arm64)
//   oneai-x86_64-apple-darwin         (macOS x86_64)
//   oneai-x86_64-unknown-linux-gnu   (Linux x86_64)
//   oneai-aarch64-unknown-linux-gnu  (Linux arm64)
//   oneai-x86_64-pc-windows-msvc.exe  (Windows x86_64)
//
// Graceful fallback: if the asset is missing (release not published yet / no
// prebuilt for this platform / offline) the install still SUCCEEDS — bin/oneai.js
// falls back to a `oneai` on PATH (e.g. `cargo install oneai-cli`). We never
// hard-fail the install over a binary download; the user just gets a helpful
// message when they actually run it without any binary available.

"use strict";

const fs = require("fs");
const https = require("https");
const path = require("path");
const { createWriteStream } = require("fs");

const REPO = "Marssssss/OneAI";
// Version comes from package.json so the download URL tracks the installed
// npm version exactly (npm install oneai@0.1.0 → fetches release v0.1.0).
const VERSION = require("./package.json").version;

/** Map the running platform/arch to the release asset name + local binary file. */
function asset() {
  const p = process.platform;
  const a = process.arch;
  const isWin = p === "win32";
  const ext = isWin ? ".exe" : "";
  let name;
  if (p === "darwin" && a === "arm64") name = "oneai-aarch64-apple-darwin";
  else if (p === "darwin" && a === "x64") name = "oneai-x86_64-apple-darwin";
  else if (p === "linux" && a === "x64") name = "oneai-x86_64-unknown-linux-gnu";
  else if (p === "linux" && a === "arm64") name = "oneai-aarch64-unknown-linux-gnu";
  else if (isWin && a === "x64") name = "oneai-x86_64-pc-windows-msvc";
  else return null; // unsupported platform — fall back to PATH
  return { name: name + ext, file: "oneai-" + name + ext, isWin: isWin };
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

const url = `https://github.com/${REPO}/releases/download/v${VERSION}/${a.name}`;
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

const tmp = dest + ".part";
const req = https.get(url, (res) => {
  // Follow a single redirect (GitHub releases 302 to the CDN) — node https
  // doesn't follow redirects by default.
  if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
    https.get(res.headers.location, writeFile).on("error", retryFallback);
    return;
  }
  if (res.statusCode !== 200) {
    // Most common: 404 when the v<VERSION> release isn't published yet.
    return retryFallback(
      new Error(`HTTP ${res.statusCode} for ${url}`),
    );
  }
  writeFile(res);
});

function writeFile(res) {
  const stream = createWriteStream(tmp);
  res.pipe(stream);
  res.on("error", retryFallback);
  stream.on("error", retryFallback);
  stream.on("finish", () => {
    stream.close((err) => {
      if (err) return retryFallback(err);
      fs.renameSync(tmp, dest);
      if (!a.isWin) fs.chmodSync(dest, 0o755);
    });
  });
}

function retryFallback(err) {
  try {
    if (fs.existsSync(tmp)) fs.unlinkSync(tmp);
  } catch {}
  fail(`could not download binary (v${VERSION}): ${err.message}`);
}

req.on("error", retryFallback);
