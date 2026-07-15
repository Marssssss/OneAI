#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────
# OneAI crates.io publish — correct dependency-topological order
# (normal + dev deps). Computed by Kahn's algorithm; edit only if the
# crate graph changes.
#
# Prereq: `cargo login` (paste your crates.io API token once).
#
#   cargo login
#   ./scripts/publish_crates.sh
#
# Idempotent: if a crate is already on the registry (e.g. re-running after
# a partial failure), the script detects "already exists" and continues.
# ──────────────────────────────────────────────────────────────────────
set -uo pipefail

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

for c in "${CRATES[@]}"; do
  echo "── publishing $c"
  out=$(cargo publish -p "$c" 2>&1)
  rc=$?
  echo "$out" | tail -4
  if [[ $rc -eq 0 ]]; then
    echo "   ✓ $c"
    continue
  fi
  # Already-published crates are not a failure — skip and continue.
  if echo "$out" | grep -qiE 'already exists|already been uploaded'; then
    echo "   ↻ $c already published — skipping"
    continue
  fi
  echo "   ⚠ $c FAILED — see above; fix and re-run (published ones auto-skip)"
  exit 1
done

echo ""
echo "✓ all ${#CRATES[@]} crates published to crates.io"
