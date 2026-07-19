# OneAI Working-State 与跨 Session 续接机制白皮书

> 版本：对应代码库 1.1.0 线。本文基于对 `crates/oneai-core`、`oneai-persistence`、`oneai-agent`、`oneai-app`、`oneai-domain` 源码的逐文件审阅撰写，所有机制均标注 `file:line` 以便核对。设计依据见同目录 `docs/agent-working-state-and-cross-session-resume.md`（调研参考）与 `~/.claude/plans/vectorized-hopping-willow.md`（落地方案）。

---

## 0. 一句话概括

OneAI 的工作状态管理是一个 **「事件溯源的 per-task 文件日志 + 内存投影 + 每步增量持久化 + 跨 session index 发现」** 引擎：一个任务的目标/步骤/决策/卡点不再摊在 session transcript 里，而是作为 append-only JSONL 事件流落到 `<root>/tasks/{task_id}.jsonl`，热路径每轮只读 `LoopState` 内存的投影（零文件 IO），新 session 启动读一次轻量 `tasks.index.json` 即可 surface 上次未完成的工作。整个行为由 DomainPack 第 7 层 `MemoryProfile.working_state` 声明，一行 `AppBuilder::working_state(root)` 开启。

---

## 1. 架构总览：分层与数据流

```
        ┌───────────────────────────────────────────────────────────┐
        │                  AgentLoop (oneai-agent)                  │
        │   控制工具执行点：exit_plan_mode / task_update / decision │
        └───────────────────┬───────────────────────────────────────┘
                            │ append_event（每步增量持久化，§4）
                            ▼
        ┌───────────────────────────────────────────────────────────┐
        │   LoopState.working_state: Option<WorkingState>           │
        │   ← 内存投影（projector 从事件 derive 一次，之后热路径只读它）│
        └───────────────────┬───────────────────────────────────────┘
                            │ inject_pinned_blocks 每轮重注入（零 IO）
                            ▼
        ┌───────────────────────────────────────────────────────────┐
        │   ContextAssembler pinned 块：[Task Anchor] / [Plan &     │
        │   Progress] / [Decisions Made] / [Blockers] /            │
        │   [Unfinished Work From Previous Sessions]（首轮）        │
        └───────────────────────────────────────────────────────────┘
                            ▲
                            │ list_open_tasks（读 index.json，新 session 首轮）
        ┌───────────────────┴───────────────────────────────────────┐
        │   FileWorkingStateStore (oneai-persistence)               │
        │   <root>/tasks/{task_id}.jsonl  ← append-only 事件日志     │
        │   <root>/tasks.index.json      ← 轻量索引（跨 session 发现）│
        │   projector / compaction / archive                       │
        └───────────────────────────────────────────────────────────┘
```

**四个 crate 的职责分工：**

| crate | 角色 | 关键文件 |
|---|---|---|
| `oneai-core` | L0 类型：`WorkingState` / `Step` / `Decision` / `Blocker` / `TaskEvent`；`WorkingStateStore` trait | `types.rs:914`, `traits.rs:386` |
| `oneai-persistence` | 文件后端：`FileWorkingStateStore` + projector + compaction + archive | `working_state_store.rs:36` |
| `oneai-agent` | 投影接线：`LoopState.working_state` + 控制工具 append 事件 + pinned 块渲染 | `agent_loop.rs`, `context_assembler.rs` |
| `oneai-app` | 集成：`AppBuilder::working_state()` + 新 session 注入 `[Unfinished Work]` + resume rehydrate | `builder.rs`, `session.rs` |
| `oneai-domain` | 声明式策略：`MemoryProfile.working_state: WorkingStatePolicy` + `RefreshPolicy::OnResume` | `memory_profile.rs`, `context_source.rs:50` |

---

## 2. L0 类型（事件 vs 投影）

工作状态分两层物：**事件**是 source of truth（append-only，落盘）；**投影** `WorkingState` 是从事件 derive 的运行时视图（内存缓存）。

- `TaskEvent`（`types.rs:1161`）— 每行一个 JSON：`{ id, task_id, session_id, parent_event_id?, event_type, payload, schema_version, ts }`。`parent_event_id` 支持审计/分叉链。
- `TaskEventType`（`types.rs:1194`）— `TaskCreated / GoalRevised / StepAdded / StepStatusChanged / DecisionMade / BlockerRaised / BlockerResolved / NoteAdded / TaskPaused / TaskResumed / TaskCompleted / TaskArchived / Reconciliation / Snapshot`。
- `TaskEventPayload`（`types.rs:1230`）— 各事件类型对应的载荷（新建任务的 goal/intent、步骤的 description/order、决策的 chosen/rationale、卡点的 resolution…）。
- `WorkingState`（`types.rs:914`）— 投影：`{ task_id, user_id, project, goal, intent, status, steps[], decisions[], blockers[], notes[], owner_session, created_at, updated_at }`。
- `TaskStatus` — `Active | Paused | Completed | Archived`；`StepStatus` — `Pending | InProgress | Completed | Failed`。
- `TaskBrief`（`types.rs:1121`）— index.json 单条记录：`{ task_id, goal, status, open_step_count, last_event_ts }`，跨 session 发现只读它。

`PlanStep`（`types.rs:844`）不删——workflow 内部仍用；working-state 侧以 `Step` 为准，`PlanState` 降级为 `Step` 的运行时投影。

---

## 3. L1 事件日志：append-only 文件

存储布局（按 `WorkingStatePolicy.storage_root`）：

- 编码场景：`<project_dir>/.oneai/tasks/{task_id}.jsonl` + `.oneai/tasks.index.json`（in-repo，可 `git diff` 人工审，git 提交 = 免费 durability + 对账 source）。
- 助手场景：`~/.oneai/working-state/{user}/{task_id}.jsonl` + `~/.oneai/working-state/{user}/index.json`。

每个 `{task_id}.jsonl` 是 append-only 事件流，每行一个事件 JSON。**写路径唯一**——`append_event`（`traits.rs:411`）：append 一行 + 增量更新 `tasks.index.json` 对应条目。读路径分两条：

- **热路径**：`LoopState.working_state` 内存缓存，每轮 `inject_pinned_blocks` 读它，零文件 IO。
- **冷路径 / 首次读**：`derive_state`（`traits.rs:422`）rebuild——找最新 `Snapshot` 事件 + 重放其后的 tail。崩溃恢复 / 新 session `continue` 走它。

**崩溃安全（§8.1）**：append-only → 最后一行若 partial-write（断电/崩溃），reload 时 `read_events`（`working_state_store.rs:79`）跳过反序列化失败的那行，不 abort 整个日志。每步 append 即持久化 → 崩溃最多丢最后一步。

**index.json 不漂**：它是事件日志的派生物，`append_event` 每次写都增量更新它；`list_open_tasks` 只读它（不逐个 derive），所以跨 session 发现是 O(1) 文件读 + O(N) 解析 index 条目，零 per-task IO。

---

## 4. L2/L4 每步增量持久化（替换旧 auto_checkpoint）

旧路径：`AgentLoop::auto_checkpoint` 构造 `AgentState` 绑给 `_agent_state`（下划线=未用）从不 `save()`；`AppSession::save_checkpoint` 存的是空 `GlobalState::new()`。崩溃恢复实际不存在——这两套 stub 已在 P4 删除。

新路径：`LoopState` 持有 `working_state: Option<WorkingState>` + `task_id`。在 plan 控制工具执行点（`agent_loop.rs` 控制工具分支）append 事件 + 更新内存投影：

- `exit_plan_mode` 接受计划 → `TaskCreated`（若新任务）+ 每个 step `StepAdded`。
- `task_update` 状态变更 → `StepStatusChanged`。**每步 append 即持久化**，崩溃最多丢最后一步。
- `request_plan_decision` 审批落定 → `DecisionMade`（chosen + rationale + alternatives）。
- 升级/`stuck`（空闲超时）→ `BlockerRaised`；恢复 → `BlockerResolved`。
- 每步 append 后按 policy 触发 `compact_if_needed`。

`WorkingStateStore` 是唯一写者——`append_event` 是唯一落盘入口，内存投影由 caller 持有；磁盘 `Snapshot` 只在 compaction 时由 projector 写成一个 `Snapshot` 事件（日志内的事件，不是日志外的并行 state → 从根上消除 §8.4 drift：snapshot 和 events 不可能不一致）。

---

## 5. L3 Pinned 投影（保留重注入模式）

`inject_pinned_blocks`（`context_assembler.rs`）保留每轮重建、不写回 durable log 的架构，**数据源从 `Conversation::metadata` 换成内存 `WorkingState`**：

- `[Task Anchor]` ← `working_state.goal / intent`。
- `[Plan & Progress]` ← `working_state.steps`，渲染 ✅/🔄/⏳。
- `[Decisions Made]` ← `working_state.decisions`（chosen + rationale）——补"关键决策"缺口。
- `[Blockers]` ← open blockers——补"卡点"缺口。

仍是 ephemeral、每轮重建、零 IO。`from_conversation`（`agent_loop.rs`）的 `original_task` 改从 `WorkingState.goal` 取，不被新 task arg 覆盖（修旧 bug：用新 task arg 覆盖目标，导致 resume 找不到原目标）。

---

## 6. L5 跨 Session 发现（用户核心诉求）

新 session 启动（`AppBuilder::create_session`）：

1. 读 `WorkingStateStore::list_open_tasks(user, project)`（一次 index.json 读，零 per-turn）→ 注入 `[Unfinished Work From Previous Sessions]` pinned 块（首轮 `EveryIteration`，之后 `OnChange`），列出未完成任务 + 进度摘要 + open blockers，问用户是否继续某个。
2. 用户 `tasks continue <id>` / 平台 `continue_task` → 新 session 绑定 `task_id=id`，`get_task(id)` derive 一次进 `LoopState.working_state`，pinned 块从该 task 投影。**不读旧 session conversation**（§6.2——conversation 是 transcript，不是 working state 的 source）。
3. `chat --resume <session_id>`（**设计如此，尚未实现**）：load conversation（现有 SQLite 路径）+ 从 conversation 取 `task_id` 指针 + `get_task(task_id)` derive rehydrate。durable 部分由事件日志覆盖；LoopState 运行时字段（paradigm/token budget）从 conversation + working state 推导，不单独 checkpoint。当前 `oneai session resume <id>` 只 print-only 预览对话历史，live 续接统一走 `tasks continue`（跨 session）。

---

## 7. L7 Ground Truth 对账（§8.2）

`RefreshPolicy`（`context_source.rs:30`）加 `OnResume` 变体（`context_source.rs:50`）。CodingPack 的 `GitReconciliationSource`（`builtin_sources.rs`）在 resume/continue 时跑 `git status` / `git log` / `git diff .oneai/`，与 `WorkingState` 的 "current step / pinned 文件" 对账：drift 则 append `Reconciliation` 事件 + pinned 块标 stale，冲突以代码为准。因 working state 落在 in-repo `.oneai/`，`git diff` 天然就是对账 source。Assistant pack 无外部 ground truth，跳过。

---

## 8. L8 场景策略（折进 MemoryProfile，不新增 DomainPack 层）

`MemoryProfile`（`memory_profile.rs`）加 `working_state: WorkingStatePolicy` 子结构：

| 字段 | 含义 |
|---|---|
| `storage_root` | `InRepo(".oneai")` / `HomeDir("~/.oneai")` |
| `persistence` | `StrictEventSourced`（唯一选项——原则派） |
| `checkpoint_granularity` | `EveryStep` / `CriticalNodes` / `OnTaskBoundary` |
| `ground_truth_reconciliation` | `Git` / `None` |
| `cross_session_surface` | `AutoInject` / `OnDemand` |
| `retention` | `ArchiveOnComplete` / `Keep` |
| `compaction` | `{ event_threshold, keep_recent, max_age_before_archive }` |
| `thickness` | `Thin`（可从外部 re-derive）/ `Thick`（无外部 GT） |

两套预设：CodingPack = `InRepo + EveryStep + Git + AutoInject + ArchiveOnComplete + Thin + compaction{200, 50, 30d}`；Assistant pack = `HomeDir + OnTaskBoundary + None + AutoInject + Keep + Thick + compaction{500, 100, 90d}`。

---

## 9. L9 事件日志 Compaction（有界增长）

- **阈值触发**（`compact_if_needed`，`traits.rs:427`）：单 task 事件数 > `event_threshold` → projector 把 `[首..最新 Snapshot 之后]` 之外的事件折叠成一个 `Snapshot` 事件（payload = 当时 `derive_state` 的全量 WorkingState JSON），保留最近 `keep_recent` 条原始事件。`derive_state` = 找最新 `Snapshot` + 重放其后 tail。
- **完成/归档触发**（`archive_task`，`traits.rs:430`）：task → `Completed`/`Archived` → 整 `{task_id}.jsonl` gzip 成 `{task_id}.archive.jsonl.gz`，index 标 archived，保留一条 summary（goal + 完成时间 + 最终 step 摘要）供历史回溯。
- **时间触发**：Archived 且超 `max_age_before_archive` → 删 `.archive.jsonl.gz`（保留 index summary）。

`Snapshot` 是**日志内的事件**，不是日志外的并行 state → snapshot 和 events 不可能不一致（§8.4 drift 从根消除）。

---

## 10. L10 CLI / API

- `oneai tasks list` / `show <id>` / `continue <id>` / `archive <id>`（`examples/cli/src/cmd_tasks.rs`，读 index/文件）。
- `oneai chat --resume <id>`（实现被引用但不存在的命令）。
- `oneai run` 无 task 时自动 surface `[Unfinished Work]`。
- UniFFI / 平台层暴露 `list_open_tasks` / `continue_task`。

---

## 11. 与旧补丁的对照（P4 已删除）

| 旧补丁（病灶） | 新机制 |
|---|---|
| `Conversation::metadata["task_anchor"]/["plan_state"]` 作 working-state source | 只存 `task_id` 指针；pinned 块读内存 `WorkingState` |
| `AgentLoop::auto_checkpoint`（no-op stub，绑 `_agent_state` 从不 save） | 删；由事件日志每步 append 取代 |
| `AppSession::save_checkpoint`（空 `GlobalState::new()`） | 删；durable 由事件日志覆盖 |
| `ProgressiveCheckpointManager` / `CheckpointBackend` / `AutoSavePolicy`（SQLite checkpoint infra） | 删整个 `progressive_checkpoint.rs`；working state 走文件事件日志，不复用此套 |
| `CoreMemory::pinned: RwLock<Vec<String>>`（进程内存，不持久） | 折进 `MemoryFact.pinned: bool` 列（`#[serde(default)]` + SQLite `pinned` 列 + 迁移），pin 状态随 fact 序列化、跨重启存活 |
| `from_conversation` 用新 task arg 覆盖 `original_task` | 从 `WorkingState.goal` rehydrate，不被覆盖 |
| `cmd_session::cmd_session_resume` print-only + 指向幽灵命令 | `tasks continue <id>` 真续接（已实现，跨 session 绑定 task_id + derive）；`chat --resume` 同 session 真续接**计划中未实现**，`session resume` 仍 print-only 预览 |

> 注：`FilePersistence` / `StatePersistence` trait / `AgentState` / `CheckpointInfo`（`checkpoint.rs`）**保留**——它们是 Studio Web UI checkpoint 浏览器的后端（list/load/browse 已保存的 state 文件），与 progressive checkpoint 管理器无关。

---

## 12. 验证要点

1. **单元**：`FileWorkingStateStore` append N 事件 → `derive_state` == 内存缓存；compaction 后 derive 一致；partial-write 末行损坏被忽略；index 与文件不漂。
2. **性能**：每轮 `inject_pinned_blocks` 读内存 WorkingState（<μs 级，零文件 IO）；append 事件延迟（ms 级，单行 append）。
3. **同 session resume**（`chat --resume`，**尚未实现**；当前 `session resume` 为 print-only 预览）：长任务跑到中途 kill → 重启 `chat --resume` → goal/steps/decisions/blockers 从 JSONL rehydrate、pinned 块正确、`original_task` 不被覆盖。
4. **跨 session（核心）**：session A 建任务跑到 step 3 未完 → 退出 → 新建 session B（新 session_id、不读 A 的 conversation） → 首轮出现 `[Unfinished Work]` 含 A 的任务（读 index.json） → `tasks continue <A_task_id>` → B 绑定 A 的 task_id、derive A 的 working_state 进 LoopState、pinned 投影 step 3 进度、不重复已完成步骤。
5. **对账**：CodingPack 下，session 间外部改了 git → continue 时 pinned 标 stale + 记 `Reconciliation` 事件。
6. **有界增长**：单 task append > `event_threshold` → 旧事件折叠成 snapshot、日志收缩；task complete → `.archive.jsonl.gz` 生成、index 标 archived。
7. **pin 持久**：core memory pin 一个 fact → 重启后 SQLite `pinned` 列仍在、`enforce_budget` 不驱逐它（`sqlite_store.rs::pinned_flag_survives_sqlite_roundtrip`）。

---

## 13. 设计取舍与业界对标

- **存储用文件不用 DB**：working state 是结构化 per-task JSONL。理由：人可读可改（手工修卡死的任务）、git 可版本化（编码场景免费 durability + diff 即对账 source）、append-only partial-write 容错、无 schema migration 痛、零依赖、匹配 working state 的"文档"substrate 性质。Claude Code（`~/.claude/projects/{cwd}/{id}.jsonl`）、RALPH（TASKS.md + git）、agent-session-resume（handoff.md）全是文件方案。DB 的索引查询优势只在跨 session 大规模搜索/多 agent 协调时显现，不在 OneAI 的本地单用户目标范围。
- **热路径走内存缓存，文件只作 durable mirror**：working state 在 session 启动 derive 一次 → 缓存进 `LoopState`；生命周期事件 append + 更新内存缓存；pinned 重注入每轮读内存缓存，零 IO。这是 Claude Code 的模型（todo 在内存、JSONL 是镜像），避免每轮查 store 的性能问题。
- **原则派 + 投影可重建**：append-only 事件日志是 source of truth；内存 working state 是从事件 derive 的 projection，随时可 rebuild。读路径走内存缓存（不每次 replay），崩溃后用事件重建。

参考来源：Claude Code session storage（JSONL append-only）、TASKS.md pattern（RALPH / agent-session-resume）、参考文档 §7.1 state derived from events、§8.1/8.2/8.4 失败模式、§10.2-10.3 working-state 文件启动注入。完整调研见 `docs/agent-working-state-and-cross-session-resume.md`。
