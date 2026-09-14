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

**MVS3-B 起必须用 pgvector 镜像**：`PgMemoryStore` 硬依赖
`CREATE EXTENSION vector`（LTM 服务端 KNN），普通 `postgres:16` 上 connect
失败 → 引擎告警并回退 SQLite memory（其余三 store 不受影响，各自独立降级）。

```bash
docker run -d --name oneai-pg-test -p 5432:5432 \
  -e POSTGRES_PASSWORD=oneai -e POSTGRES_DB=oneai_test pgvector/pgvector:pg16
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
但不可用 → 响亮警告 + 诚实降级文件后端（不会静默分叉）。MVS3-B 三 store
（memory/usage/host-allowlist）同理，各打一行选择日志、各自独立降级：
`memory: Postgres (shared)` / `usage: Postgres (shared)` /
`host-allowlist: Postgres (shared)`。

## 14. MVS3 全量验收（一键，两轮）

```bash
./deploy/docker/mvs3_run.sh    # A 轮：PgWorkingStateStore（任务恢复）
./deploy/docker/mvs3b_run.sh   # B 轮：Memory/Usage/HostAllowlist（记忆恢复）
# 等价于：node deploy/docker/mvs3_verify.mjs  --bin target/debug/oneai
#         node deploy/docker/mvs3b_verify.mjs --bin target/debug/oneai
```

A 轮验收矩阵（A-F）：per-session env 注入建会话 → 引擎日志证后端选择 → psql
种子未完成任务 + 容器内 `oneai tasks list` 真读 Pg → 新会话真实 turn 首轮
surface 种子任务（引擎 `list_open_tasks` 走 Pg）→ **kill 容器 + 删光两个卷**
→ 重连自动 Resuming → 空卷新容器仍从 Pg 恢复未完成任务（MVS3 核心卖点）→
DELETE 后容器/卷/种子行零残留。

B 轮验收矩阵（A-G）：建会话 → 引擎日志证**四后端**全选 Pg → 固定会话 id 真实
turn 记暗号 + psql 地面真值（conversations_pg/usage_records_pg 落行、
session/list·session/rename 走 Pg）→ 容器1 `host/allow` 容器2 `host/list`
可见（白名单跨容器共享）+ deny 互斥 → **kill 容器 + 删光两个卷** → 空卷新容器
session/list·session/load 从 Pg 恢复会话，真实 turn 答出暗号（记忆跨容器死亡
存活），usage 继续累计，rename 存活 → 宿主侧指向**无 pgvector** 的 postgres:16
（临时容器，端口 5433）：memory 响亮告警回退 SQLite、其余三 store 照常选 Pg
（独立降级）→ DELETE 后容器/卷/验收行零残留。

## 15. Pg 集成测试（开发侧）

```bash
ONEAI_TEST_PG_DSN=postgres://postgres:oneai@127.0.0.1:5432/oneai_test \
  cargo test -p oneai-persistence --features postgres \
  --test pg_working_state --test pg_memory_store \
  --test pg_usage_tracker --test pg_host_allowlist -- --ignored
# pg_working_state  10 测：镜像文件后端 8 项 + 同任务 10 并发 / 双任务并行
# pg_memory_store   13 测：镜像 SqliteSessionStore（STM/LTM/会话/丢弃快照/
#                   facts）+ pgvector KNN 相似度/维度隔离 + 并发 + 重连存活
# pg_usage_tracker   8 测：镜像 SqliteUsageTracker + is_estimated roundtrip
# pg_host_allowlist  8 测：镜像 SqliteHostAllowlist（互斥/重开存活/list·remove）
```

---

# MVS3-C 收尾：session-events/feedback Pg + 深度休眠卷归档 + 瘦身/预热（2026-09-13）

存储外化收口（trajectory 事件日志 + per-message feedback 进共享 Pg）；编排器
二级深度休眠（冷会话卷归档后删容器删卷）；云镜像去 ONNX 链 + web dist shiki
裁剪 + 引擎启动预热。

## 16. 镜像瘦身（feature 组合变化）

```bash
# 云镜像（MVS3-C 起）：--no-default-features 关掉 oneai-cli 默认的
# oneai-rag/fastembed —— ort-sys/libonnxruntime 整条静态链不进镜像；
# 嵌入退化到 API provider（OPENAI/VOYAGE/ONEAI_EMBEDDING_* / Ollama）或
# 关键词召回；配置显式选 fastembed 时引擎响亮告警并优雅降级。
docker build -f deploy/docker/Dockerfile -t oneai-engine:mvs3c .
# 端侧宿主二进制不受影响（default feature 含 fastembed，行为与既往一致）：
cargo build -p oneai-cli --features postgres
```

- apt 不再装 bubblewrap（R4 已证容器内不可用；引擎 `is_available` 运行期
  探测自动落 RegexBackend，装了也选不中）。
- web dist 由 shiki 细粒度打包（12 语言 + 2 主题）从 27MB/627 文件缩到
  ~7MB/5 文件；镜像 COPY 前先 `cd platforms/web && npm run build`。
- 启动预热：引擎在监听 bind 前完成 `warm_model_context`（容器日志
  `prewarm: model context ready`；30s 超时兜底不阻塞健康化）——编排器
  TCP 探活通过即引擎就绪。

## 17. 深度休眠卷归档（二级休眠）

```toml
# ~/.oneai/orchestrator.toml（或 CLI --deep-archive-timeout/--archive-dir）
idle_timeout_secs = 1800          # 一级：docker stop，卷保留本地，秒级恢复
deep_archive_timeout_secs = 86400 # 二级：Hibernating 再超此时长 → 归档+删卷删容器
archive_dir = "/srv/oneai-archive"  # 卷归档存储根（可挂 NFS/云盘；S3 留 MVS4）
```

- 归档/恢复经一次性 `alpine:3.20` helper 容器 tar czf/xzf（离线环境须预拉
  该镜像）；布局 `<archive_dir>/<session_id>/<volume>.tar.gz + manifest.json`。
- **`archive_dir` 必须对 docker daemon 可见**（bind-mount 源）：Linux 宿主
  任意路径；colima/Docker Desktop 的 VM 只挂载 `$HOME` 与 `/tmp`——macOS 的
  `$TMPDIR`（`/var/folders/…`）**不可用**（daemon 以自动创建的空目录顶替
  bind 源，tar 产物落进 VM 侧，宿主永远看不到；验收脚本对此有 canary 前置
  检查）。导出 argv 在容器内 `mkdir -p` 会话子目录，不依赖宿主目录可见性。
- **红线**：归档确认成功才 `destroy(remove_volumes=true)`；失败保持
  Hibernating 卷不动、`last_error` 记诊断、下轮 sweep 重试。
- resume 检测 `archived` 标记 → 恢复卷 → spawn → Running 后清标记删归档；
  `oneai orchestrator list` 显示 `deep-archived (N volume(s), <时间>)`。
- 所有权注意：tar 在 helper 容器内以 root 写盘——Linux 宿主非 root 编排器
  可能无法 chmod/删除归档文件（导出后尽力 chmod 644）。启用深度归档时请以
  root 跑编排器，或把 `archive_dir` 指到做 uid 映射的共享挂载（colima/macOS
  与 NFS squash 挂载均可）。

## 18. MVS3-C 全量验收（一键）

```bash
./deploy/docker/mvs3c_run.sh   # 构建 mvs3c 镜像 + 宿主二进制 + 跑 mvs3c_verify.mjs
```

C 轮验收矩阵（A-H）：建会话 → 引擎日志证**六后端**全选 Pg（+session-events/
+feedback）且出现 prewarm 行 → 真实 turn 记暗号 + `feedback/submit`×2 落
`message_feedback_pg` + `session/trajectory` 落 `session_events_pg`（psql
地面真值）→ 镜像尺寸断言 → **深度归档全生命周期**（第二编排器
idle=5s/deep=5s：写 proof 文件 → 断连自动 Hibernating → 自动 deep-archive
（容器+两卷从 docker 消失、tar.gz+manifest 落盘、status.archived 带
manifest）→ WS 重连自动 restore+spawn → `docker exec cat` proof 逐字存活 →
标记清除+归档删除 → turn 答出归档前暗号）→ **归档失败红线**（archive_dir
置只读：保持 Hibernating 卷一个不少 + last_error 诊断；恢复可写下轮归档成功
再 resume）→ S1 kill+删光两卷 → 仅凭 Pg 恢复会话列表 + feedback/trajectory
跨卷死亡存活 + turn 答出暗号 → DELETE 全部零残留。

## 19. Pg 集成测试（C 轮新增两个 store）

```bash
ONEAI_TEST_PG_DSN=postgres://postgres:oneai@127.0.0.1:5432/oneai_test \
  cargo test -p oneai-persistence --features postgres \
  --test pg_session_event_store --test pg_feedback_store -- --ignored
# pg_session_event_store 5 测：字节级 round-trip / 会话隔离 / 空载 /
#                        重连存活 / 10×5 并发零丢失（BIGSERIAL 全序）
# pg_feedback_store      4 测：round-trip+scoping / 空载 / 重连存活 / 8 并发
```

## 20. MVS4-A 多副本编排器（Pg 共享路由表 + 每会话租约）

引擎侧零改动（镜像直接复用 `oneai-engine:mvs3c`）；变化全在宿主侧编排器。

```bash
cargo build -p oneai-cli --features postgres   # 聚合 oneai-orchestrator/postgres

# 副本 A（宿主终端 1）
ONEAI_ORCHESTRATOR_SECRET=... \
ONEAI_PG_DSN=postgres://postgres:oneai@127.0.0.1:5432/oneai_mvs4 \
  ./target/debug/oneai orchestrator serve --listen 127.0.0.1:9196 --replica-id rep-a

# 副本 B（宿主终端 2；同 DSN，异 listen；--replica-id 缺省=启动生成 uuid）
ONEAI_ORCHESTRATOR_SECRET=... \
ONEAI_PG_DSN=postgres://postgres:oneai@127.0.0.1:5432/oneai_mvs4 \
  ./target/debug/oneai orchestrator serve --listen 127.0.0.1:9197 --replica-id rep-b --lease-ttl 30
```

语义速记（详见设计文档 §6 MVS4-A + 附录 F）：

- **路由表**进共享 Pg（`orchestrator_sessions`，锁 key base+6）；文件模式
  （不设 DSN）行为与 MVS2/3 逐位一致。banner 打印 Backend/Replica/lease。
- **所有权是 claim-on-act**：WS 接入/清扫/归档/对账在动作前 claim 租约，
  动作期间 `LeaseGuard` 心跳（ttl/3），结束即释放或任其过期——不粘滞，
  闲置会话任意副本可即时接管。
- **WS 打错副本** → `409` + `X-Oneai-Owner-Replica: <owner>` +
  `Retry-After: 1`（前置 LB 可据此学 sticky；租约过期后重试即被接管）。
- **副本死亡**（kill -9）→ 租约过期 → 其他副本 WS 接入或重启对账时接管，
  **容器零操作**（引擎进程不动，会话无感）。
- **同时重启**安全：对账先 claim 再探活，每条会话恰一次探活、恰一个
  owner，不误判 Crashed。
- 硬约束：`ONEAI_PG_DSN` + `lease_ttl_secs=0` 启动拒绝（多副本无所有权
  仲裁=脚枪）；逃生门 `ONEAI_ORCH_PG_NO_LEASE=1` 仅限迁移/测试。
- MVS4-A 边界：多副本 = **同 docker 宿主**多进程（容器端口发布在
  127.0.0.1）；跨宿主网络留给 K8sRunner 轮。
- 编排器日志走 stderr tracing（`RUST_LOG` 可控，默认 info）：reconcile/
  租约丢失/清扫/归档均有结构化行。

## 21. MVS4-A 全量验收（一键）

```bash
./deploy/docker/mvs4a_run.sh   # 确保 pgvector 容器 + 库 oneai_mvs4 + 宿主二进制 + 跑 mvs4a_verify.mjs
```

验收矩阵（六 phase 21 项，2026-09-14 实测 21/21）：A 单副本 Pg 基线
（banner/DDL/落行 owner+lease/真实 turn+活动落库/DELETE 零残留）→ B 双副本
协作（异 replica/非 owner WS→409+owner 头/过期接管/list 收敛）→ C 故障转移
（kill -9 A → B 接管：owner 翻转、StartedAt 逐字节不变、turn 答出 A 时代
暗号）→ D 并发对账（活跃租约不夺/ghost 过期租约双副本同时重启每条恰一
owner 全 Running）→ E 跨副本深度归档（kill C → D sweep 接管整链休眠+归档；
WS 经 D 重连 Resuming + session/load 答出归档前暗号）→ F 红线（只读归档
目录不 destroy/lease_ttl=0 启动拒绝/文件模式回归）。

## 22. Pg 集成测试（编排器 store，开发侧）

```bash
ONEAI_TEST_PG_DSN=postgres://postgres:oneai@127.0.0.1:5432/oneai_test \
  cargo test -p oneai-orchestrator --features postgres \
  --test pg_store_tests -- --ignored
# 9 测：schema 幂等 / insert-dup(PK 跨副本仲裁) / CAS 契约+10 并发恰一赢 /
#       archived claim-release 排他 / 租约 claim-renew-过期接管全生命周期 /
#       10 路并发 claim 恰一赢家 / GREATEST 活动单调 / force_update 跨连接可见
# 另有 multi_replica_tests 8 测（无需 Pg/docker：MemLeaseStore 镜像 Pg 契约，
# 双 OrchestratorState 共享真相 + 真 axum/WS 握手收 409 + 并发对账探活计数）
```
