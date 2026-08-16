#!/usr/bin/env bash
# build-web.sh — build the webUI dist and stage it into the npm package's
# `web-dist/`. Run by `npm publish` (prepublishOnly) so the published tarball
# carries the prebuilt, platform-independent SPA. `oneai web` (the binary)
# reads `ONEAI_WEB_DIST` (set by bin/oneai.js to this dir) to serve it.
#
# Idempotent: safe to re-run. Requires node + npm at publish time (on the
# maintainer's machine). The dist stays out of git (.gitignore).

set -euo pipefail

# Resolve paths relative to this script (platforms/npm/scripts).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NPM_DIR="$SCRIPT_DIR/.."          # platforms/npm
WEB_DIR="$NPM_DIR/../web"          # platforms/web
DIST_DIR="$NPM_DIR/web-dist"

echo "[build-web] building webUI in $WEB_DIR"
cd "$WEB_DIR"

if [ ! -d node_modules ]; then
  echo "[build-web] installing deps (npm ci)"
  npm ci
fi

echo "[build-web] npm run build"
npm run build

echo "[build-web] staging dist -> $DIST_DIR"
rm -rf "$DIST_DIR"
mkdir -p "$DIST_DIR"
# Copy the build output (Vite emits dist/). cp -a preserves perms/structure.
cp -a dist/. "$DIST_DIR/"

echo "[build-web] done — $(find "$DIST_DIR" -type f | wc -l | tr -d ' ') files staged"
