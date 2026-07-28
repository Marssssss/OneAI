#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────
# release-local.sh — pre-publish smoke (evolution-plan §1.3 / inspiration
# P0-3, modeled on PI's `local-release.mjs`).
#
# Equivalent of "say-exactly-ok": for every publishable workspace crate, run
# `cargo publish --dry-run`. That is the strongest publish-readiness check
# cargo offers without uploading: it rewrites path deps into registry
# requirements, packages the crate, validates registry metadata, and builds
# the package in ISOLATION (no workspace context, no path-dep leakage) —
# exactly simulating what a real consumer on crates.io will experience.
#
# When green, prints the tag command to trigger .github/workflows/publish.yml.
#
# Usage:
#   ./scripts/release-local.sh
# ──────────────────────────────────────────────────────────────────────
set -euo pipefail

# Same topological order as scripts/publish_crates.sh (Kahn sort of the
# intra-workspace graph). Keep in sync if the graph changes.
CRATES=(
  oneai-core
  oneai-parser
  oneai-persistence
  oneai-provider
  oneai-rag
  oneai-scheduler
  oneai-skill
  oneai-tool
  oneai-mcp
  oneai-trace
  oneai-workflow
  oneai-domain
  oneai-a2a
  oneai-memory
  oneai-agent
  oneai-wasm
  oneai-app
  oneai-eval
  oneai-platform-android
  oneai-platform-desktop
  oneai-platform-harmony
  oneai-platform-ios
  oneai-studio
  oneai-uniffi
  oneai-cli
)

# Skip the staticlib + example scaffolding crates that are publish=false.
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
if [[ -z "$VERSION" ]]; then
  echo "✗ could not read workspace version from root Cargo.toml" >&2
  exit 1
fi

echo "▶ release-local smoke — workspace v$VERSION, ${#CRATES[@]} crates"
echo "  (cargo publish --dry-run: package + path-dep rewrite + isolated build)"
echo ""

failed=0
for c in "${CRATES[@]}"; do
  printf "  %-28s " "$c"
  # --allow-dirty: don't fail on uncommitted local changes during dev smoke.
  # --dry-run: no upload, but full package+rewrite+isolated-build+metadata check.
  if out=$(cargo publish --dry-run -p "$c" --allow-dirty 2>&1); then
    # Confirm the packaged .crate exists; cargo prints its path on success.
    crate_path=$(echo "$out" | grep -oE 'target/package/[^ ]+\.crate' | head -1 || true)
    if [[ -n "$crate_path" ]]; then
      printf "✓ packaged %s\n" "$(basename "$crate_path")"
    else
      printf "✓ ok\n"
    fi
  else
    printf "✗ FAILED\n"
    echo "$out" | sed 's/^/      /' | tail -8
    failed=$((failed + 1))
  fi
done

echo ""
if [[ "$failed" -ne 0 ]]; then
  echo "✗ $failed crate(s) failed the dry-run publish — fix above before tagging."
  exit 1
fi

echo "✓ all ${#CRATES[@]} crates pass dry-run publish."
echo ""
echo "Next: commit, then tag + push to trigger publish.yml:"
echo "  git tag v$VERSION"
echo "  git push origin v$VERSION"
