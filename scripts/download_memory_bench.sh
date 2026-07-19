#!/usr/bin/env bash
# Download LoCoMo / LongMemEval benchmark JSON for the OneAI memory eval
# harness (docs/memory-mechanism.md §14). The harness consumes a JSONL file
# whose lines are `MemoryEvalCase` objects (see
# crates/oneai-eval/src/memory/case.rs); this script fetches the upstream
# benchmark's native format into ./bench-data/ for manual conversion.
#
# Usage:
#   scripts/download_memory_bench.sh locomo|longmemeval
#
# The native formats differ from OneAI's schema; a conversion step (LLM-assisted
# or scripted) is required before `oneai eval memory --suite jsonl --data <path>`
# will accept them. This script only fetches; it is not a build dependency.
set -euo pipefail

WHICH="${1:-locomo}"
OUT_DIR="$(pwd)/bench-data"
mkdir -p "$OUT_DIR"

case "$WHICH" in
  locomo)
    # Snap Research LoCoMo (arXiv:2402.17753) — very long-term conversational memory.
    URL="https://raw.githubusercontent.com/snap-research/LoCoMo/main/data/locomo10.json"
    OUT="$OUT_DIR/locomo10.json"
    ;;
  longmemeval)
    # LongMemEval (arXiv:2410.10813) — 500 questions, 5 abilities.
    URL="https://raw.githubusercontent.com/xiaowu0162/LongMemEval/main/data/longmemeval_s.json"
    OUT="$OUT_DIR/longmemeval_s.json"
    ;;
  *)
    echo "Usage: $0 locomo|longmemeval" >&2
    exit 1
    ;;
esac

echo "Fetching $WHICH → $OUT"
if curl -fsSL "$URL" -o "$OUT"; then
  echo "OK: $(wc -c < "$OUT") bytes"
  echo "Convert to OneAI MemoryEvalCase JSONL (one object per line) before running:"
  echo "  oneai eval memory --suite jsonl --data <converted.jsonl>"
else
  echo "Download failed (network or path changed). Inspect upstream repo:" >&2
  echo "  LoCoMo:     https://github.com/snap-research/LoCoMo" >&2
  echo "  LongMemEval: https://github.com/xiaowu0162/LongMemEval" >&2
  exit 1
fi
