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

### MVS3 —— 存储外部化 + 规模化恢复

- `PgWorkingStateStore`（**优先**：事件日志是崩溃恢复的命脉，且
  read-modify-write 的 `tasks.index.json` 在多写者下需要事务化）、
  `PgMemoryStore`（`MemoryPersistence` 最重，建议拆 conversation/stm/ltm/facts
  四个子 store 分别选型；LTM 向量检索换 pgvector 替代 in-Rust brute-force
  cosine，`sqlite_store.rs:776-845`）、`PgUsageTracker` / `PgHostAllowlist`
  （小表顺手）。统一引入连接池（deadpool-postgres；现状 rusqlite 全线无池化）。
- `AppBuilder` 补 setter 缺口：`host_allowlist_store(...)`、通用
  `memory_persistence(...)`（当前只能经 `memory_manager()` 间接注入，
  `builder.rs:635,2092-2105`）。
- 休眠卷归档对象存储（冷会话成本）。
- 容器镜像瘦身 + 启动预热（引擎 warm-up 已有 `warm_model_context` 钩子）。

### MVS4 —— 生产化

- `K8sRunner`（Pod 即容器抽象）；编排器多副本 + 路由表进共享存储 + lease。
- TLS 内置（rustls）或正式约定反代；JWT/OIDC；Secret Manager 对接。
- 配额与限流：每租户并发会话数、token 预算（复用 `UsageTracker` +
  `RateLimiter`/`CircuitBreaker`，状态进 Redis 或接受每编排器副本近似）。
- 可观测：OTEL 已有（`oneai-trace`），补 tenant_id/session_id 贯穿 span +
  容器 metrics 采集。
- egress 治理：容器网络策略（默认拒绝 + 域名放行），与引擎内
  host-allowlist/CONNECT 代理形成双层。

## 7. 存储 trait 外部化简表（MVS3 输入）

| Trait | 现有实现 | 云端缺口 |
|---|---|---|
| `MemoryPersistence`（core/traits.rs:1332） | 仅 `SqliteSessionStore` | `PgMemoryStore`（拆 4 子 store；pgvector） |
| `WorkingStateStore`（core/traits.rs:763） | `FileWorkingStateStore`、`NoTaskStore` | `PgWorkingStateStore`（事件表 + index 表事务化）——**最优先** |
| `SessionEventStore` | `FileSessionEventStore` | Pg 或对象存储 append-only |
| `HostAllowlistStore`（core/traits.rs:703） | Sqlite / InMemory / Seeded | `PgHostAllowlist`（保留 Seeded 装饰器） |
| `UsageTracker` | `SqliteUsageTracker`（同样无池化） | `PgUsageTracker` + 批量 flush |
| `FeedbackStore`/`ConversationStore`（app-server 层） | InMemory + App wrapper | Pg 直连实现，去 App 中转 |
| `StatePersistence`（checkpoint, traits.rs:730） | 无生产实现 | 编排器休眠快照元数据可用 |

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
| R3 | 编排器单点 | MVS2 单副本 + `sessions.json` 原子持久化对账恢复（重启秒级重挂，✅ 附录 B.2-D 实测 1.3s/10 会话）；MVS4 多副本 + lease |
| R4 | 容器内嵌套沙箱（bwrap-in-docker）兼容性 | **✅ MVS1 已判：不可用**（Ubuntu 24.04 宿主系统级 AppArmor 限制非特权 userns，`--privileged`/unconfined 均无法恢复；VM 层 root 可用、非 root 不可用）。引擎已补 `is_available` 运行期探测自动降级 RegexBackend（附录 A.3）；容器本身是隔离边界（§8），生产纵深防御走 gVisor/kata runtimeClass（MVS4） |
| Q1 | 端云冷迁移（导出/导入会话）要不要做 | 状态格式天然兼容（同 schema 卷），做「拷卷」即可；产品化另议 |
| Q2 | 企业合规（SOC2/HIPAA）对云化的真实驱动强度 | 调研中该论断未过核验（证据不足），面向企业客户前需单独调研 |
| Q3 | 本地↔云端状态同步的成熟工程实践 | 行业无先例可抄；working-state 事件日志是候选同步单元，暂不设计 |

## 10. 对代码库的改动面汇总

| 层 | 改动 |
|---|---|
| 引擎（core/bus/agent/app） | **零改动**（N1/N2 的排除项）。例外：`oneai-tool` sandbox `is_available` 运行期探测（MVS1 产出的缺陷修复，与环境适配无关，任何 Linux 部署受益，见附录 A.3） |
| `oneai-app-server` | 零改动（ws 监听、serve_web 均已存在） |
| 新增 crate | `oneai-orchestrator`（MVS2）、`oneai-http-auth`（MVS2，抽 a2a/scheduler 重复） |
| `oneai-persistence` | MVS3 加 Pg 后端（新文件，不动现有） |
| `oneai-app` builder | MVS3 补 2 个 setter（`host_allowlist_store`/`memory_persistence`） |
| `oneai-a2a` / `oneai-scheduler` | MVS2 把 Bearer 三件套改指向 `oneai-http-auth`（消重复） |
| CLI | `oneai orchestrator` 子命令（MVS2） |
| 部署件 | Dockerfile（MVS1）、镜像流水线（MVS4） |

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
