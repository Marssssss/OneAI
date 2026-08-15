#!/usr/bin/env node
// bin/oneai.js — the launcher. Zero business logic: locate the `oneai` engine
// binary (the one install.js fetched, else one on PATH) and exec it with the
// forwarded argv + inherited stdio. This is the entire npm shell — the TUI,
// subcommands, app-server, etc. all live in the Rust binary.

"use strict";

const fs = require("fs");
const path = require("path");
const { spawnSync } = require("child_process");

const a = (function () {
  const p = process.platform;
  const arch = process.arch;
  const isWin = p === "win32";
  const ext = isWin ? ".exe" : "";
  let name;
  if (p === "darwin" && arch === "arm64") name = "oneai-aarch64-apple-darwin";
  else if (p === "darwin" && arch === "x64") name = "oneai-x86_64-apple-darwin";
  else if (p === "linux" && arch === "x64") name = "oneai-x86_64-unknown-linux-gnu";
  else if (p === "linux" && arch === "arm64") name = "oneai-aarch64-unknown-linux-gnu";
  else if (isWin && arch === "x64") name = "oneai-x86_64-pc-windows-msvc";
  else return null;
  return { file: "oneai-" + name + ext, isWin: isWin };
})();

// 1. The binary install.js downloaded into this package's bin/.
let bin = a ? path.join(__dirname, a.file) : null;
if (bin && !fs.existsSync(bin)) bin = null;

// 2. Fallback: `oneai` on PATH (a dev who `cargo install`-ed, or a manual
//    install). Lets the npm shell work even before per-platform release
//    binaries are published.
if (!bin) {
  const lookup = spawnSync(
    a && a.isWin ? "where" : "which",
    ["oneai"],
    { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] },
  );
  if (lookup.status === 0 && lookup.stdout.trim()) {
    bin = lookup.stdout.split(/\r?\n/)[0].trim();
  }
}

if (!bin) {
  process.stderr.write(
    "[oneai] no engine binary found. Install one:\n" +
      "  • npm: this package tries to fetch a prebuilt binary on install — " +
      "if that failed, check the release for your platform at " +
      "https://github.com/Marssssss/OneAI/releases\n" +
      "  • cargo: `cargo install oneai-cli` (puts `oneai` on PATH)\n",
  );
  process.exit(127);
}

// Forward argv (drop the node + script path) + inherit stdio so the TUI /
// app-server behave exactly as if the binary were called directly.
const result = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });

// Propagate the engine's exit code; signal death → 128+signum (shell convention).
if (result.signal) process.kill(process.pid, result.signal);
process.exit(result.status ?? 1);
