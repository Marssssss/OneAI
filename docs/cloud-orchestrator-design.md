# OneAI 云端会话编排器设计（Cloud Session Orchestrator）

> 状态：设计稿（2026-09）· 前置研究：端侧/云端 Agent 形态调研（内部结论见 §1）
> 关联文档：`app-server-mechanism.md` · `bus-mechanism.md` · `working-state-mechanism.md` · `supervisor-mechanism.md`

---

## 1. 背景与动机

OneAI 是纯端侧运行的 Agent 框架：一个引擎进程绑定一个活跃会话，状态落在本地
SQLite（`~/.oneai/oneai.db`）与本地 JSONL（working-state 事件日志），前端经
stdio / UDS / named-pipe / ws 接入。

2024–2026 年行业调研结论（多源核验）：

1. **主流产品全部收敛到「本地 + 云端」双形态**，没有发生「纯云端取代端侧」：
   Claude Code（本地 CLI）+ Cowork/Agent SDK hosting（托管沙箱会话）、
   OpenAI Codex CLI + Codex Cloud、Cursor 本地 Agent + Cloud Agent、
   Google Jules（纯云）与 Aider（纯端）并存互不取代。
2. **云形态的核心价值是三件端侧物理上做不到的事**：机器离线后任务继续、
   多任务并行、移动端发起/查看。交互式低延迟场景行业共识仍归端侧。
3. **云化的工程形态 = 把同一个单会话引擎进程装进每会话一个的容器托管**。
   Anthropic Agent SDK hosting 文档明确：*"The Agent SDK spawns and supervises
   a claude CLI subprocess that owns a shell, a working directory, and session
   files on disk. Hosting it is not like hosting a typical API service."*
   多租户隔离在容器层解决，**不在引擎进程内解决**。

因此本设计的定位是：**端侧核心不动，加一个可选的云端执行层**。OneAI 的
「单进程单会话」模型恰好是行业验证过的容器化单元——云化不是重写引擎，
而是写一个薄编排层把这个进程「装进容器 + 管起来 + 路由过去」。

## 2. 目标 / 非目标

### 目标

- G1 **一会话一容器**：每个云端会话运行在独立容器内的独立引擎进程中，
  容器即租户隔离边界。
- G2 **引擎零改动**：容器内跑的就是现有 `oneai app-server`（或 `oneai serve`）
  二进制，端侧形态完全不受影响，两种形态共享同一引擎。
- G3 **会话可重建**：容器销毁/崩溃/休眠后，会话能从外部化存储完整重建
  （对话历史 + working-state + 审批状态可丢）。
- G4 **薄控制面**：一个编排器进程负责会话生命周期（创建/路由/休眠/销毁）、
  认证、健康检查；无状态可水平复制（后期）。

### 非目标（明确排除，防止定位漂移）

- N1 ~~引擎 SessionPool 化 / 单进程多会话~~ —— `InProcessBus` 的单 interrupt
  槽（`oneai-bus/src/bus.rs:82`）、单 directive mpsc、单 `AppSession` 全部保持
  原样。业界不这么做，改动深、破坏「一个引擎一个会话」模型。
- N2 ~~单进程多租户存储改造~~（全表加 user_id / tenant_id 列）—— 租户隔离
  下沉到容器与卷层面后，引擎内部保持单用户假设（见 D4）。
- N3 ~~自建 SaaS 运营面~~（计费、订阅、用户注册）—— 编排器只做技术控制面，
  运营面留给部署方。
- N4 ~~端云会话热迁移~~（同一会话在本地与云端之间无缝切换）—— 状态格式
  兼容（同一套 SQLite schema / JSONL），但不做在线迁移，只支持导出/导入式
  冷迁移（后期可选）。

## 3. 总体架构

三个平面：

```
                        ┌─────────────────────────────────────────┐
  前端                   │            控制面 (orchestrator)          │
 ┌────────┐  HTTPS/WSS  │  axum: 认证 · 会话路由表 · 生命周期 FSM     │
 │ web UI │────────────▶│  ContainerRunner trait (Docker/K8s/…)    │
 │ IDE    │             │  健康检查 · 休眠/恢复 · 配额(后期)          │
 │ 移动端  │             └───────┬─────────────────────┬───────────┘
 └────────┘                     │ spawn/stop/commit   │ WS 反代(会话亲和)
        ▲                       ▼                     ▼
        │              ┌─────────────────────────────────────────┐
        │              │        数据面 (每会话一个容器)             │
        └──────────────│  oneai app-server --listen ws://0.0.0.0  │
         直连(经反代)    │  单会话引擎 · AgentLoop · 工具 · 沙箱      │
                       │  (seatbelt/bwrap 保留为纵深防御)           │
                       └───────┬─────────────────────────────────┘
                               │ 读写(池化连接 / 挂载卷)
                       ┌───────▼─────────────────────────────────┐
                       │              存储面                       │
                       │  MVS1-2: 每会话独立 volume(SQLite+JSONL)  │
                       │  MVS3+:  共享 Postgres + 对象存储          │
                       │  密钥: Secret 注入容器 env                 │
                       └─────────────────────────────────────────┘
```

要点：

- **数据面就是现有二进制**。`oneai app-server` 已支持
  `--listen ws://host:port`（`oneai-app-server/src/lib.rs:115-172`，feature `ws`），
  容器内监听即可被编排器反代。`serve_web`（`http.rs`）还能同端口托管 web SPA，
  所以「容器 = 引擎 + 该会话的 WebUI」也是可行形态。
- **控制面对前端说同一套 JSON-RPC**：`session/create` 时编排器拉起容器，把
  后续 `turn/run` 等 WS 流量反向代理到该容器（会话亲和），前端协议零变化。
- **存储面分两档**：起步用「每会话一个卷」直接复用现有 SQLite/JSONL；
  规模化后加 Postgres/对象存储后端（trait 已抽象好，见 §6 MVS3）。

## 4. 现有积木盘点（调研结论）

### 直接复用

| 积木 | 位置 | 复用方式 |
|---|---|---|
| Bearer 三件套（`secret_from_env`/`ct_eq`/`verify_bearer`） | `oneai-a2a/src/server.rs:145-176` ≡ `oneai-scheduler/src/oneshot.rs:49-81`（字面重复两份） | 抽公共 crate `oneai-http-auth`，编排器/a2a/scheduler 三方收敛 |
| axum SSE 长任务流式模板 | `oneai-a2a/src/server.rs:369-479`（mpsc + sync sink trait + spawn runner + keep_alive） | 控制面 `session/events` 流式端点直接套用 |
| HTTP+WS 同端口 + SPA fallback | `oneai-app-server/src/http.rs:66-123` | 编排器 admin/接入层骨架 |
| CAS at-most-once（RwLock + cas + 原子 rename） | `oneai-scheduler/src/store.rs:96-113` | 会话租约（lease）/防双拉容器 |
| registry 持久化 + 「reconcile 而非 restart」恢复语义 | `oneai-supervisor/src/registry.rs:22-120,193-206`（`Running→Crashed("supervisor_restart")`，上层决定重拉） | 编排器会话路由表的持久化与重启对账直接照搬该模式 |
| Docker 容器操作经验（create/exec/commit/restore/stop 的 argv 纯函数） | `oneai-tool/src/terminal/docker.rs:32-129,246-330` | `DockerRunner`（编排器的 ContainerRunner 实现）参考其构造与测试模式 |
| working-state append-only 事件日志 + `OnResume` 对账 | `oneai-persistence/src/working_state_store.rs` + `docs/working-state-mechanism.md` | 容器崩溃后重建会话的任务进度来源（G3 的地基，已存在） |
| 会话 resume（`session/load` + SQLite 回放） | `cmd_app_server.rs` / `SqliteSessionStore` | 新容器起来后 load 卷内 DB 即恢复对话 |

### 明确不复用 / 绕过

| 积木 | 原因 |
|---|---|
| `oneai-supervisor` 运行时（daemon + InstanceHandle + IpcListener） | 它守护的是**进程内 tokio task**（`supervisor/runner.rs:1-11`），不是 OS 进程。容器方案下编排器本身就是 supervisor：容器健康检查 + 重启策略取代 InstanceHandle，registry 只借数据模型 |
| `bridge_connection` 多客户端共享同一 bus | 那是「一个引擎多个观察者」，不是多会话；编排器反代是每会话独立连接独立容器，不共享 bus |
| `TerminalBackend` 远端执行（Modal/Daytona/Docker backend） | 语义是「agent 在本地、shell 命令打到远端」（`tool_interfaces.rs:82`）。编排器方案是「agent 整个进容器」，两者正交且可叠加（容器内的 agent 仍可再用 Docker backend 嵌套沙箱）。Modal/Daytona 可作为 ContainerRunner 的替代实现参考，但其 FileSyncStrategy 缺口（仅 Docker 有 `docker cp` 实现）不在本设计范围 |

### 需要新写

1. **`oneai-orchestrator` crate**（控制面）：axum server + `ContainerRunner`
   trait + 会话路由表 + 生命周期 FSM + WS 反向代理。
2. **`oneai-http-auth` crate**（或 core 内模块）：抽 Bearer 三件套 + 未来的
   axum middleware 化（当前 a2a/scheduler 的校验都写在 handler 体内）。
3. **TLS**：全仓四个 HTTP 入口（a2a/gateway/scheduler/serve_web）均为裸 TCP。
   编排器要么内置 rustls，要么约定前置 reverse-proxy（Caddy/ALB）终止 TLS。
   MVS 阶段选后者（约定），生产阶段二选一。
4. **容器镜像**：`Dockerfile`（oneai-cli release 二进制 + web dist + 基础工具链），
   每会话容器从该镜像启动。
5. **（MVS3）Postgres 存储后端**：见 §7 简表。

## 5. 关键设计决策

### D1 一会话一容器，而非一进程多会话

依据：①行业验证形态（Agent SDK hosting = 子进程装容器）；②`InProcessBus`
单 interrupt 槽 / 单 directive 流的现状不是缺陷而是模型特征，容器化后天然
规避；③隔离（文件、网络、密钥、崩溃域）在容器边界一次性解决，引擎内无需
任何租户概念。代价是每会话一个进程的内存底座（Rust 引擎 + tokio，实测应在
数十 MB 量级），用休眠策略（D6）摊薄。

### D2 容器内入口 = `oneai app-server --listen ws://0.0.0.0:<port>`

JSON-RPC 层已是全前端统一协议（L2 adapter），选它而非 `serve`（newline-JSON
裸总线）的理由：①web/IDE/移动前端零翻译直连；②session/* 管理 RPC 现成；
③`serve_web` 可顺带托管该会话的 WebUI。容器内单会话，`AppServerRuntime`
的 `Mutex<Runtime>` 串行模型保持不变。

### D3 控制面做 WS 反向代理（会话亲和），不做协议翻译

前端 → 编排器（`/session/<id>/ws`）→ 容器 ws。编排器只在握手时校验 token、
查路由表、建立双向 pump；JSON-RPC 载荷原样透传。这样 `EngineYield`/`Directive`
协议演进不波及编排器。路由表：`session_id → (container_id, addr, state)`，
内存 + JSONL 持久化（照搬 supervisor registry 的原子写与
`recover_after_restart` 对账语义：编排器重启后对每个 `Running` 会话做容器
健康探测，活的重新挂路由，死的标 `Crashed` 等前端 resume）。

### D4 存储两档：先「每会话一卷」，后「共享 Postgres」

**MVS1–2 不做任何存储代码改动**：容器挂载独立 volume，里面就是现有的
`~/.oneai/oneai.db` + `tasks/*.jsonl` + `scenarios.json`。多租户隔离 = N 个卷，
**这就是为什么不需要全表加 user_id**（推翻早期 P0 评估）。
`SqliteSessionStore` 每方法 `open_connection()` 无池化（`sqlite_store.rs:146`）
在单会话容器内是可接受的（并发度 = 1 个引擎）。

**MVS3 引入共享后端**的动因：卷数量随会话线性增长的管理成本、跨容器查询
（运营面需要）、休眠卷的冷存储成本。届时按 trait 加实现（§7），引擎侧
仍零改动——`AppBuilder` 注入点已存在（`sqlite_persistence_at`/`working_state`/
`session_event_store`，`builder.rs:1490-1563`），只补 setter 缺口。

### D5 密钥与配置注入

每会话容器启动时由编排器注入 env：provider API key、`ONEAI_A2A_SECRET` 类的
内部通信密钥（编排器↔容器控制通道）、模型/DomainPack 配置。MVS 阶段密钥
来源 = 编排器本地配置文件；生产阶段接 Secret Manager（Vault/云 KMS），
按租户取 key。容器内 `config.toml` 不落盘（env 优先），避免密钥残留卷内。

### D6 生命周期 FSM 与休眠

```
Creating ──▶ Running ──idle超时──▶ Hibernating ──请求到达──▶ Resuming ──▶ Running
   │            │                      │
   │            └──崩溃──▶ Crashed ─────┴──显式删除──▶ Destroyed
   └──拉起失败──▶ Failed
```

- **Hibernating**：`docker stop`（保留卷）或 `docker commit` 快照后删容器
  （`DockerTerminalBackend` 的 commit/restore 路径已验证过该操作序列，
  `terminal/docker.rs:304-329`）。恢复 = start/ recreate + `session/load`。
- **Crashed**：容器死掉但卷在。编排器标 `Crashed`，下一次前端请求触发
  Resuming：新容器 + load 卷内 SQLite + working-state `OnResume` 对账
  （未完成任务自动 surface，机制已存在）。
- **Destroyed**：删容器 + 删卷（或卷归档到对象存储后删）。
- at-most-once 保证：状态迁移走 CAS（照搬 `JobStore::cas_mark_fired` 模式），
  防止两个并发请求拉起两个容器。

### D7 认证

前端 → 编排器：`Authorization: Bearer <token>`（MVS：共享密钥，复用
`oneai-http-auth`；生产：JWT/OIDC，token 内含 tenant/user 声明）。
编排器 → 容器：内部通道再用一个每会话随机密钥（容器启动时注入），
防止容器端口被同网段其他容器直连。容器永不直接暴露公网。

## 6. 分期计划

### MVS1 —— 手工容器化验证（✅ 已完成 2026-09-09，记录见附录 A）

验证「现有二进制直接进容器能跑通全链路」：

1. 写 `Dockerfile`（release 二进制 + web dist）。
2. `docker run` 一个容器：`oneai app-server --listen ws://0.0.0.0:8787`，
   挂一个 volume 到 `/root/.oneai`。
3. 宿主机 web 前端直连 `ws://<容器>:8787/ws`，跑一轮完整对话
   （含工具调用、审批、session/load 恢复）。
4. 杀容器 → 重拉新容器挂同一卷 → `session/load` → 验证历史与 working-state
   完整恢复。

产出：Dockerfile + 验证记录。**这一步不写编排器**，若发现引擎在容器内
有阻断性问题（沙箱嵌套、路径假设），先修引擎侧。

### MVS2 —— 薄编排层（新 crate `oneai-orchestrator`）（✅ 已完成 2026-09-10，记录见附录 B）

- `ContainerRunner` trait：`spawn(SessionSpec) -> ContainerHandle`、`stop`、
  `start`、`commit`、`destroy`、`health`；实现 `DockerRunner`（参考
  `terminal/docker.rs` 的 argv 纯函数 + 单测模式）。
- axum 控制面：`POST /v1/sessions`（创建+拉容器）、`GET /v1/sessions`、
  `DELETE /v1/sessions/{id}`、`GET /v1/sessions/{id}/ws`（升级后反代）、
  `GET /healthz`。Bearer 认证（`oneai-http-auth` 抽取完成）。
- 路由表：内存 + JSONL 原子持久化 + 启动对账（照搬 supervisor registry）。
- FSM：D6 状态机 + idle 超时休眠 + CAS 迁移。
- CLI：`oneai orchestrator --listen ...`（对齐「新子系统 = AppBuilder 方法 +
  CLI 子命令」惯例；编排器不进 AppBuilder，它是 App 之上的部署件，只加 CLI）。

验收：单宿主机 docker，10 个并发会话容器稳定跑；编排器重启后会话全部
重挂；杀容器后前端重连自动 Resuming。

### MVS3 —— 存储外部化 + 规模化恢复（🔶 进行中：A 轮 PgWorkingStateStore ✅ 2026-09-12；B 轮 Memory/Usage/HostAllowlist ✅ 2026-09-13；C 轮 SessionEvent/Feedback/卷归档/瘦身 ✅ 2026-09-13）

- ✅ `PgWorkingStateStore`（**优先项已交付**：事件日志是崩溃恢复的命脉，且
  read-modify-write 的 `tasks.index.json` 在多写者下需要事务化）——
  `oneai-persistence/src/pg_working_state_store.rs`，feature `postgres` 默认关
  （云镜像 `--features oneai-cli/postgres` 编入）；运行期 `ONEAI_PG_DSN` 选择，
  接线集中 `examples/cli/src/working_state.rs`（web/app-server/serve/TUI/
  `tasks *`/`export-hf` 全入口）+ `AppBuilder::working_state_store(Arc<dyn …>)`
  泛型注入点。事件 JSONB 无损 + brief 同事务 UPSERT + per-task `FOR UPDATE`
  串行化（多写者安全）；测试 `ONEAI_TEST_PG_DSN` env 门控（镜像文件后端 8 测
  + 并发 2 测）。**部署**：`~/.oneai/orchestrator.toml` 配
  `passthrough_env = ["ONEAI_PG_DSN"]`（编排器零代码改动；容器内访问宿主 Pg
  用 `host.docker.internal`）。机制细节见 `docs/working-state-mechanism.md` §14。
- ✅ `PgMemoryStore` + `PgUsageTracker` + `PgHostAllowlist`（**B 轮已交付**，
  2026-09-13）——`oneai-persistence/src/pg_{memory_store,usage_tracker,host_allowlist}.rs`，
  同 feature `postgres`、同 `ONEAI_PG_DSN` 选择、池化/advisory-lock/fail-fast
  建表模式抽进 `pg_common.rs` 四 store 共用（锁 key 注册表见其模块文档；
  `_pg` 后缀表与 SQLite 表防御性共存）。要点：
  - **pgvector 硬依赖**（用户决策）：LTM 向量检索换服务端精确 KNN
    （`ORDER BY embedding <=> $1`，无维度 `vector` 列支持混合嵌入模型；
    `vector_dims` 过滤隔离异模型行），替代 in-Rust brute-force cosine；
    `CREATE EXTENSION vector` 失败 → connect 报错 → CLI 选择层响亮告警回退
    SQLite（各 store 独立降级，互不拖累）。Pg 服务器须用 pgvector 镜像。
  - `MemoryPersistence` 17 方法全实现（含 metadata 合并 rename 保护、
    discarded 快照前缀约定、facts ON CONFLICT upsert 版本递增）；trait 增补
    `rename_conversation`/`set_conversation_archived` 默认方法（core 侧加法
    改动），SQLite/Pg 各自覆写为定向 metadata UPDATE（不重写消息 blob）。
  - **App 会话面路由**：`App` 持 `memory_persistence` 覆写（builder setter
    留存 Arc），`session/list|load|rename|archive|delete` RPC 与 turn 尾自动
    落盘全走同一后端——否则 Pg 模式下 webUI 会话列表读本地 SQLite 恒空。
    feedback / thinking-effort 仍留本地 SQLite（有意分界，见 pg_backends.rs）。
  - `PgUsageTracker` 补 `is_estimated` 真列（SQLite 表缺，round-trip 丢旗标）。
  - `PgHostAllowlist` 互斥语义事务化（admit 清 deny 行同 tx）；固有 CRUD 面
    镜像 SQLite 版，web `host/*` RPC 经新 `PgHostAllowlistRpc` adapter 接同一
    `Arc`（与引擎代理共池共表，跨容器共享白名单）。
- ✅ `AppBuilder` 补 setter 缺口：`host_allowlist_store(...)`（override 优先，
  Seeded 包裹恒保留）、通用 `memory_persistence(...)`（无条件重建
  MemoryManager，显式 override 语义对齐 `working_state_store()`）。
- ✅ `PgSessionEventStore` + `PgFeedbackStore`（**C 轮已交付**，2026-09-13）——
  `oneai-persistence/src/pg_{session_event_store,feedback_store}.rs`（锁 key
  base+4/+5），存储外化最后两块：trajectory 事件日志（`session/trajectory`
  RPC + web 泳道时间轴）与 per-message feedback（`feedback/*` RPC）跨容器/
  跨卷死亡存活。事件 `line` 列用 TEXT 而非 JSONB——trait 契约是 opaque
  string，TEXT 保证字节级 round-trip；feedback 走「固有 API + CLI
  `PgFeedbackStoreRpc` adapter」（trait 在 app-server，复刻
  `PgHostAllowlistRpc` 先例）。`apply_pg_backends` 扩三元组（7 调用点）。
  同轮：`cmd_session list/resume/delete/info/export-hf` + `cmd_memory
  search/list` 从硬连 SQLite 改走 `session_backend::open_memory_backend()`
  （Pg 模式下云会话对管理命令可见）。
- ✅ 休眠卷归档（**C 轮已交付**，2026-09-13）——编排器二级深度休眠：
  `oneai-orchestrator/src/archive.rs`（`VolumeArchiveStore` trait +
  `LocalDirArchiveStore`：`docker run --rm alpine:3.20 tar` 导出/导入卷到
  `archive_dir`，零新依赖；S3 留 MVS4 按 trait 加）。`deep_archive_timeout_secs`
  （0=禁用）超时后：归档确认成功 → CAS claim（`PersistedEntry.archived`
  标记字段，**不加 FSM 状态**）→ `destroy(remove_volumes=true)`；resume 检测
  标记 → restore 卷 → spawn → Running 后清标记删归档。红线：归档失败绝不
  destroy，会话保持 Hibernating 卷不动、下轮重试。
- ✅ 容器镜像瘦身 + 启动预热（**C 轮已交付**，2026-09-13）——①`oneai-rag`
  的 fastembed/ort 可选化（feature `fastembed` 默认关；云镜像
  `--no-default-features --features oneai-cli/postgres` 构建，整条 ONNX
  静态链不进镜像；端侧 CLI default 保留，行为零变化；feature 关时显式配置
  fastembed → 响亮告警 + 关键词召回降级）；②web dist shiki 细粒度打包
  （`shiki/core` + 显式 12 语言 + 2 主题，dist 27MB/627 文件 → ~7MB/5 文件，
  清单外语言维持 plain-`<pre>` 兜底）；③Dockerfile 去 bubblewrap（R4 已证
  容器内不可用）；④启动预热：`build_engine_server` 在监听 bind 前调
  `warm_model_context`（30s 超时兜底）——编排器 TCP 探活通过 = 引擎就绪。

### MVS4 —— 生产化（A 轮已交付，2026-09-14）

- ✅ **编排器多副本 + 路由表进共享存储 + lease**（**A 轮已交付**，
  2026-09-14）——`SessionStore` trait（`oneai-orchestrator/src/store.rs`）
  把路由表的持久半边抽成可插拔后端：`FileSessionStore`（MVS2 sessions.json
  原样承接，字节兼容，默认）+ `PgSessionStore`（feature `postgres`，第七个
  Pg 后端，复用 `pg_common` 配方，锁 key base+6=1330538831，表
  `orchestrator_sessions`）。要点：
  - **每会话行级租约**（非全局 leader）：`owner_replica`+`lease_expires_at`
    列；claim/renew/takeover 全部单条条件 UPDATE 对**服务端 now()**（副本
    时钟偏斜无关）；心跳 ttl/3。所有权是 **claim-on-act**（WS upgrade/
    sweep/归档/reconcile 各自动作前 claim，动作完释放或任其过期）——
    不粘滞，天然负载均衡。
  - **CAS 进 SQL**：状态迁移/归档标记 = `UPDATE … WHERE state=…
    RETURNING`（行锁仲裁，跨副本恰一赢家）；存在性→期望态→FSM 校验的
    顺序与 MVS2 内存版逐位一致；`insert` 的 PK 冲突即跨副本
    AlreadyExists。
  - **单 owner 代理**：WS upgrade 先 claim；他人活跃租约 → 409 +
    `X-Oneai-Owner-Replica`（LB sticky 学习窗口）。`LeaseGuard`（RAII，
    仿 `ConnGuard`）持租约与代理泵同生共死：心跳续租顺带把
    `last_activity_ms` 以 ≥1s 节流落库（`GREATEST` 单调），异地副本的
    idle sweep 靠它避免休眠活跃会话。
  - **租约门控对账**：启动 reconcile 先 claim 再探活（claim 即互斥）——
    两副本同时重启每条恰一次探活、恰一个 owner、零误判 Crashed；死亡
    容器标 Crashed 后释放租约。kill -9 owner → 租约过期 → 任意副本
    接管，**容器零操作**（StartedAt 逐字节不变）。
  - 每副本内存热缓存只存进程内 atomics（active_conns/活动钟/notify）；
    持久字段每次读经 store 刷新合并（Arc 仅在持久字段真变化时替换——
    否则 ConnGuard 持有的 Arc 被孤儿化、active_conns 泄漏）。
  - 选择层照抄 pg_backends 惯例：`ONEAI_PG_DSN`（env 优先于
    `orchestrator.toml pg_dsn`）+ 响亮告警回退；`validate()` 拒绝
    「Pg+lease_ttl=0」（逃生门 `ONEAI_ORCH_PG_NO_LEASE=1`）。CLI
    `--lease-ttl`/`--replica-id`；oneai-cli `postgres` feature 聚合
    `oneai-orchestrator/postgres`（云镜像构建命令零改动）。
  - **MVS4-A 边界**：多副本 = 同 docker 宿主多进程（scale-out + 零停机
    重启 + 故障转移）；跨宿主网络（容器端口 127.0.0.1 发布）留给
    K8sRunner 轮。租约丢失中途代理的窄竞态（新 owner 可能休眠仍有帧
    流动的会话）本轮接受并记录（心跳 gap>TTL 才触发；活动落库让 sweep
    侧二次防护）。验收：附录 F（21/21）。
- ✅ **租户配额限流 + OTEL tenant/session 贯穿**（**B 轮已交付**，
  2026-09-14）——租户维度 + 三层配额 + 引擎 span/用量按租户归因：
  - **租户维度（最小可用，JWT 前瞻兼容）**：`CreateSessionRequest.
    tenant_id`（body 为准，`X-Oneai-Tenant` 头兜底；`[a-zA-Z0-9_-]{0,64}`，
    非法 400）；存于 `SessionSpec.tenant_id`（serde default——file JSON /
    Pg spec JSONB 旧数据零迁移可读）+ Pg 去规范化 `tenant_id` 列（部分
    索引排除 Failed/Destroyed；exists 探针带列检查，旧部署自动 ALTER 升级）。
    空租户归一 `"default"` 桶。鉴权仍是单一共享密钥（受信调用方申报租户）；
    JWT/OIDC 落地后由 token 的 tenant 声明覆盖申报值。
  - **三层配额（全 opt-in，未配置=无限制→A 轮行为零变化）**：
    ① 并发会话帽——**跨副本精确**：`SessionStore::insert_if_under_quota`
    把 COUNT+INSERT 收进单仲裁单元（Pg 覆写=单事务
    `pg_advisory_xact_lock(hashtext(tenant))`+COUNT+条件 INSERT，两副本
    抢最后一个槽恰一赢家；File 覆写=op_lock commit；禁止 check-then-insert
    两步）。Failed/Destroyed 不计数（崩溃循环不得锁死租户）。
    ② token 预算——引擎容器用量行打 `metadata.tenant_id/
    orch_session_id` 标（CLI 层 `TenantTaggingUsageTracker` 装饰器，引擎
    crate 零改动），编排器 `PgUsageTracker::tenant_token_sum` 服务端 SUM
    （lifetime `max_total_tokens` 或滚动 24h `daily_token_budget`）；仅 Pg
    模式生效，无 usage 源/SUM 失败**响亮 fail-open**（用量库故障不得瘫痪
    建会话——可用性优先，与 registry 缓存 stale-read 同姿态）。
    ③ 创建限流——每副本内存令牌桶（突发=1min 配额，`Mutex<HashMap>` 零新
    依赖），跨副本精确需 Redis/Pg 行级节流，**接受每副本近似**（本节原文
    预留的选项；①的共享帽已兜住总量）。
    配置：`[quotas_default]`（全体租户基线）+ `[quotas_tenants.<桶>]`
    （逐租户覆盖，`default` 键=未打标会话）；CLI `--quota-max-sessions/
    --quota-max-tokens/--quota-daily-tokens/--quota-rate-per-min`。
    拒绝=429 JSON `{error:"quota_exceeded", reason:concurrent_sessions|
    token_budget|create_rate, tenant_id, limit, current, message}`；仅
    create_rate 附 `Retry-After`（另两种重试无益——要删会话/调预算）。
  - **OTEL 贯穿（编排器→容器→引擎 span 同 trace）**：编排器 `build_spec`
    在 caller-env 合并后**主权 upsert** 四个契约变量（调用方 env 不可伪冒）：
    `ONEAI_TENANT_ID`、`ONEAI_ORCH_SESSION_ID`（编排器会话 id，≠引擎
    conversation UUID，是 usage/span↔路由表的 join 键）、
    `OTEL_EXPORTER_OTLP_ENDPOINT`（配置 `otel_endpoint`/`ONEAI_OTEL_ENDPOINT`
    才注入）、`TRACEPARENT`（每 spawn 新生成，w3c.rs；烘焙进 spec，resume
    重 spawn 续同 trace）。引擎侧：`build_engine_server` 读标准 OTEL env 接
    `OtlpCollector`（resource 带 tenant.id/orchestrator.session.id）+5s 周期
    flusher（session 根 span 长驻不 end，不能只靠 batch-64 eager）；
    `TraceContext::seed_parent_from_traceparent` 合成远端父 span（span_id=
    trace_id），session span 在其后 enter → 全进程 span 挂进编排器 trace；
    `Span.trace_id_override` 让导出端在中间父缺席批次时仍精确还原注入的
    trace id。**顺带修复 P2-3 遗留断链**：`TraceContext` 的 collector 字段
    此前是 dead_code（on_span_start/end 全仓零调用），`trace_otel` 从未真正
    导出过任何 span——B 轮补上 enter/exit→collector 桥接（runtime 内
    detached spawn）。
  - **可观测补充**：编排器 create/delete/ws-proxy 入口 tracing span 带
    tenant.id/session.id（A 轮已接的 fmt subscriber 落 stderr）；spawn 日志
    打 traceparent 值作关联把手。容器 metrics 采集仍留后续轮。
  - **B 轮边界**：租户=受信申报（无 JWT 校验）；限流每副本近似；预算 SUM
    走 JSONB 表达式（生产大规模需 generated column+索引，DDL 注释已留迁移
    语句）；file 模式无 token 预算（无共享 usage 账本）；引擎 session 根
    span 仍不导出（长驻语义，子 span 全量导出且 trace 归属正确）；OTLP
    导出走 reqwest——daemon 级代理注入的环境（colima 实测）下 collector
    地址必须进容器 `NO_PROXY`，否则 POST 被代理劫持（502；Pg 不受影响，
    tokio-postgres 不经 reqwest）。验收：附录 G。
- `K8sRunner`（Pod 即容器抽象，跨宿主网络）——MVS4 后续轮。
- TLS 内置（rustls）或正式约定反代；JWT/OIDC（tenant 声明覆盖 B 轮的
  申报值）；Secret Manager 对接；per-session 内部密钥（D7，需引擎 ws
  auth 钩子）。
- 配额后续：跨副本精确限流（Redis/Pg 行级节流，若每副本近似不够用时）；
  预算窗口扩展（自然日/月账期）；租户配额动态下发（Pg 配置表 + 热更）。
- 可观测后续：容器 metrics 采集；编排器 tracing↔oneai-trace 桥
  （tracing-opentelemetry layer，两套系统目前仅经 TRACEPARENT env 关联）。
- egress 治理：容器网络策略（默认拒绝 + 域名放行），与引擎内
  host-allowlist/CONNECT 代理形成双层。

## 7. 存储 trait 外部化简表（MVS3 输入）

| Trait | 现有实现 | 云端缺口 |
|---|---|---|
| `MemoryPersistence`（core/traits.rs:1332） | `SqliteSessionStore` | ✅ `PgMemoryStore`（B 轮：单表族 `_pg` 后缀而非 4 子 store——trait 是单一 17 方法接口，拆子 store 收益存疑，暂不拆；pgvector 服务端 KNN 已交付） |
| `WorkingStateStore`（core/traits.rs:763） | `FileWorkingStateStore`、`NoTaskStore` | ✅ `PgWorkingStateStore`（事件表 + brief 表事务化，feature `postgres`）——最优先项已交付 |
| `SessionEventStore` | `FileSessionEventStore` | ✅ `PgSessionEventStore`（C 轮；append-only BIGSERIAL，line 列 TEXT 保字节级 round-trip） |
| `HostAllowlistStore`（core/traits.rs:703） | Sqlite / InMemory / Seeded | ✅ `PgHostAllowlist`（B 轮；Seeded 装饰器保留在 builder 层） |
| `UsageTracker` | `SqliteUsageTracker`（同样无池化） | ✅ `PgUsageTracker`（B 轮；批量 flush 未做——写路径本就每 call 一行，池化后延迟可接受） |
| `FeedbackStore`/`ConversationStore`（app-server 层） | InMemory + App wrapper | ✅ `PgFeedbackStore`（C 轮；固有 API + CLI adapter——trait 在 app-server，persistence 不可依赖；ConversationStore 经 B 轮 `App.memory_persistence` 覆写已通 Pg） |
| `StatePersistence`（checkpoint, traits.rs:730） | 无生产实现 | ⏳ 编排器休眠快照元数据可用 |

## 8. 安全与隔离

- **容器 = 租户边界**：文件系统、网络、密钥、崩溃域全部按会话隔离。
  引擎内 Seatbelt/Bubblewrap 沙箱**保留**，作为容器内的纵深防御
  （防的是模型行为，不是租户）。
- **code_interpreter 顺带解决**：端侧它是本地 CPython 子进程（不走
  TerminalBackend，`oneai-tool/src/code.rs:67-73`），容器化后天然落在
  隔离容器内——这是云端形态相对端侧的安全增益。
- **容器不暴露公网**：只有编排器反代可达；编排器↔容器走每会话随机密钥（D7）。
- **卷加密**：休眠卷含完整对话与密钥派生状态，落盘加密（LUKS/云盘加密）+
  归档对象存储时服务端加密。
- **镜像供应链**：镜像内二进制来自 `cargo publish` 同一锁定构建
  （committed Cargo.lock + cargo-deny 四道闸已存在），镜像 digest 固定。

## 9. 风险与开放问题

| # | 风险/问题 | 缓解/状态 |
|---|---|---|
| R1 | 每会话一容器的内存底座成本 | Rust 引擎空载占用小；idle 休眠（D6）+ commit 快照；实测数据 MVS1 补 |
| R2 | WS 反代多一跳的延迟 | 同宿主机/同 AZ 内 <1ms 量级；交互式场景本来就走端侧，云端形态承接的是长时任务，延迟不敏感 |
| R3 | 编排器单点 | MVS2 单副本 + `sessions.json` 原子持久化对账恢复（重启秒级重挂，✅ 附录 B.2-D 实测 1.3s/10 会话）；**✅ MVS4-A 已解**：Pg 共享路由表 + 每会话租约，kill -9 owner 后其他副本零容器操作接管（附录 F C 段实测）；文件模式单副本行为不变 |
| R4 | 容器内嵌套沙箱（bwrap-in-docker）兼容性 | **✅ MVS1 已判：不可用**（Ubuntu 24.04 宿主系统级 AppArmor 限制非特权 userns，`--privileged`/unconfined 均无法恢复；VM 层 root 可用、非 root 不可用）。引擎已补 `is_available` 运行期探测自动降级 RegexBackend（附录 A.3）；容器本身是隔离边界（§8），生产纵深防御走 gVisor/kata runtimeClass（MVS4） |
| Q1 | 端云冷迁移（导出/导入会话）要不要做 | 状态格式天然兼容（同 schema 卷），做「拷卷」即可；产品化另议 |
| Q2 | 企业合规（SOC2/HIPAA）对云化的真实驱动强度 | 调研中该论断未过核验（证据不足），面向企业客户前需单独调研 |
| Q3 | 本地↔云端状态同步的成熟工程实践 | 行业无先例可抄；working-state 事件日志是候选同步单元，暂不设计 |

## 10. 对代码库的改动面汇总

| 层 | 改动 |
|---|---|
| 引擎（core/bus/agent/app） | **零改动**（N1/N2 的排除项）。例外：`oneai-tool` sandbox `is_available` 运行期探测（MVS1 产出的缺陷修复，与环境适配无关，任何 Linux 部署受益，见附录 A.3）；`oneai-core` `MemoryPersistence` B 轮**加法**增补 `rename_conversation`/`set_conversation_archived` 默认方法（会话元数据编辑进 trait，Pg/SQLite 各自定向 UPDATE 覆写——既有实现零破坏） |
| `oneai-app-server` | 零改动（ws 监听、serve_web 均已存在） |
| 新增 crate | `oneai-orchestrator`（MVS2）、`oneai-http-auth`（MVS2，抽 a2a/scheduler 重复）；✅ MVS4-A：orchestrator 内加 `store.rs`（`SessionStore` trait + `FileSessionStore` + `LeaseGuard`）/`pg_session_store.rs`（feature `postgres`，锁 key base+6）；✅ MVS4-B：orchestrator 内加 `quota.rs`（`TenantQuotaEnforcer`/`QuotaReason`/`TenantUsageSum` trait + `PgTenantUsage` adapter），`SessionSpec/PersistedEntry` 加 `tenant_id`（serde default 零迁移） |
| `oneai-persistence` | ✅ MVS3 加 Pg 后端（`pg_working_state_store.rs` + B 轮 `pg_memory_store.rs`/`pg_usage_tracker.rs`/`pg_host_allowlist.rs` + 共用 `pg_common.rs`，均为新文件；SQLite 侧仅 helper 提为 pub(crate) + trait 覆写委托）；✅ MVS4-A `pg_common` 放宽为 `pub`（orchestrator 第七 store 直接复用配方，零复制）；✅ MVS4-B `PgUsageTracker::tenant_token_sum`（唯一服务端聚合，metadata_json->>'tenant_id' SUM，生产迁移 DDL 注释留位） |
| `oneai-app` builder | ✅ MVS3 加 `working_state_store(Arc<dyn …>)` 泛型注入；✅ B 轮补 2 setter（`host_allowlist_store`/`memory_persistence`）+ `App.memory_persistence` 会话面路由（list/load/rename/archive/delete + turn 尾自动落盘 gate） |
| `oneai-a2a` / `oneai-scheduler` | MVS2 把 Bearer 三件套改指向 `oneai-http-auth`（消重复） |
| CLI | `oneai orchestrator` 子命令（MVS2）；✅ MVS4-A `serve --lease-ttl/--replica-id` + Pg 路由表选择（`ONEAI_PG_DSN`，响亮回退）+ `postgres` feature 聚合 `oneai-orchestrator/postgres` + tracing subscriber 接线（此前编排器 info/warn 日志全被丢弃）；✅ MVS4-B `serve --quota-*/--otel-endpoint` + `create --tenant`/`list --tenant` + usage 源自动接线（token 预算配置且 Pg 模式才连 `PgUsageTracker`）+ 引擎侧 `TenantTaggingUsageTracker` 装饰器与 OTEL bootstrap（均 examples/cli 层）+ `otel` feature（默认开，云镜像显式列） |
| `oneai-trace` | ✅ MVS4-B：collector 桥接（enter/exit 喂 on_span_start/end——修复 P2-3 以来 `trace_otel` 导出路径从未收到 span 的断链）+ `seed_parent_from_traceparent`（W3C 远端父合成）+ `Span.trace_id_override`（导出端在中间父缺席批次时仍还原注入 trace id） |
| `oneai-app` | ✅ MVS4-B `session.rs` 加法：enter session span 前 seed `TRACEPARENT`，span 加 `tenant.id`/`orchestrator.session.id` 属性（env 契约，端侧无 env 零变化）——引擎 crate 改动仅此 |
| 部署件 | Dockerfile（MVS1）、镜像流水线（MVS4）；✅ MVS4-B 构建命令加 `oneai-cli/otel`（镜像 `oneai-engine:mvs4b`，106MB） |

---

## 附录 A：MVS1 验证记录（2026-09-09）

环境：macOS/arm64 + colima（Ubuntu 24.04 guest，kernel 6.8.0-117-generic）+
docker 29.5.2；镜像 linux/arm64（494MB）；provider = 宿主机 config.toml
只读 bind-mount（多 provider 池，真实 LLM 调用）。
操作手册与全部命令：`deploy/docker/README.md`；验证客户端：
`deploy/docker/mvs1_verify.mjs`（node + ws，JSON-RPC 直驱）。

### A.1 验收结果（全过）

| 项 | 结果 |
|---|---|
| 引擎二进制进容器 | ✅ 零源码改动（除 A.3 缺陷修复）；`oneai web` 同端口托管 SPA+/ws |
| ws 全链路 turn | ✅ session/create(workspace 绑定) → turn/run → thinking/stream/inference/token_usage/turn_complete 事件齐全，真实 LLM 3 迭代 ReAct |
| 工具 + 审批回路 | ✅ approval_request → approval/respond(Proceed) → tool_result；文件写读 + shell 执行均成功 |
| 杀容器恢复 | ✅ `docker kill` → 新容器挂同卷 → session/list 可见（title+折叠消息数正确）→ session/load 回放 → 零工具 turn 中模型从恢复历史正确答出上一轮写入的文件内容 |
| 工作区持久 | ✅ /workspace 卷内文件与 `.oneai/events/*.jsonl` 会话事件日志跨容器保留 |

### A.2 构建期阻断（4 个，全部固化进 Dockerfile）

1. **cargo fetch 挂死**：BuildKit RUN 不继承 VM 的代理 env，直连
   static.crates.io（Fastly）挂死。修：`~/.docker/config.json` proxies 注入
   （预定义 build-arg）+ rsproxy.cn sparse 镜像源。
2. **numkong 7.8.0（usearch C 核）aarch64+GCC 编译失败**：探测期以
   `-march=armv8.2-a+dotprod` 判定支持并全局定义 `NK_TARGET_NEONSDOT=1`，
   但 baseline TU 只有 `-march=armv8-a`，`vdotq_s32` always_inline 目标属性
   不匹配（macOS/clang 无此限制故宿主构建正常）。修：`ENV
   NK_TARGET_NEONSDOT=0`（build.rs 官方逃生口；运行期动态派发，正确性无损）。
3. **ort-sys 链接失败**：fastembed（oneai-rag 非可选依赖）拖入 ort，pyke
   预编译 ONNX 静态库需 `__cxa_call_terminate`（GCC 13+ libstdc++），
   bookworm(GCC12) 必挂。修：基础镜像 `rust:1-trixie`/`debian:trixie-slim`。
   顺带发现：CLI 未开 `ort` feature 但 fastembed 仍强制引入 ONNX 链——
   后续可评估 fastembed 可选化。
4. **R4 bwrap-in-docker**：见 A.3/A.4。

### A.3 R4 调查链与引擎修复

探测矩阵（bwrap `--unshare-all`）：

| 场景 | 结果 |
|---|---|
| 容器内非 root，默认安全选项 | ❌ `No permissions to create a new namespace` |
| 容器内，`seccomp/apparmor=unconfined` | ❌ `loopback: Failed RTM_NEWADDR`（uid_map 非恒等映射 EPERM） |
| 容器内，`--privileged`（root/非 root） | ❌ 同样 RTM_NEWADDR |
| VM 层 root `unshare -Urn` | ✅（内核无问题） |
| VM 层非 root | ❌ uid_map EPERM —— Ubuntu 24.04 AppArmor `apparmor_restrict_unprivileged_userns=1` 为**系统级**限制，与 docker 无关 |

引擎缺陷：`BubblewrapBackend::is_available` 只查 `/usr/bin/bwrap` 二进制存在
→ 容器内选择器选中 bwrap → 每次 shell 执行必挂（实测一个 turn 内 6/6
失败，无降级）。修复（`oneai-tool/src/sandbox.rs`）：is_available 改为
「存在 + 运行期探测」，探测参数与 `wrap_command` 同形
（`--unshare-all --ro-bind / / --dev /dev --proc /proc /bin/true`），
`OnceLock` 缓存结果；失败时选择器按既有链路落 Docker→Regex。
修复后容器启动日志显示 `Using regex-based sandbox backend`，shell 工具
全链路可用。fmt/clippy/258 tests 绿。

### A.4 结论对设计的回填

- **§8 安全**：容器=租户边界成立；容器内 bwrap 纵深防御在 Ubuntu 24.04
  宿主上不可得（Regex 降级 = 黑名单级防护）。生产要求更强隔离时用
  gVisor(runsc)/kata runtimeClass——runtime 级沙箱不依赖容器内 userns。
  Debian 宿主（无该 AppArmor 限制）+ 非 root 容器可能恢复 bwrap，待验证。
- **D5 密钥**：bind-mount config.toml 捷径可行（多 provider 池全功能）；
  `No LLM provider configured` 启动警告只反映单 provider 路径，池配置下
  是 cosmetic（build_engine_server 走 `[[providers]]` 分支）。
- **编排器健康探测注意**：`docker exec` 与容器 CMD 进程安全上下文一致
  （同 seccomp/apparmor/caps），探测结果可信。
- **验证纪律**：模型会口述未发生的工具调用（实测把 read_file 结果说成
  「shell cat 成功」）——验收必须以引擎日志/事件流为准。

## 附录 B：MVS2 验收记录（2026-09-10）

环境：与附录 A 同机（macOS/arm64 + colima + docker 29.5.2）；镜像沿用
`oneai-engine:mvs1`（**引擎与镜像零改动**）；provider = 宿主机
config.toml 只读 bind-mount（`--provider-config`，真实 LLM 调用）。
验收驱动：`deploy/docker/mvs2_verify.mjs`（自包含：自起/重启/关停编排器
进程；一键 `./deploy/docker/mvs2_run.sh --sessions 10`）。

### B.1 交付物

| 件 | 位置 |
|---|---|
| `oneai-http-auth` crate | Bearer 三件套唯一实现（ct_eq/secret_from_env/verify_bearer + `BearerSecret` guard）；a2a/scheduler 已收敛为薄委托（公共 API 不变） |
| `oneai-orchestrator` crate | `runner.rs`(ContainerRunner trait) · `docker.rs`(纯 argv + DockerRunner) · `fsm.rs`(D6 状态机) · `registry.rs`(路由表 CAS + sessions.json 原子持久化 + 启动对账) · `proxy.rs`(WS 双跳透传) · `idle.rs`(休眠 sweep) · `routes.rs`(5 端点) · `server.rs`(编排入口) |
| CLI | `oneai orchestrator serve/create/list/status/destroy/cleanup` |
| 验收件 | `mvs2_verify.mjs` + `mvs2_run.sh` + `tests/e2e_docker.rs`(`#[ignore]` 真 docker 冒烟) |
| 测试 | 新增 60（orchestrator 54：36 单测 + 18 集成，含 FakeRunner 全 FSM 场景与真 WS 双跳回声链路；http-auth 6）；全 workspace 三件套绿 |

### B.2 验收矩阵（16/16 全过，10 会话）

| 项 | 结果 |
|---|---|
| A. 10 并发 `POST /v1/sessions` | ✅ 859ms 全部 201→Running（colima 动态端口发布 `-p 127.0.0.1:0:8787` 实测可被宿主机转发访问——R-D 解除）；未认证请求 401 |
| B. 每会话经 WS 反代真实 turn | ✅ 9 轻量 turn + 1 写文件 turn（审批自动 Proceed），并发 3 共 8.4s；事件流见 `tool_calls`/`tool_result`（注意：kind 是复数 `tool_calls`，mvs1 脚本的单数计数是错的）；`docker exec cat` 卷内 proof 文件地面真值核对 |
| C. `docker kill` 受害容器 → 前端重连 | ✅ 重连即自动检死→Resuming→新容器挂同卷：5.1s 重连成功；`docker exec` 证卷内文件跨容器存活；session/list 见杀前会话；恢复后引擎从历史正确答出 proof 内容（零工具） |
| D. 编排器进程重启 | ✅ 1.3s healthz；路由表对账 10/10 重挂 Running（容器活着的保持 Running，**不是**全部标 Crashed——比 supervisor 的盲标更聪明）；WS 反代立即复用 |
| E. `DELETE` ×10 | ✅ 容器与卷零残留（`docker ps -aq`/`volume ls` 双零） |

### B.3 实现期发现与决策落地

1. **并发持久化竞争（真 bug，验收首轮抓出）**：整文件 `write(tmp)→rename`
   在并发迁移下共享同一 tmp 名 → rename 互抢 ENOENT。修复：persist 串行锁
   + uuid 唯一 tmp 后缀；回归测试 10 并发 insert+persist。
2. **检死时机**：MVS2 无后台健康轮询——崩溃检测放在 **WS 连接建立时**
   （TCP 探活失败 → CAS Running→Crashed → 当场触发 resume），恰好匹配 D6
   「下一次前端请求触发 Resuming」，零轮询成本。
3. **D7 取舍（已确认）**：引擎 `/ws` 无认证钩子 + 引擎零改动约束 →
   每会话内部密钥推迟；MVS2 缓解 = 容器端口只发布到 `127.0.0.1`。
   前端→编排器 Bearer 全端点强制（ws 额外支持 `?token=`，浏览器握手
   设不了 header）。
4. **休眠 sweep**：idle 判定 = `active_conns==0 && 帧级 last_activity 超时`
   （代理泵内 bump，引擎流量即心跳）；CAS 是权威判定，列表只是候选。
   `idle_timeout_secs=0` 真禁用（CLI 语义一致）。
5. **`BearerSecret::guard` 返回 `Option<Response>`** 而非 `Result`——
   clippy `result_large_err`（axum Response ≥128B）。
6. 编排器控制面延迟可忽略：创建（含 docker create+start+引擎端口就绪）
   单会话 ~300ms（colima 热路径）；反代双跳 turn 与 MVS1 直连无可感知差异
   （R2 符合预期）。

---

## 附录 C：MVS3-A 验收记录（PgWorkingStateStore，2026-09-12）

环境：与附录 A/B 同机（macOS/arm64 + colima + docker）；镜像
`oneai-engine:mvs1` **带 `--features oneai-cli/postgres` 重建**（497MB）；
Pg = `postgres:16` 一次性容器（`-p 5432:5432`，库 `oneai_mvs3`）；provider =
宿主 config.toml 只读 bind-mount（真实 LLM 调用）。验收驱动：
`deploy/docker/mvs3_verify.mjs`（一键 `./deploy/docker/mvs3_run.sh`）。

### C.1 验收矩阵（14/14 全过）

| 项 | 结果 |
|---|---|
| A. per-session env 注入 DSN 建会话 | ✅ 2×201 Running（~260ms）——`POST /v1/sessions` body `env` 字段即够，**编排器零代码改动**（passthrough_env 为部署期等价路径） |
| B. 引擎后端选择 | ✅ 容器日志 `working-state: Postgres (shared)`；无 feature 缺失/降级警告 |
| C. 宿主 psql 种子任务 → 容器内 `oneai tasks list` | ✅ JSONB 种子（TaskCreated+StepAdded+brief）被容器内 CLI 真反序列化列出——容器→宿主 Pg 读路径 + serde 线上格式地面真值 |
| D. 新会话真实 turn 首轮 surface | ✅ 模型逐字答出种子 goal（`[Unfinished Work From Previous Sessions]` ← 引擎 `list_open_tasks` 走 Pg，5.1s/turn） |
| E. **kill 容器 + 删光两卷** → 重连自动 Resuming | ✅ 3.2s 重连 Running；新容器 `tasks/` 零文件（空卷地面真值）；`tasks list` 仍见种子任务（**只可能来自 Pg**）；恢复后引擎 turn 再次 surface（10.1s） |
| F. 清理 | ✅ DELETE×2 → 容器/卷零残留；种子行清库 |

### C.2 实现期发现与决策落地

1. **`$n::jsonb` 单 cast 陷阱**：Postgres 把参数类型解析成 jsonb，
   tokio-postgres 的 `ToSql for String` 拒发（"error serializing
   parameter"）——改 `$n::text::jsonb` 双 cast，免驱动 serde_json feature。
2. **N 容器同库冷启动 DDL 竞态（验收前测试抓出，两轮）**：并发
   `CREATE TABLE IF NOT EXISTS` 撞 pg_type 唯一键；`CREATE INDEX IF NOT
   EXISTS` 命中已有索引仍拿表级 ShareLock，与其他容器 DML 互锁
   （E40P01 deadlock）。修复：稳态 boot 先 `to_regclass` catalog 探测
   （零关系锁跳过 DDL），冷库才在 advisory lock（key 0x4F4E4149）下建表，
   锁内二次探测防重复 DDL。
3. **`connect()` fail-fast 建表**：验收 C 阶段抓出"引擎已宣布选 Pg 但表
   还不存在"（DDL 原为懒触发）——改 `connect()` 即时 `ensure_schema()`：
   后端选中即表就绪，Pg 不可达/无 DDL 权限在启动当场响亮降级，不拖到
   会话中途 append 才炸（事件日志是恢复命脉，迟发静默失败是最坏模式）。
4. **colima 无 `host.docker.internal` 自动注入**（Docker Desktop 专有），
   DockerRunner argv 又不带 `--add-host` → DSN 用 bridge 网关
   `172.17.0.1`（`-p 5432:5432` 发布到 VM 全接口即可达）。README §12 已录。
5. **多写者一致性设计**：`append_event`/`compact_if_needed` 事务先锁
   brief 行（`INSERT … ON CONFLICT DO NOTHING` + `SELECT … FOR UPDATE`）
   ——同 task 并发写串行化（READ COMMITTED 下 brief 重导出必见全部已提交
   事件），不同 task 完全并行；集成测试 10 并发同 task append 零丢失、
   brief 与日志严格一致。

---

## 附录 D：MVS3-B 验收记录（PgMemoryStore/PgUsageTracker/PgHostAllowlist，2026-09-13）

环境：与附录 C 同机（macOS/arm64 + colima + docker）；镜像
`oneai-engine:mvs1` 用含 MVS3-B 代码的源码重建（497MB）；Pg =
**`pgvector/pgvector:pg16`** 一次性容器（`-p 5432:5432`，库 `oneai_mvs3`；
MVS3-B 起 pgvector 为 memory 硬依赖，`mvs3*_run.sh` 已加镜像守卫）；
provider = 宿主 config.toml 只读 bind-mount（真实 LLM 调用）。验收驱动：
`deploy/docker/mvs3b_verify.mjs`（一键 `./deploy/docker/mvs3b_run.sh`）。

### D.1 验收矩阵（23/23 全过）

| 项 | 结果 |
|---|---|
| A. per-session env 注入 DSN 建会话 | ✅ 2×201 Running（289ms，同 A 轮——编排器持续零改动） |
| B. 引擎后端选择 | ✅ 容器日志**四行齐全**：`working-state/memory/usage/host-allowlist: Postgres (shared)`；无回退告警 |
| C. 真实 turn + Pg 地面真值 | ✅ 固定会话 id 真实 LLM turn 复述暗号（2.0s）；psql 证 `conversations_pg` 落行、`usage_records_pg` 落真实 token；`session/list` 走 Pg 覆写路径列出会话；`session/rename` → `conversations_pg.title`+`metadata.title` 定向 UPDATE 同步 |
| D. 白名单跨容器共享 | ✅ 容器1 `host/allow` → `host_allowlist_pg` 落行 → **容器2** `host/list` 直接可见（零卷共享，Pg 唯一真相源，77ms）；`host/deny` 同事务清 admit 行（互斥语义） |
| E. **kill 容器 + 删光两卷** → 记忆跨容器死亡存活 | ✅ 3.2s 重连 Running；新容器本地 SQLite 零痕迹（地面真值）；`session/list` 仅凭 Pg 恢复会话；`session/load` 回放 + 真实 turn 逐字答出暗号（5.3s——**记忆只可能来自 Pg**）；usage 台账继续累计；C 阶段 rename 存活 |
| F. 无 pgvector 优雅降级 | ✅ 宿主侧指向临时 `postgres:16`（无 pgvector，端口 5433）：`PgMemoryStore connect failed … extension "vector" is not available` 响亮告警 + 回退 SQLite memory；**其余三 store 照常选 Pg**（独立降级，互不拖累，2.3s） |
| G. 清理 | ✅ DELETE×2 → 容器/卷零残留；验收行清库 |

集成测试（开发侧）：39 测全绿——`pg_memory_store` 13（镜像 SqliteSessionStore
单测 + pgvector KNN 相似度/维度隔离/空查询 + 并发 + 重连存活）、
`pg_usage_tracker` 8（镜像 + `is_estimated` roundtrip）、`pg_host_allowlist` 8
（镜像互斥/重开存活/list·remove）；`ONEAI_TEST_PG_DSN` env 门控 + `#[ignore]`。

### D.2 实现期发现与决策落地

1. **App 会话面路由缺口（实现期发现，计划外必修项）**：`App` 的
   `list/rename/archive/delete/create_session_with_id`（即 webUI
   `session/*` RPC 的全部落点）原**硬连 `sqlite_store` 具体类型**——Pg 记忆
   模式下 turn 尾经 MemoryManager 落 Pg，而会话列表读本地 SQLite → 云端
   webUI 列表恒空、resume/rename 全断。修复：`AppBuilder::memory_persistence`
   留存 Arc → `App.memory_persistence` 覆写会话面五方法；turn 尾/compact 自动
   落盘 gate 改 `conversation_persistence_enabled()`（sqlite OR 覆写）。
2. **rename/archive 进 trait**：`MemoryPersistence` 加法增补
   `rename_conversation`/`set_conversation_archived` 默认方法（泛型
   load-modify-save），SQLite/Pg 各自覆写为**定向 metadata UPDATE**（不重写
   消息 blob——避免与并发 turn 回存互踩；`SqliteSessionStore` 固有方法经
   路径限定委托，无递归）。
3. **pgvector 无维度 `vector` 列 + `vector_dims` 过滤**：混合嵌入模型共库
   （维度异构）时 `<=>` 遇异维行会**报错**（非返回 0）——`WHERE
   vector_dims(embedding) = vector_dims($1)` 先行过滤（WHERE 逐行先于
   ORDER BY 求值，安全），语义对齐 SQLite 后端"维度不匹配 → 0 分 → 滤除"。
   KNN 相似度 = `1.0 - (embedding <=> $1)`；`score > 0` 过滤与 NaN（零向量）
   排尾行为均对齐 brute-force 版。
4. **`UsageRecord` 是 `#[non_exhaustive]`**：crate 外不能结构体字面量构造
   ——经 `with_timestamp`+`with_cache_tokens` 重建再字段赋值 `is_estimated`
   （Pg 表补了 SQLite 缺的 `is_estimated` 真列，旗标不再 round-trip 丢失）。
5. **`ILIKE` 补齐 `LIKE` 语义**：Postgres `LIKE` 大小写敏感而 SQLite `LIKE`
   ASCII 不敏感——`search_ltm_keyword` 用 `ILIKE`（含 metadata_json::text）
   恢复 parity。
6. **advisory lock key 注册表**：四 store 各占一键
   （1330538825/26/27/28，`pg_common.rs` 模块文档），冷启动 DDL 互不串行；
   `CREATE EXTENSION vector` 在 memory store 锁内执行（扩展库级共享，锁已
   串行化全部 DDL）。
7. **验收脚本 F 轮竞态（首轮 22/23 的唯一失败项，非产品缺陷）**：等待循环
   在 memory 告警出现瞬间杀进程，`usage/host-allowlist` 选择行还没打完——
   改为等**整个启动选择序列**（告警+三行标记）齐活再杀。复跑 23/23。
8. **验收环境坑**：colima 稀疏盘镜像不自动缩——`docker builder prune` 后
   须 `colima ssh -- sudo fstrim -av` 才归还宿主空间（本轮实测 27GB 回收，
   否则宿主链接器 `errno=28` 磁盘满）。

---

## 附录 E：MVS3-C 验收记录（PgSessionEventStore/PgFeedbackStore + 深度休眠卷归档 + 瘦身/预热，2026-09-13）

环境：与附录 C/D 同机（macOS/arm64 + colima + docker）；镜像
**`oneai-engine:mvs3c`**（`--no-default-features --features oneai-cli/postgres`
重建，无 ONNX 链 + shiki 裁剪后 dist + 无 bubblewrap，**111MB** = B 轮 497MB
的 22%）；Pg = `pgvector/pgvector:pg16`（oneai-pg-test，库 oneai_mvs3）；
provider = 宿主 config.toml 只读 bind-mount（真实 LLM 调用）。验收驱动：
`deploy/docker/mvs3c_verify.mjs`（一键 `./deploy/docker/mvs3c_run.sh`）——
双编排器实例（主实例 idle=3600 + 归档实例 idle=5s/deep=5s/archive_dir=
`$HOME` 下临时目录，colima 挂载约束见 E.2-9）。

### E.1 验收矩阵（32/32 全过；首轮 20/23——E/F 三项败于归档目录对 docker VM 不可见，E.2-9/10 修复后复跑全绿）

| 项 | 结果 |
|---|---|
| A. per-session env 注入 DSN 建会话 | ✅ 201 Running（263ms，编排器持续零改动） |
| B. 引擎后端选择 | ✅ **六行齐全**（working-state/memory/usage/host-allowlist/**session-events/feedback**: Postgres (shared)）+ 无回退告警 + **`prewarm: model context ready`**（预热在探活通过前完成） |
| C. feedback/trajectory 落 Pg | ✅ 真实 turn 答暗号（2.3s）；feedback/submit×2（up+note）→ `message_feedback_pg` 2 行、feedback/list 回读一致（含中文备注）；session/trajectory 返回 7 事件 = `session_events_pg` 7 行（psql 地面真值） |
| D. 镜像瘦身 | ✅ **111MB**（阈值 400MB；B 轮 497MB——ONNX 静态链 + dist 27MB→7MB + bubblewrap 全去掉） |
| E. 深度归档全生命周期 | ✅ proof 文件写入卷 → 断连 30s 自动 Hibernating → 42s 自动 deep-archive：容器+两卷从 docker 消失、2×tar.gz+manifest.json 落盘（state 1579B / ws 152B）、status.archived 带 manifest → WS 重连 0.5s restore+spawn Running → `docker exec cat` proof **逐字存活**（只可能来自 tar 恢复）→ 标记清除+归档目录删除 → session/load + 真实 turn 答出归档前暗号（Pg 记忆 + 卷恢复双通道） |
| F. 归档失败红线 | ✅ archive_dir 置 0555 → sweep 导出失败（EACCES）：保持 Hibernating、**两卷一个不少**、last_error 带 `deep-archive failed` 诊断；恢复 0755 → 下轮 sweep 归档成功 → 重连 resume 再次恢复 Running |
| G. kill+删光两卷仅凭 Pg 恢复 | ✅ 3.2s 重连 Running；session/list（message_count=2）、**feedback/list（👍+中文备注跨卷死亡存活）**、**session/trajectory（7 事件跨卷死亡存活）**全部只可能来自 Pg；真实 turn 答出暗号 |
| H. 清理 | ✅ DELETE×2 → 容器/卷零残留（含归档会话的合成 handle 清卷路径）；验收行清库 |

集成测试（开发侧）：`pg_session_event_store` 5 + `pg_feedback_store` 4（新增），
既有 39 Pg 测试回归全绿（合计 48）；orchestrator 71 测（+9 深度归档集成
+5 archive 单测 +3 config +1 fsm legacy 兼容）；全 workspace **2476 tests** /
fmt / clippy / deny 绿。web：vitest 88/88、playwright 13/14（trajectory.spec
的 SVG circle 点击拦截失败为 HEAD 既有问题，与本轮无关，已复核）。

### E.2 实现期发现与决策落地

1. **事件 `line` 列用 TEXT 而非 JSONB**（对计划的偏离，有意）：trait 契约是
   「opaque JSON string」，TEXT 保证字节级 round-trip（JSONB 会归一 key 序/
   拒收非 JSON 行）；运营侧 JSON 查询日后可加 generated jsonb 列 + GIN，
   零载荷迁移。
2. **深度归档不加 FSM 状态**：`PersistedEntry.archived` 标记字段（serde
   default，旧 sessions.json 兼容测试固化）；Hibernating+archived=深度归档。
   恢复入口按**标记**而非状态分派——restore 失败落 Crashed 后标记仍在，
   下次请求仍会先 restore（绝不落到「空卷 spawn」把会话洗白）。
3. **红线序列**：archive 确认成功 → `cas_set_archived` claim（并发 sweep
   单次归档，集成测试固化）→ `destroy(true)`。claim 后 destroy 失败可恢复
   （resume 的 spawn 容忍残留容器，restore 的 tar 覆写残留卷）。
4. **归档所有权**：tar 在 alpine helper 容器内以 root 写 bind-mount——Linux
   宿主非 root 编排器可能删不掉归档文件（尽力 chmod 644 已做）；README §17
   记录部署约束（root 或 uid 映射挂载）。
5. **feedback trait 分层**：`FeedbackStore` trait 在 oneai-app-server，
   persistence 不可依赖 → `PgFeedbackStore` 固有 API + CLI
   `PgFeedbackStoreRpc` adapter（复刻 PgHostAllowlistRpc）；`apply_pg_backends`
   扩三元组，7 调用点同步。
6. **fastembed 可选化影响面**（实测）：真实代码仅 oneai-rag 三文件
   （embedding.rs/provider_adapter.rs/lib.rs）；core 的
   `EmbeddingProvider::FastEmbed` 枚举变体不 gate（配置序列化兼容），feature
   关时 resolve_one 响亮告警 + Ok(None) 关键词降级。AUTO_CHAIN 常量对改
   `auto_chain()` 函数（ort×fastembed 四组合免爆炸）。
7. **预热位置**：`build_engine_server` 内 create_session 后、返回（=bind）前
   ——web 与 app-server 两入口共用；30s 超时兜底，坏 provider 不阻塞健康化。
8. **shiki 细粒度打包**：`shiki/core` + `@shikijs/langs/*` 显式 12 语言 +
   `@shikijs/themes` 2 主题 + oniguruma engine；`createHighlighter('shiki')`
   全量入口会把 ~60 语法全部 dynamic-import 进 dist（`langs:` 选项只控制
   预载不控制 tree-shake）——这是 27MB 的真正来源。dist → ~7MB/5 文件。
9. **archive_dir 对 docker VM 的可见性（首轮验收 E/F 三项失败的根因，环境
   约束非产品缺陷）**：colima VM 只挂载 `$HOME` 与 `/tmp`，macOS `$TMPDIR`
   （`/var/folders/…`）作 bind-mount 源时 daemon 以自动创建的空目录顶替——
   宿主 `create_dir_all` 的会话子目录在容器内不可见，busybox tar 又不创建
   输出路径的前导目录（`can't open '/archive/<sid>/…'`）。验收脚本改在
   `$HOME` 下建归档目录 + **canary 前置检查**（宿主写文件→容器内必须读到，
   不可见直接 exit 2）；README §17 记录部署约束。
10. **导出 argv 容器内自建子目录**：`build_volume_export_argv` 从裸 tar 改
    `sh -c "mkdir -p /archive/<sid> && tar czf …"`——消除对宿主目录经
    bind-mount 可见性的依赖（VM 型 daemon 的挂载传播时序也一并覆盖）；
    失败红线语义不变（mkdir/tar 任一失败 → Err → 卷不动）。

## 附录 F：MVS4-A 验收记录（多副本编排器：Pg 共享路由表 + 每会话租约，2026-09-14）

环境：macOS/arm64 + colima（docker 29.5.2）+ pgvector/pgvector:pg16 容器
（`oneai-pg-test`，库 `oneai_mvs4`；编排器宿主侧 DSN 走 127.0.0.1，引擎
容器侧走 bridge 网关 172.17.0.1）。宿主二进制 `cargo build -p oneai-cli
--features postgres`；引擎镜像**直接复用 `oneai-engine:mvs3c`**（本轮
引擎侧零改动，111MB）。验收驱动：`deploy/docker/mvs4a_verify.mjs`
（自包含拉起最多 5 个编排器进程：主对 rep-a/rep-b + 短超时归档对
rep-c/rep-d + 文件模式回归实例；`mvs4a_run.sh` 一键跑）。lease_ttl=10s
（加快验收节奏；生产默认 30s）。

### F.1 验收矩阵（21/21 全过；首轮 19/21——E2/E3 两项败于验收脚本自身
###     断言，见 F.2-8/9，修脚本后复跑全绿）

| # | 项 | 结果 |
|---|---|---|
| A1 | banner：Backend Postgres (shared routing table, multi-replica) + replica rep-a + lease ttl | ✅ |
| A2 | DDL 冷启：`orchestrator_sessions` + 双索引（lease 过期扫描/state+owner） | ✅ |
| A3 | POST /v1/sessions → 201 Running；落行 owner=rep-a 且 lease_expires_at > now() | ✅ |
| A4 | WS 经 A 真实 turn 答出暗号；`last_activity_ms` 节流落库（心跳 tick 顺带 flush） | ✅ |
| A5 | DELETE → 容器/卷零残留 + Pg 行删除 | ✅ |
| B1 | 第二副本 rep-b 启动（同 DSN，异 registry） | ✅ |
| B2 | A 持新租约时 WS 连 B → **409 + `X-Oneai-Owner-Replica: rep-a`** | ✅ |
| B3 | 租约过期后 WS 连 B → claim 成功代理；owner 翻 rep-b | ✅ |
| B4 | B 建会话 owner=rep-b；A/B `GET /v1/sessions` 收敛一致 | ✅ |
| C1 | 经 A 建 s4 + 真实 turn 暗号（基线） | ✅ |
| C2 | **kill -9 A** → 租约过期 → WS 连 B 接管成功；psql owner=rep-b | ✅ |
| C3 | 接管零容器操作：`docker inspect StartedAt` 逐字节不变 | ✅ |
| C4 | 接管后真实 turn 答出 A 时代暗号（同引擎进程存活） | ✅ |
| D1 | B 持活跃租约（WS 心跳中）时重启 A → 不夺租、不误判 Crashed | ✅ |
| D2 | ghost 过期租约 + A/B **同时**重启对账 → 每条恰一 owner（实测自然分片 s2/s4→a、s3→b）、全 Running、容器未动 | ✅ |
| E1 | 经 C 建 s5 + 真实 turn 暗号（短超时归档对 idle=20s/deep=6s） | ✅ |
| E2 | **kill -9 C** → D 的 sweep 接管整链：自动休眠 → deep-archive（tar.gz×2 + manifest、容器+卷消失、archived 落库、D 日志自证） | ✅ |
| E3 | WS 经 D 重连 → Resuming（restore+spawn）→ `session/load` + 真实 turn 答出归档前暗号；archived 清空、卷回归 | ✅ |
| F1 | 红线：archive_dir 只读 → 保持 Hibernating + 卷 intact + `last_error` 诊断；恢复可写 → 下轮归档成功 | ✅ |
| F2 | `lease_ttl=0` + Pg DSN → validate 启动拒绝（exit≠0，日志含 lease_ttl_secs） | ✅ |
| F3 | 文件模式回归：无 DSN → banner file、建会话 Running、sessions.json 落盘、删除零残留 | ✅ |

### F.2 实现期发现与决策落地

1. **claim-on-act 取代常驻心跳**：所有权不是粘滞的——WS upgrade/sweep/
   深度归档/reconcile 各自在动作前 claim，`LeaseGuard` 只在动作期间心跳
   （ttl/3），drop 即释放。好处：无需后台「保有权」任务，副本死亡只影响
   它正在服务的连接；闲置会话无 owner，任意副本可即时接管（B3/C2 实测
   13s 内完成 = ttl+探测）。
2. **缓存合并的 Arc 稳定性**：热缓存的 clone-and-replace 必须只在持久字段
   真变化时发生——每次读都替换 Arc 会孤儿化 `ConnGuard`/sweep 持有的旧
   Arc，`active_conns` 计数泄漏（会话永不休眠）。活动钟用 `fetch_max`
   原地单调合并（异地副本的 flush 只升不降本地钟），同样不换 Arc。
3. **CAS 不需要 updated_at 版本号**：state 等值条件 + 行锁已是线性化 CAS
   （与 MVS2 内存版语义一致）；ABA 状态回环在 FSM 边集下无害。原计划的
   `prev_updated_at` 乐观锁被删除，trait 面更窄。
4. **深度归档必须持 LeaseGuard 而非裸 claim**：导出是分钟级 docker save|
   gzip，裸 claim 的租约会在 tar 中途过期，引来另一副本并发写同一确定性
   布局（损坏归档）。guard 心跳护住整个「导出→CAS→destroy」段。
5. **对账先 claim 再探活**：claim 即跨副本互斥——两副本同时重启对 5 条
   孤儿恰好 5 次探活（multi_replica_tests 断言探活计数），败者
   `HeldByOther` 直接跳过。
6. **Pg 模式活动钟首见语义**：store 行 `last_activity_ms>0` 时用真值播种
   本地钟（否则接管副本会把死副本的旧会话当「刚活跃」白宽限一轮）；
   =0（从未 flush）才落回 from_persisted 的 now 宽限。
7. **编排器 tracing 从未接线**（验收 E2 找地面真值时发现）：serve 进程
   没有 subscriber，reconcile/租约/sweep/deep-archive 的 info/warn 全部
   被丢弃。已接 `tracing_subscriber::fmt` → stderr（RUST_LOG 可控）。
8. **验收首轮 E2 失败是断言错误**：归档完成后 owner 为空是 claim-on-act
   的正确行为（guard drop 释放），断言却要求 owner=rep-d 持久存在；改用
   D 日志（`deep-archived` + session id）做「是 D 干的」地面真值。
9. **验收首轮 E3 失败是脚本漏 step**：归档恢复后容器是全新引擎进程，
   直接问暗号必然失败——须先 `session/load`（mvs3c E 段同款双通道，
   Pg 记忆 + 卷恢复）。
10. **MVS4-A 边界（明确记录）**：多副本 = 同 docker 宿主多进程（容器端口
    发布在 127.0.0.1，跨宿主不可达）；跨宿主/K8s 网络留给 K8sRunner 轮。
    租约丢失中途代理的窄竞态（心跳 gap>TTL 才被接管，接管方 sweep 又有
    活动落库二次防护）本轮接受，监控 `lease lost mid-flight` 告警日志。
11. **测试矩阵**：单测/集成（无外部依赖）91 项 + Pg 门控集成 9 项
    （`ONEAI_TEST_PG_DSN`，真 pgvector：并发 CAS/lease 恰一赢家、过期
    接管、GREATEST 单调）+ multi_replica_tests 8 项（MemLeaseStore 镜像
    Pg 契约：双 state 共享真相、真 axum+真 WS 握手收 409、kill 接管、
    并发对账、归档竞态）。文件模式既有 82 测零修改全绿（D8 承诺兑现）。

## 附录 G：MVS4-B 验收记录（租户配额限流 + OTEL tenant/session 贯穿，2026-09-14）

环境：macOS/arm64 + colima（docker 29.8.0）+ pgvector/pgvector:pg16 容器
（`oneai-pg-test`，库 `oneai_mvs4b`；编排器宿主侧 DSN 走 127.0.0.1，引擎
容器侧走 bridge 网关 172.17.0.1，OTLP 桩走 lima VM→host 192.168.5.2）。
宿主二进制 `cargo build -p oneai-cli --features postgres`；引擎镜像
**`oneai-engine:mvs4b`（重建，106MB）**——本轮引擎侧有改动（CLI 层用量
打标装饰器 + OTEL bootstrap + session.rs span 属性/seed + oneai-trace
collector 桥接）。验收驱动：`deploy/docker/mvs4b_verify.mjs`（自包含拉起
五个编排器进程：主实例 rep-m 无配额带 OTEL + 三个配额实例 rep-bq/rep-cq/
rep-dq + 文件模式 rep-f，外加 node OTLP 捕获桩 :4318；`mvs4b_run.sh`
一键跑）。lease_ttl=10s。

### G.1 验收矩阵（25/25 全过；前两轮 22/25——G2-G4 败于验收环境两坑，
###     见 G.2-1/2，产品代码零改动，修脚本后复跑全绿）

| # | 项 | 结果 |
|---|---|---|
| A1 | banner：Backend Postgres + rep-m + Quotas disabled + OTEL endpoint | ✅ |
| A2 | DDL：`tenant_id` 列 + `idx_orch_sess_tenant` 部分索引存在 | ✅ |
| A3 | body `tenant_id` 建会话 → 201 + snapshot.tenant_id + Pg 列三方一致 | ✅ |
| A4 | `X-Oneai-Tenant` 头兜底（body 缺省时生效） | ✅ |
| A5 | 非法 tenant_id（含空格/感叹号）→ 400 invalid tenant id | ✅ |
| A6 | `docker inspect` Env 四契约变量：ONEAI_TENANT_ID/ONEAI_ORCH_SESSION_ID/OTEL_EXPORTER_OTLP_ENDPOINT/TRACEPARENT（W3C 格式校验） | ✅ |
| G1 | 真实 turn 答出暗号（引擎+provider 基线） | ✅ |
| G2 | **OTLP 桩收到引擎 span 且 traceId == 注入 TRACEPARENT 的 trace id**（spans=2：agent_loop+inference） | ✅ |
| G3 | OTEL resource 属性带 tenant.id=acme + orchestrator.session.id | ✅ |
| G4 | 导出含 agent_loop（引擎主循环真贯穿，非仅资源声明） | ✅ |
| G5 | **用量行引擎侧打标**：SUM(acme)=8194>0 且 metadata.orch_session_id 可回联路由表 | ✅ |
| B1 | max_concurrent=2：前两个 201 Running | ✅ |
| B2 | 第 3 个 → 429 reason=concurrent_sessions + limit/current 正确 + **无 Retry-After** | ✅ |
| B3 | destroy 释放槽位 → 再建 201 | ✅ |
| B4 | 异租户桶独立（qt 满员不影响 qt2） | ✅ |
| C1 | banner 自证 usage 源接线（token budget reads usage_records_pg） | ✅ |
| C2 | 已耗预算租户（G5 真实用量）→ 429 reason=token_budget（8194/1） | ✅ |
| C3 | 零用量新租户放行 201 | ✅ |
| D1 | rate=3/min burst 6 并发 → **恰 3 成 3 拒** reason=create_rate + Retry-After≥1 | ✅ |
| E1 | `?tenant=` 过滤：xx/yy 各归各、default 匹配未打标、全量含所有 | ✅ |
| F1 | 文件模式 banner：Backend file + default 桶 max_sessions=1 | ✅ |
| F2 | 未打标会话归 default 桶：第 1 个 201、第 2 个 429（file 模式配额生效，tenant_id="default"） | ✅ |
| F3 | 命名租户桶独立于 default（ff 首个 201） | ✅ |
| J1 | sessions.json 落 tenant_id（命名租户持久化 + 未打标空串） | ✅ |
| J2 | 文件模式删除零残留（容器+卷） | ✅ |

### G.2 实现期发现与决策落地

1. **daemon 级代理注入劫持 OTLP（验收首轮 G2-G4 失败的根因）**：colima 的
   dockerd 配了代理 → 所有容器被注入 `HTTP(S)_PROXY`，默认 NO_PROXY 只有
   localhost/*.local——引擎 OTLP POST 走代理得 502（Pg 不受影响：
   tokio-postgres 直连不经 reqwest）。修法零产品改动：create 请求 env 显式
   `NO_PROXY`（含桩地址）覆盖 daemon 注入（docker 显式 -e 优先）。生产
   同理：collector 地址须在容器 NO_PROXY 内或代理可达——已记 §6 B 轮边界
   + deploy README §23。
2. **colima 网络方向二分**：容器→published 容器端口（Pg）走 bridge 网关
   172.17.0.1；容器→**macOS 宿主进程**（OTLP 桩）必须走 lima VM→host
   192.168.5.2（host.lima.internal 的 IP；容器内无该 DNS，直连 IP；
   172.17.0.1 实测不通）。首轮误用 172.17.0.1 是 G2-G4 失败的另一半。
3. **P2-3 遗留断链修复（本轮最重要的引擎侧发现）**：`TraceContext` 的
   collector 字段自诞生起就是 dead_code（`on_span_start/end` 全仓零调用，
   otel_exporter 旧测试甚至断言 completed_count==0 并注明"may not have
   been called"）——`trace_otel` 的 OTLP 导出从未真正送出过任何 span。
   B 轮补上 enter/exit→collector 桥接（runtime 内 detached spawn）后，
   G2-G4 才有意义。
4. **session 根 span 永不导出 → trace id 碎裂**：长驻引擎的 session span
   不 end，其子 span 导出时 parent 缺席 batch，原 root-walk 会把 parent id
   当 trace id（每 span 一个 trace）。修法：`Span.trace_id_override`
   （seed 时盖章到 context，enter_span 复制到每个新 span，导出端优先取）。
   G2 的 traceId==TRACEPARENT 断言即验证此路径。
5. **配额竞态收进单仲裁单元**：并发帽不是"查数再插"两步——
   `insert_if_under_quota` 在 Pg 侧是单事务 `pg_advisory_xact_lock(
   hashtext(tenant))`+COUNT+条件 INSERT（20 并发 max=5 恰 5 赢家，Pg 门控
   测试与 D1 双副本真机各自验证）；File 侧 op_lock commit；MemLeaseStore
   测试替身单 mutex hold 镜像 Pg 契约。
6. **预算 SUM 依赖引擎打标先行**：C2 的拒绝量（8194/1）来自 G5 同一 turn
   的真实用量——验收矩阵顺序（G 先于 C）即依赖顺序。装饰器只在
   `ONEAI_TENANT_ID` 非空时包装（端侧引擎/未编排容器零变化）。
7. **429 语义分层**：仅 create_rate 附 `Retry-After`（等一拍即恢复）；
   concurrent_sessions/token_budget 不附（重试无益——须删会话/调预算），
   客户端按 `reason` 判别。B2/C2/D1 分别断言。
8. **测试矩阵**：单测/集成（无外部依赖）orchestrator 70 + trace 52 +
   app 43 + persistence 78 全绿；Pg 门控 pg_store_tests 12（+3：tenant
   roundtrip/legacy 行/20 并发恰 5 赢）+ pg_usage_tracker 9（+1：
   tenant_token_sum 聚合/隔离/窗口）；multi_replica_tests 10（+2：双副本
   8 并发同租户 max=3 恰 3 Running+5 拒、限流桶每副本独立）。既有测试仅
   动一处：idle_sweep 改显式回拨活动钟（原隐式依赖 ≥1ms 墙钟流逝，
   本身脆弱，非本轮语义变化）。

### G.3 回归

- **MVS4-A 验收原样重跑 21/21 全绿**（新宿主二进制 + `oneai-engine:mvs4b`
  镜像；未配配额/OTEL 时行为零变化的承诺兑现）。唯一脚本侧修正：A4 原
  断言假设「turn 时长 > 一拍心跳（ttl/3）→ 活动必已节流落库」，provider
  快时（本轮实测 2.7s < 3.3s 首拍）偶发假阴性——改为先 close（LeaseGuard
  drop 强制 flush）再轮询等落库，断言语义不变（产品代码零改动；A 轮
  F.2-8/9 同款「验收脚本自身断言修正」先例）。
- 既有单测/集成：orchestrator 70 + trace 52（含 otel feature）+ app 43 +
  persistence 78 + multi_replica 10 全绿；idle_sweep 既有测试同因（墙钟
  ≥1ms 隐式依赖）改显式回拨活动钟。
