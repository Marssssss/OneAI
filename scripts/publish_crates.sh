#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────
# OneAI crates.io publish — dependency-topological order.
#
# Run once after `cargo login` (paste your crates.io API token).
# Each crate is published in order so downstream crates resolve their
# already-published path-deps (rewritten to registry requirements).
#
#   cargo login
#   ./scripts/publish_crates.sh
#
# Re-running is safe-ish: already-published crates error with
# "already exists" and abort the run — comment them out or use --dry-run.
# ──────────────────────────────────────────────────────────────────────
set -euo pipefail

CRATES=(
  # 1. root
  oneai-core
  # 2. leaves / mid (depend only on core + external)
  oneai-trace
  oneai-parser
  oneai-domain
  oneai-memory
  oneai-rag
  oneai-skill
  oneai-tool
  oneai-workflow
  oneai-persistence
  oneai-scheduler
  oneai-provider
  oneai-a2a
  oneai-wasm
  oneai-mcp
  oneai-eval
  oneai-studio
  oneai-platform-desktop
  oneai-platform-android
  oneai-platform-ios
  oneai-platform-harmony
  # 3. agent (depends on mid)
  oneai-agent
  # 4. SDK entry (depends on agent + mid; NOT on uniffi)
  oneai-app
  # 5. uniffi (depends on app)
  oneai-uniffi
  # 6. TUI (depends on app + uniffi + others)
  oneai-cli
)

for c in "${CRATES[@]}"; do
  echo "── publishing $c"
  if cargo publish -p "$c"; then
    echo "   ✓ $c"
  else
    echo "   ⚠ $c failed — fix and re-run (comment out the ones already published)"
    exit 1
  fi
done

echo ""
echo "✓ all $((${#CRATES[@]})) crates published to crates.io"
