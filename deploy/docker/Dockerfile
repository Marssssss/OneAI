# syntax=docker/dockerfile:1
#
# OneAI 云端会话容器镜像（MVS1 —— 见 docs/cloud-orchestrator-design.md §6）。
#
# 形态：一个容器 = 一个 `oneai web` 引擎进程 = 一个会话（D1/D2 决策）。
# 多阶段构建：rust:bookworm 内从源码编 release 二进制（BuildKit 缓存挂载
# 加速增量），运行时基于 debian:bookworm-slim。
#
# 构建（仓库根目录）：
#   docker build -f deploy/docker/Dockerfile -t oneai-engine:mvs1 .
#
# 运行（每会话两个卷：状态卷 + 工作区卷）：
#   docker volume create oneai-sess1-state
#   docker volume create oneai-sess1-ws
#   docker run -d --name oneai-sess1 -p 18787:8787 \
#     -v oneai-sess1-state:/root/.oneai \
#     -v oneai-sess1-ws:/workspace \
#     -v ~/.oneai/config.toml:/root/.oneai/config.toml:ro \
#     oneai-engine:mvs1
#
# MVS1 注意（已验证，2026-09-09，colima/Ubuntu 24.04 guest/docker 29）：
# - provider 密钥经只读 bind-mount 的 config.toml 进入容器（本机验证专用
#   捷径；生产按设计文档 D5 改为编排器 env 注入 + Secret Manager）。
# - R4 结论：bwrap 全沙箱在 docker-in-VM（Ubuntu 24.04 guest）不可用——
#   非特权 uid_map 写入被 AppArmor 限制系统级拦截（VM 层非 root 同样失败），
#   --privileged/root 也止步 loopback RTM_NEWADDR。引擎已修：
#   BubblewrapBackend::is_available 改为运行期探测（sandbox.rs），失败自动
#   降级 RegexBackend（shell 可用，防护=正则黑名单；容器本身是隔离边界）。
#   生产纵深防御走 gVisor/kata runtimeClass（设计文档 §9 R4 / MVS4）。
# - 非 root 运行（USER oneai）：状态卷挂 /home/oneai/.oneai。

# trixie (Debian 13, GCC 14)：ort-sys（经 fastembed 引入）链接的 pyke 预编译
# ONNX 静态库需要 `__cxa_call_terminate`（GCC 13+ libstdc++ 符号），bookworm
# (GCC 12) 链接必失败（MVS1 实测）。运行时阶段同步用 trixie-slim 保证
# libstdc++ ABI 满足。
FROM rust:1-trixie AS builder

# crates 国内镜像（rsproxy sparse index）+ git 依赖走 CLI。
# 宿主代理由 ~/.docker/config.json 的 proxies 注入（BuildKit 预定义 ARG，
# RUN 环境自动可见）——ort-sys 的 libonnxruntime blob 等 github/pyke 下载
# 经代理兜底。直连 static.crates.io 在部分网络下会挂死（MVS1 实测）。
RUN printf '%s\n' \
    '[source.crates-io]' \
    "replace-with = 'rsproxy-sparse'" \
    '[source.rsproxy-sparse]' \
    'registry = "sparse+https://rsproxy.cn/index/"' \
    '[net]' \
    'git-fetch-with-cli = true' \
    > /usr/local/cargo/config.toml

WORKDIR /build

# 无 build.rs 依赖 .git（已核验），.dockerignore 排除 target/node_modules/.git。
COPY . .

# NK_TARGET_NEONSDOT=0：numkong 7.8.0（usearch 的 C 核）在 linux-aarch64+GCC
# 下 flag 配错——探测期用 -march=armv8.2-a+dotprod 判定支持并全局定义
# NK_TARGET_NEONSDOT=1，但 baseline TU（dispatch_f16.c 等）只有 -march=armv8-a，
# neonsdot.h 的 vdotq_s32 always_inline 目标属性不匹配 → GCC 编译失败
# （macOS/clang 无此限制故宿主构建正常）。build.rs 官方逃生口：环境变量
# 强制关闭该 SIMD 目标；运行期动态派发，仅损失 dotprod 内核加速，正确性
# 不受影响。x86_64 构建不受影响。
ENV NK_TARGET_NEONSDOT=0

# 缓存挂载：cargo registry + target 跨构建复用（首次构建仍为全量，
# fastembed/ort-sys 会在构建期下载 linux 版 libonnxruntime blob，需网络）。
# --features postgres（MVS3 存储外部化）：编入 PgWorkingStateStore +
# MVS3-B 的 PgMemoryStore(pgvector)/PgUsageTracker/PgHostAllowlist。运行期
# 设 ONEAI_PG_DSN（编排器 orchestrator.toml 的 passthrough_env 注入；容器内
# 访问宿主 Pg 用 host.docker.internal 而非 127.0.0.1）即把 working-state
# 事件日志、记忆/会话、用量、域名白名单全部写进共享 Postgres（容器/卷全丢
# 也能恢复；白名单跨容器共享）；Pg 服务器须为 pgvector 镜像（memory 硬依赖
# CREATE EXTENSION vector，缺失则该 store 响亮告警并单独回退 SQLite）。
# 不设 DSN 则维持文件/SQLite 后端，行为与 MVS1/2 完全一致。
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release -p oneai-cli --features oneai-cli/postgres && \
    install -m 0755 target/release/oneai /oneai

# ─── 运行时 ──────────────────────────────────────────────────────────────────

FROM debian:trixie-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        bubblewrap \
        python3 \
        git \
        curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /oneai /usr/local/bin/oneai

# web dist：`oneai web` 的 resolve_web_dist 顺序含 <exe_dir>/web-dist
# （cmd_web.rs），放二进制旁即自动发现；再设 env 双保险。
COPY platforms/web/dist /usr/local/bin/web-dist
ENV ONEAI_WEB_DIST=/usr/local/bin/web-dist

# 非 root 运行（安全基线 + MVS4 前置）。注意：非 root 并不足以让 bwrap 在
# Ubuntu 24.04 宿主上工作（AppArmor 非特权 userns 限制是系统级的，
# seccomp/apparmor=unconfined 与 --privileged 实测均无法恢复完整
# --unshare-all 路径）——引擎的 is_available 运行期探测会自动降级到
# RegexBackend，见上方 R4 结论。
RUN groupadd -g 1000 oneai && useradd -m -u 1000 -g oneai -s /bin/bash oneai

# 工作区 = 会话的项目目录（文件后端时 working-state 落 <cwd>/.oneai，见
# cmd_app_server.rs；MVS3 起设 ONEAI_PG_DSN 则事件日志进共享 Postgres，
# 卷内只剩 SQLite/场景/会话事件等）；~/.oneai = 状态卷。
RUN mkdir -p /workspace /home/oneai/.oneai \
    && chown oneai:oneai /workspace /home/oneai/.oneai

USER oneai
ENV HOME=/home/oneai
WORKDIR /workspace

EXPOSE 8787

# 单会话引擎：SPA + JSON-RPC /ws 同端口（serve_web）。
CMD ["oneai", "web", "--no-open", "--host", "0.0.0.0", "--port", "8787"]
