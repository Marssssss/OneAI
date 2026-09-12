#!/usr/bin/env bash
# MVS3 验收一键跑：前置检查 → 起 Pg 测试容器（若无）→ 构建宿主机侧二进制 →
# 跑 mvs3_verify.mjs → 兜底清理。
#
# 用法（仓库根目录）：
#   ./deploy/docker/mvs3_run.sh [--bin target/debug/oneai] [--release] [--keep]
#
# 前置：docker daemon（colima 即可）、镜像 oneai-engine:mvs1（**必须带
#      postgres feature 重建**：docker build -f deploy/docker/Dockerfile
#      -t oneai-engine:mvs1 .）、~/.oneai/config.toml、node ≥18、
#      platforms/web/node_modules/ws（npm i 于 platforms/web）。
set -euo pipefail
cd "$(dirname "$0")/../.."

BIN="target/debug/oneai"
EXTRA_ARGS=()
while [[ $# -gt 0 ]]; do
  case "$1" in
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

echo "── 确保 Pg 测试容器（oneai-pg-test，-p 5432:5432 + 库 oneai_mvs3）──"
if ! docker inspect oneai-pg-test >/dev/null 2>&1; then
  docker run -d --name oneai-pg-test -p 5432:5432 \
    -e POSTGRES_PASSWORD=oneai -e POSTGRES_DB=oneai_test postgres:16 >/dev/null
  echo "   等待 Pg 就绪…"
  for _ in $(seq 1 30); do
    docker exec oneai-pg-test pg_isready -U postgres >/dev/null 2>&1 && break
    sleep 1
  done
fi
docker exec oneai-pg-test psql -U postgres -tAc \
  "SELECT 1 FROM pg_database WHERE datname='oneai_mvs3'" | grep -q 1 || \
  docker exec oneai-pg-test psql -U postgres -c "CREATE DATABASE oneai_mvs3;" >/dev/null
# 容器内到宿主 Pg：colima 无 host.docker.internal 自动注入，用 bridge 网关。
docker run --rm postgres:16 psql "postgres://postgres:oneai@172.17.0.1:5432/oneai_mvs3" \
  -tAc "SELECT 'container→host Pg OK';" >/dev/null || {
  echo "容器经 172.17.0.1:5432 到宿主 Pg 不通（检查 -p 5432:5432 绑定）"; exit 2; }

echo "── 构建宿主机侧二进制（编排器进程用；postgres feature 非必需——编排器不触 Pg）──"
cargo build -p oneai-cli

echo "── 跑验收 ──"
set +e
node deploy/docker/mvs3_verify.mjs --bin "$BIN" "${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"}"
RC=$?
set -e

echo "── 兜底清理 oneai-orch-* 残留 ──"
"$BIN" orchestrator cleanup || true

exit "$RC"
