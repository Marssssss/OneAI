# Release runbook

Releasing OneAI = four aligned artifacts on the same `vX.Y.Z`: the **crates.io
SDK** (Rust users `cargo add`/`cargo install`), the **platform binaries**
(attached to the GitHub Release, fetched by the npm shell), the **npm shell**
(`npm install -g oneai`, no cargo), and (macOS) the **`.app` zip**.

The workspace `version` (currently `0.1.0`) is the single source — every
artifact carries it. Bump it in `Cargo.toml` `[workspace.package]` (the
internal path-dep `version = "…"` fields track it) + `platforms/npm/package.json`
+ `platforms/macos/Info.plist` + the README badge, then rebuild `Cargo.lock`.

> **1.0.0 / 1.1.0 were retracted** (premature — crates.io never published, no
> external users). Versioning restarts at **0.1.0**, inheriting all work
> through the 1.1.0 macOS builds. See `CHANGELOG.md` `[Unreleased]`.

## 0. Pre-flight (local)

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check                       # supply-chain gates
./scripts/release-local.sh             # cargo publish --dry-run per crate
# macOS app (optional):
bash scripts/build_apple.sh && bash platforms/macos/build_macos.sh
```

`release-local.sh` packages every crate, rewrites path deps → registry
requirements, and isolated-builds each — it surfaces any metadata / path-dep
issue *before* you tag. **Don't skip it.**

## 1. Tag + push (triggers CI)

```bash
git tag v0.1.0
git push origin v0.1.0
```

The tag fires two workflows:

- **`.github/workflows/publish.yml`** → publishes all `oneai-*` crates to
  crates.io in Kahn topological order (`scripts/publish_crates.sh`, idempotent
  + 429 backoff). Needs ONE-TIME setup — see the workflow header:
  - Path A (preferred): Trusted Publishing — bind each crate on crates.io →
    Settings → Trusted Publishing (repo / workflow / environment), then swap
    the publish step to `rust-lang/crates-io-actions/publish@v1`.
  - Path B (token): add `CARGO_REGISTRY_TOKEN` repo secret (publish scope).
- **`.github/workflows/release-binaries.yml`** → builds per-platform `oneai`
  binaries (best-effort per arch; macOS x86_64 skipped — no ONNX Runtime
  prebuilt) and attaches them to the `v0.1.0` release. These are the assets
  the npm shell's postinstall fetches.

> **Disk space**: the publish + binary builds need a few GB of free space on
> the runner (the ONNX Runtime download + a full release build per target).
> The earlier 1.0.0 attempt hit a "磁盘满" failure — watch the run; if the
> runner fills, trim caches or split the binary matrix across jobs.

## 1b. Yank the retracted 1.0.0 (REQUIRED for 0.1.0 to be "latest")

**Critical context**: `oneai-core` / `oneai-trace` / `oneai-app` /
`oneai-agent` / `oneai-cli` … were **already published to crates.io at
`1.0.0`** on 2026-07-15 (the premature release). crates.io versions are
immutable and `cargo add` always resolves to the **highest** version — so
publishing `0.1.0` alone leaves `1.0.0` as crates.io's "latest" (users
`cargo add oneai-core` → 1.0.0, the retracted one). `0.1.0 < 1.0.0`.

To make `0.1.0` the effective latest, **yank** the published `1.0.0` crates
after `0.1.0` is up (yank hides a version from NEW dependency resolution;
existing `Cargo.lock`s still resolve). With 1.0.0 yanked, `cargo add` resolves
to the next-highest available = `0.1.0`.

```bash
# After step 1 published 0.1.0. Yank every crate's 1.0.0 (one-time, per crate).
# Requires CARGO_REGISTRY_TOKEN (publish scope) or `cargo login`.
cargo yank oneai-core@1.0.0
cargo yank oneai-trace@1.0.0
cargo yank oneai-app@1.0.0
cargo yank oneai-agent@1.0.0
cargo yank oneai-cli@1.0.0
# … and every other oneai-* crate that published a 1.0.0 (see
#   https://crates.io/users/Marssssss for the full list)
```

Yank is reversible (`cargo yank --undo …`) if a downstream genuinely needs
1.0.0. Given there are effectively no external users, yanking the premature
1.0.0 is the intended "retract" on the registry. (You cannot *delete* a
crates.io version — yank is the supported retraction.)

## 2. npm shell (after the binaries are live)

```bash
cd platforms/npm
npm pack            # sanity-check: bin/oneai.js + install.js + README.md, zero deps
npm publish         # oneai@0.1.0 — postinstall fetches the just-uploaded binaries
```

Publish npm **after** step 1's binaries are attached, so a fresh
`npm install -g oneai@0.1.0` can fetch its platform binary immediately.
(If a user installs before binaries exist, the postinstall soft-fails and
the launcher falls back to `oneai` on PATH.)

## 3. Verify

```bash
cargo install oneai-cli              # registry (crates.io)
oneai --version                      # 0.1.0
npm install -g oneai                 # binary download via npm
oneai --version
# macOS app: attach OneAI-0.1.0-macos.zip to the release, link in the notes
```

## Out of scope for 0.1.0

- Windows / Android / iOS / HarmonyOS native apps remain pre-release (no
  signed/notarized builds).
- crates.io crate names must not already be squatted — verify `cargo search
  oneai-app` etc. before the first publish.
