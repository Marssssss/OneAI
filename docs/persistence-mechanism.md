# OneAI 持久化机制

> SQLite（会话 / LTM / 用量）+ 文件事件日志（working state / 跨 session 续接），两条互补的持久化通路。

## 职责

让 Agent 的两类状态在不同时间尺度上存活：① 对话历史 / 长期事实 / 用量记录——关系型，走 SQLite；② 任务的工作状态（目标 / 步骤 / 决策 / 卡点）——append-only 事件流，走文件事件日志。后者独立于 session，跨 session 可续接。

## SQLite 会话存储

`SqliteSessionStore` 持久化会话 / LTM / 用量；`AppSession` 每次运行后自动保存。`oneai session list / resume <id> / delete / info`、`oneai memory search/list --user`。

> `oneai session resume <id>` 目前是 **print-only 预览**（显示对话历史，不跑 agent loop、不 rehydrate working state）。live 续接统一走 `tasks continue <id>`。同 session 的 `chat --resume` 尚未实现。

## 文件事件日志（working state）

任务的工作状态不摊在 session transcript 里，而是作为 **per-task append-only 事件日志**落文件（source of truth），独立于任何 session。新 session 读一次轻量索引即 surface 上次未完成工作。完整机制见 [Working-State 机制](working-state-mechanism.md)。

## 关键类型与文件

| 项 | 位置 |
|---|---|
| `SqliteSessionStore`（会话 / LTM / 用量） | `crates/oneai-persistence/src/sqlite_store.rs` |
| `FileWorkingStateStore`（事件日志） | `crates/oneai-persistence/src/working_state_store.rs` |
| `SqliteUsageTracker` | `crates/oneai-persistence/src/usage_tracker.rs` |
| Studio checkpoint 后端 | `crates/oneai-persistence/src/checkpoint.rs`、`state.rs` |

## 相关 CLI

[`session list/resume/delete/info/decay/export-hf`](cli-reference.md#持久化会话sqlite)、[`tasks list/show/continue/archive`](cli-reference.md#工作状态跨-session-任务续接)、[`memory search/list`](cli-reference.md#记忆跨会话持久事实)、[`usage report/session/export`](cli-reference.md#用量记录纯-token-维度无-usd)。

## 深入阅读

- [Working-State 机制](working-state-mechanism.md)
- 记忆持久化见 [记忆机制](memory-mechanism.md)
