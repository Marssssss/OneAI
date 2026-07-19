#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────
# OneAI FastEmbed model pre-fetch
#
# The `fastembed` crate downloads its ONNX models from HuggingFace via the
# `hf-hub` crate, whose `ureq` client ignores `https_proxy`/`HTTPS_PROXY` env
# vars on some setups — so behind a proxy that only curl (not ureq) honors,
# the one-time download fails with "Failed to retrieve model.onnx" even
# though huggingface.co is reachable.
#
# This script pre-fetches the exact files fastembed needs, via curl (which
# respects your proxy env), into the hf-hub cache layout, so fastembed finds
# them cached and runs fully offline afterwards.
#
# Run once per machine. Default model: AllMiniLML6V2 (384-dim, ~22MB) — the
# auto-detection chain's keyless last resort, so no-API-key users get real
# semantic memory/RAG.
#
# Usage:
#   ./scripts/download_fastembed_models.sh            # default model
#   ./scripts/download_fastembed_models.sh bge-base   # bge-base-en-v1.5
# Env:
#   HF_HOME  cache root (default ~/.cache/huggingface)
#   + standard curl proxy env (HTTPS_PROXY/ALL_PROXY/NO_PROXY)
# ──────────────────────────────────────────────────────────────────────
set -euo pipefail

HF_HOME="${HF_HOME:-$HOME/.cache/huggingface}"
CACHE="$HF_HOME/hub"
# tokenizer + config files at snapshot root
FILES="tokenizer.json config.json special_tokens_map.json tokenizer_config.json"

MODEL_KEY="${1:-all-MiniLM-L6-v2}"
case "$MODEL_KEY" in
  all-MiniLM-L6-v2) REPO="Qdrant/all-MiniLM-L6-v2-onnx"; ONNX_FILE="model.onnx" ;;
  bge-base|bge-base-en-v1.5) REPO="Xenova/bge-base-en-v1.5"; MODEL_KEY="bge-base-en-v1.5"; ONNX_FILE="onnx/model.onnx" ;;
  mxbai|mxbai-embed-large-v1) REPO="mixedbread-ai/mxbai-embed-large-v1"; MODEL_KEY="mxbai-embed-large-v1"; ONNX_FILE="model.onnx" ;;
  *) echo "Unknown model key '$MODEL_KEY'. Known: all-MiniLM-L6-v2, bge-base, mxbai" >&2; exit 1 ;;
esac

# Resolve the commit hash for `main` via the HF API (curl honors proxy env).
SHA=$(curl -fsSL -m 15 "https://huggingface.co/api/models/${REPO}/revision/main" \
  | python3 -c "import sys,json;print(json.load(sys.stdin)['sha'])")

CACHE_DIR="$CACHE/models--${REPO//\//--}"
SNAPSHOT_DIR="$CACHE_DIR/snapshots/$SHA"
REFS_DIR="$CACHE_DIR/refs"
mkdir -p "$SNAPSHOT_DIR" "$REFS_DIR"
echo "$SHA" > "$REFS_DIR/main"

BASE="https://huggingface.co/${REPO}/resolve/${SHA}"
echo "Pre-fetching fastembed model: $REPO @ ${SHA:0:12}"
echo "  → $SNAPSHOT_DIR"
for f in $FILES; do
  if [[ -f "$SNAPSHOT_DIR/$f" ]]; then
    echo "  ✓ $f (cached)"
    continue
  fi
  echo "  ↓ $f"
  if ! curl -fsSL -m 120 -o "$SNAPSHOT_DIR/$f" "${BASE}/${f}"; then
    echo "    (skipped — not present in this repo: $f)" >&2
    rm -f "$SNAPSHOT_DIR/$f"
  fi
done

# The ONNX model file may live in a subdir (e.g. Xenova repos use onnx/model.onnx).
ONNX_DIR="$SNAPSHOT_DIR/$(dirname "$ONNX_FILE")"
mkdir -p "$ONNX_DIR"
if [[ -f "$SNAPSHOT_DIR/$ONNX_FILE" ]]; then
  echo "  ✓ $ONNX_FILE (cached)"
else
  echo "  ↓ $ONNX_FILE"
  if ! curl -fsSL -m 600 -o "$SNAPSHOT_DIR/$ONNX_FILE" "${BASE}/${ONNX_FILE}"; then
    echo "    (FAILED — could not fetch $ONNX_FILE from $REPO)" >&2
    exit 1
  fi
fi

echo
echo "Done. fastembed will now load '$MODEL_KEY' from cache (offline)."
echo "Verify: cargo test -p oneai-rag -- --ignored test_fastembed_service_embed"
