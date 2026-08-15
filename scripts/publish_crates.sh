#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────
# OneAI crates.io publish — dependency-topological order computed at run
# time via Kahn's algorithm over `cargo metadata` (normal+build+dev deps).
# Dynamic so the list never goes stale when crates are added/removed — an
# earlier hardcoded list omitted newer crates (oneai-vector etc.), breaking
# oneai-rag's publish ("no matching package oneai-vector found").
#
# Prereq: `cargo login` (paste your crates.io API token once), OR set
# CARGO_REGISTRY_TOKEN in the env.
#
#   cargo login
#   ./scripts/publish_crates.sh
#
# Idempotent: if a crate is already on the registry (e.g. re-running after
# a partial failure), the script detects "already exists" and continues.
# ──────────────────────────────────────────────────────────────────────
set -uo pipefail

# Always run cargo from the workspace root (script lives in scripts/).
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1

# Publish order — Kahn topological sort over the current workspace graph.
# A crate publishes before every workspace crate that depends on it.
CRATES=()
while IFS= read -r c; do
  CRATES+=("$c")
done < <(python3 - <<'PY'
import json, subprocess, sys
md = json.loads(subprocess.check_output(["cargo", "metadata", "--no-deps", "--format-version", "1"]))
# publishable = `publish` field is None (publish=false → [] excluded).
members = {p["name"] for p in md["packages"] if p.get("publish") is None}
deps = {m: set() for m in members}
for p in md["packages"]:
    if p["name"] not in members:
        continue
    for d in p.get("dependencies", []):
        if d["name"] in members:
            deps[p["name"]].add(d["name"])
remaining = {m: set(s) for m, s in deps.items()}
order = []
while remaining:
    ready = sorted(n for n, d in remaining.items() if not d)
    if not ready:
        sys.stderr.write(
            "publish_crates: dependency cycle among: "
            + ", ".join(sorted(remaining)) + "\n"
        )
        sys.exit(1)
    n = ready[0]
    order.append(n)
    del remaining[n]
    for m in remaining:
        remaining[m].discard(n)
print("\n".join(order))
PY
)

if [[ ${#CRATES[@]} -eq 0 ]]; then
  echo "ERROR: no crates computed (cargo metadata / python3 failed?)" >&2
  exit 1
fi
echo "── publish order (${#CRATES[@]} crates): ${CRATES[*]}"
echo

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
    # crates.io throttles publishing NEW crate names (the first publish of a
    # crate id). On HTTP 429, wait out the cooldown and retry the SAME crate.
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
