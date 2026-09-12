# MVS1 容器化验证 Runbook（✅ 已验证通过 2026-09-09）+ MVS2 编排层（✅ 2026-09-10）

对应 `docs/cloud-orchestrator-design.md` §6 MVS1/MVS2；验证记录全文见该文档附录 A（MVS1）与附录 B（MVS2）。

**结论速览**：现有二进制零改动进容器 → ws 全链路（turn/工具/审批/流式）→
`docker kill` 后新容器挂同卷完整恢复（session/list + load + 历史感知回答）。
R4（bwrap-in-docker）：**不可用**（Ubuntu 24.04 宿主系统级 AppArmor 限制，
privileged 也无法恢复），引擎已补运行期探测自动降级 RegexBackend（见下）。

前置：colima（`colima start --cpu 6 --memory 10 --disk 80`）+ docker CLI +
docker-buildx 插件；`~/.docker/config.json` 配 `proxies`（BuildKit RUN 不继承
VM 代理 env，直连 static.crates.io 会挂死）。本机 macOS/arm64 → 镜像 linux/arm64。

## 1. 构建镜像

```bash
cd <repo 根>
docker build -f deploy/docker/Dockerfile -t oneai-engine:mvs1 .
docker run --rm oneai-engine:mvs1 oneai --version   # 冒烟
```

已固化的构建坑（Dockerfile 内有注释）：
- cargo fetch 挂死 → rsproxy 镜像源 + config.json 代理注入
- numkong 7.8.0 aarch64+GCC dotprod bug → `ENV NK_TARGET_NEONSDOT=0`
- ort-sys 链接缺 `__cxa_call_terminate` → 基础镜像用 trixie（GCC 14）

## 2. 起会话容器（每会话两卷，非 root）

```bash
docker volume create oneai-sess1-state
docker volume create oneai-sess1-ws
docker run -d --name oneai-sess1 -p 18787:8787 \
  -v oneai-sess1-state:/home/oneai/.oneai \
  -v oneai-sess1-ws:/workspace \
  -v ~/.oneai/config.toml:/home/oneai/.oneai/config.toml:ro \
  oneai-engine:mvs1
docker logs -f oneai-sess1   # 等 "✅ webUI ready"
```

预期启动日志含 `Using regex-based sandbox backend (platform-specific
isolation not available)` —— R4 降级生效的标志（Ubuntu 24.04 宿主）。
`--security-opt seccomp/apparmor=unconfined` 实测**不需要**（对 bwrap 无济
于事，引擎已自动降级）。

> MVS1 捷径：provider 密钥经只读 bind-mount config.toml 进容器。
> 生产按设计文档 D5 改编排器 env 注入 + Secret Manager。

## 3. 宿主机全链路验证（ws JSON-RPC）

```bash
V="node deploy/docker/mvs1_verify.mjs ws://127.0.0.1:18787/ws"   # bash；zsh 请整行执行

node deploy/docker/mvs1_verify.mjs ws://127.0.0.1:18787/ws list
node deploy/docker/mvs1_verify.mjs ws://127.0.0.1:18787/ws create --id mvs1 --workspace /workspace
node deploy/docker/mvs1_verify.mjs ws://127.0.0.1:18787/ws turn \
  "用 shell 工具运行: echo MVS1-SHELL-OK > shelltest.txt && cat shelltest.txt 然后原样报告输出" \
  --auto-approve --timeout 240
```

⚠️ 验证纪律：**以容器日志为准，别信模型叙述**——实测出现过模型把
read_file 结果口述成「shell cat 成功」。核对 `docker logs` 里的
`AgentLoop iteration ... ToolCalls completed` 与工具结果。

浏览器人工核验（可选）：`http://127.0.0.1:18787/`（容器同端口托管 SPA）。

## 4. R4：bwrap-in-docker（结论：不可用，已降级）

调查链（2026-09-09，colima VM = Ubuntu 24.04 guest，kernel 6.8.0-117）：

| 探测 | 结果 |
|---|---|
| 容器非 root，默认安全选项 | ❌ `No permissions to create a new namespace` |
| `seccomp/apparmor=unconfined` | ❌ `loopback: Failed RTM_NEWADDR` |
| `--privileged` + root | ❌ 同样 RTM_NEWADDR |
| VM 层 root `unshare -Urn` | ✅ 通过（内核无问题） |
| VM 层非 root | ❌ uid_map 写入 EPERM（系统级 AppArmor 限制） |

**引擎修复**（本验证的直接产出，`oneai-tool/src/sandbox.rs`）：
`BubblewrapBackend::is_available` 原来只查 `/usr/bin/bwrap` 存在 → 容器内
6/6 shell 调用全挂且无降级。已改为「存在 + 运行期探测（`OnceLock` 缓存，
探测参数与 `wrap_command` 同形）」，失败时 `default_sandbox_backend` 自动
落到 Docker→Regex。该修复对任何「bwrap 在但命名空间被禁」的环境
（Ubuntu 24.04 主机/CI runner/docker）普适。

生产纵深防御：容器即租户边界（设计 §8），更强隔离走 gVisor/kata
runtimeClass（MVS4）。

## 5. 杀容器恢复验证（✅ 已通过）

```bash
docker kill oneai-sess1 && docker rm oneai-sess1
docker run -d --name oneai-sess1b -p 18787:8787 \
  -v oneai-sess1-state:/home/oneai/.oneai \
  -v oneai-sess1-ws:/workspace \
  -v ~/.oneai/config.toml:/home/oneai/.oneai/config.toml:ro \
  oneai-engine:mvs1
# 等 ready 后：
node deploy/docker/mvs1_verify.mjs ws://127.0.0.1:18787/ws list   # mvs1 在，含 title+msgs
node deploy/docker/mvs1_verify.mjs ws://127.0.0.1:18787/ws load mvs1
node deploy/docker/mvs1_verify.mjs ws://127.0.0.1:18787/ws turn \
  "不要调用任何工具，直接回答：上一轮写入 shelltest.txt 的内容是什么？" --timeout 120
# 期望：模型从恢复历史答出 MVS1-SHELL-OK（实测 1 iteration，无工具调用）
docker exec oneai-sess1b cat /workspace/shelltest.txt             # 工作区文件仍在
```

## 6. 清理

```bash
docker rm -f oneai-sess1 oneai-sess1b 2>/dev/null
docker volume rm oneai-sess1-state oneai-sess1-ws
```

## 验收清单（2026-09-09 实测）

- [x] 镜像构建成功（linux/arm64，494MB），`oneai --version` 可跑
- [x] ws 直连：session/create + turn/run 全链路（stream/thinking/inference/turn_complete 事件齐全）
- [x] 工具调用 + 审批回路（approval_request → Proceed → tool_result；文件工具 + shell 工具）
- [x] shell 工具容器内执行成功（**经引擎 is_available 探测补丁后**，Regex 降级路径）
- [x] docker kill 后新容器挂同卷：session/list 可见 + session/load 回放 + 模型从恢复历史答出上一轮内容（零工具调用）
- [x] 工作区文件跨容器保留（/workspace 卷）；会话事件日志在 /workspace/.oneai/events/
- [x] R4 结论落档 + 引擎修复三件套绿（fmt/clippy/258 tests）

---

# MVS2 薄编排层（oneai-orchestrator，✅ 已验证通过 2026-09-10）

MVS1 的手工 `docker run` 由 `oneai-orchestrator` crate 接管：一会话一容器的
生命周期 FSM（D6）、路由表（内存 + `sessions.json` 原子持久化 + 启动对账）、
WS 反向代理（D3 纯透传）、Bearer 认证（`oneai-http-auth`，D7）。
**引擎与镜像零改动**——容器跑的还是 MVS1 的 `oneai-engine:mvs1`。

## 7. 启动编排器

```bash
export ONEAI_ORCHESTRATOR_SECRET=<前端接入密钥>
target/debug/oneai orchestrator serve \
  --listen 127.0.0.1:9191 \
  --provider-config ~/.oneai/config.toml   # 容器内引擎的 provider 配置（ro bind-mount）
# 可选：~/.oneai/orchestrator.toml（字段见 crates/oneai-orchestrator/src/config.rs 顶部示例）
# --image 默认 oneai-engine:mvs1；--idle-timeout 默认 1800s（0 禁用休眠）
```

## 8. 控制面 API（Bearer 认证）

```bash
H="Authorization: Bearer $ONEAI_ORCHESTRATOR_SECRET"
curl -s -H "$H" -H 'Content-Type: application/json' \
  -d '{"session_id":"demo1"}' http://127.0.0.1:9191/v1/sessions   # 创建（阻塞至容器就绪）
curl -s -H "$H" http://127.0.0.1:9191/v1/sessions                 # 列表
curl -s -H "$H" http://127.0.0.1:9191/v1/sessions/demo1           # 单会话状态
curl -s -X DELETE -H "$H" http://127.0.0.1:9191/v1/sessions/demo1 # 销毁（容器+卷）
curl -s http://127.0.0.1:9191/healthz                             # 无需认证
```

前端接入：`ws://127.0.0.1:9191/v1/sessions/<id>/ws?token=<secret>`（浏览器
无法为 ws 握手设 header，故支持 `?token=`）。之后就是与 MVS1 完全相同的
JSON-RPC 协议（session/create、turn/run、approval/respond……原样透传）。
CLI 同款：`oneai orchestrator create/list/status/destroy`。

会话状态机：`Creating→Running⇄Hibernating/Resuming`，容器死→`Crashed`
（下次前端请求自动重拉新容器挂同卷）；idle 超时自动 `docker stop` 休眠
（卷保留），请求到达自动唤醒。编排器重启后按容器实况对账路由表
（活的重挂 Running，死的标 Crashed 等懒恢复）。

## 9. MVS2 全量验收（一键）

```bash
./deploy/docker/mvs2_run.sh --sessions 10
# 等价于：node deploy/docker/mvs2_verify.mjs --bin target/debug/oneai --sessions 10
# 残留清理：target/debug/oneai orchestrator cleanup
```

脚本自包含（自起/自重重启/自关停编排器进程），验收矩阵见附录 B：
10 并发会话容器、每会话经反代跑真实 turn、`docker kill` 后重连自动
Resuming + 卷内文件与会话历史完整、编排器进程重启后 10/10 重挂、
DELETE 后容器与卷零残留。**验证纪律同 MVS1**：以事件流与
`docker exec` 卷内文件为准，不信模型口述。

## 10. MVS2 安全边界（D7 的 MVS2 取舍）

- 前端→编排器：`ONEAI_ORCHESTRATOR_SECRET` Bearer（未设则拒绝启动）。
- 编排器→容器：容器端口只发布在 `127.0.0.1`（`-p 127.0.0.1:0:8787`，动态
  端口），同网段其他容器不可达；colima 实测动态端口可被宿主机转发访问。
- **每会话内部密钥推迟**：引擎 `/ws` 无认证钩子，加钩子违反「引擎零改动」
  约束——待引擎提供可选 ws 认证后补（MVS3+）。生产部署 TLS 由前置反代
  （Caddy/ALB）终止；容器永不直接暴露公网。
- `sessions.json` 可能含注入容器的 env 值，已 chmod 600；生产走 Secret
  Manager（D5/MVS4）。

---

# MVS3 存储外部化：PgWorkingStateStore（2026-09-12）

working-state 事件日志（崩溃恢复命脉）从每会话卷外部化到共享 Postgres：
容器/卷全丢也能恢复未完成任务；brief 索引与事件 INSERT 同事务（多写者
安全）。机制见 `docs/working-state-mechanism.md` §14。

## 11. 镜像重建（必须带 postgres feature）

```bash
docker build -f deploy/docker/Dockerfile -t oneai-engine:mvs1 .
# Dockerfile 已改为 cargo build --features oneai-cli/postgres
```

## 12. 起共享 Pg（验收/开发用一次性容器）

```bash
docker run -d --name oneai-pg-test -p 5432:5432 \
  -e POSTGRES_PASSWORD=oneai -e POSTGRES_DB=oneai_test postgres:16
docker exec oneai-pg-test psql -U postgres -c "CREATE DATABASE oneai_mvs3;"
```

**DSN 主机名注意**：容器内访问宿主 Pg——Docker Desktop 用
`host.docker.internal`；**colima 无自动注入**（DockerRunner argv 不带
`--add-host`），用 bridge 网关 `172.17.0.1`：
`postgres://postgres:oneai@172.17.0.1:5432/oneai_mvs3`。

## 13. DSN 注入（编排器零代码改动，两条路）

```toml
# 路 A：~/.oneai/orchestrator.toml —— 编排器自身环境里有 ONEAI_PG_DSN 时
passthrough_env = ["ONEAI_PG_DSN"]
```

路 B（验收脚本用）：`POST /v1/sessions` body 的 `env` 字段 per-session 注入
`{"env": {"ONEAI_PG_DSN": "postgres://…"}}`。

引擎容器启动日志出现 `working-state: Postgres (shared)` 即选中 Pg；DSN 设了
但不可用 → 响亮警告 + 诚实降级文件后端（不会静默分叉）。

## 14. MVS3 全量验收（一键）

```bash
./deploy/docker/mvs3_run.sh
# 等价于：node deploy/docker/mvs3_verify.mjs --bin target/debug/oneai
```

验收矩阵（A-F）：per-session env 注入建会话 → 引擎日志证后端选择 → psql
种子未完成任务 + 容器内 `oneai tasks list` 真读 Pg → 新会话真实 turn 首轮
surface 种子任务（引擎 `list_open_tasks` 走 Pg）→ **kill 容器 + 删光两个卷**
→ 重连自动 Resuming → 空卷新容器仍从 Pg 恢复未完成任务（MVS3 核心卖点）→
DELETE 后容器/卷/种子行零残留。

## 15. Pg 集成测试（开发侧）

```bash
ONEAI_TEST_PG_DSN=postgres://postgres:oneai@127.0.0.1:5432/oneai_test \
  cargo test -p oneai-persistence --features postgres \
  --test pg_working_state -- --ignored
# 10 测：镜像文件后端 8 项 + 同任务 10 并发 append / 双任务并行隔离
```
