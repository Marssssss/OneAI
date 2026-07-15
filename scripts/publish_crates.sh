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
  attempt=0
  while true; do
    attempt=$((attempt + 1))
    echo "── publishing $c${attempt:+ (attempt $attempt)}"
    out=$(cargo publish -p "$c" 2>&1)
    rc=$?
    echo "$out" | tail -4
    if [[ $rc -eq 0 ]]; then
      echo "   ✓ $c"
      break
    fi
    # Already-published crates are not a failure — skip and continue.
    if echo "$out" | grep -qiE 'already exists|already been uploaded'; then
      echo "   ↻ $c already published — skipping"
      break
    fi
    # crates.io throttles publishing *new* crate names (all 25 here are new).
    # On HTTP 429, wait out the cooldown and retry the SAME crate (up to 6x).
    if echo "$out" | grep -qiE '429 Too Many Requests|too many new crates'; then
      if [[ $attempt -ge 6 ]]; then
        echo "   ⚠ $c still rate-limited after $attempt attempts — re-run later"
        exit 1
      fi
      # Default cooldown ~5 min covers the sliding window; first wait a bit longer.
      wait=300
      echo "   ⏳ $c rate-limited (429) — waiting ${wait}s, then retrying"
      sleep "$wait"
      continue
    fi
    echo "   ⚠ $c FAILED — see above; fix and re-run (published ones auto-skip)"
    exit 1
  done
done

echo ""
echo "✓ all ${#CRATES[@]} crates published to crates.io"
