#!/usr/bin/env bash
# MVS2 验收一键跑：前置检查 → 构建宿主机侧编排器二进制 → 跑 mvs2_verify.mjs → 兜底清理。
#
# 用法（仓库根目录）：
#   ./deploy/docker/mvs2_run.sh [--sessions 10] [--bin target/debug/oneai] [--release]
#
# 前置：docker daemon（colima 即可）、镜像 oneai-engine:mvs1（见 deploy/docker/README.md）、
#      ~/.oneai/config.toml（容器内引擎的 provider 配置）、node ≥18、
#      platforms/web/node_modules/ws（npm i 于 platforms/web）。
set -euo pipefail
cd "$(dirname "$0")/../.."

SESSIONS="10"
BIN="target/debug/oneai"
EXTRA_ARGS=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --sessions) SESSIONS="$2"; shift 2 ;;
    --bin) BIN="$2"; shift 2 ;;
    --release) BIN="target/release/oneai"; shift ;;
    *) EXTRA_ARGS+=("$1"); shift ;;
  esac
done

echo "── 前置检查 ──"
command -v docker >/dev/null || { echo "缺 docker CLI"; exit 2; }
docker info >/dev/null 2>&1 || { echo "docker daemon 不可达（colima start?）"; exit 2; }
docker image inspect oneai-engine:mvs1 >/dev/null 2>&1 || {
  echo "缺镜像 oneai-engine:mvs1 — 先构建：docker build -f deploy/docker/Dockerfile -t oneai-engine:mvs1 ."
  exit 2
}
[[ -f "$HOME/.oneai/config.toml" ]] || { echo "缺 ~/.oneai/config.toml"; exit 2; }
command -v node >/dev/null || { echo "缺 node（≥18，需内置 fetch）"; exit 2; }
[[ -d platforms/web/node_modules/ws ]] || { echo "缺 platforms/web/node_modules/ws — cd platforms/web && npm i"; exit 2; }

echo "── 构建宿主机侧二进制（引擎镜像不变，无需重建）──"
cargo build -p oneai-cli

echo "── 跑验收（${SESSIONS} 会话）──"
set +e
node deploy/docker/mvs2_verify.mjs --bin "$BIN" --sessions "$SESSIONS" "${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"}"
RC=$?
set -e

echo "── 兜底清理 oneai-orch-* 残留 ──"
"$BIN" orchestrator cleanup || true

exit "$RC"
