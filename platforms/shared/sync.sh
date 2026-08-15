#!/usr/bin/env bash
# sync.sh — regenerate the committed AUTO-GENERATED copies of the shared
# scenario editor from the single source of truth
# (platforms/shared/scenario-editor.js) into the two webview roots that load
# it as a static `<script>`:
#   - platforms/vscode/src/webview/scenario-editor.js  (VS Code webview)
#   - platforms/browser/scenario-editor.js             (browser popup)
#
# The VS Code webview's `localResourceRoots` and the browser extension's
# package root are each confined to their own dir, so neither can load a
# sibling `../shared/` file at runtime — hence the committed copies. The
# copies ARE committed (so both extensions work out of the box without a
# build step), but carry the AUTO-GENERATED header + this drift check so a
# source edit without a re-sync fails CI. Run this after editing the source.
# (VS Code's `npm run compile` also re-syncs its own copy automatically.)
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SRC="$SCRIPT_DIR/scenario-editor.js"

VSDEST="$ROOT/platforms/vscode/src/webview/scenario-editor.js"
BDEST="$ROOT/platforms/browser/scenario-editor.js"

for dest in "$VSDEST" "$BDEST"; do
  mkdir -p "$(dirname "$dest")"
  cp "$SRC" "$dest"
  echo "synced → $dest"
done

# Fail (in CI) if any committed copy differs from source after sync.
if [[ -n "${CI:-}" ]]; then
  if ! diff -q "$SRC" "$VSDEST" >/dev/null || ! diff -q "$SRC" "$BDEST" >/dev/null; then
    echo "ERROR: scenario-editor.js copy out of date — run platforms/shared/sync.sh" >&2
    exit 1
  fi
fi
