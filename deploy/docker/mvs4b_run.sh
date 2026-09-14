#!/usr/bin/env bash
# MVS4-B 验收一键跑：前置检查 → 起 pgvector 测试容器（若无）→ 建库 oneai_mvs4b
# → 构建宿主机侧二进制（带 postgres feature）→ 确认引擎镜像 oneai-engine:mvs4b
# （缺失即重建——本轮引擎侧有用量打标+OTEL 改动，不能复用 mvs3c）→ 跑
# mvs4b_verify.mjs → 兜底清理。
#
# 用法（仓库根目录）：
#   ./deploy/docker/mvs4b_run.sh [--bin target/debug/oneai] [--release] [--keep]
#
# 前置：docker daemon（colima 即可）、~/.oneai/config.toml、node ≥18、
#      platforms/web/node_modules/ws（npm i 于 platforms/web）、
#      Pg 测试容器必须为 pgvector 镜像（pgvector/pgvector:pg16）、
#      宿主 :4318 空闲（OTLP 捕获桩）。
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
[[ -f "$HOME/.oneai/config.toml" ]] || { echo "缺 ~/.oneai/config.toml"; exit 2; }
command -v node >/dev/null || { echo "缺 node（≥18，需内置 fetch）"; exit 2; }
[[ -d platforms/web/node_modules/ws ]] || { echo "缺 platforms/web/node_modules/ws — cd platforms/web && npm i"; exit 2; }

echo "── 确保 pgvector 测试容器（oneai-pg-test，-p 5432:5432 + 库 oneai_mvs4b）──"
if docker inspect oneai-pg-test >/dev/null 2>&1; then
  PG_IMAGE=$(docker inspect oneai-pg-test --format '{{.Config.Image}}')
  [[ "$PG_IMAGE" == pgvector/* ]] || {
    echo "oneai-pg-test 用的是 $PG_IMAGE（无 pgvector）——重建：docker rm -f oneai-pg-test 后重跑本脚本"
    exit 2
  }
else
  docker run -d --name oneai-pg-test -p 5432:5432 \
    -e POSTGRES_PASSWORD=oneai -e POSTGRES_DB=oneai_test pgvector/pgvector:pg16 >/dev/null
  echo "   等待 Pg 就绪…"
  for _ in $(seq 1 30); do
    docker exec oneai-pg-test pg_isready -U postgres >/dev/null 2>&1 && break
    sleep 1
  done
fi
docker exec oneai-pg-test psql -U postgres -tAc \
  "SELECT 1 FROM pg_database WHERE datname='oneai_mvs4b'" | grep -q 1 || \
  docker exec oneai-pg-test psql -U postgres -c "CREATE DATABASE oneai_mvs4b;" >/dev/null
docker exec oneai-pg-test psql -U postgres -d oneai_mvs4b -tAc \
  "CREATE EXTENSION IF NOT EXISTS vector; SELECT 'pgvector OK';" >/dev/null || {
  echo "oneai-pg-test 无法 CREATE EXTENSION vector（镜像不对？）"; exit 2; }
docker run --rm pgvector/pgvector:pg16 psql "postgres://postgres:oneai@172.17.0.1:5432/oneai_mvs4b" \
  -tAc "SELECT 'container→host Pg OK';" >/dev/null || {
  echo "容器经 172.17.0.1:5432 到宿主 Pg 不通（检查 -p 5432:5432 绑定）"; exit 2; }

echo "── 构建宿主机侧二进制（编排器进程；带 postgres feature）──"
cargo build -p oneai-cli --features postgres

echo "── 确保引擎镜像 oneai-engine:mvs4b（MVS4-B 引擎侧有改动，缺失即重建）──"
if ! docker image inspect oneai-engine:mvs4b >/dev/null 2>&1; then
  echo "   镜像不存在 → docker build（release 全量编译，首轮较慢）…"
  docker build -f deploy/docker/Dockerfile -t oneai-engine:mvs4b .
fi

echo "── 跑 MVS4-B 验收 ──"
set +e
node deploy/docker/mvs4b_verify.mjs --bin "$BIN" "${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"}"
RC=$?
set -e

echo "── 兜底清理 oneai-orch-* 残留 ──"
"$BIN" orchestrator cleanup || true

exit "$RC"
