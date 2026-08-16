# oneai (npm)

Cross-platform launcher for the OneAI engine — `npm install -g oneai-cli` gets
you the `oneai` command (the TUI + all subcommands: `app-server`, `serve`,
`session`, `evolve`, `web`, …) **without Rust/cargo**.

## What it is

This is the Codex-style "npm shell": the package carries **zero business logic**.
On `postinstall` it fetches the prebuilt `oneai` binary for your platform from
the matching GitHub Release; the `bin` just forwards `argv` + `stdio` to that
binary. Every feature (AgentLoop, TUI, app-server, sidecar, webUI, …) lives in
the Rust binary. The package additionally bundles the **webUI dist**
(platform-independent JS, built at publish via `prepublishOnly`) so `oneai web`
is a true one-command launch — no source checkout, no separate Vite process.

```bash
npx oneai-cli web     # one command: engine + webUI + open browser (mirrors `npx @deepseek-ai/dsh web`)
# or
npm install -g oneai-cli
oneai                 # launch the TUI
oneai app-server --listen stdio   # JSON-RPC engine process
```

## `oneai web`

`npx oneai-cli web` builds the engine in-process, serves the prebuilt SPA
static assets **and** the `/ws` JSON-RPC endpoint on one port (default
`http://127.0.0.1:8787`), and opens the browser. The launcher sets
`ONEAI_WEB_DIST` to the bundled `web-dist/` so the binary serves this package's
dist; override with `--dist <path>` or set `ONEAI_WEB_DIST` yourself. Flags:
`--port`, `--host`, `--dist`, `--no-open`, `--domain`, `--model`, `--user`.

## Fallback

If `postinstall` couldn't fetch a binary (no prebuilt for your platform, the
release isn't published yet, offline install), the launcher falls back to a
`oneai` on PATH — so a dev who `cargo install oneai-cli`-ed keeps working
unchanged. Run with no binary available and you'll get a message pointing at
the releases + `cargo install`. (`oneai web` also auto-detects a local
`platforms/web/dist` or `--dist`, so a cargo binary works without the bundled
dist.)

## Asset contract

`install.js` downloads, for package version `X.Y.Z`, one of:

| platform / arch            | asset name                              |
|----------------------------|-----------------------------------------|
| darwin arm64               | `oneai-aarch64-apple-darwin`            |
| darwin x86_64              | *(no prebuilt — falls back to PATH)*    |
| linux x86_64               | `oneai-x86_64-unknown-linux-gnu`        |
| linux arm64                | `oneai-aarch64-unknown-linux-gnu`       |
| win32 x86_64               | `oneai-x86_64-pc-windows-msvc.exe`       |

…from `https://github.com/Marssssss/OneAI/releases/download/vX.Y.Z/<asset>`.

These assets are produced by
[`.github/workflows/release-binaries.yml`](../../.github/workflows/release-binaries.yml)
on a `v*` tag (best-effort per platform; macOS x86_64 currently has no ONNX
Runtime prebuilt and is skipped — those users fall back to PATH).

## Publish (maintainer)

```bash
cd platforms/npm
npm run build:web   # (optional) build the webUI dist into web-dist/ ahead of time
npm pack            # sanity-check the tarball (bin/oneai.js + install.js + README + web-dist/)
npm publish         # prepublishOnly builds web-dist/ automatically; publish after the vX.Y.Z GitHub Release is live
```

`npm publish` runs `prepublishOnly` → `scripts/build-web.sh` which builds the
webUI (`platforms/web`: `npm ci && npm run build`) and stages the output into
`web-dist/` so the published tarball carries the dist. The dist is gitignored
(`platforms/npm/web-dist/`) — it lives in the tarball, not the repo. Requires
node/npm at publish time (on the maintainer's machine).

The npm version is kept in lockstep with the workspace `version` (currently
`0.1.0`) so the download URL always points at the right release.
