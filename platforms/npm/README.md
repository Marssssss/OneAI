# oneai (npm)

Cross-platform launcher for the OneAI engine — `npm install -g oneai` gets
you the `oneai` command (the TUI + all subcommands: `app-server`, `serve`,
`session`, `evolve`, …) **without Rust/cargo**.

## What it is

This is the Codex-style "npm shell": the package carries **zero business logic**.
On `postinstall` it fetches the prebuilt `oneai` binary for your platform from
the matching GitHub Release; the `bin` just forwards `argv` + `stdio` to that
binary. Every feature (AgentLoop, TUI, app-server, sidecar, …) lives in the
Rust binary.

```bash
npm install -g oneai
oneai                 # launch the TUI
oneai app-server --listen stdio   # JSON-RPC engine process
```

## Fallback

If `postinstall` couldn't fetch a binary (no prebuilt for your platform, the
release isn't published yet, offline install), the launcher falls back to a
`oneai` on PATH — so a dev who `cargo install oneai-cli`-ed keeps working
unchanged. Run with no binary available and you'll get a message pointing at
the releases + `cargo install`.

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
npm pack          # sanity-check the tarball contents (bin/oneai.js + install.js + README)
npm publish       # after the matching vX.Y.Z GitHub Release with binaries is live
```

The npm version is kept in lockstep with the workspace `version` (currently
`0.1.0`) so the download URL always points at the right release.
